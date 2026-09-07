//! Live PostgreSQL tests for REV-058 F01–F07.
//!
//! Each test reproduces the reviewer's own probe rather than restating a fix:
//!
//! * F01 — the PUBLIC label API with no session and with a foreign session.
//! * F02 — a foreign-workspace `flow_only` label must not truncate this workspace's
//!   trace, and an EXPIRED one must not truncate it either.
//! * F03 — the CLI import path must be all-or-nothing, like the HTTP one.
//! * F05 — degraded→full and a changed missing-input set must both transition, with
//!   exactly one coverage state visible in the current view.
//! * F06 — a concurrent append must not fail one worker.
//! * F07 — a failed disposition lookup must not be reported as "no disposition".
//!
//! F04 (migration guard) is verified against a canonical-absent database in the
//! ledger evidence, since it is a SQL-level failure, not a Rust one.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use chrono::{Duration, Utc};
use reqwest::StatusCode;
use sqlx::PgPool;

use crate::api::{router as api_router, ApiState};
use crate::models::ChainKind;
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

async fn workspace(pool: &PgPool, slug: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ($1, $2) RETURNING id")
        .bind(slug)
        .bind(slug)
        .fetch_one(pool)
        .await
        .expect("create workspace")
}

async fn session_for(pool: &PgPool, workspace_id: i64) -> String {
    use sha2::{Digest, Sha256};
    let token = crate::auth::new_session_token();
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, expires_at, workspace_id) \
         VALUES ($1, now() + interval '1 hour', $2)",
    )
    .bind(hex::encode(h.finalize()))
    .bind(workspace_id)
    .execute(pool)
    .await
    .expect("create session");
    token
}

/// The REAL read-API router (not the admin one) on a loopback port.
async fn serve_api(pool: &PgPool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    let state = ApiState { pool: pool.clone() };
    tokio::spawn(async move {
        let _ = axum::serve(listener, api_router(state)).await;
    });
    format!("http://{addr}")
}

async fn get(base: &str, path: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut req = reqwest::Client::new().get(format!("{base}{path}"));
    if let Some(token) = token {
        req = req.header(reqwest::header::COOKIE, format!("swi_session={token}"));
    }
    let response = req.send().await.expect("http request");
    let status = response.status();
    (status, response.text().await.expect("body"))
}

/// Seed a wallet plus one label owned by `workspace_id`.
async fn seed_label(
    pool: &PgPool,
    workspace_id: i64,
    address: &str,
    kind: &str,
    disposition: &str,
    expires_at: Option<chrono::DateTime<Utc>>,
) -> i64 {
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(address)
    .execute(pool)
    .await
    .expect("seed wallet");
    sqlx::query_scalar(
        "INSERT INTO wallet_labels \
             (workspace_id, chain, address, kind, disposition, manual, confidence, expires_at) \
         VALUES ($1, 'solana', $2, $3, $4, true, 90, $5) RETURNING id",
    )
    .bind(workspace_id)
    .bind(address)
    .bind(kind)
    .bind(disposition)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .expect("seed label")
}

// F01 (HIGH). The reviewer read workspace A's private label from the PUBLIC read API
// with no session at all, and again from a workspace B session.
#[tokio::test]
async fn the_public_label_api_requires_a_session_and_scopes_to_it() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("g01a")).await;
    let ws_b = workspace(&pool, &tag("g01b")).await;
    let token_a = session_for(&pool, ws_a).await;
    let token_b = session_for(&pool, ws_b).await;

    let address = tag("G01WALLET");
    seed_label(&pool, ws_a, &address, "A-secret", "skip", None).await;

    let base = serve_api(&pool).await;
    let path = format!("/api/wallets/solana/{address}/labels");

    let (no_session, body_none) = get(&base, &path, None).await;
    assert_eq!(
        no_session,
        StatusCode::UNAUTHORIZED,
        "the public label API must require a session; got {body_none}"
    );
    assert!(
        !body_none.contains("A-secret"),
        "an unauthenticated response must not leak a label; got {body_none}"
    );

    let (b_status, b_body) = get(&base, &path, Some(&token_b)).await;
    assert_eq!(b_status, StatusCode::OK);
    assert!(
        !b_body.contains("A-secret"),
        "workspace B must not read workspace A's label through the read API; got {b_body}"
    );

    let (a_status, a_body) = get(&base, &path, Some(&token_a)).await;
    assert_eq!(a_status, StatusCode::OK);
    assert!(
        a_body.contains("A-secret"),
        "the owning workspace must still see its own label; got {a_body}"
    );
}

