-- ============================================================================
-- Signal Forge — Phase 0 Migration 1002: Sources/providers
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §10 "Sources/providers"
--   (sources, provider_accounts, provider_credentials_ref, provider_endpoints,
--    provider_health, provider_usage, source_dependencies)
-- Also §7.3 source dependence + "Provider pools" (no blind round-robin, no
-- quota evasion, auth/validation errors disable credential, vendor isolation)
-- and §8.12 Source Health states (UP/SILENT/DEGRADED/DOWN/RECOVERING/DISABLED).
--
-- Secrets are external encrypted references ONLY; the UI sees fingerprint/status
-- (doc line 637). No plaintext credential is ever stored here.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Source health state machine (doc §8.12). Connected-but-silent is NOT healthy.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'source_health_state') THEN
        CREATE TYPE source_health_state AS ENUM
            ('UP', 'SILENT', 'DEGRADED', 'DOWN', 'RECOVERING', 'DISABLED');
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- Sources: a logical upstream data source / vendor family (e.g. Helius, Birdeye,
-- DEX Screener, Etherscan, Telegram MTProto). Capability = mandatory/vendor
-- replaceable (principle #4).
-- ---------------------------------------------------------------------------
CREATE TABLE sources (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name            text NOT NULL UNIQUE,   -- canonical source name
    family          text NOT NULL,          -- provider family grouping
    kind            text NOT NULL,          -- e.g. rpc, api, scraper, stream
    platform        text NOT NULL,          -- chain/platform this source serves
    capabilities    text[] NOT NULL DEFAULT '{}',  -- what this source provides
    capability_role text NOT NULL DEFAULT 'vendor'
                    CHECK (capability_role IN ('mandatory', 'vendor', 'fallback')),
    enabled         boolean NOT NULL DEFAULT true,
    config          jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Provider credentials reference: external encrypted reference ONLY. The
-- actual key lives in an external secret store (env-ref / secret manager).
-- The DB stores a fingerprint + status; never the secret itself (doc line 637).
-- ---------------------------------------------------------------------------
CREATE TABLE provider_credentials_ref (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id       bigint NOT NULL REFERENCES sources (id),
    name            text NOT NULL,          -- e.g. HELIUS_KEY_1
    fingerprint     text NOT NULL,          -- hash of the secret, for display/verification
    status          text NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'disabled', 'auth_failed', 'invalid')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT provider_credentials_ref_source_name_key UNIQUE (source_id, name)
);

-- ---------------------------------------------------------------------------
-- Provider accounts: user-owned credentials grouped for quota accounting.
-- Auth/validation errors disable the account; request-invalid is never retried
-- against every key (provider pool rules).
-- ---------------------------------------------------------------------------
CREATE TABLE provider_accounts (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id       bigint NOT NULL REFERENCES sources (id),
    credential_ref_id bigint REFERENCES provider_credentials_ref (id),
    label           text,
    status          text NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'disabled', 'cooldown', 'breaker_open')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Provider endpoints: concrete URL/endpoint mirrors with weighted health/latency
-- and chain-ID validation (provider pool "endpoint mirrors").
-- ---------------------------------------------------------------------------
CREATE TABLE provider_endpoints (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id       bigint NOT NULL REFERENCES sources (id),
    account_id      bigint REFERENCES provider_accounts (id),
    url             text NOT NULL,
    weight          double precision NOT NULL DEFAULT 1.0,
    expected_chain_id text,                -- chain-ID validation for EVM endpoints
    status          text NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'disabled')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Provider health: per-source health tracking (doc §8.12). Includes last
-- request/event/success, expected cadence, parser success, schema staleness.
-- ---------------------------------------------------------------------------
CREATE TABLE provider_health (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id       bigint NOT NULL REFERENCES sources (id),
    endpoint_id     bigint REFERENCES provider_endpoints (id),
    state           source_health_state NOT NULL DEFAULT 'UP',
    last_request_at timestamptz,
    last_event_at   timestamptz,
    last_success_at timestamptz,
    expected_cadence interval,             -- e.g. '30 seconds' for Critical
    parser_success_rate double precision,
    schema_stale    boolean NOT NULL DEFAULT false,
    latency_ms      integer,
    consecutive_failures integer NOT NULL DEFAULT 0,
    retry_after_at  timestamptz,           -- honor Retry-After
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX provider_health_source_idx ON provider_health (source_id, state);

-- ---------------------------------------------------------------------------
-- Provider usage: usage/cost accounting + token buckets (provider pool
-- "quota/class token buckets" + "usage/cost accounting").
-- ---------------------------------------------------------------------------
CREATE TABLE provider_usage (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_id       bigint NOT NULL REFERENCES sources (id),
    account_id      bigint REFERENCES provider_accounts (id),
    bucket          text NOT NULL,         -- quota class / token bucket name
    window_start    timestamptz NOT NULL,
    requests        bigint NOT NULL DEFAULT 0,
    cost            numeric,               -- monetary cost accounting (nullable if untracked)
    quota_limit     bigint,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT provider_usage_window_key UNIQUE (source_id, account_id, bucket, window_start)
);
CREATE INDEX provider_usage_source_window_idx ON provider_usage (source_id, window_start);

-- ---------------------------------------------------------------------------
-- Source dependencies (doc §7.3): two vendors repeating one upstream event are
-- NOT two independent confirmations. Records upstream->downstream so
-- confirmation counting can discount dependent sources.
-- ---------------------------------------------------------------------------
CREATE TABLE source_dependencies (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    upstream_source_id   bigint NOT NULL REFERENCES sources (id),
    downstream_source_id bigint NOT NULL REFERENCES sources (id),
    relation        text NOT NULL DEFAULT 'derives_from'
                    CHECK (relation IN ('derives_from', 'mirrors', 'resells')),
    valid_from      timestamptz NOT NULL DEFAULT now(),
    valid_until     timestamptz,           -- NULL = currently in effect
    superseded_by   bigint REFERENCES source_dependencies (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT source_dependencies_distinct CHECK (upstream_source_id <> downstream_source_id)
);
CREATE INDEX source_dependencies_upstream_idx ON source_dependencies (upstream_source_id);
CREATE INDEX source_dependencies_downstream_idx ON source_dependencies (downstream_source_id);
