//! Live PostgreSQL tests for the Recent-intelligence production pipeline
//! (REV-046-A4).
//!
//! The work order requires "one end-to-end PostgreSQL test plus real worker smoke
//! proves trigger creates persisted event and API returns it only in correct
//! workspace". These cover the persistence and workspace-isolation half; the worker
//! smoke half is exercised separately against a live database.
//!
//! Skipped (not failed) without `TEST_DATABASE_URL`/`DATABASE_URL`, so offline runs
//! stay green — the same convention as the other pg test modules.

#![cfg(all(test, feature = "pg_tests"))]

use sqlx::PgPool;

use super::graph::{EdgeType, EntityEdge, EntityNode, NodeType};
use super::recent::{ActivationTrigger, ActorExtraction};
use super::recent_pipeline::{run_for_anchor, WorkspaceScope};
use super::token::TokenLifecycle;

/// Fail closed without a live database (REV-056-F06). A test that skips its
/// assertions and reports `ok` is a gate that stopped telling you anything.
///
/// `crate::` is not reachable from `sf::` in the library, so the lookup is inline
/// here; the rule and the message are the same as `pg_test_support`.
fn live_url() -> String {
    for k in ["TEST_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    panic!(
        "the `pg_tests` feature is enabled but neither TEST_DATABASE_URL nor \
         DATABASE_URL is set; skipping would report success for assertions that never \
         ran (REV-056-F06)"
    );
}
/// A unique anchor per test run.
///
/// `recent_events` is APPEND-ONLY: migration 1019 installs a trigger that rejects
/// DELETE with "archive-not-delete". My first version of these tests tried to clean
/// up with `DELETE` and the statement was refused, so rows from an earlier run
/// survived and the next run correctly counted them as duplicates — which read as
/// "appended nothing" and looked like a pipeline bug.
///
/// The append-only rule is right and the test was wrong. Isolation therefore comes
/// from a fresh anchor rather than from deleting history.
fn unique_anchor(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("solana:PIPE_{tag}_{nanos}")
}

async fn pool() -> PgPool {
    PgPool::connect(&live_url())
        .await
        .expect("connect to the live test database")
}

fn edge(ty: EdgeType, from: &str, to: &str, refs: &[&str]) -> EntityEdge {
    EntityEdge {
        from_entity_key: from.into(),
        to_entity_key: to.into(),
        edge_type: ty,
        occurred_at: "2026-01-01T00:00:00Z".into(),
        source_id: None,
        truth_status: super::core::TruthStatus::Confirmed,
        confidence: None,
        valid_from: "2026-01-01T00:00:00Z".into(),
        valid_until: None,
        supersedes: None,
        evidence_refs: refs.iter().map(|s| s.to_string()).collect(),
    }
}

/// An anchor with a canonical deployer, plus a second token deployed by the same
/// wallet: the resolver should report `SameDeployer`.
fn scenario(anchor_token: &str, other_token: &str) -> (ActorExtraction, Vec<EntityNode>, Vec<EntityEdge>) {
    let anchor = ActorExtraction {
        token: anchor_token.into(),
        deployer: Some("solana:DEPLOYER1".into()),
        authority: None,
        fee_payer: None,
        factory: None,
        initial_funder: None,
        authority_changes: vec![],
        social_identities: vec![],
    };
    let nodes = vec![
        EntityNode { entity_key: anchor_token.into(), node_type: NodeType::Token },
        EntityNode { entity_key: other_token.into(), node_type: NodeType::Token },
    ];
    let edges = vec![edge(
        EdgeType::DeployedBy,
        "solana:DEPLOYER1",
        other_token,
        &["ev-deploy"],
    )];
    (anchor, nodes, edges)
}

