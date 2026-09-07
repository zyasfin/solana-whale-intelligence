//! REV-053-F01/F02: writer-to-worker reachability on the REAL production path.
//!
//! REV-053's objection is the one that matters: REV-052's C1 test seeded
//! `confidence=0.9, promoted=true` by hand, which proves cross-token exclusion in the
//! READER while saying nothing about whether the production WRITER can ever produce a
//! row the reader accepts. It could not. The reviewer's live probe measured
//! `confidence=0.3500 promoted=false`, the reader filtered on `promoted = true`, and
//! `token discover --once` reported `Recent before=0 / after=0`.
//!
//! So these tests never insert a `funding_edges` row themselves. Every row comes from
//! `graph::update_funding_edges()` — the same function the webhook ingest path calls —
//! and the assertions are made against `recent_events` after the real worker runs.
//!
//! Skipped (not failed) without `TEST_DATABASE_URL`/`DATABASE_URL`.

#![cfg(all(test, feature = "pg_tests"))]

use chrono::Utc;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::models::{AssetKind, ChainKind, Commitment, NormalizedTransfer};
use solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope;

async fn pool() -> PgPool {
    crate::pg_test_support::live_pool().await
}


fn tag(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}{nanos}")
}

/// A normalized SPL-token transfer, the shape the Solana adapter produces.
fn token_transfer(mint: &str, from: &str, to: &str, signature: &str) -> NormalizedTransfer {
    let now = Utc::now();
    NormalizedTransfer {
        chain: ChainKind::Solana,
        signature: signature.to_string(),
        event_index: 0,
        from_address: from.to_string(),
        to_address: to.to_string(),
        asset_kind: AssetKind::SplToken,
        mint: mint.to_string(),
        raw_amount: "1000000".to_string(),
        amount: Decimal::from(1),
        slot: Some(1),
        // Non-null on purpose: the reader excludes a NULL `block_time`, and a writer
        // that never sets one would be a second unreachability bug.
        block_time: Some(now),
        observed_at: now,
        source: "rev054-writer-probe".to_string(),
        commitment: Commitment::Confirmed,
    }
}

/// Count `recent_events` rows for one anchor, by type.
async fn count_events(pool: &PgPool, workspace_id: i64, anchor: &str, event_type: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 AND event_type = $3",
    )
    .bind(workspace_id)
    .bind(anchor)
    .bind(event_type)
    .fetch_one(pool)
    .await
    .expect("count recent_events")
}

// REV-053-F01. The row the PRODUCTION WRITER creates must be visible to the worker.
//
// This is the exact gap REV-053 measured: `update_funding_edges` scores a single
// funding component at 0.35, so `promoted` is false and the old reader saw nothing.
#[tokio::test]
async fn the_production_writer_produces_an_edge_the_worker_can_read() {
    let pool = pool().await;
    let mint = tag("W1MINT");
    let from = tag("W1FROM");
    let to = tag("W1TO");

    // The real writer, not a hand-built row.
    crate::graph::update_funding_edges(&pool, &token_transfer(&mint, &from, &to, &tag("w1sig")))
        .await
        .expect("production writer");

    // Confirm the premise REV-053 established, so this test fails loudly if the
    // writer's scoring ever changes without the reader being reconsidered.
    let (confidence, promoted): (Decimal, bool) = sqlx::query_as(
        "SELECT confidence, promoted FROM funding_edges \
          WHERE chain = $1 AND evidence ->> 'mint' = $2",
    )
    .bind(ChainKind::Solana.as_str())
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("read back the written edge");
    assert_eq!(
        confidence,
        Decimal::from_str_exact("0.3500").unwrap(),
        "the production writer records one funding component (0.35)"
    );
    assert!(
        !promoted,
        "a single funding observation must NOT promote wallet-cluster membership"
    );

    // And the worker's reader must nevertheless see it, carrying its weakness.
    let edges = crate::workers::token_owned_funding_edges(&pool, ChainKind::Solana, &mint)
        .await
        .expect("reader");
    assert_eq!(
        edges.len(),
        1,
        "the worker must read the edge the production writer just created \
         (REV-053-F01: filtering on `promoted` made this unreachable)"
    );
    assert!(
        !edges[0].promoted,
        "the edge's promotion state must be carried, not silently upgraded"
    );
    assert_eq!(edges[0].from_address, from);
}

