//! Live PostgreSQL regressions for REV-072 F06 (two HIGH residuals).
//!
//! Both reproduce the reviewer's own probe rather than restating a fix:
//!
//! * one wallet active in two clusters must not satisfy the "two INDEPENDENT
//!   eligible clusters" entry gate by itself;
//! * signal output must carry its owning workspace, and every reader — HTTP read
//!   API, admin panel, and the alert dispatcher's selection — must filter on it.
//!
//! No skip guard (REV-056-F06): with `pg_tests` enabled a missing database is a
//! configuration error, not a reason to report success.

#![cfg(all(test, feature = "pg_tests"))]

use chrono::{Duration, Utc};
use reqwest::StatusCode;
use sqlx::PgPool;

use crate::admin::{router as admin_router, AdminState};
use crate::api::{router as api_router, ApiState};
use crate::config::Settings;
use crate::models::ChainKind;

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

async fn serve(pool: &PgPool, admin: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    if admin {
        let state = AdminState {
            pool: pool.clone(),
            settings: std::sync::Arc::new(Settings {
                config: crate::config::AppConfig::default(),
                env: crate::config::EnvConfig::load(),
            }),
        };
        tokio::spawn(async move {
            let _ = axum::serve(listener, admin_router(state)).await;
        });
    } else {
        let state = ApiState { pool: pool.clone() };
        tokio::spawn(async move {
            let _ = axum::serve(listener, api_router(state)).await;
        });
    }
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

async fn seed_label_with(
    pool: &PgPool,
    workspace_id: i64,
    address: &str,
    kind: &str,
    disposition: &str,
    manual: bool,
) {
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(address)
    .execute(pool)
    .await
    .expect("seed wallet");
    sqlx::query(
        "INSERT INTO wallet_labels \
             (workspace_id, chain, address, kind, disposition, manual, confidence) \
         VALUES ($1, 'solana', $2, $3, $4, $5, 90)",
    )
    .bind(workspace_id)
    .bind(address)
    .bind(kind)
    .bind(disposition)
    .bind(manual)
    .execute(pool)
    .await
    .expect("seed label");
}

/// A token whose lifecycle, age, liquidity, and market freshness all pass, so a
/// rejection can only come from the wallet/cluster gates under test.
async fn seed_tradable_token(pool: &PgPool, mint: &str, now: chrono::DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO tokens (chain, mint, lifecycle_state, first_seen_at, risk_flags) \
         VALUES ('solana', $1, 'new_creation', $2, '[]'::jsonb) ON CONFLICT DO NOTHING",
    )
    .bind(mint)
    .bind(now - Duration::hours(1))
    .execute(pool)
    .await
    .expect("seed token");
    sqlx::query(
        "INSERT INTO market_snapshots \
             (source, chain, mint, observed_at, pair_address, liquidity_usd) \
         VALUES ('test', 'solana', $1, $2, '', 30000) ON CONFLICT DO NOTHING",
    )
    .bind(mint)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed market");
}

async fn seed_buy(
    pool: &PgPool,
    wallet: &str,
    mint: &str,
    usd: &str,
    signature: &str,
    now: chrono::DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO trades \
             (chain, wallet, mint, side, signature, event_index, block_time, \
              observed_at, usd_value) \
         VALUES ('solana', $1, $2, 'buy', $3, 0, $4, $4, $5::numeric) \
         ON CONFLICT DO NOTHING",
    )
    .bind(wallet)
    .bind(mint)
    .bind(signature)
    .bind(now)
    .bind(usd)
    .execute(pool)
    .await
    .expect("seed trade");
}

async fn seed_wallet_score(
    pool: &PgPool,
    address: &str,
    skill: i32,
    copyability: i32,
    now: chrono::DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO wallet_scores \
             (chain, address, as_of, skill_score, copyability_score, conviction, \
              history_completeness, provisional) \
         VALUES ('solana', $1, $2, $3, $4, 90, 1, false) ON CONFLICT DO NOTHING",
    )
    .bind(address)
    .bind(now)
    .bind(skill)
    .bind(copyability)
    .execute(pool)
    .await
    .expect("seed score");
}

/// A fresh cluster containing exactly one wallet.
async fn seed_cluster_with(pool: &PgPool, address: &str) -> i64 {
    let cluster_id: i64 = sqlx::query_scalar(
        "INSERT INTO swi_legacy.wallet_clusters (created_at) VALUES (now()) \
         RETURNING cluster_id",
    )
    .fetch_one(pool)
    .await
    .expect("seed cluster");
    sqlx::query(
        "INSERT INTO wallet_cluster_members (cluster_id, chain, address) \
         VALUES ($1, 'solana', $2) ON CONFLICT DO NOTHING",
    )
    .bind(cluster_id)
    .bind(address)
    .execute(pool)
    .await
    .expect("seed cluster member");
    cluster_id
}

