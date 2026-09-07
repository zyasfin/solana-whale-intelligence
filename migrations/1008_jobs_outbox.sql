-- ============================================================================
-- Signal Forge — Phase 0 Migration 1008: Jobs/outbox
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §11 "Job model"
--   (doc 762-780) + §11 priority triggers.
--
-- Job model fields:
--   job_type, entity_key, priority, available_at, lease_owner, lease_until,
--   attempts, max_attempts, dedupe_key, payload_version.
-- Workers claim via FOR UPDATE SKIP LOCKED. Backoff, dead-letter incident, and
-- operator retry are explicit.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Jobs: the outbox/queue table. Claimed via FOR UPDATE SKIP LOCKED by workers.
-- Dead-letter and operator retry are explicit (no blind retry; principle #10).
-- ---------------------------------------------------------------------------
CREATE TABLE jobs (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    job_type        text NOT NULL,
    entity_key      text NOT NULL,
    priority        integer NOT NULL DEFAULT 0,
    available_at    timestamptz NOT NULL DEFAULT now(),
    lease_owner     text,                   -- worker id currently holding the lease
    lease_until     timestamptz,
    attempts        integer NOT NULL DEFAULT 0,
    max_attempts    integer NOT NULL DEFAULT 5,
    dedupe_key      text,                   -- dedupe/idempotency
    payload_version text,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    status          text NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'leased', 'done', 'dead', 'cancelled')),
    last_error      jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_claim_idx ON jobs (status, priority, available_at)
    WHERE status = 'pending';
CREATE INDEX jobs_lease_idx ON jobs (lease_owner, lease_until);
CREATE INDEX jobs_dedupe_idx ON jobs (dedupe_key) WHERE dedupe_key IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Job dead letters: dead-lettered jobs become an incident (doc: "dead-letter
-- incident"). We do not hard-delete jobs; terminal state is tracked and linked
-- to an incident for operator retry.
-- ---------------------------------------------------------------------------
CREATE TABLE job_dead_letters (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id          bigint NOT NULL REFERENCES jobs (id),
    incident_id     bigint REFERENCES incidents (id),
    reason          text,
    retryable       boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX job_dead_letters_job_idx ON job_dead_letters (job_id);
