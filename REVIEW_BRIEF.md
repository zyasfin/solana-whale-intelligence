# Signal Forge — Review Brief (untuk agent hermes)

Tujuan: verifikasi seluruh artefak implementasi Signal Forge yang dihasilkan
dari dokumen kanonis `PLAN SWI`. Semua path di host `hermes-master`
(akses `root@hermes-master`). Ini review **read-only** — jangan ubah apa pun.

## ⚠️ Aturan main — anti over-review (WAJIB dibaca sebelum mulai)

Kerjakan review **fokus & efisien**. Jangan melebar, jangan perfeksionis.

### Prinsip
- Fokus hanya pada **correctness & bug**, bukan gaya/nitpick.
- Review **per-chunk kecil** (1 module runtime, atau 2–3 kalau erat).
- **Stop begitu hijau** — jangan terus cari "perbaikan kosmetik".
- Jangan ubah keputusan yang **sudah dibekukan** (PLAN SWI / CONVENTIONS.md).

### Yang WAJIB dicek (kelas bug yang sudah dikenal)
1. Field salah pakai (misal `amount_in` vs `amount_out`).
2. Key idempotency / dedup terlalu agresif.
3. Isolasi per `(chain, wallet, token)` / entity.
4. Fail-closed & archive-not-delete tetap terjaga.

### Yang JANGAN dilakukan (ini sumber over-review)
- ❌ Jangan refactor/rewrite kode yang sudah benar.
- ❌ Jangan tambah fitur di luar scope chunk yang ditugaskan.
- ❌ Jangan nitpick nama variabel, formatting, komentar (formatter/linter otomatis).
- ❌ Jangan ubah keputusan frozen tanpa instruksi eksplisit.
- ❌ Jangan kerjakan module lain di luar chunk.

### Definisi "selesai" (stop condition)
- `cargo test` hijau, dan regression test ditambahkan untuk tiap bug.
- Laporan **singkat**: bug ditemukan + fix + bukti test. Bukan esai panjang.

---

Dokumen arsitektur final (`PLAN-SWI-final-architecture-2026-08-29.md`) membekukan
arsitektur "Signal Forge". Dari dokumen itu telah dihasilkan:
- 25 module Rust (domain + runtime) di `/root/swi-src/src/sf/`.
- 17 migration SQL (`1001..1017`) di `/root/swi-deploy/migrations/`.
- 2 dokumen pendukung: `CONVENTIONS.md` + `REUSE_AUDIT.md`.

Status: ini adalah **fondasi/scaffold + runtime parsial**, bukan sistem jadi.
Tiga blocker arsitektur sudah dibekukan (RESOLVED). Implementasi runtime sudah
mencakup 15 modul (cost_basis, ingest_runtime, signal_gate, wallet_runtime,
portfolio_runtime, source_health_runtime, token_runtime, caller_runtime,
revival_runtime, narrative_runtime, dashboard_runtime, graph_runtime,
lp_runtime, strategy_runtime, plus `source::select_provider`), sisanya masih
tipe domain.

## 1. Daftar artefak (path lengkap)

### Dokumen kanonis + input terkait
```
/root/PLAN-SWI-final-architecture-2026-08-29.md      (SHA256: 482e24ac6e0342dfe001aaf155f9d12f9879c4d1cfdc1123e37300a43d64d7d1)
/root/signal-forge-source-matrix-2026-08-28.md
/root/signal-forge-meridian-feature-audit-2026-08-28.md
/root/signal-forge-kaiser-charon-feature-audit-2026-08-29.md
/root/signal-forge-security-auto-trade-design.md
/root/signal-forge-lp-autopilot-design.md
```

### Dokumen pendukung hasil kerja
```
/root/swi-src/CONVENTIONS.md      — keputusan frozen + 3 blocker (RESOLVED)
/root/swi-src/REUSE_AUDIT.md      — pemetaan konsep pra-freeze → final
```

### Crate Rust
```
/root/swi-src/Cargo.toml          — manifest
/root/swi-src/src/lib.rs          — gerbang crate: pub mod sf
/root/swi-src/src/main.rs         — bin lama (research prototype, belum pakai sf)
```

