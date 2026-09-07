//! PostgreSQL integration tests for the recent-intelligence store (REV-025-F07).
//!
//! Requires a disposable database; skipped unless `DATABASE_URL` / `TEST_DATABASE_URL`
//! is set and the `pg_tests` feature is enabled. Verifies the typed persistence
//! round-trip: a non-null `recent_relation` enum + `timestamptz` bind directly
//! (never a Rust `String`), workspace isolation, and one-row-per-target relation
//! projection.

#![cfg(feature = "pg_tests")]
#![cfg(test)]

use chrono::{TimeZone, Utc};
use sqlx::PgPool;

use solana_whale_intelligence::sf::core::TruthStatus;
use solana_whale_intelligence::sf::recent::{
    CapabilityStatus, Coverage, RecentConfidence, RecentEvent, RecentRelation,
};
use solana_whale_intelligence::sf::recent_store::{
    append_recent_event_if_absent, fetch_recent_timeline, fetch_relations,
};

fn event(
    event_id: &str,
    anchor: &str,
    relation: Option<RecentRelation>,
    confidence: RecentConfidence,
) -> RecentEvent {
    RecentEvent {
        event_id: event_id.into(),
        event_type: "transfer".into(),
        anchor_identity: anchor.into(),
        related_identities: vec![],
        chain_qualified_contract: anchor.into(),
        occurred_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        observed_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap(),
        relation,
        truth_status: TruthStatus::Confirmed,
        confidence: Some(0.9),
        confidence_level: confidence,
        evidence_refs: vec!["ev-1".into()],
        dependency_group: None,
        freshness: None,
        coverage: Coverage::Full,
        capability_status: CapabilityStatus::Available,
        missing_inputs: vec![],
        retraction: None,
        is_current_coverage: false,
    }
}

/// Typed round-trip: a non-null relation + timestamptz bind directly.
#[sqlx::test(migrations = false)]
async fn insert_and_fetch_typed_event(pool: PgPool) {
    crate::pg_test_support::migrate_scratch(&pool).await;
    let e = event("e1", "sol:AAA", Some(RecentRelation::SameDeployer), RecentConfidence::Exact);
    assert!(
        append_recent_event_if_absent(&pool, 1, &e).await.expect("insert"),
        "the first append must insert"
    );

    let rows = fetch_recent_timeline(&pool, 1, "sol:AAA", "all").await.expect("fetch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].relation, Some(RecentRelation::SameDeployer));
    assert_eq!(rows[0].confidence_level, RecentConfidence::Exact);
    assert_eq!(rows[0].event_id, "e1");
}

/// Workspace isolation: the same token in a different workspace is not leaked.
#[sqlx::test(migrations = false)]
async fn workspace_isolation(pool: PgPool) {
    crate::pg_test_support::migrate_scratch(&pool).await;
    let e = event("e1", "sol:AAA", Some(RecentRelation::SameDeployer), RecentConfidence::Exact);
    append_recent_event_if_absent(&pool, 1, &e).await.expect("insert ws1");

    let ws2 = fetch_recent_timeline(&pool, 2, "sol:AAA", "all").await.expect("fetch ws2");
    assert!(ws2.is_empty(), "workspace 2 must not see workspace 1 rows");
}

/// Relation projection: multiple targets under the same relation all survive,
/// and the stored confidence is preserved (not hard-coded `Estimated`).
#[sqlx::test(migrations = false)]
async fn relation_projection_preserves_confidence_and_targets(pool: PgPool) {
    crate::pg_test_support::migrate_scratch(&pool).await;
    let mut e1 = event("e1", "sol:AAA", Some(RecentRelation::SameDeployer), RecentConfidence::Exact);
    e1.related_identities = vec![solana_whale_intelligence::sf::recent::IdentityKey {
        kind: solana_whale_intelligence::sf::recent::IdentityKind::Token,
        value: "sol:BBB".into(),
    }];
    let mut e2 = event("e2", "sol:AAA", Some(RecentRelation::SameDeployer), RecentConfidence::Exact);
    e2.related_identities = vec![solana_whale_intelligence::sf::recent::IdentityKey {
        kind: solana_whale_intelligence::sf::recent::IdentityKind::Token,
        value: "sol:CCC".into(),
    }];
    append_recent_event_if_absent(&pool, 1, &e1).await.expect("insert e1");
    append_recent_event_if_absent(&pool, 1, &e2).await.expect("insert e2");

    let rels = fetch_relations(&pool, 1, "sol:AAA").await.expect("relations");
    assert_eq!(rels.len(), 2, "two SameDeployer targets must both survive");
    assert!(rels.iter().all(|c| c.confidence == RecentConfidence::Exact));
}
