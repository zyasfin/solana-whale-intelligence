//! Strategy Lab domain (Phase 5).
//!
//! Canonical source: PLAN SWI §13 "Strategy Lab and evaluation" (strategy
//! source/versioning, shadow outcomes, walk-forward/holdout, paper execution).

use serde::{Deserialize, Serialize};

/// Strategy lifecycle (doc §13): DRAFT -> SHADOW -> PAPER -> VALIDATED ->
/// APPROVED -> CANARY -> ACTIVE -> PAUSED -> RETIRED.
/// Activation follows shadow/paper/validation/approval (gate #14).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StrategyLifecycle {
    Draft,
    Shadow,
    Paper,
    Validated,
    Approved,
    Canary,
    Active,
    Paused,
    Retired,
}

/// A strategy source (doc §13): raw URL/content hash, author, claims/modules/
/// assumptions, required capabilities, chain/venue/regime scope. Editable copy
/// separated from immutable source.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrategySource {
    pub raw_url: Option<String>,
    pub content_hash: String,
    pub author: Option<String>,
    pub claims: Vec<String>,
    pub required_capabilities: Vec<String>,
    pub chain_scope: Vec<String>,
    pub regime_scope: Option<String>,
}

/// A strategy version: immutable versioned policy snapshot (reproducible, gate #2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrategyVersion {
    pub strategy_id: String,
    pub version: u32,
    pub policy: serde_json::Value,
    pub policy_hash: String,
    pub lifecycle: StrategyLifecycle,
}

/// Shadow outcome (no real capital; doc §13 evaluation requirement #1-14).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowOutcome {
    pub strategy_version_id: String,
    pub window_start: String,
    pub window_end: String,
    pub metrics: serde_json::Value,
    pub rejected_candidates: Vec<String>, // negative findings retained
}

/// Paper execution result with position-sized quote friction (doc §13 req #4).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperExecution {
    pub strategy_version_id: String,
    pub quote: String, // position-sized executable quote
    pub friction: PaperFriction,
    pub pnl: Option<String>,
}

/// Paper execution friction (doc §13 req #5): fees/gas/tip/rent/slippage/latency.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperFriction {
    pub fees: Option<String>,
    pub gas: Option<String>,
    pub tip: Option<String>,
    pub rent: Option<String>,
    pub slippage: Option<String>,
    pub latency: Option<String>,
}
