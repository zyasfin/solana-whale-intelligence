# REV-022 — Independent Review Addendum (cross-verification)

**Date:** 2026-09-01
**Mode:** Read-only independent re-verification of the REV-022 ledger entry.
**Scope:** REV-020 A–G, REV-019-F01, migrations 1013/1016/1018, and the 22
acceptance criteria in REVIEW_BRIEF.md.

This addendum does NOT edit source. It confirms, contradicts, or extends the
existing REV-022 findings. It is the second independent reviewer pass.

---

## 1. Build / test ground truth

Rust toolchain present on this host: `rustc 1.97.0`, `cargo 1.97.0`.

```
cargo build            — clean (2 pre-existing dead-code warnings: RadarCaseRow,
                          api_radar_cases)
cargo test             — 264 passed, 0 failed
                          (114 lib + 134 main + 12 recent acceptance + 4 regression)
```

This matches the REV-022 ledger exactly. The 2 dead-code warnings are the direct
symptom of REV-022-F06 (deleted funding-radar collection route).

## 2. Verification of REV-020 FIFO / idempotency fixes (older runtime)

Independently re-verified the four REV-020 fixes. All CORRECT:

- `cost_basis.rs` — buy lot sized by `amount_out` (token received), not `amount_in`.
- `cost_basis.rs` — FIFO isolated per `(chain, wallet, token)` via `HashMap` grouping.
- `cost_basis.rs` — oversell realizes proportional proceeds (`usd × matched/sell`).
- `ingest_runtime.rs` — fallback idempotency key is
  `source + entity + event_type + time_bucket + raw_hash`.

Regression tests present in both `cost_basis.rs` (3) and `ingest_runtime.rs` (2)
plus `tests/review_runtime_regressions.rs` (4). All pass.

## 3. Verification of the 19 runtime modules (acceptance criteria B)

All 19 runtime modules re-checked against REVIEW_BRIEF.md §2.B. Result: **PASS**
for all pre-existing runtime modules. Notable spot-checks:

- `signal_gate.rs` — 10 gates, first-failure-wins, fail-closed on None (age/
  liquidity/market), wash vs critical separated. Correct.
- `wallet_runtime.rs` — reuses `fifo_match` (no duplicated FIFO), average cost =
  cost/amount (0 if none), unrealized = mark−cost (no mark → −cost), early-entry
  = wallet_first − token_birth (None if either missing), recurrence via HashSet. Correct.
- `portfolio_runtime.rs` — union-find correlation grouping counted once per group. Correct.
- `source_health_runtime.rs` — disabled→DISABLED, 3+ failures→DOWN, connected-but-
  silent→SILENT, schema_stale→DEGRADED, degraded+good sample→RECOVERING. Correct.
- `strategy_runtime.rs` — lifecycle DRAFT..RETIRED, ACTIVE↔PAUSED toggle, RETIRED
  terminal, `shadow_passed` fails on any rejected candidate. Correct.
- `execution_runtime.rs` — delegates to frozen `IntentState::can_transition_to`
  (no duplicate table), Halt blocks all, ExitOnly permits only risk-reducing. Correct.
- `autonomy_runtime.rs` — `is_autonomous` only AUTO_BOUNDED; claim/close matures
  before open/reseed; thresholds_sane = 30/14/−10/CI≥0/human-approval. Correct.
- `decision_runtime.rs` — missing capability → MissingCapability; mandatory
  pass=false/None → Reject; all four REQUIRED_MANDATORY exactly-once-and-true. Correct.

## 4. Blockers #1/#2/#3 (acceptance criteria A)

- **Blocker #1** (transition table): `intent.rs::can_transition_to` is exhaustive
  and matches the frozen forward/reject/fail-closed/reconciliation table. PASS.
- **Blocker #2** (rollout thresholds): `autonomy.rs::RolloutThresholds` defaults
  = 30/14/−10.0/0.0/true; `thresholds_sane` enforces the frozen minimums. PASS.
- **Blocker #3** (provider tie-break): `source.rs::select_provider` is 3-stage
  (eligibility → rank_key → source_id lexicographic tie-break). PASS.

## 5. New REV-020/REV-021 code — independent findings

The REV-022 findings are CONFIRMED as real and still open (no fixes applied to
source). My independent pass additionally notes:

### 5.1 Documentation/count inconsistency (nitpick, not a bug)

`recent.rs::RecentRelation` and `1018_recent_intelligence.sql` both contain **12**
variants, but the doc comment and REVIEW_RESULT.md both claim "13 frozen variants".

Enumerated (12): SameDeployer, SameAuthority, SameFeePayer, SameFunder,
FundedByKnownDeployer, SameSocialAccount, ReusedSocialLink, OfficialCaAnnouncement,
CrossChainDeployment, DerivativeOf, SuspectedCopycat, LiquidityAttentionRotatedTo.

The Rust enum, `as_str()`, and SQL enum are internally consistent (all 12). This
is a count typo in prose, not a missing variant — per the brief's anti-over-review
rules I do NOT classify it as a bug, but it should be corrected when the "13"
claim is reconciled with the actual 12.

### 5.2 SQL migration immutability (confirms REV-022-F10)

`1013_strategy_lab.sql` still contains `DROP CONSTRAINT IF EXISTS ...` followed by
`ADD CONSTRAINT strategy_versions_lifecycle_check ...` on the same table — an
in-place edit to an already-shipped migration. Combined legacy `0001..0010 +
canonical` replay still fails at `1005` (`relation "tokens" already exists`).
Both already captured as REV-022-F10 / REV-007-F01; confirmed unchanged.

### 5.3 Forward-reference FK scan

Scanned all `REFERENCES` across `1001..1018`. No forward-reference FKs in the
canonical range (all referenced tables are created earlier or in the same
migration). No new finding beyond what is already recorded.

## 6. Verdict

I **concur** with the existing REV-022 verdict:

**CHANGES REQUIRED / PARTIAL IMPLEMENTATION.**

- The pre-existing 19 runtime modules + 3 blockers are sound.
- The new REV-020/REV-021 "Token Recent + Deployer/Social Reuse" vertical slice
  is not yet correct: F01–F10 (and the 6 PostgreSQL-probe findings) remain open.
- No source edits were made in this pass; only this read-only addendum is appended.
