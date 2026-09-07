-- ============================================================================
-- Signal Forge — REV-020 Migration 1018: Token Recent + Deployer/Social Reuse
-- Canonical source: REVIEW_RESULT.md REV-020 (amends PLAN SWI §8.3.1).
--
-- Adds:
--   recent_relation enum        — 13 frozen relationship variants.
--   social_identities           — platform-qualified identity with historical
--                                 handles retaining validity windows.
--   recent_events               — append-only evidence-backed recent-event
--                                 projection (temporal, never symbol-merged).
--
-- Principles: evidence-before-inference (#1), archive-not-delete (#6),
-- fail-closed deny-by-default. Rows are append-only: UPDATE/DELETE are revoked
-- from PUBLIC; retraction is a NEW recent_events row, never an UPDATE.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- recent_relation enum (REV-020 13 frozen variants).
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'recent_relation') THEN
        CREATE TYPE recent_relation AS ENUM (
            'same_deployer',
            'same_authority',
            'same_fee_payer',
            'same_funder',
            'funded_by_known_deployer',
            'same_social_account',
            'reused_social_link',
            'official_ca_announcement',
            'cross_chain_deployment',
            'derivative_of',
            'suspected_copycat',
            'liquidity_attention_rotated_to'
        );
    END IF;
END$$;

-- ---------------------------------------------------------------------------
-- Social identities: platform + immutable user ID as identity. Current and
-- historical handles/domains retain validity windows and supersession status.
-- ---------------------------------------------------------------------------
CREATE TABLE social_identities (
    id                 bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id       bigint REFERENCES workspaces (id),
    platform           text NOT NULL,          -- x / tiktok / web / telegram
    immutable_user_id  text NOT NULL,          -- permanent account/user ID
    current_handle     text,
    historical_handles jsonb NOT NULL DEFAULT '[]'::jsonb,
    domain             text,                    -- normalized registrable domain
    telegram_chat_id   text,
    valid_from         timestamptz NOT NULL DEFAULT now(),
    valid_until        timestamptz,             -- NULL = currently in effect
    superseded_by      bigint REFERENCES social_identities (id),
    payload            jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT social_identities_platform_user_key UNIQUE (platform, immutable_user_id)
);
CREATE INDEX social_identities_platform_idx ON social_identities (platform, valid_until);

-- ---------------------------------------------------------------------------
-- Recent events: append-only temporal projection. Each event carries event
-- type, anchor/related identities, chain-qualified contract, times, relation,
-- truth status, confidence components, evidence refs, dependency group,
-- freshness, coverage, capability status, and retraction/supersession status.
-- ---------------------------------------------------------------------------
CREATE TABLE recent_events (
    id                     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id           bigint REFERENCES workspaces (id),
    token_identity         text NOT NULL,       -- chain:contract (anchor lookup)
    event_type             text NOT NULL,
    anchor_identity        text NOT NULL,
    related_identities     jsonb NOT NULL DEFAULT '[]'::jsonb,
    chain_qualified_contract text NOT NULL,
    occurred_at            timestamptz NOT NULL,
    observed_at            timestamptz NOT NULL,
    relation               recent_relation,
    truth_status           text NOT NULL DEFAULT 'unknown'
                           CHECK (truth_status IN ('unknown', 'confirmed', 'disputed', 'superseded', 'erroneous')),
    confidence             double precision CHECK (confidence >= 0 AND confidence <= 1),
    confidence_level       text NOT NULL DEFAULT 'insufficient'
                           CHECK (confidence_level IN ('exact', 'reconstructed', 'estimated', 'insufficient')),
    evidence_refs          jsonb NOT NULL DEFAULT '[]'::jsonb,
    dependency_group       text,
    freshness              jsonb,
    coverage               text NOT NULL DEFAULT 'unavailable'
                           CHECK (coverage IN ('full', 'degraded', 'on_demand', 'unavailable')),
    capability_status      text NOT NULL DEFAULT 'unavailable'
                           CHECK (capability_status IN ('available', 'insufficient', 'unavailable')),
    retraction             jsonb,
    created_at             timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX recent_events_token_time_idx ON recent_events (token_identity, occurred_at);
CREATE INDEX recent_events_relation_idx ON recent_events (relation, occurred_at);
CREATE INDEX recent_events_dependency_idx ON recent_events (dependency_group);
CREATE INDEX recent_events_related_gin_idx ON recent_events USING gin (related_identities);

-- Append-only: a retraction is a NEW row; never mutate history.
REVOKE UPDATE, DELETE, TRUNCATE ON recent_events, social_identities FROM PUBLIC;
