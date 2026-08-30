//! Ingestion domain (Phase 1): Solana/EVM raw ingestion pipeline.
//!
//! Canonical source: PLAN SWI §7 "Ingestion and normalization" (7.1 envelope,
//! 7.2 ingestion flow) and §8.1 "Solana/EVM raw ingestion".
//!
//! Ingestion flow (doc 7.2):
//!   fetch/stream -> raw evidence write -> envelope validation -> idempotent
//!   append -> normalization -> entity resolution -> graph edges -> scalar
//!   projections -> trigger evaluation -> jobs/outbox.
//! Raw evidence is written BEFORE parser-derived claims where practical.

use serde::{Deserialize, Serialize};

/// A supported chain for ingestion (doc §1 Frozen scope).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Solana,
    Robinhood,
    Ethereum,
    Base,
    Bsc,
}

impl Chain {
    pub fn as_str(&self) -> &'static str {
        match self {
            Chain::Solana => "solana",
            Chain::Robinhood => "robinhood",
            Chain::Ethereum => "ethereum",
            Chain::Base => "base",
            Chain::Bsc => "bsc",
        }
    }
}

/// A normalized ingestion stage (doc §7.2). Each stage is a distinct pipeline
/// step so partial failure is observable and idempotent.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IngestionStage {
    Fetch,
    RawEvidenceWrite,
    EnvelopeValidation,
    IdempotentAppend,
    Normalization,
    EntityResolution,
    GraphEdges,
    ScalarProjections,
    TriggerEvaluation,
    JobsOutbox,
}

/// Result of a single stage. Success carries the stage; failure carries a reason
/// (fail-closed: missing mandatory data is not treated as safe).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum StageResult {
    Ok(IngestionStage),
    Skipped(IngestionStage, String),
    Failed(IngestionStage, String),
}

/// An ingestion pipeline run (doc §7.2). Produces a stage-by-stage trace so
/// every decision is reproducible (gate #2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngestionRun {
    pub chain: Chain,
    pub source_name: String,
    pub started_at: String,
    pub stages: Vec<StageResult>,
}

/// EVM chain-ID validation (doc: endpoint mirrors validate chain ID). Solana
/// has no chain-id; Robinhood/Ethereum/Base/BSC carry a runtime-validated ID.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChainIdentity {
    Solana,
    Evm { chain_id: String }, // hex without 0x, validated at startup
}
