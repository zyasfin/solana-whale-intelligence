# Solana Whale Intelligence

Rust/PostgreSQL on-chain intelligence service: discovers smart wallets, detects
insider/sniper/bundler clusters, traces fresh-wallet funding, and emits
explainable research signals with a replay guard against look-ahead bias.

Chains: **Solana** (Helius canonical) and **Robinhood Chain** (generic EVM
JSON-RPC adapter with runtime chain-ID validation).

## Safety posture

- Research and alerts only. **No trade execution, no private keys, no GMGN
  trading routes.**
- Helius is canonical on-chain evidence; GMGN is enrichment only and can never
  overwrite canonical data.
- MEV/Dex-bot auto-skip is reversible; manual blocks remain authoritative.
- Telegram text/URLs are untrusted evidence — never commands or trade facts.
- Telegram data is never used to train/fine-tune/deploy AI or ML models.
- Funding alerts say "possible project preparation" — never "insider" or
  "project confirmed" from funding alone.

## Layout

| Path | Purpose |
|---|---|
| `migrations/0001_initial.sql` | Multi-chain schema: wallets, labels, raw events, radar, Telegram, narratives, clusters, scores, signals |
| `src/chains.rs` | Solana + Robinhood adapters, EVM JSON-RPC, chain-ID validation |
| `src/helius.rs` | Provider pool: per-key token buckets, round-robin, cooldowns, breakers |
| `src/gmgn.rs` | GMGN Agent API query client, weighted bucket, envelope handling |
| `src/funding_radar.rs` | `funded → preparation → deployed` lifecycle, two-stage alerts |
| `src/telegram_ingest.rs` / `src/telegram_parse.rs` | MTProto ingestion, allowlist, deterministic parsing |
| `src/narrative.rs` | Narrative taxonomy, provenance, 49-cap for GMGN-only |
| `src/graph.rs` | Soft clusters, edge confidence, wallet tracing |
| `src/scoring.rs` | FIFO matching, skill/copyability scores, conviction cap |
| `src/signals.rs` | Entry/exit gates, one rejection code per failed gate |
| `src/replay.rs` | Temporal guard (`observed_at <= evaluation_time`), paper metrics |
| `src/queues.rs` | Fixed precedence, bottom-up pausing under pressure |

## Setup

```bash
cp .env.example .env    # fill DATABASE_URL and provider keys
cargo build --release
cargo run -- db migrate
```

## Verification

```bash
cargo test                              # unit + fixture tests (117)
cargo test --features pg_tests          # + Postgres integration (needs DATABASE_URL)
cargo run -- db migrate                 # against a disposable database
cargo run -- health                     # redacted status report
```

The Postgres integration tests (`src/funding_radar_pg_tests.rs`) verify:
funded-case creation, `funding_watch`, preparation promotion with
`preparation_alert`, one-transfer no-promotion, deployment linking, expiry
dismissal with history retention, processed-only no-promotion, and
infrastructure-source provenance handling.

## CLI

```
db migrate
token discover --once | token report <MINT> --chain <sol|robinhood>
telegram auth | telegram channels list|add|remove | telegram backfill
funding radar list|inspect|evaluate
wallet sync|score|leaderboard|block|flow-only|watch|unblock|labels|import-blocklist
trace <ADDRESS>
replay
signal rejected
watch serve-webhook
health
```

## Profiles

- **low** (2 vCPU / 4 GB): Telegram concurrency 4, provider workers 2, no
  LaserStream gRPC, 14-day raw retention, backfill pauses at 60s lag.
- **scale** (8 vCPU / 16 GB app + separate PostgreSQL): Telegram concurrency
  32, independent workers, optional LaserStream gRPC, object-storage archive.

Queue precedence (never pauses `live_watch`): `live_watch` > `funding_radar` >
`telegram_ingest` > `seed_sync` > `historical_backfill`.

## Robinhood Chain

No Robinhood network metadata is hard-coded. The chain shares the Helius
account: its EVM RPC endpoint derives from `[helius].base_url` +
`[chains.robinhood].helius_slug` in `config.toml` and authenticates with
`HELIUS_KEY_1`. Configure `robinhood_chain_id`, native symbol/decimals,
explorer URL, and start block in `config.toml`. The adapter validates
`eth_chainId` at startup; on mismatch it disables only itself and reports the
exact error in `health`. GMGN Robinhood
observations then remain `external_observation` capped at confidence 49.

## Admin panel (Opsi B)

Fully functional admin panel with password-only auth (bcrypt + HttpOnly session).

```bash
# 1. set admin password (prints ADMIN_PASSWORD_HASH_B64 for .env)
cargo run -- db hash-password

# 2. serve the panel
cargo run --release -- watch serve-admin --bind 0.0.0.0:8789
```

Features: RPC provider CRUD (per-chain rate limits, env-ref keys), Telegram
channel management, wallet labels + blocklist import, wallet/radar/signal views
with search + pagination, cluster browser. All mutations require a valid
session; secrets never leave the server (RPC keys are env-var references).