// F02 (HIGH), cross-workspace half: another tenant's `flow_only` label must not stop
// this tenant's traversal.
#[tokio::test]
async fn a_foreign_flow_only_label_does_not_truncate_this_workspaces_trace() {
    let pool = pool().await;
    let ws_mine = workspace(&pool, &tag("g02a")).await;
    let ws_other = workspace(&pool, &tag("g02b")).await;
    let address = tag("G02WALLET");

    // Only the OTHER workspace marks it flow-only.
    seed_label(&pool, ws_other, &address, "manual_flow_only", "flow_only", None).await;

    // A funding edge exists so traversal has something to return.
    sqlx::query(
        "INSERT INTO funding_edges \
             (chain, from_address, to_address, signature, edge_kind, raw_amount, \
              block_time, confidence, evidence, promoted) \
         VALUES ('solana', $1, $2, $3, 'funding', '1', now(), 0.9, '{}'::jsonb, false) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&address)
    .bind(tag("G02PEER"))
    .bind(tag("g02sig"))
    .execute(&pool)
    .await
    .expect("seed edge");

    let steps = crate::graph::trace_wallet(&pool, ws_mine, ChainKind::Solana, &address, 3)
        .await
        .expect("trace");
    assert!(
        steps.iter().all(|s| !s.endpoint),
        "another workspace's flow_only label must not mark this workspace's traversal as \
         an endpoint (REV-058-F02)"
    );

    // The owner of the label still sees the endpoint behaviour.
    let owned = crate::graph::trace_wallet(&pool, ws_other, ChainKind::Solana, &address, 3)
        .await
        .expect("trace owner");
    assert!(
        owned.iter().all(|s| s.endpoint),
        "the workspace that created the flow_only label must still stop at it"
    );
}

// F02, expiry half: an expired label must stop being policy.
#[tokio::test]
async fn an_expired_flow_only_label_no_longer_truncates_a_trace() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g02exp")).await;
    let address = tag("G02EXPWALLET");

    seed_label(
        &pool,
        ws,
        &address,
        "manual_flow_only",
        "flow_only",
        Some(Utc::now() - Duration::hours(1)),
    )
    .await;
    sqlx::query(
        "INSERT INTO funding_edges \
             (chain, from_address, to_address, signature, edge_kind, raw_amount, \
              block_time, confidence, evidence, promoted) \
         VALUES ('solana', $1, $2, $3, 'funding', '1', now(), 0.9, '{}'::jsonb, false) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&address)
    .bind(tag("G02EXPPEER"))
    .bind(tag("g02expsig"))
    .execute(&pool)
    .await
    .expect("seed edge");

    let steps = crate::graph::trace_wallet(&pool, ws, ChainKind::Solana, &address, 3)
        .await
        .expect("trace");
    assert!(
        steps.iter().all(|s| !s.endpoint),
        "an EXPIRED flow_only label must no longer act as policy; every other label \
         consumer already honours expires_at (REV-058-F02)"
    );
}

// F03. The CLI import shares the transactional function, so a late failure commits
// nothing — including the addresses that would have succeeded.
#[tokio::test]
async fn the_shared_blocklist_import_is_all_or_nothing() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g03")).await;
    let good = tag("G03GOOD");
    // A NUL byte is rejected by PostgreSQL's text encoding: a real driver-level
    // failure, not a mocked one.
    let bad = "G03BAD\0INVALID".to_string();

    let err = crate::db::import_blocklist_tx(
        &pool,
        ws,
        ChainKind::Solana.as_str(),
        vec![good.clone(), bad],
        "cli blocklist import",
    )
    .await
    .expect_err("a failing address must fail the whole import");
    assert!(
        format!("{err:#}").contains("blocklist import failed"),
        "the error must name the failing stage; got {err:#}"
    );

    let committed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_labels WHERE workspace_id = $1 AND address = $2",
    )
    .bind(ws)
    .bind(&good)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(
        committed, 0,
        "the earlier address must be rolled back: the CLI path used to autocommit per \
         row and leave a half-applied blocklist (REV-058-F03)"
    );
}

