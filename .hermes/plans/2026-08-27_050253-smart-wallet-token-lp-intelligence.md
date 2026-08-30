# Smart Wallet, Token Screener, dan LP Intelligence — Planning Draft

> **Planning only.** Tidak mengubah aplikasi, DB, service, atau deployment.

## Goal

Membuat dashboard intelligence yang bisa dikustomisasi untuk:

1. **Smart Wallet** — user mengatur sendiri sumber, threshold, tier/group, polling, dan manual override.
2. **Token Screener** — user membuat filter token berdasarkan data GMGN.
3. **LP Intelligence** — user menyaring pool Meteora DLMM, menggabungkannya dengan kualitas token, serta menilai wallet yang profit dari LP.

## Current state yang terverifikasi

- Deployment aktif: `solana-whale-intelligence` admin `:8790` + worker terpisah.
- Smart wallet data: 1.009 wallet (`906 candidate`, `62 tracked`, `41 dismissed`) dan 30.744 trade.
- Smart wallet sekarang memakai konfigurasi otomatis/ordered groups dari `config.toml`.
- Backend deployed sudah punya fondasi endpoint smart-wallet groups/manual enrich, tetapi `admin_settings` belum berisi override `smart_wallet*`; runtime masih jatuh ke default TOML.
- Token: 14.817 row, 2.224 raw GMGN token observations.
- `market_snapshots`: **0**. Token panel belum punya normalized latest metrics yang cukup untuk filter cepat dan historis.
- LP-specific tables/workers belum ada.
- Helius WebSocket sedang kena HTTP 429 berulang. Fitur LP tidak boleh ditambah ke jalur provider ini sebelum rate budget sehat.
- Source-of-truth belum rapi: `/root/swi-src` tidak memuat static dashboard/migrations terbaru, sedangkan `/root/swi-deploy` dan deployed binary memuat fitur smart-wallet lebih baru. Sinkronisasi source wajib menjadi tahap nol sebelum implementasi.

## Product principles

- **Saved profiles**, bukan satu setting global.
- **Preview before activate**: tampilkan jumlah match, contoh pass/fail, estimasi request provider.
- **Explainable**: setiap candidate punya alasan lolos/gagal dan field yang belum tersedia.
- **Manual override wins**: pin/dismiss/manual tier tidak ditimpa automation.
- **Missing-data policy explicit**: `exclude`, `allow`, atau `pending enrichment`.
- **Read-only intelligence first**: tidak ada swap, private key, auto-position, atau auto-routing.
- **Incremental enrichment**: list murah dahulu, enrich mahal hanya candidate yang lolos tahap awal.
- Jangan menerima arbitrary SQL/JS expression dari dashboard. V1 cukup field whitelist + operator + value, semua rules aktif memakai AND.

---

## 1. Smart Wallet Custom Setup

### Existing rules yang perlu diekspos ke dashboard

#### Discovery

- Source: `smartmoney`, `kol`, atau keduanya.
- Poll interval.
- Enrich batch size.
- Stats period: `7d` / `30d`.
- Consecutive enrichment failures sebelum dismiss.
- Candidate TTL/stale threshold.

#### Group/tier rules

- Nama group, warna, priority/order.
- Minimum win rate.
- Minimum realized PnL USD.
- Minimum trade count.
- Minimum average trade USD.
- Minimum X/Twitter followers.
- Require blue verification.

### Rules tambahan yang layak dari GMGN wallet stats

- Min/max realized PnL.
- Min/max unrealized PnL.
- Min/max total cost/deployed capital.
- Min/max buy count, sell count, total trades.
- Min/max PnL ratio.
- Min/max native SOL balance.
- Min/max token count.
- Max average holding period / min average holding period.
- PnL bucket distribution: `>5x`, `2x–5x`, `0–2x`, `-50–0%`, `<-50%`.
- Required/excluded wallet tags: smart_degen, KOL, sniper, bundler, rat trader, dex bot, dev, insider, arbitrage, market maker.
- Min wallet age, created-token count, GMGN follow count, remark count.

### Manual controls

