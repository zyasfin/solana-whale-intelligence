//! In-crate authority-boundary tests (REV-037-F06).
//!
//! These tests were `tests/recent_intelligence_regressions.rs`, an INTEGRATION
//! test. Integration tests are external crates, so they could not reach
//! `#[cfg(test)]` internals â€” which is why REV-036 exposed a `test_fixtures`
//! Cargo feature to let them mint social-identity store rows. The reviewer then
//! enabled that feature from a downstream crate and minted authority records
//! (`feature_forged_records=1 reconstructed=true`), and the default `cargo test`
//! stopped compiling without it.
//!
//! Tests that need to mint authority belong INSIDE the crate. `#[cfg(test)]`
//! cannot be switched on by a dependent, and the default commands keep working.
//! The purely external-observer assertions stayed behind in
//! `tests/authority_boundary.rs`, where they prove what an outside caller can and
//! cannot do.

#![cfg(test)]
// REV-020 acceptance scenarios + cross-chain collision fixtures.
//
// Each test maps one REV-020 "Minimum acceptance" scenario (1–10) to an
// observable assertion through the `sf` library public API. Updated for the
// REV-023 typed/fail-closed contract: `resolve_candidates` takes `now_secs`,
// `should_trigger_lookup` takes a typed lifecycle + trigger, `classify_social`
// verifies announced CA + author binding, `apply_retraction` resolves against
// a target event ID, and `RecentEvent` carries typed `DateTime<Utc>` timestamps.

use chrono::DateTime;
use super::core::TruthStatus;
use super::graph::{EdgeType, EntityEdge, EntityNode, NodeType};
use super::recent::{
    ActorExtraction, CapabilityStatus, Coverage, OfficialSocialBinding, RecentConfidence,
    RecentEvent, RecentRelation, Retraction, SocialEvidenceKind, SocialEvidenceObservation,
    StoredSocialIdentity,
};
use super::recent_runtime::{
    apply_retraction, assign_dependency_group, build_timeline, classify_social, coverage_status,
    records_from_store, resolve_candidates, should_trigger_lookup, SocialIdentityRecord,
};
use super::token::TokenLifecycle;

const NOW: i64 = 1_800_000_000; // 2027-01-15, after all test edges.

/// Authoritative `social_identities` records for these scenarios.
///
/// REV-034: this file is an EXTERNAL consumer, so it cannot construct
/// `SocialIdentityRecord` — the fields are private and there is no public
/// constructor. That is the point of the fix: the reviewer previously forged a
/// record here and obtained `Reconstructed`. Records can only come from store rows
/// via `records_from_store`, which is the same path production uses after
/// `recent_store::fetch_social_identities`.
///
/// `x:acct_immutable_1` and `x:acct1` are real account IDs; `alice` is recorded as
/// a HANDLE of the first, so it can never corroborate ownership.
fn bindings() -> Vec<SocialIdentityRecord> {
    records_from_store(vec![
        StoredSocialIdentity::mint_for_tests("x", "acct_immutable_1", &["alice"]),
        StoredSocialIdentity::mint_for_tests("x", "acct1", &[]),
    ])
}

fn dt(s: &str) -> DateTime<chrono::Utc> {
    s.parse().unwrap()
}

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
        deployer: Some("solana:D1".into()),
        authority: Some("solana:A1".into()),
        fee_payer: Some("solana:F1".into()),
        factory: Some("solana:FACTORY".into()),
        initial_funder: Some("wallet:FUND1".into()),
        authority_changes: vec![],
        social_identities: vec!["x:acct_immutable_1".into()],
    }
}
fn event(id: &str, anchor: &str, occurred: &str, observed: &str, coverage: Coverage, refs: &[&str]) -> RecentEvent {
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
        capability_status: CapabilityStatus::Available,
        missing_inputs: vec![],
        retraction: None,
        is_current_coverage: false,
    }
}

// 1. Same-symbol cross-chain contracts without corroboration remain unrelated.
#[test]
fn accept1_same_symbol_cross_chain_stays_unrelated() {
    let nodes = vec![token("sol:AAA"), token("eth:AAA")];
    let edges: Vec<EntityEdge> = vec![];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    assert!(out.is_empty());
}

