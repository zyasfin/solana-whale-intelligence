-- ============================================================================
-- Signal Forge — Migration 1034: fenced alert outbox + durable eval/queue state
-- Canonical source: REVIEW_RESULT.md REV-076-F02, F03, F04.
--
-- PART A — THE OUTBOX MUST NOT LIE ABOUT DELIVERY
-- `alerts.sent_at` was defined `NOT NULL DEFAULT now()` in 0001 and migration 1033
-- left that in place. Reserving a `pending` row therefore wrote a SUCCESS
-- TIMESTAMP before any HTTP call: the reviewer probed a reserved row and read
-- state=pending with sent_at already set. `sent_at` becomes nullable with no
-- default and is set only by a fenced completion.
--
-- Fencing and destination identity (`REV-076-F03` fencing, `REV-076-F04`):
--   destination     canonical delivery destination (Telegram chat id). The dedup
--                   identity is (kind, signal/case, destination): changing the chat
--                   id must NOT suppress legitimate delivery to the new one, and
--                   two destinations are two alerts.
--   claim_token     fencing token minted per claim. Completion requires the
--                   matching token AND state='pending', so a stale sender can
--                   neither mark a newer claim sent nor resurrect a sent row.
--   claim_expires_at lease. A dispatcher that crashes mid-send loses the lease;
--                   another dispatcher may re-claim only after expiry, so "the
--                   provider accepted it but we crashed" retries are bounded by
--                   the lease rather than immediate.
-- `dedup_key` remains the unique row identity, now carrying kind+destination.
-- Existing rows get destination 'unknown' — their true destination was never
-- recorded (that is exactly the F04 defect) and inventing one would be worse.
--
-- PART B — DURABLE SAME-WORKSPACE EVALUATION CLAIM (REV-076-F02)
-- Due selection was read-only: two worker processes on one workspace could pick
-- the same token and evaluate with different timestamps, defeating the
-- timestamp-based signal uniqueness and producing duplicate logical signals.
-- `signal_eval_claims` makes the claim itself a row: one PRIMARY KEY per
-- (workspace, chain, mint), INSERT-first wins, and the claimed evaluation is
-- written with a SHARED timestamp so a slow/losing path cannot invent a second
-- identity. The claim is released on success and on failure; a crashed worker's
-- claim expires (`expires_at`) and becomes claimable again.
--
-- PART C — QUEUE BACKPRESSURE NEEDS A PRODUCTION AUTHORITY (REV-076-F02)
-- Queue pause/lag was process-local memory with no writer at all: the gate
-- functioned, but nothing operational could ever pause anything, so
-- "backpressure" was a claim, not a mechanism. `queue_state` is the durable
-- authority: the worker reads it, the admin API writes it (audit-logged). One
-- row per queue; unknown queues are simply absent (never paused).
--
-- Forward-only: 1001..1033 are shipped and immutable (REV-045-F02).
-- NOTE: no dollar-dollar sequence in comments (the 1031 lesson).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Part A: fenced, destination-aware alert outbox
-- ---------------------------------------------------------------------------
DO $part_a$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'alerts'
    ) THEN
        RAISE NOTICE 'alerts absent (canonical-only lane); nothing to fence';
        RETURN;
    END IF;

    -- sent_at: a success fact, not a reservation artifact. Default removed first,
    -- then the lie is scrubbed from rows that never delivered.
    ALTER TABLE public.alerts ALTER COLUMN sent_at DROP DEFAULT;
    UPDATE public.alerts SET sent_at = NULL WHERE state <> 'sent' AND sent_at IS NOT NULL;
    ALTER TABLE public.alerts ALTER COLUMN sent_at DROP NOT NULL;

    ALTER TABLE public.alerts
        ADD COLUMN IF NOT EXISTS destination text,
        ADD COLUMN IF NOT EXISTS claim_token text,
        ADD COLUMN IF NOT EXISTS claim_expires_at timestamptz;

    -- What exists was never recorded; do not invent it.
    UPDATE public.alerts SET destination = 'unknown' WHERE destination IS NULL;
    ALTER TABLE public.alerts ALTER COLUMN destination SET NOT NULL;

    EXECUTE format(
        'COMMENT ON COLUMN public.alerts.sent_at IS %L',
        'REV-076-F03: set ONLY by a fenced successful completion. NOT NULL DEFAULT now() made every reservation carry a success timestamp before the HTTP call.'
    );
    EXECUTE format(
        'COMMENT ON COLUMN public.alerts.claim_token IS %L',
        'REV-076-F03: fencing token. Completion requires the matching token and state=pending, so a stale sender cannot mark a newer claim sent or overwrite a sent row.'
    );
    EXECUTE format(
        'COMMENT ON COLUMN public.alerts.destination IS %L',
        'REV-076-F04: canonical delivery destination; part of the dedup identity. Rows predating this column were delivered to an unrecorded destination and are marked unknown.'
    );

    INSERT INTO schema_cutover_events (cutover, detail)
    VALUES (
        'alert_outbox_fencing_and_destination',
        jsonb_build_object(
            'migration', '1034_rev076_fenced_outbox_eval_claim_queue_state.sql',
            'reason', 'REV-076: sent_at nullable (no pre-delivery success timestamp), claim_token fencing, lease expiry, destination-aware identity.'
        )
    );
END$part_a$;

-- ---------------------------------------------------------------------------
-- Part B: durable same-workspace signal-evaluation claim
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS public.signal_eval_claims (
    workspace_id bigint NOT NULL REFERENCES workspaces (id),
    chain        text NOT NULL,
    mint         text NOT NULL,
    claimed_by   text NOT NULL,
    claimed_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, chain, mint)
);

COMMENT ON TABLE public.signal_eval_claims IS
    'REV-076-F02: one durable claim per (workspace, chain, mint). INSERT wins; the loser skips the token, so two worker processes cannot evaluate the same token with different timestamps and mint duplicate logical signals. Claims expire so a crashed worker does not wedge a token.';

-- ---------------------------------------------------------------------------
-- Part C: durable queue backpressure state
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS public.queue_state (
    queue      text PRIMARY KEY,
    paused     boolean NOT NULL DEFAULT false,
    lag_seconds bigint NOT NULL DEFAULT 0,
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by text NOT NULL DEFAULT 'system'
);

COMMENT ON TABLE public.queue_state IS
    'REV-076-F02: durable pause/lag authority. Workers read it; the admin API writes it (audit-logged). Replaces process-local memory that no production path could ever set.';
