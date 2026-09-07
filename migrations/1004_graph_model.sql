-- ============================================================================
-- Signal Forge — Phase 0 Migration 1004: Evidence graph model
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §9 "Evidence and
--   graph model" (entity_nodes, entity_edges, incidents).
--
-- Core node types: Token/Contract, TokenFamily, Wallet/WalletCluster,
--   Caller/SourceAccount, Post/Message/ExternalEvent, Narrative, Pool/Position,
--   Strategy/Policy, Decision/Intent/Execution.
-- Edge examples: DEPLOYED_BY, FUNDED_BY, CALLED_BY, AMPLIFIED_BY, DERIVED_FROM,
--   OFFICIALLY_ADOPTED_BY, FIRST_LIQUID_ON, HOLDS, SWAPPED, LP_PROVIDED,
--   SAME_FAMILY_AS, EVIDENCED_BY, RESULTED_IN.
--
-- Every edge stores: time, source, truth status, confidence, evidence refs,
--   valid window, supersession status (doc line 622-623).
-- Neo4j deferred; materialized projections serve operational queries.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Entity nodes: graph vertex store. Node type is a closed enum reflecting the
-- canonical node types; identity is content/chain-qualified (entity_key).
-- ---------------------------------------------------------------------------
CREATE TABLE entity_nodes (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    entity_key      text NOT NULL,          -- chain-qualified canonical key
    node_type       text NOT NULL
                    CHECK (node_type IN (
                        'token', 'token_family', 'wallet', 'wallet_cluster',
                        'caller', 'post', 'narrative', 'pool', 'position',
                        'strategy', 'decision', 'intent', 'execution')),
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT entity_nodes_key_type_key UNIQUE (entity_key, node_type)
);
CREATE INDEX entity_nodes_type_idx ON entity_nodes (node_type);

-- ---------------------------------------------------------------------------
-- Entity edges: graph edge store. Every edge carries the mandatory columns
-- from doc line 622-623: time, source, truth status, confidence, evidence refs,
-- valid window, supersession status.
-- ---------------------------------------------------------------------------
CREATE TABLE entity_edges (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    from_node_id    bigint NOT NULL REFERENCES entity_nodes (id),
    to_node_id      bigint NOT NULL REFERENCES entity_nodes (id),
    edge_type       text NOT NULL,          -- e.g. DEPLOYED_BY, FUNDED_BY, RESULTED_IN
    occurred_at     timestamptz NOT NULL,   -- edge time
    source_id       bigint REFERENCES sources (id),  -- source of this edge
    truth_status    text NOT NULL DEFAULT 'unknown'
                    CHECK (truth_status IN ('unknown', 'confirmed', 'disputed', 'superseded', 'erroneous')),
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    valid_from      timestamptz NOT NULL DEFAULT now(),
    valid_until     timestamptz,            -- NULL = in effect
    supersedes_id   bigint REFERENCES entity_edges (id),  -- supersession status
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT entity_edges_distinct CHECK (from_node_id <> to_node_id)
);
CREATE INDEX entity_edges_from_idx ON entity_edges (from_node_id, edge_type);
CREATE INDEX entity_edges_to_idx ON entity_edges (to_node_id, edge_type);
CREATE INDEX entity_edges_type_idx ON entity_edges (edge_type, valid_until);

-- ---------------------------------------------------------------------------
-- Evidence refs join for edges: an edge may cite many evidence refs (doc
-- "evidence refs" column). Implemented as a join table to evidence_refs.
-- ---------------------------------------------------------------------------
CREATE TABLE entity_edge_evidence (
    edge_id         bigint NOT NULL REFERENCES entity_edges (id),
    evidence_ref_id bigint NOT NULL REFERENCES evidence_refs (id),
    PRIMARY KEY (edge_id, evidence_ref_id)
);

-- ---------------------------------------------------------------------------
-- Incidents: operational anomalies (chain restart, ambiguous submission,
-- source-health degradation, reconciliation events). Tied to entities and
-- evidence. History is immutable (archive-not-delete, principle #6).
-- ---------------------------------------------------------------------------
CREATE TABLE incidents (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    incident_type   text NOT NULL,          -- e.g. reconciliation, source_health, chain_restart
    severity        text NOT NULL DEFAULT 'medium'
                    CHECK (severity IN ('low', 'medium', 'high', 'critical')),
    status          text NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'investigating', 'resolved', 'superseded')),
    entity_keys     text[] NOT NULL DEFAULT '{}',
    summary         text,
    detail          jsonb NOT NULL DEFAULT '{}'::jsonb,
    opened_at       timestamptz NOT NULL DEFAULT now(),
    resolved_at     timestamptz,
    superseded_by   bigint REFERENCES incidents (id),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX incidents_status_idx ON incidents (status, opened_at);
CREATE INDEX incidents_type_idx ON incidents (incident_type);
