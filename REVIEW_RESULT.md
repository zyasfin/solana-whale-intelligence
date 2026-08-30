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
