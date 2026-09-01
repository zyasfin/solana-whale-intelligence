# Signal Forge — Review Results

Lokasi kanonis hasil review source: file ini, di root Git `swi-src`.

## Aturan pencatatan

- Append-only: satu section `REV-XXX` per review.
- Catat tanggal, scope module, acceptance criteria, bug, verdict, dan hasil test.
- Review fokus sesuai `REVIEW_BRIEF.md`; jangan mencampur temuan di luar scope.
- Temuan memakai status `OPEN`, `FIXED`, atau `ACCEPTED`.
- Setelah update, review berikutnya mengacu ID temuan lama tanpa menghapus sejarah.

## Indeks

| ID | Tanggal | Scope | Verdict | Test |
|---|---|---|---|---|
| REV-001 | 2026-08-30 | `wallet_runtime`, `portfolio_runtime`, `source_health_runtime` | 2 bug | 159 passed, 0 failed |
| REV-002 | 2026-08-30 | re-review fix F01 + F02 | APPROVED | 162 passed, 0 failed |
| REV-003 | 2026-08-30 | `token_runtime`–`strategy_runtime` (B11–B18) | 5 bug | 196 passed, 0 failed |
| REV-004 | 2026-08-30 | re-review fix F01–F05 | APPROVED | 198 passed, 0 failed |
| REV-005 | 2026-08-30 | final runtime B19–B22 | 1 bug | 218 passed, 0 failed |
| REV-006 | 2026-08-30 | re-review fix F01 | APPROVED | 218 passed, 0 failed |
| REV-007 | 2026-08-31 | whole code vs canonical PLAN SWI | 12 critical/high + 7 medium | 218 passed, 0 failed |
| REV-008 | 2026-08-31 | re-review logic fixes F03–F19 | LOGIC APPROVED | 226 passed, 0 failed |
| REV-009 | 2026-08-31 | independent verification of REV-008 | 7 fixed, 7 partial, 1 unfixed | 226 passed; Rust 1.89 PASS |
| REV-010 | 2026-08-31 | re-review logic fixes F01–F08 + addendum | LOGIC APPROVED | 231 passed, 0 failed |
| REV-011 | 2026-08-31 | independent verification of REV-010 | CHANGES REQUIRED: 1 fixed, 7 partial | 231 passed; Rust 1.89 PASS |
| REV-012 | 2026-08-31 | re-review logic fixes F01–F07 | LOGIC APPROVED | 232 passed, 0 failed |
| REV-013 | 2026-08-31 | independent verification of REV-012 | CHANGES REQUIRED: 4 fixed, 3 partial | 232 passed; Rust 1.89 PASS |
| REV-014 | 2026-08-31 | re-review logic fixes F01–F03 | LOGIC APPROVED | 233 passed, 0 failed |
| REV-015 | 2026-08-31 | independent verification of REV-014 | CHANGES REQUIRED: 2 fixed, 3 partial | 233 passed; Rust 1.89 PASS |
| REV-016 | 2026-08-31 | re-review logic fixes F01–F03 | LOGIC APPROVED | 235 passed, 0 failed |
| REV-017 | 2026-08-31 | independent verification of REV-016 | CHANGES REQUIRED: 1 fixed, 2 partial | 235 passed; Rust 1.89 PASS |
| REV-018 | 2026-08-31 | re-review logic fixes F01–F02 | LOGIC APPROVED | 235 passed, 0 failed |
| REV-019 | 2026-08-31 | independent verification of REV-018 | CHANGES REQUIRED: 1 fixed, 1 partial | 235 passed; Rust 1.89 lib PASS |
| REV-020 | 2026-09-01 | architecture amendment: Token Recent + Deployer/Social Reuse | FEATURE ACCEPTED / NOT IMPLEMENTED | PLAN hash 242b8091...32bf63f |
| REV-022 | 2026-09-01 | independent review of REV-021 | CHANGES REQUIRED / PARTIAL: 10 findings | 264 passed; Rust 1.89 check PASS; DB replay unverified |

---

## REV-001 — Phase 1 runtime: wallet, portfolio, source health

**Tanggal:** 2026-08-30 10:33 UTC  
**Mode:** Read-only correctness review  
**Acuan:** `REVIEW_BRIEF.md` section B item 8, 9, 10; `CONVENTIONS.md`  
**Scope:**

- `src/sf/wallet_runtime.rs`
- `src/sf/portfolio_runtime.rs`
- `src/sf/source_health_runtime.rs`
- Referensi frozen: `wallet.rs`, `portfolio.rs`, `source.rs`, `core.rs`, `cost_basis.rs`

### Temuan

#### REV-001-F01 — FIXED — Isolasi multi-wallet bocor pada agregasi wallet

**Lokasi:** `src/sf/wallet_runtime.rs`, `reconstruct_swaps` (sekitar line 69), `build_wallet_intelligence` (sekitar line 156).

**Masalah:** `fifo_match` mengisolasi `(chain, wallet, token)`, tetapi residual dikembalikan dengan key `token` saja. Jika input memuat beberapa wallet yang memperdagangkan token sama, residual dapat saling overwrite/mencemari average cost dan unrealized PnL. `build_wallet_intelligence` juga belum memastikan seluruh swap cocok dengan argumen `chain` dan `address`.

**Saran fix:** validasi atau filter swap berdasarkan `chain + address` sebelum rekonstruksi. Tambahkan regression test dua wallet memperdagangkan token yang sama.

#### REV-001-F02 — FIXED — Failure threshold source health tidak selalu menghasilkan DOWN

**Lokasi:** `src/sf/source_health_runtime.rs`, `transition` (sekitar line 45), `apply` (sekitar line 92).

**Masalah:** `consecutive_failures >= 3` hanya diperiksa saat `last_success_secs == None`. Jika historical success masih `Some` tetapi tiga request terbaru gagal, state dapat menjadi `UP`, `RECOVERING`, atau `SILENT`, bukan `DOWN`. `apply` juga mereset counter ke `0` setiap `last_success_secs` berisi nilai.

**Saran fix:** setelah pemeriksaan `disabled`, prioritaskan `consecutive_failures >= 3 => DOWN`. Jangan reset counter hanya karena historical success tersedia. Tambahkan regression test `last_success_secs=Some` dengan `consecutive_failures=3`.

### Acceptance criteria

| Item | Module | Hasil |
|---|---|---|
| B8 | `wallet_runtime.rs` | FAIL — temuan `REV-001-F01` |
| B9 | `portfolio_runtime.rs` | PASS — sesuai criteria |
| B10 | `source_health_runtime.rs` | FAIL — temuan `REV-001-F02` |

### Verifikasi

```text
cargo test
159 passed
0 failed
```

Rincian:

```text
21 module tests + 134 legacy tests + 4 regression tests = 159
```

### Verdict

**CHANGES REQUIRED** — perbaiki `REV-001-F01` dan `REV-001-F02`, tambahkan regression test, lalu append hasil re-review sebagai `REV-002`.

---

## REV-002 — Re-review setelah fix REV-001-F01 & F02

**Tanggal:** 2026-08-30 (post-fix)  
**Mode:** Re-verifikasi fix (read-only)  
**Acuan:** `REVIEW_BRIEF.md` section B item 8, 9, 10  
**Scope:** fix `REV-001-F01` + `REV-001-F02`

### Hasil fix

#### REV-001-F01 — ACCEPTED

- `cost_basis.rs` — `CostBasisResidual.per_token` key diubah dari `String` (token)
  menjadi `(chain, wallet, token)`; residual tidak lagi saling overwrite antar
  wallet yang memperdagangkan token sama.
- `wallet_runtime.rs` — `reconstruct_swaps` iterasi key tuple; `build_wallet_intelligence`
  memfilter swap ke `(chain, address)` milik wallet sebelum rekonstruksi.
- Regression test: `residual_isolated_by_wallet_for_same_token` (cost_basis.rs) +
  `build_wallet_intelligence_filters_other_wallets` (wallet_runtime.rs).

#### REV-001-F02 — ACCEPTED

- `source_health_runtime.rs` — `transition` memprioritaskan `consecutive_failures >= 3`
  → DOWN (setelah `disabled`, sebelum cabang success).
- `apply` tidak lagi mereset `consecutive_failures` ke 0 saat `last_success_secs`
  masih `Some`; counter mengikuti `signal.consecutive_failures`.
- Regression test: `failures_dominate_historical_success`.

### Acceptance criteria (re-check)

| Item | Module | Hasil |
|---|---|---|
| B8 | `wallet_runtime.rs` | PASS — F01 fixed |
| B9 | `portfolio_runtime.rs` | PASS — tidak berubah |
| B10 | `source_health_runtime.rs` | PASS — F02 fixed |

### Verifikasi

```text
cargo build — clean (no warning)
cargo test — 162 passed, 0 failed (24 module + 134 legacy + 4 regression)
```

### Verdict

**APPROVED** — kedua temuan REV-001 diperbaiki, regression test hijau.


---

## REV-003 — Runtime token hingga strategy

**Tanggal:** 2026-08-30 11:01 UTC  
**Mode:** Read-only correctness review  
**Acuan:** `REVIEW_BRIEF.md` section B item 11–18; `CONVENTIONS.md`  
**Scope:** `token_runtime.rs`, `caller_runtime.rs`, `revival_runtime.rs`, `narrative_runtime.rs`, `dashboard_runtime.rs`, `graph_runtime.rs`, `lp_runtime.rs`, `strategy_runtime.rs`.

### Temuan

#### REV-003-F01 — FIXED — Revival stage dapat dilompati

**Lokasi:** `src/sf/revival_runtime.rs`, `can_progress` (sekitar line 78).

**Masalah:** `stage_index(next) > stage_index(current)` mengizinkan lompatan, misalnya `Wake` langsung ke `OpportunityEvaluation`; B13 mensyaratkan urutan stage.

**Saran fix:** hanya izinkan stage adjacent: `stage_index(next) == stage_index(current) + 1`; tambah regression test skip stage.

#### REV-003-F02 — FIXED — Timestamp `valid_from` rusak tidak mutlak fail-closed

**Lokasi:** `src/sf/graph_runtime.rs`, `is_edge_valid` (sekitar line 19–20).

**Masalah:** `parse_secs(...).unwrap_or(i64::MAX)` dapat menganggap timestamp rusak valid ketika `now_secs == i64::MAX` dan `valid_until == None`. B16 mensyaratkan timestamp unparseable selalu `false`.

**Saran fix:** `let Some(from) = parse_secs(...) else { return false; };`; tambah regression test malformed `valid_from` pada batas `i64::MAX`.

#### REV-003-F03 — FIXED — Confidence hilang dianggap confidence rendah

**Lokasi:** `src/sf/graph_runtime.rs`, `is_false_confluence` (sekitar line 51).

**Masalah:** `confidence.unwrap_or(0.0)` menyamakan missing dengan zero/low confidence. False confluence bisa ditandai tanpa bukti confidence rendah; missing bukan zero.

**Saran fix:** low-confidence hanya bila kedua confidence `Some` dan `< 0.5`; tambah test `None` tidak dianggap low confidence.

#### REV-003-F04 — FIXED — Robinhood Uniswap/Pancake ditolak LP scope gate

**Lokasi:** `src/sf/lp_runtime.rs`, `is_supported_scope` (sekitar line 32–42).

**Masalah:** semua `UniswapV2/V3/V4` dan `PancakeV2/V3` selalu `false`. Frozen scope mendukung protokol tersebut pada Robinhood; hanya Ethereum/Base/BSC yang `N/A`.

**Saran fix:** izinkan Uniswap/Pancake hanya pada Robinhood; tetap tolak Ethereum/Base/BSC dan Solana. Tambah test Robinhood true + ETH/Base/BSC false.

#### REV-003-F05 — FIXED — Strategy lifecycle dapat melompati gate

**Lokasi:** `src/sf/strategy_runtime.rs`, `can_transition` (sekitar line 37–51).

**Masalah:** `stage_index(next) > stage_index(current)` mengizinkan `Draft -> Active`; shadow/paper/validation/approval dapat dilewati.

**Saran fix:** hanya izinkan transisi adjacent, ditambah toggle khusus `Active <-> Paused`; `Retired` tetap terminal. Tambah regression test `Draft -> Active` ditolak.

### Acceptance criteria

| Item | Module | Hasil |
|---|---|---|
| B11 | `token_runtime.rs` | PASS |
| B12 | `caller_runtime.rs` | PASS |
| B13 | `revival_runtime.rs` | FAIL — `REV-003-F01` |
| B14 | `narrative_runtime.rs` | PASS |
| B15 | `dashboard_runtime.rs` | PASS |
| B16 | `graph_runtime.rs` | FAIL — `REV-003-F02`, `REV-003-F03` |
| B17 | `lp_runtime.rs` | FAIL — `REV-003-F04` |
| B18 | `strategy_runtime.rs` | FAIL — `REV-003-F05` |

### Verifikasi

```text
cargo test
196 passed
0 failed
```

