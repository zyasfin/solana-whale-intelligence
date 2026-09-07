-- ============================================================================
-- Signal Forge — Phase 1 Migration 1009: Core intelligence schema enrichment
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §8.1-8.6 (Token
--   Birth Lifecycle, Family/Canonicality, Provenance, Wallet Intelligence) and
--   §12.2 (Component classes).
--
-- Extends the Phase 0 schema (1001-1008) with Phase 1 domain state. Does NOT
-- alter frozen Phase 0 tables (immutable migrations; archive-not-delete).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Token birth lifecycle (doc §8.2). Nine frozen states.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'token_lifecycle') THEN
        CREATE TYPE token_lifecycle AS ENUM
            ('created', 'pre_graduation', 'migrated', 'first_liquidity',
             'active', 'cooling', 'dormant', 'archived', 'tombstoned');
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- Provenance truth status (doc §8.4): Exact/Reconstructed/Estimated/Insufficient.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'provenance_truth_status') THEN
        CREATE TYPE provenance_truth_status AS ENUM
            ('exact', 'reconstructed', 'estimated', 'insufficient');
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- Provenance role (doc §8.4): originator, independent spread, official adopter,
-- deployer, caller/amplifier, market-leading contract.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'provenance_role') THEN
        CREATE TYPE provenance_role AS ENUM
            ('originator', 'independent_spread', 'official_adopter',
             'deployer', 'caller_amplifier', 'market_leading_contract');
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- Extend tokens with the Phase 1 lifecycle column. The existing `status` column
-- remains (coarse operational state); lifecycle is the fine-grained birth
-- timeline. Contract address remains token truth.
-- ---------------------------------------------------------------------------
ALTER TABLE tokens
    ADD COLUMN lifecycle token_lifecycle;

-- ---------------------------------------------------------------------------
-- Token family/canonicality (doc §8.3): relation official/derivative/copycat.
-- ---------------------------------------------------------------------------
CREATE TABLE token_family_relations (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    family_id       bigint NOT NULL REFERENCES token_families (id),
    token_id        bigint NOT NULL REFERENCES tokens (id),
    relation        text NOT NULL
                    CHECK (relation IN ('official', 'derivative', 'copycat')),
    canonical       boolean NOT NULL DEFAULT false,  -- market-leading contract
    valid_from      timestamptz NOT NULL DEFAULT now(),
    valid_until     timestamptz,
    supersedes_id   bigint REFERENCES token_family_relations (id),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX token_family_relations_family_idx ON token_family_relations (family_id);
CREATE INDEX token_family_relations_token_idx ON token_family_relations (token_id);

-- ---------------------------------------------------------------------------
-- Provenance evidence (doc §8.4 token-first flow): role + truth status +
-- earliest evidence. Links to raw evidence refs.
-- ---------------------------------------------------------------------------
CREATE TABLE token_provenance (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_id        bigint NOT NULL REFERENCES tokens (id),
    role            provenance_role NOT NULL,
    truth_status    provenance_truth_status NOT NULL DEFAULT 'insufficient',
    earliest_evidence_ref_id bigint REFERENCES evidence_refs (id),
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX token_provenance_token_idx ON token_provenance (token_id);

-- ---------------------------------------------------------------------------
-- Wallet swap reconstruction (doc §8.6): exact swaps + cost basis. Stored
-- append-only; cost basis is derived, not overwritten in place.
-- ---------------------------------------------------------------------------
CREATE TABLE wallet_swaps (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    chain           text NOT NULL,
    wallet_address_id bigint NOT NULL REFERENCES wallet_addresses (id),
    token_id        bigint REFERENCES tokens (id),
    direction       text NOT NULL CHECK (direction IN ('buy', 'sell')),
    amount_in       numeric NOT NULL,
    amount_out      numeric NOT NULL,
    tx_hash         text NOT NULL,
    occurred_at     timestamptz NOT NULL,
    source_id       bigint REFERENCES sources (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT wallet_swaps_tx_key UNIQUE (chain, tx_hash)
);
CREATE INDEX wallet_swaps_wallet_idx ON wallet_swaps (wallet_address_id, occurred_at);
CREATE INDEX wallet_swaps_token_idx ON wallet_swaps (token_id, occurred_at);

-- ---------------------------------------------------------------------------
-- Portfolio component classes (doc §12.2): distinguishes N/A, missing, zero,
-- safe as distinct values (principle #3). No universal score.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'component_value_kind') THEN
        CREATE TYPE component_value_kind AS ENUM
            ('na', 'missing', 'zero', 'safe', 'present');
    END IF;
END$$;

CREATE TABLE portfolio_components (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    snapshot_id     bigint NOT NULL REFERENCES portfolio_snapshots (id),
    component_class text NOT NULL
                    CHECK (component_class IN ('mandatory_pass', 'sizing_input', 'strategy_input', 'halt_input')),
    component_name  text NOT NULL,
    value_kind      component_value_kind NOT NULL DEFAULT 'missing',
    value           jsonb,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX portfolio_components_snapshot_idx ON portfolio_components (snapshot_id);
