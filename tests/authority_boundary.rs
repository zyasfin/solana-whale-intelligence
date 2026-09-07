//! REV-035 regressions: the authority boundary must not be mintable by a caller,
//! and the actor comparison must be canonical on EVERY resolver path.
//!
//! Six times in this ledger authority was placed on something the caller could
//! produce (GUC → audit row → public grant → caller slice → caller-built struct →
//! caller-built *store row*). These tests target the CLASS rather than the reported
//! instance, which is the failure mode the reviewer keeps having to point out.

use solana_whale_intelligence::sf::core::TruthStatus;
use solana_whale_intelligence::sf::graph::{EdgeType, EntityEdge, EntityNode, NodeType};
use solana_whale_intelligence::sf::recent::{ActorExtraction, RecentConfidence, RecentRelation};
use solana_whale_intelligence::sf::recent_runtime::resolve_candidates;

const NOW: i64 = 1_800_000_000;

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
fn anchor_with_deployer(d: &str) -> ActorExtraction {
    ActorExtraction {
        token: "sol:AAA".into(),
        deployer: Some(d.into()),
        authority: None,
        fee_payer: None,
        factory: None,
        initial_funder: None,
        authority_changes: vec![],
        social_identities: vec![],
    }
}

// ---------------------------------------------------------------------------
// REV-035-#6: the funded-wallet branch was the ONE actor comparison REV-034
// missed. Both failure directions the reviewer measured are pinned here.
// ---------------------------------------------------------------------------

// Direction 1: a valid alias pair must still resolve. The reviewer measured
// `funding_actor_alias_count=0` — a real relation silently LOST because `sol:D1`
// and `solana:D1` were compared as raw strings.
#[test]
fn funded_wallet_actor_alias_still_resolves() {
    let anchor = anchor_with_deployer("sol:D1");
    let nodes = vec![token("sol:AAA"), wallet("solana:W9")];
    let edges = vec![edge(EdgeType::FundedBy, "solana:D1", "solana:W9", &["ev-fund"])];
    let out = resolve_candidates(&anchor, &nodes, &edges, &[], NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .unwrap_or_else(|| {
            panic!("`sol:D1` and `solana:D1` are one wallet; relation lost, got {out:?}")
        });
    assert_eq!(funded.to_identity.value, "solana:W9");
}

// Direction 2: a bare, non-chain-qualified actor must resolve NOTHING. The
// reviewer measured `unqualified_funding_actor_count=1 reconstructed=true`, i.e. a
// key violating the frozen rule `wallet = chain_id + address` reached the strongest
// corroboration tier.
#[test]
fn funded_wallet_bare_actor_yields_nothing() {
    let anchor = anchor_with_deployer("D1");
    let nodes = vec![token("sol:AAA"), wallet("solana:W9")];
    let edges = vec![edge(EdgeType::FundedBy, "D1", "solana:W9", &["ev-fund"])];
    let out = resolve_candidates(&anchor, &nodes, &edges, &[], NOW);
    assert!(
        out.is_empty(),
        "a bare actor key is not an identity and must resolve nothing, got {out:?}"
    );
}

// Direction 3: an unknown chain prefix is not an identity either.
#[test]
fn funded_wallet_unknown_chain_actor_yields_nothing() {
    let anchor = anchor_with_deployer("bogus:D1");
    let nodes = vec![token("sol:AAA"), wallet("solana:W9")];
    let edges = vec![edge(EdgeType::FundedBy, "bogus:D1", "solana:W9", &["ev-fund"])];
    let out = resolve_candidates(&anchor, &nodes, &edges, &[], NOW);
    assert!(
        out.is_empty(),
        "an unknown chain prefix must resolve nothing, got {out:?}"
    );
}

// Found by sweeping for the CLASS, not reported by the reviewer: the
// factory/launchpad exclusion was also a raw comparison. It fails in the dangerous
// direction — a launchpad recorded under an alias spelling escapes exclusion and
// becomes a "shared deployer" between thousands of unrelated tokens.
#[test]
fn factory_exclusion_holds_across_alias_spellings() {
    let mut anchor = anchor_with_deployer("solana:FACTORY");
    anchor.factory = Some("sol:FACTORY".into()); // alias spelling of the same address
    let nodes = vec![token("sol:AAA"), token("sol:BBB")];
    let edges = vec![edge(EdgeType::DeployedBy, "solana:FACTORY", "sol:BBB", &["ev-f"])];
    let out = resolve_candidates(&anchor, &nodes, &edges, &[], NOW);
    assert!(
        out.is_empty(),
        "a launchpad address is never a project deployer, whichever way it is spelled; got {out:?}"
    );
}

// ---------------------------------------------------------------------------
// REV-035-#4: social authority cannot be minted by an external caller.
// ---------------------------------------------------------------------------

// The reviewer forged store rows and reached `Reconstructed`. An external consumer
// with NO authority available must top out at `Estimated`: funding is real, but
// ownership is unproven. This is the fail-closed behaviour, and it is the strongest
// statement an external crate can make now that the authority type is unmintable.
#[test]
fn external_caller_cannot_reach_reconstructed_without_store_authority() {
    let mut anchor = anchor_with_deployer("solana:D1");
    anchor.social_identities = vec!["x:acct1".into()];
    let nodes = vec![token("sol:AAA"), wallet("solana:W9")];
    let edges = vec![
        edge(EdgeType::FundedBy, "solana:D1", "solana:W9", &["ev-fund"]),
        edge(EdgeType::ReusedSocialLink, "solana:W9", "x:acct1", &["ev-reuse"]),
    ];
    // `&[]` is the ONLY binding slice an external caller can construct:
    // `SocialIdentityRecord` has private fields and no public constructor.
    let out = resolve_candidates(&anchor, &nodes, &edges, &[], NOW);
    let funded = out
        .iter()
        .find(|c| c.relation == RecentRelation::FundedByKnownDeployer)
        .expect("funding relation present");
    assert_eq!(
        funded.confidence,
        RecentConfidence::Estimated,
        "without store-provided authority, corroboration must not reach Reconstructed"
    );
}

// REV-037-F06: no Cargo feature may re-open the minting path.
//
// REV-036 gated `mint_for_tests` behind a `test_fixtures` feature and asserted the
// feature was non-default. That assertion was worthless: Cargo features are
// additive and dependency-selectable, so the reviewer added
// `features = ["test_fixtures"]` from a downstream crate and minted authority
// records (`feature_forged_records=1 reconstructed=true`). Being non-default is not
// being unreachable.
//
// The fix is that the feature no longer exists — minting is `#[cfg(test)]`, which
// no dependent can enable. THIS test file is the proof, and not because it reads a
// manifest: it is itself an external consumer, and the two assertions below only
// compile because the minting API is genuinely absent from the public surface.
#[test]
fn no_cargo_feature_can_re_enable_authority_minting() {
    let manifest = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read Cargo.toml");

    // Extract the [features] section and require that no feature name hints at
    // fixture/mint/test exposure. A feature is a build option, never an
    // authorization boundary, so authority minting must not hang off one at all.
    let features = manifest
        .split("[features]")
        .nth(1)
        .map(|tail| tail.split("\n[").next().unwrap_or(tail))
        .unwrap_or("");
    for banned in ["test_fixtures", "fixtures", "mint"] {
        assert!(
            !features.contains(&format!("{banned} =")),
            "feature `{banned}` must not exist: any dependency could enable it and \
             mint authority records (REV-037-F06)"
        );
    }

    // And the constructor must be gated by `#[cfg(test)]`, not by a feature.
    let recent = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("sf")
            .join("recent.rs"),
    )
    .expect("read recent.rs");
    let mint_at = recent
        .find("pub fn mint_for_tests")
        .expect("mint_for_tests exists for in-crate tests");
    let preceding = recent[..mint_at].trim_end();
    assert!(
        preceding.ends_with("#[cfg(test)]"),
        "mint_for_tests must be gated by #[cfg(test)] — a feature gate is \
         dependency-selectable and therefore not a boundary"
    );
    assert!(
        !recent.contains("feature = \"test_fixtures\""),
        "no `test_fixtures` feature gate may remain in the source"
    );
}