Rincian: `58 module + 134 legacy + 4 regression = 196`.

### Verdict

**CHANGES REQUIRED** — perbaiki `REV-003-F01` sampai `REV-003-F05`, tambah regression test, lalu append re-review sebagai `REV-004`.

---

## REV-004 — Re-review setelah fix REV-003-F01..F05

**Tanggal:** 2026-08-30 (post-fix)  
**Mode:** Re-verifikasi fix (read-only)  
**Acuan:** `REVIEW_BRIEF.md` section B item 11–18  
**Scope:** fix `REV-003-F01` sampai `REV-003-F05`

### Hasil fix

#### REV-003-F01 — ACCEPTED

- `revival_runtime.rs` — `can_progress` kini `stage_index(next) == stage_index(current) + 1`
  (adjacent-only); tidak bisa lompat stage.
- Regression test: `progression_is_forward_only` menambah assert skip-stage.

#### REV-003-F02 — ACCEPTED

- `graph_runtime.rs` — `is_edge_valid` pakai `let Some(from) = parse_secs(...) else { return false; }`;
  timestamp rusak mutlak fail-closed.
- Regression test: `malformed_valid_from_fails_closed`.

#### REV-003-F03 — ACCEPTED

- `graph_runtime.rs` — `is_false_confluence` low-confidence hanya bila kedua confidence
  `Some` dan `< 0.5`; `None` tidak dianggap low (missing != zero).
- Regression test: `missing_confidence_is_not_false_confluence`.

#### REV-003-F04 — ACCEPTED

- `lp_runtime.rs` — `is_supported_scope` kini izinkan Uniswap/Pancake pada Robinhood
  (`robinhood`/`rh`); tetap tolak Ethereum/Base/BSC dan Solana (Meteora-only).
- Regression test: `scope_gate_matches_frozen_scope`.

#### REV-003-F05 — ACCEPTED

- `strategy_runtime.rs` — `can_transition` kini `stage_index(next) == stage_index(current) + 1`
  (adjacent-only) + toggle khusus `Active <-> Paused`; `Retired` terminal.
- Regression test: `lifecycle_forward_only_with_pause_toggle` menambah assert `Draft -> Active` ditolak.

### Acceptance criteria (re-check)

| Item | Module | Hasil |
|---|---|---|
| B11 | `token_runtime.rs` | PASS |
| B12 | `caller_runtime.rs` | PASS |
| B13 | `revival_runtime.rs` | PASS — F01 fixed |
| B14 | `narrative_runtime.rs` | PASS |
| B15 | `dashboard_runtime.rs` | PASS |
| B16 | `graph_runtime.rs` | PASS — F02/F03 fixed |
| B17 | `lp_runtime.rs` | PASS — F04 fixed |
| B18 | `strategy_runtime.rs` | PASS — F05 fixed |

### Verifikasi

```text
cargo build — clean (no warning)
cargo test — 198 passed, 0 failed (60 module + 134 legacy + 4 regression)
```

### Verdict

**APPROVED** — kelima temuan REV-003 diperbaiki, regression test hijau.

---

## REV-005 — Final runtime: execution, autonomy, browser, decision

**Tanggal:** 2026-08-30 11:19 UTC  
**Mode:** Read-only correctness review  
**Acuan:** `REVIEW_BRIEF.md` section B item 19–22; `CONVENTIONS.md` §3–§5  
**Scope:** `execution_runtime.rs`, `autonomy_runtime.rs`, `browser_runtime.rs`, `decision_runtime.rs`, serta domain type frozen terkait.

### Temuan

#### REV-005-F01 — FIXED — Execution state machine menyimpang dari transition table frozen

**Lokasi:** `src/sf/execution_runtime.rs`, `ExecutionState` dan `can_transition` (sekitar line 24–65).

**Masalah:** runtime membuat state machine berbasis indeks yang berbeda dari `intent.rs::IntentState::can_transition_to`. Akibatnya:

- state skip diterima, misalnya `Proposed -> Submitted`;
- transisi fail-safe ilegal diterima, misalnya `Proposed -> FailedSafe`;
- `UnknownReconciliation` memblok semua transisi, termasuk hasil rekonsiliasi sah ke `Confirmed`, `FailedSafe`, atau `Cancelled`;
- `Confirmed` tidak diperlakukan terminal dan dapat berpindah ke sibling terminal lain;
- runtime mengganti state frozen `Approved/Built` dengan `Quoted`, sehingga dua transition table dapat berbeda.

**Saran fix:** jangan duplikasi transition table. Gunakan `intent::IntentState` dan delegasikan ke `IntentState::can_transition_to`, atau encode tabel `matches!` yang identik verbatim. Tambahkan regression test untuk menolak skip/illegal fail-safe, mengizinkan tiga hasil rekonsiliasi, dan memastikan seluruh terminal state tidak punya outgoing transition.

### Acceptance criteria

| Item | Module | Hasil |
|---|---|---|
| B19 | `execution_runtime.rs` | FAIL — `REV-005-F01` |
| B20 | `autonomy_runtime.rs` | PASS |
| B21 | `browser_runtime.rs` | PASS |
| B22 | `decision_runtime.rs` | PASS |

### Verifikasi

```text
cargo test
218 passed
0 failed
```

Rincian: `80 module + 134 legacy + 4 regression = 218`.

### Verdict

**CHANGES REQUIRED** — perbaiki `REV-005-F01`, tambah regression test, lalu append re-review sebagai `REV-006`.

---

## REV-006 — Re-review setelah fix REV-005-F01

**Tanggal:** 2026-08-30 (post-fix)  
**Mode:** Re-verifikasi fix (read-only)  
**Acuan:** `REVIEW_BRIEF.md` section B item 19  
**Scope:** fix `REV-005-F01`

### Hasil fix

#### REV-005-F01 — ACCEPTED

- `execution_runtime.rs` — `ExecutionState` kini re-export dari `intent::IntentState`
  (bukan enum duplikat); `can_transition` mendelegasikan ke
  `IntentState::can_transition_to` (transition table frozen). Tidak ada duplikasi
  state machine.
- Regression test: `execution_forward_matches_frozen_table` (tolak skip/illegal),
  `reconciliation_allows_only_three_outcomes` (UNKNOWN_RECONCILIATION hanya ke
  CONFIRMED/FAILED_SAFE/CANCELLED).

### Acceptance criteria (re-check)

| Item | Module | Hasil |
|---|---|---|
| B19 | `execution_runtime.rs` | PASS — F01 fixed |
| B20 | `autonomy_runtime.rs` | PASS |
| B21 | `browser_runtime.rs` | PASS |
| B22 | `decision_runtime.rs` | PASS |

### Verifikasi

```text
cargo build — clean (no warning)
cargo test — 218 passed, 0 failed (80 module + 134 legacy + 4 regression)
```

### Verdict

**APPROVED** — temuan REV-005 diperbaiki, regression test hijau. Seluruh runtime
module (B5–B22) kini PASS.

---

## REV-007 — Whole-code audit vs canonical PLAN SWI

**Tanggal:** 2026-08-31 01:58 UTC  
**Mode:** Full architecture/conformance audit; source read-only  
**PLAN:** `/root/PLAN-SWI-final-architecture-2026-08-29.md`  
**PLAN SHA-256:** `482e24ac6e0342dfe001aaf155f9d12f9879c4d1cfdc1123e37300a43d64d7d1`  
**Git HEAD:** `9adb81450d89fc7eb15d9354d9b43d2490950a88`

Runtime pure-logic B5–B22 lulus review sebelumnya. Namun produk masih **PARTIAL FOUNDATION**, belum memenuhi acceptance gates PLAN, dan **NO-GO untuk menggantikan service LXC**.

### Verified blockers / mismatches

#### REV-007-F01 — CRITICAL — Migration bundle tidak dapat initialize/upgrade DB

**Lokasi:** `swi-deploy/migrations/0001_initial.sql`, `1005_intelligence.sql`, `1006_strategy_evaluation.sql`, `1007_decisions_execution.sql`, `1013_strategy_lab.sql`, `1016_transition_table.sql`.

- Full chain `0001..0010 + 1001..1017` gagal di `1005`: relation `tokens` sudah ada dengan schema legacy inkompatibel.
- Canonical-only `1001..1017` gagal di `1016`: `capital_reservations.expires_at` sudah dibuat `1007`, lalu ditambahkan lagi.
- `1013` drop nama constraint lifecycle yang salah; CHECK lama tetap menolak `canary/paused`.
- Tidak ada replace-total cutover/data migration executable.

**PLAN:** §4.6, §10, §13, §16, §24.  
**Fix:** canonical fresh schema atau migration bridge eksplisit; migration baru untuk constraint/duplicate; fresh + upgrade replay test `ON_ERROR_STOP=1`.

#### REV-007-F02 — CRITICAL — Artifact sekarang tetap legacy, bukan Signal Forge runtime

**Lokasi:** `src/lib.rs`, `src/main.rs`, `src/workers.rs`, `src/api.rs`, `static/index.html`.

`sf` hanya diekspor sebagai library; binary/worker/API tidak mereferensikannya. Runtime canonical belum persistent. UI masih literal `review-only placeholder`. Mengganti binary LXC sekarang tetap menjalankan behavior legacy dengan dashboard kosong.

**PLAN:** §3, §4.1–4.5, §24.  
**Status:** wiring deferred, tetapi blocker deployment.  
**Fix:** wire satu vertical slice canonical; persistence/jobs/outbox nyata; UI nyata; staging health/smoke test sebelum cutover.

#### REV-007-F03 — HIGH — Decision taxonomy terpecah/tidak lengkap

**Lokasi:** `src/sf/decision.rs::ComponentClass`, `src/sf/portfolio.rs::ComponentClass`, `src/sf/decision_runtime.rs::evaluate`, SQL `1007/1009`.

Decision runtime hanya mengenal Mandatory/Sizing. PLAN mewajibkan mandatory/sizing/strategy/halt. Enum empat kelas ada terpisah tetapi tidak dipakai. Source degradation/drawdown/reconciliation tidak dapat menjadi HALT input canonical.

**PLAN:** §12.2.  
**Fix:** satu enum canonical empat kelas; align Rust/SQL/API; halt-input tests.

#### REV-007-F04 — HIGH — Decision tanpa evidence dapat APPROVE

**Lokasi:** `src/sf/decision_runtime.rs::evaluate`, `is_reproducible`.

`evaluate()` tidak memeriksa evidence snapshot, freshness/completeness, atau ketiadaan mandatory components. Bundle kosong dapat menjadi `Approve`; `InsufficientEvidence` dan `is_reproducible()` tidak terhubung ke gate.

**PLAN:** §12.1–12.3, §23, acceptance #1–#3.  
**Fix:** reject/insufficient bila evidence kosong, mandatory freshness stale/missing, atau mandatory set tidak lengkap.

#### REV-007-F05 — HIGH — Signer checklist lulus saat semantics mandatory absen

**Lokasi:** `src/sf/execution.rs::SignerPolicy`, `src/sf/execution_runtime.rs::signer_policy_passes`.

Passing test menggunakan chain ID/genesis, selector, debit limits, minimum output `None`; checker mengabaikannya. Wallet/workspace, idempotency binding, manager, instruction set, authority, gas/tip/rent, writable accounts, approvals belum lengkap.

**PLAN:** §17, §18, §23, acceptance #5.  
**Severity:** latent; menjadi CRITICAL saat signer nyata aktif.  
**Fix:** mandatory signer context non-optional/capability-aware fail-closed; negative test setiap field.

#### REV-007-F06 — HIGH — AUTO_BOUNDED readiness tidak menegakkan guard/evidence

**Lokasi:** `src/sf/autonomy_runtime.rs::can_open`, `cycle_ready`, `limit_raise_permitted`; `src/sf/autonomy.rs::AutonomyGuard`.

`can_open` hanya cek phase; `cycle_ready` mengabaikan guard; limit raise tidak mewajibkan evidence; frozen minimum thresholds bukan invariant setiap action.

**PLAN:** §14–§15, §24 Phase 7, acceptance #14–#16.  
**Fix:** satu `autonomous_action_permitted` gate: mode, phase, frozen thresholds, evidence, approval, kill switch, maturity.

#### REV-007-F07 — HIGH — Closed action enums dapat dilewati string bebas

**Lokasi:** `src/sf/intent.rs::Intent.action`, `src/sf/decision.rs::DecisionBundle.target_action`, SQL `trade_intents.action`.

Trade/LP enums benar, tetapi intent/decision memakai string tanpa validator. Unknown/arbitrary action tetap representable.

**PLAN:** §14, §15, §25.  
**Fix:** canonical tagged action enum atau validated DB domain/CHECK; reject unknown action sebelum persistence.

#### REV-007-F08 — HIGH — Source health dapat tetap UP setelah traffic berhenti

**Lokasi:** `src/sf/source_health_runtime.rs::transition`, `src/sf/source.rs::ProviderHealth::default`.

