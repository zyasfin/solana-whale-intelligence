-- ============================================================================
-- Signal Forge — Migration 1020: workspace binding for admin sessions
-- Canonical source: REVIEW_RESULT.md REV-025-F04.
--
-- The read-only API derives the workspace from the authenticated session, never
-- a hardcoded literal. This migration adds a nullable workspace_id to the legacy
-- admin session table; a session without a workspace binding fails closed (401)
-- at the API layer.
-- ============================================================================

ALTER TABLE admin_sessions
    ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id);

CREATE INDEX IF NOT EXISTS admin_sessions_workspace_idx
    ON admin_sessions (workspace_id);