// 2. Same chain-qualified deployer/authority creates exact relations;
//    launchpad/factory stays separate.
#[test]
fn accept2_same_deployer_exact_factory_separate() {
    let nodes = vec![token("sol:AAA"), token("sol:BBB"), token("sol:CCC")];
    let edges = vec![
        edge(EdgeType::DeployedBy, "solana:D1", "sol:BBB", &["ev-deploy"]),
        // Factory must NOT produce a SameDeployer relation.
        edge(EdgeType::DeployedBy, "solana:FACTORY", "sol:CCC", &["ev-factory"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].relation, RecentRelation::SameDeployer);
    assert_eq!(out[0].confidence, RecentConfidence::Exact);
    // REV-030/REV-025-F03: the emitted target key is CANONICAL, so the `sol:`
    // alias resolves to `solana:`. The acceptance criterion ("same chain-qualified
    // deployer creates an exact relation to that contract") is unchanged; only the
    // canonical spelling of the key is asserted now.
    assert_eq!(out[0].to_identity.value, "solana:BBB");
}

// 3. New wallet funded by known deployer + immutable X ID → Reconstructed.
//
// REV-032: wallet keys are chain-qualified per the frozen rule
// `wallet = chain_id + wallet_address`. `wallet:NEW1` carried no chain, and the
// shared emit boundary now rejects it, so the fixture uses `solana:NEW1`. The
// acceptance criterion is unchanged.
#[test]
fn accept3_funded_by_known_deployer_is_reconstructed() {
    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"]),
        // Reused-social edge: the new wallet reuses the anchor's immutable X id.
        edge(EdgeType::ReusedSocialLink, "solana:NEW1", "x:acct_immutable_1", &["ev-reuse"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].relation, RecentRelation::FundedByKnownDeployer);
    assert_eq!(out[0].confidence, RecentConfidence::Reconstructed);
    assert_eq!(out[0].to_identity.value, "solana:NEW1");
}

// 3b (REV-025-F02): funding + a social identity but NO reused-social edge is
// NOT Reconstructed.
#[test]
fn accept3b_funding_alone_is_not_reconstructed() {
    let a = anchor(); // has social_identities, but no reuse edge
    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert_eq!(out[0].confidence, RecentConfidence::Estimated);
}

// 4. Reused handle/domain without immutable ownership → Insufficient candidate.
#[test]
fn accept4_reused_link_without_ownership_is_insufficient() {
    let nodes = vec![token("sol:AAA"), token("sol:BBB")];
    let edges = vec![edge(EdgeType::ReusedSocialLink, "sol:BBB", "x:acct_immutable_1", &["ev-reuse"])];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    // Reuse resolves to the OTHER token, emitted canonically, classified Insufficient.
    assert!(out.iter().any(|c| c.to_identity.value == "solana:BBB"));
    assert!(out.iter().all(|c| c.confidence == RecentConfidence::Insufficient));
}

// 4b (REV-032-F03): a reused HANDLE — opaque-looking but recorded as a handle,
// not an account ID — must not corroborate funding up to Reconstructed.
#[test]
fn accept4b_reused_handle_does_not_corroborate_ownership() {
    let mut a = anchor();
    a.deployer = Some("solana:D1".into());
    a.social_identities = vec!["x:alice".into()]; // `alice` is a HANDLE of x:acct_immutable_1
    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"]),
        edge(EdgeType::ReusedSocialLink, "solana:NEW1", "x:alice", &["ev-reuse"]),
    ];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .expect("funding relation present");
    assert_eq!(
        funded.confidence,
        RecentConfidence::Estimated,
        "a mutable handle is not immutable ownership evidence"
    );
}

