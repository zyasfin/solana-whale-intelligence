//! Wallet intelligence domain (Phase 1).
//!
//! Canonical source: PLAN SWI §8.6 (Wallet Intelligence): chain-qualified
//! address + optional entity cluster, exact swap reconstruction and cost basis,
//! early-entry timing, realized/unrealized outcome, recurrence across tokens.
//! Custom tags are assertions with source_type/truth_status/confidence/valid
//! window/status/supersedes_id. Manual assertions never silently overwritten.

use serde::{Deserialize, Serialize};

/// Source type of a tag assertion (doc §8.6).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TagSourceType {
    Manual,
    System,
    Import,
    Vendor,
    Inferred,
}

/// Status of a tag assertion (doc §8.6).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TagStatus {
    Active,
    Disputed,
    Expired,
    Revoked,
}

/// A custom wallet tag assertion (doc §8.6): `namespace:name`.
/// Manual assertions are never silently overwritten; supersedes_id chains history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletTagAssertion {
    pub namespace: String,
    pub name: String,
    pub source_type: TagSourceType,
    pub truth_status: crate::sf::core::TruthStatus,
    pub confidence: Option<f64>,
    pub valid_from: String,
    pub valid_until: Option<String>,
    pub status: TagStatus,
    pub supersedes_id: Option<String>,
}

/// Exact swap reconstruction + cost basis (doc §8.6).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Swap {
    pub chain: String,
    pub wallet: String,
    pub token: String,      // contract address
    pub direction: SwapDirection,
    pub amount_in: String,  // decimal string to preserve precision
    pub amount_out: String,
    pub timestamp: String,
    pub tx_hash: String,
}

impl Swap {
    /// Parse the RFC3339 timestamp into unix seconds (0 if unparseable/empty),
    /// used by FIFO matching for chronological ordering and hold-time.
    pub fn timestamp_secs(&self) -> Option<i64> {
        self.timestamp
            .parse::<chrono::DateTime<chrono::Utc>>()
            .ok()
            .map(|dt| dt.timestamp())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SwapDirection {
    Buy,
    Sell,
}

/// Wallet intelligence: chain-qualified address + optional cluster.
/// No single smart-wallet score (doc §8.6); objective-specific dimensions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletIntelligence {
    pub chain: String,
    pub address: String,
    pub cluster_id: Option<String>,
    pub cost_basis: Option<CostBasis>,
    pub tags: Vec<WalletTagAssertion>,
    pub realized_outcome: Option<String>,
    pub unrealized_outcome: Option<String>,
    pub recurrence: Option<Recurrence>,
}

/// Cost basis derived from exact swap reconstruction.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CostBasis {
    pub token: String,
    pub average_cost: String,
    pub realized_pnl: Option<String>,
}

/// Recurrence across tokens (doc §8.6).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recurrence {
    pub tokens_traded: u32,
    pub distinct_clusters: u32,
}