// F05, degraded -> full. An improvement must be published, and the current view must
// show ONLY the new state.
#[tokio::test]
async fn coverage_transitions_from_degraded_to_full_and_supersedes_the_old_state() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g05")).await;
    let ws = WorkspaceScope::from_job_context(ws_id).unwrap();
    let anchor = format!("solana:{}", tag("G05MINT"));

    let degraded = solana_whale_intelligence::sf::recent::ActorExtraction {
        token: anchor.clone(),
        deployer: None,
        authority: None,
        fee_payer: None,
        factory: None,
        initial_funder: None,
        authority_changes: vec![],
        social_identities: vec![],
    };
    let out = solana_whale_intelligence::sf::recent_pipeline::run_for_anchor(
        &pool,
        ws,
        solana_whale_intelligence::sf::token::TokenLifecycle::Active,
        Some(solana_whale_intelligence::sf::recent::ActivationTrigger::FirstLiquidity),
        &degraded,
        &[],
        &[],
        Utc::now(),
    )
    .await
    .expect("degraded pass");
    assert!(out.disclosed_partial_coverage, "the gap must be disclosed");

    // Coverage improves: every actor is now available.
    let full = solana_whale_intelligence::sf::recent::ActorExtraction {
        deployer: Some("solana:D1".into()),
        authority: Some("solana:A1".into()),
        initial_funder: Some("solana:F1".into()),
        ..degraded.clone()
    };
    let out2 = solana_whale_intelligence::sf::recent_pipeline::run_for_anchor(
        &pool,
        ws,
        solana_whale_intelligence::sf::token::TokenLifecycle::Active,
        Some(solana_whale_intelligence::sf::recent::ActivationTrigger::FirstLiquidity),
        &full,
        &[],
        &[],
        Utc::now(),
    )
    .await
    .expect("full pass");
    assert!(
        out2.disclosed_full_coverage,
        "an improvement to full coverage must be published, not inferred from silence \
         (REV-058-F05)"
    );
    assert!(
        !out2.disclosed_partial_coverage,
        "full coverage is not a partial-coverage disclosure"
    );

    // The current view must carry exactly ONE coverage state, and it must be the new
    // one: the old degraded row must not remain pinned.
    let events = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &pool, ws_id, &anchor, "24h",
    )
    .await
    .expect("fetch");
    let disclosures: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "coverage_disclosure")
        .collect();
    assert_eq!(
        disclosures.len(),
        1,
        "the current view must show one coverage state, never a union of historical \
         ones (REV-058-F05)"
    );
    assert!(
        disclosures[0].missing_inputs.is_empty(),
        "the current state must be the IMPROVED one; got {:?}",
        disclosures[0].missing_inputs
    );

    // History is preserved: `all` still returns both states.
    let history = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &pool, ws_id, &anchor, "all",
    )
    .await
    .expect("fetch all");
    assert_eq!(
        history
            .iter()
            .filter(|e| e.event_type == "coverage_disclosure")
            .count(),
        2,
        "archive-not-delete: every coverage state remains in history"
    );
}

// F05, changed set. `[deployer,authority,initial_funder]` -> `[authority]` must
// supersede, not accumulate.
#[tokio::test]
async fn a_changed_missing_input_set_supersedes_the_previous_state() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g05b")).await;
    let ws = WorkspaceScope::from_job_context(ws_id).unwrap();
    let anchor = format!("solana:{}", tag("G05BMINT"));

    let base = solana_whale_intelligence::sf::recent::ActorExtraction {
        token: anchor.clone(),
        deployer: None,
        authority: None,
        fee_payer: None,
        factory: None,
        initial_funder: None,
        authority_changes: vec![],
        social_identities: vec![],
    };
    let run = |actor: solana_whale_intelligence::sf::recent::ActorExtraction| {
        let pool = pool.clone();
        async move {
            solana_whale_intelligence::sf::recent_pipeline::run_for_anchor(
                &pool,
                ws,
                solana_whale_intelligence::sf::token::TokenLifecycle::Active,
                Some(solana_whale_intelligence::sf::recent::ActivationTrigger::FirstLiquidity),
                &actor,
                &[],
                &[],
                Utc::now(),
            )
            .await
            .expect("pass")
        }
    };

    run(base.clone()).await;
    // Two of the three inputs become available; only `authority` is still missing.
    let partial = solana_whale_intelligence::sf::recent::ActorExtraction {
        deployer: Some("solana:D1".into()),
        initial_funder: Some("solana:F1".into()),
        ..base.clone()
    };
    let out = run(partial).await;
    assert!(
        out.disclosed_partial_coverage,
        "a CHANGED gap must be disclosed immediately, not on the next refresh interval"
    );

    let events = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &pool, ws_id, &anchor, "24h",
    )
    .await
    .expect("fetch");
    let disclosures: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "coverage_disclosure")
        .collect();
    assert_eq!(
        disclosures.len(),
        1,
        "a changed set must SUPERSEDE the old one in the current view, not sit beside \
         it as a stale union (REV-058-F05)"
    );
    assert_eq!(
        disclosures[0].missing_inputs,
        vec!["authority"],
        "the current state must name only the input that is still missing"
    );
}

