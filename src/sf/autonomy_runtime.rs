//! Runtime logic: bounded autonomous execution rollout (Phase 7, AUTO_BOUNDED).
//!
//! Canonical source: PLAN SWI §14/§15 autonomous cycles, `autonomy.rs` frozen
//! rollout thresholds (blocker #2), `signal-forge-security-auto-trade-design.md`
//! (rollout ladder), `signal-forge-lp-autopilot-design.md` §14 Rollout.
//!
//! Frozen ordering rule: autonomous claim/close matures BEFORE open/reseed;
//! CREATE_POOL stays out. A limit raise requires the numeric guard to pass AND
//! human approval (WebAuthn step-up, gate #9).
//!
//! Consumes the frozen `autonomy.rs` types (`TradingMode`, `RolloutPhase`,
//! `RolloutThresholds`, `AutonomyGuard`, `AutonomousCycle`,
//! `AutonomousLpCycle`) and introduces no new frozen state.

use super::autonomy::{
    AutonomyGuard, AutonomousCycle, RolloutPhase, RolloutThresholds, TradingMode,
};

/// Whether a trading mode permits any autonomous action (AUTO_BOUNDED only).
/// READ_ONLY/SHADOW/PAPER/CONFIRM_EACH are non-autonomous; PAUSED/HALTED block.
pub fn is_autonomous(mode: TradingMode) -> bool {
    mode == TradingMode::AutoBounded
}

/// Whether a rollout phase is mature enough to permit autonomous opening/
/// reseeding. Claim/close matures first (AutoBoundedClaimClose), open/reseed
/// only after (AutoBoundedOpenReseed / Advanced).
pub fn can_open(phase: RolloutPhase) -> bool {
    matches!(
        phase,
        RolloutPhase::AutoBoundedOpenReseed | RolloutPhase::Advanced
    )
}

/// Whether a rollout phase permits autonomous claim/close (the first autonomous
/// rung). Anything at or past AutoBoundedClaimClose.
pub fn can_claim_close(phase: RolloutPhase) -> bool {
    matches!(
        phase,
        RolloutPhase::AutoBoundedClaimClose
            | RolloutPhase::AutoBoundedOpenReseed
            | RolloutPhase::Advanced
    )
}

/// Evaluate a limit raise. Mirrors `AutonomyGuard::can_raise_limit` but adds the
/// explicit ordering rule: a raise is permitted only when the phase is
/// AutoBoundedClaimClose-or-later AND the numeric guard passes (human approval
/// already encoded in the guard).
pub fn limit_raise_permitted(phase: RolloutPhase, guard: &AutonomyGuard) -> bool {
    can_claim_close(phase)
        && !guard.evidence_refs.is_empty()
        && guard.can_raise_limit()
}

/// Single autonomous-action gate (REV-007-F06): mode, phase maturity, frozen
/// thresholds + evidence + human approval (all inside `can_raise_limit`).
/// This is the one entry point callers should use to decide whether an
/// autonomous action may run.
pub fn autonomous_action_permitted(phase: RolloutPhase, guard: &AutonomyGuard) -> bool {
    can_claim_close(phase)
        && !guard.evidence_refs.is_empty()
        && guard.can_raise_limit()
}

/// Validate frozen rollout thresholds are sane (min sample > 0, horizon > 0,
/// drawdown < 0, CI lower bound >= 0, requires approval). Returns true when all
/// hold (a malformed threshold set fails closed).
pub fn thresholds_sane(t: &RolloutThresholds) -> bool {
    t.min_forward_sample > 0
        && t.forward_horizon_days > 0
        && t.max_drawdown_pct < 0.0
        && t.ci_lower_bound >= 0.0
        && t.requires_human_approval
}

/// Whether an autonomous cycle is ready to run (REV-007-F06): mode is
/// AUTO_BOUNDED, phase is at least claim/close, canary is set, AND the guard
/// passes (numeric thresholds + human approval + evidence). A cycle with no
/// evidence refs or a failing guard is NOT ready.
pub fn cycle_ready(cycle: &AutonomousCycle) -> bool {
    is_autonomous(cycle.mode)
        && can_claim_close(cycle.phase)
        && cycle.canary
        && !cycle.guard.evidence_refs.is_empty()
        && cycle.guard.can_raise_limit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(approved: bool, sample: u32) -> AutonomyGuard {
        AutonomyGuard {
            thresholds: RolloutThresholds::default(),
            forward_sample: sample,
            forward_horizon_days: 14,
            current_drawdown_pct: -5.0,
            ci_lower_bound: 0.1,
            evidence_refs: vec!["ev1".into()],
            human_approved: approved,
        }
    }

    #[test]
    fn only_auto_bounded_is_autonomous() {
        assert!(is_autonomous(TradingMode::AutoBounded));
        assert!(!is_autonomous(TradingMode::Paper));
        assert!(!is_autonomous(TradingMode::ConfirmEach));
        assert!(!is_autonomous(TradingMode::Paused));
    }

    #[test]
    fn claim_close_matures_before_open() {
        assert!(can_claim_close(RolloutPhase::AutoBoundedClaimClose));
        assert!(!can_claim_close(RolloutPhase::ConfirmEach));
        assert!(!can_open(RolloutPhase::AutoBoundedClaimClose));
        assert!(can_open(RolloutPhase::AutoBoundedOpenReseed));
        assert!(can_open(RolloutPhase::Advanced));
    }

    #[test]
    fn limit_raise_requires_phase_and_guard() {
        let g = guard(true, 30);
        assert!(limit_raise_permitted(RolloutPhase::AutoBoundedClaimClose, &g));
        assert!(!limit_raise_permitted(RolloutPhase::ConfirmEach, &g)); // phase too early
        let no_approval = guard(false, 30);
        assert!(!limit_raise_permitted(RolloutPhase::AutoBoundedClaimClose, &no_approval));
    }

    #[test]
    fn thresholds_must_be_sane() {
        assert!(thresholds_sane(&RolloutThresholds::default()));
        let mut bad = RolloutThresholds::default();
        bad.min_forward_sample = 0;
        assert!(!thresholds_sane(&bad));
    }

    #[test]
    fn cycle_ready_requires_canary() {
        let mut c = AutonomousCycle {
            mode: TradingMode::AutoBounded,
            phase: RolloutPhase::AutoBoundedClaimClose,
            guard: guard(true, 30),
            canary: true,
            max_notional: None,
        };
        assert!(cycle_ready(&c));
        c.canary = false;
        assert!(!cycle_ready(&c));
    }

    // REV-007-F06: a cycle with no evidence refs is NOT ready, even if all
    // other gates pass.
    #[test]
    fn cycle_ready_requires_evidence() {
        let mut g = guard(true, 30);
        g.evidence_refs = vec![];
        let c = AutonomousCycle {
            mode: TradingMode::AutoBounded,
            phase: RolloutPhase::AutoBoundedClaimClose,
            guard: g,
            canary: true,
            max_notional: None,
        };
        assert!(!cycle_ready(&c));
    }
}