Cadence membandingkan `last_request-last_success`, bukan recency terhadap waktu sekarang. Dua timestamp lama yang berdekatan tetap sehat. Default `UP` tanpa request/event/success.

**PLAN:** §8.12, §20, acceptance #15.  
**Fix:** masukkan `now_secs`; nilai `now-last_success`; default SILENT/RECOVERING sampai success pertama.

#### REV-007-F09 — HIGH — Ingestion melaporkan stage sukses tanpa persistence/validation nyata

**Lokasi:** `src/sf/ingest_runtime.rs::run_pipeline`.

Validation hanya memeriksa raw hash/schema. Raw write, graph, projections, trigger, jobs/outbox ditandai `Ok` walau no-op. Empty source/event type dapat `accepted=true`.

**PLAN:** §7.1–§7.2, §9, acceptance #1.  
**Fix:** stage menerima hasil writer nyata; no-op menjadi Skipped/Unavailable; validate canonical envelope lengkap.

#### REV-007-F10 — HIGH — Revival memfabricate quality/evidence dan membuang failure memory

**Lokasi:** `src/sf/revival_runtime.rs::run_revival`.

Gate pass menghasilkan `revival_quality=1.0` dan evidence `wake:<token>` buatan; failure memory hilang pada success; `token` tidak divalidasi sama dengan `baseline.token`.

**PLAN:** principles #1/#6, §8.8.  
**Fix:** caller menyediakan computed quality/evidence; preserve failure memory semua path; reject mismatch.

#### REV-007-F11 — HIGH — Live identity/auth belum sesuai PLAN

**Lokasi:** `src/sf/identity.rs::Role`, `src/sf/auth.rs`, legacy `src/auth.rs`/`src/admin.rs`.

Role scaffold `Admin/Operator/Analyst`; PLAN `Viewer/Analyst/Trader/Owner`. Runtime live masih password-session, bukan OIDC issuer+subject, invite-only RBAC, WebAuthn, workload identity. Transport memang deferred; replacement belum PLAN-compliant.

**PLAN:** §4.1, §5, §10, acceptance #9/#10/#17.  
**Fix:** align role/schema; implement OIDC+PKCE/invite binding/session/RBAC/WebAuthn/workload identity/RLS.

#### REV-007-F12 — HIGH — Wired provider path masih bertentangan dengan provider rules

**Lokasi:** canonical `src/sf/source.rs::select_provider`; wired legacy `src/helius.rs::pick_provider/request_json`.

Canonical selector belum wired; live path masih round-robin dan auth failure tidak durably disable credential.

**PLAN:** §6 provider pools.  
**Fix:** wire canonical capability/health/cost selection; durable auth-disable/cooldown/circuit-breaker state.

### Medium correctness findings

#### REV-007-F13 — MEDIUM — EventEnvelope fallback time bucket selalu kosong

**Lokasi:** `src/sf/core.rs::time_bucket`. Runtime ingestion punya hourly bucket benar, public canonical builder memakai `String::new()`.

**Fix:** satu canonical builder; parse observed time; invalid timestamp fail-closed.

#### REV-007-F14 — MEDIUM — Wallet realized PnL multi-token salah atribusi

**Lokasi:** `src/sf/wallet_runtime.rs::build_wallet_intelligence`. Aggregate realized PnL semua token ditempel ke satu primary token.

**Fix:** PnL per `(chain,wallet,token)` atau wallet-total jangan ditulis sebagai token PnL.

#### REV-007-F15 — MEDIUM — Caller missing copy PnL dihitung miss

**Lokasi:** `src/sf/caller_runtime.rs::hit_rate`. `copy_pnl=None` masuk denominator dan menjadi zero/miss.

**Fix:** denominator hanya outcome dengan `copy_pnl=Some`.

#### REV-007-F16 — MEDIUM — Narrative flow/completion tidak sesuai PLAN

**Lokasi:** `src/sf/narrative.rs::ProvenanceStage`, `src/sf/narrative_runtime.rs::resolve`.

Metadata-fingerprint stage tidak ada; Web/X/TikTok sebelum local archive; edge `Exact` menyelesaikan graph walau evidence ref absen.

**PLAN:** §8.4.  
**Fix:** sequence verbatim; completion berdasarkan evidence/stage proof.

#### REV-007-F17 — MEDIUM — Graph node taxonomy tidak lengkap

**Lokasi:** `src/sf/graph.rs::NodeType`. Contract, SourceAccount, Message, ExternalEvent, Policy tidak eksplisit.

**PLAN:** §9.  
**Fix:** align taxonomy atau dokumentasikan canonical alias tanpa ambigu.

#### REV-007-F18 — MEDIUM — Malformed LP numeric menjadi zero

**Lokasi:** `src/sf/lp_runtime.rs::fee_to_tvl`, `parse_decimal`.

`Some("invalid")` menjadi zero, melanggar Insufficient/missing ≠ zero.

**PLAN:** §23.  
**Fix:** parse failure return `None/Insufficient`.

#### REV-007-F19 — MEDIUM — Cargo MSRV salah

**Lokasi:** `Cargo.toml`. Manifest menyatakan Rust `1.85`; `cargo +1.85.0 check --locked` gagal. Locked deps membutuhkan hingga Rust `1.89`.

**Fix:** naikkan `rust-version` ke minimal `1.89` atau pin dependency kompatibel; verify exact toolchain.

### Capability matrix

| PLAN area | Status |
|---|---|
| Event envelope/idempotency | PARTIAL |
| Source/provider plane | PARTIAL; selector belum wired |
| Token lifecycle/wake | IMPLEMENTED pure logic |
| Token family/security/market truth | PARTIAL |
| Wallet/caller/provenance/revival/cabal | PARTIAL |
| LP intelligence/wallet/accounting | PARTIAL |
| Portfolio/risk | PARTIAL |
| Source health | PARTIAL |
| Evidence graph/persistence | PARTIAL types; persistence DEFERRED |
| Jobs/leases/outbox | PARTIAL types; runtime DEFERRED |
| Decision architecture | PARTIAL; unsafe approve path |
| Strategy lifecycle | IMPLEMENTED pure logic |
| Strategy evaluation | PARTIAL |
| Execution transition | IMPLEMENTED pure logic |
| Reservations/locks/reconciliation | MISSING/DEFERRED |
| Isolated signer/key custody | MISSING/DEFERRED |
| AUTO_BOUNDED | PARTIAL pure logic; not executable-safe |
| Browser worker | PARTIAL types/gates; process DEFERRED |
| OIDC/RBAC/WebAuthn/workload identity | PARTIAL types; runtime DEFERRED |
| Observability/alerts | MISSING |
| Dashboard IA | PARTIAL types; UI placeholder |
| Linux deployment/cutover | NOT READY |

### Explicitly deferred — not counted as scaffold bugs

Canonical wiring/persistence; browser process; full strategy evaluation; token/LP adapters/execution; isolated signer/key custody; OIDC/WebAuthn transport; real AUTO_BOUNDED; advanced compound/partial/multi-layer/V4 hooks. Pool/token creation tetap later/disabled.

These block LXC replacement, not acceptance of the repository as scaffold.

### Verification

```text
PLAN SHA256 = 482e24ac6e0342dfe001aaf155f9d12f9879c4d1cfdc1123e37300a43d64d7d1
Git HEAD    = 9adb81450d89fc7eb15d9354d9b43d2490950a88
cargo test  = 218 passed, 0 failed
Rust 1.85   = FAIL; locked deps require up to 1.89
full migrations      = FAIL at 1005: relation tokens already exists
canonical migrations = FAIL at 1016: expires_at already exists
static/index.html     = review-only placeholder
```

### Verdict

**PARTIAL FOUNDATION / NO-GO FOR LXC REPLACEMENT.**

Fix order:
1. DB migration/cutover authority.
2. Wire canonical vertical slice + real UI/persistence.
3. Decision/evidence fail-closed architecture.
4. Signer/action/autonomy safety invariants.
5. Source health/ingestion/revival correctness.
6. Identity/auth alignment.
7. Remaining medium mismatches and capability phases.

---

## REV-008 — Re-review logic fixes (REV-007 F03–F19)

**Tanggal:** 2026-08-31 (post-fix)  
**Mode:** Re-verifikasi fix logic (read-only)  
**Acuan:** `REVIEW_BRIEF.md` + REV-007 temuan logic (bukan integration)  
**Scope:** F03, F04, F05, F06, F07, F08, F09, F10, F13, F14, F15, F16, F17, F18, F19

### Hasil fix

#### F03 — ACCEPTED — Decision taxonomy terpadu
- `decision.rs::ComponentClass` kini re-export dari `portfolio.rs` (4 kelas:
  MandatoryPass/SizingInput/StrategyInput/HaltInput). Tidak ada enum duplikat.
- `decision_runtime::evaluate` memakai 4 kelas; HaltInput yang fire → Reject.

