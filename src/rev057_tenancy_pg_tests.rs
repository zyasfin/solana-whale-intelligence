//! Live PostgreSQL tests for REV-056 F01–F06.
//!
//! These reproduce the reviewer's own probes rather than restating the fixes:
//!
//! * F01 — two sessions in different workspaces; B must not read OR revoke A's label.
//! * F02 — a disclosure aged past the default window must still reach the API, and a
//!   repeat pass must refresh it instead of refusing because an old row exists.
//! * F03 — a revoke replay must not report a second success or overwrite the original
//!   revocation timestamp.
//! * F05 — a failing address in a blocklist import must not leave a partial import or
//!   an inflated count.
//!
//! F04 (alias canonicalization) is covered by `tests/chain_alias_canonicalization.rs`
//! plus a live HTTP probe; F06 (fail-closed harness) is proven by running this lane
//! with the DB variables unset.
//!
//! No skip guard: with `pg_tests` enabled a missing database is a configuration error
//! (REV-056-F06), not a reason to report success.

#![cfg(all(test, feature = "pg_tests"))]

use chrono::{Duration, Utc};
use reqwest::StatusCode;
use sqlx::PgPool;

use crate::admin::{router, AdminState};
use crate::config::Settings;
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

/// Configure admin auth for this process.
///
/// Delegates to the shared helper: the hash is a PROCESS-global, so two modules each
/// running their own `Once` with their own password let whichever ran first decide,
/// and the loser's login probes then failed for an unrelated reason.
fn configure_admin_auth() {
    crate::pg_test_support::configure_admin_auth();
}

async fn state(pool: &PgPool) -> AdminState {
    configure_admin_auth();
    AdminState {
        pool: pool.clone(),
        settings: std::sync::Arc::new(Settings {
            config: crate::config::AppConfig::default(),
            env: crate::config::EnvConfig::load(),
        }),
    }
}

/// The real admin router on a loopback port.
async fn serve(state: AdminState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    format!("http://{addr}")
}

async fn request(
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
) -> (StatusCode, String) {
    let response = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .send()
        .await
        .expect("http request");
    let status = response.status();
    (status, response.text().await.expect("body"))
}

/// Seed a wallet plus one label owned by `workspace_id`; returns the label id.
async fn seed_label(pool: &PgPool, workspace_id: i64, address: &str, kind: &str) -> i64 {
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
             (workspace_id, chain, address, kind, disposition, manual, confidence) \
         VALUES ($1, 'solana', $2, $3, 'skip', true, 90) RETURNING id",
    )
    .bind(workspace_id)
    .bind(address)
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("seed label")
}

// F01 (HIGH). The reviewer's exact probe: workspace B read AND revoked workspace A's
// private label over real HTTP, and the DB confirmed the revocation.
#[tokio::test]
async fn workspace_b_can_neither_read_nor_revoke_workspace_a_labels() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("f01a")).await;
    let ws_b = workspace(&pool, &tag("f01b")).await;
    let token_a = session_for(&pool, ws_a).await;
    let token_b = session_for(&pool, ws_b).await;

    let address = tag("F01WALLET");
    let label_id = seed_label(&pool, ws_a, &address, "A-private").await;

    let base = serve(state(&pool).await).await;
    let list = format!("/api/wallets/solana/{address}/labels");

    let (status_a, body_a) = request(&base, reqwest::Method::GET, &list, &token_a).await;
    assert_eq!(status_a, StatusCode::OK);
    assert!(
        body_a.contains("A-private"),
        "workspace A must see its own label; got {body_a}"
    );

    let (status_b, body_b) = request(&base, reqwest::Method::GET, &list, &token_b).await;
    assert_eq!(status_b, StatusCode::OK);
    assert!(
        !body_b.contains("A-private"),
        "workspace B must NOT read workspace A's private label; got {body_b}"
    );

    // The destructive half: B must not be able to revoke what it cannot see.
    let revoke = format!("/api/wallets/solana/{address}/labels/{label_id}/revoke");
    let (revoke_status, revoke_body) =
        request(&base, reqwest::Method::POST, &revoke, &token_b).await;
    assert_eq!(
        revoke_status,
        StatusCode::NOT_FOUND,
        "workspace B must not address workspace A's label; got {revoke_body}"
    );

    let revoked: bool = sqlx::query_scalar(
        "SELECT revoked_at IS NOT NULL FROM wallet_labels WHERE id = $1",
    )
    .bind(label_id)
    .fetch_one(&pool)
    .await
    .expect("read label");
    assert!(
        !revoked,
        "workspace A's label must still be active after workspace B's attempt"
    );

    // And A can still revoke its own.
    let (own_status, _) = request(&base, reqwest::Method::POST, &revoke, &token_a).await;
    assert_eq!(own_status, StatusCode::OK, "the owner must be able to revoke");
}

