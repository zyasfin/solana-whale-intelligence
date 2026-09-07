-- Smart Wallets: group-driven promotion tiers (config [smart_wallet.groups]).
-- group_name = the first (strictest) config group whose thresholds the wallet
-- meets; '' = candidate in no group. See workers.rs smart_wallet_enrich_loop.

ALTER TABLE smart_wallets ADD COLUMN IF NOT EXISTS group_name text NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS smart_wallets_group_status_idx ON smart_wallets (group_name, status);
