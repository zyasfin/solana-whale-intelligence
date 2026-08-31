//! Runtime logic: execution state machine, kill-switch gating, signer policy
//! checklist (Phase 6).
//!
//! Canonical source: PLAN SWI §16 "Execution state machine" (lines 1024-1072),
//! §17 "Signer policy" (lines 1074-1093), §19 "Kill switches" (lines 1105-1122).
//!
//! Key rules encoded here:
//! - Execution states (PROPOSED→…→CONFIRMED/FAILED_SAFE/UNKNOWN_RECONCILIATION/
//!   CANCELLED) with a reconciliation gate: no new submission while UNKNOWN.
//! - Kill switch: when halted, no new entries/signatures by default; only
//!   risk-reducing actions may operate under EXIT_ONLY.
//! - Signer policy is a closed checklist, never "sign anything".
//!
//! Consumes the frozen `execution.rs` types (`TradeAction`, `LpAction`,
//! `KillSwitch`, `KillSwitchScope`, `KillSwitchMode`, `SignerPolicy`,
//! `PolicyLimits`) and introduces no new frozen state.

use super::execution::{KillSwitch, KillSwitchMode, LpAction, SignerPolicy, TradeAction};
use super::intent::IntentState;

/// Re-export the frozen intent state (doc §16). The execution state machine is
/// the intent state machine — we do NOT duplicate the transition table
/// (REV-005-F01): `can_transition` delegates to the frozen
/// `IntentState::can_transition_to`.
pub use super::intent::IntentState as ExecutionState;

/// Whether an execution state can transition `current -> next`, delegated to the
/// frozen transition table in `intent.rs` (REV-005-F01). This preserves the
/// reconciliation gate (UNKNOWN_RECONCILIATION only -> CONFIRMED/FAILED_SAFE/
/// CANCELLED) and the terminal-state rules exactly as frozen.
pub fn can_transition(current: IntentState, next: IntentState) -> bool {
    current.can_transition_to(next)
}

/// Whether a kill switch is halted (blocks new entries/signatures).
/// `EXIT_ONLY` still blocks risk-ADDING actions but permits risk-reducing ones.
pub fn is_halted(switches: &[KillSwitch]) -> bool {
    switches.iter().any(|s| s.active)
}

/// Whether an action is risk-reducing (permitted under EXIT_ONLY). Risk-reducing
/// = sell/close/exit/withdraw/claim/emergency-exit. Risk-ADDING (buy/open/
/// add-liquidity/reseed) is blocked under any active halt.
pub fn is_risk_reducing(action: TradeAction) -> bool {
    matches!(
        action,
        TradeAction::Sell
            | TradeAction::PartialSell
            | TradeAction::Close
            | TradeAction::EmergencyExit
    )
}

/// Whether an LP action is risk-reducing (permitted under EXIT_ONLY).
pub fn is_lp_risk_reducing(action: LpAction) -> bool {
    matches!(
        action,
        LpAction::ClaimFees
            | LpAction::PartialWithdraw
            | LpAction::ClosePosition
            | LpAction::SwapResiduals
            | LpAction::EmergencyExit
    )
}

/// Decide whether an action is permitted given the active kill switches.
/// - No active switch -> permitted.
/// - Any `Halt` switch -> blocked (no new entries/signatures).
/// - Only `ExitOnly` switches -> permitted iff the action is risk-reducing.
pub fn action_permitted(switches: &[KillSwitch], action: TradeAction) -> bool {
    let active: Vec<&KillSwitch> = switches.iter().filter(|s| s.active).collect();
    if active.is_empty() {
        return true;
    }
    let has_halt = active.iter().any(|s| s.mode == KillSwitchMode::Halt);
    if has_halt {
        return false;
    }
    // Only ExitOnly switches remain -> risk-reducing only.
    is_risk_reducing(action)
}

