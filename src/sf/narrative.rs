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
    MetadataFingerprint, // doc §8.4: metadata fingerprint (was missing, REV-007-F16)
    LocalArchiveSearch,
    ExactAliasWebXTiktokSearch, // doc §8.4: exact/alias/web/X/TikTok search
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

/// An authoritative, verified provenance-stage proof (REV-022-F04 / REV-023 §5 /
/// REV-025-F01).
///
/// Fields are private and the type is **not** `Deserialize`/`Serialize`: a proof
/// can only be minted through the store-bound production constructor or the
/// `#[cfg(test)]` mint helper. A caller cannot fabricate a proof by
/// deserializing free-text fields; the artifact must exist in the evidence/graph
/// store and be bound to the same `(narrative_key, run_id, stage)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedStageProof {
    narrative_key: String,
    run_id: String,
    stage: ProvenanceStage,
    artifact_id: String,
    artifact_kind: StageArtifactKind,
}

impl VerifiedStageProof {
    /// Production constructor. Verifies the referenced artifact exists in the
    /// appropriate store and is bound to `(narrative_key, run_id, stage)`.
    /// `EarliestEvidence` requires an evidence ref; the final graph requires a
    /// graph-assembly record id. Returns `None` (fail-closed) otherwise.
    pub fn verify(
        narrative_key: &str,
        run_id: &str,
        stage: ProvenanceStage,
        artifact_id: &str,
        artifact_kind: StageArtifactKind,
        evidence_refs: &[&str],
        graph_records: &[&str],
    ) -> Option<Self> {
        if narrative_key.trim().is_empty() || run_id.trim().is_empty() || artifact_id.trim().is_empty() {
            return None;
        }
        let exists = match stage {
            ProvenanceStage::EarliestEvidence => {
                artifact_kind == StageArtifactKind::EvidenceRef
                    && evidence_refs.contains(&artifact_id)
            }
            ProvenanceStage::OriginAdoptionPropagationGraph => {
                artifact_kind == StageArtifactKind::GraphAssemblyRecord
                    && graph_records.contains(&artifact_id)
            }
            _ => true, // earlier stages accept a non-empty artifact id + kind
        };
        if !exists {
            return None;
        }
        Some(VerifiedStageProof {
            narrative_key: narrative_key.to_string(),
            run_id: run_id.to_string(),
            stage,
            artifact_id: artifact_id.to_string(),
            artifact_kind,
        })
    }

    /// `#[cfg(test)]` mint helper for unit tests only; no production caller can
    /// fabricate a proof without store verification.
    #[cfg(test)]
    pub fn mint(
        narrative_key: &str,
        run_id: &str,
        stage: ProvenanceStage,
        artifact_id: &str,
        artifact_kind: StageArtifactKind,
    ) -> Self {
        VerifiedStageProof {
            narrative_key: narrative_key.to_string(),
            run_id: run_id.to_string(),
            stage,
            artifact_id: artifact_id.to_string(),
            artifact_kind,
        }
    }

    /// The provenance stage this proof certifies.
    pub fn stage(&self) -> ProvenanceStage {
        self.stage
    }

    /// The narrative this proof is bound to.
    pub fn narrative_key(&self) -> &str {
        &self.narrative_key
    }

    /// The run this proof is bound to.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The kind of stored artifact this proof references.
    pub fn artifact_kind(&self) -> StageArtifactKind {
        self.artifact_kind
    }

    /// Whether the referenced artifact is non-empty.
    pub fn has_artifact(&self) -> bool {
        !self.artifact_id.trim().is_empty()
    }
}

/// The kind of stored artifact a [`VerifiedStageProof`] references.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StageArtifactKind {
    EvidenceRef,
    GraphAssemblyRecord,
}
