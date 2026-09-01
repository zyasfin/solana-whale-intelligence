//! Graph domain: entity nodes and edges.
//!
//! Canonical source: PLAN SWI §9 "Evidence and graph model" (lines 614-622).

use serde::{Deserialize, Serialize};

/// Core node types (doc §9). Closed set reflecting the frozen node taxonomy.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Token,
    Contract, // synonym of Token (doc §9 "Token/Contract")
    TokenFamily,
    Wallet,
    WalletCluster,
    Caller,
    SourceAccount, // synonym of Caller (doc §9 "Caller/SourceAccount")
    Post,
    Message,       // synonym of Post (doc §9 "Post/Message/ExternalEvent")
    ExternalEvent, // synonym of Post
    Narrative,
    Pool,
    Position,
    Strategy,
    Policy, // synonym of Strategy (doc §9 "Strategy/Policy")
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
    // REV-020 recent-intelligence relations (graph-visible).
    SameDeployer,
    SameAuthority,
    SameFeePayer,
    SameFunder,
    SameSocialAccount,
    ReusedSocialLink,
    OfficialCaAnnouncement,
    CrossChainDeployment,
    SuspectedCopycat,
    LiquidityAttentionRotatedTo,
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