/// Parse a free-form action string into a closed `TradeAction` or `LpAction`,
/// returning `None` for unknown/arbitrary strings (REV-007-F07). This is the
/// validator callers MUST use before persisting an intent/decision, so unknown
/// actions are rejected instead of flowing through as free text.
pub fn parse_action(s: &str) -> Option<Action> {
    match s {
        "buy" | "Buy" | "BUY" => Some(Action::Trade(TradeAction::Buy)),
        "sell" | "Sell" | "SELL" => Some(Action::Trade(TradeAction::Sell)),
        "partial_sell" | "PartialSell" | "PARTIAL_SELL" => {
            Some(Action::Trade(TradeAction::PartialSell))
        }
        "close" | "Close" | "CLOSE" => Some(Action::Trade(TradeAction::Close)),
        "emergency_exit" | "EmergencyExit" | "EMERGENCY_EXIT" => {
            Some(Action::Trade(TradeAction::EmergencyExit))
        }
        "open_position" | "OpenPosition" | "OPEN_POSITION" => {
            Some(Action::Lp(LpAction::OpenPosition))
        }
        "add_liquidity" | "AddLiquidity" | "ADD_LIQUIDITY" => {
            Some(Action::Lp(LpAction::AddLiquidity))
        }
        "claim_fees" | "ClaimFees" | "CLAIM_FEES" => Some(Action::Lp(LpAction::ClaimFees)),
        "compound_fees" | "CompoundFees" | "COMPOUND_FEES" => {
            Some(Action::Lp(LpAction::CompoundFees))
        }
        "partial_withdraw" | "PartialWithdraw" | "PARTIAL_WITHDRAW" => {
            Some(Action::Lp(LpAction::PartialWithdraw))
        }
        "close_position" | "ClosePosition" | "CLOSE_POSITION" => {
            Some(Action::Lp(LpAction::ClosePosition))
        }
        "reseed_position" | "ReseedPosition" | "RESEED_POSITION" => {
            Some(Action::Lp(LpAction::ReseedPosition))
        }
        "swap_residuals" | "SwapResiduals" | "SWAP_RESIDUALS" => {
            Some(Action::Lp(LpAction::SwapResiduals))
        }
        _ => None,
    }
}

/// A closed action: either a token trade or an LP action (REV-007-F07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Trade(TradeAction),
    Lp(LpAction),
}

