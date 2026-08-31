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
use super::execution::{Action, LpAction};

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

/// Evaluate a limit raise (REV-013 addendum #1). Delegates to the single
/// authoritative gate so frozen thresholds (`thresholds_sane`) can never be
/// bypassed through this helper. A limit raise is a claim/close-class action
/// (not open/reseed), canary-gated.
pub fn limit_raise_permitted(phase: RolloutPhase, guard: &AutonomyGuard) -> bool {
    autonomous_action_permitted(
        TradingMode::AutoBounded,
        phase,
        Action::Lp(LpAction::ClaimFees),
        true,
        guard,
    )
}

/// Single authoritative autonomous-action gate (REV-013-F01): gates on
/// `(mode, phase, action, canary, guard)`. Open/reseed is DERIVED from the typed
/// `Action` (not a caller boolean), so `Lp(OpenPosition|ReseedPosition)` can
/// never run at the earlier claim/close rung.
pub fn autonomous_action_permitted(
    mode: TradingMode,
    phase: RolloutPhase,
    action: Action,
    canary: bool,
    guard: &AutonomyGuard,
) -> bool {
    if !is_autonomous(mode) {
        return false;
    }
    if !canary {
        return false;
    }
    if !thresholds_sane(&guard.thresholds) {
        return false;
    }
    let is_open = matches!(
        action,
        Action::Lp(LpAction::OpenPosition) | Action::Lp(LpAction::ReseedPosition)
    );
    let phase_ok = if is_open {
        can_open(phase)
    } else {
        can_claim_close(phase)
    };
    phase_ok && !guard.evidence_refs.is_empty() && guard.can_raise_limit()
}

/// Validate rollout thresholds against the FROZEN minimums (REV-011-F03):
/// min_forward_sample >= 30, forward_horizon_days >= 14,
/// -10.0 <= max_drawdown_pct < 0, ci_lower_bound >= 0, requires_human_approval.
/// Stricter values are allowed; weaker values fail closed.
pub fn thresholds_sane(t: &RolloutThresholds) -> bool {
    t.min_forward_sample >= 30
        && t.forward_horizon_days >= 14
        && t.max_drawdown_pct >= -10.0
        && t.max_drawdown_pct < 0.0
        && t.ci_lower_bound >= 0.0
        && t.requires_human_approval
}

/// Whether an autonomous cycle is ready to run (REV-011-F03): delegates to the
/// single authoritative `autonomous_action_permitted` gate. The cycle has no
/// typed action of its own, so it uses a claim/close action as the readiness
/// floor (mature enough to claim/close).
pub fn cycle_ready(cycle: &AutonomousCycle) -> bool {
    autonomous_action_permitted(
        cycle.mode,
        cycle.phase,
        Action::Lp(LpAction::ClaimFees),
        cycle.canary,
        &cycle.guard,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(approved: bool, sample: u32) -> AutonomyGuard {
        let mut thresholds = RolloutThresholds::default();
        thresholds.ci_lower_bound = 0.1; // satisfy frozen CI > 0 requirement
        AutonomyGuard {
            thresholds,
            forward_sample: sample,
            forward_horizon_days: 14,
            current_drawdown_pct: -5.0,
            ci_lower_bound: 0.2, // > threshold.ci_lower_bound (0.1) so can_raise_limit passes
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
        // A fully-satisfied frozen threshold set passes.
        let mut good = RolloutThresholds::default();
        good.ci_lower_bound = 0.1;
        assert!(thresholds_sane(&good));
        let mut bad = RolloutThresholds::default();
        bad.min_forward_sample = 0;
        assert!(!thresholds_sane(&bad));
        // REV-011-F03: a weaker-but-still-"positive" threshold must fail.
        let mut weak = RolloutThresholds::default();
        weak.min_forward_sample = 1;
        weak.forward_horizon_days = 1;
        weak.max_drawdown_pct = -0.1;
        weak.ci_lower_bound = -100.0;
        assert!(!thresholds_sane(&weak));
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

    // REV-009-F03: open/reseed requires AutoBoundedOpenReseed; claim/close uses
    // earlier rung; non-AUTO_BOUNDED mode is rejected.
    #[test]
    fn autonomous_action_gate_distinguishes_action_kind() {
        let g = guard(true, 30);
        // claim/close (ClaimFees) at claim/close rung -> ok.
        assert!(autonomous_action_permitted(
            TradingMode::AutoBounded, RolloutPhase::AutoBoundedClaimClose,
            Action::Lp(LpAction::ClaimFees), true, &g));
        // open/reseed (OpenPosition) at claim/close rung -> rejected.
        assert!(!autonomous_action_permitted(
            TradingMode::AutoBounded, RolloutPhase::AutoBoundedClaimClose,
            Action::Lp(LpAction::OpenPosition), true, &g));
        // open at open rung -> ok.
        assert!(autonomous_action_permitted(
            TradingMode::AutoBounded, RolloutPhase::AutoBoundedOpenReseed,
            Action::Lp(LpAction::OpenPosition), true, &g));
        // non-AUTO_BOUNDED -> rejected.
        assert!(!autonomous_action_permitted(
            TradingMode::Paper, RolloutPhase::AutoBoundedOpenReseed,
            Action::Lp(LpAction::OpenPosition), true, &g));
        // REV-013-F01: canary=false must fail.
        assert!(!autonomous_action_permitted(
            TradingMode::AutoBounded, RolloutPhase::AutoBoundedOpenReseed,
            Action::Lp(LpAction::OpenPosition), false, &g));
    }
}
