-- ============================================================================
-- Signal Forge — Phase 6 Migration 1014: Secure execution schema enrichment
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §14 (Token
--   Auto-Trader), §15 (LP Autopilot), §17 (Signer policy), §19 (Kill switches).
--
-- ===== FROZEN-DECISION DEFERRED =====
-- trade_intents state transition table + reservation release/expiry rules are
-- NOT frozen (execution-readiness blocker). This migration adds the *shape* of
-- execution scaffolding but does NOT finalize the transition table; that MUST
-- be frozen before Phase 6 goes live.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Automation policy limits (doc §14 "Policy limits"): max per trade/token/chain/
-- strategy, exposure, rate windows, slippage/impact/gas/tip, depth, daily loss/
-- drawdown, reserve, allowlists, cooldown/denylist.
-- ---------------------------------------------------------------------------
CREATE TABLE automation_policy_limits (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    policy_version_id bigint NOT NULL REFERENCES automation_policy_versions (id),
    max_per_trade   numeric,
    max_per_token   numeric,
    max_per_chain   numeric,
    max_per_strategy numeric,
    max_total_exposure numeric,
    trades_per_window integer,
    notional_per_window numeric,
    max_slippage    double precision,
    max_price_impact double precision,
    max_gas_tip     numeric,
    max_daily_loss  numeric,
    max_drawdown    double precision,
    wallet_reserve  numeric,
    allowed_routers text[] NOT NULL DEFAULT '{}',
    allowed_programs text[] NOT NULL DEFAULT '{}',
    allowed_contracts text[] NOT NULL DEFAULT '{}',
    cooldown        interval,
    denylist        text[] NOT NULL DEFAULT '{}',
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX automation_policy_limits_policy_idx ON automation_policy_limits (policy_version_id);

-- ---------------------------------------------------------------------------
-- Signer policy checks (doc §17): the signer independently validates full
-- transaction semantics. Recorded per-sign for audit (gate #5).
-- ---------------------------------------------------------------------------
CREATE TABLE signer_checks (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    intent_id       bigint REFERENCES trade_intents (id),
    chain_id        text,
    chain_genesis   text,
    policy_active   boolean,
    policy_not_expired boolean,
    policy_not_halted boolean,
    intent_hash_valid boolean,
    nonce_valid     boolean,
    router_allowed  boolean,
    program_allowed boolean,
    function_selector text,
    token_pair_verified boolean,
    recipient_verified boolean,
    max_native_debit numeric,
    max_token_debit numeric,
    min_output      numeric,
    slippage_ok     boolean,
    price_impact_ok boolean,
    deadline_ok     boolean,
    simulation_delta_ok boolean,
    passed          boolean NOT NULL DEFAULT false,
    checked_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX signer_checks_intent_idx ON signer_checks (intent_id);

-- ---------------------------------------------------------------------------
-- Kill switch scope key support (doc §19): global/per-chain/wallet/strategy/
-- protocol/source-health/daily-loss/signer-local. EXIT_ONLY permits risk-
-- reducing claim/withdraw/close only.
-- ---------------------------------------------------------------------------
ALTER TABLE kill_switches
    DROP CONSTRAINT IF EXISTS kill_switches_mode_check;

ALTER TABLE kill_switches
    ADD CONSTRAINT kill_switches_mode_check
    CHECK (mode IN ('halt', 'exit_only'));