// 5. First-party exact-CA announcement → evidence-backed OFFICIAL_CA_ANNOUNCEMENT,
//    verified against announced CA + author binding.
#[test]
fn accept5_exact_ca_announcement_is_official() {
    let binding = OfficialSocialBinding {
        immutable_account_id: "x:acct_immutable_1".into(),
        chain_qualified_contract: "sol:AAA".into(),
        valid_from: dt("2025-01-01T00:00:00Z"),
        valid_until: None,
    };
    let official = SocialEvidenceObservation {
        platform: super::browser::BrowserPlatform::X,
        post_or_profile_id: "p".into(),
        immutable_account_id: "x:acct_immutable_1".into(),
        text_media_hash: "h".into(),
        published_at: dt("2026-01-01T00:00:00Z"),
        observed_at: dt("2026-01-01T01:00:00Z"),
        announced_contract: Some("sol:AAA".into()),
        relation_kind: SocialEvidenceKind::OfficialCaAnnouncement,
        raw_ref: "r".into(),
        parser_version: "1".into(),
        coverage: Coverage::Full,
        session_health: super::browser::SessionHealth::Ok,
    };
    assert_eq!(
        classify_social(&official, "sol:AAA", Some(&binding)),
        RecentRelation::OfficialCaAnnouncement
    );
    // Wrong CA -> downgraded (mention/candidate), never official.
    let wrong_ca = SocialEvidenceObservation {
        announced_contract: Some("sol:OTHER".into()),
        ..official.clone()
    };
    assert_eq!(
        classify_social(&wrong_ca, "sol:AAA", Some(&binding)),
        RecentRelation::SameSocialAccount
    );
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
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].relation, RecentRelation::CrossChainDeployment);
    // REV-032/REV-025-F03: the cross-chain path now emits through the shared
    // canonical boundary, so `eth:` resolves to `ethereum:`. This is exactly the
    // path the reviewer showed producing two relations for one asset.
    assert_eq!(out[0].to_identity.value, "ethereum:BBB");
}

// 6b (REV-032/REV-025-F03): alias spellings on the cross-chain and social-reuse
// paths must collapse, not produce one relation per spelling.
#[test]
fn accept6b_cross_chain_alias_spellings_collapse() {
    let nodes = vec![token("sol:AAA"), token("eth:BBB"), token("ethereum:BBB")];
    let edges = vec![
        edge(EdgeType::CrossChainDeployment, "sol:AAA", "eth:BBB", &["ev-a"]),
        edge(EdgeType::CrossChainDeployment, "sol:AAA", "ethereum:BBB", &["ev-b"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    assert_eq!(
        out.len(),
        1,
        "`eth:BBB` and `ethereum:BBB` are one asset and must yield one relation"
    );
    assert_eq!(out[0].to_identity.value, "ethereum:BBB");
}

// 7. Correlated copies collapse into one dependency group while preserving
//    all evidence refs (transitive).
#[test]
fn accept7_copies_collapse_preserve_evidence() {
    let mut events = vec![
        event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["upstream_ev"]),
        event("e2", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["downstream_ev"]),
        event("e3", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["downstream2_ev"]),
    ];
    assign_dependency_group(
        &mut events,
        &[
            ("upstream_ev".into(), "downstream_ev".into()),
            ("downstream_ev".into(), "downstream2_ev".into()),
        ],
    );
    assert_eq!(events[1].dependency_group.as_deref(), Some("group:upstream_ev"));
    assert_eq!(events[2].dependency_group.as_deref(), Some("group:upstream_ev"));
    assert_eq!(events[1].evidence_refs, vec!["downstream_ev".to_string()]);
}

// 8. Deleted/retracted evidence remains archived and supersedes projection.
#[test]
fn accept8_retraction_archives_not_deletes() {
    let e = event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]);
    let retractions = vec![Retraction {
        target_event_id: "e1".into(),
        retracted_at: dt("2026-01-02T00:00:00Z"),
        truth_status: TruthStatus::Superseded,
    }];
    let out = apply_retraction(&[e], &retractions);
    assert_eq!(out.len(), 1, "archived, not deleted");
    assert_eq!(out[0].truth_status, TruthStatus::Superseded);
}

