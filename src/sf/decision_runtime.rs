//! Runtime logic: decision component gating and disposition (Phase 0, §12).
//!
//! Canonical source: PLAN SWI §12 "Decision architecture" (lines 800-853):
//! decision bundles are immutable point-in-time snapshots; mandatory components
//! gate execution (fail-closed); missing capability blocks; every reject/suppress
//! has a reason and later shadow outcome where feasible. N/A, missing, zero, and
//! safe are DISTINCT (principle #3).
//!
//! This module evaluates a `DecisionBundle`'s component results into a final
//! disposition. It consumes the frozen `decision.rs` types (`DecisionBundle`,
//! `ComponentResult`, `ComponentClass`) and introduces no new frozen state.

use super::decision::{ComponentClass, DecisionBundle};

/// Final disposition of a decision evaluation (derived, not frozen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Approve,
    Reject,
    MissingCapability,
    InsufficientEvidence,
}

/// Evaluate a decision bundle (fail-closed):
/// - No evidence snapshot -> InsufficientEvidence (gate #2: reproducible only
///   from an immutable point-in-time bundle).
/// - A missing capability -> MissingCapability.
/// - Any MANDATORY_PASS component not `pass == Some(true)` -> Reject (fail-closed).
/// - Any HALT_INPUT component with `pass == Some(true)` (a halt signal fired)
///   -> Reject (source degradation/drawdown/reconciliation incident halt).
/// - Otherwise -> Approve.
///
/// SIZING_INPUT and STRATEGY_INPUT are informative only (do not gate).
pub fn evaluate(bundle: &DecisionBundle) -> Disposition {
    // REV-007-F04: no evidence -> insufficient (never approve on empty bundle).
    if bundle.evidence_snapshot_ids.is_empty() {
        return Disposition::InsufficientEvidence;
    }

    // REV-009-F04: the target action must be a known, closed action — an
    // arbitrary/unknown action string must be rejected before persistence.
    if super::execution_runtime::parse_action(&bundle.target_action).is_none() {
        return Disposition::Reject;
    }

    if !bundle.missing_capabilities.is_empty() {
        return Disposition::MissingCapability;
    }
    // REV-009-F01: a decision with NO mandatory component at all is incomplete —
    // there is nothing gating execution. Reject (fail-closed).
    let has_mandatory = bundle
        .component_results
        .iter()
        .any(|c| c.component_class == ComponentClass::MandatoryPass);
    if !has_mandatory {
        return Disposition::Reject;
    }

    for c in &bundle.component_results {
        match c.component_class {
            ComponentClass::MandatoryPass => {
                if c.pass != Some(true) {
                    return Disposition::Reject;
                }
            }
            ComponentClass::HaltInput => {
                // A halt input that fires (pass == true) halts the decision.
                if c.pass == Some(true) {
                    return Disposition::Reject;
                }
            }
            ComponentClass::SizingInput | ComponentClass::StrategyInput => {
                // Informative only — no gating.
            }
        }
    }

    Disposition::Approve
}

/// Whether a decision bundle is reproducible (gate #2): it must carry at least
/// one evidence snapshot id. A bundle with no evidence is not reproducible.
pub fn is_reproducible(bundle: &DecisionBundle) -> bool {
    !bundle.evidence_snapshot_ids.is_empty()
}

/// Count unresolved MANDATORY_PASS components (those not `pass == Some(true)`).
/// Useful for the "missingness" metric (doc §20 metrics).
pub fn unresolved_mandatory(bundle: &DecisionBundle) -> usize {
    bundle
        .component_results
        .iter()
        .filter(|c| c.component_class == ComponentClass::MandatoryPass && c.pass != Some(true))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::core::TruthStatus;
    use super::super::decision::ComponentResult;

    fn bundle(
        missing: Vec<String>,
        components: Vec<ComponentResult>,
        evidence: Vec<String>,
    ) -> DecisionBundle {
        DecisionBundle {
            target_entity: "T".into(),
            target_action: "BUY".into(),
            decision_at: "2026-01-01T00:00:00Z".into(),
            evidence_snapshot_ids: evidence,
            component_results: components,
            missing_capabilities: missing,
            source_freshness: serde_json::json!({}),
            confidence: None,
            truth_status: TruthStatus::Unknown,
            strategy_version: None,
            rule_version: None,
            policy_version: None,
            alternatives: vec![],
            final_disposition: "".into(),
        }
    }

    fn mandatory(name: &str, pass: Option<bool>) -> ComponentResult {
        ComponentResult {
            component_class: ComponentClass::MandatoryPass,
            component_name: name.into(),
            pass,
            result: serde_json::json!({}),
        }
    }

    fn halt(name: &str, pass: Option<bool>) -> ComponentResult {
        ComponentResult {
            component_class: ComponentClass::HaltInput,
            component_name: name.into(),
            pass,
            result: serde_json::json!({}),
        }
    }

    #[test]
    fn missing_capability_blocks() {
        let b = bundle(vec!["provider".into()], vec![], vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::MissingCapability);
    }

    #[test]
    fn empty_evidence_is_insufficient() {
        // REV-007-F04: no evidence -> InsufficientEvidence, never Approve.
        let b = bundle(vec![], vec![mandatory("gate1", Some(true))], vec![]);
        assert_eq!(evaluate(&b), Disposition::InsufficientEvidence);
    }

    #[test]
    fn mandatory_fail_rejects() {
        let b = bundle(vec![], vec![mandatory("gate1", Some(false))], vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Reject);
    }

    #[test]
    fn mandatory_unresolved_fails_closed() {
        let b = bundle(vec![], vec![mandatory("gate1", None)], vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Reject);
    }

    #[test]
    fn halt_input_firing_rejects() {
        // REV-007-F03: a HALT_INPUT that fires (pass == true) halts the decision.
        let b = bundle(vec![], vec![mandatory("gate1", Some(true)), halt("drawdown", Some(true))], vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Reject);
    }

    // REV-009-F01: evidence present but zero mandatory components -> Reject.
    #[test]
    fn no_mandatory_component_rejects() {
        let b = bundle(vec![], vec![], vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Reject);
    }

    // REV-009-F04: an unknown target_action must be rejected.
    #[test]
    fn unknown_action_rejects() {
        let mut b = bundle(vec![], vec![mandatory("gate1", Some(true))], vec!["e1".into()]);
        b.target_action = "arbitrary_calldata".into();
        assert_eq!(evaluate(&b), Disposition::Reject);
    }

    #[test]
    fn all_mandatory_pass_approves() {
        let b = bundle(vec![], vec![mandatory("gate1", Some(true)), halt("drawdown", Some(false))], vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Approve);
    }

    #[test]
    fn reproducible_requires_evidence() {
        assert!(is_reproducible(&bundle(vec![], vec![], vec!["e1".into()])));
        assert!(!is_reproducible(&bundle(vec![], vec![], vec![])));
    }

    #[test]
    fn unresolved_mandatory_count() {
        let b = bundle(vec![], vec![mandatory("a", Some(true)), mandatory("b", None)], vec!["e1".into()]);
        assert_eq!(unresolved_mandatory(&b), 1);
    }
}