// The full production chain: writer -> worker -> resolver -> `recent_events`.
//
// REV-060-F03: the worker now derives `initial_funder` from the token's earliest
// funding edge, so a token with ONE funding edge is no longer "all three inputs
// unknown". The funding edge is still unpromoted (truth_status `Unknown`), so it
// resolves a `SameFunder` CANDIDATE at `Insufficient` — never an `Exact` family
// merge. The point REV-054 guarded still holds: an unpromoted edge is evidence two
// wallets interacted, not that they are one actor, so it must not reach `Exact`.
#[tokio::test]
async fn writer_to_worker_publishes_a_disclosure_and_never_a_forged_relation() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let mint = tag("W2MINT");
    let anchor = format!("solana:{mint}");

    crate::graph::update_funding_edges(
        &pool,
        &token_transfer(&mint, &tag("W2FROM"), &tag("W2TO"), &tag("w2sig")),
    )
    .await
    .expect("production writer");

    crate::workers::resolve_recent_for_token(&pool, ws, ChainKind::Solana, &mint, Utc::now())
        .await
        .expect("worker resolves");

    let disclosures = count_events(&pool, ws.id(), &anchor, "coverage_disclosure").await;

    assert_eq!(
        disclosures, 1,
        "a discovered token with no deployer/authority input must publish \
         exactly one coverage disclosure (REV-053-F02)"
    );

    // The disclosure names the inputs that are STILL unknown. `initial_funder` is
    // now derivable from the earliest funding edge, so it is no longer missing.
    let (coverage, missing, relation): (String, Vec<String>, Option<String>) = sqlx::query_as(
        "SELECT coverage, \
                ARRAY(SELECT jsonb_array_elements_text(missing_inputs)), \
                relation::text \
           FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure'",
    )
    .bind(ws.id())
    .bind(&anchor)
    .fetch_one(&pool)
    .await
    .expect("disclosure row");
    assert_eq!(coverage, "degraded");
    assert_eq!(
        missing,
        vec!["deployer", "authority"],
        "initial_funder is derived from the earliest funding edge, so only \
         deployer + authority remain unknown (REV-060-F03)"
    );
    assert!(relation.is_none());

    // The funding edge goes wallet -> wallet, so the derived `initial_funder` names a
    // WALLET, not a second token. No `SameFunder` relation is emitted for a
    // wallet-targeting edge, and an unpromoted edge is not evidence of one actor
    // anyway — so the relation list stays empty. The point REV-054 guarded is intact:
    // deriving initial_funder must not fabricate a family merge.
    assert_eq!(
        count_events(&pool, ws.id(), &anchor, "relation_resolved").await,
        0,
        "initial_funder is a wallet (the edge targets a wallet, not a token), and an \
         unpromoted edge is not evidence of one actor; no relation may be forged from it"
    );
    // No EXACT relation either.
    let exact: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'relation_resolved' \
            AND confidence_level = 'exact'",
    )
    .bind(ws.id())
    .bind(&anchor)
    .fetch_one(&pool)
    .await
    .expect("count exact");
    assert_eq!(exact, 0, "an unpromoted edge must never become an Exact relation");

    // Idempotent: a second discovery pass must not append a second disclosure.
    crate::workers::resolve_recent_for_token(&pool, ws, ChainKind::Solana, &mint, Utc::now())
        .await
        .expect("second pass");
    assert_eq!(
        count_events(&pool, ws.id(), &anchor, "coverage_disclosure").await,
        1,
        "the same coverage gap must be disclosed once, not once per discovery pass"
    );
}

// REV-053-F02, the case with NO evidence at all: a token discovered before any
// funding edge exists. Previously the worker returned early and published nothing.
#[tokio::test]
async fn a_token_with_no_evidence_still_publishes_its_coverage_limit() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let mint = tag("W3MINT");
    let anchor = format!("solana:{mint}");

    // No writer call at all: zero funding edges for this token.
    let edges = crate::workers::token_owned_funding_edges(&pool, ChainKind::Solana, &mint)
        .await
        .expect("reader");
    assert!(edges.is_empty(), "precondition: this token has no evidence");

    crate::workers::resolve_recent_for_token(&pool, ws, ChainKind::Solana, &mint, Utc::now())
        .await
        .expect("worker resolves");

    assert_eq!(
        count_events(&pool, ws.id(), &anchor, "coverage_disclosure").await,
        1,
        "an empty evidence set must be a DISCLOSED state, not a silent return: an \
         empty timeline is otherwise read as \"no reuse\" (REV-048/REV-053-F02)"
    );
    assert_eq!(
        count_events(&pool, ws.id(), &anchor, "relation_resolved").await,
        0,
        "no evidence must never produce a relation"
    );
}

// A PROMOTED edge is the corroborated case, and it must still not invent a relation
// while the deployer/authority/funder inputs are absent. This pins that raising
// promotion alone cannot manufacture reuse.
#[tokio::test]
async fn a_promoted_edge_alone_still_resolves_no_relation() {
    let pool = pool().await;
    let ws = WorkspaceScope::from_job_context(1).unwrap();
    let mint = tag("W4MINT");
    let anchor = format!("solana:{mint}");
    let from = tag("W4FROM");
    let to = tag("W4TO");

    // Accumulated independent evidence, written through the real evidence recorder:
    // funding + token-account creation + repeated funding = 0.35 + 0.25 + 0.15 = 0.75,
    // which clears the 0.70 membership threshold.
    let components = crate::graph::EdgeComponents {
        funding: true,
        token_account_creation: true,
        repeated_funding: true,
        ..Default::default()
    };
    assert!(
        crate::graph::score_edge_confidence(&components) >= crate::graph::MEMBERSHIP_THRESHOLD,
        "precondition: these components must promote"
    );
    crate::graph::record_edge_evidence(
        &pool,
        ChainKind::Solana,
        &from,
        &to,
        &components,
        serde_json::json!({"mint": mint, "probe": "rev054"}),
    )
    .await
    .expect("evidence recorder");

    crate::workers::resolve_recent_for_token(&pool, ws, ChainKind::Solana, &mint, Utc::now())
        .await
        .expect("worker resolves");

    assert_eq!(
        count_events(&pool, ws.id(), &anchor, "relation_resolved").await,
        0,
        "promotion is wallet-cluster membership; without a deployer/authority/funder \
         actor it must not become a token relation"
    );
    assert_eq!(
        count_events(&pool, ws.id(), &anchor, "coverage_disclosure").await,
        1,
        "the coverage limit is disclosed regardless of promotion"
    );
}
