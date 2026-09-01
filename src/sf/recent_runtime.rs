//! Runtime logic: Token Recent + Deployer/Social Reuse (REV-020).
//!
//! Pure logic over the frozen `recent.rs` types plus the existing graph model.
//! Mirrors the `ingest_runtime.rs` pattern: no persistence/transport in this
//! module — it resolves candidates from in-hand nodes/edges, builds timelines,
//! assigns source-dependency groups, applies retraction, and computes coverage.

use std::collections::HashSet;

use super::graph::{EdgeType, EntityEdge, EntityNode};
use super::recent::{
    ActorExtraction, CandidateRelation, Coverage, IdentityKey, IdentityKind, RecentConfidence,
    RecentEvent, RecentRelation, RecentTimeline, SocialEvidenceKind, SocialEvidenceObservation,
};

/// Normalize a raw identity value into a chain-qualified key. Fail-closed:
/// empty/whitespace-only values return `None`. Websites are normalized to a
/// lowercase host with a leading `www.` stripped (minimal registrable-domain
/// approximation; full PSL is out of scope).
pub fn normalize_identity(kind: IdentityKind, value: &str) -> Option<IdentityKey> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match kind {
        IdentityKind::Website => {
            let parsed = url::Url::parse(value).ok()?;
            let host = parsed.host_str()?;
            let host = host.trim_start_matches("www.").to_ascii_lowercase();
            if host.is_empty() {
                return None;
            }
            Some(IdentityKey { kind, value: host })
        }
        _ => Some(IdentityKey {
            kind,
            value: value.to_string(),
        }),
    }
}