// The pipeline must PERSIST what it resolves: resolving without appending is the gap
// the work order calls out, since an unpersisted relation is unreachable from the API.
#[tokio::test]
async fn trigger_resolves_and_persists_a_relation() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let anchor_token = unique_anchor("A");
    let anchor_token = anchor_token.as_str();
    let (anchor, nodes, edges) = scenario(anchor_token, "solana:PIPE_B");

    let out = run_for_anchor(
        &pool,
        ws,
        TokenLifecycle::Active,
        Some(ActivationTrigger::FirstLiquidity),
        &anchor,
        &nodes,
        &edges,
        chrono::Utc::now(),
    )
    .await
    .expect("pipeline runs");

    assert!(out.resolved > 0, "the scenario must resolve at least one relation");
    assert_eq!(out.appended, out.resolved, "everything resolved must be persisted");

    // Readback: the row is really in the store, not just counted.
    //
    // The scenario carries a deployer but no authority/initial_funder, so the run
    // ALSO appends one coverage-disclosure event (REV-050-F04). It is counted
    // separately from relations on purpose — a disclosure is not a relation — so the
    // expected total is relations + the disclosure.
    assert!(
        out.disclosed_partial_coverage,
        "an anchor missing authority/initial_funder must disclose partial coverage"
    );
    let persisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events WHERE workspace_id = $1 AND anchor_identity = $2",
    )
    .bind(ws.id())
    .bind(anchor_token)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(persisted as usize, out.appended + 1);

    // The disclosure names the inputs that were absent, and does NOT claim a
    // relation: "no relation" must not be readable as "no reuse".
    let disclosed: (String, Vec<String>, Option<String>) = sqlx::query_as(
        "SELECT coverage, \
                ARRAY(SELECT jsonb_array_elements_text(missing_inputs)), \
                relation::text \
           FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure'",
    )
    .bind(ws.id())
    .bind(anchor_token)
    .fetch_one(&pool)
    .await
    .expect("disclosure row");
    assert_eq!(disclosed.0, "degraded", "partial coverage must not read as full");
    assert_eq!(disclosed.1, vec!["authority", "initial_funder"]);
    assert!(disclosed.2.is_none(), "a disclosure asserts no relation");
}

// A retry must not double-publish. Without a stable key the same fact would be
// appended again on every pass.
#[tokio::test]
async fn a_second_run_is_idempotent() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let anchor_token = unique_anchor("IDEM");
    let anchor_token = anchor_token.as_str();
    let (anchor, nodes, edges) = scenario(anchor_token, "solana:PIPE_IDEM_B");

    let now = chrono::Utc::now();
    let first = run_for_anchor(
        &pool, ws, TokenLifecycle::Active, Some(ActivationTrigger::FirstLiquidity),
        &anchor, &nodes, &edges, now,
    )
    .await
    .expect("first run");
    let second = run_for_anchor(
        &pool, ws, TokenLifecycle::Active, Some(ActivationTrigger::FirstLiquidity),
        &anchor, &nodes, &edges, now,
    )
    .await
    .expect("second run");

    assert!(first.appended > 0);
    assert_eq!(second.appended, 0, "a retry must append nothing");
    assert_eq!(
        second.duplicates, first.appended,
        "the retry must recognise the earlier rows as duplicates"
    );

    // The disclosure is idempotent for the same gap as well: only the relation rows
    // plus ONE disclosure may exist after two runs.
    assert!(first.disclosed_partial_coverage);
    assert!(
        !second.disclosed_partial_coverage,
        "the same coverage gap must be disclosed once, not once per pass"
    );

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events WHERE workspace_id = $1 AND anchor_identity = $2",
    )
    .bind(ws.id())
    .bind(anchor_token)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        total,
        first.appended as i64 + 1,
        "no duplicate rows in the store (relations + one disclosure)"
    );
}

// A dormant token must not be resolved at all: the activation gate comes before any
// work, so a tombstoned asset is never individually polled.
#[tokio::test]
async fn a_dormant_token_is_not_resolved() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let dormant = unique_anchor("DORMANT");
    let (anchor, nodes, edges) = scenario(&dormant, "solana:PIPE_DORMANT_B");

    let out = run_for_anchor(
        &pool,
        ws,
        TokenLifecycle::Dormant,
        Some(ActivationTrigger::FirstLiquidity),
        &anchor,
        &nodes,
        &edges,
        chrono::Utc::now(),
    )
    .await
    .expect("pipeline runs");

    assert!(out.skipped_not_triggered);
    assert_eq!(out.resolved, 0);
    assert_eq!(out.appended, 0);
}

