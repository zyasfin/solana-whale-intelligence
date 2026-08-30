# Signal Forge — Review Brief (untuk agent hermes)

Tujuan: verifikasi seluruh artefak implementasi Signal Forge yang dihasilkan
dari dokumen kanonis `PLAN SWI`. Semua path di host `hermes-master`
(akses `root@hermes-master`). Ini review **read-only** — jangan ubah apa pun.

## 0. Konteks singkat

Dokumen arsitektur final (`PLAN-SWI-final-architecture-2026-08-29.md`) membekukan
arsitektur "Signal Forge". Dari dokumen itu telah dihasilkan:
- 25 module Rust (domain + runtime) di `/root/swi-src/src/sf/`.
- 17 migration SQL (`1001..1017`) di `/root/swi-deploy/migrations/`.
- 2 dokumen pendukung: `CONVENTIONS.md` + `REUSE_AUDIT.md`.

Status: ini adalah **fondasi/scaffold + runtime parsial**, bukan sistem jadi.
Tiga blocker arsitektur sudah dibekukan (RESOLVED). Implementasi runtime baru
3 modul (cost_basis, ingest_runtime, signal_gate), sisanya masih tipe domain.

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

### C. Kualitas SQL
8. Tidak ada forward-reference FK (tabel dirujuk sebelum di-CREATE).
9. Migration immutable (tidak mengubah migration yang sudah ada — hanya ADD).
10. Enum/CHECK konsisten dengan enum Rust di atas.

### D. Integritas repo
11. `lib.rs` valid (`pub mod sf`), `sf/mod.rs` mencantumkan semua module.
12. `Cargo.toml` punya dependency yang dibutuhkan runtime (rust_decimal,
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
