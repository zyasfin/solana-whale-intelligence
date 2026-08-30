//! Runtime logic: token birth lifecycle state machine + wake gate (Phase 1).
//!
//! Canonical source: PLAN SWI §8.2 "Token Birth Lifecycle" (lines 429-445).
//! Frozen states: CREATED / PRE_GRADUATION / MIGRATED (GRADUATED) /
//! FIRST_LIQUIDITY / ACTIVE / COOLING / DORMANT / ARCHIVED / TOMBSTONED.
//! Mint creation, launchpad creation, first pool, migration, and first
//! meaningful liquidity remain separate timestamps.
//!
//! Key rule (doc line 445): dormant/dead tokens are NOT individually polled;
//! global feeds compare against compact dormant baselines; full enrichment only
//! after a wake gate. This module implements the lifecycle transition plus a
//! cheap wake-gate predicate. It consumes the frozen `token.rs::TokenLifecycle`
//! type and introduces no new frozen state.

use super::token::TokenLifecycle;

/// The lifecycle stage as an ordered index (for "can advance to" checks).
/// Order matches the frozen §8.2 list; ARCHIVED and TOMBSTONED are terminal.
fn stage_index(s: TokenLifecycle) -> u8 {
    match s {
        TokenLifecycle::Created => 0,
        TokenLifecycle::PreGraduation => 1,
        TokenLifecycle::Migrated => 2,
        TokenLifecycle::FirstLiquidity => 3,
        TokenLifecycle::Active => 4,
        TokenLifecycle::Cooling => 5,
        TokenLifecycle::Dormant => 6,
        TokenLifecycle::Archived => 7,
        TokenLifecycle::Tombstoned => 8,
    }
}

/// Whether a transition from `current` to `next` is a forward move (or a stay),
/// and never a regression. ARCHIVED and TOMBSTONED are terminal: once reached,
/// they cannot move again (archive-not-delete, principle #6).
///
/// Returns `true` when `next` is at the same or a later stage than `current`,
/// EXCEPT that terminal states never advance.
pub fn can_advance(current: TokenLifecycle, next: TokenLifecycle) -> bool {
    if current == TokenLifecycle::Archived || current == TokenLifecycle::Tombstoned {
        return false; // terminal — no further transitions
    }
    stage_index(next) >= stage_index(current)
}

/// Cheap activation (wake) gate for a dormant token (doc §8.8 wake gate + §8.2
/// "full enrichment only after wake gate"). Returns `true` when the dormant
/// baseline should be promoted to full enrichment, based on a global wake
/// signal that is strictly greater than the dormant baseline value.
///
/// `baseline_value` and `signal_value` are caller-provided comparable scalars
/// (e.g. trade count, volume). The gate is cheap and fail-closed: a missing or
/// non-increasing signal never wakes the token.
pub fn wake_gate(baseline_value: Option<f64>, signal_value: Option<f64>) -> bool {
    match (baseline_value, signal_value) {
        (Some(base), Some(sig)) => sig > base,
        _ => false, // missing data fails closed — no wake
    }
}

/// Advance a token's lifecycle, returning the new stage. Terminal states and
/// regressions are rejected by returning `current` unchanged (fail-closed).
pub fn advance(current: TokenLifecycle, next: TokenLifecycle) -> TokenLifecycle {
    if can_advance(current, next) {
        next
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_transitions_allowed() {
        assert!(can_advance(TokenLifecycle::Created, TokenLifecycle::PreGraduation));
        assert!(can_advance(TokenLifecycle::PreGraduation, TokenLifecycle::Migrated));
        assert!(can_advance(TokenLifecycle::Migrated, TokenLifecycle::FirstLiquidity));
        assert!(can_advance(TokenLifecycle::FirstLiquidity, TokenLifecycle::Active));
        assert!(can_advance(TokenLifecycle::Active, TokenLifecycle::Cooling));
        assert!(can_advance(TokenLifecycle::Cooling, TokenLifecycle::Dormant));
    }

    #[test]
    fn regression_rejected() {
        assert!(!can_advance(TokenLifecycle::Active, TokenLifecycle::Created));
        assert!(!can_advance(TokenLifecycle::Dormant, TokenLifecycle::FirstLiquidity));
    }

    #[test]
    fn terminal_states_never_advance() {
        assert!(!can_advance(TokenLifecycle::Archived, TokenLifecycle::Tombstoned));
        assert!(!can_advance(TokenLifecycle::Tombstoned, TokenLifecycle::Active));
        // Archived -> Archived is also rejected (terminal).
        assert!(!can_advance(TokenLifecycle::Archived, TokenLifecycle::Archived));
    }

    #[test]
    fn advance_applies_or_rejects() {
        assert_eq!(advance(TokenLifecycle::Created, TokenLifecycle::PreGraduation), TokenLifecycle::PreGraduation);
        // regression -> unchanged
        assert_eq!(advance(TokenLifecycle::Active, TokenLifecycle::Created), TokenLifecycle::Active);
    }

    #[test]
    fn wake_gate_is_fail_closed_and_strict() {
        // signal must be strictly greater than baseline.
        assert!(wake_gate(Some(10.0), Some(11.0)));
        assert!(!wake_gate(Some(10.0), Some(10.0)));
        assert!(!wake_gate(Some(10.0), Some(9.0)));
        // missing data fails closed.
        assert!(!wake_gate(None, Some(11.0)));
        assert!(!wake_gate(Some(10.0), None));
    }
}