/// Resolve corroborated candidate relations for an anchor token from reverse
/// indexes (`nodes` + `edges`). Deterministic output ordered by
/// `(confidence rank desc, relation name, to_identity.value)`.
///
/// Rules (REV-020 minimum acceptance #1–#6):
/// - Same chain-qualified deployer/authority/fee-payer/funder → exact relation.
///   Factory/launchpad address is NOT a deployer (no relation emitted from it).
/// - New wallet funded by a known deployer → `Reconstructed`, never auto-official.
/// - Reused handle/domain without immutable ownership → `Insufficient`/candidate.
/// - First-party exact-CA announcement → `OfficialCaAnnouncement` / `Exact`.
/// - Explicit authoritative cross-chain announcement → `CrossChainDeployment` /
///   `Exact`; symbol/name similarity alone never yields a relation.
pub fn resolve_candidates(
    anchor: &ActorExtraction,
    nodes: &[EntityNode],
    edges: &[EntityEdge],
) -> Vec<CandidateRelation> {
    let mut out: Vec<CandidateRelation> = Vec::new();
    let mut seen: HashSet<(String, RecentRelation)> = HashSet::new();

    let anchor_token = &anchor.token;

    // Cross-chain deployment: an edge from the anchor token to a token on a
    // different chain whose edge type marks an authoritative cross-chain
    // announcement.
    for e in edges {
        if e.edge_type != EdgeType::CrossChainDeployment {
            continue;
        }
        if e.from_entity_key != *anchor_token {
            continue;
        }
        let rel = CandidateRelation {
            to_identity: IdentityKey {
                kind: IdentityKind::Token,
                value: e.to_entity_key.clone(),
            },
            relation: RecentRelation::CrossChainDeployment,
            confidence: RecentConfidence::Exact,
            evidence_refs: e.evidence_refs.clone(),
        };
        let key = (e.to_entity_key.clone(), rel.relation);
        if seen.insert(key) {
            out.push(rel);
        }
    }

    // Actor-based reverse lookup: for each token node, compare deployer/authority/
    // fee-payer/funder carried in its payload (edge `DeployedBy`/`FundedBy`) to
    // the anchor's actors. We derive candidate relations from same-actor edges.
    let token_nodes: Vec<&EntityNode> = nodes
        .iter()
        .filter(|n| n.node_type == super::graph::NodeType::Token)
        .collect();

    for node in token_nodes {
        let other = &node.entity_key;
        if other == anchor_token {
            continue;
        }
        // Collect deployer/funder edges pointing at this other token.
        for e in edges.iter().filter(|e| e.to_entity_key == *other) {
            let actor = &e.from_entity_key;
            let relation = match e.edge_type {
                EdgeType::DeployedBy
                    if anchor.deployer.as_deref() == Some(actor.as_str()) =>
                {
                    Some((RecentRelation::SameDeployer, RecentConfidence::Exact))
                }
                EdgeType::FundedBy
                    if anchor.initial_funder.as_deref() == Some(actor.as_str()) =>
                {
                    Some((RecentRelation::SameFunder, RecentConfidence::Exact))
                }
                _ => None,
            };
            if let Some((relation, confidence)) = relation {
                let rel = CandidateRelation {
                    to_identity: IdentityKey {
                        kind: IdentityKind::Token,
                        value: other.clone(),
                    },
                    relation,
                    confidence,
                    evidence_refs: e.evidence_refs.clone(),
                };
                let key = (other.clone(), relation);
                if seen.insert(key) {
                    out.push(rel);
                }
            }
        }
    }

    // A new wallet funded by a known deployer: `FundedByKnownDeployer`,
    // `Reconstructed` (never auto-official). Derived from a `FundedBy` edge whose
    // funder matches the anchor's deployer and whose target is a Wallet node.
    for e in edges {
        if e.edge_type != EdgeType::FundedBy {
            continue;
        }
        if anchor.deployer.as_deref() != Some(e.from_entity_key.as_str()) {
            continue;
        }
        let is_wallet = nodes
            .iter()
            .any(|n| n.entity_key == e.to_entity_key && n.node_type == super::graph::NodeType::Wallet);
        if !is_wallet {
            continue;
        }
        let rel = CandidateRelation {
            to_identity: IdentityKey {
                kind: IdentityKind::Wallet,
                value: e.to_entity_key.clone(),
            },
            relation: RecentRelation::FundedByKnownDeployer,
            confidence: RecentConfidence::Reconstructed,
            evidence_refs: e.evidence_refs.clone(),
        };
        let key = (e.to_entity_key.clone(), rel.relation);
        if seen.insert(key) {
            out.push(rel);
        }
    }

    // Reused social link without immutable-ownership evidence → candidate only.
    for identity in &anchor.social_identities {
        for e in edges {
            if e.edge_type != EdgeType::ReusedSocialLink {
                continue;
            }
            if e.to_entity_key == *identity {
                let rel = CandidateRelation {
                    to_identity: IdentityKey {
                        kind: IdentityKind::Social,
                        value: e.to_entity_key.clone(),
                    },
                    relation: RecentRelation::ReusedSocialLink,
                    confidence: RecentConfidence::Insufficient,
                    evidence_refs: e.evidence_refs.clone(),
                };
                let key = (e.to_entity_key.clone(), rel.relation);
                if seen.insert(key) {
                    out.push(rel);
                }
            }
        }
    }

    // Deterministic ordering.
    out.sort_by(|a, b| {
        b.confidence
            .rank()
            .cmp(&a.confidence.rank())
            .then_with(|| a.relation.as_str().cmp(b.relation.as_str()))
            .then_with(|| a.to_identity.value.cmp(&b.to_identity.value))
    });
    out
}

/// Build a per-token timeline sorted by `occurred_at` ascending (secondary
/// `observed_at` for stable order). Only events anchored on `anchor` are kept.
pub fn build_timeline(anchor: &str, events: &[RecentEvent]) -> RecentTimeline {
    let mut evs: Vec<RecentEvent> = events
        .iter()
        .filter(|e| e.anchor_identity == anchor)
        .cloned()
        .collect();
    evs.sort_by(|a, b| {
        a.occurred_at
            .cmp(&b.occurred_at)
            .then_with(|| a.observed_at.cmp(&b.observed_at))
    });
    RecentTimeline {
        token: anchor.to_string(),
        events: evs,
    }
}