- Add wallet manually.
- Pin/unpin.
- Force tier/group.
- Dismiss/restore.
- Notes/tags lokal.
- “Enrich now”.
- Rule history: profile/version mana yang mempromosikan atau menurunkan wallet.

### Recommended UI

- `Profiles`: create, duplicate, edit, enable/disable.
- `Groups`: ordered tier builder.
- `Candidates`: pass/fail reason.
- `Tracked`: manual and automatic tracked wallets.
- `Preview`: run rules tanpa mengaktifkan.

---

## 2. Token Screener — GMGN Filter Catalog

### Discovery source

- Trending: `1m`, `5m`, `1h`, `6h`, `24h`.
- Trenches: `new_creation`, `near_completion`, `completed`.
- Hot searches.
- Signal feed.
- Manual mint/watchlist.

### Market/activity filters

- Market cap, ATH market cap.
- Liquidity.
- Volume per window.
- Swap, buy, sell count.
- Net buy volume.
- Price change per window.
- Token age / pool age.
- Holder count.
- Gas/fee activity.
- Smart-money count, KOL count, bot-degen count, visitors.

### Token/pool identity filters

- Launchpad/platform: Pump.fun, letsbonk, Moonshot, Meteora pools, Raydium, Orca, etc.
- Current exchange/DEX.
- Quote token whitelist: SOL/USDC/etc.
- Lifecycle: bonding curve, near graduation, migrated/open market.
- Main pool address/exchange, pool creation time, pool liquidity, fee ratio.

### Risk/security filters

- Rug ratio.
- Renounced mint and freeze authority.
- Burn/token-burn status.
- Wash trading.
- Insider/rat-trader ratio.
- Bundler ratio.
- Entrapment ratio.
- Top-10 holder concentration.
- Top-70 sniper hold rate.
- Dev-team/creator hold rate and creator status.
- Fresh-wallet rate.
- Private-vault/vanish hold rate.
- Bot count/rate.
- Meteora pool blacklist status.

### Creator/social filters

- Creator launches count.
- Creator graduated/open count and graduation ratio.
- Creator best ATH market cap.
- X follower count.
- X rename count.
- Telegram call count.
- Has social.
- Duplicate social/image checks.
- CTO flag.

### Holder/trader drilldown

- Filter holder/trader tags: smart_degen, KOL, fresh wallet, dev, sniper, rat trader, bundler, transfer-in, dex bot, bluechip owner.
- Sort: holding %, realized profit, unrealized profit, buy volume, sell volume.

### Recommended token output

Jangan hanya satu opaque score. Tampilkan:

- **Market score**
- **Safety score**
- **Smart-money score**
- **LP suitability score**
- Hard rejection reasons
- Data freshness/completeness

---

## 3. LP Intelligence Panel — Meteora DLMM

### Official source

Base API: `https://dlmm.datapi.meteora.ag`

Available pool data:

- `/pools` and `/pools/groups`
- Pool detail
- OHLCV
- Historical volume
- Portfolio open/closed
- Portfolio total PnL
- Position PnL: `/positions/{pool_address}/pnl`
- Position history
- Wallet/pool total fee and reward claims

Meteora pool list supports native filtering/sorting by:

- `tvl`
- `volume_*`
- `fee_*`
- `fee_tvl_ratio_*`
- `apr_*`
- `fee_pct`
- `bin_step`
- `pool_created_at`
- `farm_apy`
- `is_blacklisted`
- pool/token name, mint, address

Windows: `5m`, `30m`, `1h`, `2h`, `4h`, `12h`, `24h`.

### Panel tabs

1. **Pool Screener** — all candidate pools + saved filters.
2. **Shortlist** — manually saved pools.
3. **LP Wallets** — tracked profitable LP wallets.
4. **Positions** — open/closed positions for tracked wallets.
5. **Alerts** — pool/wallet/risk events.
6. **Profiles** — saved LP strategies.

### Native pool filters

- Min/max TVL.
- Min/max volume by window.
- Min/max fees and fee/TVL by window.
- Min/max APR/APY/farm APY.
- Fee %, bin step.
- Pool age.
- Blacklist status.
- Quote-token whitelist.
- Has farm/reward.

### Derived LP filters

