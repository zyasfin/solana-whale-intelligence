//! REV-020 acceptance scenarios + cross-chain collision fixtures.
//!
//! Each test maps one REV-020 "Minimum acceptance" scenario (1–10) to an
//! observable assertion through the `sf` library public API.

use solana_whale_intelligence::sf::core::TruthStatus;
use solana_whale_intelligence::sf::graph::{EdgeType, EntityEdge, EntityNode, NodeType};
use solana_whale_intelligence::sf::recent::{
    ActorExtraction, CapabilityStatus, Coverage, RecentConfidence, RecentEvent, RecentRelation,
};
use solana_whale_intelligence::sf::recent_runtime::{
    apply_retraction, assign_dependency_group, build_timeline, coverage_status,
    resolve_candidates, should_trigger_lookup,
};

fn token(key: &str) -> EntityNode {
    EntityNode { entity_key: key.into(), node_type: NodeType::Token }
}
fn wallet(key: &str) -> EntityNode {
    EntityNode { entity_key: key.into(), node_type: NodeType::Wallet }
}
fn edge(ty: EdgeType, from: &str, to: &str, refs: &[&str]) -> EntityEdge {
    EntityEdge {
        from_entity_key: from.into(),
        to_entity_key: to.into(),
        edge_type: ty,
        occurred_at: "2026-01-01T00:00:00Z".into(),
        source_id: None,
        truth_status: TruthStatus::Confirmed,
        confidence: None,
        valid_from: "2026-01-01T00:00:00Z".into(),
        valid_until: None,
        supersedes: None,
        evidence_refs: refs.iter().map(|s| s.to_string()).collect(),
    }
}
fn anchor() -> ActorExtraction {
    ActorExtraction {
        token: "sol:AAA".into(),
        deployer: Some("wallet:D1".into()),
        authority: Some("wallet:A1".into()),
        fee_payer: Some("wallet:F1".into()),
        factory: Some("program:FACTORY".into()),
        initial_funder: Some("wallet:FUND1".into()),
        authority_changes: vec![],
        social_identities: vec!["x:acct_immutable_1".into()],
    }
}
fn event(anchor: &str, occurred: &str, observed: &str, coverage: Coverage, refs: &[&str]) -> RecentEvent {
    RecentEvent {
        event_type: "transfer".into(),
        anchor_identity: anchor.into(),
        related_identities: vec![],
        chain_qualified_contract: anchor.into(),
        occurred_at: occurred.into(),
        observed_at: observed.into(),
        relation: None,
        truth_status: TruthStatus::Confirmed,
        confidence: None,
        confidence_level: RecentConfidence::Estimated,
        evidence_refs: refs.iter().map(|s| s.to_string()).collect(),
        dependency_group: None,
        freshness: None,
        coverage,
        capability_status: CapabilityStatus::Available,
        retraction: None,
    }
}

// 1. Same-symbol cross-chain contracts without corroboration remain unrelated.
#[test]
fn accept1_same_symbol_cross_chain_stays_unrelated() {
    let nodes = vec![token("sol:AAA"), token("eth:AAA")];
    let edges: Vec<EntityEdge> = vec![];
    let out = resolve_candidates(&anchor(), &nodes, &edges);
    assert!(out.is_empty());
}

// 2. Same chain-qualified deployer/authority creates exact relations;
//    launchpad/factory stays separate.
#[test]
fn accept2_same_deployer_exact_factory_separate() {
    let nodes = vec![token("sol:AAA"), token("sol:BBB"), token("sol:CCC")];
    let edges = vec![
        edge(EdgeType::DeployedBy, "wallet:D1", "sol:BBB", &["ev-deploy"]),
        // Factory must NOT produce a SameDeployer relation.
        edge(EdgeType::DeployedBy, "program:FACTORY", "sol:CCC", &["ev-factory"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].relation, RecentRelation::SameDeployer);
    assert_eq!(out[0].confidence, RecentConfidence::Exact);
    assert_eq!(out[0].to_identity.value, "sol:BBB");
}

// 3. New wallet funded by known deployer + immutable X ID → Reconstructed.
#[test]
fn accept3_funded_by_known_deployer_is_reconstructed() {
    let nodes = vec![token("sol:AAA"), wallet("wallet:NEW1")];
    let edges = vec![edge(EdgeType::FundedBy, "wallet:D1", "wallet:NEW1", &["ev-fund"])];
    let out = resolve_candidates(&anchor(), &nodes, &edges);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].relation, RecentRelation::FundedByKnownDeployer);
    assert_eq!(out[0].confidence, RecentConfidence::Reconstructed);
}

// 4. Reused handle/domain without immutable ownership → Insufficient candidate.
#[test]
fn accept4_reused_link_without_ownership_is_insufficient() {
    let nodes = vec![token("sol:AAA"), token("sol:BBB")];
    let edges = vec![edge(EdgeType::ReusedSocialLink, "sol:BBB", "x:acct_immutable_1", &["ev-reuse"])];
    let out = resolve_candidates(&anchor(), &nodes, &edges);
    // The reused link is tied to the anchor's social identity; classified Insufficient.
    assert!(out.iter().all(|c| c.confidence == RecentConfidence::Insufficient));
}