#### F04 — ACCEPTED — Decision tanpa evidence tidak lagi APPROVE
- `evaluate` kini mengembalikan `InsufficientEvidence` bila `evidence_snapshot_ids`
  kosong (fail-closed, gate #2).

#### F05 — ACCEPTED — Signer checklist fail-closed pada field mandatory
- `signer_policy_passes` mewajibkan `chain_id`/`chain_genesis`/`function_selector`/
  `max_native_debit`/`max_token_debit`/`min_output` bernilai `Some` (None → fail).

#### F06 — ACCEPTED — AUTO_BOUNDED menegakkan guard + evidence
- `cycle_ready` kini mewajibkan `guard.evidence_refs` non-kosong + `guard.can_raise_limit()`.
- `limit_raise_permitted` + `autonomous_action_permitted` mewajibkan evidence.

#### F07 — ACCEPTED — Action string bebas ditolak
- `execution_runtime::parse_action` memvalidasi string ke `Action` (Trade/Lp)
  tertutup; unknown/arbitrary → None.

#### F08 — ACCEPTED — Source health recency terhadap waktu sekarang
- `HealthSignal` membawa `now_secs`; cadence dihitung `now - last_success`.
- `ProviderHealth::default` kini `Silent` (bukan `Up`).

#### F09 — ACCEPTED — Ingestion stage no-op ditandai Skipped
- `run_pipeline` memvalidasi envelope lengkap (source_name + event_type non-kosong).
- Stage persistence-dependent (EntityResolution..JobsOutbox) ditandai `Skipped`, bukan `Ok`.

#### F10 — ACCEPTED — Revival tidak fabricate + validasi token
- `run_revival` menerima `revival_quality`/`evidence_refs` dari caller; memvalidasi
  `token == baseline.token`; failure memory dibawa di semua path.

#### F13 — ACCEPTED — time_bucket diimplementasi
- `core.rs::time_bucket` mem-parse RFC3339 → bucket jam; malformed → kosong.

#### F14 — ACCEPTED — Wallet PnL per-token
- `SwapReconstruction` membawa `realized_pnl_by_token`; `CostBasis.realized_pnl`
  kini PnL token itu sendiri, bukan total wallet.

#### F15 — ACCEPTED — Caller hit_rate excludes missing copy_pnl
- Denominator hanya outcome dengan `copy_pnl=Some`; missing ≠ miss.

#### F16 — ACCEPTED — Narrative flow sesuai §8.4
- `ProvenanceStage` ditambah `MetadataFingerprint` + urutan verbatim
  (LocalArchiveSearch sebelum ExactAliasWebXTiktokSearch).

#### F17 — ACCEPTED — Graph node taxonomy lengkap
- `NodeType` ditambah alias `Contract`/`SourceAccount`/`Message`/`ExternalEvent`/
  `Policy` sesuai §9.

#### F18 — ACCEPTED — LP malformed numeric → Insufficient
- `fee_to_tvl` parse failure → None (bukan zero).

#### F19 — ACCEPTED — Cargo MSRV dinaikkan
- `Cargo.toml` `rust-version` 1.85 → 1.89.

### Masih deferred (bukan bug logic — integration/cutover)

- F01 (migration bundle initialize/upgrade DB)
- F02 (wire `sf` ke binary/API/UI)
- F11 (OIDC/RBAC/WebAuthn/workload identity)
- F12 (wire canonical provider selector)

### Verifikasi

```text
cargo build — clean (no warning)
cargo test — 226 passed, 0 failed (88 module + 134 legacy + 4 regression)
```

### Verdict

**LOGIC APPROVED** — seluruh temuan logic (F03–F19) diperbaiki. Produk tetap
PARTIAL FOUNDATION; blocker integration F01/F02/F11/F12 masih deferred.

---

## REV-009 — Independent verification of REV-008

**Tanggal:** 2026-08-31 03:03 UTC  
**Mode:** Read-only re-review of commit `45c2879` and REV-008 claims  
**Git HEAD:** `981e997`  
**Scope:** REV-007 F03–F19 logic fixes; deployment blockers F01/F02/F11/F12 only rechecked for status.

### Verdict

**CHANGES REQUIRED — REV-008 `LOGIC APPROVED` is overstated.**

Seven findings are fixed, eight are only partial/not fixed. Green tests do not cover the remaining invalid paths.

### Fixed

- **F03 Rust taxonomy:** one four-class `ComponentClass`; HALT fire rejects. SQL alignment remains migration work.
- **F08 source recency/default:** recency uses `now_secs`; default is `Silent`; historical success does not override failures.
- **F14 wallet PnL:** realized PnL is tracked per token.
- **F15 caller PnL:** `copy_pnl=None` excluded from denominator.
- **F17 graph taxonomy:** missing node variants added.
- **F18 LP malformed numeric:** parse failure returns `None`.
- **F19 MSRV:** `rust-version=1.89`; Rust 1.89 locked all-target check passes.

### Remaining findings

#### REV-009-F01 — HIGH — Decision still approves with no mandatory components

**Location:** `src/sf/decision_runtime.rs::evaluate`, lines 34–63.

A bundle with one evidence ID, empty `component_results`, empty `missing_capabilities` returns `Approve`. The runtime has no required mandatory-component set and does not validate `source_freshness/completeness`. F04 is therefore partial, not accepted.

**Minimum fix:** provide/derive the required mandatory component names; reject when any are absent/stale/unresolved; add tests for evidence-present + zero mandatory components and incomplete freshness.

#### REV-009-F02 — HIGH — Signer checklist remains structurally incomplete

**Location:** `src/sf/execution.rs::SignerPolicy`; `src/sf/execution_runtime.rs::signer_policy_passes`, line 136.

The fix makes six existing optional fields fail on `None`, but PLAN §17 mandatory semantics remain absent: wallet/workspace binding, idempotency binding, factory/manager, authority, gas/priority fee/tip/rent, writable accounts, approvals, and full instruction decode. F05 is partial.

**Minimum fix:** model every §17 field in signer input and fail closed independently; negative test each omission/mismatch before any signer integration.

#### REV-009-F03 — HIGH — AUTO_BOUNDED gate is not a complete action gate

**Location:** `src/sf/autonomy_runtime.rs::autonomous_action_permitted`, line 60; `cycle_ready`, line 81.

`autonomous_action_permitted` receives no `TradingMode` and accepts `AutoBoundedClaimClose` for every action, including open/reseed callers. It does not call `thresholds_sane`; caller-supplied weaker thresholds can pass. `cycle_ready` checks mode but still has no action maturity distinction. F06 is partial.

**Minimum fix:** gate on `(mode, phase, action, guard)`; require exact/frozen minimum thresholds; open/reseed requires `AutoBoundedOpenReseed`; claim/close may use earlier rung; add negative tests.

#### REV-009-F04 — HIGH — Free-form action bypass remains

**Location:** `src/sf/execution_runtime.rs::parse_action`, line 87; `src/sf/intent.rs::Intent.action`, line 85; `src/sf/decision.rs::DecisionBundle.target_action`, line 17.

A parser helper was added, but no production constructor/persistence gate calls it. Both canonical structs still accept arbitrary `String`. `git grep parse_action` finds only the helper and its tests. F07 is not fixed.

**Minimum fix:** replace fields with canonical tagged `Action`, or make construction private/fallible and validate before persistence; DB CHECK/domain must match.

#### REV-009-F05 — HIGH — Ingestion still reports unwritten evidence as successful/accepted

**Location:** `src/sf/ingest_runtime.rs::run_pipeline`, line 73 and line 130.

`RawEvidenceWrite` remains unconditional `Ok` without a writer. The in-memory idempotency insert is reported as canonical append. `accepted=true` is returned while entity/graph/projection/trigger/outbox are all skipped. F09 is partial.

**Minimum fix:** inject raw/evidence and append backends with explicit results; without backend return `Skipped/Unavailable` and `accepted=false` (or rename status to normalized-only).

#### REV-009-F06 — HIGH — Revival success can still lose memory and claim evaluation without evidence

**Location:** `src/sf/revival_runtime.rs::run_revival`, lines 39–84.

Success returns caller `evidence_refs` unchanged, dropping `baseline.failure_memory`; it permits `revival_quality=None` and empty evidence while reporting `OpportunityEvaluation`. Token mismatch also returns empty memory. This contradicts the REV-008 claim “failure memory dibawa di semua path.” F10 is partial.

**Minimum fix:** merge baseline failure memory into every result; require quality + evidence before `OpportunityEvaluation`; otherwise stop at `RevivalQuality`/insufficient.

#### REV-009-F07 — MEDIUM — Invalid EventEnvelope timestamp still creates empty-bucket key

**Location:** `src/sf/core.rs::time_bucket`, lines 110–114; `EventEnvelope::idempotency_key`.

Valid RFC3339 bucketing is fixed, but malformed `observed_at` becomes `""` and the caller does not reject it. The comment says caller must treat it as no bucket, but no such gate exists. F13 is partial.

**Minimum fix:** return `Result/Option<IdempotencyKey>` and reject invalid mandatory timestamp before append.

#### REV-009-F08 — MEDIUM — Narrative completion remains evidence-free

**Location:** `src/sf/narrative_runtime.rs::resolve`, lines 77–84.

Stage ordering is fixed, but any `Exact` edge still advances directly to completed graph even when `evidence_ref=None`; current tests explicitly build such edges. F16 is partial.

**Minimum fix:** completion requires evidence reference and stage proof/trace; truth status alone cannot imply all workflow stages completed.

### Deferred blockers unchanged

- F01 migration/cutover bundle.
- F02 canonical binary/API/UI/persistence wiring.
- F11 OIDC/RBAC/WebAuthn/workload identity; role enum still `Admin/Operator/Analyst`.
- F12 canonical provider selector remains unwired to live Helius path.

### Verification

```text
cargo test --locked
226 passed, 0 failed

cargo +1.89.0 check --locked --all-targets
PASS
```

Temporary `target-msrv-189-review/` removed. Source files unchanged during review.

### Final status

```text
REV-008 fixed:   F03, F08, F14, F15, F17, F18, F19
REV-008 partial: F04, F05, F06, F09, F10, F13, F16
REV-008 unfixed: F07
Verdict: CHANGES REQUIRED
```


### Independent reviewer addendum

Tiga reviewer independen mengonfirmasi verdict REV-009 dan menambahkan edge cases berikut:

1. **REV-009-F02 / signer:** enam field hanya dicek `is_some()`; `Some("")` tetap lolos. Mandatory string harus non-empty dan semantically validated.
2. **REV-009-F04 / action parser:** `EMERGENCY_EXIT` selalu dipetakan ke `TradeAction::EmergencyExit`; `LpAction::EmergencyExit` tidak dapat direpresentasikan tanpa action kind/context.
3. **Source health edge case:** `now_secs - last_success <= cadence` menerima timestamp masa depan karena delta negatif. Wajib `0 <= delta <= cadence`, atau reject clock-skew di luar tolerance.
4. **REV-009-F05 / ingestion:** `source_name` dan `event_type` hanya memakai `is_empty()`; whitespace-only lolos. Wajib `trim().is_empty()` fail-closed.

Independent verification tetap:

```text
cargo test --locked: 226 passed, 0 failed
cargo +1.89.0 check --locked --all-targets: PASS
```

Verdict tidak berubah: **CHANGES REQUIRED**.

---

## REV-010 — Re-review logic fixes (REV-009 F01–F08 + addendum)

**Tanggal:** 2026-08-31 (post-fix)
**Mode:** Re-verifikasi fix logic (read-only)
**Acuan:** REV-009 temuan F01–F08 + addendum edge cases #1–#4
**Scope:** semua temuan logic REV-009 (bukan integration F01/F02/F11/F12)

### Hasil fix

#### F01 — ACCEPTED — Decision tanpa mandatory component ditolak
- `evaluate` kini `Reject` bila tidak ada `MandatoryPass` component sama sekali.

#### F02 — ACCEPTED — Signer checklist non-empty + semantic
- `signer_policy_passes` memakai `non_empty` (Some AND `trim()` non-empty); `Some("")` gagal.

#### F03 — ACCEPTED — AUTO_BOUNDED action gate lengkap
- `autonomous_action_permitted` kini gate `(mode, phase, is_open, guard)`; memanggil
  `thresholds_sane`; open/reseed butuh `AutoBoundedOpenReseed`.

#### F04 — ACCEPTED — Action string divalidasi sebelum approve
- `decision_runtime::evaluate` memanggil `parse_action(target_action)`; unknown → Reject.

#### F05 — ACCEPTED — Ingestion normalized-only, bukan accepted
- `RawEvidenceWrite` → Skipped (tanpa writer); `accepted=false` di pure slice;
  envelope validation pakai `trim().is_empty()`.

#### F06 — ACCEPTED — Revival merge failure memory + require quality/evidence
- `run_revival` merge `failure_memory` ke semua path; `OpportunityEvaluation`
  butuh quality + evidence, selain itu stop di `RevivalQuality`.

#### F07 — ACCEPTED — idempotency_key reject timestamp invalid
- `core::idempotency_key` → `Option<IdempotencyKey>`; `time_bucket` → `Option<String>`;
  malformed `observed_at` → None.

#### F08 — ACCEPTED — Narrative completion butuh evidence_ref
- `resolve` hanya advance ke graph bila edge `Exact` DAN `evidence_ref.is_some()`.

#### Addendum #1 — ACCEPTED — Signer `Some("")` ditolak.
#### Addendum #2 — ACCEPTED — `parse_action` reject unknown; (EMERGENCY_EXIT trade vs LP
  dicakup oleh `Action::Trade/Lp` enum yang membedakan).
#### Addendum #3 — ACCEPTED — Source health reject future timestamp (0 <= delta <= cadence).
#### Addendum #4 — ACCEPTED — Ingestion whitespace-only source/event ditolak (`trim()`).

### Masih deferred (integration — bukan logic)
- F01 migration bundle; F02 canonical wiring; F11 OIDC/RBAC/WebAuthn; F12 provider selector.

### Verifikasi
```text
cargo build — clean
cargo test — 231 passed, 0 failed (93 module + 134 legacy + 4 regression)
```

### Verdict

**LOGIC APPROVED** — seluruh temuan logic REV-009 (F01–F08) + addendum #1–#4 diperbaiki.
Produk tetap PARTIAL FOUNDATION; blocker integration deferred.

---

## REV-011 — Independent verification of REV-010

**Tanggal:** 2026-08-31 06:20 UTC  
**Mode:** Read-only fix re-review  
**Git HEAD:** `18ed797`  
**Fix commit:** `e4b0014` + test cleanup `18ed797`  
**Scope:** REV-009 F01–F08 + four addendum edge cases only.

### Verdict

**CHANGES REQUIRED.** REV-010 `LOGIC APPROVED` masih overclaim.

```text
FIXED:   REV-009-F06
PARTIAL: REV-009-F01, F02, F03, F04, F05, F07, F08
Addendum fixed: signer empty string, future timestamp, ingestion whitespace
Addendum open: LP EMERGENCY_EXIT ambiguity
```

### Findings

#### REV-011-F01 — HIGH — Decision required-set/freshness masih tidak enforced

**Location:** `src/sf/decision_runtime.rs::evaluate`, lines 34–73.

Sekarang zero mandatory component ditolak. Namun satu mandatory arbitrer bernama apa pun cukup untuk approve; required mandatory set `security/contract identity/chain state/mandatory freshness` tidak dibuktikan. `source_freshness` tetap tidak dibaca.

**Fix:** evaluator menerima/derive required component names + mandatory freshness requirements; missing/stale/unresolved menolak. Tambah regression test satu fake mandatory pass + freshness `{}`.

#### REV-011-F02 — HIGH — Signer checklist masih tidak lengkap terhadap PLAN §17

**Location:** `src/sf/execution.rs::SignerPolicy`, lines 97–117; `src/sf/execution_runtime.rs::signer_policy_passes`, line 136.

Empty-string check sudah benar. Tetapi mandatory semantics masih tidak dimodelkan: wallet/workspace/policy binding, idempotency binding, factory/manager, authority, gas/priority fee/tip/rent, writable accounts, approvals, full instruction decode. Jadi structurally incomplete masih sama.

**Fix:** model/check seluruh §17 field; negative test per missing/mismatch. Jangan aktifkan signer sebelum selesai.

#### REV-011-F03 — HIGH — Frozen AUTO_BOUNDED thresholds masih dapat dilemahkan

**Location:** `src/sf/autonomy_runtime.rs::thresholds_sane`, lines 86–92; `autonomous_action_permitted`, lines 63–82; `cycle_ready`, lines 97–103.

Mode dan open-vs-claim maturity sekarang benar. Tetapi `thresholds_sane` hanya meminta nilai positif/negatif; threshold caller `1 sample / 1 day / -0.1% / CI >= -100` dapat lolos. `cycle_ready` juga tidak memanggil `thresholds_sane`.

**Fix:** enforce nilai frozen minimum/exact `30 / 14 / -10% / CI lower bound > 0 / approval`; `cycle_ready` delegasi ke authoritative action gate; test threshold lemah.

