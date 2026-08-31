//! Decision domain: decision bundles and component classes.
//!
//! Canonical source: PLAN SWI §12 "Decision architecture" (lines 800-826).

use serde::{Deserialize, Serialize};

/// Component class (doc §12.2). Re-exported from `portfolio.rs` so there is ONE
/// canonical four-class taxonomy (REV-007-F03): mandatory/sizing/strategy/halt.
pub use super::portfolio::ComponentClass;

/// A decision bundle (doc §12.1): immutable point-in-time decision snapshot.
/// Every decision is reproducible from an immutable point-in-time bundle
/// (gate #2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionBundle {
    pub target_entity: String,
    pub target_action: String,
    pub decision_at: String,
    pub evidence_snapshot_ids: Vec<String>,
    pub component_results: Vec<ComponentResult>,
    pub missing_capabilities: Vec<String>,
    pub source_freshness: serde_json::Value,
    pub confidence: Option<f64>,
    pub truth_status: super::core::TruthStatus,
    pub strategy_version: Option<String>,
    pub rule_version: Option<String>,
    pub policy_version: Option<String>,
    pub alternatives: Vec<String>,
    pub final_disposition: String,
}

/// A single component result within a decision bundle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentResult {
    pub component_class: ComponentClass,
    pub component_name: String,
    pub pass: Option<bool>, // required for mandatory components
    pub result: serde_json::Value,
}