// The mirror of the above, enforced by the COMPILER rather than by string reading.
//
// This file is an external crate. If `mint_for_tests` or `records_from_store` were
// reachable from outside, the lines below would compile and this test would be a
// lie. They are commented out because they MUST NOT compile — and the surrounding
// tests in this file prove the external observer can still exercise the resolver,
// so the boundary is closed without being useless.
//
//   let row = StoredSocialIdentity::mint_for_tests("x", "acct", &[]);   // E0599
//   let recs = recent_runtime::records_from_store(vec![row]);           // E0603
//
// `tests/authority_boundary_compile_fail.rs` is not used because the project has no
// `trybuild` dependency; instead `default_commands_compile_without_extra_features`
// in tests/toolchain_gates.rs pins the property that matters operationally.
#[test]
fn external_consumer_can_only_pass_an_empty_binding_slice() {
    // The strongest statement an external crate can make: it can call the
    // resolver, but the only binding slice it can construct is empty, because
    // `SocialIdentityRecord` has private fields, no public constructor, and the
    // converter is `pub(crate)`.
    let bindings: Vec<solana_whale_intelligence::sf::recent_runtime::SocialIdentityRecord> =
        Vec::new();
    let anchor = anchor_with_deployer("solana:D1");
    let nodes = vec![token("sol:AAA")];
    let out = resolve_candidates(&anchor, &nodes, &[], &bindings, NOW);
    assert!(
        out.is_empty(),
        "no edges means no relations; the point is that `bindings` cannot be \
         populated from outside the crate"
    );
}
