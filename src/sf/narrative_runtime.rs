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
use super::token::{ProvenanceRole, ProvenanceTruthStatus};

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
pub fn resolve(narrative_key: &str, edges: &[NarrativeEdge]) -> NarrativeResolution {
    let mut originator: Option<String> = None;
    let mut official_adopter: Option<String> = None;
    let mut market_leading_contract: Option<String> = None;
    let mut independent_spread: Vec<String> = Vec::new();

    let mut furthest = ProvenanceStage::DeployFirstLiquidity;

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

        // Furthest stage is the max stage index among edges' truth status:
        // an edge with `Exact` evidence implies EarliestEvidence reached.
        if e.truth_status == ProvenanceTruthStatus::Exact {
            furthest = ProvenanceStage::EarliestEvidence;
        }
    }

    // If we reached EarliestEvidence, the graph stage is also reached.
    if furthest == ProvenanceStage::EarliestEvidence {
        furthest = ProvenanceStage::OriginAdoptionPropagationGraph;
    }

    NarrativeResolution {
        narrative_key: narrative_key.to_string(),
        resolved_stage: furthest,
        originator,
        independent_spread,
        official_adopter,
        market_leading_contract,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: &str, role: ProvenanceRole, ts: ProvenanceTruthStatus) -> NarrativeEdge {
        NarrativeEdge {
            from_key: from.into(),
            to_key: "TOKEN".into(),
            role,
            truth_status: ts,
            evidence_ref: None,
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
            edge("spread2", ProvenanceRole::IndependentSpread, ProvenanceTruthStatus::Estimated),
            edge("adopter", ProvenanceRole::OfficialAdopter, ProvenanceTruthStatus::Exact),
        ];
        let r = resolve("narr1", &edges);
        assert_eq!(r.originator.as_deref(), Some("originator_wallet"));
        assert_eq!(r.official_adopter.as_deref(), Some("adopter"));
        assert_eq!(r.independent_spread.len(), 2);
        assert_eq!(r.resolved_stage, ProvenanceStage::OriginAdoptionPropagationGraph);
    }

    #[test]
    fn resolve_no_evidence_stays_at_deploy() {
        let r = resolve("narr1", &[]);
        assert_eq!(r.resolved_stage, ProvenanceStage::DeployFirstLiquidity);
        assert!(r.originator.is_none());
    }
}
