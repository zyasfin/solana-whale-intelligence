-- ============================================================================
-- Signal Forge — Migration 1032: signal tenancy + canonical cluster membership
-- Canonical source: REVIEW_RESULT.md REV-072 (two HIGH residuals under F06).
--
-- PART A — ONE WALLET MAY NOT IMPERSONATE TWO INDEPENDENT CLUSTERS
-- `wallet_cluster_members` is unique on (cluster_id, chain, address) only, so one
-- active (chain, address) could hold memberships in several clusters at once. The
-- entry gate counts COUNT(DISTINCT m.cluster_id) and documents a requirement of TWO
-- INDEPENDENT eligible clusters, so a single wallet in two active clusters satisfied
-- it by itself. The reviewer reproduced the false acceptance through the production
-- evaluator: one eligible wallet, two active memberships, eligible_clusters=2,
-- signal created.
--
-- `graph.rebuild_cluster_for` is how those rows appear: it picked ONE existing
-- cluster with `LIMIT 1` and never merged or revoked the others when a component
-- converged, so every convergence left the wallet counted twice.
--
-- The repair is a real MERGE, not a truncation. If a wallet is active in clusters A
-- and B then A and B are one component, and the correct state is one cluster — not
-- "keep A, forget B". Cluster ids are label-propagated to a fixpoint (each cluster
-- adopts the smallest id it shares an active member with, repeatedly, until nothing
-- changes; ids strictly decrease so it terminates), memberships are re-pointed at
-- the canonical cluster, and the superseded rows are REVOKED rather than deleted —
-- `wallet_cluster_members` is history, and revocation is how this schema retires a
-- membership everywhere else.
--
-- Only then can the partial unique index exist. Creating it first would abort the
-- upgrade on any database that already has duplicates, which is every database the
-- defect ever ran on.
--
-- PART B — SIGNAL OUTPUT MUST CARRY ITS OWNER
-- `evaluate_token_signals` takes `workspace_id` and uses it to decide POLICY (which
-- wallets may contribute alpha), but `signals` and `signal_evaluations` have no
-- tenancy column at all. The reviewer showed the same facts accepted in workspace A
-- and rejected in workspace B, with the accepted row landing in a global table that
-- no API, admin view, report, or alert dispatcher can filter — ownership that is not
-- stored cannot be enforced downstream. A policy-dependent output in an untenanted
-- table is a cross-tenant disclosure by construction.
--
-- Backfill is deliberate and audited, exactly as 1019 and 1031 did it: existing rows
-- predate the column and their true owner is unknowable, so they are assigned to the
-- `default` workspace and the assignment is recorded in `schema_cutover_events`.
-- NOT NULL is set AFTER the backfill so the upgrade cannot fail on legacy rows.
--
-- Both tables are created by `0001_initial.sql` and are NOT among the five colliding
-- tables migration 1000 archives into `swi_legacy`, so they stay in `public` on every
-- lane. The existence guards still tolerate their absence: a canonical-only database
-- that never ran 0001 has neither table and must not fail here.
--
-- Forward-only: 1001..1031 are shipped and immutable (REV-045-F02). Idempotent on
-- fresh, legacy-bridged, and already-upgraded databases.
--
-- NOTE: no dollar-dollar sequence may appear anywhere in this file's comments; the
-- bodies below are dollar-quoted and such a sequence would close the quote (the
-- failure mode recorded in 1031).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Part A: canonical cluster membership
-- ---------------------------------------------------------------------------
DO $part_a$
DECLARE
    v_merged     bigint := 0;
    v_revoked    bigint := 0;
    v_iterations int    := 0;
    v_remaining  bigint := 0;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'wallet_cluster_members'
    ) THEN
        RAISE NOTICE 'wallet_cluster_members absent (canonical-only lane); nothing to canonicalize';
        RETURN;
    END IF;

    CREATE TEMP TABLE cluster_canon ON COMMIT DROP AS
    SELECT DISTINCT cluster_id, cluster_id AS canon_id
      FROM public.wallet_cluster_members
     WHERE revoked_at IS NULL;

    -- Label propagation to a fixpoint. Each canonical label adopts the smallest
    -- label it shares an ACTIVE (chain, address) with; labels only ever decrease,
    -- so the loop terminates. The bound is a safety net, not the exit condition:
    -- if it were ever reached the data would be inconsistent and silently shipping
    -- a half-merged graph is worse than failing the upgrade.
    LOOP
        v_iterations := v_iterations + 1;
        IF v_iterations > 1000 THEN
            RAISE EXCEPTION 'cluster canonicalization did not converge after % passes', v_iterations;
        END IF;

        UPDATE cluster_canon c
           SET canon_id = s.min_canon
          FROM (
            SELECT cc.canon_id AS old_canon, MIN(cc2.canon_id) AS min_canon
              FROM public.wallet_cluster_members m
              JOIN public.wallet_cluster_members m2
                ON m2.chain = m.chain
               AND m2.address = m.address
               AND m2.revoked_at IS NULL
              JOIN cluster_canon cc  ON cc.cluster_id  = m.cluster_id
              JOIN cluster_canon cc2 ON cc2.cluster_id = m2.cluster_id
             WHERE m.revoked_at IS NULL
             GROUP BY cc.canon_id
          ) s
         WHERE c.canon_id = s.old_canon
           AND c.canon_id <> s.min_canon;

        EXIT WHEN NOT FOUND;
    END LOOP;

    -- Re-point every active membership onto its canonical cluster. A row that is
    -- already there is reactivated rather than duplicated.
    --
    -- `DISTINCT ON` is required, not cosmetic: a wallet active in three clusters
    -- yields three source rows for ONE canonical (cluster, chain, address), and
    -- PostgreSQL refuses an `ON CONFLICT DO UPDATE` that would touch the same row
    -- twice in one statement. The strongest membership wins the collapse.
    INSERT INTO public.wallet_cluster_members
        (cluster_id, chain, address, membership_kind, confidence)
    SELECT DISTINCT ON (cc.canon_id, m.chain, m.address)
           cc.canon_id, m.chain, m.address, m.membership_kind, m.confidence
      FROM public.wallet_cluster_members m
      JOIN cluster_canon cc ON cc.cluster_id = m.cluster_id
     WHERE m.revoked_at IS NULL
       AND cc.canon_id <> m.cluster_id
     ORDER BY cc.canon_id, m.chain, m.address, m.confidence DESC, m.id ASC
    ON CONFLICT (cluster_id, chain, address) DO UPDATE
        SET revoked_at = NULL,
            confidence = GREATEST(wallet_cluster_members.confidence, EXCLUDED.confidence);
    GET DIAGNOSTICS v_merged = ROW_COUNT;

    -- Retire the superseded memberships. Revoked, never deleted: this table is the
    -- membership history and every other retirement in this schema is a revocation.
    UPDATE public.wallet_cluster_members m
       SET revoked_at = now()
      FROM cluster_canon cc
     WHERE cc.cluster_id = m.cluster_id
       AND cc.canon_id <> m.cluster_id
       AND m.revoked_at IS NULL;
    GET DIAGNOSTICS v_revoked = ROW_COUNT;

    SELECT COUNT(*) INTO v_remaining
      FROM (
        SELECT chain, address
          FROM public.wallet_cluster_members
         WHERE revoked_at IS NULL
         GROUP BY chain, address
        HAVING COUNT(*) > 1
      ) dupes;
    IF v_remaining > 0 THEN
        RAISE EXCEPTION
            'cluster canonicalization left % wallet(s) with several active memberships', v_remaining;
    END IF;

    -- The invariant the entry gate depends on, enforced by the database rather than
    -- by every caller remembering to deduplicate.
    CREATE UNIQUE INDEX IF NOT EXISTS wallet_cluster_members_one_active_idx
        ON public.wallet_cluster_members (chain, address)
        WHERE revoked_at IS NULL;

    EXECUTE format(
        'COMMENT ON INDEX public.wallet_cluster_members_one_active_idx IS %L',
        'REV-072-F06: at most one ACTIVE membership per (chain, address). COUNT(DISTINCT cluster_id) is a hard entry gate documented as "two independent clusters"; without this index one wallet in two active clusters satisfied it alone.'
    );

    IF v_merged > 0 OR v_revoked > 0 THEN
        INSERT INTO schema_cutover_events (cutover, detail)
        VALUES (
            'wallet_cluster_membership_canonicalization',
            jsonb_build_object(
                'migration', '1032_rev072_signal_tenancy_and_cluster_canonicalization.sql',
                'memberships_repointed', v_merged,
                'memberships_revoked', v_revoked,
                'propagation_passes', v_iterations,
                'reason', 'a wallet active in several clusters counted as several independent clusters in the entry gate; overlapping clusters are one component and are merged onto the smallest cluster id.'
            )
        );
        RAISE NOTICE 'canonicalized cluster membership: % re-pointed, % revoked', v_merged, v_revoked;
    END IF;
