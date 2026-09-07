-- ============================================================================
-- Signal Forge — Migration 1036: alert workspace ownership + legacy subject repair
-- Canonical source: REVIEW_RESULT.md REV-080 F02/F06/F07.
--
-- PART A — THE OUTBOX ROW MUST KNOW ITS TENANT (REV-080-F02)
-- `alerts` carries no `workspace_id`. The REV-079 funding drain selected pending
-- rows by destination alone and then claimed them with the CURRENT worker's
-- workspace: worker A selected workspace B's retry, and because the dedup key
-- embeds the workspace, A created a SECOND row under A while B's row stayed
-- pending — a cross-tenant duplicate delivery. The row's workspace is already
-- encoded inside dedup_key (kind:workspace:subject:destination), but an identity
-- you cannot query is not ownership. The column is added, backfilled from the
-- dedup key itself (which is the only authoritative record of which workspace
-- the identity was minted for), and made NOT NULL.
--
-- PART B — LEGACY SUBJECT BACKFILL MUST CLASSIFY FROM THE KEY (REV-080-F06)
-- 1035 backfilled every pre-existing row as subject_kind='signal'. A REV-077-era
-- funding row existed exactly when its funding case id happened to be a valid
-- signal id (the reviewer's constructed example `funding:880001:880001:chat`
-- became subject_kind=signal). The dedup key's kind segment is the minted truth;
-- the FK target is only corroboration. Rows are re-classified: key-kind funding
-- with a VALID funding_case target becomes funding; key-kind funding whose case
-- is gone (or key-kind signal whose signal is gone) is NOT silently flipped —
-- it stays for audit with subject_kind 'unknown' and is excluded from every
-- selection (the CHECK constrains only signal/funding; unknown rows keep their
-- historical FK shape).
--
-- PART C — CUTOVER EVENTS MUST BE REPLAY-SAFE (REV-080-F07)
-- 1035 inserted its `schema_cutover_events` row unconditionally, so a direct
-- replay appended a second one. This migration uses a NOT-EXISTS guard for its
-- own event and repairs nothing retroactively — the duplicate 1035 rows, where
-- they exist, are history; the guard is the rule going forward.
--
-- Forward-only: 1001..1035 are shipped and immutable (REV-045-F02). Idempotent.
-- NOTE: no dollar-dollar sequence in comments (the 1031 lesson).
-- ============================================================================

