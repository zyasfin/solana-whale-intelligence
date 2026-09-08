-- ============================================================================
-- Signal Forge — Migration 1041: positive-ID invariant
-- Canonical source: REVIEW_RESULT.md REV-093-F02.
--
-- THE DEFECT
-- `db.rs::safe_int8_predicate` treats a dedup-key segment as unsafe unless it
-- denotes a POSITIVE bigint, and the preflight then reassigns such a row to the
-- default workspace. That is correct only if a non-positive id can never name a
-- real row — and nothing in the schema said so. `workspaces.id` is
-- GENERATED ALWAYS AS IDENTITY (1001:39) and `funding_radar_cases.id` is
-- bigserial (0001:258); both start at 1 in practice, but `OVERRIDING SYSTEM
-- VALUE` (and a direct setval) can store 0 or a negative. On such a database the
-- guard would classify a SCHEMA-LEGAL owner as unsafe and silently move its
-- alerts to the default workspace — the exact cross-tenant corruption REV-090-F01
-- and REV-093-F01 were about, arriving through the other door.
--
-- THE POLICY (owner decision, REV-093-F02 option 2)
-- Enforce `id > 0` in the schema, so the production predicate's assumption is a
-- fact rather than a hope. The alternative — accepting every schema-legal bigint
-- including zero — was rejected because it would weaken the guard back into
-- accepting ids that no identity/bigserial sequence can legitimately produce.
--
-- FAIL CLOSED, NEVER REPAIR SILENTLY
-- If any offending row already exists, this migration ABORTS with a named
-- diagnostic listing the tables and ids. It does NOT reassign, delete, or
-- renumber anything: those rows may be referenced by alerts, events and outbox
-- identities, and picking a new id for someone else's data is not a decision a
-- migration may make. An operator reconciles, then re-runs.
--
-- SCOPE
-- Exactly the ID domains `safe_int8_predicate` is applied to:
--   * `workspaces.id`           — dedup-key segment 2 (the workspace segment);
--   * `funding_radar_cases.id`  — dedup-key segment 3 (the funding subject).
-- No other table's id is constrained here; widening the invariant beyond what the
-- predicate relies on would be scope this finding does not authorize.
--
-- Constraints are added as validated CHECKs (the default): PostgreSQL verifies
-- every existing row at ADD time, so a constraint that lands is proof the table
-- already satisfies it.
--
-- Forward-only: 1001..1040 are shipped and immutable. Idempotent.
-- NOTE: no dollar-dollar sequence in comments (the 1031 lesson).
-- ============================================================================

DO $$
DECLARE
    v_bad_workspaces bigint := 0;
    v_bad_cases      bigint := 0;
    v_detail         text   := '';
BEGIN
    -- ------------------------------------------------------------------
    -- Detection FIRST, before any DDL. A partially-constrained schema is
    -- worse than an unconstrained one.
    -- ------------------------------------------------------------------
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'workspaces'
    ) THEN
        SELECT count(*) INTO v_bad_workspaces FROM public.workspaces WHERE id <= 0;
        IF v_bad_workspaces > 0 THEN
            SELECT string_agg(id::text, ', ' ORDER BY id)
              INTO v_detail
              FROM (SELECT id FROM public.workspaces WHERE id <= 0 ORDER BY id LIMIT 20) s;
            v_detail := 'workspaces.id: ' || v_detail;
        END IF;
    END IF;

    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'funding_radar_cases'
    ) THEN
        SELECT count(*) INTO v_bad_cases FROM public.funding_radar_cases WHERE id <= 0;
        IF v_bad_cases > 0 THEN
            v_detail := CASE WHEN v_detail = '' THEN '' ELSE v_detail || '; ' END
                     || 'funding_radar_cases.id: '
                     || (SELECT string_agg(id::text, ', ' ORDER BY id)
                           FROM (SELECT id FROM public.funding_radar_cases
                                  WHERE id <= 0 ORDER BY id LIMIT 20) s);
        END IF;
    END IF;

    IF v_bad_workspaces > 0 OR v_bad_cases > 0 THEN
        RAISE EXCEPTION
            'positive-ID invariant violated: % workspace row(s) and % funding-case row(s) carry id <= 0 (%). '
            'Migration 1041 enforces id > 0 because db.rs::safe_int8_predicate treats a non-positive '
            'dedup-key segment as unsafe and would silently reassign such an owner to the default '
            'workspace. These rows are NOT repaired automatically: they may be referenced by alerts, '
            'funding_radar_events and outbox dedup keys, so renumbering them is an operator decision. '
            'Reconcile the offending rows, then re-run the migrator',
            v_bad_workspaces, v_bad_cases, v_detail
        USING ERRCODE = 'check_violation';
    END IF;

    -- ------------------------------------------------------------------
    -- Only now, with the data proven clean, install the invariant.
    -- ------------------------------------------------------------------
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'workspaces'
    ) AND NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'workspaces_id_positive_check'
    ) THEN
        ALTER TABLE public.workspaces
            ADD CONSTRAINT workspaces_id_positive_check CHECK (id > 0);
    END IF;

    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'funding_radar_cases'
    ) AND NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'funding_radar_cases_id_positive_check'
    ) THEN
        ALTER TABLE public.funding_radar_cases
            ADD CONSTRAINT funding_radar_cases_id_positive_check CHECK (id > 0);
    END IF;

    -- ------------------------------------------------------------------
    -- Replay-safe cutover record (1038 made `cutover` unique).
    -- ------------------------------------------------------------------
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'schema_cutover_events'
    ) AND NOT EXISTS (
        SELECT 1 FROM schema_cutover_events WHERE cutover = 'positive_id_invariant'
    ) THEN
        INSERT INTO schema_cutover_events (cutover, detail)
        VALUES (
            'positive_id_invariant',
            jsonb_build_object(
                'migration', '1041_rev093_positive_id_invariant.sql',
                'constraints', jsonb_build_array(
                    'workspaces_id_positive_check',
                    'funding_radar_cases_id_positive_check'
                ),
                'reason', 'REV-093-F02: db.rs::safe_int8_predicate refuses non-positive dedup-key segments and the preflight reassigns such rows to the default workspace, but no schema rule guaranteed ids are positive. OVERRIDING SYSTEM VALUE could store 0, making a schema-legal owner unsafe by the guard''s reckoning. The invariant is now enforced where the predicate relies on it; pre-existing violations abort the migration with a named diagnostic rather than being silently reassigned, deleted, or renumbered.'
            )
        );
    END IF;
END$$;
