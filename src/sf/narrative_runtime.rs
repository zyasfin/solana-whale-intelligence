//! Runtime logic: narrative provenance resolution (Phase 2).
//!
//! Canonical source: PLAN SWI §8.4 "Name/Meme Provenance" (lines 459-500):
//! token-first flow — deploy/first liquidity -> metadata fingerprint -> local
//! archive search -> exact/alias/web/X/TikTok search -> OCR/ASR/image/phonetic
//! expansion -> earliest evidence -> origin/adoption/propagation graph.
//! Roles remain separate; truth status is Exact/Reconstructed/Estimated/
//! Insufficient.
//!
//! This module drives the frozen `ProvenanceStage` sequence and assembles a
//! `NarrativeResolution` from the evidence. It consumes the frozen
//! `narrative.rs` domain types (`Narrative`, `NarrativeEdge`, `ProvenanceStage`,
//! `NarrativeResolution`) and `token.rs` (`ProvenanceRole`,
//! `ProvenanceTruthStatus`) and introduces no new frozen state.

use super::narrative::{
    NarrativeEdge, NarrativeResolution, ProvenanceStage, StageArtifactKind, VerifiedStageProof,
};
use super::token::ProvenanceRole;

/// Ordered stage index (matches the frozen §8.4 token-first flow).
fn stage_index(s: ProvenanceStage) -> u8 {
    match s {
        ProvenanceStage::DeployFirstLiquidity => 0,
        ProvenanceStage::MetadataFingerprint => 1,
        ProvenanceStage::LocalArchiveSearch => 2,
        ProvenanceStage::ExactAliasWebXTiktokSearch => 3,
        ProvenanceStage::OcrAsrImagePhoneticExpansion => 4,
        ProvenanceStage::EarliestEvidence => 5,
        ProvenanceStage::OriginAdoptionPropagationGraph => 6,
    }
}

/// Whether a provenance stage can advance forward (no regression).
pub fn can_advance(current: ProvenanceStage, next: ProvenanceStage) -> bool {
    stage_index(next) > stage_index(current)
}

/// Build a narrative resolution from the evidence edges. The resolved stage is
/// the furthest stage with at least one supporting edge; a token with no
/// evidence stays at `DeployFirstLiquidity` (fail-closed — never infer a stage
/// that has no evidence).
///
pub fn resolve(
    narrative_key: &str,
    edges: &[NarrativeEdge],
    completed_stages: &[VerifiedStageProof],
) -> NarrativeResolution {
    let mut originator: Option<String> = None;
    let mut official_adopter: Option<String> = None;
    let mut market_leading_contract: Option<String> = None;
    let mut independent_spread: Vec<String> = Vec::new();

    for e in edges {
        match e.role {
            ProvenanceRole::Originator => {
                if originator.is_none() {
                    originator = Some(e.from_key.clone());
                }
            }
            ProvenanceRole::OfficialAdopter => {
                if official_adopter.is_none() {
                    official_adopter = Some(e.from_key.clone());
                }
            }
            ProvenanceRole::MarketLeadingContract => {
                if market_leading_contract.is_none() {
                    market_leading_contract = Some(e.from_key.clone());
                }
            }
            ProvenanceRole::IndependentSpread => {
                independent_spread.push(e.from_key.clone());
            }
            _ => {}
        }
    }

    // REV-025-F01: each proof must be bound to THIS narrative; a foreign
    // narrative/run proof is rejected (fail-closed).
    let resolved_stage = contiguous_stage(narrative_key, completed_stages);

    NarrativeResolution {
        narrative_key: narrative_key.to_string(),
        resolved_stage,
        originator,
        independent_spread,
        official_adopter,
        market_leading_contract,
    }
}

