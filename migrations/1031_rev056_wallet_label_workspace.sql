-- ============================================================================
-- Signal Forge — Migration 1031: workspace ownership for wallet labels
-- Canonical source: REVIEW_RESULT.md REV-056-F01 (HIGH).
--
-- THE BREACH THIS CLOSES
-- `wallet_labels` had no tenancy column at all, and `list_labels` / `add_label` /
-- `revoke_label` / `import_blocklist` authenticated the session without ever binding
-- the session's workspace. The reviewer proved the consequence with two real HTTP
-- sessions:
--
--     workspace A owns label kind=A-private
--     workspace B GET  labels   -> 200, A-private returned
--     workspace B POST revoke   -> 200, revoked=1
--     DB revoked_at IS NOT NULL -> true
--
-- So one tenant could read another tenant's private classification AND destroy it.
-- Recent-intelligence rows were already workspace-scoped (1018/1019); labels were the
-- hole left behind, because they predate the workspace model entirely (0001).
--
-- WHICH TABLE, ON WHICH LANE
-- `wallet_labels` is a PRE-FREEZE table created by `0001_initial.sql`. Migration 1000
-- moves only the five COLLIDING legacy tables into `swi_legacy`; `wallet_labels` is
-- not one of them, so it stays in `public` on every lane and this migration finds it
-- there. The guard below still tolerates its absence, because a canonical-only
-- database that never ran 0001 has no such table and must not fail.
--
-- BACKFILL IS DELIBERATE, NOT INCIDENTAL
-- Existing rows have no recorded owner. Guessing per-row ownership is impossible, and
-- leaving them NULL would leave the same hole open (a NULL matches no workspace filter
-- but also fails closed inconsistently across queries). They are assigned to the
-- `default` workspace — the single tenant that existed while these rows were being
-- written — and the assignment is recorded in `schema_cutover_events` so an operator
-- can audit or re-assign it rather than discovering it later.
--
-- NOT NULL is set AFTER the backfill so the upgrade cannot fail on legacy rows, the
-- same ordering 1019 used for `recent_events.workspace_id`.
--
-- Forward-only: 1001..1030 are shipped and immutable (REV-045-F02). Idempotent on
-- fresh, legacy-bridged, and already-upgraded databases.
-- ============================================================================

DO $$
DECLARE
    v_default_workspace bigint;
    v_backfilled        bigint := 0;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'wallet_labels'
    ) THEN
        RAISE NOTICE 'wallet_labels absent (canonical-only lane); nothing to scope';
        RETURN;
    END IF;

    -- The workspace every pre-tenancy row belongs to. Seeded by 1019.
    IF NOT EXISTS (SELECT 1 FROM workspaces WHERE slug = 'default') THEN
        INSERT INTO workspaces (name, slug) VALUES ('Default', 'default');
    END IF;
    SELECT id INTO v_default_workspace FROM workspaces WHERE slug = 'default';

    ALTER TABLE public.wallet_labels
        ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id);

    UPDATE public.wallet_labels
       SET workspace_id = v_default_workspace
     WHERE workspace_id IS NULL;
    GET DIAGNOSTICS v_backfilled = ROW_COUNT;

    ALTER TABLE public.wallet_labels
        ALTER COLUMN workspace_id SET NOT NULL;

    -- Tenancy-first index: every read filters by workspace, so it leads the key.
    CREATE INDEX IF NOT EXISTS wallet_labels_workspace_active_idx
        ON public.wallet_labels (workspace_id, chain, address, kind)
        WHERE revoked_at IS NULL;

    -- REV-058-F04: the comment must live INSIDE the existence branch.
    --
    -- It used to be a top-level statement after the DO block, so on a canonical-only
    -- database the guard correctly said "wallet_labels absent; nothing to scope" and
    -- then the very next statement failed with
    --     ERROR: relation public.wallet_labels does not exist   (exit=3)
    -- A guard that reports success and then aborts anyway is not a guard. Dynamic SQL
    -- is used because COMMENT ON COLUMN cannot be made conditional.
    --
    -- NOTE: no dollar-dollar sequence may appear anywhere in this file's comments.
    -- This body is dollar-quoted, so such a sequence inside a comment CLOSES the quote
    -- and the remainder is parsed as statements. That is exactly how the first attempt
    -- at this fix failed, with "syntax error at or near" pointing into prose.
    EXECUTE format(
        'COMMENT ON COLUMN public.wallet_labels.workspace_id IS %L',
        'REV-056-F01: owning workspace. Every label read, write, revoke, import, and disposition lookup MUST filter on it: without it one tenant could read and revoke another tenant''s private classifications.'
    );

    IF v_backfilled > 0 THEN
        INSERT INTO schema_cutover_events (cutover, detail)
        VALUES (
            'wallet_labels_workspace_backfill',
            jsonb_build_object(
                'migration', '1031_rev056_wallet_label_workspace.sql',
                'assigned_workspace_id', v_default_workspace,
                'rows_backfilled', v_backfilled,
                'reason', 'pre-tenancy rows (0001_initial) had no recorded owner; assigned to the default workspace. Re-assign deliberately if this database was ever multi-tenant.'
            )
        );
        RAISE NOTICE 'assigned % pre-tenancy wallet_labels row(s) to workspace %',
            v_backfilled, v_default_workspace;
    END IF;
END$$;