### Module domain (`/root/swi-src/src/sf/`)
```
mod.rs  core.rs  identity.rs  source.rs  graph.rs  decision.rs  intent.rs
jobs.rs  auth.rs  token.rs  wallet.rs  portfolio.rs  ingest.rs  dashboard.rs
caller.rs  narrative.rs  browser.rs  revival.rs  lp.rs  strategy.rs
execution.rs  autonomy.rs
```

### Module runtime (logika kerja — fokus utama review)
```
/root/swi-src/src/sf/cost_basis.rs       — FIFO cost-basis matching
/root/swi-src/src/sf/ingest_runtime.rs   — ingestion pipeline §7.2
/root/swi-src/src/sf/signal_gate.rs      — entry signal gate model
/root/swi-src/src/sf/wallet_runtime.rs   — §8.6 swap reconstruction → wallet intel
/root/swi-src/src/sf/portfolio_runtime.rs — §8.11 portfolio projections + risk
/root/swi-src/src/sf/source_health_runtime.rs — §8.12 source-health state machine
/root/swi-src/src/sf/token_runtime.rs        — §8.2 token lifecycle + wake gate
/root/swi-src/src/sf/caller_runtime.rs       — §8.5 caller intelligence
/root/swi-src/src/sf/revival_runtime.rs      — §8.8 revival stage progression
/root/swi-src/src/sf/narrative_runtime.rs    — §8.4 name/meme provenance
/root/swi-src/src/sf/dashboard_runtime.rs    — §21/§22 dashboard + retention
/root/swi-src/src/sf/graph_runtime.rs        — §9 entity edges + false confluence
/root/swi-src/src/sf/lp_runtime.rs           — §8.9 LP pool metrics
/root/swi-src/src/sf/strategy_runtime.rs     — §13 strategy lifecycle + evaluation
```

### Migration SQL (`/root/swi-deploy/migrations/`)
```
1001_access_control.sql   1002_sources_providers.sql  1003_event_evidence.sql
1004_graph_model.sql      1005_intelligence.sql       1006_strategy_evaluation.sql
1007_decisions_execution.sql 1008_jobs_outbox.sql     1009_core_intelligence.sql
1010_caller_provenance.sql 1011_revival_cabal.sql     1012_lp_intelligence.sql
1013_strategy_lab.sql     1014_secure_execution.sql   1015_autonomy.sql
1016_transition_table.sql 1017_rollout_thresholds.sql
```
(Legacy pra-freeze `0001..0010` DITINGGALKAN — replace-total.)

### Kode pra-freeze (referensi konsep, BUKAN untuk disalin — prinsip #13)
```
/root/swi-src/src/funding_radar.rs  scoring.rs  signals.rs  replay.rs
helius.rs  telegram_ingest.rs  gmgn.rs  narrative.rs  graph.rs  dll.
```

## 2. Titik fokus review (acceptance criteria)

### A. Konsistensi terhadap dokumen (paling penting)
1. Enum/state harus cocok verbatim dengan PLAN SWI:
   - Intent state (11) — bandingkan `sf/intent.rs` vs §16.
   - Token lifecycle (9) — `sf/token.rs` vs §8.2.
   - Strategy lifecycle (9) — `sf/strategy.rs` vs §13.
   - Trading modes / LP actions — `sf/execution.rs` vs §14/§15.
