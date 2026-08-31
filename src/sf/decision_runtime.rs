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

use super::decision::{ComponentClass, ComponentResult, DecisionBundle};

/// Final disposition of a decision evaluation (derived, not frozen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Approve,
    Reject,
    MissingCapability,
    InsufficientEvidence,
}
/// Frozen universal mandatory minimum (PLAN §12.2, REV-011-F01). A decision may
/// only approve when ALL four are present (exactly one each) and pass.
pub const REQUIRED_MANDATORY: [&str; 4] = [
    "security",
    "contract_identity",
    "chain_state",
    "mandatory_freshness",
];

/// Evaluate a decision bundle (fail-closed):
/// - No evidence snapshot -> InsufficientEvidence.
/// - Unknown action -> Reject.
/// - Missing capability -> MissingCapability.
/// - Not all four REQUIRED_MANDATORY present & passing -> Reject.
/// - A HALT_INPUT that fires -> Reject.
/// - Otherwise -> Approve.
pub fn evaluate(bundle: &DecisionBundle) -> Disposition {
    if bundle.evidence_snapshot_ids.is_empty() {
        return Disposition::InsufficientEvidence;
    }

    // REV-011-F01: source_freshness must be a non-empty object.
    if bundle.source_freshness.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return Disposition::InsufficientEvidence;
    }

    // (target_action is now the canonical `Action` type — no string parsing
    // needed; REV-011-F04.)

    if !bundle.missing_capabilities.is_empty() {
        return Disposition::MissingCapability;
    }

    // REV-011-F01: each of the four REQUIRED_MANDATORY names must appear exactly
    // once as a MandatoryPass component, all passing.
    for required in REQUIRED_MANDATORY {
        let matching: Vec<&ComponentResult> = bundle
            .component_results
            .iter()
            .filter(|c| {
                c.component_class == ComponentClass::MandatoryPass
                    && c.component_name == required
            })
            .collect();
        if matching.len() != 1 || matching[0].pass != Some(true) {
            return Disposition::Reject;
        }
    }

    for c in &bundle.component_results {
        match c.component_class {
            ComponentClass::MandatoryPass => {
                if c.pass != Some(true) {
                    return Disposition::Reject;
                }
            }
            ComponentClass::HaltInput => {
                if c.pass == Some(true) {
                    return Disposition::Reject;
                }
            }
            ComponentClass::SizingInput | ComponentClass::StrategyInput => {}
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
            target_action: super::super::execution::Action::Trade(super::super::execution::TradeAction::Buy),
            decision_at: "2026-01-01T00:00:00Z".into(),
            evidence_snapshot_ids: evidence,
            component_results: components,
            missing_capabilities: missing,
            source_freshness: serde_json::json!({"fresh": true}),
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

    // All four REQUIRED_MANDATORY, all passing.
    fn required() -> Vec<ComponentResult> {
        REQUIRED_MANDATORY
            .iter()
            .map(|n| mandatory(n, Some(true)))
            .collect()
    }

    #[test]
    fn missing_capability_blocks() {
        let b = bundle(vec!["provider".into()], required(), vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::MissingCapability);
    }

    #[test]
    fn empty_evidence_is_insufficient() {
        let b = bundle(vec![], required(), vec![]);
        assert_eq!(evaluate(&b), Disposition::InsufficientEvidence);
    }

    // REV-011-F01: a single fake mandatory + empty freshness -> not approve.
    #[test]
    fn fake_mandatory_and_empty_freshness_rejects() {
        let mut b = bundle(vec![], vec![mandatory("fake", Some(true))], vec!["e1".into()]);
        b.source_freshness = serde_json::json!({});
        assert_eq!(evaluate(&b), Disposition::InsufficientEvidence);
    }

    #[test]
    fn missing_required_mandatory_rejects() {
        // Only 3 of 4 required names present.
        let comps = vec![
            mandatory("security", Some(true)),
            mandatory("contract_identity", Some(true)),
            mandatory("chain_state", Some(true)),
        ];
        let b = bundle(vec![], comps, vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Reject);
    }

    #[test]
    fn halt_input_firing_rejects() {
        let mut comps = required();
        comps.push(halt("drawdown", Some(true)));
        let b = bundle(vec![], comps, vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Reject);
    }

    #[test]
    fn all_required_pass_approves() {
        let mut comps = required();
        comps.push(halt("drawdown", Some(false)));
        let b = bundle(vec![], comps, vec!["e1".into()]);
        assert_eq!(evaluate(&b), Disposition::Approve);
    }

    #[test]
    fn reproducible_requires_evidence() {
        assert!(is_reproducible(&bundle(vec![], required(), vec!["e1".into()])));
        assert!(!is_reproducible(&bundle(vec![], required(), vec![])));
    }

    #[test]
    fn unresolved_mandatory_count() {
        let comps = vec![
            mandatory("security", Some(true)),
            mandatory("contract_identity", None),
        ];
        let b = bundle(vec![], comps, vec!["e1".into()]);
        assert_eq!(unresolved_mandatory(&b), 1);
    }
}