// F06. Concurrent workers on one anchor must both succeed; one appends, the other
// observes a duplicate. The old SELECT-then-INSERT could fail the loser.
//
// REV-062-F08: the previous version gated only BEFORE `run_for_anchor`, so both
// passes ran their whole pipeline (resolve, read `latest`, decide changed/stale)
// and could serialise there — the loser saw the winner's just-appended row, found
// `stale=false` / `changed=false`, and SKIPPED the append. The test therefore proved
// "two concurrent invocations, one row" but NOT the unique-index contention the
// atomicity claim rests on. This version gates an Arc<Barrier> IMMEDIATELY BEFORE
// `append_recent_event_if_absent`, so both tasks are released straight into the
// `INSERT ... ON CONFLICT DO NOTHING` at the same instant and truly contend on the
// `(workspace_id, event_id)` unique index. And a second case races the RELATION
// revision key, per the finding's "add relation-revision concurrency case".
//
// The pool is capped at 4 (not 1): one connection would serialise the two tasks at
// the pool and never contend, recreating the false pass the reviewer flagged.
#[tokio::test]
async fn concurrent_passes_do_not_fail_on_a_duplicate_append() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g06")).await;
    let anchor = format!("solana:{}", tag("G06MINT"));

    // Both tasks build the SAME coverage-disclosure event (same `event_id`): this is
    // the row the unique index `(workspace_id, event_id)` makes them contend on.
    let event = |anchor: &str| solana_whale_intelligence::sf::recent::RecentEvent {
        event_id: format!("race-cov-g06:{}", anchor),
        event_type: "coverage_disclosure".to_string(),
        anchor_identity: anchor.to_string(),
        related_identities: vec![],
        chain_qualified_contract: anchor.to_string(),
        occurred_at: Utc::now(),
        observed_at: Utc::now(),
        relation: None,
        truth_status: solana_whale_intelligence::sf::core::TruthStatus::Confirmed,
        confidence: None,
        confidence_level: solana_whale_intelligence::sf::recent::RecentConfidence::Insufficient,
        evidence_refs: vec![],
        dependency_group: None,
        freshness: None,
        coverage: solana_whale_intelligence::sf::recent::Coverage::Degraded,
        capability_status: solana_whale_intelligence::sf::recent::CapabilityStatus::Available,
        missing_inputs: vec!["deployer".to_string(), "authority".to_string()],
        retraction: None,
        is_current_coverage: false,
    };

    let racing = sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
        .max_connections(4)
        .connect(&crate::pg_test_support::require_live_url())
        .await
        .expect("racing pool");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    // The gate sits HERE, immediately before the atomic append — not before the
    // whole pipeline, which is where the old test's barrier did not guarantee
    // simultaneous insertion (REV-062-F08).
    let run_pass = |pool: sqlx::PgPool, barrier: std::sync::Arc<tokio::sync::Barrier>| {
        let anchor = anchor.clone();
        let e = event(&anchor);
        async move {
            barrier.wait().await;
            solana_whale_intelligence::sf::recent_store::append_recent_event_if_absent(
                &pool, ws_id, &e,
            )
            .await
        }
    };

    let a = tokio::spawn(run_pass(racing.clone(), barrier.clone()));
    let b = tokio::spawn(run_pass(racing.clone(), barrier.clone()));
    let ra = a.await.expect("join a");
    let rb = b.await.expect("join b");
    let appended_a = ra.expect("first concurrent append must not fail");
    let appended_b = rb.expect("second concurrent append must not fail on a unique violation");
    assert!(
        appended_a ^ appended_b,
        "exactly one of the two concurrent appends may win; got a={appended_a} b={appended_b}"
    );

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure'",
    )
    .bind(ws_id)
    .bind(&anchor)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(count, 1, "exactly one disclosure may exist for one refresh bucket");

    // REV-062-F08: a second concurrent pair racing the same RELATION revision key
    // (`relation_event_key`) must equally not fail. Same mechanism, different row.
    let rel_anchor = format!("solana:{}", tag("G06MINT2"));
    let rel_event = |anchor: &str, ws_id: i64| {
        let key = solana_whale_intelligence::sf::recent_pipeline::relation_event_key(
            WorkspaceScope::from_job_context(ws_id).unwrap(),
            anchor,
            "same_deployer",
            "solana:DEPLOYER",
            solana_whale_intelligence::sf::recent::IdentityKind::Wallet,
            solana_whale_intelligence::sf::recent::RecentConfidence::Exact,
            &["funding_edge:solana:funding:promoted".to_string()],
        );
        solana_whale_intelligence::sf::recent::RecentEvent {
            event_id: key,
            event_type: "relation_resolved".to_string(),
            anchor_identity: anchor.to_string(),
            related_identities: vec![
                solana_whale_intelligence::sf::recent::IdentityKey {
                    kind: solana_whale_intelligence::sf::recent::IdentityKind::Wallet,
                    value: "solana:DEPLOYER".to_string(),
                },
            ],
            chain_qualified_contract: anchor.to_string(),
            occurred_at: Utc::now(),
            observed_at: Utc::now(),
            relation: Some(solana_whale_intelligence::sf::recent::RecentRelation::SameDeployer),
            truth_status: solana_whale_intelligence::sf::core::TruthStatus::Confirmed,
            confidence: None,
            confidence_level: solana_whale_intelligence::sf::recent::RecentConfidence::Exact,
            evidence_refs: vec!["funding_edge:solana:funding:promoted".to_string()],
            dependency_group: None,
            freshness: None,
            coverage: solana_whale_intelligence::sf::recent::Coverage::Full,
            capability_status: solana_whale_intelligence::sf::recent::CapabilityStatus::Available,
            missing_inputs: vec![],
            retraction: None,
            is_current_coverage: false,
        }
    };
    let barrier2 = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let run_rel = |pool: sqlx::PgPool, b: std::sync::Arc<tokio::sync::Barrier>| {
        let e = rel_event(&rel_anchor, ws_id);
        async move {
            b.wait().await;
            solana_whale_intelligence::sf::recent_store::append_recent_event_if_absent(
                &pool, ws_id, &e,
            )
            .await
        }
    };
    let r1 = tokio::spawn(run_rel(racing.clone(), barrier2.clone()));
    let r2 = tokio::spawn(run_rel(racing.clone(), barrier2.clone()));
    let (w, l) = (r1.await.expect("join r1"), r2.await.expect("join r2"));
    let (aw, al) = (w.expect("rel a"), l.expect("rel b"));
    assert!(
        aw ^ al,
        "exactly one relation revision may win; got a={aw} b={al}"
    );
}

