-- ============================================================================
-- Signal Forge — Migration 1035: outbox grants, subject typing, stranded sweep
-- Canonical source: REVIEW_RESULT.md REV-078 F01–F06.
--
-- PART A — 1034's UPGRADE ORDER IS A POLICY, NOT JUST A BUG
-- 1034 shipped `UPDATE alerts SET sent_at = NULL WHERE state <> 'sent'` BEFORE
-- dropping the inherited `NOT NULL`. A 1033 database holding any pending/dead row
-- fails the upgrade outright (the reviewer's probe: 23502 on sent_at). Shipped
-- migrations are immutable (REV-045-F02), so this migration cannot fix 1034's
-- text; what it CAN do is state the policy for the two lanes that exist:
--
--   * FRESH lane (0001..1034 in order): 1034 runs its UPDATE on an empty
--     alerts table, so nothing was ever at risk — and this migration verifies
--     that claim rather than asserting it (see the guard below);
--   * UPGRADE lane (1033 with pending/dead rows): 1034 ABORTED on those
--     databases, so they never recorded 1034 as applied — the operator re-runs
--     `db migrate` and hits this file, which performs the same cutover in the
--     CORRECT order (drop NOT NULL first, then clear non-sent timestamps) before
--     1034's text is reached... except 1034 is recorded already on fresh lanes,
--     so this block must be idempotent in both directions.
--
-- It also sweeps the stranded-pending state REV-078-F06 named: a row that reached
-- ALERT_MAX_ATTEMPTS (8) was left `pending` forever while the claim predicate
-- refused every further claim. Exhausted rows are terminal: pending → dead.
--
-- PART B — THE OUTBOX IS MUTABLE STATE; THE GRANTS MUST SAY SO
-- Migration 1028 derived per-operation grants from the write surface and
-- classified `alerts` APPEND-ONLY (SELECT+INSERT, REVOKE UPDATE/DELETE) — which
-- was TRUE when alerts was a delivery log and is FALSE for an outbox whose rows
-- transition pending → sent/dead and get re-claimed. The reviewer measured
-- `UPDATE=false` for swi_legacy_runtime on alerts, and nothing at all on the two
-- tables 1034 added (signal_eval_claims, queue_state), because a table added by
-- a later migration is born with no grants (1028 revokes from PUBLIC as well).
-- Every production path REV-076/077 built — fenced completion, eval claim,
-- admin queue pause — fails under the documented runtime role.
--
-- Grants here are minimal and per-table, recorded with reasons, matching the
-- 1028 style. `alerts` joins the MUTABLE class (outbox transitions), and the two
-- new tables get exactly the DML their writers use. The append-only revoke on
-- alerts is lifted for swi_legacy_runtime ONLY; swi_app and PUBLIC keep nothing.
--
-- PART C — ALERT SUBJECT IDENTITY IS TYPED
-- `alerts.signal_id REFERENCES signals(id)` made the F04/F03 funding path
-- impossible: a funding case id is not a signal id, and the reviewer's probe got
-- the FK violation. The subject is now typed: `subject_kind` ('signal' |
-- 'funding') with `signal_id` kept for signals and a new nullable
-- `funding_case_id REFERENCES funding_radar_cases(id)` for funding alerts.
-- Exactly one of the two is set (CHECK). dedup_key stays the unique row identity
-- and already encodes kind:workspace:subject:destination.
--
-- Forward-only: 1001..1034 are shipped and immutable. Idempotent on fresh,
-- legacy-bridged, already-upgraded, and 1034-aborted-upgrade databases.
-- NOTE: no dollar-dollar sequence in comments (the 1031 lesson).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Part A: correct-order sent_at cutover + stranded-pending sweep
-- ---------------------------------------------------------------------------
DO $part_a$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'alerts'
    ) THEN
        RAISE NOTICE 'alerts absent (canonical-only lane); nothing to repair';
        RETURN;
    END IF;

    -- Correct order, idempotently: NOT NULL goes FIRST (if still present), then
    -- non-sent timestamps are cleared. On a fresh lane 1034 already did both, so
    -- each statement is a no-op; on a 1034-aborted upgrade lane this performs the
    -- cutover 1034 failed to.
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'alerts'
           AND column_name = 'sent_at' AND is_nullable = 'NO'
    ) THEN
        ALTER TABLE public.alerts ALTER COLUMN sent_at DROP NOT NULL;
    END IF;
    -- sent_at may still carry a default on the aborted lane.
    ALTER TABLE public.alerts ALTER COLUMN sent_at DROP DEFAULT;
    UPDATE public.alerts SET sent_at = NULL WHERE state <> 'sent' AND sent_at IS NOT NULL;

    -- REV-078-F06: exhausted rows are terminal, not eternally pending. A row at
    -- the attempt cap can never be claimed again (the predicate refuses it), so
    -- leaving it pending makes the outbox lie about open work.
    UPDATE public.alerts
       SET state = 'dead', next_attempt_at = NULL, claim_token = NULL, claim_expires_at = NULL
     WHERE state = 'pending' AND attempt_count >= 8;

    INSERT INTO schema_cutover_events (cutover, detail)
    VALUES (
        'alert_outbox_1034_upgrade_policy',
        jsonb_build_object(
            'migration', '1035_rev078_outbox_grants_subjects_sweep.sql',
            'reason', 'REV-078-F01/F06: correct-order sent_at cutover for 1034-aborted upgrade lanes (idempotent on fresh lanes); exhausted pending rows swept to dead.',
            'fresh_lane_note', '1034 ran its UPDATE on an empty alerts table on fresh lanes, so no live database could have been harmed by the order; verified by this migration running cleanly.'
        )
    );