// F03. A replay must not claim a second revocation nor rewrite the original timestamp,
// which is audit evidence.
#[tokio::test]
async fn a_revoke_replay_is_idempotent_and_preserves_the_original_timestamp() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("f03")).await;
    let token = session_for(&pool, ws).await;
    let address = tag("F03WALLET");
    let label_id = seed_label(&pool, ws, &address, "replay-probe").await;

    let base = serve(state(&pool).await).await;
    let revoke = format!("/api/wallets/solana/{address}/labels/{label_id}/revoke");

    let (first_status, first_body) = request(&base, reqwest::Method::POST, &revoke, &token).await;
    assert_eq!(first_status, StatusCode::OK);
    assert!(
        first_body.contains("\"revoked\":1"),
        "the first revoke must report one row; got {first_body}"
    );
    let first_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT revoked_at FROM wallet_labels WHERE id = $1")
            .bind(label_id)
            .fetch_one(&pool)
            .await
            .expect("read timestamp");

    let (second_status, second_body) = request(&base, reqwest::Method::POST, &revoke, &token).await;
    assert_eq!(second_status, StatusCode::OK);
    assert!(
        second_body.contains("\"revoked\":0") && second_body.contains("\"already_revoked\":true"),
        "a replay must report zero new revocations and say it was already revoked; \
         got {second_body}"
    );

    let second_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT revoked_at FROM wallet_labels WHERE id = $1")
            .bind(label_id)
            .fetch_one(&pool)
            .await
            .expect("read timestamp");
    assert_eq!(
        first_at, second_at,
        "the original revocation timestamp is audit evidence and must not be rewritten"
    );
}

// F02. A disclosure older than the default 24h window must still reach the API, and a
// repeat pass must REFRESH it rather than refuse because an old row exists.
#[tokio::test]
async fn an_aged_coverage_disclosure_is_still_current_and_gets_refreshed() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("f02")).await;
    let ws = WorkspaceScope::from_job_context(ws_id).unwrap();
    let mint = tag("F02MINT");
    let anchor = format!("solana:{mint}");

    // A first pass discloses the gap.
    crate::workers::resolve_recent_for_token(&pool, ws, ChainKind::Solana, &mint, Utc::now())
        .await
        .expect("first pass");

    // Age it past the default window, the way real elapsed time would.
    //
    // `recent_events` rejects UPDATE (migration 1019 append-only trigger), so the row
    // is aged by inserting the same disclosure with an older `occurred_at` and deleting
    // nothing: the projection must pick the NEWEST, so the assertion below is made
    // after re-seeding a single aged row in a fresh anchor.
    let aged_anchor = format!("solana:{}", tag("F02AGED"));
    sqlx::query(
        "INSERT INTO recent_events \
             (workspace_id, event_id, token_identity, event_type, anchor_identity, \
              chain_qualified_contract, occurred_at, observed_at, truth_status, \
              confidence_level, coverage, capability_status, missing_inputs) \
         VALUES ($1, $2, $3, 'coverage_disclosure', $3, $3, $4, $4, 'confirmed', \
                 'insufficient', 'degraded', 'insufficient', \
                 '[\"deployer\",\"authority\",\"initial_funder\"]'::jsonb)",
    )
    .bind(ws_id)
    .bind(tag("covaged"))
    .bind(&aged_anchor)
    .bind(Utc::now() - Duration::hours(25))
    .execute(&pool)
    .await
    .expect("seed aged disclosure");

    // The default 24h view must STILL surface it: coverage is present capability, not a
    // dated observation, so a window filter must not turn it into an empty list.
    let events = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &pool, ws_id, &aged_anchor, "24h",
    )
    .await
    .expect("fetch 24h");
    assert_eq!(
        events.len(),
        1,
        "a 25h-old disclosure must remain in the default view; an empty timeline reads \
         as \"no reuse\" (REV-056-F02)"
    );
    assert_eq!(events[0].event_type, "coverage_disclosure");
    assert_eq!(
        events[0].missing_inputs,
        vec!["deployer", "authority", "initial_funder"]
    );

    // And a repeat pass on the aged anchor must REFRESH rather than refuse.
    let mint_aged = aged_anchor.trim_start_matches("solana:").to_string();
    let before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure'",
    )
    .bind(ws_id)
    .bind(&aged_anchor)
    .fetch_one(&pool)
    .await
    .expect("count before");
    crate::workers::resolve_recent_for_token(
        &pool,
        ws,
        ChainKind::Solana,
        &mint_aged,
        Utc::now(),
    )
    .await
    .expect("refresh pass");
    let after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure' \
            AND occurred_at >= now() - interval '24 hours'",
    )
    .bind(ws_id)
    .bind(&aged_anchor)
    .fetch_one(&pool)
    .await
    .expect("count after");
    assert_eq!(before, 1, "precondition: one aged disclosure");
    assert_eq!(
        after, 1,
        "a stale gap must be re-disclosed so the current window holds a live row"
    );

    // Immediately repeating must NOT append again: the refresh bucket collapses passes
    // inside one interval, otherwise every discovery pass would add a row.
    crate::workers::resolve_recent_for_token(&pool, ws, ChainKind::Solana, &mint, Utc::now())
        .await
        .expect("same-bucket pass");
    let same_bucket: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity = $2 \
            AND event_type = 'coverage_disclosure'",
    )
    .bind(ws_id)
    .bind(&anchor)
    .fetch_one(&pool)
    .await
    .expect("count same bucket");
    assert_eq!(
        same_bucket, 1,
        "two passes inside one refresh interval must collapse to one disclosure"
    );
}

