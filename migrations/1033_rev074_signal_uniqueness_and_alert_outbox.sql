-- ============================================================================
-- Signal Forge — Migration 1033: workspace-prefixed signal uniqueness + alert outbox
-- Canonical source: REVIEW_RESULT.md REV-074-F01 (HIGH) and REV-074-F03 (HIGH).
--
-- PART A — GLOBAL SIGNAL UNIQUENESS IS A CROSS-TENANT COUPLING
-- 1032 gave `signals` a mandatory `workspace_id`, but it deliberately did not touch
-- the baseline constraint `UNIQUE (chain, mint, signal_kind, created_at)` from
-- 0001. The reviewer's probe: workspace A writes a signal; workspace B evaluates the
-- SAME mint legitimately (its own policy) and its insert fails with SQLSTATE 23505.
-- One tenant can block another tenant's signal — an availability coupling even
-- though read isolation is green. The identity of a signal is now
-- (workspace, chain, mint, kind, created_at): the same mint at the same second in
-- two tenants is two signals, not one.
--
-- The constraint is dropped and recreated with the workspace leading the key.
-- `signals` is a small table and the old unique index disappears with the
-- constraint, so no lock-heavy index rebuild is needed.
--
-- PART B — ALERT DELIVERY IS AN OUTBOX, NOT A LOG
-- `alerts` was an append-only delivery LOG: `dispatch_alerts` inserted a row even
-- when the Telegram call failed (recording `delivery_error`), and the selection
-- excluded any signal that had ANY alert row. A timeout, 429, or 5xx was therefore
-- never retried — the notification was permanently lost while the database said
-- "alerted".
--
-- The columns below turn the table into a real outbox state machine:
--
--   state           'pending' (reserved, not yet sent) / 'sent' / 'dead'
--   attempt_count   delivery attempts so far
--   next_attempt_at when a retry becomes eligible (bounded exponential backoff)
--   sent_at         set ONLY on success; selection keys on it being NULL
--   last_error      the most recent failure, for operators (delivery_error kept as
--                   the legacy alias so existing readers do not break)
--
-- Existing rows are delivery history, not open work: rows with a recorded error
-- are marked 'dead' (they were terminal under the old semantics and resurrecting
-- them could re-alert on stale signals), rows without are 'sent'. This mirrors the
-- old read semantics exactly.
--
-- Both tables live in `public` on every lane (created by 0001, not among the five
-- tables migration 1000 archives). Guards tolerate their absence on a
-- canonical-only database.
--
-- Forward-only: 1001..1032 are shipped and immutable (REV-045-F02).
-- NOTE: no dollar-dollar sequence may appear in this file's comments (the bodies
-- are dollar-quoted; such a sequence would close the quote — the 1031 lesson).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Part A: workspace-prefixed signal uniqueness
-- ---------------------------------------------------------------------------
DO $part_a$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'signals'
    ) THEN
        RAISE NOTICE 'signals absent (canonical-only lane); nothing to re-key';
        RETURN;
    END IF;

    -- Idempotency: only replace the constraint while the GLOBAL one still exists.
    -- A database that already has the workspace-prefixed constraint is left alone.
    IF EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conrelid = 'public.signals'::regclass
           AND conname = 'signals_chain_mint_signal_kind_created_at_key'
    ) THEN
        ALTER TABLE public.signals
            DROP CONSTRAINT signals_chain_mint_signal_kind_created_at_key;

        ALTER TABLE public.signals
            ADD CONSTRAINT signals_workspace_chain_mint_kind_created_key
            UNIQUE (workspace_id, chain, mint, signal_kind, created_at);

        EXECUTE format(
            'COMMENT ON CONSTRAINT signals_workspace_chain_mint_kind_created_key ON public.signals IS %L',
            'REV-074-F01: signal identity is (workspace, chain, mint, kind, created_at). The global baseline constraint let one tenant block another tenant''s signal on the same mint/second with SQLSTATE 23505.'
        );

        INSERT INTO schema_cutover_events (cutover, detail)
        VALUES (
            'signals_uniqueness_workspace_prefixed',
            jsonb_build_object(
                'migration', '1033_rev074_signal_uniqueness_and_alert_outbox.sql',
                'dropped', 'signals_chain_mint_signal_kind_created_at_key',
                'added', 'signals_workspace_chain_mint_kind_created_key',
                'reason', 'REV-074-F01: global uniqueness coupled tenants: workspace B got 23505 writing a legitimate signal on the same mint/second as workspace A.'
            )
        );
        RAISE NOTICE 'signals uniqueness is now workspace-prefixed (REV-074-F01)';
    END IF;
END$part_a$;

-- ---------------------------------------------------------------------------
-- Part B: alert outbox state machine
-- ---------------------------------------------------------------------------
DO $part_b$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'alerts'
    ) THEN
        RAISE NOTICE 'alerts absent (canonical-only lane); nothing to outbox';
        RETURN;
    END IF;

    ALTER TABLE public.alerts
        ADD COLUMN IF NOT EXISTS state text NOT NULL DEFAULT 'pending',
        ADD COLUMN IF NOT EXISTS attempt_count integer NOT NULL DEFAULT 0,
        ADD COLUMN IF NOT EXISTS next_attempt_at timestamptz,
        ADD COLUMN IF NOT EXISTS last_error text;

    -- sent_at already exists (0001) but was written unconditionally; it now means
    -- "delivered". History is classified to match the semantics it had:
    -- a row with a recorded error was a terminal failure, anything else succeeded.
    UPDATE public.alerts
       SET state = CASE WHEN delivery_error IS NULL THEN 'sent' ELSE 'dead' END,
           last_error = delivery_error
     WHERE state = 'pending';

    ALTER TABLE public.alerts
        DROP CONSTRAINT IF EXISTS alerts_state_check;
    ALTER TABLE public.alerts
        ADD CONSTRAINT alerts_state_check CHECK (state IN ('pending', 'sent', 'dead'));

    -- The dispatcher's selection: open work, eligible now.
    CREATE INDEX IF NOT EXISTS alerts_outbox_due_idx
        ON public.alerts (next_attempt_at)
        WHERE state = 'pending';

    EXECUTE format(
        'COMMENT ON COLUMN public.alerts.state IS %L',
        'REV-074-F03: outbox state. pending = reserved, not yet delivered; sent = delivered (sent_at set); dead = permanent failure. A failed send MUST NOT mark the row sent — the old log semantics lost every transient failure forever.'
    );
END$part_b$;
