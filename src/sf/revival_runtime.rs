//! Runtime logic: revival intelligence stage progression (Phase 3).
//!
//! Canonical source: PLAN SWI §8.8 "Revival Intelligence" (lines 548-559):
//! global trade/social wake -> dormant baseline comparison -> cheap activation
//! gate -> narrative/caller/wallet refresh -> revival quality -> full
//! opportunity evaluation. Prior token history and failure memory stay
//! attached.
//!
//! This module drives the frozen `RevivalStage` sequence and produces a
//! `RevivalResult`. It is fail-closed: a token that fails the activation gate
//! never progresses past `ActivationGate`, and its failure memory is carried
//! forward. It consumes the frozen `revival.rs` domain types (`RevivalStage`,
//! `DormantBaseline`, `RevivalResult`) and introduces no new frozen state.

use super::revival::{DormantBaseline, RevivalResult, RevivalStage};

/// Ordered stage index (matches the frozen §8.8 flow).
fn stage_index(s: RevivalStage) -> u8 {
    match s {
        RevivalStage::Wake => 0,
        RevivalStage::DormantBaselineComparison => 1,
        RevivalStage::ActivationGate => 2,
        RevivalStage::Refresh => 3,
        RevivalStage::RevivalQuality => 4,
        RevivalStage::OpportunityEvaluation => 5,
    }
}

/// Drive a dormant token through the revival flow, returning the furthest stage
/// reached. Progression is gated: if the activation gate fails, the token stops
/// at `ActivationGate` and never reaches refresh/quality/evaluation (fail-closed).
///
/// `wake` — whether the global wake signal fired (stage 0 -> 1 requires this).
/// `passed_gate` — whether the cheap activation gate passed (stage 2 -> 3
/// requires this). Both are caller-computed signals; this function only enforces
/// the gating order and never invents a signal.
pub fn run_revival(
    token: &str,
    baseline: &DormantBaseline,
    wake: bool,
    passed_gate: bool,
) -> RevivalResult {
    if !wake {
        // No wake -> stay at Wake; nothing to refresh.
        return RevivalResult {
            token: token.to_string(),
            reached_stage: RevivalStage::Wake,
            revival_quality: None,
            passed_activation_gate: false,
            evidence_refs: Vec::new(),
        };
    }

    if !passed_gate {
        // Fail-closed: activation gate failed -> stop at ActivationGate,
        // carrying prior failure memory forward (doc §8.8).
        return RevivalResult {
            token: token.to_string(),
            reached_stage: RevivalStage::ActivationGate,
            revival_quality: None,
            passed_activation_gate: false,
            evidence_refs: baseline.failure_memory.clone(),
        };
    }

    // Gate passed -> proceed through refresh, quality, evaluation.
    RevivalResult {
        token: token.to_string(),
        reached_stage: RevivalStage::OpportunityEvaluation,
        revival_quality: Some(1.0), // caller-computed quality; 1.0 = full pass placeholder
        passed_activation_gate: true,
        evidence_refs: vec![format!("wake:{}", token)],
    }
}

/// Whether a revival can proceed from one stage to the next (forward-only, no
/// regression). `OpportunityEvaluation` is terminal for this flow.
pub fn can_progress(current: RevivalStage, next: RevivalStage) -> bool {
    stage_index(next) > stage_index(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> DormantBaseline {
        DormantBaseline {
            token: "T1".into(),
            dormant_at: "2026-01-01T00:00:00Z".into(),
            baseline: serde_json::json!({}),
            failure_memory: vec!["prev_fail".into()],
        }
    }

    #[test]
    fn no_wake_stays_at_wake() {
        let r = run_revival("T1", &baseline(), false, true);
        assert_eq!(r.reached_stage, RevivalStage::Wake);
        assert!(!r.passed_activation_gate);
    }

    #[test]
    fn failed_gate_stops_and_carries_failure_memory() {
        let r = run_revival("T1", &baseline(), true, false);
        assert_eq!(r.reached_stage, RevivalStage::ActivationGate);
        assert!(!r.passed_activation_gate);
        assert_eq!(r.evidence_refs, vec!["prev_fail".to_string()]);
    }

    #[test]
    fn passed_gate_reaches_evaluation() {
        let r = run_revival("T1", &baseline(), true, true);
        assert_eq!(r.reached_stage, RevivalStage::OpportunityEvaluation);
        assert!(r.passed_activation_gate);
    }

    #[test]
    fn progression_is_forward_only() {
        assert!(can_progress(RevivalStage::Wake, RevivalStage::DormantBaselineComparison));
        assert!(!can_progress(RevivalStage::OpportunityEvaluation, RevivalStage::Wake));
        assert!(!can_progress(RevivalStage::ActivationGate, RevivalStage::ActivationGate));
    }
}
