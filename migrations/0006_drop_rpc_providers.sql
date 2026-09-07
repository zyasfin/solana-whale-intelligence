-- Drop the legacy RPC Providers feature (superseded by the API Keys panel:
-- helius_keys / gmgn_keys). The table was created in 0002_admin.sql and was
-- already unused at runtime; its UI and routes are removed in the same change.

DROP TABLE IF EXISTS rpc_providers;