// 5. First-party exact-CA announcement → evidence-backed OFFICIAL_CA_ANNOUNCEMENT.
#[test]
fn accept5_exact_ca_announcement_is_official() {
    // classify_social maps OfficialCaAnnouncement -> RecentRelation::OfficialCaAnnouncement.
    let rel = solana_whale_intelligence::sf::recent_runtime::classify_social(
        &solana_whale_intelligence::sf::recent::SocialEvidenceObservation {
            platform: solana_whale_intelligence::sf::browser::BrowserPlatform::X,
            post_or_profile_id: "p".into(),
            immutable_account_id: "x:acct_immutable_1".into(),
            text_media_hash: "h".into(),
            published_at: "2026-01-01T00:00:00Z".into(),
            observed_at: "2026-01-01T01:00:00Z".into(),
            relation_kind: solana_whale_intelligence::sf::recent::SocialEvidenceKind::OfficialCaAnnouncement,
            raw_ref: "r".into(),
            parser_version: "1".into(),
            coverage: Coverage::Full,
            session_health: solana_whale_intelligence::sf::browser::SessionHealth::Ok,
        },
    );
    assert_eq!(rel, RecentRelation::OfficialCaAnnouncement);
}

// 6. Explicit authoritative cross-chain announcement joins family;
//    symbol-only namesakes do not.
#[test]
fn accept6_cross_chain_announcement_joins_family() {
    let nodes = vec![token("sol:AAA"), token("eth:BBB"), token("eth:CCC")];
    let edges = vec![
        edge(EdgeType::CrossChainDeployment, "sol:AAA", "eth:BBB", &["ev-xc"]),
        // eth:CCC is a symbol-only namesake with no edge -> unrelated.
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].relation, RecentRelation::CrossChainDeployment);
    assert_eq!(out[0].to_identity.value, "eth:BBB");
}

// 7. Correlated copies collapse into one dependency group while preserving
//    all evidence refs.
#[test]
fn accept7_copies_collapse_preserve_evidence() {
    let mut events = vec![
        event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["upstream_ev"]),
        event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["downstream_ev"]),
    ];
    assign_dependency_group(&mut events, &[("upstream_ev".into(), "downstream_ev".into())]);
    // The downstream copy is grouped under the upstream; evidence preserved.
    assert_eq!(events[1].dependency_group.as_deref(), Some("group:upstream_ev"));
    assert_eq!(events[1].evidence_refs, vec!["downstream_ev".to_string()]);
}

// 8. Deleted/retracted evidence remains archived and supersedes projection.
#[test]
fn accept8_retraction_archives_not_deletes() {
    let mut e = event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]);
    e.retraction = Some(solana_whale_intelligence::sf::recent::Retraction {
        superseded_by: "ev-new".into(),
        retracted_at: "2026-01-02T00:00:00Z".into(),
        truth_status: TruthStatus::Superseded,
    });
    let out = apply_retraction(&[e]);
    assert_eq!(out.len(), 1, "archived, not deleted");
    assert_eq!(out[0].truth_status, TruthStatus::Superseded);
}

// 9. Timeline sorts by occurred_at and exposes observed_at/relation/evidence/
//    confidence/freshness/coverage.
#[test]
fn accept9_timeline_sorts_and_exposes_texture() {
    let mut a = event("sol:AAA", "2026-01-03T00:00:00Z", "2026-01-03T01:00:00Z", Coverage::Full, &["r3"]);
    a.relation = Some(RecentRelation::SameDeployer);
    let b = event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]);
    let c = event("sol:AAA", "2026-01-02T00:00:00Z", "2026-01-02T01:00:00Z", Coverage::Degraded, &["r2"]);
    let tl = build_timeline("sol:AAA", &[a.clone(), b.clone(), c.clone()]);
    assert_eq!(tl.events[0].occurred_at, "2026-01-01T00:00:00Z");
    assert_eq!(tl.events[2].occurred_at, "2026-01-03T00:00:00Z");
    // Texture fields are present and populated.
    assert_eq!(tl.events[2].relation, Some(RecentRelation::SameDeployer));
    assert!(!tl.events[2].observed_at.is_empty());
    assert!(!tl.events[2].evidence_refs.is_empty());
    assert!(tl.events[2].confidence_level == RecentConfidence::Estimated);
    assert_eq!(tl.events[2].coverage, Coverage::Full);
}

// 10. Dormant/tombstoned tokens receive no individual polling.
#[test]
fn accept10_dormant_tokens_no_polling() {
    assert!(!should_trigger_lookup("dormant"));
    assert!(!should_trigger_lookup("archived"));
    assert!(!should_trigger_lookup("tombstoned"));
    assert!(should_trigger_lookup("active"));
    assert!(should_trigger_lookup("first_liquidity"));
}

// Cross-chain collision fixture: two independent deployers on different chains
// must not be conflated; the anchor resolves only its own chain's deployer.
#[test]
fn cross_chain_collision_deployer_not_conflated() {
    let nodes = vec![token("sol:AAA"), token("sol:BBB"), token("eth:CCC")];
    let edges = vec![
        // Anchor's deployer funds a DIFFERENT chain token via an unrelated actor.
        edge(EdgeType::DeployedBy, "wallet:D1", "sol:BBB", &["ev1"]),
        // eth:CCC deployed by an unrelated wallet on another chain.
        edge(EdgeType::DeployedBy, "wallet:D_OTHER", "eth:CCC", &["ev2"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges);
    // Only sol:BBB (same deployer) is a SameDeployer; eth:CCC is unrelated.
    assert!(out.iter().any(|c| c.to_identity.value == "sol:BBB" && c.relation == RecentRelation::SameDeployer));
    assert!(!out.iter().any(|c| c.to_identity.value == "eth:CCC"));
}

// Cross-chain collision fixture: coverage never coerces unavailable to zero.
#[test]
fn cross_chain_collision_coverage_not_zero() {
    let evs = vec![
        event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Unavailable, &["r1"]),
        event("sol:AAA", "2026-01-02T00:00:00Z", "2026-01-02T01:00:00Z", Coverage::Full, &["r2"]),
    ];
    assert_eq!(coverage_status(&evs), Coverage::Degraded);
}
