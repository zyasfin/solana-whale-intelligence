-- ============================================================================
-- Signal Forge — Migration 1038: cutover event replay identity
-- Canonical source: REVIEW_RESULT.md REV-082-F06.
--
-- REV-080-F07 named migration 1035's duplicate cutover event on direct replay.
-- 1036 guarded its OWN event but explicitly left 1035's defect unchanged:
-- "the duplicate 1035 rows, where they exist, are history; the guard is the
-- rule going forward." That is not a fix for the reported finding — 1035's
-- INSERT remains unguarded and a direct replay appends a second row.
--
-- This migration:
--   1. Adds a UNIQUE constraint on schema_cutover_events.cutover so no future
--      migration can append a duplicate, regardless of whether it remembers
--      the NOT-EXISTS guard.
--   2. Reconciles existing duplicates (keeps the earliest, deletes the rest).
--
-- Forward-only: 1001..1037 are shipped and immutable. Idempotent.
-- NOTE: no dollar-dollar sequence in comments (the 1031 lesson).
-- ============================================================================

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'schema_cutover_events'
    ) THEN
        RETURN;
    END IF;

    -- Reconcile: keep the earliest row per cutover name, delete duplicates.
    DELETE FROM schema_cutover_events a
     USING schema_cutover_events b
     WHERE a.cutover = b.cutover
       AND a.id > b.id;

    -- Unique identity: one row per cutover, forever.
    CREATE UNIQUE INDEX IF NOT EXISTS schema_cutover_events_cutover_uidx
        ON schema_cutover_events (cutover);
END$$;