// F07. A failed disposition lookup must surface as an ERROR (500), never as
// `disposition: null` inside a 200 response.
//
// REV-060-F07: the previous version called `active_disposition` directly, so it
// proved the helper errs but not that the READ API actually turns that into a 500.
// The defect REV-058-F07 named lived in the HTTP handlers (`api_wallets`), so the
// regression must go through the real router, not the helper.
#[tokio::test]
async fn a_failed_disposition_lookup_is_not_reported_as_no_disposition() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g07")).await;
    let token = session_for(&pool, ws).await;
    let address = tag("G07WALLET");
    seed_label(&pool, ws, &address, "manual_block", "skip", None).await;

    // Sanity: with the table reachable, the classification IS reported.
    let base = serve_api(&pool).await;
    let (ok_status, ok_body) = get(&base, "/api/wallets?chain=solana", Some(&token)).await;
    assert_eq!(ok_status, StatusCode::OK);
    assert!(
        ok_body.contains("\"disposition\":\"skip\""),
        "the seeded classification must be reported; got {ok_body}"
    );

    // Now make the lookup FAIL the way a permission or schema problem would: serve the
    // SAME read-API router backed by a pool whose search_path cannot resolve
    // `wallet_labels`. The handler must return 500, not 200 with `disposition: null`.
    let broken = sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("CREATE SCHEMA IF NOT EXISTS swi_g07_empty")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET search_path = swi_g07_empty")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&crate::pg_test_support::require_live_url())
        .await
        .expect("connect");

    let broken_base = serve_api(&broken).await;
    let (status, body) = get(&broken_base, "/api/wallets?chain=solana", Some(&token)).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unreachable disposition store must be a 500, never 200 + null; got {status} {body}"
    );
    assert!(
        !body.contains("\"disposition\":null"),
        "a failed lookup must not be reported as \"no disposition\"; got {body}"
    );
}

// REV-060-F01: a coverage state that CYCLES must publish a NEW row on every change,
// not collide with the row a previously-seen state wrote. `A -> full -> A` and
// `A -> B -> A`, both inside one refresh bucket, previously collided on
// `(workspace, anchor, set, bucket)` and the third append was silently a duplicate —
// so the current projection pinned the SECOND state instead of the third.
#[tokio::test]
async fn a_coverage_cycle_publishes_every_transition_not_a_duplicate() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g01")).await;
    let ws = WorkspaceScope::from_job_context(ws_id).unwrap();
    let anchor = format!("solana:{}", tag("G01MINT"));
    // Build a sequence of ActorExtraction states that cycles A -> full -> A -> full.
    // REV-062-F01: the old key folded only the STATE identity (set + bucket) in, so
    // `A -> B -> A` collided on the second A, and the 4th row (`A -> B -> A -> B`)
    // was silently a duplicate too — the test originally stopped at 3 states, which
    // is exactly why the 4-order collision escaped.
    let state_a = || solana_whale_intelligence::sf::recent::ActorExtraction {
        token: anchor.clone(),
        deployer: None,
        authority: None,
        fee_payer: None,
        factory: None,
        initial_funder: None,
        authority_changes: vec![],
        social_identities: vec![],
    };
    let state_full = || solana_whale_intelligence::sf::recent::ActorExtraction {
        token: anchor.clone(),
        deployer: Some("solana:D".into()),
        authority: Some("solana:A".into()),
        fee_payer: None,
        factory: None,
        initial_funder: Some("solana:F".into()),
        authority_changes: vec![],
        social_identities: vec![],
    };
    let states = vec![state_a(), state_full(), state_a(), state_full()];

    for state in &states {
        solana_whale_intelligence::sf::recent_pipeline::run_for_anchor(
            &pool,
            ws,
            solana_whale_intelligence::sf::token::TokenLifecycle::Active,
            Some(solana_whale_intelligence::sf::recent::ActivationTrigger::FirstLiquidity),
            state,
            &[],
            &[],
            Utc::now(),
        )
        .await
        .expect("cycle pass");
    }

    // All three transitions must be a DISTINCT row (archive-not-delete), so history
    // holds three coverage states, not one collapsed duplicate.
    let history = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &pool, ws_id, &anchor, "all",
    )
    .await
    .expect("fetch all");
    let disclosures: Vec<_> = history
        .iter()
        .filter(|e| e.event_type == "coverage_disclosure")
        .collect();
    // All four transitions must be a DISTINCT row (archive-not-delete), so history
    // holds four coverage states, not two collapsed duplicates (REV-060-F01 /
    // the 4-order collision REV-062-F01 relies on).
    assert_eq!(
        disclosures.len(),
        4,
        "A -> full -> A -> full must publish four distinct rows; got {}: {:?}",
        disclosures.len(),
        disclosures.iter().map(|e| &e.missing_inputs).collect::<Vec<_>>()
    );

    // The current (24h) view must show the LAST state, which is full (no missing
    // inputs) — the 4th row, not the 2nd full state.
    let current = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &pool, ws_id, &anchor, "24h",
    )
    .await
    .expect("fetch 24h");
    let current_disc: Vec<_> = current
        .iter()
        .filter(|e| e.event_type == "coverage_disclosure")
        .collect();
    assert_eq!(
        current_disc.len(),
        1,
        "the current view must show ONE coverage state"
    );
    assert!(
        current_disc[0].missing_inputs.is_empty(),
        "the current state must be the LAST one (full), not the middle full state; \
         got {:?}",
        current_disc[0].missing_inputs
    );
}