- Volume/TVL consistency across windows.
- Fee/TVL consistency, bukan spike satu window.
- APR sustainability.
- Price volatility from OHLCV.
- Drawdown and trend regime.
- Fee-to-volatility ratio.
- Pool liquidity trend.
- Estimated out-of-range risk for a chosen width.
- Expected rebalance frequency.
- Pool duplication: pilih best pool per pair, jangan tampilkan semua clone sebagai opportunity terpisah.
- Token market-cap/TVL ratio.
- Token risk profile from GMGN.

### LP candidate pipeline

1. Fetch Meteora pool list cheaply.
2. Apply native pool filters server-side.
3. Join token mint to latest GMGN token metrics.
4. Reject hard token risks.
5. Fetch OHLCV/details only for remaining shortlist.
6. Compute pool economics + volatility + LP suitability.
7. Save ranked result with pass/fail reasons and freshness.

### Suggested presets (editable)

- **Small-cap meme farm**: MC `<$50M`, healthy token checks, minimum TVL/volume, high fee/TVL, acceptable volatility.
- **Low rebalance**: lower volatility, wider expected range survival, older/stable pool.
- **Aggressive high-fee**: newer pools, high volume/TVL and fee/TVL, stricter token-risk limits.
- **New pool radar**: recent pool, minimum early TVL/volume, smart-money confirmation.

Threshold angka jangan dikunci sebelum snapshot historis tersedia. Dashboard boleh menyediakan defaults, tetapi user bisa duplicate/edit.

---

## 4. Profitable LP Wallets

### Important constraint

Meteora Data API bisa menghitung PnL **untuk wallet yang sudah diketahui**, tetapi tidak menyediakan endpoint global “top profitable LP wallets”. Discovery wallet perlu sumber seed.

### Seed sources — urutan recommended

1. Manual wallet input.
2. Existing `smart_wallets` yang GMGN activity-nya memiliki `add`/`remove` liquidity.
3. Wallet yang berinteraksi dengan shortlisted Meteora pools, ditemukan dari on-chain DLMM events/Helius.
4. Baru kemudian perluas discovery global jika rate/cost terbukti aman.

### LP wallet metrics

- Closed/realized portfolio PnL USD dan SOL.
- Live/open PnL.
- PnL %.
- Fees and rewards claimed.
- Fee yield relative to deposits.
- Number of closed/open positions.
- Win rate per closed position.
- Median PnL; jangan hanya average karena outlier.
- Average holding duration.
- Out-of-range rate/time.
- Rebalance/withdraw/deposit frequency.
- Pool/token concentration.
- Data completeness and sample size.

### LP wallet tiers

Pisahkan dari trading Smart Wallet tiers:

- `lp_elite`
- `lp_solid`
- `lp_watch`
- `lp_candidate`
- `lp_dismissed`

Jangan gabungkan trading PnL dan LP PnL menjadi satu skor. Tampilkan keduanya berdampingan.

---

## 5. Cross-feature intelligence — highest-value addition

### Confluence view

Satu opportunity mendapat sinyal lintas modul:

- Token lolos safety profile.
- Pool lolos LP economics profile.
- Smart money sedang membeli/holding.
- Profitable LPer membuka atau menambah posisi.
- Fee/volume tetap sehat.

Output:

- `Token quality`
- `Pool economics`
- `Smart-wallet confirmation`
- `LP-wallet confirmation`
- `Risk/rejection reasons`
- `Freshness`

Ini lebih berguna daripada tiga panel yang terpisah total.

### Alerts yang disarankan

- New pool matches profile.
- Pool volume/fee/TVL jatuh di bawah threshold.
- Volatility melonjak.
- Tracked position out of range.
- Smart money exits/accumulates token.
- Profitable LPer opens/adds/removes/closes.
- Token risk worsens: dev sell, insider/bundler rise, liquidity drop, blacklist.
- Pool duplicate yang lebih baik muncul untuk token pair sama.

Alert harus cooldown + dedupe. Tidak ada auto-routing/trading tanpa approval.

---

## Minimal data model

Gunakan satu profile framework kecil, bukan arbitrary expression engine:

### `screening_profiles`