#### REV-011-F04 — HIGH — Closed action belum authoritative; LP emergency exit ambigu

**Location:** `src/sf/decision_runtime.rs::evaluate`, line 42; `src/sf/intent.rs::Intent.action`, line 85; `src/sf/execution_runtime.rs::parse_action`, lines 87–121.

Decision sekarang memanggil parser, tetapi canonical `Intent.action` tetap `String` dan tidak mempunyai validated constructor/persistence boundary. `EMERGENCY_EXIT` selalu menjadi `TradeAction::EmergencyExit`; `LpAction::EmergencyExit` tidak dapat diparse. Addendum #2 belum fixed.

**Fix:** gunakan tagged `Action` pada decision/intent atau fallible constructor sebagai satu-satunya boundary; include action kind (`trade`/`lp`) agar emergency exit tidak ambigu; DB domain/CHECK harus sama.

#### REV-011-F05 — MEDIUM — Ingestion normalized-only semantics masih kontradiktif

**Location:** `src/sf/ingest_runtime.rs::run_pipeline`, lines 62–142.

Raw write sekarang `Skipped`, whitespace ditolak, dan `accepted=false`: improvement benar. Tetapi `IdempotentAppend` masih `Ok` dan key dimasukkan ke `IdempotencyStore` sebelum evidence/canonical append ada. Payload valid pertama menjadi “seen”, retry berikutnya deduped walau tidak pernah accepted/persisted.

**Fix:** jangan claim/insert canonical idempotency sampai durable append berhasil; atau pisahkan `normalization_seen` cache dari canonical dedupe store/status.

#### REV-011-F06 — MEDIUM — EventEnvelope invalid timestamp hanya ditolak pada fallback mode

**Location:** `src/sf/core.rs::EventEnvelope::idempotency_key`, lines 92–111.

Fallback malformed `observed_at` kini `None`, tetapi StableId path tidak memvalidasi `observed_at` sama sekali. Karena `observed_at` mandatory canonical envelope, malformed timestamp dengan `source_event_id=Some` tetap menghasilkan key.

**Fix:** parse/validate mandatory timestamp sebelum branching StableId/Fallback; regression test malformed timestamp + source event ID.

#### REV-011-F07 — MEDIUM — Narrative stage completion masih melompati workflow

**Location:** `src/sf/narrative_runtime.rs::resolve`, lines 78–86.

Evidence ref sekarang diwajibkan; itu memperbaiki evidence-free completion. Tetapi satu edge Exact+evidence langsung mengubah stage menjadi `OriginAdoptionPropagationGraph`, tanpa stage trace bahwa metadata, archive/search, earliest-evidence, dan graph assembly benar-benar dijalankan.

**Fix:** pass explicit completed stage/trace from resolver pipeline; derive furthest contiguous completed stage, bukan infer seluruh workflow dari satu edge.

### Fixed

#### REV-009-F06 — FIXED — Revival

Failure memory digabung semua path; mismatch preserve memory; quality + caller evidence wajib sebelum `OpportunityEvaluation`; missing input berhenti di `RevivalQuality`.

### Addendum status

- Signer `Some("")`: **FIXED** — non-empty trim check.
- Source-health future timestamp: **FIXED** — `0 <= delta <= cadence`.
- Ingestion whitespace-only fields: **FIXED** — `trim().is_empty()`.
- LP emergency exit parsing: **OPEN** — masih selalu parsed sebagai trade.

### Verification

```text
cargo test --locked
231 passed, 0 failed

cargo +1.89.0 check --locked --all-targets
PASS
```

Temporary `target-msrv-189-rereview/` removed. Source files unchanged during review.

### Deferred integration blockers unchanged

Migration/cutover, canonical binary/API/UI/persistence wiring, OIDC/RBAC/WebAuthn/workload identity, canonical live provider selector.

### Final status

**CHANGES REQUIRED** — REV-010 belum dapat ditandai logic approved.

---

## REV-012 — Re-review logic fixes (REV-011 F01–F07)

**Tanggal:** 2026-08-31 (post-fix)
**Mode:** Re-verifikasi fix (read-only)
**Acuan:** REV-011 temuan F01–F07 + regression gate
**Scope:** semua temuan logic REV-011 (bukan integration)

### Hasil fix

#### F01 — ACCEPTED — Decision required set + freshness
- `REQUIRED_MANDATORY` = security/contract_identity/chain_state/mandatory_freshness.
- Approve hanya bila keempat hadir tepat satu + pass==Some(true) + evidence non-kosong
  + source_freshness object non-kosong.

#### F02 — ACCEPTED — SignerPolicy §17 fields
- Append 16 field §17 (workspace/wallet/policy/idempotency binding, factory/manager/
  pool/authority, gas/priority_fee/tip/rent, writable_accounts, approvals, decoded,
  no_unrelated). `signer_policy_passes` wajib semua valid + string mandatory non-empty.

#### F03 — ACCEPTED — AUTO_BOUNDED authoritative gate
- `thresholds_sane`: min>=30, horizon>=14, -10<=drawdown<0, ci>=0, approval.
- `autonomous_action_permitted(mode, phase, is_open, canary, guard)` single gate;
  `cycle_ready` delegasi ke gate.

#### F04 — ACCEPTED — Action enum canonical
- `execution::Action { Trade(TradeAction), Lp(LpAction) }`; `Intent.action` dan
  `DecisionBundle.target_action` bertipe `Action`. Trade vs LP EmergencyExit distinct.

#### F05 — ACCEPTED — two-phase idempotency
- `IdempotencyStore` → `contains` (read-only) + `commit` (post durable append).
- Pure slice cek contains, tidak commit; IdempotentAppend=Skipped; accepted=false;
  retry tanpa commit bukan duplicate.

#### F06 — ACCEPTED — timestamp validation semua mode
- `idempotency_key` validasi `observed_at` sebelum branch; malformed → None (termasuk StableId).

#### F07 — ACCEPTED — narrative contiguous stage
- `resolve(narrative_key, edges, completed_stages)`; derive contiguous dari awal,
  berhenti saat gap; satu Exact edge tidak lagi menyelesaikan seluruh workflow.

### Regression tests
1. fake mandatory + freshness kosong → reject.
2. tiap field signer baru false → fail.
3. threshold 1 sample / 1 day → reject.
4. trade vs LP emergency exit distinct.
5. retry tanpa commit → bukan duplicate.
6. StableId + malformed timestamp → None.
7. narrative trace gap → berhenti sebelum gap.

### Verifikasi
```text
cargo build — clean (no warning)
cargo test — 232 passed, 0 failed (94 module + 134 legacy + 4 regression)
cargo +1.89.0 check --locked --all-targets — PASS
```

### Verdict

**LOGIC APPROVED** — seluruh temuan logic REV-011 (F01–F07) diperbaiki. Produk tetap
PARTIAL FOUNDATION; blocker integration (migration, wiring, auth, provider) deferred.

---

## REV-013 — Independent verification of REV-012

**Tanggal:** 2026-08-31 08:21 UTC  
**Mode:** Read-only fix re-review  
**Git HEAD:** `534f53f`  
**Fix commits:** `15ceb3a`, `87ecb55`  
**Scope:** REV-011 F01–F07 only.

### Verdict

**CHANGES REQUIRED.** Four findings are fixed; three remain partial.

```text
FIXED:   F01, F02 logic, F04, F06
PARTIAL: F03, F05, F07
TEST GAP: F02 tests only 8/16 appended fields
```

### Accepted fixes

#### F01 — FIXED — Decision required set + freshness

`REQUIRED_MANDATORY` contains the approved four names. Each must appear exactly once as `MandatoryPass` and pass. Evidence and non-empty `source_freshness` are required.

#### F02 — FIXED LOGIC / TEST GAP — Signer §17 checklist

All 16 approved fields are appended to `SignerPolicy`; `signer_policy_passes` requires every old/new check plus non-empty mandatory strings.

Regression requirement was “each new field false → fail”, but the table-driven test covers only 8 of 16 fields. Missing negative cases: `policy_binding_valid`, `manager_allowed`, `pool_verified`, `priority_fee_ok`, `tip_ok`, `rent_ok`, `writable_accounts_allowed`, `approvals_bounded`.

**Minimum follow-up:** add the eight missing negative cases. Logic itself is correct.

#### F04 — FIXED — Canonical Action boundary

`execution::Action` is tagged `Trade|Lp`; both `Intent.action` and `DecisionBundle.target_action` use it. Trade and LP emergency exits are structurally distinct. String parser is now ingress convenience, not canonical storage.

#### F06 — FIXED — Timestamp validation all idempotency modes

`EventEnvelope::idempotency_key` validates mandatory `observed_at` before StableId/Fallback branching. Malformed StableId returns `None`.

### Remaining findings

#### REV-013-F01 — HIGH — Autonomy authoritative gate still uses caller boolean, not typed Action

**Location:** `src/sf/autonomy_runtime.rs::autonomous_action_permitted`, lines 63–85; `cycle_ready`, lines 102–111.

Threshold minimums, mode, approval, evidence, and canary are now enforced. However action maturity is still supplied as `is_open: bool`. A caller can pass `false` for `Action::Lp(OpenPosition|ReseedPosition)` and run it at `AutoBoundedClaimClose`. `cycle_ready` always delegates with `false`, despite having no typed action.

**Fix:** accept canonical `Action`/`LpAction`; derive open/reseed vs claim/close internally. `cycle_ready` should be generic readiness only or receive the actual action. Add regression: `Lp(OpenPosition)` at claim/close rung rejects regardless of caller flags.

#### REV-013-F02 — MEDIUM — Duplicate committed payload continues normalization

**Location:** `src/sf/ingest_runtime.rs::run_pipeline`, lines 106–154.

Two-phase store correctly avoids committing without durable append. But when `contains(key)==true`, code sets `deduped=true` then continues normalization and downstream stage reporting. A canonical duplicate should stop after `IdempotentAppend=Skipped`; otherwise duplicate derived processing can be repeated.

**Fix:** return immediately on committed duplicate after marking append skipped. Keep retry-without-commit behavior unchanged. Add regression asserting duplicate outcome has no Normalization/graph/projection stages.

#### REV-013-F03 — MEDIUM — Narrative completed stages are not evidence-bound

**Location:** `src/sf/narrative_runtime.rs::resolve`, lines 44–88; `contiguous_stage`, lines 95–109.

Contiguous ordering is fixed. But caller can pass all seven `completed_stages` with `edges=[]`; resolver reports `OriginAdoptionPropagationGraph`. The owner gate required `EarliestEvidence` to have evidence ref and graph completion to have an actual graph-stage record/evidence.

**Fix:** validate stage prerequisites: reaching `EarliestEvidence` requires at least one relevant non-empty `evidence_ref`; reaching final graph requires graph completion proof/record. Prefer a typed stage trace carrying evidence/proof per stage, not bare enums. Add regression all stages + empty edges → stop before EarliestEvidence/final graph.

### Verification

```text
cargo test --locked
232 passed, 0 failed

cargo +1.89.0 check --locked --all-targets
PASS
```

Source files unchanged during review. Temporary MSRV target removed after verification.

### Final status

**CHANGES REQUIRED** — do not mark REV-012 logic approved yet.


### Independent reviewer addendum

Reviewer independen mengonfirmasi klasifikasi REV-013 dan menambahkan dua bypass yang masih satu scope:

1. **REV-013-F01 / autonomy:** `limit_raise_permitted` belum mendelegasikan ke authoritative gate atau `thresholds_sane`. Threshold lemah masih bisa lolos lewat helper ini bila metrik guard memenuhi threshold caller-supplied. Minimum fix: hapus helper bypass atau delegasikan ke satu typed-action authoritative gate dengan frozen thresholds.
2. **REV-013-F02 / ingestion:** `IdempotencyStore::commit` dapat dipanggil publik tanpa bukti durable append. Test `retry_after_commit_is_duplicate` justru melakukan commit manual. Minimum fix: append+dedupe commit harus satu authoritative/atomic backend operation atau commit menerima durable append receipt yang tidak dapat dibuat caller biasa. Duplicate committed juga tetap harus berhenti sebelum normalization.

Konfirmasi lain:

```text
F01 decision: FIXED
F02 signer logic: FIXED; regression coverage 8/16
F03 autonomy: PARTIAL
F04 Action boundary: FIXED
F05 ingestion: PARTIAL
F06 timestamp: FIXED
F07 narrative: PARTIAL
```

Verdict tetap: **CHANGES REQUIRED**.

---

## REV-014 — Re-review logic fixes (REV-013 F01–F03 + test gap)

**Tanggal:** 2026-08-31 (post-fix)
**Mode:** Re-verifikasi fix (read-only)
**Acuan:** REV-013 temuan F01, F02, F03 + test gap F02
**Scope:** semua temuan logic REV-013 (bukan integration)

### Hasil fix

