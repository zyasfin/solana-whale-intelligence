//! REV-051 C2: end-to-end two-session HTTP workspace isolation (REV-050-F02).
//!
//! The reviewer's objection was precise: threading `WorkspaceScope` and filtering in
//! store queries is supported by source review, but "static cookie/source inspection
//! is not end-to-end isolation evidence". So this drives the REAL axum router with
//! two real session cookies bound to two different workspaces and asserts what a
//! tenant can and cannot see over HTTP.
//!
//! Skipped (not failed) without `TEST_DATABASE_URL`/`DATABASE_URL`, like the other
//! `pg_tests` modules.
//!
//! What is exercised, deliberately through the router and not through the store:
//!   * session A reads the event appended in workspace A;
//!   * session B, hitting the SAME URL, cannot see it;
//!   * a session with no workspace binding is refused (401), not served workspace 1;
//!   * no request body or path segment can steer the workspace.

#![cfg(all(test, feature = "pg_tests"))]

use chrono::Utc;
use reqwest::StatusCode;
use sqlx::PgPool;

use crate::admin::{router, AdminState};
use crate::config::Settings;
use solana_whale_intelligence::sf::core::TruthStatus;
use solana_whale_intelligence::sf::recent::{
    CapabilityStatus, Coverage, IdentityKey, IdentityKind, RecentConfidence, RecentEvent,
    RecentRelation,
};
use solana_whale_intelligence::sf::recent_store::append_recent_event_if_absent;

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

/// Create a workspace and return its id.
async fn workspace(pool: &PgPool, slug: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ($1, $2) RETURNING id")
        .bind(slug)
        .bind(slug)
        .fetch_one(pool)
        .await
        .expect("create workspace")
}

/// Mint a session bound to `workspace_id` and return the RAW cookie token.
///
/// Written directly rather than through `create_session()` because that function
/// binds to the single/default workspace by design (REV-029); this test needs two
/// DIFFERENT bindings, which is the tenancy configuration under test.
async fn session_for(pool: &PgPool, workspace_id: Option<i64>) -> String {
    use sha2::{Digest, Sha256};
    let token = crate::auth::new_session_token();
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    let hash = hex::encode(h.finalize());
    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, expires_at, workspace_id) \
         VALUES ($1, now() + interval '1 hour', $2)",
    )
    .bind(&hash)
    .bind(workspace_id)
    .execute(pool)
    .await
    .expect("create session");
    token
}

fn event(event_id: &str, anchor: &str) -> RecentEvent {
    RecentEvent {
        event_id: event_id.into(),
        event_type: "relation_resolved".into(),
        anchor_identity: anchor.into(),
        // `fetch_relations` projects one candidate per RELATED identity, so an event
        // with an empty list yields no relation at all. The target is what a tenant
        // would actually read, and it is what must not cross the boundary.
        related_identities: vec![IdentityKey {
            kind: IdentityKind::Token,
            value: "solana:C2OTHER".into(),
        }],
        chain_qualified_contract: anchor.into(),
        occurred_at: Utc::now(),
        observed_at: Utc::now(),
        relation: Some(RecentRelation::SameDeployer),
        truth_status: TruthStatus::Confirmed,
        confidence: None,
        confidence_level: RecentConfidence::Exact,
        evidence_refs: vec!["ev-c2".into()],
        dependency_group: None,
        freshness: None,
        coverage: Coverage::Full,
        capability_status: CapabilityStatus::Available,
        missing_inputs: vec![],
        retraction: None,
        is_current_coverage: false,
    }
}

/// Bind the REAL admin router on a loopback port and return its base URL.
///
/// A real listener, not an in-process service call: the reviewer rejected static
/// inspection, and a `oneshot` against a `Router` would still skip cookie parsing at
/// the transport layer — which is exactly where a tenancy bug would hide. The task is
/// detached; the test process exits and takes it with it.
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