2. Transition table (blocker #1) — `sf/intent.rs::can_transition_to` harus
   konsisten dengan `1016_transition_table.sql` + CONVENTIONS.md §3.
3. Rollout thresholds (blocker #2) — `sf/autonomy.rs::RolloutThresholds` harus
   = 30 sample / 14 hari / -10% / CI>0 / wajib approval (CONVENTIONS.md §4).
4. Provider tie-break (blocker #3) — `sf/source.rs::select_provider` harus
   3-tahap (eligibility → ranking → source_id tie-break) sesuai §5.

### B. Kebenaran logika runtime
5. `cost_basis.rs::fifo_match` — FIFO benar (buys buka lot, sells tutup
   tertua), cost proporsional, sell berlebih tidak bikin short, residual cost
   akurat (perhatikan: `lot.cost_usd` harus ikut dikurangi saat partial).
6. `ingest_runtime.rs::run_pipeline` — 10 stage §7.2, fail-closed pada envelope
   invalid, idempotency 2-mode, malformed counted not fatal.
7. `signal_gate.rs::evaluate_entry_gate` — first-failure-wins, 10 gate berurutan,
   fail-closed pada None (age/liquidity/market), wash vs critical terpisah.
8. `wallet_runtime.rs` (§8.6) — cek hal berikut:
   - `reconstruct_swaps` — average cost = open_cost / open_amount (0 jika tak
     ada lot), realized PnL = Σ matched proceeds − Σ consumed cost, unrealized
     PnL = mark − residual cost (tanpa mark → −cost, JANGAN fabricate harga).
   - Reuse `fifo_match` (bukan duplikasi logika FIFO); isolasi per
     `(chain, wallet, token)` terjaga lewat engine yang sama.
   - `early_entry_timing` — `wallet_first − token_birth_ts`; `None` bila salah
     satu timestamp absen (jangan infer timestamp).
   - `compute_recurrence` — tokens_traded & distinct_clusters dihitung dari
     `HashSet` (distinct, bukan total swap).
9. `portfolio_runtime.rs` (§8.11) — cek hal berikut:
   - `project` — total notional = Σ semua line; `by_kind` subtotal per
     token/pool/chain/strategy benar; correlated exposure pakai union-find dan
     dihitung SEKALI per grup (bukan additive per line); malformed notional →
     0 (fail-closed, jangan parse jadi error).
   - `check_treasury_separation` — fail-closed: treasury tidak terpisah = Err.
10. `source_health_runtime.rs` (§8.12) — cek hal berikut:
    - State verbatim: UP/SILENT/DEGRADED/DOWN/RECOVERING/DISABLED.
    - Aturan kunci: **connected-but-silent = SILENT** (request terus tapi tak
      ada success dalam cadence → SILENT, bukan UP).
    - schema_stale → DEGRADED; consecutive_failures ≥ 3 → DOWN; satu sample
      bagus setelah degraded → RECOVERING (bukan langsung UP); disabled → DISABLED.
    - `apply` harus mutate `ProviderHealth` konsisten dengan `transition`.
11. `token_runtime.rs` (§8.2) — cek hal berikut:
    - State verbatim: CREATED/PRE_GRADUATION/MIGRATED/FIRST_LIQUIDITY/ACTIVE/
      COOLING/DORMANT/ARCHIVED/TOMBSTONED.
    - `can_advance` forward-only; ARCHIVED & TOMBSTONED terminal (tak bisa lanjut).
    - `wake_gate` fail-closed: signal harus > baseline; data absen = no wake.
12. `caller_runtime.rs` (§8.5) — cek hal berikut:
    - `hit_rate` hanya atas call ber-outcome; tanpa outcome → None (jangan fabricate).
    - `average_mfe` hanya atas outcome ber-MFE.
    - `clamp_window_days` cap +21d.
    - `build_reputation` confidence monotonik (0..1, tak pernah 1.0 persis).
13. `revival_runtime.rs` (§8.8) — cek hal berikut:
    - Stage urut: Wake → DormantBaselineComparison → ActivationGate → Refresh →
      RevivalQuality → OpportunityEvaluation.
    - Gate fail → berhenti di ActivationGate + failure memory dibawa.
    - No wake → stay di Wake.
14. `narrative_runtime.rs` (§8.4) — cek hal berikut:
    - Stage verbatim (perhatikan `WebXTiktokSearch`, X kapital).
    - `resolve` ekstrak role originator/adopter/spread; tanpa evidence → stay
      DeployFirstLiquidity (fail-closed).
15. `dashboard_runtime.rs` (§21/§22) — cek hal berikut:
    - `retention_tier`: active → Hot; open window → Warm; else Cold.
    - `is_operational`: hanya Hot + opportunity_first.
16. `graph_runtime.rs` (§9) — cek hal berikut:
    - `is_edge_valid` pakai valid_from/until; timestamp unparseable → false.
    - `is_false_confluence`: disjoint evidence + low confidence + non-Confirmed.
    - `neighbor_count` distinct (HashSet).
17. `lp_runtime.rs` (§8.9) — cek hal berikut:
    - `fee_to_tvl`: None bila data absen atau TVL=0 (jangan bagi nol).
    - `is_supported_scope`: ETH/Base/BSC = N/A (Uniswap/Pancake false).
18. `strategy_runtime.rs` (§13) — cek hal berikut:
    - Lifecycle verbatim: DRAFT/SHADOW/PAPER/VALIDATED/APPROVED/CANARY/ACTIVE/
      PAUSED/RETIRED.
    - `can_transition` forward-only + pause/resume toggle (ACTIVE↔PAUSED); RETIRED
      terminal.
    - `shadow_passed` fail-closed: ada rejected candidate = FAIL.
    - `negative_findings` menjumlah rejected candidates (retention req #13).

### C. Kualitas SQL
19. Tidak ada forward-reference FK (tabel dirujuk sebelum di-CREATE).
20. Migration immutable (tidak mengubah migration yang sudah ada — hanya ADD).
21. Enum/CHECK konsisten dengan enum Rust di atas.

### D. Integritas repo
22. `lib.rs` valid (`pub mod sf`), `sf/mod.rs` mencantumkan semua module.
23. `Cargo.toml` punya dependency yang dibutuhkan runtime (rust_decimal,
    chrono, serde). Catatan: toolchain Rust TIDAK terpasang di host ini.

## 3. Batas yang diketahui (jangan dilaporkan sebagai bug)

- Scaffold `sf/` TIDAK dihubungkan ke `main.rs` (bin lama) — disengaja ("nanti dulu").
- Runtime 3 modul belum terhubung ke PostgreSQL (`sqlx`) — masih pure logic + test.
- Migration belum dijalankan ke DB live (belum ada instance Postgres terverifikasi).
- Tidak ada toolchain Rust di host → `cargo build` belum dijalankan di sini.
  (Scaffold divalidasi terpisah: 20 test lulus di Rust 1.97.)

## 4. Perintah verifikasi cepat (untuk agent)

```bash
# 1. Verifikasi checksum dokumen kanonis
sha256sum /root/PLAN-SWI-final-architecture-2026-08-29.md
# harus: 482e24ac6e0342dfe001aaf155f9d12f9879c4d1cfdc1123e37300a43d64d7d1

# 2. List semua module sf
ls /root/swi-src/src/sf/

# 3. List migration baru
ls /root/swi-deploy/migrations/101*.sql

# 4. Cek module terdeklarasi di mod.rs
grep -c 'pub mod' /root/swi-src/src/sf/mod.rs

# 5. Baca ringkasan keputusan
cat /root/swi-src/CONVENTIONS.md
```

## 5. Status review (updated after agent hermes review)

Review telah dijalankan oleh agent hermes. Hasil: **4 bug ditemukan & diperbaiki**.

Bug yang ditemukan (test `tests/review_runtime_regressions.rs`):
1. `cost_basis.rs` — buy memakai `amount_in` (aset dibayar) bukan `amount_out`
   (token diterima) → lot buy kini pakai `amount_out`.
2. `cost_basis.rs` — FIFO tidak terisolasi per wallet → kini group by
   `(chain, wallet, token)`.
3. `cost_basis.rs` — oversell menghitung proceeds penuh → kini proporsional
   `usd_value × (matched/sell_amount)`.
4. `ingest_runtime.rs` — fallback idempotency key terlalu agresif → kini
   `source + entity + event_type + time_bucket + raw_hash`; `RawPayload`
   bertambah field `event_type`.

Regression test in-module ditambahkan di `cost_basis.rs` (3 test) dan
`ingest_runtime.rs` (2 test).

Verifikasi build/test (crate `solana-whale-intelligence`, Rust 1.97):
- `cargo build` bersih.
- `cargo test` = 143 passed, 0 failed (134 legacy + 4 agent review + 5 regression).
