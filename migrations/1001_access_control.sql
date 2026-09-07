-- ============================================================================
-- Signal Forge — Phase 0 Migration 1001: Access/control
-- Canonical source: PLAN-SWI-final-architecture-2026-08-29.md §10 "Access/control"
--   (workspaces, identities, workspace_memberships, invites, sessions,
--    webauthn_credentials, workload_identities, audit_events)
-- Replaces pre-freeze password-only auth entirely (replace-total).
--
-- Frozen decisions:
--   * Permanent identity = issuer + subject (doc line 252).
--   * Invite binds once to an immutable OIDC identity (doc line 270).
--   * Worker authentication = workload identity, never human session
--     (doc line 255; acceptance gate #10).
--   * Human privileged changes require OIDC identity + WebAuthn step-up
--     (gate #9).
--   * Audit logs redact secrets and are append-only (gate #17).
-- ============================================================================

-- ---------------------------------------------------------------------------
-- Identity: permanent identity is issuer + subject (OIDC). One identity may
-- belong to many workspaces; membership is a separate, revocable relation.
-- ---------------------------------------------------------------------------
CREATE TABLE identities (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    issuer         text NOT NULL,          -- OIDC issuer URL (e.g. https://accounts.google.com)
    subject        text NOT NULL,          -- OIDC `sub` claim
    email          text,                   -- best-effort display claim, NOT an identity key
    display_name   text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    -- Permanent identity is (issuer, subject); email/name are mutable claims.
    CONSTRAINT identities_issuer_subject_key UNIQUE (issuer, subject)
);

-- ---------------------------------------------------------------------------
-- Workspaces: multi-tenant boundary. Global public data may be workspace-null
-- (see event envelope workspace_id nullable). Workspace-scoped data is not.
-- ---------------------------------------------------------------------------
CREATE TABLE workspaces (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name           text NOT NULL,
    slug           text NOT NULL,
    status         text NOT NULL DEFAULT 'active'
                   CHECK (status IN ('active', 'suspended', 'closed')),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT workspaces_slug_key UNIQUE (slug)
);

-- ---------------------------------------------------------------------------
-- Membership: join table between identities and workspaces. Role is RBAC.
-- Membership is revocable and versioned via valid window (archive-not-delete,
-- principle #6); a revoked membership is superseded, never hard-deleted.
-- ---------------------------------------------------------------------------
CREATE TABLE workspace_memberships (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id   bigint NOT NULL REFERENCES workspaces (id),
    identity_id    bigint NOT NULL REFERENCES identities (id),
    role           text NOT NULL,          -- RBAC role (e.g. operator, analyst, admin)
    valid_from     timestamptz NOT NULL DEFAULT now(),
    valid_until    timestamptz,            -- NULL = currently active
    superseded_by  bigint REFERENCES workspace_memberships (id),
    created_at     timestamptz NOT NULL DEFAULT now(),
    -- One active membership per (workspace, identity) enforced by partial index.
    CONSTRAINT workspace_memberships_window CHECK (valid_until IS NULL OR valid_until > valid_from)
);
CREATE UNIQUE INDEX workspace_memberships_active_key
    ON workspace_memberships (workspace_id, identity_id)
    WHERE valid_until IS NULL;

-- ---------------------------------------------------------------------------
-- Invites: invite-only Google OIDC onboarding. An invite binds ONCE to an
-- immutable OIDC identity (doc line 270). Never reuse an invite after bind.
-- ---------------------------------------------------------------------------
CREATE TABLE invites (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id   bigint NOT NULL REFERENCES workspaces (id),
    token_hash     text NOT NULL UNIQUE,   -- hash of the invite token; plaintext never stored
    invited_email  text,                   -- optional expected email (may be empty)
    role           text NOT NULL,
    status         text NOT NULL DEFAULT 'pending'
                   CHECK (status IN ('pending', 'accepted', 'expired', 'revoked')),
    accepted_by_identity_id bigint REFERENCES identities (id),
    expires_at     timestamptz NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    accepted_at    timestamptz,
    -- Invite binds once: accepted_by_identity_id set exactly when accepted.
    CONSTRAINT invites_bind_once CHECK (
        (status = 'accepted' AND accepted_by_identity_id IS NOT NULL AND accepted_at IS NOT NULL)
        OR (status <> 'accepted' AND accepted_by_identity_id IS NULL)
    )
);

-- ---------------------------------------------------------------------------
-- Sessions: human browser sessions (OIDC + session). Session never carries a
-- private key (gate #8); it authorizes API/UI only.
-- ---------------------------------------------------------------------------
CREATE TABLE sessions (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    identity_id    bigint NOT NULL REFERENCES identities (id),
    token_hash     text NOT NULL UNIQUE,   -- hash of session token; plaintext never stored
    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    last_seen_at   timestamptz,
    revoked_at     timestamptz,
    ip             text,
    user_agent     text
);
CREATE INDEX sessions_identity_idx ON sessions (identity_id);
CREATE INDEX sessions_expiry_idx ON sessions (expires_at);

-- ---------------------------------------------------------------------------
-- WebAuthn credentials: passkey for step-up on privileged changes (gate #9).
-- One identity may hold several credentials; each has its own sign counter
-- for replay protection.
-- ---------------------------------------------------------------------------
CREATE TABLE webauthn_credentials (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    identity_id    bigint NOT NULL REFERENCES identities (id),
    credential_id  bytea NOT NULL UNIQUE,  -- WebAuthn credential ID (binary)
    public_key     bytea NOT NULL,         -- COSE public key
    sign_count     bigint NOT NULL DEFAULT 0,
    transports     text[],                 -- e.g. {internal, hybrid}
    name           text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_used_at   timestamptz
);
CREATE INDEX webauthn_credentials_identity_idx ON webauthn_credentials (identity_id);

-- ---------------------------------------------------------------------------
-- Workload identities: service-to-service auth for workers (gate #10).
-- Workers authenticate via workload identity, never via a human session.
-- ---------------------------------------------------------------------------
CREATE TABLE workload_identities (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id   bigint NOT NULL REFERENCES workspaces (id),
    name           text NOT NULL,          -- e.g. "signer", "ingest-solana", "browser-x"
    key_fingerprint text NOT NULL,         -- fingerprint of the workload credential
    status         text NOT NULL DEFAULT 'active'
                   CHECK (status IN ('active', 'disabled')),
    scopes         text[] NOT NULL DEFAULT '{}',
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_used_at   timestamptz,
    CONSTRAINT workload_identities_workspace_name_key UNIQUE (workspace_id, name)
);

-- ---------------------------------------------------------------------------
-- Audit events: append-only trail (gate #17). Secrets are REDACTED here; only
-- fingerprints/status are stored. No UPDATE/DELETE path exists at the DB level.
-- ---------------------------------------------------------------------------
CREATE TABLE audit_events (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id   bigint REFERENCES workspaces (id),
    identity_id    bigint REFERENCES identities (id),   -- human actor (nullable for worker)
    workload_identity_id bigint REFERENCES workload_identities (id), -- worker actor (nullable)
    action         text NOT NULL,
    entity_type    text NOT NULL,
    entity_id      text,                  -- string form so cross-domain entities fit
    before_state   jsonb,                 -- redacted prior state
    after_state    jsonb,                 -- redacted resulting state
    metadata       jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_events_workspace_idx ON audit_events (workspace_id, created_at);
CREATE INDEX audit_events_entity_idx ON audit_events (entity_type, entity_id);

-- ---------------------------------------------------------------------------
-- Append-only guard: revoke UPDATE/DELETE from the audit table at the DB level
-- so even the application cannot mutate history (gate #17).
-- ---------------------------------------------------------------------------
REVOKE UPDATE, DELETE, TRUNCATE ON audit_events FROM PUBLIC;