// ---------------------------------------------------------------------------
// F06 residual 1 (HIGH) — one wallet may not impersonate two clusters
// ---------------------------------------------------------------------------

// The reviewer's probe, through the REAL `evaluate_token_signals`.
//
//   one eligible (`score`) wallet, skill/copyability 99/99
//   that ONE wallet active in TWO clusters
//   two meaningful buys by that one wallet
//   result before the fix: accepted, eligible_clusters=2, signal row created
//
// The gate is documented as "two INDEPENDENT eligible clusters". Counting
// `COUNT(DISTINCT cluster_id)` over raw memberships let one wallet satisfy it alone,
// because the schema permitted several active memberships per (chain, address) and
// `rebuild_cluster_for` never merged the overlaps.
#[tokio::test]
async fn one_wallet_in_two_clusters_is_one_cluster_for_the_entry_gate() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g72f06a")).await;
    let stem = tag("G72F06A");
    let mint = format!("{stem}MINT");
    let config = crate::config::SignalsConfig::default();
    let now = Utc::now();

    seed_tradable_token(&pool, &mint, now).await;

    let wallet = format!("{stem}SOLO");
    seed_label_with(&pool, ws, &wallet, "manual_policy", "score", true).await;
    seed_wallet_score(&pool, &wallet, 99, 99, now).await;
    // Two meaningful buys, so the buy gate cannot be the thing that rejects.
    seed_buy(&pool, &wallet, &mint, "100", &format!("sig-{stem}-1"), now).await;
    seed_buy(
        &pool,
        &wallet,
        &mint,
        "100",
        &format!("sig-{stem}-2"),
        now - Duration::minutes(1),
    )
    .await;

    // The defect: the SAME wallet in two clusters. Written with an explicit INSERT
    // rather than through `rebuild_cluster_for`, because a pre-upgrade database is
    // exactly the state this must survive — and because migration 1032's index now
    // forbids it, the second membership is created and the constraint violation is
    // asserted, which is itself the proof the invariant is enforced.
    seed_cluster_with(&pool, &wallet).await;
    let second_cluster: i64 = sqlx::query_scalar(
        "INSERT INTO swi_legacy.wallet_clusters (created_at) VALUES (now()) \
         RETURNING cluster_id",
    )
    .fetch_one(&pool)
    .await
    .expect("second cluster");
    let second_membership = sqlx::query(
        "INSERT INTO wallet_cluster_members (cluster_id, chain, address) \
         VALUES ($1, 'solana', $2)",
    )
    .bind(second_cluster)
    .bind(&wallet)
    .execute(&pool)
    .await;
    assert!(
        second_membership.is_err(),
        "migration 1032 must forbid a second ACTIVE membership for one (chain, address); \
         without the index the entry gate can be satisfied by one wallet"
    );

    // Even so, the COUNT must not depend on the index alone: a database upgraded
    // later, or a writer that revokes and re-adds out of order, must still be
    // counted per wallet. Force the duplicate past the index by making the second
    // membership legitimate the only way the schema allows — a revoked row — and
    // assert the count ignores it.
    sqlx::query(
        "INSERT INTO wallet_cluster_members (cluster_id, chain, address, revoked_at) \
         VALUES ($1, 'solana', $2, now())",
    )
    .bind(second_cluster)
    .bind(&wallet)
    .execute(&pool)
    .await
    .expect("revoked membership is permitted");

    let signal = crate::workers::evaluate_token_signals(
        &pool,
        ws,
        ChainKind::Solana,
        &mint,
        now,
        &config,
    )
    .await
    .expect("evaluation");
    assert!(
        signal.is_none(),
        "one wallet must not satisfy a two-INDEPENDENT-cluster gate by itself \
         (REV-072-F06)"
    );

    let rejection: Option<String> = sqlx::query_scalar(
        "SELECT rejection_code FROM signal_evaluations \
          WHERE workspace_id = $1 AND chain = 'solana' AND mint = $2 \
          ORDER BY id DESC LIMIT 1",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("rejection");
    assert_eq!(
        rejection.as_deref(),
        Some("insufficient_clusters"),
        "the first gate must be the one that refuses, proving the cluster count was 1"
    );

    let signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count signals");
    assert_eq!(signals, 0, "no signal row may be created");

    // The refusal must be about INDEPENDENCE, not about the data being too thin: a
    // SECOND eligible wallet in its own cluster passes the same gate.
    let second_wallet = format!("{stem}PEER");
    seed_label_with(&pool, ws, &second_wallet, "manual_policy", "score", true).await;
    seed_wallet_score(&pool, &second_wallet, 99, 99, now).await;
    seed_buy(
        &pool,
        &second_wallet,
        &mint,
        "100",
        &format!("sig-{stem}-3"),
        now,
    )
    .await;
    seed_cluster_with(&pool, &second_wallet).await;

    let accepted = crate::workers::evaluate_token_signals(
        &pool,
        ws,
        ChainKind::Solana,
        &mint,
        now + Duration::seconds(1),
        &config,
    )
    .await
    .expect("second evaluation");
    assert!(
        accepted.is_some(),
        "two wallets in two clusters must still pass; otherwise the fix is hiding \
         data rather than counting independence"
    );
}

