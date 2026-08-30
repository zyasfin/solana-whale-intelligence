//! Runtime logic: strategy lifecycle + shadow/paper evaluation (Phase 5).
//!
//! Canonical source: PLAN SWI §13 "Strategy Lab and evaluation" (lines 855-895).
//! Lifecycle: DRAFT -> SHADOW -> PAPER -> VALIDATED -> APPROVED -> CANARY ->
//! ACTIVE -> PAUSED -> RETIRED. Activation follows shadow/paper/validation/
//! approval (gate #14): a strategy can never skip to ACTIVE without passing
//! SHADOW and PAPER first.
//!
//! This module enforces the lifecycle order and evaluates shadow outcomes. It
//! consumes the frozen `strategy.rs` types (`StrategyLifecycle`,
//! `StrategyVersion`, `ShadowOutcome`, `PaperExecution`) and introduces no new
//! frozen state.

use super::strategy::{PaperExecution, ShadowOutcome, StrategyLifecycle};

/// Ordered lifecycle index (matches frozen §13 list).
fn stage_index(s: StrategyLifecycle) -> u8 {
    match s {
        StrategyLifecycle::Draft => 0,
        StrategyLifecycle::Shadow => 1,
        StrategyLifecycle::Paper => 2,
        StrategyLifecycle::Validated => 3,
        StrategyLifecycle::Approved => 4,
        StrategyLifecycle::Canary => 5,
        StrategyLifecycle::Active => 6,
        StrategyLifecycle::Paused => 7,
        StrategyLifecycle::Retired => 8,
    }
}

/// Whether a strategy can transition `current -> next`.
/// - Forward-only (no regression), except PAUSED <-> ACTIVE (a reversible
///   operational toggle, doc §13 gate #14 allows pause/resume).
/// - RETIRED is terminal.
/// - The SHADOW and PAPER gates are enforced: to reach ACTIVE/CANARY, the
///   strategy must have passed through SHADOW and PAPER (index >= Paper).
pub fn can_transition(current: StrategyLifecycle, next: StrategyLifecycle) -> bool {
    if current == StrategyLifecycle::Retired {
        return false; // terminal
    }
    // Pause/resume toggle: ACTIVE <-> PAUSED is allowed both ways.
    if (current == StrategyLifecycle::Active && next == StrategyLifecycle::Paused)
        || (current == StrategyLifecycle::Paused && next == StrategyLifecycle::Active)
    {
        return true;
    }
    stage_index(next) == stage_index(current) + 1
}

/// Whether a strategy version is ready to go live: it must be APPROVED (or
/// CANARY/ACTIVE) — never DRAFT/SHADOW/PAPER/VALIDATED (gate #14: shadow before
/// canary/active).
pub fn is_live_ready(lifecycle: StrategyLifecycle) -> bool {
    matches!(
        lifecycle,
        StrategyLifecycle::Approved | StrategyLifecycle::Canary | StrategyLifecycle::Active
    )
}

/// Count negative findings retained across shadow outcomes (doc §13 requirement
/// #13: negative findings retained; they power missed-runner review).
pub fn negative_findings(outcomes: &[ShadowOutcome]) -> usize {
    outcomes.iter().map(|o| o.rejected_candidates.len()).sum()
}

/// Whether a shadow outcome passed (no rejected candidates and has metrics).
/// A shadow with rejected candidates is a FAIL (fail-closed — rejection means
/// the candidate did not survive shadow).
pub fn shadow_passed(outcome: &ShadowOutcome) -> bool {
    !outcome.rejected_candidates.is_empty() == false && outcome.metrics != serde_json::Value::Null
}

/// Whether a paper execution produced a positive PnL (None -> false, fail-closed).
pub fn paper_positive(exec: &PaperExecution) -> bool {
    exec.pnl
        .as_ref()
        .and_then(|p| p.parse::<f64>().ok())
        .map(|p| p > 0.0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_forward_only_with_pause_toggle() {
        assert!(can_transition(StrategyLifecycle::Draft, StrategyLifecycle::Shadow));
        assert!(can_transition(StrategyLifecycle::Shadow, StrategyLifecycle::Paper));
        assert!(!can_transition(StrategyLifecycle::Active, StrategyLifecycle::Draft)); // regression
        assert!(can_transition(StrategyLifecycle::Active, StrategyLifecycle::Paused));
        assert!(can_transition(StrategyLifecycle::Paused, StrategyLifecycle::Active));
        // REV-003-F05: cannot skip gates (Draft -> Active rejected).
        assert!(!can_transition(StrategyLifecycle::Draft, StrategyLifecycle::Active));
    }

    #[test]
    fn retired_is_terminal() {
        assert!(!can_transition(StrategyLifecycle::Retired, StrategyLifecycle::Active));
    }

    #[test]
    fn live_ready_requires_approval() {
        assert!(!is_live_ready(StrategyLifecycle::Paper));
        assert!(!is_live_ready(StrategyLifecycle::Validated));
        assert!(is_live_ready(StrategyLifecycle::Approved));
        assert!(is_live_ready(StrategyLifecycle::Active));
    }

    #[test]
    fn shadow_passed_rejects_candidates() {
        let pass = ShadowOutcome {
            strategy_version_id: "v1".into(),
            window_start: "2026-01-01".into(),
            window_end: "2026-01-08".into(),
            metrics: serde_json::json!({"sharpe": 1.0}),
            rejected_candidates: vec![],
        };
        assert!(shadow_passed(&pass));

        let fail = ShadowOutcome {
            strategy_version_id: "v1".into(),
            window_start: "2026-01-01".into(),
            window_end: "2026-01-08".into(),
            metrics: serde_json::json!({}),
            rejected_candidates: vec!["cand1".into()],
        };
        assert!(!shadow_passed(&fail));
    }

    #[test]
    fn negative_findings_are_counted() {
        let o1 = ShadowOutcome { strategy_version_id: "v1".into(), window_start: "a".into(), window_end: "b".into(), metrics: serde_json::json!({}), rejected_candidates: vec!["x".into(), "y".into()] };
        let o2 = ShadowOutcome { strategy_version_id: "v2".into(), window_start: "a".into(), window_end: "b".into(), metrics: serde_json::json!({}), rejected_candidates: vec!["z".into()] };
        assert_eq!(negative_findings(&[o1, o2]), 3);
    }

    #[test]
    fn paper_positive_pnl() {
        let pos = PaperExecution { strategy_version_id: "v1".into(), quote: "q".into(), friction: super::super::strategy::PaperFriction { fees: None, gas: None, tip: None, rent: None, slippage: None, latency: None }, pnl: Some("5.0".into()) };
        assert!(paper_positive(&pos));

        let neg = PaperExecution { strategy_version_id: "v1".into(), quote: "q".into(), friction: super::super::strategy::PaperFriction { fees: None, gas: None, tip: None, rent: None, slippage: None, latency: None }, pnl: Some("-5.0".into()) };
        assert!(!paper_positive(&neg));
    }
}
