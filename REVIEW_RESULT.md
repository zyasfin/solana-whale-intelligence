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