// `rebuild_cluster_for` is how overlapping memberships appeared. Converging
// components must MERGE, not accumulate: the reviewer's minimum fix asks for
// canonicalization, and a merge that only re-points the wallet under rebuild would
// leave the other cluster's members stranded in a revoked cluster.
#[tokio::test]
async fn a_converging_component_merges_into_one_cluster() {
    let pool = pool().await;
    let stem = tag("G72F06M");
    let (a, b, c) = (
        format!("{stem}A"),
        format!("{stem}B"),
        format!("{stem}C"),
    );
    let now = Utc::now();

    let edge = |from: &str, to: &str, sig: String| {
        let from = from.to_string();
        let to = to.to_string();
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO funding_edges \
                     (chain, from_address, to_address, signature, edge_kind, raw_amount, \
                      block_time, confidence, evidence, promoted) \
                 VALUES ('solana', $1, $2, $3, 'funding', '1', now(), 0.9, '{}'::jsonb, true) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(from)
            .bind(to)
            .bind(sig)
            .execute(&pool)
            .await
            .expect("seed edge");
        }
    };

    // Two separate components first: A-B and C alone in its own cluster.
    edge(&a, &b, format!("{stem}sig1")).await;
    let cluster_ab = crate::graph::rebuild_cluster_for(&pool, ChainKind::Solana, &a, now)
        .await
        .expect("rebuild ab")
        .expect("a cluster");
    let cluster_c = seed_cluster_with(&pool, &c).await;
    assert_ne!(cluster_ab, cluster_c, "the fixture must start with two clusters");

    // Now they converge: B funds C, so A-B-C is one component.
    edge(&b, &c, format!("{stem}sig2")).await;
    let merged = crate::graph::rebuild_cluster_for(&pool, ChainKind::Solana, &b, now)
        .await
        .expect("rebuild after convergence")
        .expect("a cluster");

    // Every member is active in exactly ONE cluster, and it is the same one.
    for member in [&a, &b, &c] {
        let active: Vec<i64> = sqlx::query_scalar(
            "SELECT cluster_id FROM wallet_cluster_members \
              WHERE chain = 'solana' AND address = $1 AND revoked_at IS NULL",
        )
        .bind(member)
        .fetch_all(&pool)
        .await
        .expect("active memberships");
        assert_eq!(
            active,
            vec![merged],
            "{member} must hold exactly one active membership, in the merged cluster"
        );
    }

    // The superseded membership is REVOKED, not deleted: this table is history.
    let revoked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_cluster_members \
          WHERE chain = 'solana' AND address = $1 AND revoked_at IS NOT NULL",
    )
    .bind(&c)
    .fetch_one(&pool)
    .await
    .expect("count revoked");
    assert_eq!(revoked, 1, "the superseded membership must be revoked, not erased");
}

// ---------------------------------------------------------------------------
// F06 residual 2 (HIGH) — signal output must be workspace-owned
// ---------------------------------------------------------------------------

/// Seed the fixture that ACCEPTS in one workspace, in `ws`.
async fn seed_accepting_fixture(pool: &PgPool, ws: i64, stem: &str, mint: &str, now: chrono::DateTime<Utc>) {
    seed_tradable_token(pool, mint, now).await;
    for i in 0..2 {
        let wallet = format!("{stem}W{i}");
        seed_label_with(pool, ws, &wallet, "manual_policy", "score", true).await;
        seed_wallet_score(pool, &wallet, 99, 99, now).await;
        seed_buy(pool, &wallet, mint, "100", &format!("sig-{stem}-{i}"), now).await;
        seed_cluster_with(pool, &wallet).await;
    }
}

