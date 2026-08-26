-- Smart Wallets: a separate analysis track from Funding Radar.
-- Discovers/tracks high-quality smart-money & KOL wallets from GMGN
-- (/v1/user/smartmoney, /v1/user/kol trades + /v1/user/wallet_stats enrichment)
-- for early copy-entry. GMGN social fields are untrusted third-party data.

CREATE TABLE IF NOT EXISTS smart_wallets (
    id                bigserial PRIMARY KEY,
    chain             text NOT NULL DEFAULT 'solana',
    address           text NOT NULL,
    source            text NOT NULL,             -- 'smartmoney'|'kol' (first seen from)
    twitter_username  text,
    twitter_name      text,
    followers_count   integer,
    is_blue_verified  boolean,
    status            text NOT NULL DEFAULT 'candidate',  -- candidate|tracked|dismissed
    tracked           boolean NOT NULL DEFAULT false,     -- user pin: manually keep watching
    stats             jsonb,                     -- last wallet_stats payload (pnl, winrate, ...)
    first_seen_at     timestamptz NOT NULL DEFAULT now(),
    last_seen_at      timestamptz NOT NULL DEFAULT now(),
    last_stats_at     timestamptz,
    notes             text NOT NULL DEFAULT '',
    UNIQUE (chain, address)
);

CREATE INDEX IF NOT EXISTS smart_wallets_status_idx ON smart_wallets (status);
CREATE INDEX IF NOT EXISTS smart_wallets_last_seen_idx ON smart_wallets (last_seen_at DESC);

CREATE TABLE IF NOT EXISTS smart_wallet_trades (
    id          bigserial PRIMARY KEY,
    chain       text NOT NULL DEFAULT 'solana',
    wallet      text NOT NULL,
    mint        text NOT NULL,
    symbol      text,
    side        text NOT NULL,
    amount_usd  numeric,
    trade_ts    timestamptz NOT NULL,
    source      text NOT NULL,                  -- 'smartmoney'|'kol'
    raw         jsonb,
    UNIQUE (chain, wallet, mint, side, trade_ts)
);

CREATE INDEX IF NOT EXISTS smart_wallet_trades_wallet_ts_idx ON smart_wallet_trades (wallet, trade_ts DESC);
CREATE INDEX IF NOT EXISTS smart_wallet_trades_mint_idx ON smart_wallet_trades (mint);
