-- ============================================================================
-- Signal Forge — Phase 7 Migration 1015: AUTO_BOUNDED schema enrichment
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §14/§15 autonomous
--   cycles + trading modes (AUTO_BOUNDED). Autonomous limit raises and LP open/
--   reseed are gated by reviewed forward results/evidence.
--
-- ===== FROZEN-DECISION DEFERRED =====
-- Measurable rollout thresholds (sample/horizon/loss/approval) are NOT frozen.
-- The columns below hold the shape; exact criteria MUST be frozen before
-- Phase 7 goes live.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Autonomy guards (doc §15 "autonomous cycles"): forward-evidence gates for
-- autonomous actions.
-- ---------------------------------------------------------------------------
CREATE TABLE autonomy_guards (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    min_forward_sample integer,             -- TBD (frozen decision)
    forward_horizon interval,               -- TBD (frozen decision)
    max_loss        numeric,                -- TBD (frozen decision)
    requires_approval boolean NOT NULL DEFAULT true,
    evidence_ref_ids jsonb NOT NULL DEFAULT '[]'::jsonb, -- reviewed forward results
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Autonomous cycles (doc §14/§15): entry/exit canary + LP open/reseed, bounded
-- by an autonomy guard. Canary = limited-scale first run.
-- ---------------------------------------------------------------------------
CREATE TABLE autonomous_cycles (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    mode            text NOT NULL
                    CHECK (mode IN ('auto_bounded', 'confirm_each')),
    guard_id        bigint NOT NULL REFERENCES autonomy_guards (id),
    canary          boolean NOT NULL DEFAULT true,
    max_notional    numeric,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Autonomous LP cycles (doc §15): LP open/reseed after forward evidence.
-- ---------------------------------------------------------------------------
CREATE TABLE autonomous_lp_cycles (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    cycle_id        bigint NOT NULL REFERENCES autonomous_cycles (id),
    pool_id         bigint NOT NULL REFERENCES pools (id),
    action          text NOT NULL
                    CHECK (action IN ('open_position', 'claim_fees', 'compound_fees',
                                      'close_position', 'reseed_position')),
    reseed_after_evidence boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX autonomous_lp_cycles_pool_idx ON autonomous_lp_cycles (pool_id);
