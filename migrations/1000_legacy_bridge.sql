-- ============================================================================
-- Signal Forge — Migration 1000: legacy → canonical upgrade bridge
-- Canonical source: REVIEW_RESULT.md REV-029 (REV-025-F10 PARTIAL),
--                   REV-007-F01, CONVENTIONS.md frozen decision #2 (replace-total).
--
-- THE FAILURE THIS REMOVES
-- Replaying the legacy baseline (0001..0010) and then the canonical schema
-- (1001+) aborted at `1005_intelligence.sql` with `relation "tokens" already
-- exists`. Six table names are created by BOTH lanes:
--
--     tokens, token_lifecycle_events, wallet_clusters, narratives   (canonical 1005)
--     funding_edges                                                 (canonical 1011)
--     admin_sessions                                                (1019a, already IF NOT EXISTS)
--
-- The two schemas are deliberately incompatible — decision #2 is "replace total",
-- not "merge" — so the canonical `CREATE TABLE` cannot be softened to
-- `IF NOT EXISTS`: that would silently keep legacy column shapes and let every
-- later canonical migration build on the wrong table.
--
-- WHY A SCHEMA MOVE, NOT A RENAME
-- Renaming to `legacy_tokens` is not enough: a table rename does NOT rename its
-- primary key or indexes, so `tokens_pkey`, `funding_edges_from_idx`, etc. would
-- still collide when the canonical lane recreates them (identifiers are unique
-- per schema). `ALTER TABLE ... SET SCHEMA` moves the table together with its
-- constraints and indexes, so every collision disappears at once.
--
-- Legacy data is MOVED, never dropped (archive-not-delete, principle #6). Foreign
-- keys from other legacy tables keep working: FKs are valid across schemas.
--
-- File name sorts as 0010 < 1000 < 1001, so the runner applies this bridge after
-- the legacy baseline and before the canonical schema.
--
-- On a FRESH canonical database none of these tables exist and every statement
-- below is a no-op; the file is idempotent in both lanes.
--
-- ⚠ OPERATOR CONSEQUENCE (deliberate, per decision #2)
-- After this bridge, unqualified `tokens` / `narratives` / `funding_edges` resolve
-- to the CANONICAL tables, which have different columns (`contract_address` vs
-- `mint`, `lifecycle` vs `lifecycle_state`, ...). The pre-freeze binary must NOT
-- be run against a bridged database; its queries target the legacy shapes, which
-- now live in `swi_legacy`. This is the replace-total cutover point, and it is
-- flagged here rather than discovered in production.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Archive schema for the pre-freeze tables.
-- ---------------------------------------------------------------------------
CREATE SCHEMA IF NOT EXISTS swi_legacy;

-- Single-line literal on purpose: adjacent string-literal continuation is legal
-- PostgreSQL but not portable across SQL parsers used in CI checks.
COMMENT ON SCHEMA swi_legacy IS 'Pre-freeze (0001..0010) tables archived by migration 1000 during the replace-total cutover. Retained for audit/backfill; not read by the canonical runtime.';

-- ---------------------------------------------------------------------------
-- Move each colliding legacy table out of `public`.
--
-- The guard checks that the table exists in `public` AND has a legacy-only column,
-- so a canonical table is never moved by mistake — even if this migration were
-- somehow replayed out of order. Each legacy fingerprint column below does not
-- exist on its canonical namesake.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    v_pairs text[][] := ARRAY[
        -- table                    legacy-only fingerprint column
        ARRAY['tokens',                 'mint'],
        ARRAY['token_lifecycle_events', 'mint'],
        ARRAY['narratives',             'slug'],
        ARRAY['wallet_clusters',        'cluster_id'],
        ARRAY['funding_edges',          'signature']
    ];
    v_pair   text[];
    v_table  text;
    v_column text;
BEGIN
    FOREACH v_pair SLICE 1 IN ARRAY v_pairs LOOP
        v_table  := v_pair[1];
        v_column := v_pair[2];

        IF EXISTS (
            SELECT 1
              FROM information_schema.columns
             WHERE table_schema = 'public'
               AND table_name = v_table
               AND column_name = v_column
        ) THEN
            EXECUTE format('ALTER TABLE public.%I SET SCHEMA swi_legacy', v_table);
            RAISE NOTICE 'legacy bridge: archived public.% -> swi_legacy.%', v_table, v_table;
        END IF;
    END LOOP;
END$$;

-- ---------------------------------------------------------------------------
-- Legacy tables that do NOT collide stay in `public` on purpose: they carry
-- pre-freeze evidence (raw_events, transfers, trades, telegram_*, wallet_*,
-- funding_observations, funding_radar_*, market_snapshots, signals, ...) and the
-- canonical lane never recreates those names. Moving them would break the legacy
-- read paths for no schema-conflict benefit.
--
-- Record the cutover so the state is auditable rather than inferred.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS schema_cutover_events (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    cutover     text NOT NULL,
    detail      jsonb NOT NULL DEFAULT '{}'::jsonb,
    applied_at  timestamptz NOT NULL DEFAULT now()
);

INSERT INTO schema_cutover_events (cutover, detail)
SELECT
    'legacy_to_canonical_bridge',
    jsonb_build_object(
        'archived_schema', 'swi_legacy',
        'archived_tables', (
            SELECT coalesce(jsonb_agg(table_name ORDER BY table_name), '[]'::jsonb)
              FROM information_schema.tables
             WHERE table_schema = 'swi_legacy'
        ),
        'migration', '1000_legacy_bridge.sql'
    )
WHERE EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'swi_legacy');
