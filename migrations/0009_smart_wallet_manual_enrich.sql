-- Smart Wallets: manual enrich requests. When enrich_requested_at IS NOT NULL
-- the enrich worker drains that wallet FIRST (before the stale batch), then
-- clears the flag. Set via POST /api/smart-wallets/enrich-all and
-- /api/smart-wallets/{address}/enrich. Survives restarts (DB column).

ALTER TABLE smart_wallets ADD COLUMN IF NOT EXISTS enrich_requested_at timestamptz;

CREATE INDEX IF NOT EXISTS smart_wallets_enrich_requested_idx
    ON smart_wallets (enrich_requested_at)
    WHERE enrich_requested_at IS NOT NULL;