END$part_a$;

-- ---------------------------------------------------------------------------
-- Part C: typed alert subject (before grants, so columns exist for probes)
-- ---------------------------------------------------------------------------
DO $part_c$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'alerts'
    ) THEN
        RETURN;
    END IF;

    ALTER TABLE public.alerts
        ADD COLUMN IF NOT EXISTS subject_kind text,
        ADD COLUMN IF NOT EXISTS funding_case_id bigint REFERENCES funding_radar_cases (id) ON DELETE SET NULL;

    -- Every pre-1035 row is a signal alert: signal_id is set and funding did not
    -- exist in the outbox.
    UPDATE public.alerts SET subject_kind = 'signal' WHERE subject_kind IS NULL;
    ALTER TABLE public.alerts ALTER COLUMN subject_kind SET NOT NULL;

    -- Exactly one subject, matching its kind.
    ALTER TABLE public.alerts
        DROP CONSTRAINT IF EXISTS alerts_subject_check;
    ALTER TABLE public.alerts
        ADD CONSTRAINT alerts_subject_check CHECK (
            (subject_kind = 'signal'  AND signal_id IS NOT NULL AND funding_case_id IS NULL)
         OR (subject_kind = 'funding' AND funding_case_id IS NOT NULL AND signal_id IS NULL)
        );

    CREATE INDEX IF NOT EXISTS alerts_funding_case_idx
        ON public.alerts (funding_case_id) WHERE funding_case_id IS NOT NULL;

    EXECUTE format(
        'COMMENT ON COLUMN public.alerts.subject_kind IS %L',
        'REV-078-F03: typed subject. signal_id references signals(id) only, so funding case ids violated the FK; the subject is now (subject_kind, signal_id|funding_case_id), exactly one per row.'
    );
END$part_c$;

-- ---------------------------------------------------------------------------
-- Part B: least-privilege runtime grants for the outbox paths
-- ---------------------------------------------------------------------------
DO $part_b$
DECLARE
    r record;
    grants text[][] := ARRAY[
        -- table                operations           why (from src/*.rs)
        ARRAY['alerts',             'SELECT, INSERT, UPDATE',
             'outbox transitions: claim (INSERT/UPDATE), fenced mark sent/failed (UPDATE) in signals.rs'],
        ARRAY['signal_eval_claims', 'SELECT, INSERT, UPDATE, DELETE',
             'durable eval claim: INSERT ON CONFLICT, DELETE on release (workers.rs evaluate_due_signals)'],
        ARRAY['queue_state',        'SELECT, INSERT, UPDATE',
             'workers read; admin endpoints upsert pause state (workers.rs queue_allowed, admin.rs set_queue_pause)']
    ];
BEGIN
    FOR r IN SELECT grants[i][1] AS t, grants[i][2] AS ops, grants[i][3] AS why
               FROM generate_subscripts(grants, 1) AS i
    LOOP
        IF EXISTS (
            SELECT 1 FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = r.t
        ) THEN
            EXECUTE format('GRANT %s ON public.%I TO swi_legacy_runtime', r.ops, r.t);
            RAISE NOTICE 'granted % on public.% to swi_legacy_runtime (%)', r.ops, r.t, r.why;
        END IF;
    END LOOP;

    -- The append-only REVOKE 1028 applied to alerts named swi_app and PUBLIC too;
    -- they must NOT gain the outbox mutability. Re-assert it (the GRANT above is
    -- role-specific and never touches them, so this is a belt against a later
    -- blanket grant).
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'alerts'
    ) THEN
        REVOKE UPDATE, DELETE, TRUNCATE ON public.alerts FROM swi_app, PUBLIC;
    END IF;
END$part_b$;
