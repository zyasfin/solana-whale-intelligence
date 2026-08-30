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
