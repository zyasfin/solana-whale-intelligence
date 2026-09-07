-- ============================================================================
-- Signal Forge — Phase 2 Migration 1010: Caller + provenance schema enrichment
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §8.5 (Caller
--   Intelligence) and §4.3 (browser worker).
--
-- Extends Phase 0 schema (1001-1008) with Phase 2 domain: caller outcomes
-- (MFE/MAE, copy PnL, +21d windows), copy-caller propagation graph, and browser
-- worker capture. Immutable migrations; does NOT alter frozen tables.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Caller outcomes (doc §8.5): MFE/MAE, realistic copy entry/PnL, outcome
-- windows through +21d. Appended per call; history preserved.
-- ---------------------------------------------------------------------------
CREATE TABLE caller_outcomes (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    call_id         bigint NOT NULL REFERENCES calls (id),
    mfe             double precision,      -- maximum favorable excursion
    mae             double precision,      -- maximum adverse excursion
    copy_entry      numeric,               -- realistic copy entry price
    copy_pnl        numeric,               -- realistic copy PnL
    outcome_window_days integer NOT NULL DEFAULT 21 CHECK (outcome_window_days BETWEEN 1 AND 21),
    measured_at     timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX caller_outcomes_call_idx ON caller_outcomes (call_id);

-- ---------------------------------------------------------------------------
-- Copy-caller propagation graph (doc §8.5): a caller propagates a call made by
-- another caller. Modeled as an edge; source/truth/confidence per edge.
-- ---------------------------------------------------------------------------
CREATE TABLE caller_propagation_edges (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    from_caller_id  bigint NOT NULL REFERENCES callers (id),
    to_caller_id    bigint NOT NULL REFERENCES callers (id),
    call_id         bigint REFERENCES calls (id),
    occurred_at     timestamptz NOT NULL,
    truth_status    text NOT NULL DEFAULT 'unknown'
                    CHECK (truth_status IN ('unknown', 'confirmed', 'disputed', 'superseded', 'erroneous')),
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    source_id       bigint REFERENCES sources (id),
    valid_from      timestamptz NOT NULL DEFAULT now(),
    valid_until     timestamptz,
    supersedes_id   bigint REFERENCES caller_propagation_edges (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT caller_propagation_distinct CHECK (from_caller_id <> to_caller_id)
);
CREATE INDEX caller_propagation_from_idx ON caller_propagation_edges (from_caller_id);
CREATE INDEX caller_propagation_to_idx ON caller_propagation_edges (to_caller_id);

-- ---------------------------------------------------------------------------
-- Browser worker captures (doc §4.3): raw payload/media capture with parser
-- version and challenge/session health reporting.
-- ---------------------------------------------------------------------------
CREATE TABLE browser_captures (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    platform        text NOT NULL CHECK (platform IN ('x', 'tiktok')),
    task_type       text NOT NULL CHECK (task_type IN ('search', 'profile', 'list', 'token_triggered_resolve')),
    target          text NOT NULL,
    raw_ref         text NOT NULL,         -- content-addressed raw payload/media
    parser_version  text,
    challenge_health text NOT NULL DEFAULT 'ok'
                    CHECK (challenge_health IN ('ok', 'challenge_detected', 'failed')),
    session_health  text NOT NULL DEFAULT 'ok'
                    CHECK (session_health IN ('ok', 'expired', 'invalid')),
    captured_at     timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX browser_captures_target_idx ON browser_captures (platform, target, captured_at);

-- ---------------------------------------------------------------------------
-- Media enrichment (ASR/OCR) — only shortlisted candidates (doc §4.3 cheap-first).
-- ---------------------------------------------------------------------------
CREATE TABLE media_enrichments (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    capture_id      bigint NOT NULL REFERENCES browser_captures (id),
    kind            text NOT NULL CHECK (kind IN ('audio', 'image', 'video')),
    raw_ref         text NOT NULL,
    transcript      text,                  -- ASR output
    text            text,                  -- OCR output
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX media_enrichments_capture_idx ON media_enrichments (capture_id);
