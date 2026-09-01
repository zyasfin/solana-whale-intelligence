//! Runtime logic: Token Recent + Deployer/Social Reuse (REV-020).
//!
//! Pure logic over the frozen `recent.rs` types plus the existing graph model.
//! Mirrors the `ingest_runtime.rs` pattern: no persistence/transport in this
//! module — it resolves candidates from in-hand nodes/edges, builds timelines,
//! assigns source-dependency groups, applies retraction, and computes coverage.

use std::collections::HashSet;

use super::graph::{EdgeType, EntityEdge, EntityNode};
use super::recent::{
    ActivationTrigger, ActorExtraction, CandidateRelation, Coverage, IdentityKey, IdentityKind,
    OfficialSocialBinding, RecentConfidence, RecentEvent, RecentRelation, RecentTimeline,
    Retraction, SocialEvidenceKind, SocialEvidenceObservation,
};
use super::token::TokenLifecycle;

/// Normalize a raw identity value into a chain-qualified key. Fail-closed
/// (REV-023 §2 / addendum #2):
/// - empty/whitespace-only values return `None`;
/// - Token/Wallet identities must be canonical `chain:address` (a non-empty
///   chain prefix and a non-empty address/contract remainder); arbitrary or
///   unqualified strings are rejected;
/// - Social identities require `platform:immutable_user_id`;
/// - Websites are normalized to a lowercase host with a leading `www.` stripped
///   (minimal registrable-domain approximation; full PSL is out of scope).
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
        IdentityKind::Token | IdentityKind::Wallet => {
            // Must be a chain-qualified, known-chain key (REV-025-F03).
            canonical_chain(value)?;
            Some(IdentityKey {
                kind,
                value: value.to_string(),
            })
        }
        IdentityKind::Social => {
            // Must be `platform:immutable_user_id` (any non-empty platform).
            chain_prefix(value)?;
            Some(IdentityKey {
                kind,
                value: value.to_string(),
            })
        }
        IdentityKind::Telegram => {
            // Must be `platform:chat_id`.
            chain_prefix(value)?;
            Some(IdentityKey {
                kind,
                value: value.to_string(),
            })
        }
    }
}

/// Split a chain-qualified key into `(chain, rest)` without validating the
/// chain (used for social/telegram platform prefixes).
fn chain_prefix(key: &str) -> Option<&str> {
    let (chain, rest) = key.split_once(':')?;
    if chain.is_empty() || rest.is_empty() {
        return None;
    }
    Some(chain)
}

/// Parse an RFC3339 timestamp into unix seconds (None when unparseable).
fn parse_secs(s: &str) -> Option<i64> {
    s.parse::<chrono::DateTime<chrono::Utc>>()
        .ok()
        .map(|dt| dt.timestamp())
}

/// Canonicalize the chain/platform prefix of a chain-qualified key
/// (`chain:rest`) against the frozen chain vocabulary (REV-025-F03). Returns
/// the canonical chain string, or `None` when the key is not chain-qualified,
/// the prefix is empty, or the chain is unknown/alias-not-recognized.
fn canonical_chain(key: &str) -> Option<&'static str> {
    let (chain, rest) = key.split_once(':')?;
    if chain.is_empty() || rest.is_empty() {
        return None;
    }
    match chain.to_ascii_lowercase().as_str() {
        "sol" | "solana" => Some("solana"),
        "rh" | "robinhood" => Some("robinhood"),
        "eth" | "ethereum" => Some("ethereum"),
        "base" => Some("base"),
        "bsc" => Some("bsc"),
        _ => None,
    }
}

