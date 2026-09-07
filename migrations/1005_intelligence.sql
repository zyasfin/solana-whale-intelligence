-- ============================================================================
-- Signal Forge — Phase 0 Migration 1005: Intelligence domain
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §10 "Intelligence"
--   (tokens, token_lifecycle_events, token_families, wallet_addresses,
--    wallet_clusters, wallet_tag_definitions, wallet_tag_assertions, wallet_notes,
--    callers, calls, narratives, pools, lp_positions, portfolio_snapshots,
--    risk_findings, entity_memory, cooldowns).
--
-- Contract address is token truth; ticker/name are discovery clues (doc line 57).
-- Archived/dead tokens become compact tombstones, never deleted (line 58).
-- Custom tags are assertions (doc §8.6): namespace:name, source_type, truth_status,
--   confidence, valid window, status, supersedes_id. Manual never silently
--   overwritten.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Tokens: contract-address-keyed. Ticker/name are mutable discovery clues, not
-- identity. Archive = tombstone, never delete.
-- ---------------------------------------------------------------------------
CREATE TABLE tokens (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    chain           text NOT NULL,          -- SOL / RH / ETH / Base / BSC
    contract_address text NOT NULL,         -- token truth (doc line 57)
    ticker          text,
    name            text,
    status          text NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'archived', 'dead')),
    tombstone       jsonb NOT NULL DEFAULT '{}'::jsonb,  -- compact tombstone when archived/dead
    first_seen_at   timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT tokens_chain_address_key UNIQUE (chain, contract_address)
);
CREATE INDEX tokens_status_idx ON tokens (status);

-- ---------------------------------------------------------------------------
-- Token lifecycle events: birth, family, canonicality, revival, etc.
-- Append-only timeline; history preserved for revival/audit (principle #6).
-- ---------------------------------------------------------------------------
CREATE TABLE token_lifecycle_events (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_id        bigint NOT NULL REFERENCES tokens (id),
    event_type      text NOT NULL,          -- e.g. birth, first_liquidity, revival, death
    occurred_at     timestamptz NOT NULL,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    source_id       bigint REFERENCES sources (id),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX token_lifecycle_token_idx ON token_lifecycle_events (token_id, occurred_at);

-- ---------------------------------------------------------------------------
-- Token families: family/canonicality grouping (SAME_FAMILY_AS edges).
-- ---------------------------------------------------------------------------
CREATE TABLE token_families (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    name            text NOT NULL,
    canonical_token_id bigint REFERENCES tokens (id),
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX token_families_canonical_idx ON token_families (canonical_token_id);

-- ---------------------------------------------------------------------------
-- Wallet addresses: chain-qualified address + optional entity cluster.
-- ---------------------------------------------------------------------------
CREATE TABLE wallet_addresses (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    chain           text NOT NULL,
    address         text NOT NULL,
    cluster_id      bigint,                 -- FK added below (wallet_clusters)
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT wallet_addresses_chain_addr_key UNIQUE (chain, address)
);

-- ---------------------------------------------------------------------------
-- Wallet clusters: correlated wallet grouping (cabal/funding graph).
-- ---------------------------------------------------------------------------
CREATE TABLE wallet_clusters (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    name            text,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- resolve circular FK
ALTER TABLE wallet_addresses
    ADD CONSTRAINT wallet_addresses_cluster_fk
    FOREIGN KEY (cluster_id) REFERENCES wallet_clusters (id);

-- ---------------------------------------------------------------------------
-- Wallet tag definitions: the tag vocabulary (namespace:name).
-- ---------------------------------------------------------------------------
CREATE TABLE wallet_tag_definitions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    namespace       text NOT NULL,
    name            text NOT NULL,
    description     text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT wallet_tag_definitions_key UNIQUE (namespace, name)
);

-- ---------------------------------------------------------------------------
-- Wallet tag assertions: an assertion of a tag on a wallet (doc §8.6).
-- Manual assertions never silently overwritten; supersedes_id chains history.
-- ---------------------------------------------------------------------------
CREATE TABLE wallet_tag_assertions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    wallet_address_id bigint NOT NULL REFERENCES wallet_addresses (id),
    tag_definition_id bigint NOT NULL REFERENCES wallet_tag_definitions (id),
    source_type     text NOT NULL
                    CHECK (source_type IN ('manual', 'system', 'import', 'vendor', 'inferred')),
    truth_status    text NOT NULL DEFAULT 'unknown'
                    CHECK (truth_status IN ('unknown', 'confirmed', 'disputed', 'superseded', 'erroneous')),
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    valid_from      timestamptz NOT NULL DEFAULT now(),
    valid_until     timestamptz,
    status          text NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'disputed', 'expired', 'revoked')),
    supersedes_id   bigint REFERENCES wallet_tag_assertions (id),
    source_id       bigint REFERENCES sources (id),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX wallet_tag_assertions_wallet_idx ON wallet_tag_assertions (wallet_address_id, status);

-- ---------------------------------------------------------------------------
-- Wallet notes: operator annotations.
-- ---------------------------------------------------------------------------
CREATE TABLE wallet_notes (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    wallet_address_id bigint NOT NULL REFERENCES wallet_addresses (id),
    identity_id     bigint REFERENCES identities (id),  -- author
    body            text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX wallet_notes_wallet_idx ON wallet_notes (wallet_address_id);

-- ---------------------------------------------------------------------------
-- Callers: source accounts (Telegram/X/etc.) that call tokens.
-- ---------------------------------------------------------------------------
CREATE TABLE callers (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    platform        text NOT NULL,          -- telegram / x / web / tiktok
    platform_user_id text NOT NULL,
    display_name    text,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT callers_platform_user_key UNIQUE (platform, platform_user_id)
);

-- ---------------------------------------------------------------------------
-- Calls: a caller's signal/mention of a token, with lead time and outcome.
-- ---------------------------------------------------------------------------
CREATE TABLE calls (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    caller_id       bigint NOT NULL REFERENCES callers (id),
    token_id        bigint REFERENCES tokens (id),
    event_id        uuid REFERENCES events (event_id),
    occurred_at     timestamptz NOT NULL,
    lead_time       interval,               -- time from call to entry
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX calls_caller_idx ON calls (caller_id, occurred_at);
CREATE INDEX calls_token_idx ON calls (token_id, occurred_at);

-- ---------------------------------------------------------------------------
-- Narratives: name/meme provenance graph nodes (narrative domain).
-- ---------------------------------------------------------------------------
CREATE TABLE narratives (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    token_id        bigint REFERENCES tokens (id),
    narrative_key   text NOT NULL,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT narratives_key UNIQUE (narrative_key)
);

-- ---------------------------------------------------------------------------
-- Pools: LP pools (Meteora DLMM, Uniswap v2/v3/v4, PancakeSwap v2/v3).
-- ---------------------------------------------------------------------------
CREATE TABLE pools (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    chain           text NOT NULL,
    protocol        text NOT NULL,          -- meteora / uniswap_v2 / uniswap_v3 / ...
    pool_address    text NOT NULL,
    token0_id       bigint REFERENCES tokens (id),
    token1_id       bigint REFERENCES tokens (id),
    fee_tier        text,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT pools_chain_addr_key UNIQUE (chain, pool_address)
);

-- ---------------------------------------------------------------------------
-- LP positions: opened liquidity positions.
-- ---------------------------------------------------------------------------
CREATE TABLE lp_positions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    pool_id         bigint NOT NULL REFERENCES pools (id),
    wallet_address_id bigint REFERENCES wallet_addresses (id),
    position_key    text NOT NULL,
    status          text NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'closed', 'liquidated')),
    opened_at       timestamptz,
    closed_at       timestamptz,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT lp_positions_pool_key UNIQUE (pool_id, position_key)
);