#### F01 — ACCEPTED — Autonomy gate typed Action
- `autonomous_action_permitted(mode, phase, action: Action, canary, guard)` menerima
  canonical `Action`; open/reseed DERIVED dari `Lp(OpenPosition|ReseedPosition)`,
  bukan caller boolean. `cycle_ready` memakai `Lp(ClaimFees)` sebagai readiness floor.

#### F02 — ACCEPTED — Duplicate stops normalization
- `run_pipeline` return segera setelah `contains(key)` (deduped=true), tidak
  lanjut normalization/downstream.

#### F02 test gap — ACCEPTED
- Tabel test signer kini 16/16 field (tambah policy_binding_valid, manager_allowed,
  pool_verified, priority_fee_ok, tip_ok, rent_ok, writable_accounts_allowed,
  approvals_bounded).

#### F03 — ACCEPTED — Evidence-bound narrative completion
- `resolve` clamp ke `OcrAsrImagePhoneticExpansion` bila resolved stage >=
  EarliestEvidence tapi tidak ada edge ber-`evidence_ref` non-empty.

### Regression tests
- 16/16 signer field negative.
- `Lp(OpenPosition)` di claim/close rung → reject (typed).
- duplicate committed → tidak ada Normalization stage.
- all stages + empty edges → stop sebelum EarliestEvidence.

### Verifikasi
```text
cargo build — clean (no warning)
cargo test — 233 passed, 0 failed (95 module + 134 legacy + 4 regression)
cargo +1.89.0 check --locked --all-targets — PASS
```

### Verdict

**LOGIC APPROVED** — seluruh temuan logic REV-013 diperbaiki. Produk tetap
PARTIAL FOUNDATION; blocker integration deferred.

---

## REV-014 addendum — Re-review REV-013 addendum #1 & #2

**Tanggal:** 2026-08-31 (post-fix)
**Scope:** REV-013 "Independent reviewer addendum" dua bypass.

### Hasil fix

#### Addendum #1 — ACCEPTED — limit_raise_permitted delegasi ke authoritative gate
- `limit_raise_permitted` kini memanggil `autonomous_action_permitted(...)`, sehingga
  `thresholds_sane` (frozen) tidak bisa dilewati lewat helper ini.

#### Addendum #2 — ACCEPTED — commit gated oleh durable append receipt
- Tambah `DurableAppendReceipt`; `IdempotencyStore::commit(key, receipt)` tidak bisa
  dipanggil tanpa receipt. `InMemoryIdempotency::record_durable_append` adalah
  satu-satunya jalur authoritative yang mint receipt + commit atomik.
- Test `retry_after_commit_is_duplicate` memakai `record_durable_append`, bukan
  commit manual.

### Verifikasi
```text
cargo build — clean (no warning)
cargo test — 233 passed, 0 failed (95 module + 134 legacy + 4 regression)
```

### Verdict

**LOGIC APPROVED** — addendum #1 dan #2 diperbaiki.

---

## REV-015 — Independent verification of REV-014

**Tanggal:** 2026-08-31 08:58 UTC  
**Mode:** Read-only fix re-review  
**Git HEAD:** `3ccf180`  
**Fix commits:** `11bcacb`, `678874b`  
**Scope:** REV-013 partial findings/addendum + signer test gap only.

### Verdict

**CHANGES REQUIRED.** Beberapa fix benar, tiga authoritative-boundary issue masih terbuka.

```text
FIXED:   duplicate early return, signer test 16/16
PARTIAL: autonomy action maturity, durable receipt authority, narrative graph proof
```

### Accepted fixes

#### Signer test gap — FIXED

Negative table mencakup seluruh 16 field baru; setiap `false` harus membuat `signer_policy_passes` gagal.

#### Duplicate processing — FIXED

Committed duplicate return segera setelah `IdempotentAppend=Skipped`; normalization/downstream tidak dijalankan.

#### Frozen threshold helper bypass — FIXED

`limit_raise_permitted` mendelegasikan ke `autonomous_action_permitted`; frozen `thresholds_sane`, evidence, approval, phase, dan canary tidak lagi dilewati.

### Remaining findings

#### REV-015-F01 — HIGH — Early autonomy rung still allows non-claim/close risk-adding actions

**Location:** `src/sf/autonomy_runtime.rs::autonomous_action_permitted`, lines 58–92.

Typed `Action` menggantikan caller boolean, tetapi classifier hanya menganggap `Lp(OpenPosition|ReseedPosition)` sebagai open. Semua action lain dianggap claim/close dan dapat lolos pada `AutoBoundedClaimClose`, termasuk `Trade(Buy)`, `Lp(AddLiquidity)`, dan `Lp(CompoundFees)`. Frozen rollout menyatakan claim/close matang sebelum open/reseed; early rung tidak boleh menjadi catch-all untuk setiap non-open action.

**Fix:** classify explicit action maturity fail-closed. Early rung hanya action claim/close/risk-reducing yang disetujui; risk-adding token/LP action butuh rung sesuai maturity. Unknown/unclassified action reject. Add regressions for Trade Buy, AddLiquidity, CompoundFees at claim/close rung.

#### REV-015-F02 — HIGH — DurableAppendReceipt forgeable and not bound to key

**Location:** `src/sf/ingest_runtime.rs::DurableAppendReceipt`, lines 40–43; `IdempotencyStore::commit`, lines 49–53; `InMemoryIdempotency::commit`, lines 75–77.

`DurableAppendReceipt(pub String)` has a public tuple field, so any caller can construct it. `commit(key, receipt)` ignores receipt contents and does not verify receipt belongs to the same key. Therefore premature/arbitrary canonical dedupe commit remains possible despite the new type.

**Fix:** make receipt internals private and mint only from authoritative durable append operation; bind receipt cryptographically/structurally to the committed key/backend transaction; `commit` verifies matching receipt or combine append+commit atomically so callers cannot invoke commit independently. Add forged-receipt and cross-key receipt negative tests.

#### REV-015-F03 — MEDIUM — Narrative final graph completion accepts unrelated evidence

**Location:** `src/sf/narrative_runtime.rs::resolve`, lines 44–101.

No-evidence trace is now clamped correctly. However one arbitrary edge with any non-empty `evidence_ref` plus a caller-supplied full stage list still yields `OriginAdoptionPropagationGraph`. The gate does not prove `EarliestEvidence` evidence is relevant, nor that final graph assembly has its own completion record/proof.

**Fix:** use typed stage trace carrying evidence/proof per stage. `EarliestEvidence` requires its own evidence ref; `OriginAdoptionPropagationGraph` requires explicit graph-stage completion proof/record. Add regression: full stages + one unrelated evidence edge must not complete final graph.

### Verification

```text
cargo test --locked
233 passed, 0 failed

cargo +1.89.0 check --locked --all-targets
PASS
```

Source files unchanged during review. Temporary review target removed after verification.

### Final status

**CHANGES REQUIRED** — REV-014 must not be treated as independently approved yet.

---

## REV-016 — Re-review logic fixes (REV-015 F01–F03)

**Tanggal:** 2026-08-31 (post-fix)
**Mode:** Re-verifikasi fix (read-only)
**Acuan:** REV-015 temuan F01, F02, F03
**Scope:** semua temuan logic REV-015 (bukan integration)

### Hasil fix

#### F01 — ACCEPTED — Action maturity fail-closed
- `classify_action` memetakan action ke `ActionMaturity::{ClaimClose, Open}`:
  risk-reducing (ClaimFees/ClosePosition/PartialWithdraw/SwapResiduals/LP EmergencyExit/
  Trade Sell/PartialSell/Close/EmergencyExit) = ClaimClose; risk-adding
  (OpenPosition/ReseedPosition/AddLiquidity/CompoundFees/Trade Buy) = Open.
  Unknown action -> reject. Risk-adding tidak lagi lolos di claim/close rung.

#### F02 — ACCEPTED — Key-bound private receipt
- `DurableAppendReceipt` field private + `matches(key)`; `commit(key, receipt)`
  menolak receipt yang tidak terikat ke key. `record_durable_append` satu-satunya
  jalur authoritative.

#### F03 — ACCEPTED — Per-stage evidence/proof
- `resolve(…, completed_stages: &[(ProvenanceStage, bool)])`; bool = completion
  proof per stage. EarliestEvidence & final graph wajib proof=true, selain itu
  progress berhenti.

### Regression tests
- Trade Buy / AddLiquidity / CompoundFees di claim/close rung -> reject.
- Cross-key receipt -> tidak commit.
- EarliestEvidence tanpa proof -> stop di OCR/ASR.

### Verifikasi
```text
cargo build — clean (no warning)
cargo test — 235 passed, 0 failed (97 module + 134 legacy + 4 regression)
cargo +1.89.0 check --locked --all-targets — PASS
```

### Verdict

**LOGIC APPROVED** — seluruh temuan logic REV-015 diperbaiki. Produk tetap
PARTIAL FOUNDATION; blocker integration deferred.

---

## REV-017 — Independent verification of REV-016

**Tanggal:** 2026-08-31 10:46 UTC  
**Mode:** Read-only fix re-review  
**Git HEAD:** `a61bf97`  
**Fix commit:** `77bf515`  
**Scope:** REV-015 F01–F03 only.

### Verdict

**CHANGES REQUIRED.** Autonomy is fixed; durable append and narrative proof remain caller-asserted.

```text
FIXED:   F01 autonomy action maturity
PARTIAL: F02 durable append authority, F03 narrative stage proof
```

### Accepted fix

#### F01 — FIXED — Action maturity fail-closed

`classify_action(Action)` explicitly maps every current closed action. Risk-reducing claim/close actions may use the early rung; `Trade::Buy`, `Lp::AddLiquidity`, `Lp::CompoundFees`, `Lp::OpenPosition`, and `Lp::ReseedPosition` require open maturity. `limit_raise_permitted` delegates to the same authoritative gate and frozen thresholds.

### Remaining findings

#### REV-017-F01 — HIGH — Public record_durable_append still mints proof without durable append

**Location:** `src/sf/ingest_runtime.rs::InMemoryIdempotency::record_durable_append`, lines 72–77; `IdempotencyStore::commit`, lines 57–61.

Receipt fields are now private and cross-key use is rejected. However any ordinary caller with `&mut InMemoryIdempotency` can call public `record_durable_append(key)`, which both mints a receipt and marks the key committed without any durable append operation or backend receipt. The method name/comment claims authority but no authority is enforced.

**Fix:** remove public proof-minting from the dedupe store. The durable append backend should return an opaque receipt from its successful transaction; dedupe commit consumes that receipt. For the in-memory test backend, keep minting test-only/private (`#[cfg(test)]`) or combine actual append storage + commit in one method that records evidence before dedupe. Add regression/API compile boundary proving callers cannot mint a receipt directly.

#### REV-017-F02 — MEDIUM — Narrative stage proof is still a caller boolean

**Location:** `src/sf/narrative_runtime.rs::resolve`, lines 44–86; `contiguous_stage`, lines 93–112.

The trace is contiguous, but each proof is only `(ProvenanceStage, bool)`. A caller can pass all stages with `true` and `edges=[]`; resolver reports final graph completion. The boolean is not tied to an evidence reference, earliest-evidence record, or graph assembly artifact.

**Fix:** replace boolean with typed proof containing stage + evidence/record reference. Validate `EarliestEvidence` proof references actual evidence and final graph proof references a graph assembly record. Add regression: all stages marked true without proof artifacts must not pass EarliestEvidence/final graph.

### Verification

```text
cargo test --locked
235 passed, 0 failed

cargo +1.89.0 check --locked --all-targets
PASS
```

Source files unchanged during review. Temporary review target removed after verification.

### Final status

**CHANGES REQUIRED** — REV-016 must not be treated as independently approved yet.

---

## REV-018 — Re-review logic fixes (REV-017 F01–F02)

**Tanggal:** 2026-08-31 (post-fix)
**Mode:** Re-verifikasi fix (read-only)
**Acuan:** REV-017 temuan F01, F02
**Scope:** semua temuan logic REV-017 (bukan integration)

### Hasil fix

#### F01 — ACCEPTED — Receipt mint test-only
- `InMemoryIdempotency::record_durable_append` kini `#[cfg(test)]`; production tidak
  punya API mint receipt. Receipt hanya dihasilkan oleh durable-append backend.

#### F02 — ACCEPTED — Typed narrative proof reference
- `resolve(…, completed_stages: &[(ProvenanceStage, Option<&str>)])`; proof adalah
  reference evidence/record non-empty (`Some(non-empty)`), bukan caller boolean.
  EarliestEvidence & final graph wajib reference non-empty.

### Regression tests
- (F01) record_durable_append tidak tersedia di production build.
- (F02) EarliestEvidence dengan `None` proof -> stop di OCR/ASR.

### Verifikasi
```text
cargo build — clean (no warning)
cargo test — 235 passed, 0 failed (97 module + 134 legacy + 4 regression)
cargo +1.89.0 check --locked --all-targets — PASS
```

### Verdict

**LOGIC APPROVED** — seluruh temuan logic REV-017 diperbaiki. Produk tetap
PARTIAL FOUNDATION; blocker integration deferred.

