-- ============================================================================
-- Signal Forge — Migration 1030: REV-051 C4 / REV-050-F04 coverage disclosure
-- Canonical source: REVIEW_RESULT.md REV-048 ("Add visible coverage metadata"),
--                   REV-050-F04, REV-051 "Coverage disclosure".
--
-- THE PROBLEM THIS CLOSES
-- The Recent production worker resolves relations with
--
--     deployer:        None
--     authority:       None
--     initial_funder:  None
--
-- so every deployer/authority/funder-derived relation type fails closed and
-- resolves nothing. That part is correct. What was NOT correct is that the event
-- it appends carried `coverage = 'full'`, which tells a reader "we looked and
-- there is no reuse" when the truth is "we never had the input". REV-048 states
-- the requirement directly: '"No relation" must not be rendered as "no reuse"',
-- and it names the shape of the disclosure — `coverage: partial` plus the list of
-- missing inputs.
--
-- `coverage` already exists and its CHECK already permits `degraded`, which is
-- the vocabulary's name for partial. What did not exist is anywhere to record
-- WHICH inputs were absent, so a consumer could not distinguish "degraded because
-- the deployer was never extracted" from "degraded because a provider was down".
-- A generic level with no subject is not a disclosure.
--
-- WHY A COLUMN AND NOT AN EVIDENCE REF
-- `evidence_refs` is evidence FOR the relation. A missing input is the absence of
-- evidence, and putting it in the same list makes an absence look like a source.
-- It gets its own column, defaulting to the empty list so every existing row
-- keeps its current meaning (nothing claimed about missing inputs).
--
-- Forward-only: 1001..1029 are shipped and immutable (CONVENTIONS.md decision on
-- release baselines, REV-045-F02). Idempotent on both lanes.
--
-- APPEND-ONLY IS NOT VIOLATED. Migration 1019 installs BEFORE UPDATE OR DELETE
-- row triggers on `recent_events`. `ADD COLUMN ... DEFAULT` is DDL, not an UPDATE
-- statement, so no row trigger fires; on PostgreSQL 11+ the default is stored in
-- catalog metadata and existing rows are not rewritten at all.
-- ============================================================================

ALTER TABLE recent_events
    ADD COLUMN IF NOT EXISTS missing_inputs jsonb NOT NULL DEFAULT '[]'::jsonb;

-- Shape guard: the disclosure is a LIST of input names. A scalar or object here
-- would be silently accepted by `jsonb` and would break every reader.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE contype = 'c'
           AND conname = 'recent_events_missing_inputs_is_array'
    ) THEN
        ALTER TABLE recent_events
            ADD CONSTRAINT recent_events_missing_inputs_is_array
            CHECK (jsonb_typeof(missing_inputs) = 'array');
    END IF;
END$$;

-- A non-empty missing-input list and `coverage = 'full'` are contradictory
-- claims: "these inputs were absent" cannot coexist with "coverage is complete".
-- Enforced in the database because the Rust side is not the only possible writer,
-- and because REV-048's requirement is a property of the DATA, not of one caller.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE contype = 'c'
           AND conname = 'recent_events_missing_inputs_not_full'
    ) THEN
        ALTER TABLE recent_events
            ADD CONSTRAINT recent_events_missing_inputs_not_full
            CHECK (missing_inputs = '[]'::jsonb OR coverage <> 'full');
    END IF;
END$$;

COMMENT ON COLUMN recent_events.missing_inputs IS 'REV-050-F04/REV-051: authoritative inputs that were NOT available when this event was resolved (e.g. deployer, authority, initial_funder). Non-empty implies coverage <> full: "no relation" must never be presented as "no reuse".';

-- The runtime role appends these rows; the column inherits the table grant, so no
-- privilege change is required. Stated so a future reader does not add a blanket
-- GRANT "to be safe" and undo 1028's per-operation posture.
