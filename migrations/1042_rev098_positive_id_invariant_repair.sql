-- ============================================================================
-- Signal Forge — Migration 1042: table-qualified / exact repair of 1041
-- Canonical source: REVIEW_RESULT.md REV-098-F01.
--
-- WHY THIS FILE EXISTS AT ALL
-- REV-097 implemented the REV-096-F02 correction by EDITING 1041 in place. 1041
-- was already shipped and applied, so its ledger digest no longer matched the
-- file on disk and every database that had run REV-095/096 refused to upgrade:
--
--     Error: applied migration `1041_rev093_positive_id_invariant.sql`
--            no longer matches the file on disk
--
-- 1041 has therefore been restored byte-for-byte to its shipped form
-- (sha256 d7a8984d…) and the correction lives here, forward-only, where it can
-- reach databases that already applied the original.
--
-- WHAT 1041 GOT WRONG
--   1. `SELECT 1 FROM pg_constraint WHERE conname = '<name>'` is NOT qualified by
--      `conrelid`. Constraint names are unique per table, not per database, so a
--      same-named constraint anywhere else (a decoy, or an unrelated table that
--      happened to pick the name) satisfied the NOT EXISTS and 1041 skipped
--      installing the invariant on the table that actually needs it. From then on
--      `db.rs::safe_int8_predicate` relied on a rule nothing enforced.
--   2. Existence is not correctness. A constraint of the right NAME with the wrong
--      DEFINITION (`CHECK (id >= 0)`, `CHECK (id <> 5)`, …) also satisfied the
--      NOT EXISTS, and was silently accepted as the invariant.
--
-- WHAT THIS MIGRATION DOES
-- For `public.workspaces` and `public.funding_radar_cases`, independently:
--   * look the constraint up BY (conname, conrelid) — the only correct key;
--   * absent  → validate the data first, then install `CHECK (id > 0)`;
--   * present → compare the SERVER's own rendering (`pg_get_constraintdef`) with
--               whitespace normalized, and RAISE on anything other than
--               `CHECK((id>0))`. Comparing the server's rendering rather than our
--               own text means quoting and spacing differences can never read as
--               drift, and a wrong constraint can never read as satisfied.
--
-- Detection runs for BOTH tables before ANY DDL, and reports both, so a database
-- with violations in each does not get a partially-constrained schema plus a
-- diagnostic naming only the first table.
--
-- FAIL CLOSED, NEVER REPAIR SILENTLY: offending rows are reported, never
-- renumbered or deleted — they may be referenced by alerts, funding_radar_events
-- and outbox dedup identities. Same policy as 1041.
--
-- Idempotent: on a database where 1041 already installed both constraints
-- correctly, every branch is a no-op and no DDL is issued.
-- NOTE: no dollar-dollar sequence in comments (the 1031 lesson).
-- ============================================================================