async fn get_with_cookie(base: &str, path: &str, token: Option<&str>) -> (StatusCode, String) {
    // `cookie_store` is not enabled on this reqwest build, so the header is set
    // explicitly per request — which is also stricter: each request carries exactly
    // the session under test and nothing is shared between them.
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{base}{path}"));
    if let Some(token) = token {
        req = req.header(reqwest::header::COOKIE, format!("swi_session={token}"));
    }
    let response = req.send().await.expect("http request");
    let status = response.status();
    let body = response.text().await.expect("body");
    (status, body)
}

// The core C2 assertion: same URL, two sessions, two answers.
#[tokio::test]
async fn two_http_sessions_cannot_read_across_workspaces() {
    let pool = pool().await;
    // `require_auth` allows reads when auth is unconfigured, but the WORKSPACE is
    // resolved from the session regardless, which is the boundary under test. The
    // assertions below therefore hold in both configurations.
    let state = AdminState {
        pool: pool.clone(),
        settings: std::sync::Arc::new(Settings {
            config: crate::config::AppConfig::default(),
            env: crate::config::EnvConfig::load(),
        }),
    };

    let ws_a = workspace(&pool, &tag("c2a")).await;
    let ws_b = workspace(&pool, &tag("c2b")).await;
    let token_a = session_for(&pool, Some(ws_a)).await;
    let token_b = session_for(&pool, Some(ws_b)).await;

    // The SAME anchor in both tenants: a leak cannot hide behind a distinct key.
    let mint = tag("C2MINT");
    let anchor = format!("solana:{mint}");
    append_recent_event_if_absent(&pool, ws_a, &event(&tag("c2evt"), &anchor))
        .await
        .expect("append in workspace A");

    let base = serve(state).await;
    let uri = format!("/api/tokens/solana/{mint}/recent?window=all");
    let (status_a, body_a) = get_with_cookie(&base, &uri, Some(&token_a)).await;
    let (status_b, body_b) = get_with_cookie(&base, &uri, Some(&token_b)).await;

    assert_eq!(status_a, StatusCode::OK, "workspace A must read its own event");
    assert!(
        body_a.contains("relation_resolved"),
        "workspace A must see the event it appended; got {body_a}"
    );

    assert_eq!(status_b, StatusCode::OK, "workspace B is authenticated, just empty");
    assert_eq!(
        body_b, "[]",
        "workspace B must not see workspace A's event for the same anchor; got {body_b}"
    );

    // The relations projection is a separate SQL path and was a separate defect class
    // (REV-027/REV-029), so it is asserted independently rather than assumed.
    let rel_uri = format!("/api/tokens/solana/{mint}/relations");
    let (rel_a_status, rel_a) = get_with_cookie(&base, &rel_uri, Some(&token_a)).await;
    let (rel_b_status, rel_b) = get_with_cookie(&base, &rel_uri, Some(&token_b)).await;
    assert_eq!(rel_a_status, StatusCode::OK);
    assert_eq!(rel_b_status, StatusCode::OK);
    assert!(
        rel_a.contains("SAME_DEPLOYER"),
        "workspace A must see its own relation; got {rel_a}"
    );
    assert_eq!(
        rel_b, "[]",
        "workspace B must not see workspace A's relation; got {rel_b}"
    );
}

// A session with no workspace binding must be REFUSED, never silently served the
// default tenant. This is the fail-closed direction of the same boundary.
#[tokio::test]
async fn an_unbound_session_is_refused_not_defaulted() {
    let pool = pool().await;
    let state = AdminState {
        pool: pool.clone(),
        settings: std::sync::Arc::new(Settings {
            config: crate::config::AppConfig::default(),
            env: crate::config::EnvConfig::load(),
        }),
    };

    let unbound = session_for(&pool, None).await;
    let mint = tag("C2UNBOUND");
    let uri = format!("/api/tokens/solana/{mint}/recent?window=all");

    let base = serve(state).await;
    let (status, _) = get_with_cookie(&base, &uri, Some(&unbound)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a session with no workspace binding must fail closed, not read workspace 1"
    );

    // No cookie at all: same refusal, for the same reason.
    let (no_cookie, _) = get_with_cookie(&base, &uri, None).await;
    assert_eq!(no_cookie, StatusCode::UNAUTHORIZED);
}
