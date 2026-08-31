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

use super::narrative::{NarrativeEdge, NarrativeResolution, ProvenanceStage};
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
/// `originator`/`official_adopter`/`market_leading_contract` are extracted from
/// the edges by role; `independent_spread` collects all edges with that role.
pub fn resolve(
    narrative_key: &str,
    edges: &[NarrativeEdge],
    completed_stages: &[(ProvenanceStage, Option<&str>)],
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

    // REV-017-F02: each stage's proof is a typed evidence/record reference
    // (`Some(non-empty ref)`), not a caller-asserted boolean.
    let resolved_stage = contiguous_stage(completed_stages);

    NarrativeResolution {
        narrative_key: narrative_key.to_string(),
        resolved_stage,
        originator,
        independent_spread,
        official_adopter,
        market_leading_contract,
    }
}

/// Furthest stage reached with NO gap. Each entry is `(stage, proof)` where
/// `proof` is `Some(non-empty evidence/record reference)`. EarliestEvidence and
/// the final graph REQUIRE a non-empty proof; a `None`/empty proof stops there.
fn contiguous_stage(completed: &[(ProvenanceStage, Option<&str>)]) -> ProvenanceStage {
    let mut expected = 0u8;
    let mut last = ProvenanceStage::DeployFirstLiquidity;
    for &(s, proof) in completed {
        if stage_index(s) != expected {
            break; // gap
        }
        if stage_index(s) >= stage_index(ProvenanceStage::EarliestEvidence) {
            let has_proof = proof.map(|r| !r.trim().is_empty()).unwrap_or(false);
            if !has_proof {
                break;
            }
        }
        last = s;
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
            (ProvenanceStage::DeployFirstLiquidity, Some("p0")),
            (ProvenanceStage::MetadataFingerprint, Some("p1")),
            (ProvenanceStage::LocalArchiveSearch, Some("p2")),
            (ProvenanceStage::ExactAliasWebXTiktokSearch, Some("p3")),
            (ProvenanceStage::OcrAsrImagePhoneticExpansion, Some("p4")),
            (ProvenanceStage::EarliestEvidence, Some("ev-proof")),
            (ProvenanceStage::OriginAdoptionPropagationGraph, Some("graph-proof")),
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
            (ProvenanceStage::DeployFirstLiquidity, Some("p0")),
            (ProvenanceStage::LocalArchiveSearch, Some("p2")), // gap: skipped MetadataFingerprint
            (ProvenanceStage::ExactAliasWebXTiktokSearch, Some("p3")),
        ];
        let r = resolve("narr1", &[], &stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::DeployFirstLiquidity);
    }

    // REV-017-F02: EarliestEvidence/final graph WITHOUT a non-empty proof ref
    // stops there, even if earlier stages carry proofs.
    #[test]
    fn earliest_evidence_without_proof_stops() {
        let stages = [
            (ProvenanceStage::DeployFirstLiquidity, Some("p0")),
            (ProvenanceStage::MetadataFingerprint, Some("p1")),
            (ProvenanceStage::LocalArchiveSearch, Some("p2")),
            (ProvenanceStage::ExactAliasWebXTiktokSearch, Some("p3")),
            (ProvenanceStage::OcrAsrImagePhoneticExpansion, Some("p4")),
            (ProvenanceStage::EarliestEvidence, None), // no proof ref
            (ProvenanceStage::OriginAdoptionPropagationGraph, Some("graph-proof")),
        ];
        let r = resolve("narr1", &[], &stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::OcrAsrImagePhoneticExpansion);
    }
}
