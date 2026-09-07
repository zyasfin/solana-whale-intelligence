-- ============================================================================
-- Signal Forge — Migration 1019a: admin_sessions baseline for the canonical lane
-- Canonical source: REVIEW_RESULT.md REV-027-F10 / REV-028-F07.
--
-- Problem: shipped migration 1020 does `ALTER TABLE admin_sessions ADD COLUMN
-- workspace_id`, but `admin_sessions` is created only by LEGACY 0002_admin.sql.
-- A canonical-only lane (1001..1020) therefore aborts at 1020 with
-- `relation "admin_sessions" does not exist`, which is exactly the failure the
-- reviewer reproduced.
--
-- Shipped migrations are immutable, so 1020 cannot be edited. Instead this file
-- is named to sort BETWEEN 1019 and 1020 ("1019a" < "1020" lexicographically,
-- which is the order the runner in `db::migrate` uses), so it runs as a
-- prerequisite on a fresh canonical database.
--
-- It is fully idempotent: on a legacy-upgraded database where 0002 already
-- created the table (and 1020 may already have run), every statement is a
-- no-op. Column shape matches 0002_admin.sql exactly so both lanes converge.
-- ============================================================================

CREATE TABLE IF NOT EXISTS admin_sessions (
    token_hash   text PRIMARY KEY,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    last_seen_at timestamptz,
    ip           text,
    user_agent   text
);

CREATE INDEX IF NOT EXISTS admin_sessions_expiry_idx ON admin_sessions (expires_at);