-- ---------------------------------------------------------------------------
-- Portfolio snapshots: point-in-time portfolio projection.
-- ---------------------------------------------------------------------------
CREATE TABLE portfolio_snapshots (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    snapshot_at     timestamptz NOT NULL,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,  -- exposure, PnL, notional
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX portfolio_snapshots_ws_at_idx ON portfolio_snapshots (workspace_id, snapshot_at);

-- ---------------------------------------------------------------------------
-- Risk findings: portfolio/risk assessments.
-- ---------------------------------------------------------------------------
CREATE TABLE risk_findings (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    finding_type    text NOT NULL,
    severity        text NOT NULL DEFAULT 'medium'
                    CHECK (severity IN ('low', 'medium', 'high', 'critical')),
    status          text NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'acknowledged', 'mitigated', 'closed')),
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX risk_findings_status_idx ON risk_findings (status);

-- ---------------------------------------------------------------------------
-- Entity memory: prior history/failure memory attached to entities (revival).
-- ---------------------------------------------------------------------------
CREATE TABLE entity_memory (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    entity_type     text NOT NULL,
    entity_id       text NOT NULL,
    memory_type     text NOT NULL,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT entity_memory_key UNIQUE (entity_type, entity_id, memory_type)
);

-- ---------------------------------------------------------------------------
-- Cooldowns: per-entity/action cooldown enforcement.
-- ---------------------------------------------------------------------------
CREATE TABLE cooldowns (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    entity_type     text NOT NULL,
    entity_id       text NOT NULL,
    cooldown_type   text NOT NULL,
    cooldown_until  timestamptz NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cooldowns_entity_idx ON cooldowns (entity_type, entity_id, cooldown_until);