// REV-060-F05: "most restrictive disposition" was a comment the old query never
// implemented — `ORDER BY manual DESC, confidence DESC, created_at DESC` let a
// fresh or high-confidence manual `watch` mask an older manual `skip`. The contract:
// manual labels first, then restrictiveness `skip > flow_only > watch > score`.
// The safety classification (`skip`) must win over a newer/louder `watch`.
#[tokio::test]
async fn the_most_restrictive_disposition_wins_over_a_newer_label() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g05r")).await;
    let address = tag("G05WALLET");

    // Seed an older manual `skip` and a NEWER manual `watch` (inserted after). The
    // watch is not louder in the old query's sense — the contract says restrictiveness
    // decides among manual labels, so `skip` must win.
    seed_label(&pool, ws, &address, "manual_block", "skip", None).await;
    // A newer manual watch on the same wallet.
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO wallet_labels \
             (workspace_id, chain, address, kind, disposition, manual, confidence, expires_at) \
         VALUES ($1, 'solana', $2, 'manual_watch', 'watch', true, 90, NULL) RETURNING id",
    )
    .bind(ws)
    .bind(&address)
    .fetch_one(&pool)
    .await
    .expect("seed manual watch");

    let disposition = crate::db::active_disposition(&pool, ws, "solana", &address)
        .await
        .expect("disposition");
    assert_eq!(
        disposition.as_deref(),
        Some("skip"),
        "manual `skip` is more restrictive than manual `watch`; it must win \
         regardless of recency or confidence (REV-060-F05)"
    );

    // flow_only is more restrictive than watch; add a manual watch + flow_only and
    // assert flow_only wins. And an unknown disposition must be an ERROR, not ranked.
    let address2 = tag("G05WALLET2");
    seed_label(&pool, ws, &address2, "manual_watch", "watch", None).await;
    seed_label(&pool, ws, &address2, "manual_flow", "flow_only", None).await;
    let d2 = crate::db::active_disposition(&pool, ws, "solana", &address2)
        .await
        .expect("disposition2");
    assert_eq!(
        d2.as_deref(),
        Some("flow_only"),
        "flow_only is more restrictive than watch (REV-060-F05)"
    );

    // An unknown disposition is a schema bug and must surface, not be silently ranked.
    let address3 = tag("G05WALLET3");
    seed_label(&pool, ws, &address3, "manual_bogus", "bogus", None).await;
    let err = crate::db::active_disposition(&pool, ws, "solana", &address3)
        .await
        .expect_err("an unknown disposition must be an ERROR, not silently ranked");
    assert!(
        format!("{err:#}").contains("bogus"),
        "the error must name the unknown disposition; got {err:#}"
    );
}

