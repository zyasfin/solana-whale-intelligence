//! Graph domain: entity nodes and edges.
//!
//! Canonical source: PLAN SWI §9 "Evidence and graph model" (lines 614-622).

use serde::{Deserialize, Serialize};

/// Core node types (doc §9). Closed set reflecting the frozen node taxonomy.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Token,
    TokenFamily,
    Wallet,
    WalletCluster,
    Caller,
    Post,
    Narrative,
    Pool,
    Position,
    Strategy,
    Decision,
    Intent,
    Execution,
}

/// Edge types (doc §9 edge examples).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    DeployedBy,
    FundedBy,
    CalledBy,
    AmplifiedBy,
    DerivedFrom,
    OfficiallyAdoptedBy,
    FirstLiquidOn,
    Holds,
    Swapped,
    LpProvided,
    SameFamilyAs,
    EvidencedBy,
    ResultedIn,
}

/// An entity node. Identity is the chain-qualified entity key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityNode {
    pub entity_key: String,
    pub node_type: NodeType,
}

/// An entity edge. Every edge stores time, source, truth status, confidence,
/// evidence refs, valid window, and supersession status (doc line 622-623).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityEdge {
    pub from_entity_key: String,
    pub to_entity_key: String,
    pub edge_type: EdgeType,
    pub occurred_at: String,
    pub source_id: Option<String>,
    pub truth_status: super::core::TruthStatus,
    pub confidence: Option<f64>,
    pub valid_from: String,
    pub valid_until: Option<String>,
    pub supersedes: Option<String>,
    pub evidence_refs: Vec<String>,
}
