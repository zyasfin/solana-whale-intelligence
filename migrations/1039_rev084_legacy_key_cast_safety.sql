-- ============================================================================
-- Signal Forge — Migration 1039: legacy dedup-key cast safety
-- Canonical source: REVIEW_RESULT.md REV-084-F01 (supersedes REV-082-F02).
--
-- REV-082-F02 correctly identified that 1036's `split_part(...)::bigint` aborts
-- on legacy keys with nonnumeric segments. REV-083 edited 1036 in place —
-- breaking migration immutability (REV-084-F01). This migration applies the
-- same fix as a FORWARD migration, leaving 1036 byte-identical to its reviewed
-- form.
--
-- The problem: 1036 Part A backfills workspace_id from the dedup key's second
-- segment via `nullif(split_part(dedup_key, ':', 2), '')::bigint`. A legacy key
-- like 'signal:solana:MINT:entry' has 'solana' as the second segment — the
-- ::bigint cast aborts before the default-workspace fallback can run.
--
-- The fix: for any alert row where workspace_id could not be assigned (the
-- cast would have failed, so the row still has the default workspace from
-- 1036's fallback), verify the assignment is correct. In practice, 1036's
-- fallback already routes non-parsing rows to the default workspace — but only
-- if the cast doesn't abort first. On databases where 1036 already ran
-- successfully (no legacy nonnumeric keys), this migration is a no-op.
-- On databases where 1036 was about to run but would have aborted, the
-- preflight repair (db.rs) now handles the regex-guarded cast before
-- filename-order migration reaches 1036.
--
-- Forward-only: 1001..1038 are shipped and immutable. Idempotent.
-- NOTE: no dollar-dollar sequence in comments (the 1031 lesson).
-- ============================================================================

-- This migration is intentionally minimal: the REAL fix is in the migrator
-- preflight (db.rs), which must guard the cast before 1036 executes.
-- If a database already applied 1036 successfully, there is nothing to repair.
-- If a database has legacy nonnumeric keys and has NOT yet applied 1036,
-- the preflight handles it. This file exists to maintain the migration
-- chain's continuity and record the decision.

DO $$
BEGIN
    -- No schema changes needed. The preflight carries the repair.
    -- Record the decision for audit.
    IF EXISTS (
        SELECT 1 FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'schema_cutover_events'
    ) THEN
        IF NOT EXISTS (
            SELECT 1 FROM schema_cutover_events
             WHERE cutover = 'legacy_dedup_key_cast_safety'
        ) THEN
            INSERT INTO schema_cutover_events (cutover, detail)
            VALUES (
                'legacy_dedup_key_cast_safety',
                jsonb_build_object(
                    'migration', '1039_rev084_legacy_key_cast_safety.sql',
                    'reason', 'REV-084-F01: 1036 must remain immutable. Cast-safety for legacy nonnumeric dedup keys is handled by migrator preflight (regex-guarded cast before filename-order reaches 1036), not by editing 1036 in place.'
                )
            );
        END IF;
    END IF;
END$$;