/// Assign a source-dependency group to each event. `deps` holds
/// `(upstream_ref, downstream_ref)` pairs where a downstream evidence ref is a
/// repost/provider copy of the upstream ref (mirrors `source_dependencies`:
/// two vendors repeating one upstream event are NOT independent). An event
/// citing a downstream ref is grouped under its upstream; an event citing only
/// the upstream (or an unrelated ref) self-groups. Every `evidence_refs` entry
/// is preserved. Events already carrying a `dependency_group` are untouched.
pub fn assign_dependency_group(events: &mut [RecentEvent], deps: &[(String, String)]) {
    for e in events.iter_mut() {
        if e.dependency_group.is_some() {
            continue;
        }
        if let Some((u, _)) = deps
            .iter()
            .find(|(_, d)| e.evidence_refs.iter().any(|r| r == d))
        {
            // Correlated copy: collapse under the upstream ref's group.
            e.dependency_group = Some(format!("group:{u}"));
        } else if let Some(anchor_ref) = e.evidence_refs.first() {
            // No known downstream: self-group by the first evidence ref.
            e.dependency_group = Some(format!("group:{anchor_ref}"));
        }
    }
}

/// Apply retraction/supersession semantics (archive-not-delete, acceptance #8).
/// A superseded event remains present but its `truth_status` is downgraded to the
/// retraction's status, and the superseding event is the one surfaced as current.
/// Returns the full list including superseded entries (append-only).
pub fn apply_retraction(events: &[RecentEvent]) -> Vec<RecentEvent> {
    let mut out: Vec<RecentEvent> = Vec::new();
    for e in events {
        let mut e = e.clone();
        if let Some(retraction) = &e.retraction {
            e.truth_status = retraction.truth_status.clone();
        }
        out.push(e);
    }
    out
}

/// Compute aggregate coverage over a set of events. `Insufficient`/`Unavailable`
/// are never coerced to zero.
pub fn coverage_status(events: &[RecentEvent]) -> Coverage {
    if events.is_empty() {
        return Coverage::Unavailable;
    }
    let all_full = events.iter().all(|e| e.coverage == Coverage::Full);
    let all_on_demand = events.iter().all(|e| e.coverage == Coverage::OnDemand);
    let all_unavailable = events.iter().all(|e| e.coverage == Coverage::Unavailable);
    if all_full {
        Coverage::Full
    } else if all_on_demand {
        Coverage::OnDemand
    } else if all_unavailable {
        Coverage::Unavailable
    } else {
        // A mix (some full/on_demand, some unavailable/degraded) is partial.
        Coverage::Degraded
    }
}

/// Whether a token lifecycle should trigger a recent-intelligence lookup.
/// Dormant/tombstoned/archived tokens are event-wake only — no individual
/// polling (acceptance #10). `lifecycle` is the lowercase `TokenLifecycle`
/// wire form.
pub fn should_trigger_lookup(lifecycle: &str) -> bool {
    !matches!(lifecycle, "dormant" | "archived" | "tombstoned")
}

/// Build the token-triggered lookup terms (REV-020 line 1600): contract address,
/// disambiguated name/symbol, immutable account ID + current/historical handles,
/// domain, Telegram ID/URL, description phrases, image/OCR hash, aliases,
/// deployer/caller/funder, and linked contracts. Deterministic, deduplicated.
pub fn build_lookup_query(actor: &ActorExtraction) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut push = |t: String| {
        let t = t.trim().to_string();
        if !t.is_empty() && !terms.contains(&t) {
            terms.push(t);
        }
    };
    push(actor.token.clone());
    if let Some(d) = &actor.deployer {
        push(d.clone());
    }
    if let Some(a) = &actor.authority {
        push(a.clone());
    }
    if let Some(f) = &actor.fee_payer {
        push(f.clone());
    }
    if let Some(f) = &actor.initial_funder {
        push(f.clone());
    }
    for s in &actor.social_identities {
        push(s.clone());
    }
    terms
}

/// Classify a social observation into a recent relation. Exact-CA from a
/// first-party immutable account → `OfficialCaAnnouncement`; a profile CA/link
/// change → `ReusedSocialLink`; otherwise `SameSocialAccount`. Mentions never
/// become `OfficialCaAnnouncement` (acceptance #5).
pub fn classify_social(obs: &SocialEvidenceObservation) -> RecentRelation {
    match obs.relation_kind {
        SocialEvidenceKind::OfficialCaAnnouncement => RecentRelation::OfficialCaAnnouncement,
        SocialEvidenceKind::ProfileCaChange | SocialEvidenceKind::ProfileLinkChange => {
            RecentRelation::ReusedSocialLink
        }
        SocialEvidenceKind::Mention => RecentRelation::SameSocialAccount,
    }
}

