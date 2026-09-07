-- ============================================================================
-- Signal Forge — Phase 0 Migration 1006: Strategy/evaluation
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §10 "Strategy/evaluation"
--   (strategy_sources, strategies, strategy_versions, strategy_requirements,
--    strategy_evaluations, shadow_outcomes, lessons, lesson_validations).
--
-- Lesson lifecycle (doc): PROPOSED -> VALIDATED -> ACTIVE -> RETIRED.
-- No direct auto-activation from prose or tiny samples.
-- Strategy activation follows shadow/paper/validation/approval lifecycle (gate #14).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Strategy sources: where a strategy definition originates (versioned policy).
-- ---------------------------------------------------------------------------
CREATE TABLE strategy_sources (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    name            text NOT NULL,
    source_type     text NOT NULL,          -- manual / imported / generated
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Strategies: a versioned policy component (doc: strategy is a versioned policy
-- component). The strategy itself is a container; versions carry the policy.
-- ---------------------------------------------------------------------------
CREATE TABLE strategies (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    source_id       bigint REFERENCES strategy_sources (id),
    name            text NOT NULL,
    status          text NOT NULL DEFAULT 'draft'
                    CHECK (status IN ('draft', 'active', 'paused', 'retired')),
    current_version_id bigint,              -- FK added below (strategy_versions)
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Strategy versions: immutable versioned policy snapshots.
-- ---------------------------------------------------------------------------
CREATE TABLE strategy_versions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    strategy_id     bigint NOT NULL REFERENCES strategies (id),
    version         integer NOT NULL,
    policy          jsonb NOT NULL DEFAULT '{}'::jsonb,   -- the frozen policy content
    policy_hash     text NOT NULL,          -- content hash for reproducibility (gate #2)
    lifecycle_state text NOT NULL DEFAULT 'draft'
                    CHECK (lifecycle_state IN ('draft', 'shadow', 'paper', 'validated', 'approved', 'active', 'retired')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT strategy_versions_key UNIQUE (strategy_id, version)
);
CREATE INDEX strategy_versions_state_idx ON strategy_versions (lifecycle_state);

-- resolve circular FK
ALTER TABLE strategies
    ADD CONSTRAINT strategies_current_version_fk
    FOREIGN KEY (current_version_id) REFERENCES strategy_versions (id);

-- ---------------------------------------------------------------------------
-- Strategy requirements: declared preconditions a strategy must satisfy.
-- ---------------------------------------------------------------------------
CREATE TABLE strategy_requirements (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    strategy_id     bigint NOT NULL REFERENCES strategies (id),
    requirement_type text NOT NULL,         -- e.g. min_sample, horizon, mandatory_data
    requirement     jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX strategy_requirements_strategy_idx ON strategy_requirements (strategy_id);

-- ---------------------------------------------------------------------------
-- Strategy evaluations: walk-forward/holdout evaluation results.
-- ---------------------------------------------------------------------------
CREATE TABLE strategy_evaluations (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    strategy_version_id bigint NOT NULL REFERENCES strategy_versions (id),
    evaluation_type text NOT NULL,          -- walk_forward / holdout
    metrics         jsonb NOT NULL DEFAULT '{}'::jsonb,
    evaluated_at    timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX strategy_evaluations_version_idx ON strategy_evaluations (strategy_version_id);

-- ---------------------------------------------------------------------------
-- Shadow outcomes: shadow-mode results (no real capital).
-- ---------------------------------------------------------------------------
CREATE TABLE shadow_outcomes (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    strategy_version_id bigint NOT NULL REFERENCES strategy_versions (id),
    outcome         jsonb NOT NULL DEFAULT '{}'::jsonb,
    window_start    timestamptz NOT NULL,
    window_end      timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX shadow_outcomes_version_idx ON shadow_outcomes (strategy_version_id, window_start);

-- ---------------------------------------------------------------------------
-- Lessons: extracted learnings with gated lifecycle (no auto-activation).
-- ---------------------------------------------------------------------------
CREATE TABLE lessons (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    strategy_id     bigint REFERENCES strategies (id),
    body            jsonb NOT NULL DEFAULT '{}'::jsonb,
    lifecycle_state text NOT NULL DEFAULT 'proposed'
                    CHECK (lifecycle_state IN ('proposed', 'validated', 'active', 'retired')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX lessons_state_idx ON lessons (lifecycle_state);

-- ---------------------------------------------------------------------------
-- Lesson validations: evidence supporting a lesson's promotion.
-- ---------------------------------------------------------------------------
CREATE TABLE lesson_validations (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    lesson_id       bigint NOT NULL REFERENCES lessons (id),
    validation_type text NOT NULL,
    result          jsonb NOT NULL DEFAULT '{}'::jsonb,
    validated_at    timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX lesson_validations_lesson_idx ON lesson_validations (lesson_id);