// 9. Timeline sorts by occurred_at and exposes observed_at/relation/evidence/
//    confidence/freshness/coverage.
#[test]
fn accept9_timeline_sorts_and_exposes_texture() {
    let mut a = event("a", "sol:AAA", "2026-01-03T00:00:00Z", "2026-01-03T01:00:00Z", Coverage::Full, &["r3"]);
    a.relation = Some(RecentRelation::SameDeployer);
    let b = event("b", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Full, &["r1"]);
    let c = event("c", "sol:AAA", "2026-01-02T00:00:00Z", "2026-01-02T01:00:00Z", Coverage::Degraded, &["r2"]);
    let tl = build_timeline("sol:AAA", &[a.clone(), b.clone(), c.clone()]);
    assert_eq!(tl.events[0].occurred_at, dt("2026-01-01T00:00:00Z"));
    assert_eq!(tl.events[2].occurred_at, dt("2026-01-03T00:00:00Z"));
    assert_eq!(tl.events[2].relation, Some(RecentRelation::SameDeployer));
    assert_eq!(tl.events[2].confidence_level, RecentConfidence::Estimated);
    assert_eq!(tl.events[2].coverage, Coverage::Full);
}

// 10. Dormant/tombstoned tokens receive no individual polling; typed gate.
#[test]
fn accept10_dormant_tokens_no_polling() {
    use super::recent::ActivationTrigger::*;
    assert!(!should_trigger_lookup(TokenLifecycle::Active, None));
    assert!(should_trigger_lookup(TokenLifecycle::Active, Some(FirstLiquidity)));
    assert!(!should_trigger_lookup(TokenLifecycle::Dormant, Some(FirstLiquidity)));
    assert!(should_trigger_lookup(TokenLifecycle::Dormant, Some(RevivalWake)));
    assert!(should_trigger_lookup(TokenLifecycle::Dormant, Some(OperatorRequest)));
    assert!(!should_trigger_lookup(TokenLifecycle::Archived, Some(VolumeActivation)));
    assert!(!should_trigger_lookup(TokenLifecycle::Tombstoned, Some(FirstLiquidity)));
}

