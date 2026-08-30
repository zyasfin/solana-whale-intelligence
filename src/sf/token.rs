//! Token intelligence domain (Phase 1).
//!
//! Canonical source: PLAN SWI §8.1-8.4 (Token Intelligence, Token Birth
//! Lifecycle, Token Family/Canonicality, Name/Meme Provenance).

use serde::{Deserialize, Serialize};

/// Token birth lifecycle (doc §8.2). Frozen state list.
/// Mint creation, launchpad creation, first pool, migration, and first
/// meaningful liquidity remain separate timestamps.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenLifecycle {
    Created,
    PreGraduation,
    Migrated,     // = GRADUATED (MIGRATED/GRADUATED)
    FirstLiquidity,
    Active,
    Cooling,
    Dormant,
    Archived,
    Tombstoned,
}

/// Truth status for provenance claims (doc §8.4).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceTruthStatus {
    Exact,
    Reconstructed,
    Estimated,
    Insufficient,
}

/// Provenance role (doc §8.4). Roles remain separate.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceRole {
    Originator,
    IndependentSpread,
    OfficialAdopter,
    Deployer,
    CallerAmplifier,
    MarketLeadingContract,
}

/// Token family/canonicality relation (doc §8.3).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FamilyRelation {
    Official,
    Derivative,
    Copycat,
}

/// Token intelligence payload. Contract address is token truth; ticker/name are
/// discovery clues only (doc line 57).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenIntelligence {
    pub chain: String,
    pub contract_address: String, // token truth
    pub ticker: Option<String>,   // discovery clue
    pub name: Option<String>,     // discovery clue
    pub lifecycle: TokenLifecycle,
    pub family: Option<TokenFamily>,
    pub metadata_fingerprint: Option<String>,
    pub market_status: Option<String>,
}

/// Token family grouping (doc §8.3).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenFamily {
    pub family_key: String,
    pub canonical_token: Option<String>, // contract address of market leader
    pub relation: FamilyRelation,
}

/// Provenance evidence link (doc §8.4 token-first flow).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProvenanceEvidence {
    pub role: ProvenanceRole,
    pub truth_status: ProvenanceTruthStatus,
    pub earliest_evidence_ref: Option<String>,
    pub confidence: Option<f64>,
}