DO $$
DECLARE
    v_unknown bigint := 0;
    v_funding bigint := 0;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'alerts'
    ) THEN
        RAISE NOTICE 'alerts absent (canonical-only lane); nothing to own';
        RETURN;
    END IF;

    -- ------------------------------------------------------------------
    -- Part A: workspace ownership, backfilled from the dedup key itself.
    -- The key is kind:workspace:subject:destination; workspace is the second
    -- segment. Rows whose key does not parse to an existing workspace are
    -- historical noise — they keep NULL only until the NOT NULL step, so they
    -- are assigned to the default workspace with the same audit trail 1031/1032
    -- used for pre-tenancy rows.
    -- ------------------------------------------------------------------
    ALTER TABLE public.alerts
        ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id);

    UPDATE public.alerts a
       SET workspace_id = w.id
      FROM workspaces w
     WHERE a.workspace_id IS NULL
       AND w.id = nullif(split_part(a.dedup_key, ':', 2), '')::bigint;

    IF NOT EXISTS (SELECT 1 FROM workspaces WHERE slug = 'default') THEN
        INSERT INTO workspaces (name, slug) VALUES ('Default', 'default');
    END IF;
    UPDATE public.alerts
       SET workspace_id = (SELECT id FROM workspaces WHERE slug = 'default')
     WHERE workspace_id IS NULL;

    ALTER TABLE public.alerts ALTER COLUMN workspace_id SET NOT NULL;

    CREATE INDEX IF NOT EXISTS alerts_workspace_due_idx
        ON public.alerts (workspace_id, state, next_attempt_at)
        WHERE state = 'pending';

    -- REV-080-F02 (second half): the funding drain derives due intent from
    -- durable CASE state, and cases carried no tenant either — any workspace's
    -- drain could derive another tenant's case. Same backfill policy as the
    -- outbox: pre-tenancy rows belong to the default workspace, and every writer
    -- from here on must set it (the radar producer binds the worker's workspace).
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'funding_radar_cases'
    ) THEN
        ALTER TABLE public.funding_radar_cases
            ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id);
        UPDATE public.funding_radar_cases
           SET workspace_id = (SELECT id FROM workspaces WHERE slug = 'default')
         WHERE workspace_id IS NULL;
        ALTER TABLE public.funding_radar_cases ALTER COLUMN workspace_id SET NOT NULL;
        EXECUTE format(
            'COMMENT ON COLUMN public.funding_radar_cases.workspace_id IS %L',
            'REV-080-F02: owning workspace. The funding retry drain derives intent from durable case state; without tenancy here, any workspace could derive another tenant''s preparation case and deliver it.'
        );
    END IF;

    -- Grant: the outbox paths run as swi_legacy_runtime (1035).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON public.alerts TO swi_legacy_runtime';

    -- ------------------------------------------------------------------
    -- Part B: re-classify subjects from the minted key kind.
    -- ------------------------------------------------------------------
    -- funding keys with a live funding case: become funding (FK moved off the
    -- accidental signal overlap).
    UPDATE public.alerts a
       SET subject_kind = 'funding',
           funding_case_id = c.id,
           signal_id = NULL
      FROM funding_radar_cases c
     WHERE split_part(a.dedup_key, ':', 1) = 'funding'
       AND a.subject_kind = 'signal'
       AND c.id = nullif(split_part(a.dedup_key, ':', 3), '')::bigint;
    GET DIAGNOSTICS v_funding = ROW_COUNT;

    -- The 1035 CHECK constrained subject to exactly one of signal/funding; the
    -- unknown class needs the constraint widened FIRST, or the reclassification
    -- below would violate it.
    ALTER TABLE public.alerts
        DROP CONSTRAINT IF EXISTS alerts_subject_check;
    ALTER TABLE public.alerts
        ADD CONSTRAINT alerts_subject_check CHECK (
            (subject_kind = 'signal'  AND signal_id IS NOT NULL AND funding_case_id IS NULL)
         OR (subject_kind = 'funding' AND funding_case_id IS NOT NULL AND signal_id IS NULL)
         OR (subject_kind = 'unknown')
        );

    -- signal keys with a live signal: already signal; funding keys whose case is
    -- gone, and any row whose key kind cannot be corroborated: explicit unknown.
    UPDATE public.alerts a
       SET subject_kind = 'unknown'
     WHERE split_part(a.dedup_key, ':', 1) = 'funding'
       AND a.subject_kind = 'signal';
    GET DIAGNOSTICS v_unknown = ROW_COUNT;

    EXECUTE format(
        'COMMENT ON COLUMN public.alerts.workspace_id IS %L',
        'REV-080-F02: owning workspace, backfilled from the dedup key itself. The REV-079 funding drain selected by destination alone and re-keyed another tenant''s retry under the current worker''s workspace — a cross-tenant duplicate delivery. Every selection MUST filter on this column.'
    );

    -- ------------------------------------------------------------------
    -- Part C: replay-safe cutover record.
    -- ------------------------------------------------------------------
    IF NOT EXISTS (
        SELECT 1 FROM schema_cutover_events
         WHERE cutover = 'alert_workspace_ownership_and_subject_repair'
    ) THEN
        INSERT INTO schema_cutover_events (cutover, detail)
        VALUES (
            'alert_workspace_ownership_and_subject_repair',
            jsonb_build_object(
                'migration', '1036_rev080_alert_workspace_subject_cutover.sql',
                'reclassified_funding', v_funding,
                'unknown_subjects', v_unknown,
                'reason', 'REV-080: alerts carry workspace_id (drain must filter it); legacy subjects re-classified from the minted dedup-key kind with unknown never silently flipped; cutover inserts are guarded going forward.'
            )
        );
    END IF;
END$$;