// ---------------------------------------------------------------------------
// REV-066-F05 — upgrade idempotency across a key-encoding version bump
// ---------------------------------------------------------------------------

/// Which shipped release's key encoding a legacy fixture is written with.
#[derive(Clone, Copy)]
enum LegacyKeyGeneration {
    /// REV-065: every field length-framed, NO version field. The IMMEDIATE
    /// predecessor of the current encoding.
    Rev065Framed,
    /// REV-063: delimiter-joined fields.
    Rev063Delimited,
}

/// The key a given shipped release would have minted, computed HERE.
///
/// REV-069-F05: the fixtures used to call `legacy_relation_event_keys()` — the
/// production helper under test — so implementation and fixture were wrong in the
/// same way and the tests stayed green while the immediate predecessor's format was
/// missing entirely. A regression must derive the predecessor key independently or
/// it proves nothing about compatibility.
///
/// These encoders are FROZEN copies of what each release shipped. They must never be
/// "kept in sync" with production; that is the whole point.
fn frozen_relation_key(
    generation: LegacyKeyGeneration,
    ws: WorkspaceScope,
    anchor: &str,
    relation: &str,
    target: &str,
    target_kind: super::recent::IdentityKind,
    confidence: super::recent::RecentConfidence,
    evidence_refs: &[String],
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    match generation {
        LegacyKeyGeneration::Rev065Framed => {
            frozen_framed(&mut h, &ws.id().to_le_bytes());
            frozen_framed(&mut h, anchor.as_bytes());
            frozen_framed(&mut h, relation.as_bytes());
            frozen_framed(&mut h, target.as_bytes());
            frozen_framed(&mut h, target_kind.as_str().as_bytes());
            frozen_framed(&mut h, confidence.as_str().as_bytes());
            frozen_framed(&mut h, &frozen_set_bytes(evidence_refs));
        }
        LegacyKeyGeneration::Rev063Delimited => {
            h.update(ws.id().to_le_bytes());
            h.update(b"|");
            h.update(anchor.as_bytes());
            h.update(b"|");
            h.update(relation.as_bytes());
            h.update(b"|");
            h.update(target.as_bytes());
            h.update(b"|");
            h.update(target_kind.as_str().as_bytes());
            h.update(b"|");
            h.update(confidence.as_str().as_bytes());
            h.update(b"|");
            h.update(&frozen_set_bytes(evidence_refs));
        }
    }
    format!("rel:{:x}", h.finalize())
}

/// Frozen coverage-key encoder, same rules as [`frozen_relation_key`].
fn frozen_coverage_key(
    generation: LegacyKeyGeneration,
    ws: WorkspaceScope,
    anchor: &str,
    missing_inputs: &[String],
    refresh_bucket: i64,
    predecessor: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    match generation {
        LegacyKeyGeneration::Rev065Framed => {
            frozen_framed(&mut h, &ws.id().to_le_bytes());
            frozen_framed(&mut h, anchor.as_bytes());
            frozen_framed(&mut h, &frozen_set_bytes(missing_inputs));
            frozen_framed(&mut h, &refresh_bucket.to_le_bytes());
            frozen_framed(&mut h, predecessor.as_bytes());
        }
        LegacyKeyGeneration::Rev063Delimited => {
            h.update(ws.id().to_le_bytes());
            h.update(b"|");
            h.update(anchor.as_bytes());
            h.update(b"|");
            h.update(&frozen_set_bytes(missing_inputs));
            h.update(b"|");
            h.update(refresh_bucket.to_le_bytes());
            h.update(b"|");
            h.update(predecessor.as_bytes());
        }
    }
    format!("cov:{:x}", h.finalize())
}