// The reviewer's probe: the SAME facts are accepted under workspace A's policy and
// rejected under workspace B's, and the accepted row must belong to A alone. Before
// the fix `signals`/`signal_evaluations` had no `workspace_id` at all, so A's
// policy outcome was a global row and no reader could filter ownership that was
// never stored.
#[tokio::test]
async fn a_signal_is_owned_by_the_workspace_whose_policy_accepted_it() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("g72f06wa")).await;
    let ws_b = workspace(&pool, &tag("g72f06wb")).await;
    let stem = tag("G72F06W");
    let mint = format!("{stem}MINT");
    let config = crate::config::SignalsConfig::default();
    let now = Utc::now();

    // A's policy: both wallets contribute alpha.
    seed_accepting_fixture(&pool, ws_a, &stem, &mint, now).await;
    // B's policy over the SAME wallets: excluded.
    for i in 0..2 {
        seed_label_with(&pool, ws_b, &format!("{stem}W{i}"), "manual_block", "skip", true).await;
    }

    let accepted = crate::workers::evaluate_token_signals(
        &pool, ws_a, ChainKind::Solana, &mint, now, &config,
    )
    .await
    .expect("workspace A evaluation")
    .expect("workspace A must accept");

    let rejected = crate::workers::evaluate_token_signals(
        &pool, ws_b, ChainKind::Solana, &mint, now, &config,
    )
    .await
    .expect("workspace B evaluation");
    assert!(
        rejected.is_none(),
        "workspace B's policy excludes both wallets, so it must reject the same facts"
    );

    // The accepted row belongs to A.
    let owner: i64 = sqlx::query_scalar("SELECT workspace_id FROM signals WHERE id = $1")
        .bind(accepted)
        .fetch_one(&pool)
        .await
        .expect("signal owner");
    assert_eq!(owner, ws_a, "the signal must be owned by the accepting workspace");

    // And each workspace sees only its own evaluations for this mint.
    let a_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signal_evaluations WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws_a)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count a");
    let b_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signal_evaluations WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws_b)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count b");
    assert_eq!(a_rows, 1, "workspace A wrote exactly its own acceptance");
    assert_eq!(b_rows, 1, "workspace B wrote exactly its own rejection");
    let b_signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws_b)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count b signals");
    assert_eq!(b_signals, 0, "a rejecting workspace owns no signal");
}

// Ownership that is stored but not filtered is not ownership. Both HTTP surfaces,
// over the real routers with real sessions.
#[tokio::test]
async fn neither_http_surface_shows_another_workspaces_signal() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("g72f06ha")).await;
    let ws_b = workspace(&pool, &tag("g72f06hb")).await;
    let token_a = session_for(&pool, ws_a).await;
    let token_b = session_for(&pool, ws_b).await;
    let stem = tag("G72F06H");
    let mint = format!("{stem}MINT");
    let now = Utc::now();

    seed_accepting_fixture(&pool, ws_a, &stem, &mint, now).await;
    let signal_id = crate::workers::evaluate_token_signals(
        &pool,
        ws_a,
        ChainKind::Solana,
        &mint,
        now,
        &crate::config::SignalsConfig::default(),
    )
    .await
    .expect("evaluation")
    .expect("workspace A must accept");

    for admin in [false, true] {
        let base = serve(&pool, admin).await;
        let surface = if admin { "admin" } else { "read API" };

        // No session at all must not read a tenant's signal.
        let (anon, anon_body) = get(&base, "/api/signals?limit=500", None).await;
        assert!(
            anon.is_client_error() || !anon_body.contains(&mint),
            "the {surface} must not serve a signal without a session; got {anon} {anon_body}"
        );

        let (b_status, b_body) = get(&base, "/api/signals?limit=500", Some(&token_b)).await;
        assert_eq!(b_status, StatusCode::OK, "workspace B session is valid on the {surface}");
        assert!(
            !b_body.contains(&mint),
            "workspace B must not see workspace A's signal on the {surface}; got {b_body}"
        );

        let (a_status, a_body) = get(&base, "/api/signals?limit=500", Some(&token_a)).await;
        assert_eq!(a_status, StatusCode::OK);
        assert!(
            a_body.contains(&mint),
            "the owning workspace must still see its own signal on the {surface}; got {a_body}"
        );

        // Rejections are policy answers too.
        let (b_rej_status, b_rej) =
            get(&base, "/api/signals/rejections?limit=500", Some(&token_b)).await;
        assert_eq!(b_rej_status, StatusCode::OK);
        assert!(
            !b_rej.contains(&mint),
            "workspace B must not read workspace A's rejection history on the {surface}"
        );
    }

    // The alert dispatcher selects from the same table. It must not pick up another
    // tenant's signal for delivery to this tenant's chat.
    let for_b = crate::workers::pending_signal_alerts(&pool, ws_b, "test-chat")
        .await
        .expect("pending for B");
    assert!(
        !for_b.iter().any(|(id, _, _, _, _, _)| *id == signal_id),
        "workspace B's dispatcher must not deliver workspace A's signal"
    );
    let for_a = crate::workers::pending_signal_alerts(&pool, ws_a, "test-chat")
        .await
        .expect("pending for A");
    assert!(
        for_a.iter().any(|(id, _, _, _, _, _)| *id == signal_id),
        "the owning workspace's dispatcher must still deliver its own signal; \
         otherwise the filter is dropping data rather than scoping it"
    );
}
