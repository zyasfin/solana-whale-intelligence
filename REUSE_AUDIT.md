# Signal Forge — Reuse Audit (pre-freeze concepts → final architecture)

Canonical architecture: `/root/PLAN-SWI-final-architecture-2026-08-29.md`
Pre-freeze code: `/root/swi-src/src/*.rs` (research-only prototype).

This document maps **concepts** from the pre-freeze code that are worth reusing
as references for the runtime implementation of Phase 1–2. Per principle #13
("no unlicensed code reuse; concepts are studied, source is not copied"), we
reuse the *ideas/design rules*, not the code.

## 1. Summary verdict

The pre-freeze code is not a different system from our scaffold — it is an
**earlier incarnation of the same design principles**. Most "rules" in the old
code are already formalized as PLAN SWI principles (§2, §8). The only thing to
fully discard (not reference) is password auth (`auth.rs`, `admin.rs`), which
the final architecture replaces with OIDC + WebAuthn + RBAC.

## 2. Concept map (reuse as reference, not copy)

Legend: ✅ REUSE-CONCEPT (reference the idea) · ⚠️ REPLACE (final architecture supersedes)

| Old module | Valuable concept | Verdict | Target in `sf/` |
|---|---|---|---|
| `replay.rs` | Temporal replay guard — prevent look-ahead bias; unknown availability → `incomplete_history_replay`/`stale_market_replay`, never leakage | ✅ | `sf/core.rs`, `sf/decision.rs` |
| `graph.rs` | Soft clustering: one transfer NEVER merges; membership needs confidence ≥0.70 across independent evidence; historical edges never mutated | ✅ | `sf/graph.rs`, `sf/revival.rs` |
| `signals.rs` | Gate model: first-failure-wins, exactly ONE rejection code per gate; evidence-based, no single score | ✅ | `sf/decision.rs`, `sf/portfolio.rs` |
| `scoring.rs` | FIFO trade matching for cost-basis; skill vs copyability SEPARATED; conviction capped 49 while completeness <0.80 | ✅ | `sf/wallet.rs` |
| `funding_radar.rs` | Two-stage alert (watch → preparation); dual threshold (SOL floor + USD floor + percentile) | ✅ | `sf/revival.rs` |
| `telegram_ingest.rs` | MTProto public allowlist; media NEVER downloaded; FloodWait pauses only affected channel; auth/ban disable without tight-loop; session outside repo | ✅ | `sf/caller.rs`, `sf/browser.rs` |
| `helius.rs` | Provider pool: per-key token bucket, cooldown, breaker, usage; no key rotation for evasion | ✅ | `sf/source.rs` |
| `gmgn.rs` | Query-only enrichment; trading routes/keys NEVER used; enrichment cannot overwrite canonical | ✅ | `sf/source.rs` |
| `narrative.rs` | GMGN-only evidence capped confidence 49 (narrative alone never creates a signal); temporal rule `observed_at <= eval_time` | ✅ | `sf/narrative.rs` |
| `auth.rs` (old) | bcrypt + cookie session | ⚠️ REPLACE | `sf/auth.rs` (already OIDC/RBAC/WebAuthn) |
| `admin.rs` (old) | password admin panel | ⚠️ REPLACE | dashboard (OIDC), not yet implemented |

## 3. Key finding: old rules = formalized PLAN SWI principles

| Old-code rule | PLAN SWI principle |
|---|---|
| `replay.rs` "no look-ahead" | #2 point-in-time truth |
| `graph.rs` "1 transfer never merges" | #11 manual assertions coexist |
| `gmgn.rs` "enrichment can't overwrite canonical" | #1 evidence before inference |
| `helius.rs` "no key rotation for evasion" | provider tie-break blocker #3 (already frozen) |
| `narrative.rs` "GMGN-only capped 49" | #8 LLM proposes/explains only |

## 4. What to reference when (runtime phases)

- **Phase 1** (core intelligence) → reference `scoring.rs` (FIFO cost-basis),
  `signals.rs` (gate model), `replay.rs` (temporal guard) for `sf/wallet.rs`
  and `sf/decision.rs` logic.
- **Phase 2** (caller + provenance) → reference `telegram_ingest.rs`
  (allowlist + no-media + FloodWait) and `narrative.rs` (confidence cap 49)
  for `sf/caller.rs` and `sf/narrative.rs` logic.
- **Provider pool** → reference `helius.rs` (token bucket + no-evasion) for
  `sf/source.rs` logic (tie-break already implemented).

## 5. What must NOT be reused (copy) — and why

- Any **source code** verbatim (principle #13).
- Password auth (`auth.rs`, `admin.rs`) — superseded by OIDC/WebAuthn/RBAC.
- GMGN "trading routes" — explicitly non-goal; query-only enrichment stays.

## 6. Recommended reading order for runtime implementers

1. `replay.rs` — the temporal guard is the single most reusable concept.
2. `signals.rs` — gate model + rejection codes (maps to `sf/decision.rs`).
3. `scoring.rs` — FIFO cost-basis (maps to `sf/wallet.rs`).
4. `helius.rs` — provider pool (maps to `sf/source.rs`).