DO $$
DECLARE
    v_bad_workspaces bigint := 0;
    v_bad_cases      bigint := 0;
    v_detail         text   := '';
    v_def            text;
    v_has_workspaces boolean;
    v_has_cases      boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'workspaces'
    ) INTO v_has_workspaces;
    SELECT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'funding_radar_cases'
    ) INTO v_has_cases;

    -- ------------------------------------------------------------------
    -- Definition check FIRST: a wrong constraint is a fault regardless of
    -- whether any row currently violates it.
    -- ------------------------------------------------------------------
    IF v_has_workspaces THEN
        SELECT pg_get_constraintdef(oid) INTO v_def
          FROM pg_constraint
         WHERE conname = 'workspaces_id_positive_check'
           AND conrelid = 'public.workspaces'::regclass;
        IF v_def IS NOT NULL AND regexp_replace(v_def, '\s+', '', 'g') <> 'CHECK((id>0))' THEN
            RAISE EXCEPTION
                'constraint workspaces_id_positive_check exists on public.workspaces with an '
                'unexpected definition (%); the positive-ID invariant cannot be assumed. '
                'Reconcile it deliberately rather than letting this migration accept it',
                v_def
            USING ERRCODE = 'check_violation';
        END IF;
    END IF;

    IF v_has_cases THEN
        SELECT pg_get_constraintdef(oid) INTO v_def
          FROM pg_constraint
         WHERE conname = 'funding_radar_cases_id_positive_check'
           AND conrelid = 'public.funding_radar_cases'::regclass;
        IF v_def IS NOT NULL AND regexp_replace(v_def, '\s+', '', 'g') <> 'CHECK((id>0))' THEN
            RAISE EXCEPTION
                'constraint funding_radar_cases_id_positive_check exists on '
                'public.funding_radar_cases with an unexpected definition (%); the '
                'positive-ID invariant cannot be assumed. Reconcile it deliberately',
                v_def
            USING ERRCODE = 'check_violation';
        END IF;
    END IF;

    -- ------------------------------------------------------------------
    -- Data detection for BOTH tables before ANY DDL (REV-096-F05).
    -- ------------------------------------------------------------------
    IF v_has_workspaces THEN
        SELECT count(*) INTO v_bad_workspaces FROM public.workspaces WHERE id <= 0;
        IF v_bad_workspaces > 0 THEN
            SELECT string_agg(id::text, ', ' ORDER BY id)
              INTO v_detail
              FROM (SELECT id FROM public.workspaces WHERE id <= 0 ORDER BY id LIMIT 20) s;
            v_detail := 'workspaces.id: ' || v_detail;
        END IF;
    END IF;

    IF v_has_cases THEN
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
            'Migration 1042 repairs the table-qualified installation 1041 could skip, and enforces '
            'id > 0 because db.rs::safe_int8_predicate treats a non-positive dedup-key segment as '
            'unsafe and would silently reassign such an owner to the default workspace. These rows '
            'are NOT repaired automatically: they may be referenced by alerts, funding_radar_events '
            'and outbox dedup keys, so renumbering them is an operator decision. Reconcile the '
            'offending rows, then re-run the migrator',
            v_bad_workspaces, v_bad_cases, v_detail
        USING ERRCODE = 'check_violation';
    END IF;

    -- ------------------------------------------------------------------
    -- Data proven clean and no wrong definition present: install whatever
    -- 1041 skipped. Table-qualified lookup, so a same-named decoy elsewhere
    -- cannot stand in for the real invariant.
    -- ------------------------------------------------------------------
    IF v_has_workspaces AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'workspaces_id_positive_check'
           AND conrelid = 'public.workspaces'::regclass
    ) THEN
        ALTER TABLE public.workspaces
            ADD CONSTRAINT workspaces_id_positive_check CHECK (id > 0);
    END IF;

    IF v_has_cases AND NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'funding_radar_cases_id_positive_check'
           AND conrelid = 'public.funding_radar_cases'::regclass
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
        SELECT 1 FROM schema_cutover_events WHERE cutover = 'positive_id_invariant_repair'
    ) THEN
        INSERT INTO schema_cutover_events (cutover, detail)
        VALUES (
            'positive_id_invariant_repair',
            jsonb_build_object(
                'migration', '1042_rev098_positive_id_invariant_repair.sql',
                'constraints', jsonb_build_array(
                    'workspaces_id_positive_check',
                    'funding_radar_cases_id_positive_check'
                ),
                'reason', 'REV-098-F01: 1041 looked its constraints up by conname alone and accepted mere existence. A same-named constraint on any other table made it skip installation, and a right-named constraint with a wrong definition was accepted as the invariant. 1041 is shipped and applied, so it was restored byte-for-byte and the correction ships here, forward-only: lookups are keyed by (conname, conrelid) and the server rendering of an existing constraint must be exactly CHECK ((id > 0)).'
            )
        );
    END IF;
END $$;