fn frozen_framed(h: &mut impl sha2::Digest, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

fn frozen_set_bytes(items: &[String]) -> Vec<u8> {
    let mut sorted: Vec<&str> = items.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut buf = Vec::new();
    for item in sorted {
        buf.extend_from_slice(&(item.len() as u64).to_le_bytes());
        buf.extend_from_slice(item.as_bytes());
    }
    buf
}

/// Seed the row a PREVIOUS release would have written for the candidate the given
/// scenario resolves: same semantic tuple, keyed with THAT release's encoding.
///
/// The candidates are resolved through the production resolver rather than
/// hand-built, so the seeded row is the one the pipeline will actually try to
/// re-publish. A hand-built approximation would let the guard pass against a fact
/// the pipeline never produces. The KEY, in contrast, is computed by the frozen
/// encoders above, never by the production helper under test (REV-069-F05).
async fn seed_legacy_relation_rows(
    pool: &PgPool,
    ws: WorkspaceScope,
    generation: LegacyKeyGeneration,
    anchor: &ActorExtraction,
    nodes: &[EntityNode],
    edges: &[EntityEdge],
    now: chrono::DateTime<chrono::Utc>,
    mutate: impl Fn(&mut super::recent::RecentEvent),
) -> usize {
    let candidates = super::recent_store::resolve_candidates_from_store(
        pool, ws.id(), anchor, nodes, edges, now.timestamp(),
    )
    .await
    .expect("resolve candidates");
    assert!(!candidates.is_empty(), "the scenario must resolve something to seed");

    for c in &candidates {
        let legacy_key = frozen_relation_key(
            generation,
            ws,
            &anchor.token,
            c.relation.as_str(),
            &c.to_identity.value,
            c.to_identity.kind,
            c.confidence,
            &c.evidence_refs,
        );
        let mut event = super::recent::RecentEvent {
            event_id: legacy_key,
            event_type: "relation_resolved".to_string(),
            anchor_identity: anchor.token.clone(),
            related_identities: vec![c.to_identity.clone()],
            chain_qualified_contract: anchor.token.clone(),
            occurred_at: now,
            observed_at: now,
            relation: Some(c.relation),
            truth_status: super::core::TruthStatus::Confirmed,
            confidence: None,
            confidence_level: c.confidence,
            evidence_refs: c.evidence_refs.clone(),
            dependency_group: None,
            freshness: None,
            coverage: super::recent::Coverage::Degraded,
            capability_status: super::recent::CapabilityStatus::Available,
            missing_inputs: vec!["authority".to_string(), "initial_funder".to_string()],
            retraction: None,
            is_current_coverage: false,
        };
        mutate(&mut event);
        assert!(
            super::recent_store::append_recent_event_if_absent(pool, ws.id(), &event)
                .await
                .expect("seed legacy row"),
            "the legacy seed must actually insert"
        );
    }
    candidates.len()
}

// REV-066-F05 case 1+2 / REV-069-F05. A fact published by ANY shipped predecessor
// must not be re-appended by this release. Versioning the key rewrites the identity
// of every stored fact, so without an upgrade path a plain retry after deploy appends
// a second row for an assertion that was never ambiguous.
//
// Both live generations are exercised. REV-069-F05: only the delimiter generation was
// covered, and the IMMEDIATE predecessor — REV-065, framed WITHOUT a version field —
// was missing from the compat list entirely. That is the format actually sitting in a
// database upgrading from REV-065, i.e. the one most likely to be hit.
#[tokio::test]
async fn a_fact_published_under_any_previous_key_encoding_is_not_appended_again() {
    for (label, generation, tag) in [
        ("REV-065 framed-without-version", LegacyKeyGeneration::Rev065Framed, "UPG1A"),
        ("REV-063 delimiter", LegacyKeyGeneration::Rev063Delimited, "UPG1B"),
    ] {
        let pool = pool().await;
        let ws = WorkspaceScope::from_job_context(1).unwrap();
        let anchor_token = unique_anchor(tag);
        let anchor_token = anchor_token.as_str();
        let (anchor, nodes, edges) = scenario(anchor_token, "solana:PIPE_UPG1_B");
        let now = chrono::Utc::now();

        let seeded = seed_legacy_relation_rows(
            &pool, ws, generation, &anchor, &nodes, &edges, now, |_| {},
        )
        .await;

        let out = run_for_anchor(
            &pool, ws, TokenLifecycle::Active, Some(ActivationTrigger::FirstLiquidity),
            &anchor, &nodes, &edges, now,
        )
        .await
        .expect("post-upgrade run");

        assert_eq!(
            out.appended, 0,
            "an identical fact already published under the {label} key must append \
             nothing after the encoding bump (REV-069-F05)"
        );
        assert_eq!(
            out.duplicates, seeded,
            "every resolved relation must be a duplicate under {label}"
        );

        let relations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM recent_events \
              WHERE workspace_id = $1 AND anchor_identity = $2 \
                AND event_type = 'relation_resolved'",
        )
        .bind(ws.id())
        .bind(anchor_token)
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(
            relations, seeded as i64,
            "exactly the {label} rows may exist; the upgrade must not double-publish"
        );
    }
}

