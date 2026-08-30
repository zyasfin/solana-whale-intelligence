# Signal Forge — Implementation Conventions & Frozen Decisions

Canonical architecture: `/root/PLAN-SWI-final-architecture-2026-08-29.md`
(checksum `482e24ac6e0342dfe001aaf155f9d12f9879c4d1cfdc1123e37300a43d64d7d1`, 1380 lines).

This document records the decisions frozen during the Phase 0–7 scaffold pass
and the blockers still open. It is a working record, not the architecture itself;
the canonical PLAN SWI document supersedes this file on any conflict.

## 1. Frozen decisions

| # | Decision | Value |
|---|---|---|
| 1 | Language / framework | Rust (Axum) |
| 2 | Pre-freeze schema (`swi-deploy/migrations/0001..0010`) | Replace total |
| 3 | Repository | Continue pre-freeze repo (`swi-src`), evolve in-place |
| 4 | Intent state machine (§16) | **FROZEN** — see "Frozen intent transition table" below |

Derived conventions:
- **Repo layout**: single crate `swi-src`, modular monolith (principle #12).
  New domain code lives under `src/sf/` (namespaced `sf::`), distinct from the
  legacy flat CLI modules (`api`, `auth`, `models`, ...) still referenced by
  `main.rs`. As phases complete, legacy modules are replaced by `sf::` domains.
- **Migrations**: new canonical schema starts at `1001_` (deliberately past the
  legacy `0001..0010`) so replace-total is unambiguous. Immutable; never edit a
  shipped migration — add a new one (archive-not-delete, principle #6).
- **Domain scaffold** (`src/sf/*.rs`): types/enums/interfaces only (buildable,
  dependency-light: `serde` + `serde_json`). Runtime logic is wired per phase.

## 2. Architecture principles encoded (PLAN SWI §2)

1. Evidence before inference.
2. Point-in-time truth.
3. No opaque universal score (N/A, missing, zero, safe are DISTINCT).
4. Capability mandatory, vendor replaceable.
5. Cheap first.
6. Archive, not delete (valid-window + supersedes_id columns).
7. Fail closed for execution.
8. LLM proposes/explains only.
9. No direct source-to-signer path (immutable intent).
10. Reconciliation before retry.
11. Manual assertions coexist (never silently overwritten).
12. Minimal deployables (modular monolith).
13. No unlicensed code reuse.

## 3. Frozen intent transition table (blocker #1 — RESOLVED)

Authoritative transition table (fail-closed, principle #7/#10). Encoded in
`sf/intent.rs` (`IntentState::can_transition_to`) and `1016_transition_table.sql`.

Forward path:
  PROPOSED -> APPROVED -> RESERVED -> BUILT -> SIMULATED -> SIGNED -> SUBMITTED -> CONFIRMED
Reject/cancel:  PROPOSED -> CANCELLED | APPROVED -> CANCELLED
Fail-closed:    RESERVED|BUILT|SIMULATED|SIGNED|SUBMITTED -> FAILED_SAFE (release reservation)
Reconciliation: SUBMITTED -> UNKNOWN_RECONCILIATION
                UNKNOWN_RECONCILIATION -> CONFIRMED (LANDED) | FAILED_SAFE (NOT_LANDED) | CANCELLED (escalation)
Illegal: backward transitions, state skips, retry from terminal states,
         SUBMITTED from UNKNOWN_RECONCILIATION (no new submission while UNKNOWN).

Reservation rules (defaults):
  - Expiry: configurable per chain, DEFAULT 120s (RESERVED..SIGNED).
  - UNKNOWN_RECONCILIATION holds reservation; escalation to CANCELLED after 10m
    (no auto-release until recon is definite).
  - FAILED_SAFE never auto-retries; a retry is a NEW intent (new idempotency key).

## 4. Frozen rollout thresholds (blocker #2 — RESOLVED)

Encoded in `sf/autonomy.rs` (`RolloutThresholds`) and `1017_rollout_thresholds.sql`.

- Min forward sample: **30** trades/cycles per strategy/pool.
- Forward horizon: **14** calendar days.
- Max drawdown: **-10%** of dedicated hot wallet (hard stop).
- CI: 95% confidence interval lower bound **> 0** before raise.
- Approval: every limit raise requires **human approval** (WebAuthn step-up, gate #9).
- Ordering: claim/close matures before open/reseed; CREATE_POOL stays out.

## 5. Frozen provider tie-break (blocker #3 — RESOLVED)

Encoded in `sf/source.rs` (`select_provider`, `rank_key`, `ineligible_reason`).
Deterministic 3-stage scheme (no blind round-robin, no quota evasion):

1. **Eligibility filter** (fail-closed): disabled / auth-failed / cooldown /
   breaker-open / health DOWN-DISABLED / out-of-quota / chain-id-invalid are
   excluded, not ranked.
2. **Deterministic ranking**: capability_role (mandatory>vendor>fallback) ->
   health (UP>RECOVERING>DEGRADED>SILENT) -> weight desc -> latency asc ->
   cost asc.
3. **Final tie-break**: `source_id` lexicographic ascending (stable/reproducible).

Anti-evasion rules: request-invalid stops retry (not tried on all keys);
auth/validation error disables credential; semantic fallback stored as separate
observation (vendors never silently overwrite); public no-key API uses one
host-level limiter/cache.

## 6. Artifact map

- `swi-src/src/lib.rs` — crate gate, `pub mod sf`.
- `swi-src/src/sf/` — 21 domain modules (see below).
- `swi-deploy/migrations/1001..1017` — canonical schema (replace-total).

Domain modules (`sf/`):
- Foundation: `core`, `identity`, `source`, `graph`, `decision`, `intent`, `jobs`, `auth`
- Phase 1: `token`, `wallet`, `portfolio`, `ingest`, `dashboard`
- Phase 2: `caller`, `narrative`, `browser`
- Phase 3: `revival`
- Phase 4: `lp`
- Phase 5: `strategy`
- Phase 6: `execution`
- Phase 7: `autonomy`

## 7. Runtime implementation status & review findings

Runtime logic implemented (Phase 1), all in `src/sf/`:
- `cost_basis.rs` — FIFO cost-basis matching (buys open `amount_out` lots; per
  `(chain, wallet, token)` isolation; oversell realizes only matched proceeds).
- `ingest_runtime.rs` — §7.2 ingestion pipeline (10 stages, fail-closed,
  idempotency 2-mode).
- `signal_gate.rs` — entry signal gate model (first-failure-wins, 10 gates,
  fail-closed on missing data).

Review findings (agent hermes, `tests/review_runtime_regressions.rs`) — 4 bugs
found and fixed, with in-module regression tests added:
1. Buy used `amount_in` instead of `amount_out` for token quantity → fixed.
2. FIFO was not isolated per wallet → grouped by `(chain, wallet, token)`.
3. Oversell realized full proceeds instead of matched-only → proportional.
4. Fallback idempotency key too aggressive → now `source + entity + event type
   + time bucket + raw hash` (§7.1); `RawPayload` gained `event_type`.

Build/test status (verified locally on Rust 1.97, crate `solana-whale-intelligence`):
- `cargo build` clean; `cargo test` = 143 passed (134 legacy `main.rs` + 4 agent
  review + 5 in-module regression), 0 failed.

## 8. Known limitations

- Runtime modules are pure logic (no `sqlx`/Postgres wiring yet).
- `main.rs` (legacy bin) does not yet `use` the `sf` library — deferred.
- Migrations not yet applied to a live Postgres (no verified instance on
  `hermes-master`).
- No Rust toolchain on `hermes-master` (build/test done on a Windows copy).
- Source of truth for editing/build: Windows copy `C:/temp/swi-review-current/swi-src`; `hermes-master` is deploy-only (no build, no `target/`).
- Phase 7 "advanced compound/partial/multi-layer/V4 hooks" are enums/columns only.

## 9. Recommended next steps

All three blockers resolved. Remaining work:
1. Install Rust toolchain on the build host; run `cargo build` + `sqlx migrate run`.
2. Wire `sf` runtime into `main.rs` (or a new bin).
3. Implement remaining runtime: provider calls (`source.rs`), revival/funding
   graph, wallet swap reconstruction, and DB persistence for the three modules.

## 10. Runtime implementation workflow (per-runtime, git-backed)

Build/edit source of truth is the Windows copy (`C:/temp/swi-review-current/swi-src`);
`hermes-master` is deploy-only and reads from this path directly (no per-file scp).

Workflow per runtime module:
1. Implement 1 runtime module.
2. Review + fix bugs; add regression test for each bug.
3. `cargo test` must be green before commit (never commit red).
4. Commit one module per commit: message `sf(<module>): <short description>`.
5. Bug fixes from review: separate commit `fix(<module>): <bug>`.