- `id`
- `domain`: `smart_wallet`, `token`, `lp_pool`, `lp_wallet`
- `name`
- `enabled`
- `source_config jsonb`
- `rules jsonb` — array `{field, op, value, missing_policy}`
- `sort_config jsonb`
- `created_at`, `updated_at`

### Domain data

- `token_metric_snapshots` — normalized scalar fields, indexed by mint/time.
- `lp_pools` — pool identity/current metadata.
- `lp_pool_snapshots` — TVL/volume/fees/APR/price/time.
- `lp_wallets` — candidate/tracked/manual metadata.
- `lp_wallet_snapshots` — aggregate portfolio metrics.
- `lp_positions` — current/closed position summaries.
- `screening_runs` + `screening_results` — profile version, candidate, decision, reasons, observed_at.

Jangan filter dashboard langsung dari jutaan `raw_events` atau JSONB observations. Gunakan latest scalar tables; raw payload hanya provenance/debug.

---

## Recommended phases

### Phase 0 — Stabilize source and providers

- Tetapkan repo/source-of-truth yang lengkap.
- Reconcile deployed binary, migrations, static dashboard, `/root/swi-src`, `/root/swi-deploy`.
- Fix/rotate Helius WebSocket path yang 429.
- Add provider request budget and freshness health.

### Phase 1 — Smart Wallet Custom Profiles

- Expose semua existing group/discovery settings.
- Add saved profiles, preview, manual override, pass/fail reasons.
- Preserve current defaults as a built-in profile.

### Phase 2 — Token Screener

- Normalize GMGN latest metrics.
- Add saved token profiles and results table.
- Add token detail/holder/trader drilldown.
- No LP joins yet.

### Phase 3 — LP Pool Screener

- Ingest Meteora pool snapshots.
- Native pool filters + GMGN token-profile join.
- Derived volatility/economics metrics.
- Shortlist and pool alerts.

### Phase 4 — LP Wallet Analytics

- Start with manual + existing smart-wallet seeds.
- Fetch Meteora portfolio/position/claim metrics.
- Add separate LP tiers and tracked positions.
- Add on-chain LP wallet discovery only after request-cost measurement.

### Phase 5 — Confluence and alerts

- Cross-score without hiding component scores.
- Alerts for new matches, exits, risk deterioration, out-of-range, fee collapse.

---

## Explicitly defer

- Auto-create/rebalance/close LP positions.
- Wallet private keys or transaction signing.
- Global scan of every Solana/Meteora wallet.
- Arbitrary nested Boolean rule DSL.
- ML prediction.
- “Real-time everything”; only shortlist needs frequent refresh.
- One opaque universal score.

## Recommended MVP

Ship only:

1. Smart Wallet saved profiles + preview + manual override.
2. Token saved filters using normalized GMGN metrics.
3. Meteora pool screener joined with one token safety profile.
4. Manual/known-wallet LP profitability tracking.

Global profitable-LPer discovery and cross-feature alerts follow after data quality, provider cost, and scoring validity are proven.

## Acceptance criteria for the planning stage

Before implementation, decide:

1. LP scope: DLMM only or DLMM + DAMM v2. Recommended: **DLMM only**.
2. Smart wallet matching: first matching ordered tier. Recommended: **keep existing first-match model**.
3. Rule composition: AND only or nested AND/OR. Recommended: **AND only v1**.
4. LP wallet discovery: manual/known seeds or global. Recommended: **manual + existing smart wallets first**.
5. Alert destination/frequency. Recommended: **dashboard first; Telegram opt-in later**.
6. Initial LP strategy profile values; defaults should be evaluated against snapshots before activation.

## Validation later (implementation phase)

- Unit tests for every metric/operator/missing policy.
- Profile preview must be side-effect-free.
- Golden payload tests for GMGN and Meteora schema drift.
- DB query plans: no full scan of `raw_events`/observation JSONB.
- Rate-limit simulation and retry/cooldown tests.
- Compare LP PnL against Meteora UI/API samples.
- Verify manual overrides survive profile edits/restarts.
- Dashboard end-to-end: create profile → preview → activate → candidate appears with reasons.
- No mutation/deployment until user approves implementation phase.
