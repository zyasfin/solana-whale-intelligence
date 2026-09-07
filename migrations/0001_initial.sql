-- Solana Whale Intelligence — initial schema.
-- Multi-chain: every record is keyed by (chain, ...) and isolated per chain.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ---------------------------------------------------------------------------
-- Wallets and labels
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS wallets (
    chain      text NOT NULL,
    address    text NOT NULL,
    first_seen timestamptz NOT NULL,
    last_seen  timestamptz NOT NULL,
    source     text NOT NULL DEFAULT 'unknown',
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (chain, address)
);

-- Label history. Rows are never deleted; revocation sets revoked_at.
-- Active labels: revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now()).
CREATE TABLE IF NOT EXISTS wallet_labels (
    id          bigserial PRIMARY KEY,
    chain       text NOT NULL,
    address     text NOT NULL,
    kind        text NOT NULL,
    disposition text NOT NULL DEFAULT 'annotation',
    reason      text,
    source      text NOT NULL DEFAULT 'local',
    confidence  integer NOT NULL DEFAULT 50,
    manual      boolean NOT NULL DEFAULT false,
    expires_at  timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    revoked_at  timestamptz,
    FOREIGN KEY (chain, address) REFERENCES wallets (chain, address) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS wallet_labels_active_idx
    ON wallet_labels (chain, address, kind)
    WHERE revoked_at IS NULL;

-- ---------------------------------------------------------------------------
-- Raw provider events and sync cursors
-- ---------------------------------------------------------------------------

-- Raw payloads are stored BEFORE normalization; deduplicated by signature.
CREATE TABLE IF NOT EXISTS raw_events (
    chain      text NOT NULL,
    source     text NOT NULL,
    signature  text NOT NULL,
    payload    jsonb NOT NULL,
    observed_at timestamptz NOT NULL,
    PRIMARY KEY (chain, source, signature)
);

CREATE INDEX IF NOT EXISTS raw_events_observed_idx ON raw_events (chain, observed_at);

CREATE TABLE IF NOT EXISTS chain_sync_state (
    chain       text NOT NULL,
    stream_kind text NOT NULL,
    cursor      text,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    last_error  text,
    PRIMARY KEY (chain, stream_kind)
);

CREATE TABLE IF NOT EXISTS wallet_sync_state (
    chain                text NOT NULL,
    address              text NOT NULL,
    cursor               text,
    next_allowed_at      timestamptz,
    completed_until      timestamptz,
    requested_from       timestamptz,
    history_completeness numeric(6,4),
    last_error           text,
    updated_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (chain, address)
);

-- ---------------------------------------------------------------------------
-- Tokens, markets, GMGN enrichment
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS tokens (
    chain              text NOT NULL,
    mint               text NOT NULL,
    symbol             text,
    name               text,
    pair_created_at    timestamptz,
    first_seen_at      timestamptz NOT NULL DEFAULT now(),
    lifecycle_state    text NOT NULL DEFAULT 'new_creation',
    lifecycle_updated_at timestamptz,
    risk_flags         jsonb NOT NULL DEFAULT '[]'::jsonb,
    PRIMARY KEY (chain, mint)
);

CREATE TABLE IF NOT EXISTS market_snapshots (
    source         text NOT NULL,
    chain          text NOT NULL,
    mint           text NOT NULL,
    observed_at    timestamptz NOT NULL,
    pair_address   text NOT NULL DEFAULT '',
    dex_id         text,
    price_usd      numeric(30,10),
    market_cap_usd numeric(30,4),
    liquidity_usd  numeric(30,4),
    volume_24h_usd numeric(30,4),
    raw            jsonb NOT NULL DEFAULT 'null'::jsonb,
    PRIMARY KEY (source, chain, mint, observed_at, pair_address)
);

CREATE INDEX IF NOT EXISTS market_snapshots_latest_idx
    ON market_snapshots (chain, mint, observed_at);

CREATE TABLE IF NOT EXISTS gmgn_token_observations (
    chain      text NOT NULL,
    mint       text NOT NULL,
    observed_at timestamptz NOT NULL,
    endpoint   text NOT NULL,
    payload    jsonb NOT NULL,
    PRIMARY KEY (chain, mint, observed_at, endpoint)
);

CREATE TABLE IF NOT EXISTS gmgn_wallet_observations (
    chain      text NOT NULL,
    address    text NOT NULL,
    observed_at timestamptz NOT NULL,
    endpoint   text NOT NULL,
    period     text NOT NULL DEFAULT '',
    payload    jsonb NOT NULL,
    PRIMARY KEY (chain, address, observed_at, endpoint, period)
);

-- ---------------------------------------------------------------------------
-- Narratives
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS narratives (
    id         bigserial PRIMARY KEY,
    slug       text NOT NULL UNIQUE,
    name       text NOT NULL,
    category   text NOT NULL DEFAULT 'unknown',
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS token_narratives (
    chain       text NOT NULL,
    mint        text NOT NULL,
    narrative_id bigint NOT NULL REFERENCES narratives (id),
    score       numeric(6,2) NOT NULL DEFAULT 0,
    confidence  integer NOT NULL DEFAULT 0,
    status      text NOT NULL DEFAULT 'candidate',
    evidence    jsonb NOT NULL DEFAULT 'null'::jsonb,
    observed_at timestamptz NOT NULL,
    PRIMARY KEY (chain, mint, narrative_id, observed_at)
);

CREATE TABLE IF NOT EXISTS narrative_evidence (
    id            bigserial PRIMARY KEY,
    chain         text NOT NULL,
    mint          text NOT NULL,
    narrative_id  bigint NOT NULL REFERENCES narratives (id),
    source_kind   text NOT NULL,
    source_ref    text NOT NULL DEFAULT '',
    canonical_url text,
    claim_type    text NOT NULL,
    claim_text    text NOT NULL DEFAULT '',
    polarity      text NOT NULL DEFAULT 'supporting',
    published_at  timestamptz,
    observed_at   timestamptz NOT NULL,
    confidence    integer NOT NULL DEFAULT 50,
    content_hash  text,
    raw           jsonb NOT NULL DEFAULT 'null'::jsonb
);

CREATE INDEX IF NOT EXISTS narrative_evidence_token_idx
    ON narrative_evidence (chain, mint, narrative_id, observed_at);

-- ---------------------------------------------------------------------------
-- Telegram ingestion (MTProto, allowlisted public channels)
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS telegram_channels (
    channel_key     text PRIMARY KEY,
    title           text,
    username        text,
    access_hash     text,
    allowlisted     boolean NOT NULL DEFAULT true,
    last_cursor     text,
    last_observed_at timestamptz,
    status          text NOT NULL DEFAULT 'active'
);

CREATE TABLE IF NOT EXISTS telegram_messages (
    channel_key text NOT NULL REFERENCES telegram_channels (channel_key) ON DELETE CASCADE,
    message_id  bigint NOT NULL,
    edit_version integer NOT NULL DEFAULT 0,
    posted_at   timestamptz,
    observed_at timestamptz NOT NULL,
    author_ref  text,
    author_name text,
    text_content text,
    raw         jsonb NOT NULL DEFAULT 'null'::jsonb,
    content_hash text,
    deleted_at  timestamptz,
    PRIMARY KEY (channel_key, message_id, edit_version)
);

CREATE INDEX IF NOT EXISTS telegram_messages_channel_time_idx
    ON telegram_messages (channel_key, posted_at);

CREATE TABLE IF NOT EXISTS telegram_mentions (
    id              bigserial PRIMARY KEY,
    channel_key     text NOT NULL,
    message_id      bigint NOT NULL,
    edit_version    integer NOT NULL DEFAULT 0,
    chain           text NOT NULL,
    mint            text,
    wallet          text,
    mention_kind    text NOT NULL,
    extracted_value text NOT NULL,
    context         text,
    confidence      numeric(5,2) NOT NULL DEFAULT 50,
    UNIQUE (channel_key, message_id, edit_version, mention_kind, extracted_value)
);

CREATE INDEX IF NOT EXISTS telegram_mentions_mint_idx
    ON telegram_mentions (chain, mint, mention_kind);

-- ---------------------------------------------------------------------------
-- Funding observations and radar
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS funding_observations (
    id                   bigserial PRIMARY KEY,
    chain                text NOT NULL,
    signature            text NOT NULL,
    slot                 bigint,
    observed_at          timestamptz NOT NULL,
    from_address         text NOT NULL,
    to_address           text NOT NULL,
    asset_kind           text NOT NULL,
    mint                 text NOT NULL DEFAULT '',
    raw_amount           text NOT NULL,
    amount_usd           numeric(30,4),
    native_usd_price     numeric(20,8),
    commitment           text NOT NULL DEFAULT 'confirmed',
    recipient_age_seconds bigint,
    source_disposition   text NOT NULL DEFAULT 'unknown',
    raw                  jsonb NOT NULL DEFAULT 'null'::jsonb,
    UNIQUE (chain, signature, from_address, to_address, asset_kind, mint)
);

CREATE INDEX IF NOT EXISTS funding_observations_recipient_idx
    ON funding_observations (chain, to_address, observed_at);

CREATE TABLE IF NOT EXISTS funding_radar_cases (
    id                   bigserial PRIMARY KEY,
    chain                text NOT NULL,
    recipient            text NOT NULL,
    first_funded_at      timestamptz NOT NULL,
    first_funding_usd    numeric(30,4),
    first_funding_native numeric(30,9) NOT NULL,
    source_address       text NOT NULL,
    source_kind          text,
    fanout_count         integer NOT NULL DEFAULT 0,
    deploy_window_ends_at timestamptz NOT NULL,
    stage                text NOT NULL DEFAULT 'funded',
    confidence           integer NOT NULL DEFAULT 50,
    evidence             jsonb NOT NULL DEFAULT 'null'::jsonb,
    updated_at           timestamptz NOT NULL DEFAULT now(),
    dismissed_reason     text,
    FOREIGN KEY (chain, recipient) REFERENCES wallets (chain, address) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS funding_radar_cases_stage_idx
    ON funding_radar_cases (chain, stage, updated_at);

CREATE TABLE IF NOT EXISTS funding_radar_events (
    id          bigserial PRIMARY KEY,
    case_id     bigint NOT NULL REFERENCES funding_radar_cases (id) ON DELETE CASCADE,
    event_kind  text NOT NULL,
    observed_at timestamptz NOT NULL,
    evidence    jsonb NOT NULL DEFAULT 'null'::jsonb
);

CREATE INDEX IF NOT EXISTS funding_radar_events_case_idx
    ON funding_radar_events (case_id, observed_at);

-- ---------------------------------------------------------------------------
-- Trades, transfers, funding edges, clusters
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS trades (
    chain            text NOT NULL,
    signature        text NOT NULL,
    event_index      integer NOT NULL,
    wallet           text NOT NULL,
    mint             text NOT NULL,
    side             text NOT NULL,
    raw_native_amount text NOT NULL DEFAULT '0',
    raw_token_amount  text NOT NULL DEFAULT '0',
    native_amount    numeric(30,9),
    token_amount     numeric(36,12),
    usd_value        numeric(30,4),
    slot             bigint,
    block_time       timestamptz,
    observed_at      timestamptz NOT NULL,
    dex_id           text,
    source           text NOT NULL DEFAULT 'helius',
    PRIMARY KEY (chain, signature, event_index, wallet)
);

CREATE INDEX IF NOT EXISTS trades_wallet_idx ON trades (chain, wallet, block_time);
CREATE INDEX IF NOT EXISTS trades_mint_idx ON trades (chain, mint, block_time);

CREATE TABLE IF NOT EXISTS transfers (
    chain       text NOT NULL,
    signature   text NOT NULL,
    event_index integer NOT NULL,
    from_address text NOT NULL,
    to_address   text NOT NULL,
    asset_kind  text NOT NULL,
    mint        text NOT NULL DEFAULT '',
    raw_amount  text NOT NULL,
    amount      numeric(36,12),
    slot        bigint,
    block_time  timestamptz,
    observed_at timestamptz NOT NULL,
    source      text NOT NULL DEFAULT 'helius',
    PRIMARY KEY (chain, signature, event_index, from_address, to_address)
);

CREATE INDEX IF NOT EXISTS transfers_from_idx ON transfers (chain, from_address, block_time);
CREATE INDEX IF NOT EXISTS transfers_to_idx ON transfers (chain, to_address, block_time);

CREATE TABLE IF NOT EXISTS funding_edges (
    chain       text NOT NULL,
    from_address text NOT NULL,
    to_address   text NOT NULL,
    signature   text NOT NULL,
    edge_kind   text NOT NULL,
    raw_amount  text NOT NULL,
    block_time  timestamptz,
    confidence  numeric(5,4) NOT NULL DEFAULT 0,
    evidence    jsonb NOT NULL DEFAULT 'null'::jsonb,
    promoted    boolean NOT NULL DEFAULT false,
    PRIMARY KEY (chain, from_address, to_address, signature, edge_kind)
);

CREATE INDEX IF NOT EXISTS funding_edges_to_idx ON funding_edges (chain, to_address);
CREATE INDEX IF NOT EXISTS funding_edges_from_idx ON funding_edges (chain, from_address);

CREATE TABLE IF NOT EXISTS wallet_clusters (
    cluster_id bigserial PRIMARY KEY,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS wallet_cluster_members (
    id              bigserial PRIMARY KEY,
    cluster_id      bigint NOT NULL REFERENCES wallet_clusters (cluster_id) ON DELETE CASCADE,
    chain           text NOT NULL,
    address         text NOT NULL,
    membership_kind text NOT NULL DEFAULT 'soft',
    confidence      numeric(5,4) NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    revoked_at      timestamptz,
    UNIQUE (cluster_id, chain, address)
);

CREATE INDEX IF NOT EXISTS wallet_cluster_members_addr_idx
    ON wallet_cluster_members (chain, address) WHERE revoked_at IS NULL;

-- ---------------------------------------------------------------------------
-- Lifecycle, scores, signals, usage, alerts
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS token_lifecycle_events (
    chain      text NOT NULL,
    mint       text NOT NULL,
    state      text NOT NULL,
    observed_at timestamptz NOT NULL,
    source     text NOT NULL,
    evidence   jsonb NOT NULL DEFAULT 'null'::jsonb,
    PRIMARY KEY (chain, mint, state, observed_at, source)
);

CREATE TABLE IF NOT EXISTS wallet_scores (
    chain                text NOT NULL,
    address              text NOT NULL,
    as_of                timestamptz NOT NULL,
    realized_pnl_usd     numeric(30,4),
    size_weighted_roi    numeric(10,4),
    meaningful_win_rate  numeric(6,4),
    early_entry_rate     numeric(6,4),
    skill_score          integer NOT NULL DEFAULT 0,
    copyability_score    integer NOT NULL DEFAULT 0,
    history_completeness numeric(6,4) NOT NULL DEFAULT 0,
    mev_likelihood       numeric(5,4) NOT NULL DEFAULT 0,
    conviction           integer NOT NULL DEFAULT 0,
    provisional          boolean NOT NULL DEFAULT true,
    evidence             jsonb NOT NULL DEFAULT 'null'::jsonb,
    PRIMARY KEY (chain, address, as_of)
);

CREATE INDEX IF NOT EXISTS wallet_scores_latest_idx
    ON wallet_scores (chain, address, as_of DESC);

CREATE TABLE IF NOT EXISTS signal_evaluations (
    id             bigserial PRIMARY KEY,
    chain          text NOT NULL,
    mint           text NOT NULL,
    signal_kind    text NOT NULL,
    evaluated_at   timestamptz NOT NULL,
    status         text NOT NULL,
    rejection_code text,
    evidence       jsonb NOT NULL DEFAULT 'null'::jsonb,
    CONSTRAINT signal_evaluations_rejection_check CHECK (
        (status = 'rejected' AND rejection_code IS NOT NULL)
        OR (status <> 'rejected' AND rejection_code IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS signal_evaluations_time_idx
    ON signal_evaluations (chain, mint, evaluated_at);

CREATE TABLE IF NOT EXISTS signals (
    id           bigserial PRIMARY KEY,
    chain        text NOT NULL,
    mint         text NOT NULL,
    signal_kind  text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    score        integer NOT NULL DEFAULT 0,
    status       text NOT NULL DEFAULT 'active',
    evidence     jsonb NOT NULL DEFAULT 'null'::jsonb,
    UNIQUE (chain, mint, signal_kind, created_at)
);

CREATE TABLE IF NOT EXISTS helius_usage (
    provider_id  text NOT NULL,
    api_class    text NOT NULL,
    window_start timestamptz NOT NULL,
    request_count bigint NOT NULL DEFAULT 0,
    credit_count  numeric(12,4) NOT NULL DEFAULT 0,
    PRIMARY KEY (provider_id, api_class, window_start)
);

CREATE TABLE IF NOT EXISTS alerts (
    dedup_key      text PRIMARY KEY,
    signal_id      bigint REFERENCES signals (id) ON DELETE SET NULL,
    sent_at        timestamptz NOT NULL DEFAULT now(),
    delivery_error text
);

-- ---------------------------------------------------------------------------
-- Constraints: allowed enum-like values
-- ---------------------------------------------------------------------------

ALTER TABLE funding_radar_cases
    ADD CONSTRAINT funding_radar_cases_stage_check CHECK (
        stage IN ('funded', 'preparation', 'deployed', 'dismissed')
    );
