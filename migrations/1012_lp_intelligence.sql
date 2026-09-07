-- ============================================================================
-- Signal Forge — Phase 4 Migration 1012: LP intelligence schema enrichment
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §8.9 (LP Pool
--   Intelligence) and §8.10 (LP Wallet Intelligence). LP = Solana Meteora +
--   Robinhood Uniswap/Pancake; ETH/Base/BSC LP = N/A.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- LP pool intelligence (doc §8.9). PnL components remain SEPARATE (gate #13).
-- ---------------------------------------------------------------------------
CREATE TABLE lp_pool_intelligence (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pool_id         bigint NOT NULL REFERENCES pools (id),
    active_bin      text,                  -- Meteora
    bin_step        text,
    range           text,
    tvl             numeric,
    active_tvl      numeric,
    reserves        jsonb,
    volume          numeric,
    fees            numeric,
    fee_to_tvl      double precision,
    observed_at     timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX lp_pool_intelligence_pool_idx ON lp_pool_intelligence (pool_id, observed_at);

-- ---------------------------------------------------------------------------
-- LP PnL components (doc "LP accounting"). Each component is a SEPARATE column;
-- never collapsed into a single score (gate #13).
-- ---------------------------------------------------------------------------
CREATE TABLE lp_pnl (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    position_id     bigint NOT NULL REFERENCES lp_positions (id),
    inventory_pnl   numeric,
    fee_pnl         numeric,
    reward_pnl      numeric,
    impermanent_loss_estimate numeric,
    swap_rebalance_friction numeric,
    gas_rent_tips   numeric,
    realized_pnl    numeric,
    unrealized_pnl  numeric,
    measured_at     timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX lp_pnl_position_idx ON lp_pnl (position_id, measured_at);

-- ---------------------------------------------------------------------------
-- LP range chamber (doc §22 "LP Range Chamber"): range shifts and reseed
-- recommendations, gated by evidence.
-- ---------------------------------------------------------------------------
CREATE TABLE lp_range_chamber (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    pool_id         bigint NOT NULL REFERENCES pools (id),
    current_range   text,
    recommended_range text,
    range_shift_reason text,
    evidence_ref_ids jsonb NOT NULL DEFAULT '[]'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX lp_range_chamber_pool_idx ON lp_range_chamber (pool_id, created_at);
