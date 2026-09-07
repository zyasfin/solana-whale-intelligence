-- ============================================================================
-- Signal Forge — Phase 0 Migration 1003: Event/evidence envelope
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §7.1 "Canonical
--   event envelope" + §9 "Evidence and graph model" (raw_evidence, evidence_refs,
--   observations).
--
-- EventEnvelope fields (doc 363-401):
--   event_id, workspace_id (nullable for global public data), chain/platform,
--   event_type, entity_keys[], source_id, source_event_id, occurred_at (nullable),
--   observed_at, ingested_at, raw_hash/raw_ref, parser_version,
--   payload_schema_version, truth_status, confidence.
--
-- Idempotency key defaults:
--   (a) source_id + source_event_id + payload_schema_version
--   (b) source_id + normalized entity + event type + time bucket + raw hash
--   (when source lacks stable ID)
--
-- Principle #1 evidence-before-inference, #2 point-in-time truth.
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Raw evidence: written BEFORE parser-derived claims where practical (doc 7.2).
-- Content-addressed blob evidence (doc line 222 V1 + canonical summary line 1364).
-- ---------------------------------------------------------------------------
CREATE TABLE raw_evidence (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),   -- nullable = global public
    source_id       bigint NOT NULL REFERENCES sources (id),
    raw_hash        text NOT NULL,          -- content hash (sha256) of the raw payload
    raw_ref         text NOT NULL,          -- content-addressed storage reference
    chain           text,                   -- chain/platform qualifier
    event_type      text,
    payload_schema_version text,
    parser_version  text,
    occurred_at     timestamptz,            -- nullable (unknown event time)
    observed_at     timestamptz NOT NULL DEFAULT now(),
    ingested_at     timestamptz NOT NULL DEFAULT now(),
    size_bytes      bigint,
    CONSTRAINT raw_evidence_hash_key UNIQUE (raw_hash)
);
CREATE INDEX raw_evidence_source_idx ON raw_evidence (source_id, ingested_at);

-- ---------------------------------------------------------------------------
-- Events: the canonical, normalized event envelope (doc 7.1). Idempotent
-- append; two idempotency modes enforced by partial unique indexes.
-- ---------------------------------------------------------------------------
CREATE TABLE events (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id        uuid NOT NULL,          -- application-assigned event id
    workspace_id    bigint REFERENCES workspaces (id),   -- nullable = global public
    chain           text,                   -- chain/platform
    event_type      text NOT NULL,
    entity_keys     text[] NOT NULL DEFAULT '{}',  -- normalized entity keys this event touches
    source_id       bigint NOT NULL REFERENCES sources (id),
    source_event_id text,                   -- stable upstream id (nullable => fallback mode)
    occurred_at     timestamptz,            -- nullable
    observed_at     timestamptz NOT NULL,
    ingested_at     timestamptz NOT NULL DEFAULT now(),
    raw_hash        text NOT NULL REFERENCES raw_evidence (raw_hash),
    parser_version  text,
    payload_schema_version text NOT NULL,
    truth_status    text NOT NULL DEFAULT 'unknown'
                    CHECK (truth_status IN ('unknown', 'confirmed', 'disputed', 'superseded', 'erroneous')),
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    normalized_entity text,                 -- fallback idempotency component
    time_bucket     timestamptz,            -- fallback idempotency component
    CONSTRAINT events_event_id_key UNIQUE (event_id)
);

-- Idempotency mode (a): source_id + source_event_id + payload_schema_version
CREATE UNIQUE INDEX events_idempotency_stable_key
    ON events (source_id, source_event_id, payload_schema_version)
    WHERE source_event_id IS NOT NULL;

-- Idempotency mode (b): source lacks stable ID
CREATE UNIQUE INDEX events_idempotency_fallback_key
    ON events (source_id, normalized_entity, event_type, time_bucket, raw_hash)
    WHERE source_event_id IS NULL;

-- ---------------------------------------------------------------------------
-- Observations: normalized, parser-derived claims that reference raw evidence.
-- Every observation carries source/freshness/confidence (gate #1).
-- ---------------------------------------------------------------------------
CREATE TABLE observations (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    event_id        uuid NOT NULL REFERENCES events (event_id),
    entity_keys     text[] NOT NULL DEFAULT '{}',
    observation_type text NOT NULL,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    truth_status    text NOT NULL DEFAULT 'unknown'
                    CHECK (truth_status IN ('unknown', 'confirmed', 'disputed', 'superseded', 'erroneous')),
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    freshness_status text,                  -- mandatory freshness indicator (gate #3)
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX observations_event_idx ON observations (event_id);
CREATE INDEX observations_entity_idx ON observations USING gin (entity_keys);

-- ---------------------------------------------------------------------------
-- Evidence refs: links between entities/observations/decisions and their
-- supporting evidence (raw_evidence). Used by decision bundles (doc 12.1
-- "evidence snapshot IDs").
-- ---------------------------------------------------------------------------
CREATE TABLE evidence_refs (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    raw_hash        text NOT NULL REFERENCES raw_evidence (raw_hash),
    entity_type     text NOT NULL,          -- what kind of thing is evidenced
    entity_id       text NOT NULL,          -- string form for cross-domain entities
    role            text NOT NULL DEFAULT 'supports',
    valid_from      timestamptz NOT NULL DEFAULT now(),
    valid_until     timestamptz,            -- NULL = in effect
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX evidence_refs_entity_idx ON evidence_refs (entity_type, entity_id);
CREATE INDEX evidence_refs_hash_idx ON evidence_refs (raw_hash);
