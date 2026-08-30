//! Cabal/funding graph and revival intelligence (Phase 3).
//!
//! Canonical source: PLAN SWI §8.7 (Cabal/Funding Graph) and §8.8 (Revival
//! Intelligence).

use serde::{Deserialize, Serialize};

/// A cabal/funding graph edge (doc §8.7). Evidence/confidence per edge; false
/// confluence detection is explicit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundingEdge {
    pub from_wallet: String,
    pub to_wallet: String,
    pub relation: FundingRelation,
    pub evidence_ref: Option<String>,
    pub confidence: Option<f64>,
    pub false_confluence_risk: bool, // false confluence detection flag
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FundingRelation {
    FundedBy,
    SynchronizedEntry,
    SynchronizedExit,
    CommonDeployer,
    CommonAuthority,
    SharedCounterparty,
    CorrelatedCluster,
}

/// Revival intelligence flow (doc §8.8):
/// global trade/social wake -> dormant baseline comparison -> cheap activation
/// gate -> narrative/caller/wallet refresh -> revival quality -> full
/// opportunity evaluation. Prior history + failure memory stay attached.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RevivalStage {
    Wake,
    DormantBaselineComparison,
    ActivationGate,
    Refresh,
    RevivalQuality,
    OpportunityEvaluation,
}

/// A dormant baseline (doc §8.8 + §8.2 dormant/dead tokens not polled).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DormantBaseline {
    pub token: String,
    pub dormant_at: String,
    pub baseline: serde_json::Value, // compact dormant snapshot
    pub failure_memory: Vec<String>, // prior failure memory stays attached
}

/// A revival result (doc §8.8).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevivalResult {
    pub token: String,
    pub reached_stage: RevivalStage,
    pub revival_quality: Option<f64>,
    pub passed_activation_gate: bool,
    pub evidence_refs: Vec<String>,
}
