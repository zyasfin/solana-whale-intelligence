-- ============================================================================
-- Signal Forge — Migration 1029: REV-041 corrective (forward-only)
-- Canonical source: REVIEW_RESULT.md REV-041-F01 required remediation #2.
--
-- THE BUG CLASS: a migration that DROPs a constraint under one name and ADDs the
-- replacement under a DIFFERENT name is not idempotent across history.
--
--   1013_strategy_lab.sql (current):
--       DROP CONSTRAINT IF EXISTS strategy_versions_lifecycle_state_check;
--       ADD  CONSTRAINT          strategy_versions_lifecycle_check ...
--
--   1013_strategy_lab.sql (a historical revision of the SAME filename):
--       created strategy_versions_lifecycle_state_check
--
-- On a database that ran the historical file, the DROP in the current file misses
-- (the name it looks for is the one that IS there only in the other revision), so
-- the OLD restrictive definition survives. Reproduced live:
--
--     ledger claims 1013 = current file (sha 056ff03bd957)
--     constraint present = strategy_versions_lifecycle_state_check
--     lifecycle_state 'draft'  -> ACCEPTED
--     lifecycle_state 'active' -> ACCEPTED
--     lifecycle_state 'canary' -> REJECTED-BY-CHECK   <-- 1013 promises this works
--     lifecycle_state 'paused' -> REJECTED-BY-CHECK   <-- 1013 promises this works
--
-- So §13's frozen lifecycle (DRAFT/SHADOW/PAPER/VALIDATED/APPROVED/CANARY/ACTIVE/
-- PAUSED/RETIRED) is silently unavailable while every checksum looks green.
--
-- ENUMERATION, NOT JUST THIS INSTANCE. Scanning all 40 migrations for
-- `DROP CONSTRAINT IF EXISTS x` with no matching `ADD CONSTRAINT x` found two:
--
--   1013_strategy_lab.sql   drops strategy_versions_lifecycle_state_check  -> DRIFT
--   1019_rev022_corrective  drops social_identities_platform_user_key      -> NOT drift
--
-- The 1019 case is a deliberate REMOVAL (§4 replaces a non-versioned UNIQUE with a
-- partial unique index), so nothing needs re-adding. `DROP INDEX IF EXISTS` and
-- `DROP TRIGGER IF EXISTS` renames: none found. One real instance, and the guard
-- below keeps the class closed rather than just this row.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Reconcile the lifecycle constraint to the ONE definition §13 freezes,
-- regardless of which historical revision of 1013 this database ran.
--
-- Both names are dropped and the canonical one re-created, so the outcome is the
-- same whether the database carries the old name, the new name, both, or neither.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    v_bad text;
    v_conname text;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'strategy_versions'
    ) THEN
        RETURN;
    END IF;

    -- Refuse to widen a constraint while rows violate the target set: that would
    -- convert a schema problem into silent data acceptance. There is nothing to
    -- reject here (the target set is a SUPERSET of both historical sets), but the
    -- check is stated so a future edit to this block cannot skip it.
    SELECT string_agg(DISTINCT lifecycle_state, ', ')
      INTO v_bad
      FROM strategy_versions
     WHERE lifecycle_state IS NOT NULL
       AND lifecycle_state NOT IN ('draft', 'shadow', 'paper', 'validated',
                                   'approved', 'canary', 'active', 'paused', 'retired');
    IF v_bad IS NOT NULL THEN
        RAISE EXCEPTION
            'strategy_versions holds lifecycle_state values outside the frozen §13 set (%); '
            'resolve the data before reconciling the constraint', v_bad;
    END IF;

    -- Drop every CHECK constraint on lifecycle_state, whatever it is called, then
    -- create the canonical one.
    --
    -- REV-043-F01: naming the two spellings I happened to know about was the same
    -- guess that caused the original drift — `1013` did exactly that and missed.
    -- Discovery is by DEFINITION instead: `contype = 'c'` restricts this to CHECK
    -- constraints (so PG18's `contype = 'n'` NOT NULL metadata is untouched), and
    -- the `pg_attribute` join restricts it to constraints on THIS column. A third
    -- historical spelling nobody remembers is handled without knowing its name.
    FOR v_conname IN
        SELECT c.conname
          FROM pg_constraint c
          JOIN pg_attribute a
            ON a.attrelid = c.conrelid
           AND a.attnum = ANY (c.conkey)
         WHERE c.conrelid = 'strategy_versions'::regclass
           AND c.contype = 'c'
           AND a.attname = 'lifecycle_state'
    LOOP
        EXECUTE format('ALTER TABLE strategy_versions DROP CONSTRAINT %I', v_conname);
    END LOOP;

    EXECUTE 'ALTER TABLE strategy_versions
             ADD CONSTRAINT strategy_versions_lifecycle_check
             CHECK (lifecycle_state IN (''draft'', ''shadow'', ''paper'', ''validated'',
                                        ''approved'', ''canary'', ''active'', ''paused'',
                                        ''retired''))';
END$$;

-- ---------------------------------------------------------------------------
-- Class guard: assert the reconciled state, so a future in-place edit to a shipped
-- migration cannot leave a stale definition behind unnoticed.
--
-- This is a MIGRATION-time assertion rather than a test, because the condition it
-- protects is a property of a live database, not of the source tree. A source test
-- cannot see which revision of a file a given database actually ran — that is the
-- whole lesson of REV-041-F01.
--
-- REV-043-F01: the first version of this guard matched constraints by NAME:
--
--     conname LIKE '%lifecycle%' AND conname <> 'strategy_versions_lifecycle_check'
--
-- On PostgreSQL 18 that broke fresh installs. PG18 records NOT NULL constraints in
-- `pg_constraint` with `contype = 'n'`, so the perfectly legitimate metadata row
-- `strategy_versions_lifecycle_state_not_null` matched `LIKE '%lifecycle%'` and the
-- guard rejected it as stale. Migrations 1000..1028 applied, then 1029 aborted:
--
--     MIGRATE_EXIT=1
--     stale lifecycle constraint(s) remain on strategy_versions:
--     {strategy_versions_lifecycle_state_not_null}
--
-- The minimum fix is `contype = 'c'`. That is not what this does, for two reasons.
--
-- 1. A NAME is a guess about intent. What actually matters is the DEFINITION: is
--    there a CHECK on `lifecycle_state` whose permitted set differs from §13?
--    Selecting by definition is stable across catalog changes and across whatever
--    name a historical revision happened to use.
-- 2. `pg_get_constraintdef` + `conkey` describe the constraint's actual meaning, so
--    a future PostgreSQL adding another `contype` cannot make this guard fire on
--    unrelated metadata again.
--
-- I already knew this pattern: migration 1021's `drop_column_checks()` helper
-- filters `c.contype = 'c'` and joins `pg_attribute` to target a specific column.
-- I wrote that, then did not apply it here.
-- ---------------------------------------------------------------------------
DO $$
DECLARE
    v_stale text[];
    v_canonical_def text;
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'strategy_versions'
    ) THEN
        RETURN;
    END IF;

    -- Any CHECK constraint (contype 'c' ONLY) that constrains the lifecycle_state
    -- column and is not the canonical one. Selected by what it CONSTRAINS, not by
    -- what it is called: `conkey` names the column, so a historical revision using
    -- any other constraint name is still caught, and NOT NULL / foreign-key /
    -- unique / exclusion metadata can never be caught.
    SELECT array_agg(c.conname ORDER BY c.conname)
      INTO v_stale
      FROM pg_constraint c
      JOIN pg_attribute a
        ON a.attrelid = c.conrelid
       AND a.attnum = ANY (c.conkey)
     WHERE c.conrelid = 'strategy_versions'::regclass
       AND c.contype = 'c'
       AND a.attname = 'lifecycle_state'
       AND c.conname <> 'strategy_versions_lifecycle_check';

    IF v_stale IS NOT NULL THEN
        RAISE EXCEPTION
            'stale lifecycle CHECK constraint(s) remain on strategy_versions: %; '
            'the canonical name is strategy_versions_lifecycle_check', v_stale;
    END IF;

    -- The canonical constraint must exist AND permit the frozen §13 set. Asserting
    -- existence alone would pass if some future edit narrowed the definition while
    -- keeping the name — the exact failure mode 1013 had.
    SELECT pg_get_constraintdef(c.oid)
      INTO v_canonical_def
      FROM pg_constraint c
     WHERE c.conrelid = 'strategy_versions'::regclass
       AND c.contype = 'c'
       AND c.conname = 'strategy_versions_lifecycle_check';

    IF v_canonical_def IS NULL THEN
        RAISE EXCEPTION
            'strategy_versions_lifecycle_check is missing after reconciliation';
    END IF;

    IF v_canonical_def NOT LIKE '%canary%' OR v_canonical_def NOT LIKE '%paused%' THEN
        RAISE EXCEPTION
            'strategy_versions_lifecycle_check does not permit the frozen §13 states '
            '(canary/paused); definition is: %', v_canonical_def;
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- Verify after applying (on a database that ran the HISTORICAL 1013):
--
--   SELECT conname FROM pg_constraint
--    WHERE conrelid = 'strategy_versions'::regclass AND conname LIKE '%lifecycle%';
--   -- exactly: strategy_versions_lifecycle_check
--
--   INSERT ... lifecycle_state = 'canary'  -- ACCEPTED (was REJECTED)
--   INSERT ... lifecycle_state = 'paused'  -- ACCEPTED (was REJECTED)
--   INSERT ... lifecycle_state = 'bogus'   -- still REJECTED
-- ---------------------------------------------------------------------------
