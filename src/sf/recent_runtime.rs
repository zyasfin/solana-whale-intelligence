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
            // Must be a chain-qualified, known-chain key (REV-025-F03), and the
            // emitted key is CANONICAL: REV-028 validated the chain but stored the
            // caller's alias verbatim, so `sol:AAA` and `solana:AAA` stayed two
            // distinct identities for the same asset (REV-029/REV-025-F03).
            let chain = canonical_chain(value)?;
            let (_, rest) = value.split_once(':')?;
            Some(IdentityKey {
                kind,
                value: format!("{chain}:{rest}"),
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

/// Whether a social identity is an IMMUTABLE platform account ID, established by
/// authoritative platform evidence (REV-031/REV-025-F02).
///
/// REV-030 inferred this from the string's SHAPE: anything without `@`, `/` or
/// whitespace counted as immutable, so `x:alice` — a plain handle — was accepted
/// and promoted funding to `Reconstructed`. Shape is not provenance. A handle can
/// be renamed or transferred; only the platform's own account/user ID cannot.
///
/// The decision is now made against the workspace's `social_identities` records
/// (migration 1018 + the versioned key from 1021), which store the immutable
/// `immutable_user_id` alongside the handles observed for it. An identity counts
/// as immutable only when a record asserts that this exact value IS the account
/// ID for that platform.
///
/// Fail-closed: no matching record means not-immutable, which merely keeps
/// confidence at `Estimated`. It never fabricates a relation.
pub fn is_immutable_social_identity(
    key: &str,
    bindings: &[SocialIdentityRecord],
) -> bool {
    let Some((platform, id)) = key.split_once(':') else {
        return false;
    };
    let platform = platform.trim().to_ascii_lowercase();
    let id = id.trim();
    if platform.is_empty() || id.is_empty() {
        return false;
    }
    bindings.iter().any(|b| {
        b.platform.trim().to_ascii_lowercase() == platform
            && b.immutable_user_id.trim() == id
            // A value that the record itself lists as a HANDLE is a mutable label,
            // even if it also happens to appear as an id string somewhere.
            && !b
                .handles
                .iter()
                .any(|h| h.trim().eq_ignore_ascii_case(id))
    })
}

/// An authoritative social-identity record, projected from `social_identities`.
///
/// `immutable_user_id` is the platform's own account/user ID; `handles` are the
/// mutable labels observed for it (current + historical). Keeping both lets the
/// resolver tell an account ID apart from a handle instead of guessing from
/// punctuation (REV-031/REV-025-F02).
///
/// FIELDS ARE PRIVATE, and the type is neither `Deserialize` nor constructible by
/// a caller (REV-033/REV-034). REV-032 gave it public fields, so a caller simply
/// built the record it wanted and handed it in — the reviewer's probe reported
/// `forged_reconstructed=true`. That was the fifth instance of the same mistake in
/// this ledger: authority the caller can mint (GUC → audit row → public grant →
/// caller-supplied slice → caller-built struct).
///
/// In production the only way to obtain these records is
/// [`load_social_identity_records`], which reads the workspace-scoped
/// `social_identities` current view. A `#[cfg(test)]` mint helper exists for unit
/// tests, exactly as with `DurableAppendReceipt` and `VerifiedStageProof`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocialIdentityRecord {
    platform: String,
    immutable_user_id: String,
    handles: Vec<String>,
}

impl SocialIdentityRecord {
    /// Platform this record belongs to (`x`, `telegram`, ...).
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// The platform's own immutable account/user ID.
    pub fn immutable_user_id(&self) -> &str {
        &self.immutable_user_id
    }

    /// Mutable labels observed for this account (current + historical).
    pub fn handles(&self) -> &[String] {
        &self.handles
    }

    /// `#[cfg(test)]` mint helper: unit tests need fixtures, production callers
    /// must go through [`records_from_store`].
    #[cfg(test)]
    pub fn mint(platform: &str, immutable_user_id: &str, handles: &[&str]) -> Self {
        Self {
            platform: platform.to_string(),
            immutable_user_id: immutable_user_id.to_string(),
            handles: handles.iter().map(|h| h.to_string()).collect(),
        }
    }
}

/// Convert authoritative store rows into resolver-trusted records.
///
/// `pub(crate)` (REV-035-#4). REV-034 left this `pub`, so an external caller could
/// hand it hand-built rows and still obtain `Reconstructed`. The rows themselves
/// are now unmintable outside the crate ([`super::recent::StoredSocialIdentity`]),
/// and this converter is no longer part of the public surface either — belt and
/// braces, because the whole point is that authority cannot be produced by a
/// caller.
///
/// Rows that are not usable as identity evidence are DROPPED rather than passed
/// through: an empty platform or account ID cannot establish ownership, and a row
/// whose account ID also appears among its own handles is self-contradictory.
/// Fail-closed — a dropped row lowers confidence, it never fabricates a relation.
pub(crate) fn records_from_store(
    rows: impl IntoIterator<Item = super::recent::StoredSocialIdentity>,
) -> Vec<SocialIdentityRecord> {
    rows.into_iter()
        .filter_map(|r| {
            let platform = r.platform().trim().to_ascii_lowercase();
            let id = r.immutable_user_id().trim().to_string();
            if platform.is_empty() || id.is_empty() {
                return None;
            }
            if r
                .handles()
                .iter()
                .any(|h| h.trim().eq_ignore_ascii_case(&id))
            {
                return None; // contradictory: the "account ID" is also a handle
            }
            Some(SocialIdentityRecord {
                platform,
                immutable_user_id: id,
                handles: r.handles().to_vec(),
            })
        })
        .collect()
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

/// Whether two actor keys denote the same on-chain actor (REV-033/REV-034).
///
/// Both sides are canonicalized before comparison, so `sol:D` and `solana:D` are
/// one actor. Fail-closed in both directions:
///   * a `None` anchor actor matches nothing;
///   * a key that is not chain-qualified with a KNOWN chain matches nothing, so an
///     unqualified `D` can no longer produce an `Exact` relation.
///
/// Wallet-kind normalization is used because deployer/authority/fee-payer/funder
/// are all wallet addresses under the frozen rule `wallet = chain_id + address`.
/// Whether two wallet keys denote the same wallet (REV-037-F07).
///
/// The edge-to-edge counterpart of [`same_actor`]: both sides are canonicalized
/// under the frozen rule `wallet = chain_id + address`, and a key that is not
/// chain-qualified with a KNOWN chain matches nothing (fail-closed).
///
/// This exists as its own function because `same_actor` takes an `Option` anchor
/// actor, and using it for an edge-to-edge join would have meant wrapping a value
/// that is never optional. Three separate reviews found a raw comparison on a
/// different branch, so every wallet-key comparison now routes through one of these
/// two helpers and nothing compares wallet keys with `==`.
fn same_wallet(a: &str, b: &str) -> bool {
    let Some(a) = normalize_identity(IdentityKind::Wallet, a) else {
        return false;
    };
    let Some(b) = normalize_identity(IdentityKind::Wallet, b) else {
        return false;
    };
    a.value == b.value
}

/// Whether two token keys denote the same contract (REV-039-F06).
///
/// The token counterpart of [`same_wallet`]. Six raw token joins survived REV-038
/// because I swept `wallet` comparisons and then wrote that no wallet key was
/// compared with `==` — literally true, and misleading, since the token joins were
/// untouched. The reviewer measured two distinct harms:
///
///   * alias spellings LOSE valid relations (`sol:BBB` node vs `solana:BBB` edge);
///   * worse, an anchor `solana:AAA` against a node `sol:AAA` produced a
///     SELF-RELATION — one contract with two accepted spellings was reported as a
///     different token sharing its own deployer.
///
/// Self-exclusion is the reason this must be canonical rather than merely
/// normalized on output: a comparison that fails to recognise identity cannot
/// exclude it.
fn same_token(a: &str, b: &str) -> bool {
    let Some(a) = normalize_identity(IdentityKind::Token, a) else {
        return false;
    };
    let Some(b) = normalize_identity(IdentityKind::Token, b) else {
        return false;
    };
    a.value == b.value
}

/// Whether two social-identity keys denote the same account (REV-039-F06).
///
/// Found by enumerating every `==` on an entity key rather than by working from the
/// reported list — the reviewer named five token joins and this was a sixth. It
/// compares `platform:account_id` keys, so `X:Acct1` and `x:acct1` are one account.
/// Fail-closed: a key that is not platform-qualified matches nothing, so a bare
/// handle cannot be mistaken for an account identity.
/// Canonical form of a social key: lowercase platform, verbatim account id.
///
/// `normalize_identity(IdentityKind::Social, ..)` validates the shape but returns
/// the value UNCHANGED — it does not fold the platform. Discovered while making the
/// tests below pass rather than by reading the code, which is why they exist. The
/// account id is deliberately NOT case-folded: platforms treat account ids as
/// opaque, and `is_immutable_social_identity` compares ids verbatim too, so folding
/// here would disagree with the authority check.
fn canonical_social_key(key: &str) -> Option<String> {
    // Shape validation stays with the frozen helper.
    normalize_identity(IdentityKind::Social, key)?;
    let (platform, id) = key.split_once(':')?;
    let platform = platform.trim().to_ascii_lowercase();
    let id = id.trim();
    if platform.is_empty() || id.is_empty() {
        return None;
    }
    Some(format!("{platform}:{id}"))
}

fn same_social_identity(a: &str, b: &str) -> bool {
    match (canonical_social_key(a), canonical_social_key(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn same_actor(anchor_actor: Option<&str>, edge_actor: &str) -> bool {
    let Some(anchor_actor) = anchor_actor else {
        return false;
    };
    let Some(a) = normalize_identity(IdentityKind::Wallet, anchor_actor) else {
        return false;
    };
    let Some(b) = normalize_identity(IdentityKind::Wallet, edge_actor) else {
        return false;
    };
    a.value == b.value
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
/// `social_bindings` carries the authoritative `social_identities` records used to
/// decide whether a reused identity is an immutable account ID or a mutable handle
/// (REV-031/REV-025-F02). An empty slice means "no authoritative binding known",
/// which keeps funding corroboration at `Estimated` — fail-closed.
///
/// REV-035-#4: production code should call
/// [`resolve_candidates_from_store`](super::recent_store::resolve_candidates_from_store),
/// which performs the social lookup INSIDE the operation instead of accepting
/// bindings from its caller. This function stays public because the resolver logic
/// is pure and worth testing directly, but since `SocialIdentityRecord` cannot be
/// minted outside the crate, an external caller can only ever pass an empty slice
/// here — which is the fail-closed answer.
pub fn resolve_candidates(
    anchor: &ActorExtraction,
    nodes: &[EntityNode],
    edges: &[EntityEdge],
    social_bindings: &[SocialIdentityRecord],
    now_secs: i64,
) -> Vec<CandidateRelation> {
    let mut out: Vec<CandidateRelation> = Vec::new();
    // Dedupe key is (canonical value, kind, relation): the kind belongs in the key
    // because a Token and a Wallet may share a value (cf. REV-027 on the SQL side).
    let mut seen: HashSet<(String, IdentityKind, RecentRelation)> = HashSet::new();

    // ONE emit boundary for every resolver path (REV-031/REV-025-F03).
    //
    // REV-030 canonicalized only the actor-reverse-lookup path and claimed the
    // resolver was universally canonical. It was not: CrossChainDeployment,
    // FundedByKnownDeployer, and the standalone social-reuse path still emitted raw
    // keys, so `eth:BBB` and `ethereum:BBB` produced two relations for one asset.
    // Patching each site invites the same omission again, so emission is funnelled
    // through this closure: it normalizes the identity, rejects anything
    // unqualified/unknown-chain (fail-closed), and dedupes on the CANONICAL key.
    // No path may construct a `CandidateRelation` directly.
    let mut emit = |kind: IdentityKind,
                    raw_value: &str,
                    relation: RecentRelation,
                    confidence: RecentConfidence,
                    evidence_refs: Vec<String>| {
        let Some(identity) = normalize_identity(kind, raw_value) else {
            return; // unqualified / unknown chain resolves nothing
        };
        let key = (identity.value.clone(), identity.kind, relation);
        if !seen.insert(key) {
            return;
        }
        out.push(CandidateRelation {
            to_identity: identity,
            relation,
            confidence,
            evidence_refs,
        });
    };

    let anchor_token = &anchor.token;
    // Fail-closed: an anchor with an unknown/unqualified chain resolves nothing.
    let Some(anchor_chain) = canonical_chain(anchor_token) else {
        return Vec::new();
    };

    // Cross-chain deployment: an authoritative edge from the anchor token to a
    // token on a DIFFERENT chain. Same-chain or non-authoritative edges never
    // produce a family merge.
    for e in edges {
        if e.edge_type != EdgeType::CrossChainDeployment {
            continue;
        }
        // REV-039-F06: canonical, so a cross-chain edge recorded as `sol:AAA`
        // still matches the anchor `solana:AAA`. Raw equality silently dropped the
        // whole cross-chain branch for alias-spelled sources.
        if !same_token(&e.from_entity_key, anchor_token) {
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
        // Reaching here already required `edge_is_authoritative`, so the edge is
        // Exact by construction.
        emit(
            IdentityKind::Token,
            &e.to_entity_key,
            RecentRelation::CrossChainDeployment,
            RecentConfidence::Exact,
            e.evidence_refs.clone(),
        );
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
        // REV-039-F06: SELF-EXCLUSION must be canonical. This raw comparison was
        // the most damaging of the six: an anchor `solana:AAA` against a node
        // `sol:AAA` is the SAME contract, but raw equality did not recognise it, so
        // the token was reported as another token sharing its own deployer
        // (`alias_self_relation=1`). A comparison that cannot recognise identity
        // cannot exclude it.
        if same_token(other, anchor_token) {
            continue;
        }
        // REV-025-F03: the TARGET key must be canonical too, not just the anchor.
        // A Confirmed, evidence-backed `DeployedBy` edge pointing at `bogus:BBB`
        // previously yielded `SameDeployer/Exact` because only the anchor chain
        // was validated. An unknown/unqualified target chain resolves nothing.
        //
        // REV-029: the emitted key is normalized as well, so an alias spelling
        // (`sol:` vs `solana:`) cannot split one asset into two relations.
        let Some(other_canonical) =
            normalize_identity(IdentityKind::Token, other).map(|k| k.value)
        else {
            continue;
        };
        // REV-039-F06: canonical node-to-edge join. A node `sol:BBB` and an edge
        // targeting `solana:BBB` are one contract; raw equality lost the relation.
        for e in edges.iter().filter(|e| same_token(&e.to_entity_key, other)) {
            let actor = &e.from_entity_key;
            // Factory/launchpad is never a project deployer.
            //
            // REV-035: this exclusion was still a raw string comparison, found
            // while sweeping for the class of bug rather than the one instance the
            // reviewer reported. It fails in the dangerous direction: a factory
            // recorded as `sol:FACTORY` against an edge actor `solana:FACTORY`
            // would NOT be excluded, so a launchpad address shared by thousands of
            // unrelated tokens would be treated as a common deployer and produce
            // `SameDeployer`/`Exact` between strangers. Canonical comparison makes
            // the exclusion hold across alias spellings.
            if same_actor(anchor.factory.as_deref(), actor) {
                continue;
            }
            let authoritative = edge_is_authoritative(e, now_secs);
            // REV-033/REV-034: actor identities are compared CANONICALLY, not as
            // raw strings. REV-032 canonicalized the emitted target but left the
            // comparison on both sides raw, so the reviewer measured:
            //   actor=sol:D    edge=solana:D  -> count=0  (valid relation LOST)
            //   actor=D        edge=D         -> Exact    (unqualified actor ACCEPTED)
            // `same_actor` fixes both directions: an alias pair matches, and a key
            // that is not chain-qualified matches nothing at all (fail-closed).
            let relation = match e.edge_type {
                EdgeType::DeployedBy if same_actor(anchor.deployer.as_deref(), actor) => {
                    Some((RecentRelation::SameDeployer, authoritative))
                }
                EdgeType::FundedBy if same_actor(anchor.initial_funder.as_deref(), actor) => {
                    Some((RecentRelation::SameFunder, authoritative))
                }
                _ => None,
            };
            // SameAuthority / SameFeePayer are resolved from explicit edges
            // carrying the matching actor (REV-022-F01: these were never emitted).
            let relation = relation.or_else(|| match e.edge_type {
                EdgeType::SameAuthority if same_actor(anchor.authority.as_deref(), actor) => {
                    Some((RecentRelation::SameAuthority, authoritative))
                }
                EdgeType::SameFeePayer if same_actor(anchor.fee_payer.as_deref(), actor) => {
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
                emit(
                    IdentityKind::Token,
                    &other_canonical,
                    relation,
                    confidence,
                    e.evidence_refs.clone(),
                );
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
        // REV-035-#6: this branch was the ONE actor comparison REV-034 missed.
        // I added `same_actor` to the direct deployer/authority/fee-payer/funder
        // branch and then wrote that actor comparison was canonical — it was not,
        // and the reviewer measured both failure directions here:
        //   anchor `sol:D1`  + edge `solana:D1` -> count=0  (valid relation LOST)
        //   anchor `D1`      + edge `D1`        -> Reconstructed (bare actor ACCEPTED)
        // Fixing the example instead of the class is exactly the pattern I keep
        // repeating, so the comparison is funnelled through the same helper.
        if !same_actor(anchor.deployer.as_deref(), &e.from_entity_key) {
            continue;
        }
        // REV-037-F07: the node lookup is a wallet-key join too, so it is
        // canonicalized for the same reason as the corroboration edge. A wallet node
        // recorded as `sol:W` would otherwise not match a funding edge targeting
        // `solana:W`, and the whole `FundedByKnownDeployer` branch would silently
        // skip a real funded wallet.
        let is_wallet = nodes.iter().any(|n| {
            same_wallet(&n.entity_key, &e.to_entity_key)
                && n.node_type == super::graph::NodeType::Wallet
        });
        if !is_wallet {
            continue;
        }
        let authoritative = edge_is_authoritative(e, now_secs);
        // A real reused-social/domain edge: the funded wallet reuses one of the
        // anchor's immutable social identities (REV-025-F02). Merely *having* a
        // social identity is not evidence.
        //
        // REV-027 re-review: the corroborating edge must ALSO be authoritative.
        // Previously any `ReusedSocialLink` edge counted, so a disputed, expired,
        // or evidence-free social edge could promote authoritative funding all the
        // way to `Reconstructed`. The corroboration is now held to exactly the
        // same bar as the funding edge itself (Confirmed + evidence + valid
        // window), and the reused identity must be a chain/platform-qualified
        // immutable ID — a bare handle is never ownership.
        // REV-029/REV-025-F02: authority, identity match, and time window must all
        // hold on the SAME edge. REV-028 split them into two independent scans, so
        // a second (weaker) `ReusedSocialLink` edge could supply the time window
        // for an edge that never carried it — corroboration assembled from parts.
        // `corroborating_social_edge` returns the ONE edge satisfying every
        // condition at once, or `None`.
        let funding_at = parse_secs(&e.occurred_at);
        let corroborating_social_edge = funding_at.and_then(|occ| {
            edges.iter().find(|re| {
                re.edge_type == EdgeType::ReusedSocialLink
                    // REV-037-F07: the funded wallet and the wallet that reused the
                    // identity must be compared CANONICALLY. This was the third
                    // actor comparison left raw: REV-034 fixed the direct-actor
                    // branch, REV-036 fixed the funding source and factory, and I
                    // claimed to have swept the class both times. The reviewer
                    // measured what remained:
                    //   funding target solana:W, reuse source solana:W -> Reconstructed
                    //   funding target solana:W, reuse source sol:W    -> Estimated
                    // A valid relation lost its corroboration purely because of
                    // alias spelling. `same_wallet` is the same canonicalization
                    // used everywhere else, so this join cannot drift again.
                    && same_wallet(&re.from_entity_key, &e.to_entity_key)
                    // same bar as the funding edge: Confirmed + evidence + active window
                    && edge_is_authoritative(re, now_secs)
                    // the reused identity is one of the anchor's, and it is an
                    // immutable platform-qualified account ID — never a handle
                    // REV-039-F06: canonical social-identity join. Not in the
                    // reviewer's list of five — found by enumerating every `==` on
                    // an entity key instead of working from the reported set. A
                    // reuse edge recorded as `X:Acct1` against an anchor identity
                    // `x:acct1` is the same account, and raw equality dropped the
                    // corroboration.
                    && anchor.social_identities.iter().any(|s| {
                        same_social_identity(&re.to_entity_key, s)
                            && is_immutable_social_identity(s, social_bindings)
                    })
                    // and THIS edge's own validity window contains the funding:
                    // an identity reused only AFTER the funding is not evidence for it
                    && parse_secs(&re.valid_from).map(|vf| occ >= vf).unwrap_or(false)
                    && re
                        .valid_until
                        .as_deref()
                        .and_then(parse_secs)
                        .map(|vu| occ <= vu)
                        .unwrap_or(true)
            })
        });
        let confidence = if authoritative && corroborating_social_edge.is_some() {
            RecentConfidence::Reconstructed
        } else if authoritative {
            RecentConfidence::Estimated
        } else {
            RecentConfidence::Insufficient
        };
        emit(
            IdentityKind::Wallet,
            &e.to_entity_key,
            RecentRelation::FundedByKnownDeployer,
            confidence,
            e.evidence_refs.clone(),
        );
    }

    // Reused social link without immutable-ownership evidence → candidate only.
    // The relation resolves to the OTHER token/project using the social
    // identity, never the social identity itself (REV-022-F01).
    for identity in &anchor.social_identities {
        for e in edges {
            if e.edge_type != EdgeType::ReusedSocialLink {
                continue;
            }
            // REV-039-F06: canonical on both sides of this path too — the social
            // identity lookup and the token node lookup.
            if same_social_identity(&e.to_entity_key, identity) {
                // `from` is the other project reusing the identity. Only a TOKEN
                // project produces a ReusedSocialLink candidate relation; a
                // wallet reusing the identity is used for `Reconstructed`
                // corroboration, not a standalone reuse relation.
                let is_token = nodes.iter().any(|n| {
                    same_token(&n.entity_key, &e.from_entity_key)
                        && n.node_type == super::graph::NodeType::Token
                });
                if !is_token {
                    continue;
                }
                // REV-032: this path also deduped on the SOCIAL identity key
                // (`e.to_entity_key`) rather than on the emitted TOKEN target, so
                // two different projects reusing one identity collapsed into a
                // single relation and one of them silently disappeared. The shared
                // `emit` boundary keys on the emitted target, which fixes both the
                // alias split and this collapse.
                emit(
                    IdentityKind::Token,
                    &e.from_entity_key,
                    RecentRelation::ReusedSocialLink,
                    RecentConfidence::Insufficient,
                    e.evidence_refs.clone(),
                );
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

    /// Authoritative social-identity records for the tests: `x:12345` and
    /// `x:acct1`/`x:acct_immutable_1` are real immutable account IDs, and `alice`
    /// is recorded as a HANDLE of `x:12345` so it can never pass as an account ID
    /// (REV-031/REV-025-F02).
    fn bindings() -> Vec<SocialIdentityRecord> {
        vec![
            SocialIdentityRecord {
                platform: "x".into(),
                immutable_user_id: "12345".into(),
                handles: vec!["alice".into(), "@alice".into()],
            },
            SocialIdentityRecord {
                platform: "x".into(),
                immutable_user_id: "acct1".into(),
                handles: vec![],
            },
            SocialIdentityRecord {
                platform: "x".into(),
                immutable_user_id: "acct_immutable_1".into(),
                handles: vec![],
            },
        ]
    }

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
            missing_inputs: vec![],
            retraction: None,
            is_current_coverage: false,
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
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("eth:BBB")];
        let edges: Vec<EntityEdge> = vec![];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert!(out.is_empty(), "symbol similarity alone must not relate contracts");
    }

    #[test]
    fn same_deployer_creates_exact_relation() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("sol:BBB")];
        let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "sol:BBB", &["ev1"])];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::SameDeployer);
        assert_eq!(out[0].confidence, RecentConfidence::Exact);
    }

    // REV-027 / REV-025-F03: the TARGET token key must be canonical too. A
    // Confirmed, evidence-backed DeployedBy edge to a bogus-chain target used to
    // yield SameDeployer/Exact because only the anchor chain was validated.
    #[test]
    fn bogus_target_chain_resolves_nothing() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        // Target `bogus:BBB` carries an unknown chain prefix.
        let nodes = vec![token_node("sol:AAA"), token_node("bogus:BBB")];
        let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "bogus:BBB", &["ev1"])];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert!(
            out.is_empty(),
            "an unknown target chain must not produce a relation, let alone Exact"
        );

        // An unqualified target (no chain prefix at all) is likewise rejected.
        let nodes2 = vec![token_node("sol:AAA"), token_node("BBB")];
        let edges2 = vec![edge(EdgeType::DeployedBy, "solana:D1", "BBB", &["ev1"])];
        assert!(resolve_candidates(&anchor, &nodes2, &edges2, &bindings(), NOW).is_empty());
    }

    // REV-029/REV-025-F03: an alias chain spelling must canonicalize, so the same
    // asset cannot appear as two identities.
    #[test]
    fn alias_chain_spellings_canonicalize() {
        let a = normalize_identity(IdentityKind::Token, "sol:AAA").unwrap();
        let b = normalize_identity(IdentityKind::Token, "solana:AAA").unwrap();
        assert_eq!(a.value, b.value, "sol: and solana: must produce one key");
        assert_eq!(a.value, "solana:AAA");

        let rh = normalize_identity(IdentityKind::Wallet, "rh:W1").unwrap();
        assert_eq!(rh.value, "robinhood:W1");
        let eth = normalize_identity(IdentityKind::Token, "eth:T1").unwrap();
        assert_eq!(eth.value, "ethereum:T1");
    }

    // REV-029/REV-025-F03: the resolver must EMIT canonical target keys, and two
    // alias spellings of one target must collapse to a single relation.
    #[test]
    fn resolver_emits_canonical_target_and_dedupes_aliases() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        // The SAME target asset spelled two ways.
        let nodes = vec![
            token_node("sol:AAA"),
            token_node("sol:BBB"),
            token_node("solana:BBB"),
        ];
        let edges = vec![
            edge(EdgeType::DeployedBy, "solana:D1", "sol:BBB", &["ev1"]),
            edge(EdgeType::DeployedBy, "solana:D1", "solana:BBB", &["ev2"]),
        ];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert_eq!(out.len(), 1, "alias spellings must collapse into one relation");
        assert_eq!(out[0].to_identity.value, "solana:BBB", "target key must be canonical");
    }

    // REV-029/REV-025-F02: a mutable handle is not immutable ownership, so it
    // cannot corroborate funding up to `Reconstructed`.
    // REV-031/REV-025-F02: immutability comes from an authoritative record, NOT
    // from the string's shape. REV-030 accepted `x:alice` because it contained no
    // `@`, slash or whitespace — a plain handle passing as an account ID.
    #[test]
    fn handle_is_not_immutable_social_identity() {
        let b = bindings();
        // Backed by a record -> immutable.
        assert!(is_immutable_social_identity("x:12345", &b));
        assert!(is_immutable_social_identity("x:acct1", &b));

        // THE REV-031 BYPASS: opaque-looking, but recorded as a handle of x:12345.
        assert!(
            !is_immutable_social_identity("x:alice", &b),
            "a handle must never pass as an immutable account ID"
        );

        // No authoritative record at all -> fail-closed, whatever the shape.
        assert!(!is_immutable_social_identity("x:99999", &b));
        assert!(!is_immutable_social_identity("telegram:100200300", &b));
        // Malformed keys stay rejected.
        assert!(!is_immutable_social_identity("x:", &b));
        assert!(!is_immutable_social_identity("12345", &b));
        // An empty binding set never asserts immutability.
        assert!(!is_immutable_social_identity("x:12345", &[]));
    }

    // REV-029/REV-025-F02: authority, identity match and time window must hold on
    // ONE edge. REV-028 scanned for them separately, so a second weaker edge could
    // supply the time window for an edge that never carried it.
    #[test]
    fn time_window_cannot_come_from_a_second_edge() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("sol:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec!["x:12345".into()],
        };
        let nodes = vec![token_node("sol:AAA"), wallet_node("sol:W1")];
        let funding = edge(EdgeType::FundedBy, "sol:D1", "sol:W1", &["ev-fund"]);

        // Edge A: authoritative + identity matches, but its window ENDS before the
        // funding occurred (2026-01-01), so it cannot corroborate.
        let mut expired = edge(EdgeType::ReusedSocialLink, "sol:W1", "x:12345", &["ev-a"]);
        expired.valid_from = "2025-01-01T00:00:00Z".into();
        expired.valid_until = Some("2025-06-01T00:00:00Z".into());

        // Edge B: window DOES contain the funding, but it is disputed and carries
        // the identity only incidentally — it must not lend its window to edge A.
        let mut weak = edge(EdgeType::ReusedSocialLink, "sol:W1", "x:12345", &[]);
        weak.truth_status = TruthStatus::Disputed;

        let out = resolve_candidates(&anchor, &nodes, &[funding, expired, weak], &bindings(), NOW);
        let funded = out
            .iter()
            .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
            .expect("funding relation present");
        assert_eq!(
            funded.confidence,
            RecentConfidence::Estimated,
            "a window borrowed from a second edge must not reach Reconstructed"
        );
    }

    // REV-027 / REV-025-F02: the corroborating ReusedSocialLink edge must itself
    // be authoritative. A disputed or evidence-free social edge must not promote
    // authoritative funding to `Reconstructed`.
    #[test]
    fn non_authoritative_social_edge_does_not_reach_reconstructed() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("sol:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec!["x:12345".into()],
        };
        let nodes = vec![token_node("sol:AAA"), wallet_node("sol:W1")];

        let funding = edge(EdgeType::FundedBy, "sol:D1", "sol:W1", &["ev-fund"]);

        // Case 1: social edge is DISPUTED -> not authoritative.
        let mut disputed = edge(EdgeType::ReusedSocialLink, "sol:W1", "x:12345", &["ev-soc"]);
        disputed.truth_status = TruthStatus::Disputed;
        let out = resolve_candidates(&anchor, &nodes, &[funding.clone(), disputed], &bindings(), NOW);
        let funded = out
            .iter()
            .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
            .expect("funding relation present");
        assert_eq!(
            funded.confidence,
            RecentConfidence::Estimated,
            "disputed social corroboration must stay Estimated, not Reconstructed"
        );

        // Case 2: social edge has NO evidence -> not authoritative.
        let no_evidence = edge(EdgeType::ReusedSocialLink, "sol:W1", "x:12345", &[]);
        let out2 = resolve_candidates(&anchor, &nodes, &[funding.clone(), no_evidence], &bindings(), NOW);
        let funded2 = out2
            .iter()
            .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
            .expect("funding relation present");
        assert_eq!(funded2.confidence, RecentConfidence::Estimated);

        // Case 3: fully authoritative social edge inside its window -> Reconstructed.
        let good = edge(EdgeType::ReusedSocialLink, "sol:W1", "x:12345", &["ev-soc"]);
        let out3 = resolve_candidates(&anchor, &nodes, &[funding, good], &bindings(), NOW);
        let funded3 = out3
            .iter()
            .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
            .expect("funding relation present");
        assert_eq!(funded3.confidence, RecentConfidence::Reconstructed);
    }

    #[test]
    fn same_authority_and_fee_payer_resolve() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: None,
            authority: Some("solana:A1".into()),
            fee_payer: Some("solana:F1".into()),
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("sol:BBB")];
        let edges = vec![
            edge(EdgeType::SameAuthority, "solana:A1", "sol:BBB", &["ev-a"]),
            edge(EdgeType::SameFeePayer, "solana:F1", "sol:BBB", &["ev-f"]),
        ];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
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
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert!(out.is_empty(), "disputed cross-chain edge must not produce Exact");
    }

    // Wallet keys are chain-qualified (`solana:W9`), per the frozen identity rule
    // `wallet = chain_id + wallet_address`. These fixtures previously used
    // `wallet:W9`, which is NOT chain-qualified: the shared emit boundary now
    // rejects it, which is how REV-032-F01 surfaced.
    #[test]
    fn funded_by_known_deployer_is_reconstructed() {
        // Funding + reused social identity + coherent time -> Reconstructed.
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec!["x:acct1".into()],
        };
        let nodes = vec![token_node("sol:AAA"), wallet_node("solana:W9")];
        // Funding edge + a real reused-social edge (wallet reuses the anchor's
        // social identity) -> Reconstructed.
        let edges = vec![
            edge(EdgeType::FundedBy, "solana:D1", "solana:W9", &["ev2"]),
            edge(EdgeType::ReusedSocialLink, "solana:W9", "x:acct1", &["ev-reuse"]),
        ];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, RecentRelation::FundedByKnownDeployer);
        assert_eq!(out[0].confidence, RecentConfidence::Reconstructed);
        assert_eq!(out[0].to_identity.value, "solana:W9");
    }

    #[test]
    fn funding_alone_is_not_reconstructed() {
        // No social corroboration -> funding alone is Estimated, not Reconstructed.
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), wallet_node("solana:W9")];
        let edges = vec![edge(EdgeType::FundedBy, "solana:D1", "solana:W9", &["ev2"])];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, RecentConfidence::Estimated);
    }

    // REV-032-F01: a non-chain-qualified wallet key must NOT be emitted at all.
    // Before the shared boundary, the FundedByKnownDeployer path emitted
    // `e.to_entity_key` verbatim, so `wallet:W9` (no chain) became a relation
    // target and violated the frozen identity rule.
    #[test]
    fn unqualified_wallet_target_is_not_emitted() {
        // The ANCHOR and the funding actor are canonical on purpose, so the only
        // thing under test is the TARGET key: `wallet:W9` carries a kind prefix,
        // not a chain, so it is not a valid identity and must resolve to nothing.
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: None,
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        // Deliberately NOT chain-qualified. Do not "fix" this key: the whole
        // point of the test is that an unqualified target is dropped.
        const UNQUALIFIED_TARGET: &str = "wallet:W9";
        let nodes = vec![token_node("sol:AAA"), wallet_node(UNQUALIFIED_TARGET)];
        let edges = vec![edge(
            EdgeType::FundedBy,
            "solana:D1",
            UNQUALIFIED_TARGET,
            &["ev2"],
        )];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert!(
            out.is_empty(),
            "`wallet:W9` is not chain-qualified and must not be emitted as a target, got {out:?}"
        );
    }

    #[test]
    fn factory_is_not_deployer() {
        let anchor = ActorExtraction {
            token: "sol:AAA".into(),
            deployer: Some("solana:D1".into()),
            authority: None,
            fee_payer: None,
            factory: Some("solana:F1_FACTORY".into()),
            initial_funder: None,
            authority_changes: vec![],
            social_identities: vec![],
        };
        let nodes = vec![token_node("sol:AAA"), token_node("sol:BBB")];
        let edges = vec![edge(EdgeType::DeployedBy, "solana:F1_FACTORY", "sol:BBB", &["ev3"])];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
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
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
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
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        // The target is the other TOKEN, emitted canonically (`sol:` -> `solana:`),
        // and never the social identity itself.
        assert!(out.iter().any(|c| c.to_identity.value == "solana:BBB"));
        assert!(!out.iter().any(|c| c.to_identity.value == "x:acct1"));
    }

    // REV-032-F02: this path deduped on the SOCIAL identity, so two different
    // projects reusing one identity collapsed and one silently disappeared.
    #[test]
    fn two_projects_reusing_one_identity_both_surface() {
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
        let nodes = vec![
            token_node("sol:AAA"),
            token_node("sol:BBB"),
            token_node("sol:CCC"),
        ];
        let edges = vec![
            edge(EdgeType::ReusedSocialLink, "sol:BBB", "x:acct1", &["ev-b"]),
            edge(EdgeType::ReusedSocialLink, "sol:CCC", "x:acct1", &["ev-c"]),
        ];
        let out = resolve_candidates(&anchor, &nodes, &edges, &bindings(), NOW);
        assert!(out.iter().any(|c| c.to_identity.value == "solana:BBB"));
        assert!(
            out.iter().any(|c| c.to_identity.value == "solana:CCC"),
            "the second project reusing the same identity must not be swallowed"
        );
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
            deployer: Some("solana:D1".into()),
            authority: Some("solana:A1".into()),
            fee_payer: Some("solana:F1".into()),
            factory: Some("solana:FACTORY".into()),
            initial_funder: Some("wallet:FUND1".into()),
            authority_changes: vec!["wallet:OLD1".into()],
            social_identities: vec!["x:acct1".into()],
        };
        let q = build_lookup_query(&actor);
        assert!(q.contains(&"sol:AAA".to_string()));
        assert!(q.contains(&"solana:D1".to_string()));
        assert!(q.contains(&"solana:FACTORY".to_string()));
        assert!(q.contains(&"wallet:OLD1".to_string()));
        assert!(q.contains(&"x:acct1".to_string()));
        // deterministic dedup: no duplicate entries
        let mut dedup = q.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(q.len(), dedup.len());
    }
}
