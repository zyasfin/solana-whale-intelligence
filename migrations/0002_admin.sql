-- Admin panel: sessions, RPC providers, editable settings.

CREATE TABLE IF NOT EXISTS admin_sessions (
    token_hash  text PRIMARY KEY,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    last_seen_at timestamptz,
    ip          text,
    user_agent  text
);

CREATE INDEX IF NOT EXISTS admin_sessions_expiry_idx ON admin_sessions (expires_at);

-- RPC provider registry. Secrets are referenced by env var name only.
CREATE TABLE IF NOT EXISTS rpc_providers (
    id            bigserial PRIMARY KEY,
    chain         text NOT NULL,
    name          text NOT NULL,
    url           text NOT NULL,
    api_key_ref   text,
    enabled       boolean NOT NULL DEFAULT true,
    rpc_rate_per_second integer NOT NULL DEFAULT 10,
    enhanced_rate_per_second integer NOT NULL DEFAULT 2,
    wallet_rate_per_second integer NOT NULL DEFAULT 2,
    kind          text NOT NULL DEFAULT 'rpc',
    last_ok_at    timestamptz,
    last_error    text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (chain, name)
);

CREATE INDEX IF NOT EXISTS rpc_providers_enabled_idx ON rpc_providers (chain, enabled);

CREATE TABLE IF NOT EXISTS admin_settings (
    key   text PRIMARY KEY,
    value jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
