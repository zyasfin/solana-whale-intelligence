//! Runtime logic: caller intelligence aggregation (Phase 2).
//!
//! Canonical source: PLAN SWI §8.5 "Caller Intelligence" (lines 502-511):
//! Telegram MTProto caller truth, CA resolution, immutable T0 snapshot, call
//! timestamp + lead time, MFE/MAE, realistic copy entry/PnL, outcome windows
//! through +21d, caller reputation by regime and sample confidence, propagation
//! and copy-caller graph.
//!
//! This module aggregates `Call`s into objective caller metrics (hit rate,
//! average MFE) and builds reputation scoped to a market regime. It consumes
//! the frozen `caller.rs` domain types (`Call`, `CallOutcome`,
//! `CallerReputation`, `CallerPropagationEdge`) and introduces no new frozen
//! state. Outcome windows are clamped to the frozen +21d cap.

use super::caller::{Call, CallerReputation};

/// The frozen maximum outcome window in days (doc §8.5 "+21d").
pub const MAX_OUTCOME_WINDOW_DAYS: u32 = 21;

/// Clamp a requested outcome window to the frozen +21d cap (fail-closed: never
/// allow an unbounded window).
pub fn clamp_window_days(days: u32) -> u32 {
    days.min(MAX_OUTCOME_WINDOW_DAYS)
}

/// Compute a caller's hit rate: the fraction of calls that realized a positive
/// copy PnL, over calls that have an outcome. Returns `None` when there are no
/// calls with a resolved outcome (no fabricated hit rate).
pub fn hit_rate(calls: &[Call]) -> Option<f64> {
    let resolved: Vec<&Call> = calls
        .iter()
        .filter(|c| c.outcome.is_some())
        .collect();
    if resolved.is_empty() {
        return None;
    }
    let hits = resolved
        .iter()
        .filter(|c| c.outcome.as_ref().unwrap().copy_pnl.unwrap_or(0.0) > 0.0)
        .count() as f64;
    Some(hits / resolved.len() as f64)
}

/// Average MFE across resolved calls (None when no resolved outcome).
pub fn average_mfe(calls: &[Call]) -> Option<f64> {
    let mfes: Vec<f64> = calls
        .iter()
        .filter_map(|c| c.outcome.as_ref().and_then(|o| o.mfe))
        .collect();
    if mfes.is_empty() {
        return None;
    }
    Some(mfes.iter().sum::<f64>() / mfes.len() as f64)
}

/// Build a caller reputation scoped to a regime, with sample confidence derived
/// from sample size (a simple monotonically increasing confidence curve; the
/// exact curve is a frozen-decision candidate — see CONVENTIONS.md §4).
///
/// `regime` is the market regime the reputation is scoped to. `sample_confidence`
/// is `1 - 1/(sample_size+1)` so it is 0 at n=0 and approaches 1 asymptotically,
/// never reaching a fabricated 1.0.
pub fn build_reputation(caller_id: &str, regime: &str, calls: &[Call]) -> CallerReputation {
    let hr = hit_rate(calls);
    let amfe = average_mfe(calls);
    let n = calls.len() as u32;
    let confidence = 1.0 - 1.0 / (n as f64 + 1.0);

    CallerReputation {
        caller_id: caller_id.to_string(),
        regime: regime.to_string(),
        sample_size: n,
        sample_confidence: Some(confidence),
        hit_rate: hr,
        avg_mfe: amfe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::caller::{CallerPlatform, CallOutcome};

    fn call(copy_pnl: Option<f64>, mfe: Option<f64>) -> Call {
        Call {
            caller_id: "c1".into(),
            platform: CallerPlatform::Telegram,
            token: "TOKEN".into(),
            occurred_at: "2026-01-01T00:00:00Z".into(),
            lead_time: None,
            immutable_t0_snapshot: "hash".into(),
            outcome: copy_pnl.map(|pnl| CallOutcome {
                mfe,
                mae: None,
                copy_entry: None,
                copy_pnl: Some(pnl),
                outcome_window_days: 21,
            }),
        }
    }

    #[test]
    fn hit_rate_counts_positive_copy_pnl() {
        let calls = vec![
            call(Some(10.0), Some(20.0)),  // hit
            call(Some(-5.0), Some(5.0)),   // miss
            call(None, None),              // unresolved -> excluded
        ];
        let hr = hit_rate(&calls).unwrap();
        assert!((hr - 0.5).abs() < 1e-9); // 1 hit / 2 resolved
    }

    #[test]
    fn hit_rate_none_when_no_resolved() {
        let calls = vec![call(None, None), call(None, None)];
        assert_eq!(hit_rate(&calls), None);
    }

    #[test]
    fn average_mfe_sums_resolved_only() {
        let calls = vec![call(Some(10.0), Some(20.0)), call(None, None), call(Some(-1.0), Some(10.0))];
        let mfe = average_mfe(&calls).unwrap();
        assert!((mfe - 15.0).abs() < 1e-9); // (20 + 10) / 2
    }

    #[test]
    fn window_is_clamped_to_21() {
        assert_eq!(clamp_window_days(5), 5);
        assert_eq!(clamp_window_days(21), 21);
        assert_eq!(clamp_window_days(100), 21);
    }

    #[test]
    fn reputation_has_monotonic_confidence() {
        let calls = vec![call(Some(10.0), Some(20.0)), call(Some(5.0), Some(10.0))];
        let rep = build_reputation("c1", "bull", &calls);
        assert_eq!(rep.sample_size, 2);
        assert!(rep.sample_confidence.unwrap() > 0.0);
        assert!(rep.sample_confidence.unwrap() < 1.0);
        assert_eq!(rep.hit_rate, Some(1.0)); // both positive
    }
}