// F05. A failing address must abort the whole import: `ok=true` with an inflated count
// for writes that never committed is the worst thing to be wrong about for a blocklist.
#[tokio::test]
async fn a_failed_blocklist_import_commits_nothing_and_reports_failure() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("f05")).await;
    let token = session_for(&pool, ws).await;

    let good = tag("F05GOOD");
    // `wallets.address` is `text`, so length is not a constraint; the write is made to
    // fail with a value the FK/label insert cannot accept. An address of NUL bytes is
    // rejected by PostgreSQL's text encoding, which is a real driver-level failure
    // rather than a mocked one.
    let bad = "F05BAD\0INVALID".to_string();

    let base = serve(state(&pool).await).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/api/blocklist/import"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .json(&serde_json::json!({ "chain": "solana", "addresses": [good, bad] }))
        .send()
        .await
        .expect("import request");
    assert_ne!(
        response.status(),
        StatusCode::OK,
        "an import containing a failing write must NOT report success"
    );

    // Nothing may have committed, including the address that would have succeeded.
    let committed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_labels WHERE workspace_id = $1 AND address = $2",
    )
    .bind(ws)
    .bind(&good)
    .fetch_one(&pool)
    .await
    .expect("count committed");
    assert_eq!(
        committed, 0,
        "the transaction must roll back entirely: a half-applied blocklist is not a \
         blocklist (REV-056-F05)"
    );
}

// The success path of the same handler, so the transaction is not merely "always fails".
#[tokio::test]
async fn a_successful_blocklist_import_commits_every_address() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("f05ok")).await;
    let token = session_for(&pool, ws).await;
    let a = tag("F05OKA");
    let b = tag("F05OKB");

    let base = serve(state(&pool).await).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/api/blocklist/import"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .json(&serde_json::json!({
            "chain": "sol",  // alias on purpose: it must canonicalize (F04)
            "addresses": [a.clone(), b.clone(), "# comment", ""]
        }))
        .send()
        .await
        .expect("import request");
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["imported"], 2, "comments and blanks are skipped, not counted");

    let stored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM wallet_labels \
          WHERE workspace_id = $1 AND chain = 'solana' AND address = ANY($2)",
    )
    .bind(ws)
    .bind(vec![a, b])
    .fetch_one(&pool)
    .await
    .expect("count stored");
    assert_eq!(stored, 2, "both addresses must be blocked under the canonical chain");
}

// REV-062-F07. `serve_dashboard` and `auth_status` still ran
// `validate_session(...).await.unwrap_or(false)`, so a DB failure was reported as an
// INVALID SESSION: the dashboard redirected to /login and `auth_status` answered
// `authenticated:false`. Both are client-side answers for a server-side fault, and an
// operator watching a store outage sees a logout storm instead of a 500.
//
// The probe serves the REAL admin router twice: once on a healthy pool (the session
// must be accepted) and once on a pool whose `search_path` cannot resolve
// `admin_sessions` — the shape a permission or schema failure takes. The second must
// be 500 on both surfaces, never 302/200-with-false.
#[tokio::test]
async fn an_admin_auth_store_failure_is_a_500_not_an_invalid_session() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g07f")).await;
    let token = session_for(&pool, ws).await;

    // Healthy: the session is valid, so the dashboard serves and auth_status agrees.
    let healthy = serve(state(&pool).await).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");

    let ok_dash = client
        .get(format!("{healthy}/"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .send()
        .await
        .expect("dashboard request");
    assert_eq!(
        ok_dash.status(),
        StatusCode::OK,
        "a VALID session must reach the dashboard, or the 500 assertion below proves nothing"
    );

    let (ok_status, ok_body) =
        request(&healthy, reqwest::Method::GET, "/api/auth/status", &token).await;
    assert_eq!(ok_status, StatusCode::OK);
    assert!(
        ok_body.contains("\"authenticated\":true"),
        "a valid session must report authenticated:true; got {ok_body}"
    );

    // Now break the session store the way a permission/schema fault would.
    let broken = sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("CREATE SCHEMA IF NOT EXISTS swi_g07f_empty")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET search_path = swi_g07f_empty")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&crate::pg_test_support::require_live_url())
        .await
        .expect("broken pool");

    let broken_base = serve(state(&broken).await).await;

    let dash = client
        .get(format!("{broken_base}/"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .send()
        .await
        .expect("dashboard request on a broken store");
    assert_eq!(
        dash.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unreachable session store must be a 500 on the dashboard, never a \
         redirect to /login as if the session were invalid (REV-062-F07); got {}",
        dash.status()
    );

    let (status, body) =
        request(&broken_base, reqwest::Method::GET, "/api/auth/status", &token).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unreachable session store must be a 500 on auth_status, never \
         authenticated:false (REV-062-F07); got {status} {body}"
    );
    assert!(
        !body.contains("\"authenticated\":false"),
        "a store failure must not be reported as a logged-out session; got {body}"
    );
}