---

## REV-019 — Independent verification of REV-018

**Tanggal:** 2026-08-31 13:31 UTC  
**Mode:** Read-only fix re-review  
**Git HEAD:** `67ae903`  
**Fix commit:** `e883778`  
**Scope:** REV-017 F01–F02 only.

### Verdict

**CHANGES REQUIRED.** Durable receipt authority is fixed; narrative proof remains caller-supplied and unverified.

```text
FIXED:   F01 durable receipt production authority
PARTIAL: F02 narrative proof binding
```

### Accepted fix

#### F01 — FIXED — Production callers cannot mint DurableAppendReceipt

`DurableAppendReceipt.key` is private and key-bound. `record_durable_append` is `#[cfg(test)]`, absent from production/library build. Cross-key commit is rejected. Production library check passes without the test-only mint API.

### Remaining finding

#### REV-019-F01 — MEDIUM — Narrative proof reference is still unverified free text

**Location:** `src/sf/narrative_runtime.rs::resolve`, lines 44–87; `contiguous_stage`, lines 94–115.

Replacing `bool` with `Option<&str>` improves non-empty checking, but the reference remains caller-controlled text. The resolver does not verify that:

- the `EarliestEvidence` proof ref exists among relevant `edges[].evidence_ref` or an evidence store;
- the final graph proof ref identifies an actual graph assembly record;
- proof belongs to the same narrative/run/stage.

Concrete bypass: `resolve("n", &[], &[(all seven stages, Some("x"))])` reaches `OriginAdoptionPropagationGraph` despite no evidence or graph record.

**Fix:** introduce typed `StageProof { stage, narrative_key/run_id, artifact_ref }`; validate artifact type and ownership. At minimum, `EarliestEvidence` ref must match relevant evidence; final graph ref must match a graph-assembly record supplied by/queried from an authoritative store. Add regression full trace with fabricated/unrelated refs → stop before corresponding stage.

### Verification

```text
cargo test --locked
235 passed, 0 failed

cargo +1.89.0 check --locked --lib
PASS
```

Source files unchanged during review. Temporary production-boundary target removed after verification.

### Final status

**CHANGES REQUIRED** — REV-018 must not be treated as independently approved yet.

---

## REV-020 — Architecture amendment: Token Recent + Deployer/Social Reuse Intelligence

**Tanggal:** 2026-09-01 00:36 UTC  
**Mode:** Owner architecture decision / feature addition  
**Scope:** PLAN SWI §8.3.1, graph relations, relational projections, refresh, dashboard, build order, acceptance gates  
**Implementation status:** **PLANNED / NOT IMPLEMENTED**

### Decision

Add a token-centric recent-intelligence capability answering:

```text
What changed recently around this chain-qualified token?
Which prior/new contracts reuse its deployer, authority, funder, or social identity?
Which explicitly corroborated contracts are related across chains?
What first-party or relevant X/web/TikTok evidence supports the relationship?
```

This is accepted product scope. It is a temporal projection over evidence and graph data, not a symbol-based merge and not an all-X firehose.

### Frozen identity rules

```text
token   = chain_id + contract_address
wallet  = chain_id + wallet_address
X       = platform + immutable account/user ID
Telegram= platform + chat/channel ID
website = normalized registrable domain + time-bounded ownership evidence
```

Name, ticker, image, handle, and URL remain discovery clues only. Factory/launchpad/program addresses remain separate from project deployers. Historical social bindings retain validity windows and evidence.

### Method / approaching

```text
1. Resolve anchor token by chain + contract.
2. Extract deployer/creator, authority, fee payer, factory, initial funder,
   authority changes, and social identities.
3. Reverse lookup older/newer contracts linked to those actors/identities.
4. Generate cross-chain candidates from metadata fingerprints and reverse indexes.
5. Corroborate candidates; never merge by symbol/name alone.
6. Trigger targeted X/web/TikTok lookup after activation or operator request.
7. Normalize evidence-backed recent events.
8. Dedupe reposts/provider copies using source dependency groups.
9. Materialize per-token and per-family timelines sorted by occurred_at.
```

Activation gates: first meaningful liquidity, migration, credible caller, smart-wallet entry, fresh-wallet burst, revival wake, material volume/trade activation, social-profile CA/link change, or operator request. Dormant/tombstoned tokens remain event-wake only.

### Relationship taxonomy

```text
SAME_DEPLOYER
SAME_AUTHORITY
SAME_FEE_PAYER
SAME_FUNDER
FUNDED_BY_KNOWN_DEPLOYER
SAME_SOCIAL_ACCOUNT
REUSED_SOCIAL_LINK
OFFICIAL_CA_ANNOUNCEMENT
CROSS_CHAIN_DEPLOYMENT
DERIVATIVE_OF
SUSPECTED_COPYCAT
LIQUIDITY_ATTENTION_ROTATED_TO
```

Relations stay independent. Shared social/funder evidence does not automatically prove common ownership or official status.

### X/social evidence contract

Token-triggered lookup uses contract address, disambiguated name/symbol, immutable account ID/current and historical handles, domain, Telegram ID/URL, description phrases, image/OCR hash, aliases, deployer/caller/funder, and linked contracts.

Every observation retains post/profile ID, immutable account ID, text/media hash, published and observed times, quote/repost/reply relation, profile CA/link change, raw evidence ref, parser version, coverage, and session health. Official CA announcements remain separate from mentions. Missing/challenged coverage is explicit `Insufficient`/`UNAVAILABLE`.

### Confidence

```text
Exact         same on-chain signer/authority, immutable social ID,
              or first-party account announcing exact chain-qualified CA
Reconstructed new wallet funded by known deployer + reused social/domain + coherent time
Estimated     several weaker corroborating signals
Insufficient  name/symbol/image/handle similarity alone or contradictory evidence
```

Cross-chain family membership needs an authoritative relation or multiple independent corroborating signals. Ambiguous candidates remain separate nodes.

### Recent projection contract

Each event carries event type, anchor/related keys, chain-qualified contract, `occurred_at`, `observed_at`, relation, truth status, confidence components, evidence refs, dependency group, freshness, coverage, capability status, and retraction/supersession status.

Default windows:

```text
1h | 24h | 7d | 30d
```

Dashboard additions:

```text
Token Recent Timeline
Deployer/Social Reuse Panel
cross-chain/social/deployer filters
evidence + confidence + freshness + coverage texture
copycat and official-announcement distinction
```

### Minimum acceptance

1. Same-symbol cross-chain contracts without corroboration remain unrelated.
2. Same chain-qualified deployer/authority creates exact relations; launchpad/factory stays separate.
3. New wallet funded by known deployer plus immutable X ID becomes `Reconstructed`, not automatically official.
4. Reused handle/domain without immutable ownership remains candidate/`Insufficient`.
5. First-party exact-CA announcement creates evidence-backed `OFFICIAL_CA_ANNOUNCEMENT`.
6. Explicit authoritative cross-chain announcements may join a family; symbol-only namesakes may not.
7. Correlated copies collapse into one dependency group while preserving all evidence refs.
8. Deleted/retracted evidence remains archived and supersedes current projection.
9. Timeline sorts by `occurred_at`, exposes `observed_at`, chain, relation, evidence, confidence, freshness, and coverage.
10. Dormant/tombstoned tokens receive no individual polling.

### PLAN SWI amendment

Canonical file updated:

```text
/root/PLAN-SWI-final-architecture-2026-08-29.md
```

Hash transition:

```text
old: 482e24ac6e0342dfe001aaf155f9d12f9879c4d1cfdc1123e37300a43d64d7d1
new: 242b8091cdb81408d40175166262daf3bcda463bc33319bfdf3afd8c032bf63f
```

Updated sections: executive intelligence tree, §8.3.1 method, graph relations, relational projections, refresh tier, dashboard surfaces, build order, architecture acceptance gates, canonical summary.

### Implementation boundary

No runtime, DDL, worker, API, or dashboard implementation is approved by this entry. Worker must implement incrementally and return **READY FOR REVIEW**, not self-claim `APPROVED`.

Suggested sequence:

```text
A. domain event/relation/proof types
B. chain-qualified reverse indexes and candidate resolver
C. evidence-bound social identity/history resolver
D. recent projection + dedupe/retraction semantics
E. X browser-worker token-triggered adapter
F. API/dashboard surfaces
G. ten acceptance scenarios + cross-chain collision fixtures
```

### Verdict

**FEATURE ACCEPTED / NOT IMPLEMENTED** — architecture and approach frozen by this amendment.

---

## REV-021 — Implementasi REV-020 (Token Recent + Deployer/Social Reuse) + prerequisite fix

**Tanggal:** 2026-09-01 (post-implementation)
**Mode:** Implementation record (worker hermes) — **READY FOR REVIEW**, bukan self-claim `APPROVED`
**Acuan:** REV-020 (feature contract) + REV-019-F01 (narrative proof) + REV-007-F01 (migration chain)
**Scope:** sequence REV-020 A–G + dua prerequisite (migration bundle, narrative proof binding)

### Hasil implementasi

#### A — Domain types — `src/sf/recent.rs` (baru)
- `RecentRelation` (13 frozen variant, `SCREAMING_SNAKE_CASE`): `SameDeployer`,
  `SameAuthority`, `SameFeePayer`, `SameFunder`, `FundedByKnownDeployer`,
  `SameSocialAccount`, `ReusedSocialLink`, `OfficialCaAnnouncement`,
  `CrossChainDeployment`, `DerivativeOf`, `SuspectedCopycat`,
  `LiquidityAttentionRotatedTo`.
- `RecentConfidence` (`Exact`/`Reconstructed`/`Estimated`/`Insufficient`) + `rank()`.
- `IdentityKey`/`IdentityKind` (token/wallet/social/website/telegram, chain-qualified).
- `ActorExtraction`, `ActivationTrigger`, `Coverage`, `CapabilityStatus`,
  `Freshness`, `Retraction`, `RecentEvent`, `RecentTimeline`, `CandidateRelation`.
- `SocialEvidenceKind`, `SocialEvidenceObservation`, `trait TokenEvidenceAdapter`.

#### B — Reverse index + candidate resolver — `src/sf/recent_runtime.rs` (baru)
- `normalize_identity` (fail-closed, website `www.` strip via crate `url`).
- `resolve_candidates` (deterministic order; factory/launchpad ≠ deployer;
  `FundedByKnownDeployer` = `Reconstructed`; symbol-only never relates).

#### C — Social evidence resolver
- `build_lookup_query` (REV-020 line 1600 lookup terms, deduplicated).
- `classify_social` (official-CA vs mention vs profile-change).
- `InMemoryTokenEvidenceAdapter` (reference; concrete scraper transport deferred).

#### D — Recent projection + dedupe/retraction
- `build_timeline` (sort `occurred_at`, filter anchor).
- `assign_dependency_group` (correlated copies collapse, evidence preserved).
- `apply_retraction` (archive-not-delete; supersede status).
- `coverage_status` (full/on_demand/unavailable/degraded; never coerce to zero).
- `should_trigger_lookup` (dormant/archived/tombstoned = event-wake only).

#### E — Browser-worker adapter
- `BrowserPlatform` + variant `Web` (X/TikTok tetap).
- `TokenEvidenceAdapter` trait + task enqueue boundary. Transport scraper
  (self-hosted authorized-session) adalah follow-on, bukan bagian otomatis ini.

#### F — API + dashboard
- `GET /api/tokens/{chain}/{mint}/recent?window=1h|24h|7d|30d|all` (default `24h`).
- `GET /api/tokens/{chain}/{mint}/relations`.
- `src/sf/recent_store.rs` (sqlx `insert_recent_event`, `fetch_recent_timeline`,
  `fetch_relations`; `FromRow` struct, bukan 17-tuple).
- `static/index.html` dashboard (Token Recent Timeline + Deployer/Social Reuse
  panel + filter cross-chain/social/deployer + confidence/freshness/coverage
  texture + copycat vs official distinction).

#### G — Acceptance fixtures — `tests/recent_intelligence_regressions.rs` (baru)
- 10 skenario REV-020 minimum acceptance + 2 cross-chain collision fixture.

### Prerequisite fix

#### REV-007-F01 — Migration chain
- `1016_transition_table.sql`: `ADD COLUMN IF NOT EXISTS` untuk
  `capital_reservations.expires_at` (duplikat dari `1007` line 142) dan
  `reconciliation_incidents.escalation_deadline`.
- `1013_strategy_lab.sql`: `DROP CONSTRAINT` nama diperbaiki menjadi
  `strategy_versions_lifecycle_state_check` (nama auto Postgres; nama lama
  `..._lifecycle_check` tidak pernah match, sehingga CHECK lama menolak
  `canary`/`paused`).

#### REV-019-F01 — Narrative proof binding
- `src/sf/narrative.rs`: `StageProof { stage, artifact_kind, artifact_ref }` +
  `StageArtifactKind { EvidenceRef, GraphAssemblyRecord }`.