/// Whether an edge is authoritative enough to support an `Exact`/family merge
/// (REV-023 §2). All of the following must hold, otherwise the edge stays a
/// candidate/non-Exact:
/// - `truth_status == Confirmed` (disputed/erroneous/unknown never Exact),
/// - non-empty `evidence_refs`,
/// - within its active validity window at `now_secs` (fail-closed on malformed
///   `valid_from`; an unparseable `valid_until` also fails closed).
fn edge_is_authoritative(edge: &EntityEdge, now_secs: i64) -> bool {
    if edge.truth_status != super::core::TruthStatus::Confirmed {
        return false;
    }
    if edge.evidence_refs.is_empty() {
        return false;
    }
    let Some(from) = parse_secs(&edge.valid_from) else {
        return false;
    };
    if now_secs < from {
        return false;
    }
    match &edge.valid_until {
        Some(until) => parse_secs(until).map(|u| now_secs <= u).unwrap_or(false),
        None => true,
    }
}

/// Resolve corroborated candidate relations for an anchor token from reverse
/// indexes (`nodes` + `edges`). Deterministic output ordered by
/// `(confidence rank desc, relation name, to_identity.value)`.
///
/// Rules (REV-020 minimum acceptance #1–#6, REV-023 §2):
/// - Same chain-qualified deployer/authority/fee-payer/funder → exact relation
///   ONLY when the supporting edge is authoritative (Confirmed + evidence +
///   valid window). Factory/launchpad address is NOT a deployer.
/// - New wallet funded by a known deployer is `Reconstructed` ONLY with
///   funding + reused social/domain + coherent time; funding alone stays a
///   weaker candidate (`Estimated`).
/// - Reused handle/domain without immutable ownership → `Insufficient`/candidate.
/// - First-party exact-CA announcement → `OfficialCaAnnouncement` / `Exact`.
/// - Explicit authoritative cross-chain announcement → `CrossChainDeployment` /
///   `Exact`; requires distinct chains + authoritative edge; symbol/name
///   similarity alone never yields a relation.
/// - Social reuse returns the OTHER token/project using the identity, never the
///   social identity itself.
pub fn resolve_candidates(
    anchor: &ActorExtraction,
    nodes: &[EntityNode],
    edges: &[EntityEdge],
    now_secs: i64,
) -> Vec<CandidateRelation> {
    let mut out: Vec<CandidateRelation> = Vec::new();
    let mut seen: HashSet<(String, RecentRelation)> = HashSet::new();

    let anchor_token = &anchor.token;
    // Fail-closed: an anchor with an unknown/unqualified chain resolves nothing.
    let Some(anchor_chain) = canonical_chain(anchor_token) else {
        return out;
    };

    // Cross-chain deployment: an authoritative edge from the anchor token to a
    // token on a DIFFERENT chain. Same-chain or non-authoritative edges never
    // produce a family merge.
    for e in edges {
        if e.edge_type != EdgeType::CrossChainDeployment {
            continue;
        }
        if e.from_entity_key != *anchor_token {
            continue;
        }
        // Distinct canonical chains + authoritative edge required.
        let to_chain = canonical_chain(&e.to_entity_key);
        let distinct_chain = match to_chain {
            Some(t) => t != anchor_chain,
            None => false,
        };
        if !distinct_chain || !edge_is_authoritative(e, now_secs) {
            continue;
        }
        let confidence = if edge_is_authoritative(e, now_secs) {
            RecentConfidence::Exact
        } else {
            RecentConfidence::Insufficient
        };
        let rel = CandidateRelation {
            to_identity: IdentityKey {
                kind: IdentityKind::Token,
                value: e.to_entity_key.clone(),
            },
            relation: RecentRelation::CrossChainDeployment,
            confidence,
            evidence_refs: e.evidence_refs.clone(),
        };
        let key = (e.to_entity_key.clone(), rel.relation);
        if seen.insert(key) {
            out.push(rel);
        }
    }

    // Actor-based reverse lookup: for each token node, compare
    // deployer/authority/fee-payer/funder carried in its payload to the
    // anchor's actors. Only authoritative edges become Exact.
    let token_nodes: Vec<&EntityNode> = nodes
        .iter()
        .filter(|n| n.node_type == super::graph::NodeType::Token)
        .collect();

    for node in token_nodes {
        let other = &node.entity_key;
        if other == anchor_token {
            continue;
        }
        for e in edges.iter().filter(|e| e.to_entity_key == *other) {
            let actor = &e.from_entity_key;
            // Factory/launchpad is never a project deployer.
            if anchor.factory.as_deref() == Some(actor.as_str()) {
                continue;
            }
            let authoritative = edge_is_authoritative(e, now_secs);
            let relation = match e.edge_type {
                EdgeType::DeployedBy
                    if anchor.deployer.as_deref() == Some(actor.as_str()) =>
                {
                    Some((RecentRelation::SameDeployer, authoritative))
                }
                EdgeType::FundedBy
                    if anchor.initial_funder.as_deref() == Some(actor.as_str()) =>
                {
                    Some((RecentRelation::SameFunder, authoritative))
                }
                _ => None,
            };
            // SameAuthority / SameFeePayer are resolved from explicit edges
            // carrying the matching actor (REV-022-F01: these were never emitted).
            let relation = relation.or_else(|| match e.edge_type {
                EdgeType::SameAuthority
                    if anchor.authority.as_deref() == Some(actor.as_str()) =>
                {
                    Some((RecentRelation::SameAuthority, authoritative))
                }
                EdgeType::SameFeePayer
                    if anchor.fee_payer.as_deref() == Some(actor.as_str()) =>
                {
                    Some((RecentRelation::SameFeePayer, authoritative))
                }
                _ => None,
            });
            if let Some((relation, is_exact)) = relation {
                let confidence = if is_exact {
                    RecentConfidence::Exact
                } else {
                    RecentConfidence::Insufficient
                };
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

    // A new wallet funded by a known deployer. `Reconstructed` requires
    // funding + reused immutable social/domain evidence + coherent time;
    // funding alone is only `Estimated` (REV-023 addendum #3).
    for e in edges {
        if e.edge_type != EdgeType::FundedBy {
            continue;
        }
        if anchor.deployer.as_deref() != Some(e.from_entity_key.as_str()) {
            continue;
        }
        let is_wallet = nodes.iter().any(|n| {
            n.entity_key == e.to_entity_key && n.node_type == super::graph::NodeType::Wallet
        });
        if !is_wallet {
            continue;
        }
        let authoritative = edge_is_authoritative(e, now_secs);
        // A real reused-social/domain edge: the funded wallet (or a token bound
        // to it) reuses one of the anchor's immutable social identities
        // (REV-025-F02). Merely *having* a social identity is not evidence.
        let reused_social_edge = edges.iter().any(|re| {
            re.edge_type == EdgeType::ReusedSocialLink
                && re.from_entity_key == e.to_entity_key
                && anchor.social_identities.iter().any(|s| &re.to_entity_key == s)
        });
        let coherent_time = parse_secs(&e.valid_from)
            .zip(parse_secs(&e.occurred_at))
            .map(|(vf, occ)| occ >= vf)
            .unwrap_or(false);
        let confidence = if authoritative && reused_social_edge && coherent_time {
            RecentConfidence::Reconstructed
        } else if authoritative {
            RecentConfidence::Estimated
        } else {
            RecentConfidence::Insufficient
        };
        let rel = CandidateRelation {
            to_identity: IdentityKey {
                kind: IdentityKind::Wallet,
                value: e.to_entity_key.clone(),
            },
            relation: RecentRelation::FundedByKnownDeployer,
            confidence,
            evidence_refs: e.evidence_refs.clone(),
        };
        let key = (e.to_entity_key.clone(), rel.relation);
        if seen.insert(key) {
            out.push(rel);
        }
    }

    // Reused social link without immutable-ownership evidence → candidate only.
    // The relation resolves to the OTHER token/project using the social
    // identity, never the social identity itself (REV-022-F01).
    for identity in &anchor.social_identities {
        for e in edges {
            if e.edge_type != EdgeType::ReusedSocialLink {
                continue;
            }
            if e.to_entity_key == *identity {
                // `from` is the other project reusing the identity. Only a TOKEN
                // project produces a ReusedSocialLink candidate relation; a
                // wallet reusing the identity is used for `Reconstructed`
                // corroboration, not a standalone reuse relation.
                let is_token = nodes.iter().any(|n| {
                    n.entity_key == e.from_entity_key
                        && n.node_type == super::graph::NodeType::Token
                });
                if !is_token {
                    continue;
                }
                let other_token = e.from_entity_key.clone();
                let rel = CandidateRelation {
                    to_identity: IdentityKey {
                        kind: IdentityKind::Token,
                        value: other_token,
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
/// `occurred_at`/`observed_at` are typed `DateTime<Utc>`; ordering is
/// chronological (never lexicographic, REV-022-F08).
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

/// Resolve the transitive root of a source-dependency edge. `deps` holds
/// `(upstream_ref, downstream_ref)` pairs where a downstream evidence ref is a
/// repost/provider copy of the upstream ref. Resolution is transitive
/// (REV-023 §4): for `u → d → d2`, every correlated event maps to root `u`.
/// Returns a mapping `ref -> root ref`.
fn dependency_roots(deps: &[(String, String)]) -> std::collections::HashMap<String, String> {
    // parent: downstream -> immediate upstream
    let mut parent: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for (upstream, downstream) in deps {
        parent.insert(downstream.clone(), upstream.clone());
    }
    let mut roots: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    fn find_root(
        r: &str,
        parent: &std::collections::HashMap<String, String>,
        roots: &mut std::collections::HashMap<String, String>,
    ) -> String {
        if let Some(cached) = roots.get(r) {
            return cached.clone();
        }
        let root = match parent.get(r) {
            Some(up) => find_root(up, parent, roots),
            None => r.to_string(),
        };
        roots.insert(r.to_string(), root.clone());
        root
    }
    for r in parent.keys() {
        find_root(r, &parent, &mut roots);
    }
    for (upstream, _) in deps {
        roots.entry(upstream.clone()).or_insert_with(|| upstream.clone());
    }
    roots
}

/// Assign a source-dependency group to each event using transitive dependency
/// resolution. Every `evidence_refs` entry is preserved. Events already
/// carrying a `dependency_group` are untouched. An event citing a downstream
/// ref is grouped under its transitive upstream root; an event citing only an
/// upstream (or an unrelated ref) self-groups.
pub fn assign_dependency_group(events: &mut [RecentEvent], deps: &[(String, String)]) {
    let roots = dependency_roots(deps);
    for e in events.iter_mut() {
        if e.dependency_group.is_some() {
            continue;
        }
        // Find the transitive root of any evidence ref this event cites.
        let root = e
            .evidence_refs
            .iter()
            .find_map(|r| roots.get(r).cloned())
            .or_else(|| e.evidence_refs.first().cloned());
        if let Some(root) = root {
            e.dependency_group = Some(format!("group:{root}"));
        }
    }
}

/// Apply retraction/supersession semantics (archive-not-delete, acceptance #8,
/// REV-023 §4). `retractions` are separate append-only rows bound to a real
/// target `event_id`. For each retraction:
/// - a dangling target (no matching event) is ignored,
/// - an illegal status (not `Superseded`/`Erroneous`) is ignored,
/// - otherwise the target event's `truth_status` is downgraded to the
///   retraction's status and its `retraction` field is populated.
/// Returns the full list including superseded entries (append-only). Both the
/// superseded and superseding rows remain archived.
pub fn apply_retraction(
    events: &[RecentEvent],
    retractions: &[Retraction],
) -> Vec<RecentEvent> {
    let mut out: Vec<RecentEvent> = events.to_vec();
    for retraction in retractions {
        let is_legal = matches!(
            retraction.truth_status,
            super::core::TruthStatus::Superseded | super::core::TruthStatus::Erroneous
        );
        if !is_legal {
            continue;
        }
        if let Some(target) = out
            .iter_mut()
            .find(|e| e.event_id == retraction.target_event_id)
        {
            target.truth_status = retraction.truth_status;
            target.retraction = Some(retraction.clone());
        }
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

/// Whether a token lifecycle should trigger a recent-intelligence lookup
/// (typed gate, REV-023 §3). No trigger means no lookup. Dormant/dead tokens
/// allow only authorized event-wake or operator request; unknown lifecycle
/// fails closed. Cheap-first/event-wake semantics remain authoritative.
pub fn should_trigger_lookup(
    lifecycle: TokenLifecycle,
    trigger: Option<ActivationTrigger>,
) -> bool {
    let Some(trigger) = trigger else {
        return false; // no trigger -> no lookup
    };
    match lifecycle {
        TokenLifecycle::Dormant | TokenLifecycle::Archived | TokenLifecycle::Tombstoned => {
            // Event-wake only: global revival wake or operator request.
            matches!(
                trigger,
                ActivationTrigger::RevivalWake | ActivationTrigger::OperatorRequest
            )
        }
        TokenLifecycle::Created
        | TokenLifecycle::PreGraduation
        | TokenLifecycle::Migrated
        | TokenLifecycle::FirstLiquidity
        | TokenLifecycle::Active
        | TokenLifecycle::Cooling => true,
    }
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
    if let Some(f) = &actor.factory {
        push(f.clone());
    }
    if let Some(f) = &actor.initial_funder {
        push(f.clone());
    }
    for c in &actor.authority_changes {
        push(c.clone());
    }
    for s in &actor.social_identities {
        push(s.clone());
    }
    terms
}

/// Classify a social observation into a recent relation (authoritative,
/// REV-022-F02 / REV-023 §3). `OFFICIAL_CA_ANNOUNCEMENT` is produced ONLY when
/// all of the following hold:
/// - the observation's `relation_kind` is `OfficialCaAnnouncement` (a mention
///   never becomes official),
/// - the extracted announced CA equals the canonical anchor contract,
/// - the author's immutable account ID has an official binding to the anchor
///   contract that was valid at `obs.published_at`,
/// - the raw evidence ref is non-empty.
/// Wrong CA, unrelated author, expired/missing binding, missing evidence, and
/// plain mention are downgraded to `SameSocialAccount`. A profile CA/link
/// change → `ReusedSocialLink`.
pub fn classify_social(
    obs: &SocialEvidenceObservation,
    anchor_contract: &str,
    official_binding: Option<&OfficialSocialBinding>,
) -> RecentRelation {
    match obs.relation_kind {
        SocialEvidenceKind::ProfileCaChange | SocialEvidenceKind::ProfileLinkChange => {
            RecentRelation::ReusedSocialLink
        }
        SocialEvidenceKind::OfficialCaAnnouncement => {
            let ca_matches = obs
                .announced_contract
                .as_deref()
                .map(|ca| ca == anchor_contract)
                .unwrap_or(false);
            let binding_valid = official_binding.map_or(false, |b| {
                b.immutable_account_id == obs.immutable_account_id
                    && b.chain_qualified_contract == anchor_contract
                    && obs.published_at >= b.valid_from
                    && b
                        .valid_until
                        .map(|u| obs.published_at <= u)
                        .unwrap_or(true)
            });
            let has_evidence = !obs.raw_ref.trim().is_empty();
            if ca_matches && binding_valid && has_evidence {
                RecentRelation::OfficialCaAnnouncement
            } else {
                RecentRelation::SameSocialAccount
            }
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

    const NOW: i64 = 1_800_000_000; // 2027-01-15T00:00:00Z-ish, after all test edges.

    fn dt(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse::<chrono::DateTime<chrono::Utc>>().unwrap()
    }

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
        id: &str,
        anchor: &str,
        occurred: &str,
        observed: &str,
        coverage: Coverage,
        refs: &[&str],
    ) -> RecentEvent {
        RecentEvent {
            event_id: id.into(),
            event_type: "transfer".into(),
            anchor_identity: anchor.into(),
            related_identities: vec![],
            chain_qualified_contract: anchor.into(),
            occurred_at: dt(occurred),
            observed_at: dt(observed),
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
    fn normalize_identity_rejects_unqualified() {
        // Token/Wallet/Social identities must be chain-qualified.
        assert!(normalize_identity(IdentityKind::Token, "AAA").is_none());
        assert!(normalize_identity(IdentityKind::Wallet, "wallet-without-chain").is_none());
        assert!(normalize_identity(IdentityKind::Token, "sol:").is_none());
        assert!(normalize_identity(IdentityKind::Token, "sol:AAA").is_some());
    }

    #[test]
    fn normalize_identity_strips_www() {
        let k = normalize_identity(IdentityKind::Website, "https://www.Example.COM/").unwrap();
        assert_eq!(k.value, "example.com");
    }

    #[test]
    fn same_symbol_cross_chain_stays_unrelated() {
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
        let edges: Vec<EntityEdge> = vec![];
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
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
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::SameDeployer);
        assert_eq!(out[0].confidence, RecentConfidence::Exact);
    }

    #[test]
    fn same_authority_and_fee_payer_resolve() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: None,
            authority: Some("wallet:A1".into()),
            fee_payer: Some("wallet:F1".into()),
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("sol:BBB")];
        let edges = vec![
            edge(EdgeType::SameAuthority, "wallet:A1", "sol:BBB", &["ev-a"]),
            edge(EdgeType::SameFeePayer, "wallet:F1", "sol:BBB", &["ev-f"]),
        ];
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
        assert!(out.iter().any(|c| c.relation == RecentRelation::SameAuthority));
        assert!(out.iter().any(|c| c.relation == RecentRelation::SameFeePayer));
    }

    #[test]
    fn disputed_cross_chain_edge_is_not_exact() {
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
        let mut e = edge(EdgeType::CrossChainDeployment, "sol:AAA", "eth:BBB", &["ev"]);
        e.truth_status = TruthStatus::Disputed;
        let edges = vec![e];
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
        assert!(out.is_empty(), "disputed cross-chain edge must not produce Exact");
    }

    #[test]
    fn funded_by_known_deployer_is_reconstructed() {
        // Funding + reused social identity + coherent time -> Reconstructed.
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("wallet:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec!["x:acct1".into()],
        };
        let nodes = vec![token_node("sol:AAA"), wallet_node("wallet:W9")];
        // Funding edge + a real reused-social edge (wallet reuses the anchor's
        // social identity) -> Reconstructed.
        let edges = vec![
            edge(EdgeType::FundedBy, "wallet:D1", "wallet:W9", &["ev2"]),
            edge(EdgeType::ReusedSocialLink, "wallet:W9", "x:acct1", &["ev-reuse"]),
        ];
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::FundedByKnownDeployer);
        assert_eq!(out[0].confidence, RecentConfidence::Reconstructed);
    }

    #[test]
    fn funding_alone_is_not_reconstructed() {
        // No social corroboration -> funding alone is Estimated, not Reconstructed.
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
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, RecentConfidence::Estimated);
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
        let edges = vec![edge(EdgeType::DeployedBy, "program:F1", "sol:BBB", &["ev3"])];
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
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
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::CrossChainDeployment);
        assert_eq!(out[0].confidence, RecentConfidence::Exact);
    }

    #[test]
    fn social_reuse_returns_other_token() {
        // Reuse relation resolves to the OTHER token, not the social identity.
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: None,
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec!["x:acct1".into()],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("sol:BBB")];
        let edges = vec![edge(EdgeType::ReusedSocialLink, "sol:BBB", "x:acct1", &["ev-r"])];
        let out = resolve_candidates(&anchor, &nodes, &edges, NOW);
        assert!(out.iter().any(|c| c.to_identity.value == "sol:BBB"));
        assert!(!out.iter().any(|c| c.to_identity.value == "x:acct1"));
    }

    #[test]
    fn timeline_sorts_by_occurred_at() {
        let events = vec![
            event("e1", "sol:AAA", "2026-01-03T00:00:00Z", "2026-01-03T01:00:00Z", Coverage::Full, &["r1"]),
            event("e2", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r2"]),
            event("e3", "sol:AAA", "2026-01-02T00:00:00Z", "2026-01-02T01:00:00Z", Coverage::Full, &["r3"]),
        ];
        let tl = build_timeline("sol:AAA", &events);
        assert_eq!(tl.events[0].occurred_at, dt("2026-01-01T00:00:00Z"));
        assert_eq!(tl.events[2].occurred_at, dt("2026-01-03T00:00:00Z"));
    }

    #[test]
    fn timeline_orders_mixed_offsets_chronologically() {
        // `b` has a -01:00 offset, so its instant (2026-01-01T00:30:00Z) is LATER
        // than `a` (2026-01-01T00:00:00Z), even though its lexical string
        // ("2025-12-31...") sorts first.
        let a = event("a", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z", Coverage::Full, &["ra"]);
        let b = event("b", "sol:AAA", "2025-12-31T23:30:00-01:00", "2025-12-31T23:31:00-01:00", Coverage::Full, &["rb"]);
        let tl = build_timeline("sol:AAA", &[b.clone(), a.clone()]);
        assert_eq!(tl.events[0].event_id, "a");
        assert_eq!(tl.events[1].event_id, "b");
    }
    #[test]
    fn timeline_filters_by_anchor() {
        let events = vec![
            event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]),
            event("e2", "sol:BBB", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r2"]),
        ];
        let tl = build_timeline("sol:AAA", &events);
        assert_eq!(tl.events.len(), 1);
    }

    #[test]
    fn dependency_group_collapses_copies_transitively() {
        let mut events = vec![
            event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["upstream"]),
            event("e2", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["downstream"]),
            event("e3", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["downstream2"]),
        ];
        // u -> d -> d2 : d2 must resolve to root u.
        let deps = vec![
            ("upstream".to_string(), "downstream".to_string()),
            ("downstream".to_string(), "downstream2".to_string()),
        ];
        assign_dependency_group(&mut events, &deps);
        assert_eq!(events[1].dependency_group.as_deref(), Some("group:upstream"));
        assert_eq!(events[2].dependency_group.as_deref(), Some("group:upstream"));
    }

    #[test]
    fn retraction_resolves_target_not_deletes() {
        let e = event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]);
        let retractions = vec![Retraction {
            target_event_id: "e1".into(),
            retracted_at: dt("2026-01-02T00:00:00Z"),
            truth_status: TruthStatus::Superseded,
        }];
        let out = apply_retraction(&[e], &retractions);
        assert_eq!(out.len(), 1, "retracted evidence is archived, not deleted");
        assert_eq!(out[0].truth_status, TruthStatus::Superseded);
        assert!(out[0].retraction.is_some());
    }

    #[test]
    fn retraction_ignores_illegal_status_and_dangling_target() {
        let e = event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]);
        let retractions = vec![
            Retraction {
                target_event_id: "e1".into(),
                retracted_at: dt("2026-01-02T00:00:00Z"),
                truth_status: TruthStatus::Confirmed, // illegal
            },
            Retraction {
                target_event_id: "dangling".into(),
                retracted_at: dt("2026-01-02T00:00:00Z"),
                truth_status: TruthStatus::Erroneous,
            },
        ];
        let out = apply_retraction(&[e], &retractions);
        assert_eq!(out[0].truth_status, TruthStatus::Confirmed, "illegal status ignored");
        assert!(out[0].retraction.is_none());
    }

    #[test]
    fn coverage_never_coerces_to_zero() {
        assert_eq!(coverage_status(&[]), Coverage::Unavailable);
        let all_unavailable = vec![
            event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Unavailable, &["r1"]),
        ];
        assert_eq!(coverage_status(&all_unavailable), Coverage::Unavailable);
        let all_full = vec![
            event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]),
        ];
        assert_eq!(coverage_status(&all_full), Coverage::Full);
    }

    #[test]
    fn trigger_requires_typed_gate() {
        use ActivationTrigger::*;
        // No trigger -> no lookup.
        assert!(!should_trigger_lookup(TokenLifecycle::Active, None));
        // Unknown lifecycle fails closed (represented by a terminal variant: dormant).
        assert!(!should_trigger_lookup(TokenLifecycle::Dormant, Some(FirstLiquidity)));
        // Dormant/dead only wake/operator.
        assert!(should_trigger_lookup(TokenLifecycle::Dormant, Some(RevivalWake)));
        assert!(should_trigger_lookup(TokenLifecycle::Dormant, Some(OperatorRequest)));
        assert!(!should_trigger_lookup(TokenLifecycle::Archived, Some(VolumeActivation)));
        assert!(!should_trigger_lookup(TokenLifecycle::Tombstoned, Some(FirstLiquidity)));
        // Active/created/cooling with any explicit activation trigger passes.
        assert!(should_trigger_lookup(TokenLifecycle::Active, Some(FirstLiquidity)));
        assert!(should_trigger_lookup(TokenLifecycle::Created, Some(Migration)));
        assert!(should_trigger_lookup(TokenLifecycle::Cooling, Some(VolumeActivation)));
    }

    #[test]
    fn classify_social_verifies_ca_and_binding() {
        let binding = OfficialSocialBinding {
            immutable_account_id: "x:acct_immutable_1".into(),
            chain_qualified_contract: "sol:AAA".into(),
            valid_from: dt("2025-01-01T00:00:00Z"),
            valid_until: None,
        };
        let base = SocialEvidenceObservation {
            platform: super::super::browser::BrowserPlatform::X,
            post_or_profile_id: "p1".into(),
            immutable_account_id: "x:acct_immutable_1".into(),
            text_media_hash: "h1".into(),
            published_at: dt("2026-01-01T00:00:00Z"),
            observed_at: dt("2026-01-01T01:00:00Z"),
            announced_contract: Some("sol:AAA".into()),
            relation_kind: SocialEvidenceKind::OfficialCaAnnouncement,
            raw_ref: "raw1".into(),
            parser_version: "1".into(),
            coverage: Coverage::Full,
            session_health: super::super::browser::SessionHealth::Ok,
        };
        // Valid: exact CA + valid binding + evidence -> official.
        assert_eq!(
            classify_social(&base, "sol:AAA", Some(&binding)),
            RecentRelation::OfficialCaAnnouncement
        );
        // Wrong CA -> downgraded.
        let wrong_ca = SocialEvidenceObservation {
            announced_contract: Some("sol:OTHER".into()),
            ..base.clone()
        };
        assert_eq!(
            classify_social(&wrong_ca, "sol:AAA", Some(&binding)),
            RecentRelation::SameSocialAccount
        );
        // Missing binding -> downgraded.
        assert_eq!(
            classify_social(&base, "sol:AAA", None),
            RecentRelation::SameSocialAccount
        );
        // Mention never official.
        let mention = SocialEvidenceObservation {
            relation_kind: SocialEvidenceKind::Mention,
            ..base.clone()
        };
        assert_eq!(
            classify_social(&mention, "sol:AAA", Some(&binding)),
            RecentRelation::SameSocialAccount
        );
    }

    #[test]
    fn build_lookup_query_dedupes() {
        let actor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("wallet:D1".into()),
            authority: Some("wallet:A1".into()),
            fee_payer: Some("wallet:F1".into()),
            factory: Some("program:FACTORY".into()),
            initial_funder: Some("wallet:FUND1".into()),
            authority_changes: vec!["wallet:OLD1".into()],
            social_identities: vec!["x:acct1".into()],
        };
        let q = build_lookup_query(&actor);
        assert!(q.contains(&"sol:AAA".to_string()));
        assert!(q.contains(&"wallet:D1".to_string()));
        assert!(q.contains(&"program:FACTORY".to_string()));
        assert!(q.contains(&"wallet:OLD1".to_string()));
        assert!(q.contains(&"x:acct1".to_string()));
        // deterministic dedup: no duplicate entries
        let mut dedup = q.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(q.len(), dedup.len());
    }
}
