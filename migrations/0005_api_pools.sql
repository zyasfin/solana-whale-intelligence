-- DB-backed API key pools for Helius and GMGN (admin "API panel").
--
-- Keys round-robin with per-second rate limits (in-process token buckets) and
-- monthly quota tracking. `used_this_month` is valid only while `usage_month`
-- equals the current UTC month (to_char(now(), 'YYYY-MM')); readers treat a
-- stale marker as zero and reset it before counting new usage.
-- Ids are BIGSERIAL to match rpc_providers and the admin UI's inline handlers.

CREATE TABLE IF NOT EXISTS helius_keys (
    id              bigserial PRIMARY KEY,
    name            text NOT NULL DEFAULT '',
    api_key         text NOT NULL UNIQUE,
    -- Canonical "{helius.base_url}/?api-key={key}" (https scheme).
    rpc_url         text NOT NULL,
    enabled         boolean NOT NULL DEFAULT true,
    monthly_limit   integer,                 -- NULL = unlimited
    used_this_month integer NOT NULL DEFAULT 0,
    usage_month     text NOT NULL DEFAULT to_char(now(), 'YYYY-MM'),
    last_ok_at      timestamptz,
    last_error      text,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS helius_keys_enabled_idx ON helius_keys (enabled);

-- GMGN Ed25519 keypairs generated server-side (admin UI). The private key is
-- stored server-side only and is NEVER returned by any API route.
CREATE TABLE IF NOT EXISTS gmgn_pubkeys (
    id              bigserial PRIMARY KEY,
    public_key_pem  text NOT NULL,
    private_key_pem text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS gmgn_keys (
    id              bigserial PRIMARY KEY,
    name            text NOT NULL DEFAULT '',
    api_key         text NOT NULL UNIQUE,
    pubkey_id       bigint REFERENCES gmgn_pubkeys(id) ON DELETE SET NULL,
    enabled         boolean NOT NULL DEFAULT true,
    monthly_limit   integer,                 -- NULL = unlimited
    used_this_month integer NOT NULL DEFAULT 0,
    usage_month     text NOT NULL DEFAULT to_char(now(), 'YYYY-MM'),
    last_ok_at      timestamptz,
    last_error      text,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS gmgn_keys_enabled_idx ON gmgn_keys (enabled);
CREATE INDEX IF NOT EXISTS gmgn_keys_pubkey_idx ON gmgn_keys (pubkey_id);