/// Furthest stage reached with NO gap. Each entry is a [`VerifiedStageProof`]
/// bound to `narrative_key`, whose artifact must be non-empty and of the correct
/// kind. A proof bound to a different narrative/run stops progression
/// (REV-025-F01). `EarliestEvidence` requires `EvidenceRef`; the final graph
/// requires `GraphAssemblyRecord`.
fn contiguous_stage(narrative_key: &str, completed: &[VerifiedStageProof]) -> ProvenanceStage {
    let mut expected = 0u8;
    let mut last = ProvenanceStage::DeployFirstLiquidity;
    for proof in completed {
        if proof.narrative_key() != narrative_key {
            break; // foreign narrative proof is never valid for this narrative
        }
        let stage = proof.stage();
        if stage_index(stage) != expected {
            break; // gap
        }
        if !proof.has_artifact() {
            break; // empty artifact is never valid proof
        }
        let kind_ok = match stage {
            ProvenanceStage::EarliestEvidence => {
                proof.artifact_kind() == StageArtifactKind::EvidenceRef
            }
            ProvenanceStage::OriginAdoptionPropagationGraph => {
                proof.artifact_kind() == StageArtifactKind::GraphAssemblyRecord
            }
            _ => true, // earlier stages accept any non-empty artifact id
        };
        if !kind_ok {
            break;
        }
        last = stage;
        expected += 1;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::token::ProvenanceTruthStatus;

    fn edge(from: &str, role: ProvenanceRole, ts: ProvenanceTruthStatus) -> NarrativeEdge {
        NarrativeEdge {
            from_key: from.into(),
            to_key: "TOKEN".into(),
            role,
            truth_status: ts,
            evidence_ref: if ts == ProvenanceTruthStatus::Exact { Some("ev".into()) } else { None },
            confidence: None,
        }
    }

    /// Build a stage proof via the `#[cfg(test)]` mint helper. `kind` defaults to
    /// `EvidenceRef` unless the stage is the final graph (which must be a
    /// `GraphAssemblyRecord`).
    fn proof(stage: ProvenanceStage, artifact_ref: &str) -> VerifiedStageProof {
        let artifact_kind = match stage {
            ProvenanceStage::OriginAdoptionPropagationGraph => StageArtifactKind::GraphAssemblyRecord,
            _ => StageArtifactKind::EvidenceRef,
        };
        VerifiedStageProof::mint("narr1", "run1", stage, artifact_ref, artifact_kind)
    }

    #[test]
    fn stage_advances_forward_only() {
        assert!(can_advance(ProvenanceStage::DeployFirstLiquidity, ProvenanceStage::MetadataFingerprint));
        assert!(!can_advance(ProvenanceStage::EarliestEvidence, ProvenanceStage::DeployFirstLiquidity));
    }

    #[test]
    fn resolve_extracts_roles() {
        let edges = vec![
            edge("originator_wallet", ProvenanceRole::Originator, ProvenanceTruthStatus::Exact),
            edge("spread1", ProvenanceRole::IndependentSpread, ProvenanceTruthStatus::Reconstructed),
            edge("adopter", ProvenanceRole::OfficialAdopter, ProvenanceTruthStatus::Exact),
        ];
        let all_stages = [
            proof(ProvenanceStage::DeployFirstLiquidity, "p0"),
            proof(ProvenanceStage::MetadataFingerprint, "p1"),
            proof(ProvenanceStage::LocalArchiveSearch, "p2"),
            proof(ProvenanceStage::ExactAliasWebXTiktokSearch, "p3"),
            proof(ProvenanceStage::OcrAsrImagePhoneticExpansion, "p4"),
            proof(ProvenanceStage::EarliestEvidence, "ev-proof"),
            proof(ProvenanceStage::OriginAdoptionPropagationGraph, "graph-proof"),
        ];
        let r = resolve("narr1", &edges, &all_stages);
        assert_eq!(r.originator.as_deref(), Some("originator_wallet"));
        assert_eq!(r.official_adopter.as_deref(), Some("adopter"));
        assert_eq!(r.independent_spread.len(), 1);
        assert_eq!(r.resolved_stage, ProvenanceStage::OriginAdoptionPropagationGraph);
    }

    #[test]
    fn resolve_no_evidence_stays_at_deploy() {
        let r = resolve("narr1", &[], &[]);
        assert_eq!(r.resolved_stage, ProvenanceStage::DeployFirstLiquidity);
        assert!(r.originator.is_none());
    }

    // REV-011-F07: a gap in completed_stages stops contiguous progress.
    #[test]
    fn gap_in_stages_stops_before_gap() {
        let stages = [
            proof(ProvenanceStage::DeployFirstLiquidity, "p0"),
            proof(ProvenanceStage::LocalArchiveSearch, "p2"), // gap: skipped MetadataFingerprint
            proof(ProvenanceStage::ExactAliasWebXTiktokSearch, "p3"),
        ];
        let r = resolve("narr1", &[], &stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::DeployFirstLiquidity);
    }

    // REV-019-F01: EarliestEvidence with an EMPTY ref stops there.
    #[test]
    fn earliest_evidence_without_proof_stops() {
        let stages = [
            proof(ProvenanceStage::DeployFirstLiquidity, "p0"),
            proof(ProvenanceStage::MetadataFingerprint, "p1"),
            proof(ProvenanceStage::LocalArchiveSearch, "p2"),
            proof(ProvenanceStage::ExactAliasWebXTiktokSearch, "p3"),
            proof(ProvenanceStage::OcrAsrImagePhoneticExpansion, "p4"),
            proof(ProvenanceStage::EarliestEvidence, ""), // empty ref
            proof(ProvenanceStage::OriginAdoptionPropagationGraph, "graph-proof"),
        ];
        let r = resolve("narr1", &[], &stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::OcrAsrImagePhoneticExpansion);
    }

    // REV-019-F01: EarliestEvidence carrying a GraphAssemblyRecord (wrong kind)
    // stops at the prior stage.
    #[test]
    fn earliest_evidence_wrong_kind_stops() {
        let stages = [
            proof(ProvenanceStage::DeployFirstLiquidity, "p0"),
            proof(ProvenanceStage::MetadataFingerprint, "p1"),
            proof(ProvenanceStage::LocalArchiveSearch, "p2"),
            proof(ProvenanceStage::ExactAliasWebXTiktokSearch, "p3"),
            proof(ProvenanceStage::OcrAsrImagePhoneticExpansion, "p4"),
            VerifiedStageProof::mint(
                "narr1",
                "run1",
                ProvenanceStage::EarliestEvidence,
                "ev-proof",
                StageArtifactKind::GraphAssemblyRecord,
            ),
            proof(ProvenanceStage::OriginAdoptionPropagationGraph, "graph-proof"),
        ];
        let r = resolve("narr1", &[], &stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::OcrAsrImagePhoneticExpansion);
    }

    // REV-019-F01: final graph with an EvidenceRef (wrong kind) stops at
    // EarliestEvidence.
    #[test]
    fn final_graph_wrong_kind_stops() {
        let stages = [
            proof(ProvenanceStage::DeployFirstLiquidity, "p0"),
            proof(ProvenanceStage::MetadataFingerprint, "p1"),
            proof(ProvenanceStage::LocalArchiveSearch, "p2"),
            proof(ProvenanceStage::ExactAliasWebXTiktokSearch, "p3"),
            proof(ProvenanceStage::OcrAsrImagePhoneticExpansion, "p4"),
            proof(ProvenanceStage::EarliestEvidence, "ev-proof"),
            VerifiedStageProof::mint(
                "narr1",
                "run1",
                ProvenanceStage::OriginAdoptionPropagationGraph,
                "graph-proof",
                StageArtifactKind::EvidenceRef,
            ),
        ];
        let r = resolve("narr1", &[], &stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::EarliestEvidence);
    }

    // REV-022-F04: the production constructor verifies the artifact exists in
    // the store and is bound to the right stage; a fabricated/absent artifact
    // is rejected.
    #[test]
    fn verify_rejects_absent_artifact() {
        // EarliestEvidence requires an evidence ref present in the store.
        assert!(
            VerifiedStageProof::verify(
                "narr1",
                "run1",
                ProvenanceStage::EarliestEvidence,
                "missing-ev",
                StageArtifactKind::EvidenceRef,
                &["other-ev"],
                &[],
            )
            .is_none()
        );
        // Final graph requires a graph-assembly record present in the store.
        assert!(
            VerifiedStageProof::verify(
                "narr1",
                "run1",
                ProvenanceStage::OriginAdoptionPropagationGraph,
                "missing-graph",
                StageArtifactKind::GraphAssemblyRecord,
                &[],
                &["other-graph"],
            )
            .is_none()
        );
    }

    #[test]
    fn verify_accepts_present_artifact() {
        let ev = VerifiedStageProof::verify(
            "narr1",
            "run1",
            ProvenanceStage::EarliestEvidence,
            "ev-1",
            StageArtifactKind::EvidenceRef,
            &["ev-1"],
            &[],
        );
        assert!(ev.is_some());
        let graph = VerifiedStageProof::verify(
            "narr1",
            "run1",
            ProvenanceStage::OriginAdoptionPropagationGraph,
            "graph-1",
            StageArtifactKind::GraphAssemblyRecord,
            &[],
            &["graph-1"],
        );
        assert!(graph.is_some());
    }

    // REV-025-F01: a proof bound to a foreign narrative must not advance the
    // victim narrative's stage.
    #[test]
    fn foreign_narrative_proof_is_rejected() {
        let all_stages = [
            proof(ProvenanceStage::DeployFirstLiquidity, "p0"),
            proof(ProvenanceStage::MetadataFingerprint, "p1"),
            proof(ProvenanceStage::LocalArchiveSearch, "p2"),
            proof(ProvenanceStage::ExactAliasWebXTiktokSearch, "p3"),
            proof(ProvenanceStage::OcrAsrImagePhoneticExpansion, "p4"),
            proof(ProvenanceStage::EarliestEvidence, "ev-proof"),
            // This final graph proof is minted for a DIFFERENT narrative.
            VerifiedStageProof::mint(
                "foreign-narrative",
                "run1",
                ProvenanceStage::OriginAdoptionPropagationGraph,
                "graph-proof",
                StageArtifactKind::GraphAssemblyRecord,
            ),
        ];
        let r = resolve("victim-narrative", &[], &all_stages);
        // All proofs are minted for "narr1"; resolving "victim-narrative"
        // rejects them all, so the stage stays at Deploy.
        assert_eq!(r.resolved_stage, ProvenanceStage::DeployFirstLiquidity);
    }

    // A foreign-narrative FINAL proof stops at the prior (matching) stage.
    #[test]
    fn foreign_final_proof_stops_at_prior_stage() {
        let stages = [
            proof(ProvenanceStage::DeployFirstLiquidity, "p0"),
            proof(ProvenanceStage::MetadataFingerprint, "p1"),
            proof(ProvenanceStage::LocalArchiveSearch, "p2"),
            proof(ProvenanceStage::ExactAliasWebXTiktokSearch, "p3"),
            proof(ProvenanceStage::OcrAsrImagePhoneticExpansion, "p4"),
            proof(ProvenanceStage::EarliestEvidence, "ev-proof"),
            VerifiedStageProof::mint(
                "foreign-narrative",
                "run1",
                ProvenanceStage::OriginAdoptionPropagationGraph,
                "graph-proof",
                StageArtifactKind::GraphAssemblyRecord,
            ),
        ];
        // Early proofs are for "narr1"; resolve for "narr1".
        let r = resolve("narr1", &[], &stages);
        // The foreign final proof is rejected, so the stage stops at EarliestEvidence.
        assert_eq!(r.resolved_stage, ProvenanceStage::EarliestEvidence);
    }
}