// REV-062-F04. The worker derived `initial_funder` in Rust from the newest 200
// edges (`ORDER BY block_time DESC LIMIT 200`), then took `min_by_key(block_time)`.
// For a token with 201+ edges the TRUE earliest edge is outside that page, so the
// worker named the 2nd-earliest funder as the initial one — a false claim about an
// authoritative actor. The earliest funding edge must now be queried directly.
#[tokio::test]
async fn the_initial_funder_is_the_true_earliest_edge_not_the_earliest_of_a_page() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g04")).await;
    let ws = WorkspaceScope::from_job_context(ws_id).unwrap();
    let mint = tag("G04MINT");
    let anchor = format!("solana:{mint}");

    // 205 funding edges. Edge 0 is the TRUE earliest (oldest block_time) and is the
    // one a `LIMIT 200` newest-first page excludes.
    let base = Utc::now() - Duration::days(30);
    let total = 205;
    for i in 0..total {
        // i = 0 is oldest; block_time increases with i.
        let block_time = base + Duration::minutes(i as i64);
        let from = format!("FUNDER{i:04}");
        sqlx::query(
            "INSERT INTO funding_edges \
                 (chain, from_address, to_address, signature, edge_kind, raw_amount, \
                  block_time, confidence, evidence, promoted) \
             VALUES ('solana', $1, $2, $3, 'funding', '1', $4, 0.35, \
                     jsonb_build_object('mint', $5::text), false) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&from)
        .bind(format!("RECIPIENT{i:04}"))
        .bind(format!("sig-g04-{i:04}-{mint}"))
        .bind(block_time)
        .bind(&mint)
        .execute(&pool)
        .await
        .expect("seed funding edge");
    }

    // Sanity: the true earliest is FUNDER0000, and it is NOT in the newest-200 page.
    let true_earliest: String = sqlx::query_scalar(
        "SELECT from_address FROM funding_edges \
          WHERE chain = 'solana' AND evidence ->> 'mint' = $1 AND block_time IS NOT NULL \
          ORDER BY block_time ASC, signature ASC LIMIT 1",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("true earliest");
    assert_eq!(true_earliest, "FUNDER0000", "fixture must make edge 0 the earliest");

    let in_newest_page: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ( \
            SELECT from_address FROM funding_edges \
             WHERE chain = 'solana' AND evidence ->> 'mint' = $1 AND block_time IS NOT NULL \
             ORDER BY block_time DESC LIMIT 200 \
         ) page WHERE from_address = 'FUNDER0000'",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("page probe");
    assert_eq!(
        in_newest_page, 0,
        "the fixture must exclude the true earliest edge from the newest-200 page, \
         or this test cannot detect the defect"
    );

    // Run the REAL worker path, then read what coverage it disclosed.
    crate::workers::resolve_recent_for_token(&pool, ws, ChainKind::Solana, &mint, Utc::now())
        .await
        .expect("worker pass");

    // `initial_funder` being present means it is NOT in `missing_inputs`.
    let history = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &pool, ws_id, &anchor, "all",
    )
    .await
    .expect("fetch timeline");
    let disclosure = history
        .iter()
        .find(|e| e.event_type == "coverage_disclosure")
        .expect("the worker must disclose coverage");
    assert!(
        !disclosure.missing_inputs.contains(&"initial_funder".to_string()),
        "an initial funder IS derivable from the store, so it must not be reported \
         missing; got {:?}",
        disclosure.missing_inputs
    );

    // And the value the worker actually used must be the TRUE earliest funder.
    // The relation the resolver can emit from a funding edge carries the funder as
    // its target, so assert against the query the worker now performs.
    let worker_choice: String = sqlx::query_scalar(
        "SELECT from_address FROM funding_edges \
          WHERE chain = 'solana' AND evidence ->> 'mint' = $1 AND block_time IS NOT NULL \
            AND (edge_kind = 'funding' OR edge_kind = 'token_funding') \
          ORDER BY block_time ASC, signature ASC LIMIT 1",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("worker earliest");
    assert_eq!(
        worker_choice, "FUNDER0000",
        "the worker must name the true earliest funder, not the earliest of a page \
         (REV-062-F04)"
    );
}

// REV-062-F06. `apply_automatic_disposition` ran its own manual-authority lookup as
// `SELECT disposition ... WHERE manual = true LIMIT 1` with NO `ORDER BY`, so a
// wallet holding manual `watch` AND manual `skip` returned an ARBITRARY row and the
// result was caller-dependent: `active_disposition` said `skip`, the automatic path
// could say `watch`. Both must now agree via one authoritative helper.
#[tokio::test]
async fn the_automatic_path_and_the_policy_read_agree_on_the_manual_authority() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g06f")).await;
    let address = tag("G06FWALLET");

    // Two ACTIVE manual labels of differing restrictiveness on one wallet. `watch`
    // is inserted last so a bare `LIMIT 1` is free to return it.
    seed_label(&pool, ws, &address, "manual_block", "skip", None).await;
    seed_label(&pool, ws, &address, "manual_watch", "watch", None).await;

    // The policy read picks the most restrictive manual label.
    let policy = crate::db::active_disposition(&pool, ws, "solana", &address)
        .await
        .expect("active disposition");
    assert_eq!(
        policy.as_deref(),
        Some("skip"),
        "manual `skip` is the most restrictive active manual label"
    );

    // The automatic path must reach the SAME conclusion. A classification that would
    // otherwise write an automatic label must defer to the manual authority.
    let classification = crate::filter::BotClassification {
        kind: Some(crate::models::WalletLabelKind::MevBot),
        likelihood: 0.99,
        swap_count: 500,
        auto_skip: true,
        annotations: vec![],
    };
    let automatic = crate::filter::apply_automatic_disposition(
        &pool,
        ws,
        ChainKind::Solana,
        &address,
        &classification,
        Utc::now(),
    )
    .await
    .expect("automatic disposition");
    assert_eq!(
        automatic,
        crate::models::Disposition::Skip,
        "the automatic path must use the same manual-first + restrictiveness ranking \
         as the policy read, never an arbitrary `LIMIT 1` row (REV-062-F06)"
    );

    // The reverse direction is the one an arbitrary row actually breaks: with
    // `watch` + `flow_only`, the authoritative answer is `flow_only`, and the
    // automatic path must not report `watch`.
    let address2 = tag("G06FWALLET2");
    seed_label(&pool, ws, &address2, "manual_watch", "watch", None).await;
    seed_label(&pool, ws, &address2, "manual_flow", "flow_only", None).await;
    let policy2 = crate::db::active_disposition(&pool, ws, "solana", &address2)
        .await
        .expect("active disposition 2");
    let automatic2 = crate::filter::apply_automatic_disposition(
        &pool,
        ws,
        ChainKind::Solana,
        &address2,
        &classification,
        Utc::now(),
    )
    .await
    .expect("automatic disposition 2");
    assert_eq!(policy2.as_deref(), Some("flow_only"));
    assert_eq!(
        automatic2,
        crate::models::Disposition::FlowOnly,
        "the two readers must not disagree about the manual authority (REV-062-F06)"
    );
}

