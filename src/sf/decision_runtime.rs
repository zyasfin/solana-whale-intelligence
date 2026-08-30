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

/// Evaluate a decision bundle:
/// - A missing capability always blocks (fail-closed).
/// - Any mandatory component with `pass == false` (or `None`, i.e. not resolved)
///   -> Reject (mandatory gate, §12.2).
/// - Otherwise -> Approve.
///
/// `pass == None` on a mandatory component is treated as failure (fail-closed);
/// `None` on a non-mandatory component is tolerated.
pub fn evaluate(bundle: &DecisionBundle) -> Disposition {
    if !bundle.missing_capabilities.is_empty() {
        return Disposition::MissingCapability;
    }

    for c in &bundle.component_results {
        if c.component_class == ComponentClass::Mandatory {
            match c.pass {
                Some(true) => {} // mandatory passed
                Some(false) | None => return Disposition::Reject,
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

/// Count missing mandatory components (those that are Mandatory and unresolved).
/// Useful for the "missingness" metric (doc §20 metrics).
pub fn unresolved_mandatory(bundle: &DecisionBundle) -> usize {
    bundle
        .component_results
        .iter()
        .filter(|c| c.component_class == ComponentClass::Mandatory && c.pass != Some(true))
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
            component_class: ComponentClass::Mandatory,
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
    fn all_mandatory_pass_approves() {
        let b = bundle(vec![], vec![mandatory("gate1", Some(true))], vec!["e1".into()]);
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