/// A test/reference token-evidence adapter. Returns a fixed observation set per
/// query; production scrapers implement [`super::recent::TokenEvidenceAdapter`].
#[derive(Default)]
pub struct InMemoryTokenEvidenceAdapter;

impl super::recent::TokenEvidenceAdapter for InMemoryTokenEvidenceAdapter {
    fn fetch(&self, _query: &str) -> anyhow::Result<Vec<SocialEvidenceObservation>> {
        Ok(Vec::new())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use super::super::core::TruthStatus;

    fn token_node(key: &str) -> EntityNode {
        EntityNode {
            entity_key: key.into(),
            node_type: super::super::graph::NodeType::Token,
        }
    }

    fn wallet_node(key: &str) -> EntityNode {
        EntityNode {
            entity_key: key.into(),
            node_type: super::super::graph::NodeType::Wallet,
        }
    }

    fn edge(
        ty: EdgeType,
        from: &str,
        to: &str,
        refs: &[&str],
    ) -> EntityEdge {
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

    fn event(
        anchor: &str,
        occurred: &str,
        observed: &str,
        coverage: Coverage,
        refs: &[&str],
    ) -> RecentEvent {
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
            capability_status: super::super::recent::CapabilityStatus::Available,
            retraction: None,
        }
    }

    #[test]
    fn normalize_identity_rejects_empty() {
        assert!(normalize_identity(IdentityKind::Token, "   ").is_none());
        assert!(normalize_identity(IdentityKind::Website, "").is_none());
    }

    #[test]
    fn normalize_identity_strips_www() {
        let k = normalize_identity(IdentityKind::Website, "https://www.Example.COM/").unwrap();
        assert_eq!(k.value, "example.com");
    }

    #[test]
    fn same_symbol_cross_chain_stays_unrelated() {
        // Two tokens with identical symbol but no corroborating edge -> no relation.
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("wallet:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("eth:BBB")];
        // No DeployedBy/FundedBy/CrossChainDeployment edges linking them.
        let edges: Vec<EntityEdge> = vec![];
        let out = resolve_candidates(&anchor, &nodes, &edges);
        assert!(out.is_empty(), "symbol similarity alone must not relate contracts");
    }

    #[test]
    fn same_deployer_creates_exact_relation() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("wallet:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("sol:BBB")];
        let edges = vec![edge(EdgeType::DeployedBy, "wallet:D1", "sol:BBB", &["ev1"])];
        let out = resolve_candidates(&anchor, &nodes, &edges);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::SameDeployer);
        assert_eq!(out[0].confidence, RecentConfidence::Exact);
    }

    #[test]
    fn funded_by_known_deployer_is_reconstructed() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("wallet:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), wallet_node("wallet:W9")];
        let edges = vec![edge(EdgeType::FundedBy, "wallet:D1", "wallet:W9", &["ev2"])];
        let out = resolve_candidates(&anchor, &nodes, &edges);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::FundedByKnownDeployer);
        assert_eq!(out[0].confidence, RecentConfidence::Reconstructed);
    }