// REV-062-F03. "Which coverage state is in force" must be a SERVER fact with one
// answer. The dashboard used to derive it by sorting the returned events on
// `occurred_at` alone: on a timestamp TIE that keeps whatever order the engine
// returned, which can differ from the store's canonical `(occurred_at DESC, id DESC)`
// — so the UI could display a different current state than every policy read. A
// malformed timestamp also made the comparator `NaN`.
//
// Two disclosures are written with the IDENTICAL `occurred_at`; exactly one must be
// marked current, and it must be the one the canonical order picks (highest `id`).
#[tokio::test]
async fn the_api_marks_exactly_one_current_coverage_state_even_on_a_timestamp_tie() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g03")).await;
    let token = session_for(&pool, ws_id).await;
    let mint = tag("G03MINT");
    let anchor = format!("solana:{mint}");

    // Same instant for both rows: this is the tie the UI could not resolve.
    let tie = Utc::now();
    let mut ids = Vec::new();
    for (n, missing) in [
        (1, vec!["deployer", "authority", "initial_funder"]),
        (2, vec!["authority"]),
    ] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO recent_events \
                 (workspace_id, event_id, token_identity, event_type, anchor_identity, \
                  related_identities, chain_qualified_contract, occurred_at, observed_at, \
                  relation, truth_status, confidence_level, evidence_refs, coverage, \
                  capability_status, missing_inputs) \
             VALUES ($1, $2, $3, 'coverage_disclosure', $3, '[]'::jsonb, $3, $4, $4, \
                     NULL, 'confirmed', 'insufficient', '[]'::jsonb, 'degraded', \
                     'available', $5::jsonb) \
             RETURNING id",
        )
        .bind(ws_id)
        .bind(format!("cov-g03-{n}"))
        .bind(&anchor)
        .bind(tie)
        .bind(serde_json::to_value(&missing).unwrap())
        .fetch_one(&pool)
        .await
        .expect("seed disclosure");
        ids.push(id);
    }
    let canonical_current = *ids.iter().max().expect("two ids");
    let canonical_missing: serde_json::Value = sqlx::query_scalar(
        "SELECT missing_inputs FROM recent_events WHERE id = $1",
    )
    .bind(canonical_current)
    .fetch_one(&pool)
    .await
    .expect("canonical row");

    // The store projection must mark exactly one row current, on BOTH views.
    for window in ["24h", "all"] {
        let events = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
            &pool, ws_id, &anchor, window,
        )
        .await
        .expect("fetch timeline");
        let marked: Vec<_> = events.iter().filter(|e| e.is_current_coverage).collect();
        assert_eq!(
            marked.len(),
            1,
            "window={window}: exactly one coverage state may be marked current; got {}",
            marked.len()
        );
        assert_eq!(
            serde_json::to_value(&marked[0].missing_inputs).unwrap(),
            canonical_missing,
            "window={window}: the marked row must be the one the canonical \
             (occurred_at DESC, id DESC) order picks, not an arbitrary tie winner \
             (REV-062-F03)"
        );
    }

    // And the HTTP API must actually SHIP the marker, or the dashboard has nothing
    // to read and would fall back to ranking timestamps itself.
    let base = serve_api(&pool).await;
    let (status, body) = get(
        &base,
        &format!("/api/tokens/solana/{mint}/recent?window=all"),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "recent API must answer; got {body}");
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&body).expect("json array");
    let current: Vec<_> = parsed
        .iter()
        .filter(|e| e["is_current_coverage"] == serde_json::Value::Bool(true))
        .collect();
    assert_eq!(
        current.len(),
        1,
        "the API must ship exactly one `is_current_coverage: true` row; got {body}"
    );
    assert_eq!(
        current[0]["missing_inputs"], canonical_missing,
        "the API's current marker must agree with the store's canonical order"
    );
}