// REV-066-F05 case 3 / REV-072-F05. The legacy key is NOT sufficient evidence on its
// own: a REV-063 delimiter key really can be shared by two DIFFERENT tuples — that
// ambiguity is the defect later encodings fix — so a stored row whose semantic tuple
// differs is a different fact and the new assertion must still be appended.
//
// REV-072-F05: this used to occupy a REV-065 FRAMED key with a hand-mutated row.
// REV-065 framing is injective, so no two tuples can share one of those keys: the
// fixture asserted the semantic guard against a collision that cannot exist. The
// preimage collision is constructed here for real, in the encoding that actually has
// one, and the test asserts the collision BEFORE relying on it — otherwise a change
// to the frozen encoder would silently turn this into the same vacuous test again.
//
//   pipeline tuple:  anchor = A|same_deployer|B      target = C
//   stored  tuple:   anchor = A                      target = B|same_deployer|C
//
// Both encode to `ws|anchor|relation|target|kind|confidence|evidence`, so the
// delimiter-joined byte stream is identical while the facts are not.
#[tokio::test]
async fn a_legacy_key_collision_with_a_different_fact_still_appends() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let a = format!("solana:UPG4A{nanos}");
    let b = format!("solana:UPG4B{nanos}");
    let c = format!("solana:UPG4C{nanos}");
    // The anchor the pipeline actually resolves: its VALUE contains the separator.
    let anchor_token = format!("{a}|same_deployer|{b}");
    // The tuple a previous release stored, whose REV-063 key is the same bytes.
    let colliding_anchor = a.clone();
    let colliding_target = format!("{b}|same_deployer|{c}");

    let (anchor, nodes, edges) = scenario(&anchor_token, &c);
    let now = chrono::Utc::now();

    // The candidate the pipeline will publish, resolved by the PRODUCTION resolver so
    // the collision is built against the fact the pipeline really produces.
    let candidates = super::recent_store::resolve_candidates_from_store(
        &pool, ws.id(), &anchor, &nodes, &edges, now.timestamp(),
    )
    .await
    .expect("resolve candidates");
    assert_eq!(candidates.len(), 1, "the scenario must resolve exactly one relation");
    let candidate = &candidates[0];
    assert_eq!(candidate.to_identity.value, c, "the resolved target must be C");

    let real_key = frozen_relation_key(
        LegacyKeyGeneration::Rev063Delimited,
        ws,
        &anchor_token,
        candidate.relation.as_str(),
        &candidate.to_identity.value,
        candidate.to_identity.kind,
        candidate.confidence,
        &candidate.evidence_refs,
    );
    let colliding_key = frozen_relation_key(
        LegacyKeyGeneration::Rev063Delimited,
        ws,
        &colliding_anchor,
        candidate.relation.as_str(),
        &colliding_target,
        candidate.to_identity.kind,
        candidate.confidence,
        &candidate.evidence_refs,
    );
    // The precondition. Without it the test could pass because the guard was never
    // consulted at all, which is exactly how the previous version passed.
    assert_eq!(
        real_key, colliding_key,
        "the fixture must build a REAL preimage collision under the frozen REV-063 \
         encoding; if these differ the test proves nothing about ambiguity"
    );

    // Seed the DIFFERENT fact under the shared legacy key.
    let stored = super::recent::RecentEvent {
        event_id: colliding_key.clone(),
        event_type: "relation_resolved".to_string(),
        anchor_identity: colliding_anchor.clone(),
        related_identities: vec![super::recent::IdentityKey {
            kind: candidate.to_identity.kind,
            value: colliding_target.clone(),
        }],
        chain_qualified_contract: colliding_anchor.clone(),
        occurred_at: now,
        observed_at: now,
        relation: Some(candidate.relation),
        truth_status: super::core::TruthStatus::Confirmed,
        confidence: None,
        confidence_level: candidate.confidence,
        evidence_refs: candidate.evidence_refs.clone(),
        dependency_group: None,
        freshness: None,
        coverage: super::recent::Coverage::Degraded,
        capability_status: super::recent::CapabilityStatus::Available,
        missing_inputs: vec!["authority".to_string(), "initial_funder".to_string()],
        retraction: None,
        is_current_coverage: false,
    };
    assert!(
        super::recent_store::append_recent_event_if_absent(&pool, ws.id(), &stored)
            .await
            .expect("seed the colliding legacy row"),
        "the legacy seed must actually insert"
    );

    let out = run_for_anchor(
        &pool, ws, TokenLifecycle::Active, Some(ActivationTrigger::FirstLiquidity),
        &anchor, &nodes, &edges, now,
    )
    .await
    .expect("post-upgrade run");

    assert_eq!(
        out.appended, 1,
        "a REV-063 key occupied by a DIFFERENT fact must not suppress this assertion; \
         inheriting the delimiter ambiguity would silently drop a real relation"
    );
    assert_eq!(out.duplicates, 0, "nothing here is a duplicate of anything");

    // Legacy row A survives untouched.
    let legacy_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND event_id = $2 AND anchor_identity = $3",
    )
    .bind(ws.id())
    .bind(&colliding_key)
    .bind(&colliding_anchor)
    .fetch_one(&pool)
    .await
    .expect("count legacy row");
    assert_eq!(legacy_rows, 1, "the stored legacy fact must remain exactly as it was");

    // The new fact exists exactly once, under the CURRENT key.
    let new_key = super::recent_pipeline::relation_event_key(
        ws,
        &anchor_token,
        candidate.relation.as_str(),
        &candidate.to_identity.value,
        candidate.to_identity.kind,
        candidate.confidence,
        &candidate.evidence_refs,
    );
    assert_ne!(new_key, colliding_key, "the current encoding must not collide");
    let new_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'relation_resolved'",
    )
    .bind(ws.id())
    .bind(&anchor_token)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(new_rows, 1, "the distinct fact must be published exactly once");
}

