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
    completed_stages: &[ProvenanceStage],
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

    // REV-011-F07: derive the furthest CONTIGUOUS completed stage from the
    // resolver's explicit `completed_stages` trace. We stop at the first gap.
    let mut resolved_stage = contiguous_stage(completed_stages);

    // REV-013-F03: EarliestEvidence (and the final graph) require actual
    // evidence. If the resolved stage reaches EarliestEvidence or beyond but no
    // edge carries a non-empty evidence_ref, clamp back to the OCR/ASR stage
    // (one before EarliestEvidence) — evidence-bound completion.
    let has_evidence = edges
        .iter()
        .any(|e| e.evidence_ref.as_deref().map(|r| !r.trim().is_empty()).unwrap_or(false));
    if stage_index(resolved_stage) >= stage_index(ProvenanceStage::EarliestEvidence)
        && !has_evidence
    {
        resolved_stage = ProvenanceStage::OcrAsrImagePhoneticExpansion;
    }

    NarrativeResolution {
        narrative_key: narrative_key.to_string(),
        resolved_stage,
        originator,
        independent_spread,
        official_adopter,
        market_leading_contract,
    }
}

/// Furthest stage reached with NO gap, starting from `DeployFirstLiquidity`.
/// Stages must appear in order; stop at the first missing stage.
fn contiguous_stage(completed: &[ProvenanceStage]) -> ProvenanceStage {
    let mut expected = 0u8;
    let mut last = ProvenanceStage::DeployFirstLiquidity;
    for &s in completed {
        if stage_index(s) == expected {
            last = s;
            expected += 1;
        } else {
            // Out-of-order or duplicate -> stop (gap detected).
            break;
        }
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
            ProvenanceStage::DeployFirstLiquidity,
            ProvenanceStage::MetadataFingerprint,
            ProvenanceStage::LocalArchiveSearch,
            ProvenanceStage::ExactAliasWebXTiktokSearch,
            ProvenanceStage::OcrAsrImagePhoneticExpansion,
            ProvenanceStage::EarliestEvidence,
            ProvenanceStage::OriginAdoptionPropagationGraph,
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
        // Missing MetadataFingerprint (index 1) -> only DeployFirstLiquidity counted.
        let stages = [
            ProvenanceStage::DeployFirstLiquidity,
            ProvenanceStage::LocalArchiveSearch, // gap: skipped MetadataFingerprint
            ProvenanceStage::ExactAliasWebXTiktokSearch,
        ];
        let r = resolve("narr1", &[], &stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::DeployFirstLiquidity);
    }

    // REV-013-F03: all stages completed but NO edges/evidence -> stop before
    // EarliestEvidence (clamp to OCR/ASR).
    #[test]
    fn all_stages_without_evidence_stops_before_earliest_evidence() {
        let all_stages = [
            ProvenanceStage::DeployFirstLiquidity,
            ProvenanceStage::MetadataFingerprint,
            ProvenanceStage::LocalArchiveSearch,
            ProvenanceStage::ExactAliasWebXTiktokSearch,
            ProvenanceStage::OcrAsrImagePhoneticExpansion,
            ProvenanceStage::EarliestEvidence,
            ProvenanceStage::OriginAdoptionPropagationGraph,
        ];
        let r = resolve("narr1", &[], &all_stages);
        assert_eq!(r.resolved_stage, ProvenanceStage::OcrAsrImagePhoneticExpansion);
    }
}
