//! Intent domain: the immutable intent all actions flow through.
//!
//! Canonical source: PLAN SWI §16 "Execution state machine" (lines 1024-1059)
//! and principle #9 (no direct source-to-signer path).
//!
//! ## Frozen transition table (blocker #1 resolved)
//! Legal forward path:
//!   PROPOSED -> APPROVED -> RESERVED -> BUILT -> SIMULATED -> SIGNED -> SUBMITTED -> CONFIRMED
//! Reject/cancel:
//!   PROPOSED -> CANCELLED
//!   APPROVED -> CANCELLED
//! Fail-closed (release reservation):
//!   RESERVED | BUILT | SIMULATED | SIGNED | SUBMITTED -> FAILED_SAFE
//! Reconciliation (principle #10 — reconciliation before retry):
//!   SUBMITTED -> UNKNOWN_RECONCILIATION
//!   UNKNOWN_RECONCILIATION -> CONFIRMED   (recon LANDED)
//!   UNKNOWN_RECONCILIATION -> FAILED_SAFE (recon NOT_LANDED)
//!   UNKNOWN_RECONCILIATION -> CANCELLED   (escalation after T)
//! Illegal: any backward transition, any state skip, any retry from a terminal
//! state, and SUBMITTED from UNKNOWN_RECONCILIATION (no new submission while
//! UNKNOWN — §16).

use serde::{Deserialize, Serialize};

/// Intent states (doc §16). Frozen list.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentState {
    Proposed,
    Approved,
    Reserved,
    Built,
    Simulated,
    Signed,
    Submitted,
    Confirmed,
    FailedSafe,
    UnknownReconciliation,
    Cancelled,
}

impl IntentState {
    /// Whether `next` is a legal transition from `self` (frozen table).
    pub fn can_transition_to(self, next: IntentState) -> bool {
        use IntentState::*;
        matches!(
            (self, next),
            // forward path
            (Proposed, Approved)
                | (Approved, Reserved)
                | (Reserved, Built)
                | (Built, Simulated)
                | (Simulated, Signed)
                | (Signed, Submitted)
                | (Submitted, Confirmed)
                // reject/cancel
                | (Proposed, Cancelled)
                | (Approved, Cancelled)
                // fail-closed
                | (Reserved, FailedSafe)
                | (Built, FailedSafe)
                | (Simulated, FailedSafe)
                | (Signed, FailedSafe)
                | (Submitted, FailedSafe)
                // reconciliation
                | (Submitted, UnknownReconciliation)
                | (UnknownReconciliation, Confirmed)
                | (UnknownReconciliation, FailedSafe)
                | (UnknownReconciliation, Cancelled)
        )
    }

    /// Terminal states: no legal outgoing transition.
    pub fn is_terminal(self) -> bool {
        matches!(self, IntentState::Confirmed | IntentState::FailedSafe | IntentState::Cancelled)
    }
}

/// An execution intent (principle #9: no action bypasses an immutable intent).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Intent {
    pub intent_hash: String,
    pub chain: String,
    pub wallet: String,
    pub action: String,
    pub target_entity: String,
    pub policy_version: String,
    pub strategy_version: Option<String>,
    pub source_event_id: Option<String>,
    pub nonce: Option<String>,
    pub state: IntentState,
}

/// Reconciliation outcome (doc §16). No new submission while `Unknown`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationResult {
    Landed,
    NotLanded,
    Unknown,
}

/// Reservation release rule (frozen blocker #1). Defaults:
/// - expiry: configurable per chain, default 120s (RESERVED..SIGNED).
/// - UNKNOWN_RECONCILIATION holds reservation; escalation after 10m -> CANCELLED
///   (no auto-release until recon is definite).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReservationAction {
    Hold,    // keep reserved (forward path + UNKNOWN)
    Release, // release capital (CONFIRMED consumed / FAILED_SAFE / CANCELLED / expiry)
}

/// Derive the reservation action for a transition (frozen rules).
pub fn reservation_action(to: IntentState) -> ReservationAction {
    use IntentState::*;
    match to {
        Confirmed | FailedSafe | Cancelled => ReservationAction::Release,
        UnknownReconciliation => ReservationAction::Hold,
        // forward path + pre-submit states keep the reservation held
        Proposed | Approved | Reserved | Built | Simulated | Signed | Submitted => {
            ReservationAction::Hold
        }
    }
}