// REV-066-F05 case 4. Concurrent passes at the NEW version must still resolve to one
// winner: the legacy lookup reads history a prior release wrote, and must not become
// a check-then-insert race for same-version concurrency.
#[tokio::test]
async fn concurrent_post_upgrade_passes_still_yield_one_row() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let anchor_token = unique_anchor("UPG3");
    let anchor_token = anchor_token.as_str();
    let (anchor, nodes, edges) = scenario(anchor_token, "solana:PIPE_UPG3_B");
    let now = chrono::Utc::now();

    // Two independent pools so the passes are two backends, not one serialized queue.
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let p = PgPool::connect(&live_url()).await.expect("racing pool");
        let b = barrier.clone();
        let (a, n, e) = (anchor.clone(), nodes.clone(), edges.clone());
        handles.push(tokio::spawn(async move {
            b.wait().await;
            run_for_anchor(
                &p, ws, TokenLifecycle::Active, Some(ActivationTrigger::FirstLiquidity),
                &a, &n, &e, now,
            )
            .await
        }));
    }
    let mut appended = 0usize;
    let mut duplicates = 0usize;
    for h in handles {
        let out = h.await.expect("join").expect("concurrent pass");
        appended += out.appended;
        duplicates += out.duplicates;
    }

    let relations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'relation_resolved'",
    )
    .bind(ws.id())
    .bind(anchor_token)
    .fetch_one(&pool)
    .await
    .expect("count");

    // REV-072-F05: the property is EXACTLY ONE WINNER, so the assertion has to be
    // exact. `relations == appended && relations > 0` was satisfied by two winners —
    // two passes appending, two rows stored — which is precisely the double-publish
    // this test exists to forbid. The scenario resolves one relation, so the only
    // acceptable outcome is one append, one duplicate, one row.
    assert_eq!(
        appended, 1,
        "exactly one concurrent pass may append; {appended} did"
    );
    assert_eq!(
        duplicates, 1,
        "the losing pass must observe the duplicate outcome, not an error or a \
         second append; got {duplicates}"
    );
    assert_eq!(
        relations, 1,
        "exactly one row may exist after both passes; found {relations}"
    );
}