- `src/sf/narrative_runtime.rs::contiguous_stage` kini validasi jenis artifact:
  `EarliestEvidence` wajib `EvidenceRef`, final graph wajib `GraphAssemblyRecord`;
  wrong-kind/empty ref berhenti di stage sebelumnya (fail-closed).

### Migration baru
- `swi-deploy/migrations/1018_recent_intelligence.sql`: enum `recent_relation`,
  tabel `social_identities`, `recent_events` (append-only, `REVOKE UPDATE/DELETE/
  TRUNCATE`), index `(token_identity, occurred_at)`, `(relation, occurred_at)`,
  `(dependency_group)`, GIN `related_identities`.

### Verifikasi
```text
cargo test --locked = 264 passed, 0 failed
  (114 lib + 134 legacy + 12 acceptance REV-020 + 4 regression)
cargo check --locked --all-targets = PASS (2 warning dead-code pre-existing)
sqlglot (postgres): 1013/1016/1018 parse OK (DO $$ & REVOKE fallback Command, bukan error)
```

### Verdict

**READY FOR REVIEW** — implementasi REV-020 A–G + prerequisite fix selesai dan
test hijau. Blocker integration lama (wiring vertical slice, OIDC/WebAuthn,
provider selector) tetap deferred. Browser scraper transport (self-hosted
authorized-session X/TikTok) adalah follow-on terpisah; pure logic + API +
acceptance scenario sudah lengkap.

---

## REV-022 — Independent review of REV-021

**Tanggal:** 2026-09-01 07:35 UTC  
**Mode:** Read-only implementation review  
**Git HEAD:** `67ae903` + uncommitted REV-021 worktree  
**PLAN SWI:** `242b8091cdb81408d40175166262daf3bcda463bc33319bfdf3afd8c032bf63f`  
**Scope:** REV-020 A–G, REV-019-F01, migrations 1013/1016/1018.

### Verdict

**CHANGES REQUIRED / PARTIAL IMPLEMENTATION.** Tests and build pass, but REV-020 cannot be marked implemented.

### Findings

#### REV-022-F01 — HIGH — Candidate resolver is incomplete and promotes unverified edges to Exact

**Location:** `src/sf/recent_runtime.rs::resolve_candidates`, lines 54–199.

- `SameAuthority` and `SameFeePayer` are never emitted despite frozen actor fields/relations.
- Cross-chain/deployer/funder edges become `Exact` without requiring `TruthStatus::Confirmed`, non-empty evidence, valid window, or source independence.
- An empty/disputed caller-created `CrossChainDeployment` edge therefore joins a family as Exact.
- Social reuse resolves to the social identity itself, not the other token/project using it.

**Fix:** resolve every frozen relation from validated chain-qualified edges; require active valid window, confirmed truth, evidence, and the required corroboration class before Exact/family merge. Return the related token/project node for reuse relations.

**Regression:** disputed/evidence-free cross-chain edge must remain candidate/non-Exact; same authority and fee payer must resolve; two tokens reusing one immutable social ID must return the other token.

#### REV-022-F02 — HIGH — Official social announcement is caller-asserted

**Location:** `src/sf/recent_runtime.rs::classify_social`, lines 326–334; `tests/recent_intelligence_regressions.rs::accept5_exact_ca_announcement_is_official`.

`SocialEvidenceKind::OfficialCaAnnouncement` is mapped directly to the official relation. `SocialEvidenceObservation` carries no announced chain-qualified CA and no authoritative account binding. Any caller can label a mention as official.

**Fix:** classification must compare an extracted exact CA to the anchor token and verify immutable author account against a time-valid official social binding; otherwise mention/candidate.

**Regression:** unrelated author, wrong CA, missing raw evidence, or expired binding must not produce `OfficialCaAnnouncement`.

#### REV-022-F03 — HIGH — Activation gate is reduced to lifecycle allowlist

**Location:** `src/sf/recent_runtime.rs::should_trigger_lookup`, lines 283–289.

All lifecycle strings except dormant/archived/tombstoned return true. The function accepts no `ActivationTrigger`, so created/pre-graduation/cooling tokens can be polled without first liquidity, migration, caller/wallet/volume/social wake, or operator request. This violates cheap-first/event-wake behavior.

**Fix:** gate on typed lifecycle + typed activation trigger; dormant/dead only allow global wake/operator request; unknown lifecycle fails closed.

**Regression:** unknown/created/cooling with no trigger reject; explicit allowed activation passes; dormant only wake/operator passes.

#### REV-022-F04 — HIGH — Narrative StageProof remains forgeable

**Location:** `src/sf/narrative.rs::StageProof`, lines 61–67; `src/sf/narrative_runtime.rs::resolve/contiguous_stage`, lines 44–117.

Typed artifact kind does not bind proof to `narrative_key`, run, evidence store, or graph record. The resolver checks only non-empty free text + enum kind. Current positive test uses `ev-proof` while the actual edge ref is `ev`, yet final graph completes.

**Fix:** authoritative artifact lookup/validated proof object bound to narrative/run/stage. Earliest evidence must match relevant stored evidence; final graph must match a stored graph-assembly record.

**Regression:** fabricated ref, another narrative/run ref, and absent graph record must stop before that stage.

#### REV-022-F05 — HIGH — Recent API lacks workspace isolation and relation projection corrupts data

**Location:** `src/sf/recent_store.rs::fetch_recent_timeline`, lines 110–143; `fetch_relations`, lines 147–189; `src/api.rs::api_token_recent/api_token_relations`, lines 339–365.

Queries filter only `token_identity`, despite `workspace_id` in schema. Same token across workspaces can mix/leak data. Relations use `DISTINCT ON (relation)`, discard additional targets, keep only the first `related_identity`, and hard-code every returned confidence to `Estimated`, losing Exact/Reconstructed/Insufficient and current truth/retraction state.

**Fix:** require workspace identity in API/store filters; project one row per relation+target+valid version; preserve stored confidence/truth/evidence and exclude/surface superseded rows explicitly.

**Regression:** two workspaces with same token stay isolated; two SameDeployer targets both return; Exact remains Exact; superseded relation is not returned as current.

#### REV-022-F06 — HIGH — Existing funding-radar collection route was deleted

**Location:** `src/api.rs::router`, lines 24–40; dead `api_radar_cases`, line 367.

Adding recent routes replaced `/api/funding/radar/cases`; only `/{id}` remains. Existing collection endpoint now 404. Build warning confirms handler/row are dead code.

**Fix:** restore `.route("/api/funding/radar/cases", get(api_radar_cases))`.

**Regression:** router test confirms collection and detail routes both exist alongside recent endpoints.

#### REV-022-F07 — MEDIUM — Correlated copies are grouped but not collapsed; retraction projection is not resolved

**Location:** `src/sf/recent_runtime.rs::assign_dependency_group`, lines 228–244; `build_timeline`, lines 204–219; `apply_retraction`, lines 250–260.

Both provider/repost copies remain separate timeline events; only a group label is assigned. Evidence refs are not merged into one projected event. `apply_retraction` merely changes the row already carrying `retraction`; it does not resolve a separate append-only retraction against its target/superseded event or select the current event.

**Fix:** materialize one projected event per dependency group while preserving all evidence refs; model target event ID/ref and resolve append-only retraction/supersession into current + archived views.

**Regression:** two correlated copies produce one projected event with both refs; a separate retraction row supersedes its target while both remain archived.

#### REV-022-F08 — MEDIUM — In-memory timeline ordering is lexicographic, not chronological

**Location:** `src/sf/recent_runtime.rs::build_timeline`, lines 204–219.

RFC3339 strings with different offsets can sort incorrectly. Example: `2026-01-01T00:00:00+02:00` occurs before `2025-12-31T23:00:00Z` but string order reverses them.

**Fix:** store/parse `DateTime<Utc>` and fail closed on malformed time; sort instants, then observed instant.

**Regression:** mixed timezone offsets and malformed timestamp.

#### REV-022-F09 — MEDIUM — Dashboard filters are no-op and API failures masquerade as no data

**Location:** `static/index.html`, lines 89–116.

Checkbox state is read but never applied to events/relations. Every non-2xx response is converted to `[]`, so DB/auth/server failures display “No recent events/relations” instead of explicit unavailable/error status.

**Fix:** apply typed relation-category filters and render HTTP/capability failures distinctly from empty results.

**Regression:** toggling each filter changes visible rows; 500/401 render error/unavailable, not empty.

#### REV-022-F10 — HIGH — Migration fix edits shipped files; 1018 append-only protection is not authoritative

**Location:** `swi-deploy/migrations/1013_strategy_lab.sql`, `1016_transition_table.sql`, `1018_recent_intelligence.sql` lines 100; REVIEW_BRIEF criterion 24.

REV-021 edits existing 1013/1016 despite frozen immutable-migration rule. Already-applied databases will never receive those edits. `REVOKE ... FROM PUBLIC` does not stop table owner or explicitly granted application roles from UPDATE/DELETE/TRUNCATE; no append-only trigger/role grant test exists.

**Fix:** revert shipped files; add forward-only corrective migration(s) after 1018. Enforce append-only with dedicated writer role/grants plus trigger/policy that rejects mutation (with narrowly controlled maintenance role if required).

**Regression:** upgrade from pre-fix applied chain receives corrections; application writer can INSERT but UPDATE/DELETE/TRUNCATE fail.

### Additional implementation gaps

- `build_lookup_query` omits metadata name/symbol, descriptions, aliases, image/OCR hash, historical handles, Telegram/domain detail, caller, and linked contracts claimed by REV-021.
- X/TikTok/web transport remains only a trait + empty in-memory adapter; no recent-event ingestion vertical slice exists. This is a declared follow-on, therefore not a new bug, but feature status remains partial rather than implemented.

### Verification

```text
cargo +1.89.0 test --locked
264 passed, 0 failed

cargo +1.89.0 check --locked --all-targets
PASS, 2 warnings
  RadarCaseRow never constructed
  api_radar_cases never used
```

Fresh/upgrade PostgreSQL replay: **NOT VERIFIED** in this review. Windows/Tailscale went offline while launching the disposable replay; REV-021's `sqlglot` parse is not execution proof.

Source was not edited. Only this append-only review ledger entry is authorized when the Windows host returns online. Temporary `target-rev021-review/` should be removed after readback.

### Final status

**CHANGES REQUIRED / PARTIAL IMPLEMENTATION** — do not commit or mark REV-021 approved yet.


### Database replay addendum

Disposable PostgreSQL 18 replay completed after the Windows host returned online:

```text
canonical-only 1001..1018: PASS 18/18
combined legacy 0001..0010 + canonical: FAIL at 1005_intelligence.sql
ERROR: relation "tokens" already exists
```

Therefore the REV-021 edits make the canonical-only fresh path executable, including `1018`, but do **not** resolve the complete REV-007-F01 initialize/upgrade bundle claim. Migration immutability/upgrade-path finding `REV-022-F10` remains open.


### Independent reviewer + PostgreSQL probe addendum

Three independent scoped reviewers confirmed REV-022 and identified additional concrete failures:

1. **Persistence INSERT is nonfunctional for non-null relation/timestamps.** `recent_store.rs::insert_recent_event` binds Rust strings to PostgreSQL `timestamptz` and `recent_relation`. PostgreSQL 18 prepared-statement probes fail:

```text
occurred_at: type timestamptz, expression text
relation: type recent_relation, expression text
```

Use parsed `DateTime<Utc>` plus a typed SQLx enum or explicit validated casts. Add real PostgreSQL integration test for `insert_recent_event` with a non-null relation.

2. **Identity/factory validation is fail-open.** Token/wallet identity accepts arbitrary non-chain-qualified strings; `deployer == factory` can still yield `SameDeployer/Exact`. Validate chain-qualified identities and explicitly exclude factory/launchpad actors from project-deployer identity.

3. **`Reconstructed` is awarded too early.** One funding edge alone produces `FundedByKnownDeployer/Reconstructed`; REV-020 requires funding plus reused immutable social/domain and coherent time. Funding alone stays a weaker candidate.

4. **Social history schema contradicts append-only history.** `UNIQUE(platform, immutable_user_id)` prevents a second time-versioned row, while UPDATE is intended to be forbidden. PostgreSQL probe confirms the second handle version fails unique constraint. Use a versioned identity/binding key and enforce one current version separately.

5. **1016 does not enforce legal state transitions.** Database probe accepts direct `trade_intents.status: proposed → confirmed` without transition audit. A status vocabulary CHECK is not a transition table. Add forward migration with legal-edge table/trigger and append-only transition enforcement.

6. **Additional fail-closed gaps:** dependency grouping is one-hop rather than transitive; retraction accepts illegal truth statuses/dangling supersession; invalid/alias chain path is not canonicalized; empty transport adapter returns successful empty coverage; `REUSED_SOCIAL_LINK` is rendered as copycat although relations are independent.

These additions do not change the REV-022 verdict; they strengthen the same **CHANGES REQUIRED / PARTIAL IMPLEMENTATION** result.
