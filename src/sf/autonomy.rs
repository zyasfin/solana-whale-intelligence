//! Bounded autonomous execution (Phase 7, AUTO_BOUNDED).
//!
//! Canonical source: PLAN SWI §14/§15 autonomous cycles + trading modes
//! (AUTO_BOUNDED); `signal-forge-security-auto-trade-design.md` (rollout ladder);
//! `signal-forge-lp-autopilot-design.md` §14 Rollout.
//!
//! ## Frozen rollout thresholds (blocker #2 RESOLVED)
//! - Min forward sample: 30 trades/cycles per strategy/pool.
//! - Forward horizon: 14 calendar days.
//! - Max drawdown: -10% of dedicated hot wallet (hard stop).
//! - Approval: every limit raise requires human approval (WebAuthn step-up).
//! - Confidence interval: 95% CI lower bound > 0 before raise.
//! - Ordering: claim/close matures before open/reseed; CREATE_POOL stays out.

use serde::{Deserialize, Serialize};

/// Trading modes (doc §1 "Trading modes"). MVP = research/shadow/paper; future
/// bounded autonomous token trading + LP opening/management.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TradingMode {
    ReadOnly,
    Shadow,
    Paper,
    ConfirmEach,
    AutoBounded,
    Paused,
    Halted,
}

/// Frozen rollout thresholds (blocker #2). Concrete values, no longer TBD.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct RolloutThresholds {
    pub min_forward_sample: u32,     // 30
    pub forward_horizon_days: u32,   // 14
    pub max_drawdown_pct: f64,       // -10.0 (hard stop)
    pub ci_lower_bound: f64,         // 0.0 (95% CI lower bound > 0)
    pub requires_human_approval: bool, // true
}

impl Default for RolloutThresholds {
    fn default() -> Self {
        Self {
            min_forward_sample: 30,
            forward_horizon_days: 14,
            max_drawdown_pct: -10.0,
            ci_lower_bound: 0.0,
            requires_human_approval: true,
        }
    }
}

/// An autonomy guard: the forward-evidence gate for autonomous actions.
/// A limit raise is permitted only when the guard passes AND human approval
/// is granted (WebAuthn step-up, gate #9).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutonomyGuard {
    pub thresholds: RolloutThresholds,
    pub forward_sample: u32,
    pub forward_horizon_days: u32,
    pub current_drawdown_pct: f64,
    pub ci_lower_bound: f64,
    pub evidence_refs: Vec<String>, // reviewed forward results
    pub human_approved: bool,
}

impl AutonomyGuard {
    /// Evaluate whether a limit raise is permitted. Requires human approval
    /// (frozen rule), so this returns `false` even if all numeric thresholds
    /// pass but `human_approved` is false.
    pub fn can_raise_limit(&self) -> bool {
        self.thresholds.requires_human_approval
            && self.human_approved
            && self.forward_sample >= self.thresholds.min_forward_sample
            && self.forward_horizon_days >= self.thresholds.forward_horizon_days
            && self.current_drawdown_pct >= self.thresholds.max_drawdown_pct
            && self.ci_lower_bound > self.thresholds.ci_lower_bound
    }
}

/// Rollout ladder (security design "Auto-trade rollout" + LP design §14).
/// Autonomous claim/close matures before autonomous opening.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RolloutPhase {
    Shadow,
    Paper,
    ConfirmEach,
    AutoBoundedClaimClose,
    AutoBoundedOpenReseed,
    Advanced,
}

/// Autonomous cycle guard result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutonomousCycle {
    pub mode: TradingMode,
    pub phase: RolloutPhase,
    pub guard: AutonomyGuard,
    pub canary: bool, // canary = limited-scale first run
    pub max_notional: Option<String>,
}

/// Autonomous LP cycle (doc §15). `CREATE_POOL` remains outside this rollout.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutonomousLpCycle {
    pub pool_address: String,
    pub phase: RolloutPhase,
    pub guard: AutonomyGuard,
    pub action: super::execution::LpAction,
    pub reseed_after_evidence: bool,
}