// REV-066-F05, coverage half — what this actually proves, and what it does not.
//
// A coverage row identifies a state TRANSITION whose predecessor is read from the
// store, and the append is gated on `changed || stale`. So after the encoding bump a
// state already disclosed under v1 does not reach the key at all: the pass reads the
// legacy row as the current state, sees an unchanged set inside the refresh bucket,
// and appends nothing. That is the property this test pins — the version bump cannot
// re-publish coverage history.
//
// It is NOT a test of the legacy-key compat lookup: that path only opens when two
// processes at different versions write the SAME transition concurrently, which this
// in-process test cannot stage without contriving row order. The compat lookup is
// covered directly below, at the guard, where its contract is observable.
#[tokio::test]
async fn a_coverage_state_disclosed_under_the_previous_encoding_is_not_reappended() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let anchor_token = unique_anchor("UPG4");
    let anchor_token = anchor_token.as_str();
    let (anchor, nodes, edges) = scenario(anchor_token, "solana:PIPE_UPG4_B");
    let now = chrono::Utc::now();

    // The very first disclosure has an EMPTY predecessor, which is the state a
    // pre-upgrade process would have written for this fresh anchor.
    let bucket = now.timestamp() / super::recent_pipeline::COVERAGE_REFRESH_SECONDS;
    let missing = vec!["authority".to_string(), "initial_funder".to_string()];
    // Keyed by the FROZEN immediate-predecessor encoder, computed here — not by the
    // production helper under test (REV-069-F05).
    let legacy_key = frozen_coverage_key(
        LegacyKeyGeneration::Rev065Framed, ws, anchor_token, &missing, bucket, "",
    );
    let legacy = legacy_disclosure(anchor_token, &legacy_key, &missing, now);
    assert!(
        super::recent_store::append_recent_event_if_absent(&pool, ws.id(), &legacy)
            .await
            .expect("seed legacy disclosure"),
        "the legacy disclosure seed must insert"
    );

    let out = run_for_anchor(
        &pool, ws, TokenLifecycle::Active, Some(ActivationTrigger::FirstLiquidity),
        &anchor, &nodes, &edges, now,
    )
    .await
    .expect("post-upgrade run");
    assert!(
        !out.disclosed_partial_coverage,
        "the same coverage state already disclosed under v1 must not be re-published"
    );

    let disclosures: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure'",
    )
    .bind(ws.id())
    .bind(anchor_token)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(disclosures, 1, "exactly one disclosure may exist for this state");
}