END$part_a$;

-- ---------------------------------------------------------------------------
-- Part B: workspace ownership for signal output
-- ---------------------------------------------------------------------------
DO $part_b$
DECLARE
    v_default_workspace bigint;
    v_signals           bigint := 0;
    v_evaluations       bigint := 0;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'signals'
    ) OR NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'signal_evaluations'
    ) THEN
        RAISE NOTICE 'signals/signal_evaluations absent (canonical-only lane); nothing to scope';
        RETURN;
    END IF;

    -- The workspace every pre-tenancy row belongs to. Seeded by 1019.
    IF NOT EXISTS (SELECT 1 FROM workspaces WHERE slug = 'default') THEN
        INSERT INTO workspaces (name, slug) VALUES ('Default', 'default');
    END IF;
    SELECT id INTO v_default_workspace FROM workspaces WHERE slug = 'default';

    ALTER TABLE public.signals
        ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id);
    ALTER TABLE public.signal_evaluations
        ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id);

    UPDATE public.signals SET workspace_id = v_default_workspace WHERE workspace_id IS NULL;
    GET DIAGNOSTICS v_signals = ROW_COUNT;
    UPDATE public.signal_evaluations SET workspace_id = v_default_workspace WHERE workspace_id IS NULL;
    GET DIAGNOSTICS v_evaluations = ROW_COUNT;

    ALTER TABLE public.signals          ALTER COLUMN workspace_id SET NOT NULL;
    ALTER TABLE public.signal_evaluations ALTER COLUMN workspace_id SET NOT NULL;

    -- Tenancy-first indexes: every read filters by workspace, so it leads the key.
    CREATE INDEX IF NOT EXISTS signals_workspace_created_idx
        ON public.signals (workspace_id, created_at DESC);
    CREATE INDEX IF NOT EXISTS signal_evaluations_workspace_time_idx
        ON public.signal_evaluations (workspace_id, chain, mint, evaluated_at);

    EXECUTE format(
        'COMMENT ON COLUMN public.signals.workspace_id IS %L',
        'REV-072-F06: owning workspace. Acceptance depends on workspace-scoped policy, so an untenanted signal row is a cross-tenant disclosure: every read, report, and alert dispatch MUST filter on it.'
    );
    EXECUTE format(
        'COMMENT ON COLUMN public.signal_evaluations.workspace_id IS %L',
        'REV-072-F06: owning workspace. A rejection code is a policy answer and belongs to the tenant whose policy produced it.'
    );

    IF v_signals > 0 OR v_evaluations > 0 THEN
        INSERT INTO schema_cutover_events (cutover, detail)
        VALUES (
            'signal_output_workspace_backfill',
            jsonb_build_object(
                'migration', '1032_rev072_signal_tenancy_and_cluster_canonicalization.sql',
                'assigned_workspace_id', v_default_workspace,
                'signals_backfilled', v_signals,
                'signal_evaluations_backfilled', v_evaluations,
                'reason', 'pre-tenancy rows (0001_initial) had no recorded owner; assigned to the default workspace. Re-assign deliberately if this database was ever multi-tenant.'
            )
        );
        RAISE NOTICE 'assigned % signal row(s) and % evaluation row(s) to workspace %',
            v_signals, v_evaluations, v_default_workspace;
    END IF;
END$part_b$;