/// Whether a signer policy checklist passes (every closed check must be true).
/// Mandatory fields are NON-OPTIONAL at sign time: a `None` on chain_id,
/// chain_genesis, function_selector, max_native_debit, max_token_debit, or
/// min_output fails closed (REV-007-F05). The signer validates the full
/// transaction semantics — it never signs with absent mandatory context.
pub fn signer_policy_passes(p: &SignerPolicy) -> bool {
    // REV-009-F02 + addendum #1: mandatory fields must be Some AND non-empty
    // (whitespace-only fails closed). The signer never signs with absent or
    // empty mandatory context.
    let non_empty = |s: &Option<String>| s.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false);
    non_empty(&p.chain_id)
        && non_empty(&p.chain_genesis)
        && non_empty(&p.function_selector)
        && non_empty(&p.max_native_debit)
        && non_empty(&p.max_token_debit)
        && non_empty(&p.min_output)
        && p.policy_active
        && p.policy_not_expired
        && p.policy_not_halted
        && p.intent_hash_valid
        && p.nonce_valid
        && p.router_allowed
        && p.program_allowed
        && p.token_pair_verified
        && p.recipient_verified
        && p.slippage_ok
        && p.price_impact_ok
        && p.deadline_ok
        && p.simulation_delta_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::execution::KillSwitchScope;

    fn ks(mode: KillSwitchMode, active: bool) -> KillSwitch {
        KillSwitch {
            scope: KillSwitchScope::Global,
            scope_key: None,
            mode,
            active,
        }
    }

    #[test]
    fn execution_forward_matches_frozen_table() {
        // Frozen forward path (intent.rs): Proposed -> Approved -> Reserved ->
        // Built -> Simulated -> Signed -> Submitted -> Confirmed.
        assert!(can_transition(ExecutionState::Proposed, ExecutionState::Approved));
        assert!(can_transition(ExecutionState::Approved, ExecutionState::Reserved));
        assert!(can_transition(ExecutionState::Reserved, ExecutionState::Built));
        assert!(can_transition(ExecutionState::Signed, ExecutionState::Submitted));
        assert!(can_transition(ExecutionState::Submitted, ExecutionState::Confirmed));
        // REV-005-F01: state skip is illegal.
        assert!(!can_transition(ExecutionState::Proposed, ExecutionState::Submitted));
        assert!(!can_transition(ExecutionState::Proposed, ExecutionState::Reserved));
        // Terminal states have no outgoing transition.
        assert!(!can_transition(ExecutionState::Confirmed, ExecutionState::Proposed));
        assert!(!can_transition(ExecutionState::Cancelled, ExecutionState::Confirmed));
    }

    #[test]
    fn reconciliation_allows_only_three_outcomes() {
        // Frozen: UNKNOWN_RECONCILIATION -> CONFIRMED | FAILED_SAFE | CANCELLED.
        assert!(can_transition(ExecutionState::Submitted, ExecutionState::UnknownReconciliation));
        assert!(can_transition(ExecutionState::UnknownReconciliation, ExecutionState::Confirmed));
        assert!(can_transition(ExecutionState::UnknownReconciliation, ExecutionState::FailedSafe));
        assert!(can_transition(ExecutionState::UnknownReconciliation, ExecutionState::Cancelled));
        // No new submission while UNKNOWN.
        assert!(!can_transition(ExecutionState::UnknownReconciliation, ExecutionState::Submitted));
        assert!(!can_transition(ExecutionState::UnknownReconciliation, ExecutionState::Proposed));
    }

    #[test]
    fn halt_blocks_everything_exit_only_allows_reduce() {
        let halt = vec![ks(KillSwitchMode::Halt, true)];
        assert!(!action_permitted(&halt, TradeAction::Buy));
        assert!(!action_permitted(&halt, TradeAction::Sell)); // Halt blocks even reduce

        let exit_only = vec![ks(KillSwitchMode::ExitOnly, true)];
        assert!(!action_permitted(&exit_only, TradeAction::Buy));
        assert!(action_permitted(&exit_only, TradeAction::Close));
        assert!(action_permitted(&exit_only, TradeAction::EmergencyExit));
    }

    #[test]
    fn no_active_switch_permits_all() {
        assert!(action_permitted(&[], TradeAction::Buy));
        assert!(action_permitted(&[ks(KillSwitchMode::Halt, false)], TradeAction::Buy));
    }

    #[test]
    fn signer_policy_is_closed_checklist() {
        let pass = SignerPolicy {
            chain_id: Some("solana".into()), chain_genesis: Some("genesis".into()),
            policy_active: true, policy_not_expired: true, policy_not_halted: true,
            intent_hash_valid: true, nonce_valid: true, router_allowed: true,
            program_allowed: true, function_selector: Some("0x...".into()),
            token_pair_verified: true, recipient_verified: true,
            max_native_debit: Some("1".into()), max_token_debit: Some("1".into()),
            min_output: Some("0".into()),
            slippage_ok: true, price_impact_ok: true, deadline_ok: true, simulation_delta_ok: true,
        };
        assert!(signer_policy_passes(&pass));

        let fail = SignerPolicy { policy_active: false, ..pass.clone() };
        assert!(!signer_policy_passes(&fail));
        let missing_output = SignerPolicy { min_output: None, ..pass.clone() };
        // REV-009 addendum #1: empty string must also fail.
        let empty_chain = SignerPolicy { chain_id: Some("".into()), ..pass.clone() };
        assert!(!signer_policy_passes(&empty_chain));
    }

    // REV-007-F07: unknown/arbitrary action strings must be rejected.
    #[test]
    fn parse_action_rejects_unknown() {
        assert_eq!(parse_action("buy"), Some(Action::Trade(TradeAction::Buy)));
        assert_eq!(parse_action("EMERGENCY_EXIT"), Some(Action::Trade(TradeAction::EmergencyExit)));
        assert_eq!(parse_action("claim_fees"), Some(Action::Lp(LpAction::ClaimFees)));
        assert_eq!(parse_action("arbitrary_calldata"), None);
        assert_eq!(parse_action("wallet_sweep"), None);
        assert_eq!(parse_action(""), None);
    }
}
