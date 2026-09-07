-- ============================================================================
-- Signal Forge — Phase 0 Migration 1007: Decisions/execution
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §10 "Decisions/execution"
--   (decision_bundles, decision_components, decision_transitions,
--    automation_policies, automation_policy_versions, trade_intents,
--    capital_reservations, execution_attempts, chain_transactions,
--    reconciliation_incidents, positions, position_actions, kill_switches).
--
-- All status transitions append audit events. Operational projection may update,
-- but history is immutable (doc line 749).
--
-- ===== FROZEN-DECISION DEFERRED (blocker) =====================================
-- trade_intents state machine (§16, doc 1024-1038) lists 11 states but has NO
-- transition table and NO reservation release/expiry rules. Per discussion, we
-- DEFER the transition table; status is a loose TEXT with a broad CHECK. A TODO
-- is recorded below. This MUST be frozen before Phase 6 (secure execution).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Decision bundles: immutable point-in-time decision snapshot (doc §12.1).
-- ---------------------------------------------------------------------------
CREATE TABLE decision_bundles (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    target_entity   text NOT NULL,
    target_action   text NOT NULL,
    decision_at     timestamptz NOT NULL,
    evidence_snapshot_ids jsonb NOT NULL DEFAULT '[]'::jsonb,
    component_results jsonb NOT NULL DEFAULT '{}'::jsonb,
    missing_capabilities text[] NOT NULL DEFAULT '{}',
    source_freshness jsonb NOT NULL DEFAULT '{}'::jsonb,
    confidence      double precision CHECK (confidence >= 0 AND confidence <= 1),
    truth_status    text NOT NULL DEFAULT 'unknown',
    strategy_version_id bigint REFERENCES strategy_versions (id),
    rule_version    text,
    policy_version  text,
    alternatives    jsonb NOT NULL DEFAULT '[]'::jsonb,
    final_disposition text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX decision_bundles_ws_at_idx ON decision_bundles (workspace_id, decision_at);

-- ---------------------------------------------------------------------------
-- Decision components: individual component results (MANDATORY PASS / SIZING INPUT).
-- ---------------------------------------------------------------------------
CREATE TABLE decision_components (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    decision_bundle_id bigint NOT NULL REFERENCES decision_bundles (id),
    component_class text NOT NULL
                    CHECK (component_class IN ('mandatory', 'sizing_input')),
    component_name  text NOT NULL,
    result          jsonb NOT NULL DEFAULT '{}'::jsonb,
    pass            boolean,                -- for mandatory components
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX decision_components_bundle_idx ON decision_components (decision_bundle_id);

-- ---------------------------------------------------------------------------
-- Decision transitions: audit of decision state changes (immutable).
-- ---------------------------------------------------------------------------
CREATE TABLE decision_transitions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    decision_bundle_id bigint NOT NULL REFERENCES decision_bundles (id),
    from_state      text,
    to_state        text NOT NULL,
    transitioned_at timestamptz NOT NULL DEFAULT now(),
    actor_identity_id bigint REFERENCES identities (id),
    actor_workload_id bigint REFERENCES workload_identities (id),
    reason          text
);
CREATE INDEX decision_transitions_bundle_idx ON decision_transitions (decision_bundle_id);

-- ---------------------------------------------------------------------------
-- Automation policies + versions: policies that authorize bounded autonomy.
-- ---------------------------------------------------------------------------
CREATE TABLE automation_policies (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    name            text NOT NULL,
    status          text NOT NULL DEFAULT 'draft'
                    CHECK (status IN ('draft', 'active', 'paused', 'retired')),
    current_version_id bigint,              -- FK added below
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE automation_policy_versions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    policy_id       bigint NOT NULL REFERENCES automation_policies (id),
    version         integer NOT NULL,
    policy          jsonb NOT NULL DEFAULT '{}'::jsonb,
    policy_hash     text NOT NULL,          -- reproducibility (gate #2, #4)
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT automation_policy_versions_key UNIQUE (policy_id, version)
);

ALTER TABLE automation_policies
    ADD CONSTRAINT automation_policies_current_version_fk
    FOREIGN KEY (current_version_id) REFERENCES automation_policy_versions (id);

-- ---------------------------------------------------------------------------
-- Trade intents: the immutable intent that all actions flow through
-- (principle #9, gate #4). Includes policy version, intent hash, reservation.
--
-- TODO(FROZEN-DECISION): §16 lists 11 intent states but does not define the
-- legal transition table nor reservation release/expiry rules. Status is loose
-- TEXT + broad CHECK until frozen before Phase 6.
-- ---------------------------------------------------------------------------
CREATE TABLE trade_intents (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    intent_hash     text NOT NULL UNIQUE,   -- idempotency/content hash (gate #4)
    automation_policy_version_id bigint REFERENCES automation_policy_versions (id),
    strategy_version_id bigint REFERENCES strategy_versions (id),
    chain           text NOT NULL,
    wallet_address_id bigint REFERENCES wallet_addresses (id),
    source_event_id text,                   -- source signal/event (idempotency)
    action          text NOT NULL,          -- e.g. ENTRY, EXIT, OPEN_POSITION, CLAIM
    target_entity   text NOT NULL,
    status          text NOT NULL DEFAULT 'proposed',
    nonce           text,                   -- one-time nonce
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX trade_intents_status_idx ON trade_intents (status);
CREATE INDEX trade_intents_wallet_idx ON trade_intents (wallet_address_id, status);

-- ---------------------------------------------------------------------------
-- Capital reservations: atomic capital/exposure reservation before side effect
-- (§16 "Locks/reservations"). Release/expiry rules DEFERRED (same blocker).
-- ---------------------------------------------------------------------------
CREATE TABLE capital_reservations (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    intent_id       bigint NOT NULL REFERENCES trade_intents (id),
    workspace_id    bigint REFERENCES workspaces (id),
    chain           text NOT NULL,
    asset           text NOT NULL,
    amount          numeric NOT NULL,
    reserved_at     timestamptz NOT NULL DEFAULT now(),
    released_at     timestamptz,            -- NULL = still reserved
    expires_at      timestamptz,            -- intent expiry (one-time nonce)
    status          text NOT NULL DEFAULT 'reserved'
                    CHECK (status IN ('reserved', 'released', 'expired')),
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX capital_reservations_status_idx ON capital_reservations (status, expires_at);
CREATE INDEX capital_reservations_intent_idx ON capital_reservations (intent_id);

-- ---------------------------------------------------------------------------
-- Execution attempts: each attempt to execute an intent.
-- ---------------------------------------------------------------------------
CREATE TABLE execution_attempts (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    intent_id       bigint NOT NULL REFERENCES trade_intents (id),
    attempt_no      integer NOT NULL,
    status          text NOT NULL,
    started_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz,
    error           jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT execution_attempts_key UNIQUE (intent_id, attempt_no)
);

-- ---------------------------------------------------------------------------
-- Chain transactions: on-chain tx records (full decode, signer validation).
-- ---------------------------------------------------------------------------
CREATE TABLE chain_transactions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    intent_id       bigint REFERENCES trade_intents (id),
    chain           text NOT NULL,
    tx_hash         text NOT NULL,
    signer_identity text NOT NULL,          -- which signer signed
    status          text NOT NULL DEFAULT 'submitted'
                    CHECK (status IN ('submitted', 'confirmed', 'failed', 'landed', 'not_landed', 'unknown')),
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT chain_transactions_hash_key UNIQUE (chain, tx_hash)
);
CREATE INDEX chain_transactions_intent_idx ON chain_transactions (intent_id);

-- ---------------------------------------------------------------------------
-- Reconciliation incidents: ambiguous submission outcomes (§16 reconciliation).
-- ---------------------------------------------------------------------------
CREATE TABLE reconciliation_incidents (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    intent_id       bigint REFERENCES trade_intents (id),
    chain           text NOT NULL,
    tx_hash         text,
    result          text,                   -- LANDED / NOT_LANDED / UNKNOWN
    detail          jsonb NOT NULL DEFAULT '{}'::jsonb,
    opened_at       timestamptz NOT NULL DEFAULT now(),
    resolved_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX reconciliation_incidents_open_idx ON reconciliation_incidents (resolved_at);

-- ---------------------------------------------------------------------------
-- Positions + position actions: open trading/LP positions with one-action lock.
-- ---------------------------------------------------------------------------
CREATE TABLE positions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    chain           text NOT NULL,
    wallet_address_id bigint REFERENCES wallet_addresses (id),
    token_id        bigint REFERENCES tokens (id),
    side            text NOT NULL CHECK (side IN ('long', 'short')),
    status          text NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'closed')),
    opened_at       timestamptz NOT NULL DEFAULT now(),
    closed_at       timestamptz,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX positions_status_idx ON positions (status);

CREATE TABLE position_actions (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    position_id     bigint NOT NULL REFERENCES positions (id),
    action          text NOT NULL,
    action_hash     text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX position_actions_position_idx ON position_actions (position_id);

-- ---------------------------------------------------------------------------
-- Kill switches: global/per-chain/wallet/strategy/protocol/signer-local stops.
-- ---------------------------------------------------------------------------
CREATE TABLE kill_switches (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id    bigint REFERENCES workspaces (id),
    scope           text NOT NULL,          -- global / chain / wallet / strategy / protocol / signer
    scope_key       text,                   -- which chain/wallet/strategy/etc.
    mode            text NOT NULL DEFAULT 'halt'
                    CHECK (mode IN ('halt', 'exit_only')),
    active          boolean NOT NULL DEFAULT true,
    reason          text,
    activated_at    timestamptz NOT NULL DEFAULT now(),
    deactivated_at  timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX kill_switches_active_idx ON kill_switches (scope, scope_key, active);
