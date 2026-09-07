-- ============================================================================
-- Signal Forge — Phase 5 Migration 1013: Strategy Lab schema enrichment
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §13 "Strategy Lab
--   and evaluation". Strategy lifecycle: DRAFT/SHADOW/PAPER/VALIDATED/APPROVED/
--   CANARY/ACTIVE/PAUSED/RETIRED.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Extend strategy_versions with the full lifecycle state (doc §13). The Phase 0
-- schema used a draft/shadow/paper/validated/approved/active/retired subset;
-- this adds CANARY + PAUSED and records paper friction.
-- ---------------------------------------------------------------------------
ALTER TABLE strategy_versions
    DROP CONSTRAINT IF EXISTS strategy_versions_lifecycle_state_check;

ALTER TABLE strategy_versions
    ADD CONSTRAINT strategy_versions_lifecycle_check
    CHECK (lifecycle_state IN ('draft', 'shadow', 'paper', 'validated', 'approved',
                               'canary', 'active', 'paused', 'retired'));

-- ---------------------------------------------------------------------------
-- Paper execution with position-sized quote friction (doc §13 req #4/#5).
-- ---------------------------------------------------------------------------
CREATE TABLE paper_executions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    strategy_version_id bigint NOT NULL REFERENCES strategy_versions (id),
    quote           numeric NOT NULL,      -- position-sized executable quote
    fees            numeric,
    gas             numeric,
    tip             numeric,
    rent            numeric,
    slippage        numeric,
    latency         numeric,
    pnl             numeric,
    executed_at     timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX paper_executions_version_idx ON paper_executions (strategy_version_id);

-- ---------------------------------------------------------------------------
-- Shadow outcome enhancements (doc §13 req #2/#13): rejected-candidate shadow
-- outcomes + negative findings retained.
-- ---------------------------------------------------------------------------
ALTER TABLE shadow_outcomes
    ADD COLUMN rejected_candidates jsonb NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN negative_findings jsonb NOT NULL DEFAULT '[]'::jsonb;