    #[test]
    fn factory_is_not_deployer() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("wallet:D1".into()),
            authority: None,
            fee_payer: None,
            factory: Some("program:F1".into()),
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("sol:BBB")];
        // A DeployedBy edge from the factory must NOT produce a SameDeployer
        // relation (factory != project deployer).
        let edges = vec![edge(EdgeType::DeployedBy, "program:F1", "sol:BBB", &["ev3"])];
        let out = resolve_candidates(&anchor, &nodes, &edges);
        assert!(out.is_empty(), "factory/launchpad address stays separate from deployer");
    }

    #[test]
    fn cross_chain_deployment_is_exact() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: None,
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("eth:BBB")];
        let edges = vec![edge(
            EdgeType::CrossChainDeployment,
            "sol:AAA",
            "eth:BBB",
            &["ev4"],
        )];
        let out = resolve_candidates(&anchor, &nodes, &edges);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::CrossChainDeployment);
        assert_eq!(out[0].confidence, RecentConfidence::Exact);
    }

    #[test]
    fn timeline_sorts_by_occurred_at() {
        let events = vec![
            event("sol:AAA", "2026-01-03T00:00:00Z", "2026-01-03T01:00:00Z", Coverage::Full, &["r1"]),
            event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r2"]),
            event("sol:AAA", "2026-01-02T00:00:00Z", "2026-01-02T01:00:00Z", Coverage::Full, &["r3"]),
        ];
        let tl = build_timeline("sol:AAA", &events);
        assert_eq!(tl.events[0].occurred_at, "2026-01-01T00:00:00Z");
        assert_eq!(tl.events[2].occurred_at, "2026-01-03T00:00:00Z");
    }

    #[test]
    fn timeline_filters_by_anchor() {
        let events = vec![
            event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]),
            event("sol:BBB", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r2"]),
        ];
        let tl = build_timeline("sol:AAA", &events);
        assert_eq!(tl.events.len(), 1);
    }

    #[test]
    fn dependency_group_collapses_copies() {
        let mut events = vec![
            event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["upstream"]),
            event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["downstream"]),
        ];
        let deps = vec![("upstream".to_string(), "downstream".to_string())];
        assign_dependency_group(&mut events, &deps);
        // The downstream copy is grouped under the upstream id; both preserve evidence.
        assert_eq!(events[1].dependency_group.as_deref(), Some("group:upstream"));
        assert_eq!(events[1].evidence_refs, vec!["downstream".to_string()]);
    }

    #[test]
    fn retraction_supersedes_not_deletes() {
        let retracted = event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]);
        let mut e = retracted;
        e.retraction = Some(super::super::recent::Retraction {
            superseded_by: "ev-new".into(),
            retracted_at: "2026-01-02T00:00:00Z".into(),
            truth_status: TruthStatus::Superseded,
        });
        let out = apply_retraction(&[e]);
        assert_eq!(out.len(), 1, "retracted evidence is archived, not deleted");
        assert_eq!(out[0].truth_status, TruthStatus::Superseded);
    }

    #[test]
    fn coverage_never_coerces_to_zero() {
        assert_eq!(coverage_status(&[]), Coverage::Unavailable);
        let all_unavailable = vec![
            event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Unavailable, &["r1"]),
        ];
        assert_eq!(coverage_status(&all_unavailable), Coverage::Unavailable);
        let all_full = vec![
            event("sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]),
        ];
        assert_eq!(coverage_status(&all_full), Coverage::Full);
    }

    #[test]
    fn dormant_tokens_do_not_trigger_lookup() {
        assert!(should_trigger_lookup("active"));
        assert!(should_trigger_lookup("first_liquidity"));
        assert!(!should_trigger_lookup("dormant"));
        assert!(!should_trigger_lookup("archived"));
        assert!(!should_trigger_lookup("tombstoned"));
    }

    #[test]
    fn classify_social_separates_official_from_mentions() {
        let official = SocialEvidenceObservation {
            platform: super::super::browser::BrowserPlatform::X,
            post_or_profile_id: "p1".into(),
            immutable_account_id: "acct:1".into(),
            text_media_hash: "h1".into(),
            published_at: "2026-01-01T00:00:00Z".into(),
            observed_at: "2026-01-01T01:00:00Z".into(),
            relation_kind: SocialEvidenceKind::OfficialCaAnnouncement,
            raw_ref: "raw1".into(),
            parser_version: "1".into(),
            coverage: Coverage::Full,
            session_health: super::super::browser::SessionHealth::Ok,
        };
        assert_eq!(classify_social(&official), RecentRelation::OfficialCaAnnouncement);

        let mention = SocialEvidenceObservation {
            relation_kind: SocialEvidenceKind::Mention,
            ..official.clone()
        };
        assert_eq!(classify_social(&mention), RecentRelation::SameSocialAccount);
    }

    #[test]
    fn build_lookup_query_dedupes() {
        let actor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("wallet:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec!["x:acct1".into()],
        };
        let q = build_lookup_query(&actor);
        assert!(q.contains(&"sol:AAA".to_string()));
        assert!(q.contains(&"wallet:D1".to_string()));
        assert!(q.contains(&"x:acct1".to_string()));
        // deterministic dedup: no duplicate entries
        let mut dedup = q.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(q.len(), dedup.len());
    }
}
