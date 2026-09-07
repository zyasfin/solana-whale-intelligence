-- ============================================================================
-- Signal Forge — Phase 3 Migration 1011: Revival + cabal schema enrichment
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §8.7 (Cabal/
--   Funding Graph) and §8.8 (Revival Intelligence).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Funding graph edges (doc §8.7): funding relationships, synchronized entries/
-- exits, common deployer/authority, shared counterparties, correlated clusters,
-- false confluence detection, evidence/confidence per edge.
-- ---------------------------------------------------------------------------
CREATE TABLE funding_edges (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    from_wallet_id  bigint NOT NULL REFERENCES wallet_addresses (id),
    to_wallet_id    bigint NOT NULL REFERENCES wallet_addresses (id),
    relation        text NOT NULL
                    CHECK (relation IN ('funded_by', 'synchronized_entry', 'synchronized_exit',
                                        'common_deployer', 'common_authority',
                                        'shared_counterparty', 'correlated_cluster')),
    evidence_ref_id bigint REFERENCES evidence_refs (id),
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    false_confluence_risk boolean NOT NULL DEFAULT false,
    valid_from      timestamptz NOT NULL DEFAULT now(),
    valid_until     timestamptz,
    supersedes_id   bigint REFERENCES funding_edges (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT funding_edges_distinct CHECK (from_wallet_id <> to_wallet_id)
);
CREATE INDEX funding_edges_from_idx ON funding_edges (from_wallet_id);
CREATE INDEX funding_edges_to_idx ON funding_edges (to_wallet_id);

-- ---------------------------------------------------------------------------
-- Dormant baselines (doc §8.8 + §8.2): dormant/dead tokens are not polled;
-- global feeds compare against compact dormant baselines. Prior history +
-- failure memory stay attached.
-- ---------------------------------------------------------------------------
CREATE TABLE dormant_baselines (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_id        bigint NOT NULL REFERENCES tokens (id),
    dormant_at      timestamptz NOT NULL,
    baseline        jsonb NOT NULL DEFAULT '{}'::jsonb,
    failure_memory  jsonb NOT NULL DEFAULT '[]'::jsonb, -- prior failure memory attached
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT dormant_baselines_token_key UNIQUE (token_id)
);

-- ---------------------------------------------------------------------------
-- Revival incidents (doc §8.8): a token waking from dormancy triggers a revival
-- flow. History is immutable (archive-not-delete).
-- ---------------------------------------------------------------------------
CREATE TABLE revival_incidents (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    token_id        bigint NOT NULL REFERENCES tokens (id),
    baseline_id     bigint REFERENCES dormant_baselines (id),
    stage           text NOT NULL
                    CHECK (stage IN ('wake', 'dormant_baseline_comparison', 'activation_gate',
                                     'refresh', 'revival_quality', 'opportunity_evaluation')),
    passed_activation_gate boolean,
    revival_quality double precision,
    evidence_ref_ids jsonb NOT NULL DEFAULT '[]'::jsonb,
    opened_at       timestamptz NOT NULL DEFAULT now(),
    resolved_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX revival_incidents_token_idx ON revival_incidents (token_id, opened_at);