fn legacy_disclosure(
    anchor: &str,
    event_id: &str,
    missing: &[String],
    now: chrono::DateTime<chrono::Utc>,
) -> super::recent::RecentEvent {
    super::recent::RecentEvent {
        event_id: event_id.to_string(),
        event_type: "coverage_disclosure".to_string(),
        anchor_identity: anchor.to_string(),
        related_identities: vec![],
        chain_qualified_contract: anchor.to_string(),
        occurred_at: now,
        observed_at: now,
        relation: None,
        truth_status: super::core::TruthStatus::Confirmed,
        confidence: None,
        confidence_level: super::recent::RecentConfidence::Insufficient,
        evidence_refs: vec![],
        dependency_group: None,
        freshness: None,
        coverage: super::recent::Coverage::Degraded,
        capability_status: super::recent::CapabilityStatus::Available,
        missing_inputs: missing.to_vec(),
        retraction: None,
        is_current_coverage: false,
    }
}

// The compat guard itself, on a live store. Its contract has two halves and both
// matter: a legacy row publishing THIS state must suppress the re-append, and a
// legacy row under the same key publishing a DIFFERENT state must not — otherwise
// the guard would inherit the v1 ambiguity and swallow a real disclosure.
#[tokio::test]
async fn the_legacy_compat_guard_matches_on_the_state_not_the_key() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let anchor_token = unique_anchor("UPG5");
    let anchor_token = anchor_token.as_str();
    let now = chrono::Utc::now();
    let bucket = now.timestamp() / super::recent_pipeline::COVERAGE_REFRESH_SECONDS;

    let missing_a = vec!["authority".to_string(), "initial_funder".to_string()];
    let missing_b = vec!["deployer".to_string()];
    // Each shipped generation gets its own seeded row, keyed by the FROZEN encoder
    // for that release — never by the production helper under test (REV-069-F05).
    // The production helper is then asked whether it recognises them; a generation
    // missing from its list fails here.
    let legacy_keys = super::recent_pipeline::legacy_coverage_event_keys(
        ws, anchor_token, &missing_a, bucket, "",
    );
    for (label, generation) in [
        ("REV-065 framed-without-version", LegacyKeyGeneration::Rev065Framed),
        ("REV-063 delimiter", LegacyKeyGeneration::Rev063Delimited),
    ] {
        let frozen = frozen_coverage_key(generation, ws, anchor_token, &missing_a, bucket, "");
        assert!(
            legacy_keys.contains(&frozen),
            "the compat list must include the {label} key, or a database upgrading \
             from that release double-publishes (REV-069-F05)"
        );
        let legacy = legacy_disclosure(anchor_token, &frozen, &missing_a, now);
        assert!(
            super::recent_store::append_recent_event_if_absent(&pool, ws.id(), &legacy)
                .await
                .expect("seed"),
            "the {label} seed must insert"
        );

        // Same state, new encoding: recognised, so the pass would not double-publish.
        let same = legacy_disclosure(anchor_token, "cov:whatever-v2", &missing_a, now);
        assert!(
            super::recent_store::legacy_row_publishes_the_same_fact(
                &pool, ws.id(), std::slice::from_ref(&frozen), &same
            )
            .await
            .expect("guard"),
            "a {label} row asserting THIS state must be recognised as already published"
        );
    }

    // Different state under the SAME legacy key: not this fact, so it stays appendable.
    let different = legacy_disclosure(anchor_token, "cov:whatever-v2", &missing_b, now);
    assert!(
        !super::recent_store::legacy_row_publishes_the_same_fact(
            &pool, ws.id(), &legacy_keys, &different
        )
        .await
        .expect("guard"),
        "a legacy key occupied by a DIFFERENT state must not suppress this disclosure"
    );

    // Another tenant's legacy row is another tenant's fact.
    let same = legacy_disclosure(anchor_token, "cov:whatever-v2", &missing_a, now);
    let other_ws = WorkspaceScope::from_job_context(2).unwrap();
    assert!(
        !super::recent_store::legacy_row_publishes_the_same_fact(
            &pool, other_ws.id(), &legacy_keys, &same
        )
        .await
        .expect("guard"),
        "the compat lookup must stay workspace-scoped"
    );
}