// Cross-chain collision fixture: two independent deployers on different chains
// must not be conflated; the anchor resolves only its own chain's deployer.
#[test]
fn cross_chain_collision_deployer_not_conflated() {
    let nodes = vec![token("sol:AAA"), token("sol:BBB"), token("eth:CCC")];
    let edges = vec![
        edge(EdgeType::DeployedBy, "solana:D1", "sol:BBB", &["ev1"]),
        edge(EdgeType::DeployedBy, "solana:D_OTHER", "eth:CCC", &["ev2"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    // REV-030/REV-025-F03: keys are emitted canonically (`sol:` -> `solana:`,
    // `eth:` -> `ethereum:`). The criterion is unchanged: the anchor resolves only
    // its own chain's deployer, and the other chain's contract never appears.
    assert!(out
        .iter()
        .any(|c| c.to_identity.value == "solana:BBB" && c.relation == RecentRelation::SameDeployer));
    assert!(!out.iter().any(|c| c.to_identity.value.ends_with(":CCC")));
}

// Cross-chain collision fixture: coverage never coerces unavailable to zero.
#[test]
fn cross_chain_collision_coverage_not_zero() {
    let evs = vec![
        event("e1", "sol:AAA", "2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z", Coverage::Unavailable, &["r1"]),
        event("e2", "sol:AAA", "2026-01-02T00:00:00Z", "2026-01-02T01:00:00Z", Coverage::Full, &["r2"]),
    ];
    assert_eq!(coverage_status(&evs), Coverage::Degraded);
}

// ---------------------------------------------------------------------------
// REV-034: regressions for the two residual findings the reviewer measured.
// ---------------------------------------------------------------------------

// REV-033-F03 residual, direction 1: the ACTOR comparison must be canonical.
//
// REV-032 canonicalized the emitted TARGET but left the actor comparison a raw
// string equality on both sides. The reviewer measured `actor=sol:D` against
// `edge=solana:D` producing count=0 — a real relation LOST because two spellings
// of one wallet were treated as two wallets.
#[test]
fn rev034_actor_alias_spellings_still_match() {
    let mut a = anchor();
    a.deployer = Some("sol:D1".into()); // alias spelling on the anchor side
    let nodes = vec![token("sol:AAA"), token("sol:BBB")];
    // canonical spelling on the edge side
    let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "sol:BBB", &["ev-alias"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert_eq!(
        out.len(),
        1,
        "`sol:D1` and `solana:D1` are one wallet; the relation must not be lost, got {out:?}"
    );
    assert_eq!(out[0].relation, RecentRelation::SameDeployer);
    assert_eq!(out[0].confidence, RecentConfidence::Exact);
    assert_eq!(out[0].to_identity.value, "solana:BBB");
}

// REV-033-F03 residual, direction 2: an actor key with NO chain must match
// nothing. Raw string equality accepted `actor=D` vs `edge=D` and handed out
// `Exact` — the strongest confidence in the system — for a key that violates the
// frozen rule `wallet = chain_id + address`. Fail-closed in both directions.
#[test]
fn rev034_unqualified_actor_yields_no_relation() {
    let mut a = anchor();
    a.deployer = Some("D1".into()); // no chain prefix at all
    let nodes = vec![token("sol:AAA"), token("sol:BBB")];
    let edges = vec![edge(EdgeType::DeployedBy, "D1", "sol:BBB", &["ev-bare"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(
        out.is_empty(),
        "an actor key without a chain is not an identity and must resolve nothing, got {out:?}"
    );
}

// Same guard for an actor whose prefix looks chain-shaped but is not in the
// frozen vocabulary: an unknown chain is not an identity either.
#[test]
fn rev034_unknown_chain_actor_yields_no_relation() {
    let mut a = anchor();
    a.deployer = Some("bogus:D1".into());
    let nodes = vec![token("sol:AAA"), token("sol:BBB")];
    let edges = vec![edge(EdgeType::DeployedBy, "bogus:D1", "sol:BBB", &["ev-bogus"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(
        out.is_empty(),
        "an unknown chain prefix must resolve nothing, got {out:?}"
    );
}

// REV-032-F02 residual: `records_from_store` is the ONLY way to obtain a
// `SocialIdentityRecord`, so it is the single place where a store row can be
// rejected. It must drop rows that cannot establish ownership rather than pass
// them through as authoritative bindings.
#[test]
fn rev034_records_from_store_drops_unusable_rows() {
    let out = records_from_store(vec![
        // empty platform: no platform means no account namespace
        StoredSocialIdentity::mint_for_tests("   ", "acct_ok", &[]),
        // empty immutable id: nothing to own with
        StoredSocialIdentity::mint_for_tests("x", "", &["alice"]),
        // self-contradictory: the "immutable account ID" is also listed as one of
        // its own mutable handles, so the row cannot say which it is
        StoredSocialIdentity::mint_for_tests("x", "acct_conflict", &["ACCT_CONFLICT"]),
        // the only usable row
        StoredSocialIdentity::mint_for_tests("X", "acct_good", &["bob"]),
    ]);
    assert_eq!(out.len(), 1, "three unusable rows must be dropped, got {out:?}");
    assert_eq!(out[0].platform(), "x", "platform is normalized to lowercase");
    assert_eq!(out[0].immutable_user_id(), "acct_good");
}

// And a dropped row must LOWER confidence, never fabricate one: with no usable
// binding, funding + a reused identity stays `Estimated` instead of climbing to
// `Reconstructed`. This is the fail-closed half of the boundary.
#[test]
fn rev034_dropped_binding_row_cannot_promote_confidence() {
    // Contradictory row -> dropped -> no authoritative binding known.
    let empty_bindings = records_from_store(vec![StoredSocialIdentity::mint_for_tests(
        "x",
        "acct_immutable_1",
        &["acct_immutable_1"],
    )]);
    assert!(empty_bindings.is_empty(), "the fixture row must be dropped");

    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"]),
        edge(EdgeType::ReusedSocialLink, "solana:NEW1", "x:acct_immutable_1", &["ev-reuse"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &empty_bindings, NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .expect("funding relation present");
    assert_eq!(
        funded.confidence,
        RecentConfidence::Estimated,
        "with no authoritative binding, funding corroboration must not reach Reconstructed"
    );
}

// ---------------------------------------------------------------------------
// REV-037-F07: the reused-social corroboration edge and the wallet node lookup
// are wallet-key joins, and were still raw. These need minted authority, so they
// live in-crate.
// ---------------------------------------------------------------------------

// The reviewer measured a valid `Reconstructed` decaying to `Estimated` purely
// because the reuse edge spelled the same wallet differently:
//     funding target solana:W + reuse source solana:W -> Reconstructed
//     funding target solana:W + reuse source sol:W    -> Estimated
#[test]
fn rev037_reuse_edge_alias_spelling_keeps_corroboration() {
    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"]),
        // Same wallet as the funding target, alias spelling.
        edge(EdgeType::ReusedSocialLink, "sol:NEW1", "x:acct_immutable_1", &["ev-reuse"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .expect("funding relation present");
    assert_eq!(
        funded.confidence,
        RecentConfidence::Reconstructed,
        "`sol:NEW1` and `solana:NEW1` are one wallet; corroboration must not be lost \
         to alias spelling"
    );
}

// Fail-closed direction: a reuse edge whose source is NOT chain-qualified is not an
// identity, so it must not corroborate anything.
#[test]
fn rev037_reuse_edge_bare_wallet_does_not_corroborate() {
    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"]),
        edge(EdgeType::ReusedSocialLink, "NEW1", "x:acct_immutable_1", &["ev-reuse"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .expect("funding relation present");
    assert_eq!(
        funded.confidence,
        RecentConfidence::Estimated,
        "a bare wallet key is not an identity and must not corroborate ownership"
    );
}

// A reuse edge from a DIFFERENT wallet must not corroborate, even though both are
// canonical. Canonicalizing the join must not make it looser.
#[test]
fn rev037_reuse_edge_from_other_wallet_does_not_corroborate() {
    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"]),
        edge(EdgeType::ReusedSocialLink, "solana:OTHER", "x:acct_immutable_1", &["ev-reuse"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .expect("funding relation present");
    assert_eq!(
        funded.confidence,
        RecentConfidence::Estimated,
        "another wallet reusing the identity is not evidence about THIS wallet"
    );
}

// The wallet NODE lookup is a wallet-key join as well: a node spelled `sol:NEW1`
// must still be recognised as the wallet a `solana:NEW1` funding edge targets,
// otherwise the branch skips a real funded wallet entirely.
#[test]
fn rev037_wallet_node_alias_spelling_is_still_found() {
    let nodes = vec![token("sol:AAA"), wallet("sol:NEW1")]; // alias-spelled node
    let edges = vec![edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"])];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    assert!(
        out.iter().any(|c| c.relation == RecentRelation::FundedByKnownDeployer),
        "an alias-spelled wallet node must still match the funding target, got {out:?}"
    );
}

// ---------------------------------------------------------------------------
// REV-039-F06: every TOKEN and SOCIAL join canonicalized, not just wallet joins.
//
// Enumerating every `==` on an entity key found SIX raw joins; the reviewer
// reported five. The sixth (the social-identity join inside the funding
// corroboration) is covered by `rev039_reuse_identity_case_still_corroborates`.
// ---------------------------------------------------------------------------

// The most serious consequence the reviewer measured: one contract with two
// accepted spellings was reported as a DIFFERENT token sharing its own deployer.
// Self-exclusion must be canonical, because a comparison that cannot recognise
// identity cannot exclude it.
#[test]
fn rev039_alias_spelled_anchor_does_not_self_relate() {
    let mut a = anchor();
    a.token = "solana:AAA".into();
    // The same contract, spelled with the `sol:` alias, present as a node.
    let nodes = vec![token("sol:AAA")];
    let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "sol:AAA", &["ev-self"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(
        out.is_empty(),
        "`sol:AAA` and `solana:AAA` are ONE contract; it must not be reported as \
         another token sharing its own deployer, got {out:?}"
    );
}

// Alias on the NODE side must still match an edge recorded canonically.
#[test]
fn rev039_alias_token_node_still_matches_canonical_edge() {
    let mut a = anchor();
    a.token = "solana:AAA".into();
    let nodes = vec![token("solana:AAA"), token("sol:BBB")];
    let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "solana:BBB", &["ev-x"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert_eq!(
        out.len(),
        1,
        "node `sol:BBB` and edge `solana:BBB` are one contract, got {out:?}"
    );
    assert_eq!(out[0].relation, RecentRelation::SameDeployer);
    assert_eq!(out[0].to_identity.value, "solana:BBB");
}

// And alias on the EDGE side must match a canonical node.
#[test]
fn rev039_canonical_token_node_still_matches_alias_edge() {
    let mut a = anchor();
    a.token = "solana:AAA".into();
    let nodes = vec![token("solana:AAA"), token("solana:BBB")];
    let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "sol:BBB", &["ev-x"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert_eq!(out.len(), 1, "edge `sol:BBB` must match node `solana:BBB`, got {out:?}");
    assert_eq!(out[0].to_identity.value, "solana:BBB");
}

// The cross-chain branch reads the anchor from the edge SOURCE, which was also raw.
#[test]
fn rev039_cross_chain_alias_source_still_matches_anchor() {
    let mut a = anchor();
    a.token = "solana:AAA".into();
    let nodes = vec![token("solana:AAA"), token("ethereum:BBB")];
    // Source spelled with the alias; the anchor is canonical.
    let edges = vec![edge(
        EdgeType::CrossChainDeployment,
        "sol:AAA",
        "ethereum:BBB",
        &["ev-xc"],
    )];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(
        out.iter().any(|c| c.relation == RecentRelation::CrossChainDeployment),
        "an alias-spelled cross-chain source must still match the anchor, got {out:?}"
    );
}

// The standalone social-reuse path joins on BOTH the social identity and the token
// node. Case/alias differences on either side must not drop the relation.
#[test]
fn rev039_social_reuse_path_is_canonical_on_both_joins() {
    let mut a = anchor();
    a.token = "solana:AAA".into();
    a.social_identities = vec!["x:acct_immutable_1".into()];
    let nodes = vec![token("solana:AAA"), token("sol:BBB")]; // alias node
    let edges = vec![edge(
        EdgeType::ReusedSocialLink,
        "solana:BBB",              // canonical edge source
        "X:acct_immutable_1",      // different case on the identity
        &["ev-r"],
    )];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(
        out.iter().any(|c| c.to_identity.value == "solana:BBB"
            && c.relation == RecentRelation::ReusedSocialLink),
        "both the identity join and the token node join must be canonical, got {out:?}"
    );
}

// The sixth join, not in the reviewer's list: the social identity comparison inside
// the funding corroboration.
#[test]
fn rev039_reuse_identity_case_still_corroborates() {
    let nodes = vec![token("sol:AAA"), wallet("solana:NEW1")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:NEW1", &["ev-fund"]),
        // Same account, different case than the anchor's `x:acct_immutable_1`.
        edge(EdgeType::ReusedSocialLink, "solana:NEW1", "X:acct_immutable_1", &["ev-reuse"]),
    ];
    let out = resolve_candidates(&anchor(), &nodes, &edges, &bindings(), NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .expect("funding relation present");
    assert_eq!(
        funded.confidence,
        RecentConfidence::Reconstructed,
        "`X:acct_immutable_1` and `x:acct_immutable_1` are one account; corroboration \
         must not be lost to letter case"
    );
}

// Canonicalizing must not make any join LOOSER. Two genuinely different contracts
// stay different, and an unknown chain still resolves nothing.
#[test]
fn rev039_canonical_token_join_is_not_looser() {
    let mut a = anchor();
    a.token = "solana:AAA".into();
    let nodes = vec![token("solana:AAA"), token("solana:BBB")];
    // Edge targets a different contract entirely.
    let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "solana:CCC", &["ev-x"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(out.is_empty(), "different contracts must not match, got {out:?}");

    // Unknown chain on the node side resolves nothing.
    let nodes = vec![token("solana:AAA"), token("bogus:BBB")];
    let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "bogus:BBB", &["ev-x"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(out.is_empty(), "an unknown chain must resolve nothing, got {out:?}");

    // A token key with no chain at all resolves nothing.
    let nodes = vec![token("solana:AAA"), token("BBB")];
    let edges = vec![edge(EdgeType::DeployedBy, "solana:D1", "BBB", &["ev-x"])];
    let out = resolve_candidates(&a, &nodes, &edges, &bindings(), NOW);
    assert!(out.is_empty(), "a bare token key must resolve nothing, got {out:?}");
}
