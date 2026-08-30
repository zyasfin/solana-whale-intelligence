//! Narrative graph domain (Phase 2).
//!
//! Canonical source: PLAN SWI §8.4 (Name/Meme Provenance) + §22 "Narrative
//! River/Origin Trace". Narrative is a first-class intelligence plane node;
//! name/meme provenance follows a token-first flow (deploy -> metadata
//! fingerprint -> archive search -> earliest evidence -> origin/adoption/
//! propagation graph).

use serde::{Deserialize, Serialize};

use super::token::{ProvenanceRole, ProvenanceTruthStatus};

/// A narrative node (name/meme provenance).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Narrative {
    pub narrative_key: String,
    pub token: Option<String>, // contract address, when token-linked
    pub name_fingerprint: String, // metadata fingerprint (name/symbol/image)
    pub origin_evidence: Option<String>, // earliest evidence ref
}

/// A provenance step in the origin/adoption/propagation graph (doc §8.4).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NarrativeEdge {
    pub from_key: String,
    pub to_key: String,
    pub role: ProvenanceRole,
    pub truth_status: ProvenanceTruthStatus,
    pub evidence_ref: Option<String>,
    pub confidence: Option<f64>,
}

/// Token-first provenance flow stages (doc §8.4).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceStage {
    DeployFirstLiquidity,
    WebXTiktokSearch,
    LocalArchiveSearch,
    ExactAliasSearch,
    OcrAsrImagePhoneticExpansion,
    EarliestEvidence,
    OriginAdoptionPropagationGraph,
}

/// A narrative resolver result (worker role "Narrative resolver", doc §4.2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NarrativeResolution {
    pub narrative_key: String,
    pub resolved_stage: ProvenanceStage,
    pub originator: Option<String>,
    pub independent_spread: Vec<String>,
    pub official_adopter: Option<String>,
    pub market_leading_contract: Option<String>,
}
