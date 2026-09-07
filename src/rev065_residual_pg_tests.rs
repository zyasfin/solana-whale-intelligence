//! Live PostgreSQL regressions for REV-064 F04–F08.
//!
//! Each test reproduces the reviewer's own probe rather than restating a fix:
//!
//! * F04 — identical fixtures with reversed insertion order must name the SAME
//!   initial funder (the old ORDER BY was not a total order).
//! * F05 — two tuples that flattened to identical bytes under the `|` join must
//!   produce two distinct rows, not one swallowed by `ON CONFLICT DO NOTHING`.
//! * F06 — `trace_wallet` must obey the disposition AUTHORITY, not the existence of
//!   any `flow_only` row.
//! * F07 — a rejected `login_attempts` INSERT/DELETE or `admin_sessions` DELETE must
//!   be a 500 through the real router, never a successful login or logout.
//! * F08 — two EXPLICIT connections with distinct backend PIDs, the first insert held
//!   open in a transaction, the second demonstrably blocked on the unique index.
//!
//! No skip guard (REV-056-F06): with `pg_tests` enabled a missing database is a
//! configuration error, not a reason to report success.

#![cfg(all(test, feature = "pg_tests"))]

use chrono::{Duration, Utc};
use reqwest::StatusCode;
use sqlx::{Connection, PgConnection, PgPool};

use crate::admin::{router as admin_router, AdminState};
use crate::config::Settings;
use crate::models::ChainKind;
use solana_whale_intelligence::sf::recent::{
    CapabilityStatus, Coverage, IdentityKey, IdentityKind, RecentConfidence, RecentEvent,
    RecentRelation,
};
use solana_whale_intelligence::sf::recent_pipeline::{relation_event_key, WorkspaceScope};
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

/// Seed one funding edge with an explicit signature/block_time so a TIE can be built.
#[allow(clippy::too_many_arguments)]
async fn seed_edge(
    pool: &PgPool,
    from: &str,
    to: &str,
    signature: &str,
    block_time: chrono::DateTime<Utc>,
    mint: &str,
) {
    sqlx::query(
        "INSERT INTO funding_edges \
             (chain, from_address, to_address, signature, edge_kind, raw_amount, \
              block_time, confidence, evidence, promoted) \
         VALUES ('solana', $1, $2, $3, 'funding', '1', $4, 0.35, \
                 jsonb_build_object('mint', $5::text), false) \
         ON CONFLICT DO NOTHING",
    )
    .bind(from)
    .bind(to)
    .bind(signature)
    .bind(block_time)
    .bind(mint)
    .execute(pool)
    .await
    .expect("seed funding edge");
}

// ---------------------------------------------------------------------------
// F04 — total order
// ---------------------------------------------------------------------------

// REV-064-F04. `ORDER BY block_time ASC, signature ASC LIMIT 1` is not a total order
// over the legacy PK `(chain, from_address, to_address, signature, edge_kind)`: two
// rows may share `block_time` AND `signature`. The reviewer's probe seeded the same
// fixture twice with the insertion order reversed and got two different "initial
// funders", i.e. the authoritative actor depended on physical order.
//
// The tie is only OBSERVABLE when the planner sorts the heap rows itself: an index
// scan feeds the sort in index order and hides it, which is exactly why an
// incomplete ORDER BY survives casual testing and then changes its answer in
// production after a plan flip. The probe therefore runs the production helper under
// BOTH plans; with the old ORDER BY the sequential plan returns `Z_FUNDER` for the
// reversed fixture and `A_FUNDER` for the other.
#[tokio::test]
async fn the_initial_funder_is_stable_under_a_block_time_and_signature_tie() {
    let pool = pool().await;
    let block_time = Utc::now() - Duration::days(3);

    // Fixture 1: A_FUNDER inserted first. Both rows share block_time AND signature,
    // so only the remaining PK columns can break the tie.
    let mint_a = tag("G64F04A");
    let sig_a = format!("sig-tie-{mint_a}");
    seed_edge(&pool, "A_FUNDER", "RECIPIENT_1", &sig_a, block_time, &mint_a).await;
    seed_edge(&pool, "Z_FUNDER", "RECIPIENT_2", &sig_a, block_time, &mint_a).await;

    // Fixture 2: identical data, insertion order REVERSED.
    let mint_b = tag("G64F04B");
    let sig_b = format!("sig-tie-{mint_b}");
    seed_edge(&pool, "Z_FUNDER", "RECIPIENT_2", &sig_b, block_time, &mint_b).await;
    seed_edge(&pool, "A_FUNDER", "RECIPIENT_1", &sig_b, block_time, &mint_b).await;

    for (plan, probe_pool) in [("index scan", pool.clone()), ("sequential scan", heap_order_pool().await)] {
        let first = crate::workers::earliest_token_funder(&probe_pool, ChainKind::Solana, &mint_a)
            .await
            .expect("earliest funder, insertion order A,Z");
        let reverse = crate::workers::earliest_token_funder(&probe_pool, ChainKind::Solana, &mint_b)
            .await
            .expect("earliest funder, insertion order Z,A");

        assert_eq!(
            first, reverse,
            "under a {plan} identical data must yield the same authoritative actor \
             regardless of insertion order; got {first:?} vs {reverse:?} (REV-064-F04)"
        );
        assert_eq!(
            first.as_deref(),
            Some("A_FUNDER"),
            "under a {plan} the tie must be broken by the remaining PK columns, so the \
             lexicographically first `from_address` wins deterministically"
        );
    }
}

/// A pool whose connections cannot use an index scan, so the planner reads the heap
/// and sorts it — i.e. the row order the sort sees IS the physical order.
///
/// This is not a contrived setting: it is the plan PostgreSQL picks by itself once a
/// table is small, statistics change, or the index is unusable. An ORDER BY that is
/// only deterministic under one plan is not deterministic (REV-064-F04).
async fn heap_order_pool() -> PgPool {
    sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                for stmt in [
                    "SET search_path = swi_legacy, public",
                    "SET enable_indexscan = off",
                    "SET enable_indexonlyscan = off",
                    "SET enable_bitmapscan = off",
                ] {
                    sqlx::query(stmt).execute(&mut *conn).await?;
                }
                Ok(())
            })
        })
        .connect(&crate::pg_test_support::require_live_url())
        .await
        .expect("heap-order pool")
}

// ---------------------------------------------------------------------------
// F05 — injective key encoding, proven at the STORE
// ---------------------------------------------------------------------------

fn relation_event(ws_id: i64, anchor: &str, relation: &str, target: &str) -> RecentEvent {
    let key = relation_event_key(
        WorkspaceScope::from_job_context(ws_id).unwrap(),
        anchor,
        relation,
        target,
        IdentityKind::Wallet,
        RecentConfidence::Exact,
        &["funding_edge:solana:funding:promoted".to_string()],
    );
    RecentEvent {
        event_id: key,
        event_type: "relation_resolved".to_string(),
        // The anchor stored on the row must satisfy the anchor invariant, so the
        // varying part of the tuple is carried by the KEY, which is what this test
        // is about.
        anchor_identity: anchor.to_string(),
        related_identities: vec![IdentityKey {
            kind: IdentityKind::Wallet,
            value: target.to_string(),
        }],
        chain_qualified_contract: anchor.to_string(),
        occurred_at: Utc::now(),
        observed_at: Utc::now(),
        relation: Some(RecentRelation::SameDeployer),
        truth_status: solana_whale_intelligence::sf::core::TruthStatus::Confirmed,
        confidence: None,
        confidence_level: RecentConfidence::Exact,
        evidence_refs: vec!["funding_edge:solana:funding:promoted".to_string()],
        dependency_group: None,
        freshness: None,
        coverage: Coverage::Full,
        capability_status: CapabilityStatus::Available,
        missing_inputs: vec![],
        retraction: None,
        is_current_coverage: false,
    }
}

// REV-064-F05. The tuple was joined with a bare `|`, so
// `anchor=A            target=B|same_deployer|C` and
// `anchor=A|same_deployer|B  target=C`
// hashed to the SAME key (the reviewer published the digest). Two different facts
// then shared one `event_id`, and the second was swallowed by `ON CONFLICT DO
// NOTHING` — a silent loss, not a visible error. Two rows must exist.
#[tokio::test]
async fn two_tuples_that_collided_under_the_old_join_now_publish_two_rows() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g64f05")).await;
    let stem = tag("G64F05");
    // Under the old `ws|anchor|relation|target|...` join both tuples flatten to the
    // identical byte string `<anchor_a>|same_deployer|solana:B|same_deployer|C`,
    // which is the collision the reviewer published a digest for.
    let anchor_a = format!("solana:{stem}A");
    let anchor_b = format!("{anchor_a}|same_deployer|solana:B");

    let e1 = relation_event(ws_id, &anchor_a, "same_deployer", "solana:B|same_deployer|C");
    let e2 = relation_event(ws_id, &anchor_b, "same_deployer", "C");

    assert_ne!(
        e1.event_id, e2.event_id,
        "two distinct tuples must not share an event key (REV-064-F05)"
    );

    assert!(
        append_recent_event_if_absent(&pool, ws_id, &e1)
            .await
            .expect("append first"),
        "the first fact must be appended"
    );
    assert!(
        append_recent_event_if_absent(&pool, ws_id, &e2)
            .await
            .expect("append second"),
        "the second fact must be appended, not silently swallowed as a duplicate"
    );

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events \
          WHERE workspace_id = $1 AND anchor_identity IN ($2, $3)",
    )
    .bind(ws_id)
    .bind(&anchor_a)
    .bind(&anchor_b)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(rows, 2, "two distinct facts must persist as two rows");

    // A byte-identical retry is still ONE fact: the framing must not break idempotency.
    assert!(
        !append_recent_event_if_absent(&pool, ws_id, &e1)
            .await
            .expect("retry"),
        "an identical retry must remain a no-op"
    );
}

// ---------------------------------------------------------------------------
// F06 — traversal obeys the disposition authority
// ---------------------------------------------------------------------------

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

// REV-064-F06. `trace_wallet` asked the table "does ANY active row say flow_only?",
// which is not the policy. With a manual `watch` beside an automatic `flow_only` the
// authority is `watch` (manual wins), yet the trace still truncated: the traversal
// contradicted `active_disposition`, i.e. two readers of one policy disagreed.
#[tokio::test]
async fn traversal_follows_the_disposition_authority_not_any_flow_only_row() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g64f06")).await;
    let stem = tag("G64F06");

    // Case 1: manual `watch` + automatic `flow_only`. Authority = watch => WALK.
    let walker = format!("{stem}WALK");
    seed_label_with(&pool, ws, &walker, "manual_watch", "watch", true).await;
    seed_label_with(&pool, ws, &walker, "auto_exchange", "flow_only", false).await;
    seed_edge(
        &pool,
        &walker,
        &format!("{stem}PEER"),
        &format!("sig-{stem}-walk"),
        Utc::now() - Duration::hours(2),
        &format!("{stem}MINT"),
    )
    .await;

    assert_eq!(
        crate::db::active_disposition(&pool, ws, "solana", &walker)
            .await
            .expect("authority")
            .as_deref(),
        Some("watch"),
        "manual labels win: the authority here is `watch`"
    );
    let steps = crate::graph::trace_wallet(&pool, ws, ChainKind::Solana, &walker, 3)
        .await
        .expect("trace");
    assert!(
        !steps.is_empty() && steps.iter().all(|s| !s.endpoint),
        "the authority is `watch`, so traversal must continue; got {} steps, \
         endpoint flags {:?} (REV-064-F06)",
        steps.len(),
        steps.iter().map(|s| s.endpoint).collect::<Vec<_>>()
    );

    // Case 2: automatic `flow_only` alone. Authority = flow_only => STOP.
    let stopper = format!("{stem}STOP");
    seed_label_with(&pool, ws, &stopper, "auto_exchange", "flow_only", false).await;
    seed_edge(
        &pool,
        &stopper,
        &format!("{stem}PEER2"),
        &format!("sig-{stem}-stop"),
        Utc::now() - Duration::hours(2),
        &format!("{stem}MINT"),
    )
    .await;
    let steps = crate::graph::trace_wallet(&pool, ws, ChainKind::Solana, &stopper, 3)
        .await
        .expect("trace");
    assert!(
        !steps.is_empty() && steps.iter().all(|s| s.endpoint),
        "an authoritative `flow_only` must terminate the trace at its immediate edges"
    );

    // Case 3: manual `skip` + automatic `flow_only`. Authority = skip, which is
    // strictly MORE restrictive than flow_only, so traversal must stop. Stopping
    // only on the literal string `flow_only` would let the stricter label LOOSEN
    // the policy — the exact inversion this finding is about.
    let skipper = format!("{stem}SKIP");
    seed_label_with(&pool, ws, &skipper, "manual_block", "skip", true).await;
    seed_label_with(&pool, ws, &skipper, "auto_exchange", "flow_only", false).await;
    seed_edge(
        &pool,
        &skipper,
        &format!("{stem}PEER3"),
        &format!("sig-{stem}-skip"),
        Utc::now() - Duration::hours(2),
        &format!("{stem}MINT"),
    )
    .await;
    let steps = crate::graph::trace_wallet(&pool, ws, ChainKind::Solana, &skipper, 3)
        .await
        .expect("trace");
    assert!(
        !steps.is_empty() && steps.iter().all(|s| s.endpoint),
        "a `skip` wallet is more restrictive than `flow_only`; traversal must not \
         walk through it"
    );

    // Case 4: an EXPIRED or REVOKED flow_only must not truncate (unchanged contract,
    // re-proven through the new code path).
    let expired = format!("{stem}EXPIRED");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&expired)
    .execute(&pool)
    .await
    .expect("seed wallet");
    sqlx::query(
        "INSERT INTO wallet_labels \
             (workspace_id, chain, address, kind, disposition, manual, confidence, expires_at) \
         VALUES ($1, 'solana', $2, 'auto_exchange', 'flow_only', false, 90, now() - interval '1 hour')",
    )
    .bind(ws)
    .bind(&expired)
    .execute(&pool)
    .await
    .expect("seed expired label");
    seed_edge(
        &pool,
        &expired,
        &format!("{stem}PEER4"),
        &format!("sig-{stem}-expired"),
        Utc::now() - Duration::hours(2),
        &format!("{stem}MINT"),
    )
    .await;
    let steps = crate::graph::trace_wallet(&pool, ws, ChainKind::Solana, &expired, 3)
        .await
        .expect("trace");
    assert!(
        steps.iter().all(|s| !s.endpoint),
        "an expired label must not suppress traversal"
    );

    // Case 5: an unknown disposition is a schema bug and must SURFACE as an error,
    // never be silently ranked as "walk".
    let bogus = format!("{stem}BOGUS");
    seed_label_with(&pool, ws, &bogus, "auto_weird", "teleport", false).await;
    let err = match crate::graph::trace_wallet(&pool, ws, ChainKind::Solana, &bogus, 3).await {
        Ok(steps) => panic!(
            "an unknown disposition must be an error, not a silent walk; got {} steps",
            steps.len()
        ),
        Err(err) => err,
    };
    assert!(
        format!("{err:#}").contains("teleport"),
        "the error must name the unknown disposition; got {err:#}"
    );
}

// ---------------------------------------------------------------------------
// F07 — auth mutation errors are 500, through the real router
// ---------------------------------------------------------------------------

fn configure_admin_auth() -> &'static str {
    crate::pg_test_support::configure_admin_auth()
}

/// A pool whose `search_path` starts at a shadow schema where `table` REJECTS the
/// named statement kinds, so the handler meets a real store failure.
///
/// The shadow table is created `LIKE` the real one and a `BEFORE` trigger raises —
/// that is how a permission denial or a broken constraint presents to the handler,
/// without needing a second database role.
async fn denying_pool(schema: &str, table: &str, events: &str) -> PgPool {
    let admin = pool().await;
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&admin)
        .await
        .expect("drop shadow schema");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .expect("create shadow schema");
    sqlx::query(&format!(
        "CREATE TABLE {schema}.{table} (LIKE public.{table} INCLUDING ALL)"
    ))
    .execute(&admin)
    .await
    .expect("create shadow table");
    sqlx::query(&format!(
        "CREATE OR REPLACE FUNCTION {schema}.deny() RETURNS trigger LANGUAGE plpgsql AS \
         $fn$ BEGIN RAISE EXCEPTION 'store unavailable'; END $fn$"
    ))
    .execute(&admin)
    .await
    .expect("create deny function");
    sqlx::query(&format!(
        "CREATE TRIGGER deny_writes BEFORE {events} ON {schema}.{table} \
         FOR EACH ROW EXECUTE FUNCTION {schema}.deny()"
    ))
    .execute(&admin)
    .await
    .expect("create deny trigger");

    let search_path = format!("SET search_path = {schema}, swi_legacy, public");
    sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
        .max_connections(2)
        .after_connect(move |conn, _| {
            let sql = search_path.clone();
            Box::pin(async move {
                sqlx::query(&sql).execute(&mut *conn).await?;
                Ok(())
            })
        })
        .connect(&crate::pg_test_support::require_live_url())
        .await
        .expect("denying pool")
}

async fn serve_admin(pool: &PgPool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
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
    format!("http://{addr}")
}

// REV-064-F07. `let _ = auth::record_attempt(...)` made the rate limiter optional:
// the audit row is what `is_rate_limited` counts, so a failing INSERT meant failed
// logins were never counted and brute-force protection silently did not exist.
#[tokio::test]
async fn a_failed_login_attempt_that_cannot_be_recorded_is_a_500() {
    let password = configure_admin_auth();
    let denying = denying_pool("swi_rev065_f07a", "login_attempts", "INSERT").await;
    let base = serve_admin(&denying).await;

    // Wrong password: the attempt must be recorded, and it cannot be.
    let response = reqwest::Client::new()
        .post(format!("{base}/login"))
        .form(&[("password", "definitely-wrong")])
        .send()
        .await
        .expect("login request");
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unrecordable failed attempt must fail closed, not render the login page"
    );

    // Correct password: the successful attempt must be recorded too.
    let response = reqwest::Client::new()
        .post(format!("{base}/login"))
        .form(&[("password", password)])
        .send()
        .await
        .expect("login request");
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a login whose audit row cannot be written must not hand out a session"
    );
    assert!(
        response.headers().get(reqwest::header::SET_COOKIE).is_none(),
        "no session cookie may be issued when the login could not be audited"
    );
}

// Same class, the DELETE side: `clear_attempts` failing leaves the failure counter
// standing, so the next login is rate-limited for a reason the operator cannot see.
#[tokio::test]
async fn a_successful_login_that_cannot_clear_attempts_is_a_500() {
    let password = configure_admin_auth();
    let denying = denying_pool("swi_rev065_f07b", "login_attempts", "DELETE").await;
    let base = serve_admin(&denying).await;
    // A per-row `BEFORE DELETE` trigger only fires when there IS a row to delete, so
    // seed the failed attempt `clear_attempts` is supposed to remove. Without it the
    // DELETE is a no-op and the test would pass for the wrong reason.
    sqlx::query(
        "INSERT INTO login_attempts (key, success, ip, user_agent) \
         VALUES ('ip:unknown', false, NULL, NULL)",
    )
    .execute(&denying)
    .await
    .expect("seed failed attempt");


    let response = reqwest::Client::new()
        .post(format!("{base}/login"))
        .form(&[("password", password)])
        .send()
        .await
        .expect("login request");
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a login that cannot clear the failure counter must fail closed"
    );
    assert!(
        response.headers().get(reqwest::header::SET_COOKIE).is_none(),
        "no session cookie may be issued"
    );
}

// REV-064-F07, logout half: the browser was told "logged out" (cookie cleared,
// redirect) while the server-side session was still valid for anyone holding the
// token. A store failure must be a 500 and must NOT clear the cookie.
#[tokio::test]
async fn a_logout_that_cannot_destroy_the_session_is_a_500() {
    configure_admin_auth();
    let live = pool().await;
    let ws = workspace(&live, &tag("g64f07c")).await;
    let token = session_for(&live, ws).await;

    let denying = denying_pool("swi_rev065_f07c", "admin_sessions", "DELETE").await;
    let base = serve_admin(&denying).await;

    // Seed the SAME session row in the shadow table so the DELETE has a row to hit;
    // the trigger fires per row.
    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, expires_at, workspace_id) \
         SELECT token_hash, expires_at, workspace_id FROM public.admin_sessions \
          WHERE workspace_id = $1",
    )
    .bind(ws)
    .execute(&denying)
    .await
    .expect("mirror session row");

    let response = reqwest::Client::new()
        .post(format!("{base}/logout"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .send()
        .await
        .expect("logout request");
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "an undestroyable session must not be reported as logged out"
    );
    assert!(
        response.headers().get(reqwest::header::SET_COOKIE).is_none(),
        "the cookie must not be cleared while the server session is still valid"
    );
}

// ---------------------------------------------------------------------------
// F08 — real contention on the unique index
// ---------------------------------------------------------------------------

fn coverage_event(anchor: &str, event_id: &str) -> RecentEvent {
    RecentEvent {
        event_id: event_id.to_string(),
        event_type: "coverage_disclosure".to_string(),
        anchor_identity: anchor.to_string(),
        related_identities: vec![],
        chain_qualified_contract: anchor.to_string(),
        occurred_at: Utc::now(),
        observed_at: Utc::now(),
        relation: None,
        truth_status: solana_whale_intelligence::sf::core::TruthStatus::Confirmed,
        confidence: None,
        confidence_level: RecentConfidence::Insufficient,
        evidence_refs: vec![],
        dependency_group: None,
        freshness: None,
        coverage: Coverage::Degraded,
        capability_status: CapabilityStatus::Available,
        missing_inputs: vec!["deployer".to_string()],
        retraction: None,
        is_current_coverage: false,
    }
}

/// A dedicated backend, connected the way the RUNTIME connects.
///
/// REV-067-F08: the previous helper used `db::connect(url, 1)`, but that connector
/// clamps to `max_connections.max(2)` — so "one dedicated backend" was not what the
/// pool provided, and the PID read from it was not necessarily the appender's. A raw
/// `PgConnection` is one backend by construction, and the appender runs ON it, so the
/// PID we assert about is the PID that contends.
async fn runtime_connection() -> PgConnection {
    let url = crate::pg_test_support::require_live_url();
    let mut conn = PgConnection::connect(&url).await.expect("connection");
    // Same `search_path` the runtime connector sets, so the test resolves the same
    // tables production does.
    sqlx::query("SET search_path = swi_legacy, public")
        .execute(&mut conn)
        .await
        .expect("search_path");
    conn
}

async fn backend_pid(conn: &mut PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn)
        .await
        .expect("backend pid")
}

/// Hold an uncommitted insert of `event.event_id` open on one backend, then run the
/// production append for the SAME key on a second backend and prove — through
/// PostgreSQL's own lock view — that it is genuinely blocked on the unique index.
///
/// REV-067-F08 replaced three weak proofs:
///
/// * `!JoinHandle::is_finished()` after a sleep says only "not done yet"; it cannot
///   distinguish a lock wait from a slow start. The waiter is now identified in
///   `pg_stat_activity` with `wait_event_type = 'Lock'`, which is the database
///   asserting the contention, not the test inferring it.
/// * a START LATCH is set inside the spawned task, so the append is known to have
///   begun before the wait is judged — otherwise a task that had not yet been polled
///   would look identical to a blocked one.
/// * every phase runs under an OUTER TIMEOUT, so a genuine deadlock fails the test
///   instead of hanging the lane.
///
/// Returns what the blocked append reported once the holder resolved.
async fn assert_second_append_blocks_on_the_unique_index(
    ws_id: i64,
    event: RecentEvent,
    commit_first: bool,
) -> bool {
    let overall = std::time::Duration::from_secs(30);
    tokio::time::timeout(overall, async move {
        let mut holder = runtime_connection().await;
        let holder_pid = backend_pid(&mut holder).await;

        let mut appender_conn = runtime_connection().await;
        let appender_pid = backend_pid(&mut appender_conn).await;
        assert_ne!(
            holder_pid, appender_pid,
            "the two participants must be two distinct backends, or nothing contends"
        );

        // Hold an UNCOMMITTED insert of the same key open. Built through the same
        // production writer so the row shape cannot drift from what the appender
        // will try to insert.
        let mut tx = holder.begin().await.expect("begin");
        insert_event_tx(&mut tx, ws_id, &event).await;

        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let start_signal = started.clone();
        let e = event.clone();
        let appender = tokio::spawn(async move {
            start_signal.notify_one();
            solana_whale_intelligence::sf::recent_store::append_recent_event_on(
                &mut appender_conn,
                ws_id,
                &e,
            )
            .await
        });

        // The append has been entered; anything after this is real work.
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("the appender task must start");

        // PostgreSQL itself must report the appender waiting on a lock. This is the
        // assertion that cannot pass without contention.
        let mut observer = runtime_connection().await;
        let waited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let row: Option<(String, Option<String>)> = sqlx::query_as(
                    "SELECT wait_event_type, wait_event FROM pg_stat_activity \
                      WHERE pid = $1 AND wait_event_type = 'Lock'",
                )
                .bind(appender_pid)
                .fetch_optional(&mut observer)
                .await
                .expect("pg_stat_activity");
                if let Some(row) = row {
                    return row;
                }
                assert!(
                    !appender.is_finished(),
                    "the append finished without ever waiting on a lock, so it never \
                     contended with the uncommitted insert (REV-067-F08)"
                );
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the second append must block on the unique index while the first \
                 insert is uncommitted");
        assert_eq!(
            waited.0, "Lock",
            "the appender must be waiting on a lock, not on client or IO"
        );

        if commit_first {
            tx.commit().await.expect("commit holder");
        } else {
            tx.rollback().await.expect("rollback holder");
        }

        tokio::time::timeout(std::time::Duration::from_secs(10), appender)
            .await
            .expect("the blocked append must resolve once the holder resolves")
            .expect("join")
            .expect("the blocked append must not fail on a unique violation")
    })
    .await
    .expect("the contention probe must not hang")
}

/// The holder's uncommitted insert.
///
/// This one statement stays in the test on purpose: it must run INSIDE a transaction
/// the test controls (so it can be held open, then committed or rolled back), which
/// the production helper deliberately does not expose. It is a plain `INSERT` with no
/// `ON CONFLICT` — it is the row being contended FOR, not the append under test.
/// The append under test is `recent_store::append_recent_event_on`, called directly.
async fn insert_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ws_id: i64,
    e: &RecentEvent,
) {
    sqlx::query(
        "INSERT INTO recent_events ( \
             workspace_id, event_id, token_identity, anchor_identity, \
             chain_qualified_contract, event_type, related_identities, \
             occurred_at, observed_at, relation, truth_status, confidence_level, \
             evidence_refs, coverage, capability_status, missing_inputs) \
         VALUES ($1, $2, $3, $3, $3, $4, $5, now(), now(), $6::recent_relation, \
                 'confirmed', $7, $8, \
                 CASE WHEN $9::jsonb = '[]'::jsonb THEN 'full' ELSE 'degraded' END, \
                 'available', $9)",
    )
    .bind(ws_id)
    .bind(&e.event_id)
    .bind(&e.anchor_identity)
    .bind(&e.event_type)
    .bind(serde_json::to_value(&e.related_identities).unwrap_or_default())
    .bind(e.relation.map(|r| r.as_str().to_string()))
    .bind(e.confidence_level.as_str())
    .bind(serde_json::to_value(&e.evidence_refs).unwrap_or_default())
    .bind(serde_json::to_value(&e.missing_inputs).unwrap_or_default())
    .execute(&mut **tx)
    .await
    .expect("holder insert");
}

#[tokio::test]
async fn a_blocked_concurrent_append_resolves_as_a_no_op_when_the_winner_commits() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g64f08a")).await;
    let anchor = format!("solana:{}", tag("G64F08A"));
    let event = coverage_event(&anchor, &format!("cov-race-{anchor}"));

    let appended = assert_second_append_blocks_on_the_unique_index(ws_id, event, true).await;
    assert!(
        !appended,
        "the loser of a real contention must report `false` (already present), not \
         an error and not a second row"
    );

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events WHERE workspace_id = $1 AND anchor_identity = $2",
    )
    .bind(ws_id)
    .bind(&anchor)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(rows, 1, "exactly one row may exist for one event key");
}

#[tokio::test]
async fn a_blocked_concurrent_append_wins_when_the_holder_rolls_back() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g64f08b")).await;
    let anchor = format!("solana:{}", tag("G64F08B"));
    let event = coverage_event(&anchor, &format!("cov-race-{anchor}"));

    // Rolling back proves the block was on the INDEX, not on anything incidental:
    // the waiter is released and its own insert succeeds.
    let appended = assert_second_append_blocks_on_the_unique_index(ws_id, event, false).await;
    assert!(
        appended,
        "when the holder rolls back, the blocked append must proceed and win"
    );

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recent_events WHERE workspace_id = $1 AND anchor_identity = $2",
    )
    .bind(ws_id)
    .bind(&anchor)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(rows, 1, "exactly one row may exist for one event key");
}

// The same mechanism on a RELATION revision, per the finding's explicit "coverage
// AND relation revision" requirement.
//
// REV-067-F08: the previous version computed a `rel:` key and then wrapped it in
// `coverage_event(...)`, so the row it persisted was a `coverage_disclosure` with
// `relation NULL`. It named a relation and tested a disclosure. This builds and
// READS BACK a real `relation_resolved` row carrying a non-null relation enum.
#[tokio::test]
async fn a_relation_revision_append_also_contends_on_the_unique_index() {
    let pool = pool().await;
    let ws_id = workspace(&pool, &tag("g64f08c")).await;
    let anchor = format!("solana:{}", tag("G64F08C"));
    let evidence = vec!["funding_edge:solana:funding:promoted".to_string()];
    let key = relation_event_key(
        WorkspaceScope::from_job_context(ws_id).unwrap(),
        &anchor,
        "same_deployer",
        "solana:DEPLOYER",
        IdentityKind::Wallet,
        RecentConfidence::Exact,
        &evidence,
    );
    let event_id = key.clone();
    let event = RecentEvent {
        event_id: key,
        event_type: "relation_resolved".to_string(),
        anchor_identity: anchor.clone(),
        related_identities: vec![IdentityKey {
            kind: IdentityKind::Wallet,
            value: "solana:DEPLOYER".to_string(),
        }],
        chain_qualified_contract: anchor.clone(),
        occurred_at: Utc::now(),
        observed_at: Utc::now(),
        relation: Some(RecentRelation::SameDeployer),
        truth_status: solana_whale_intelligence::sf::core::TruthStatus::Confirmed,
        confidence: None,
        confidence_level: RecentConfidence::Exact,
        evidence_refs: evidence,
        dependency_group: None,
        freshness: None,
        coverage: Coverage::Full,
        capability_status: CapabilityStatus::Available,
        missing_inputs: vec![],
        retraction: None,
        is_current_coverage: false,
    };

    let appended = assert_second_append_blocks_on_the_unique_index(ws_id, event, true).await;
    assert!(
        !appended,
        "a relation revision key must contend the same way a coverage key does"
    );

    // The persisted row must really be a relation, or this test would once again be
    // a disclosure test wearing a relation's name.
    //
    // REV-069-F08: read back through the DOMAIN type, not `relation::text`. A text
    // cast would still pass if the enum stopped round-tripping into
    // `Option<RecentRelation>` — which is the property the store's typed binding is
    // supposed to guarantee.
    let row: (String, String, Option<RecentRelation>) = sqlx::query_as(
        "SELECT event_id, event_type, relation FROM recent_events \
          WHERE workspace_id = $1 AND event_id = $2",
    )
    .bind(ws_id)
    .bind(&event_id)
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert_eq!(row.0, event_id);
    assert_eq!(row.1, "relation_resolved");
    assert_eq!(row.2, Some(RecentRelation::SameDeployer));
}


// ---------------------------------------------------------------------------
// REV-067-F06 — the disposition contract is enforced at every boundary
// ---------------------------------------------------------------------------

// The contract in `models.rs` says `skip` is "excluded from deep-sync/scoring" and
// only `score` contributes alpha. Three production boundaries read no policy at all,
// so it was documentation. Each is asserted at ITS OWN boundary, on live data.
#[tokio::test]
async fn scoring_is_refused_for_every_wallet_the_policy_excludes() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g67f06s")).await;
    let stem = tag("G67F06");
    let config = crate::config::ScoringConfig::default();
    let as_of = Utc::now();

    // A `score` wallet is scored; the three excluded dispositions are not.
    for (disposition, may_score) in [
        ("score", true),
        ("watch", false),
        ("flow_only", false),
        ("skip", false),
    ] {
        let address = format!("{stem}{}", disposition.to_uppercase());
        seed_label_with(&pool, ws, &address, "manual_policy", disposition, true).await;
        let scored = crate::workers::score_wallet(
            &pool, ws, ChainKind::Solana, &address, as_of, &config,
        )
        .await
        .expect("score_wallet");
        assert_eq!(
            scored.is_some(),
            may_score,
            "disposition `{disposition}` must {} scoring (REV-067-F06)",
            if may_score { "permit" } else { "refuse" }
        );

        // And nothing may be persisted for an excluded wallet: a stored row is a
        // contribution regardless of who reads it.
        let rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM wallet_scores WHERE chain = 'solana' AND address = $1",
        )
        .bind(&address)
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(
            rows > 0,
            may_score,
            "an excluded wallet must leave no score row behind"
        );
    }

    // An unknown disposition is a schema bug and must SURFACE, never silently
    // downgrade to `watch` the way `scoring_disposition` used to.
    let bogus = format!("{stem}BOGUS");
    seed_label_with(&pool, ws, &bogus, "auto_weird", "teleport", false).await;
    let err = crate::workers::score_wallet(
        &pool, ws, ChainKind::Solana, &bogus, as_of, &config,
    )
    .await
    .expect_err("an unknown disposition must be an error");
    assert!(
        format!("{err:#}").contains("teleport"),
        "the error must name the unknown disposition; got {err:#}"
    );
}

// REV-069-F06. The reviewer's exact CLI fixture, through the REAL
// `evaluate_token_signals`, not through the eligibility helper.
//
//   1 wallet `score`: skill/copyability 99/99, but a $0.50 trade
//   2 wallets `skip`: two distinct clusters, $100 trades each
//   healthy token/market: liquidity $30,000
//
// REV-068 filtered only the wallet-score lookup, so the two `skip` wallets still
// supplied `eligible_clusters=2` and `meaningful_buys=2` — the two hard entry gates —
// and the signal was ACCEPTED. Testing the helper alone could not see this, because
// the helper was correct and the gates around it were not.
#[tokio::test]
async fn excluded_wallets_cannot_supply_cluster_or_buy_alpha() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g69f06")).await;
    let stem = tag("G69F06");
    let mint = format!("{stem}MINT");
    let config = crate::config::SignalsConfig::default();
    let now = Utc::now();

    seed_tradable_token(&pool, &mint, now).await;

    // The only policy-eligible buyer trades below the meaningful threshold.
    let scorer = format!("{stem}SCORE");
    seed_label_with(&pool, ws, &scorer, "manual_policy", "score", true).await;
    seed_buy(&pool, &scorer, &mint, "0.50", now).await;
    seed_wallet_score(&pool, &scorer, 99, 99, now).await;

    // Two excluded wallets, each its own cluster, each a meaningful buy.
    for i in 0..2 {
        let skipper = format!("{stem}SKIP{i}");
        seed_label_with(&pool, ws, &skipper, "manual_block", "skip", true).await;
        seed_buy(&pool, &skipper, &mint, "100", now).await;
        seed_cluster_member(&pool, &skipper, &format!("{stem}CLUSTER{i}")).await;
    }

    let signal = crate::workers::evaluate_token_signals(
        &pool, ws, ChainKind::Solana, &mint, now, &config,
    )
    .await
    .expect("evaluation");
    assert!(
        signal.is_none(),
        "two `skip` wallets must not supply the cluster and meaningful-buy alpha that \
         flips a rejection into an accepted signal (REV-069-F06)"
    );

    let signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE chain = 'solana' AND mint = $1",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count signals");
    assert_eq!(signals, 0, "no signal row may be created");

    // The recorded rejection must name the FIRST gate, proving the counts were
    // actually zero rather than the signal failing later for another reason.
    let rejection: Option<String> = sqlx::query_scalar(
        "SELECT rejection_code FROM signal_evaluations \
          WHERE chain = 'solana' AND mint = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("rejection");
    assert_eq!(
        rejection.as_deref(),
        Some("insufficient_clusters"),
        "the excluded wallets' clusters must not be counted at all"
    );

    // Re-label both excluded wallets `score` and the SAME data now passes the two
    // gates — so the refusal above was policy, not missing data.
    sqlx::query(
        "UPDATE wallet_labels SET disposition = 'score' \
          WHERE workspace_id = $1 AND address LIKE $2",
    )
    .bind(ws)
    .bind(format!("{stem}SKIP%"))
    .execute(&pool)
    .await
    .expect("relabel");
    let eligible = crate::workers::eligible_signal_buyers(&pool, ws, ChainKind::Solana, &mint)
        .await
        .expect("eligible buyers");
    assert_eq!(
        eligible.len(),
        3,
        "with every buyer `score`, all three must be eligible; otherwise the filter \
         is hiding data rather than applying policy"
    );
}

// An unknown disposition is a schema bug: the evaluation must STOP, not record a
// normal rejection. A rejection row is a policy answer, and a policy that could not
// be read has no answer to give.
#[tokio::test]
async fn an_unknown_disposition_stops_signal_evaluation_without_a_rejection_row() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g69f06u")).await;
    let stem = tag("G69F06U");
    let mint = format!("{stem}MINT");
    let now = Utc::now();

    seed_tradable_token(&pool, &mint, now).await;
    let buyer = format!("{stem}BOGUS");
    seed_label_with(&pool, ws, &buyer, "auto_weird", "teleport", false).await;
    seed_buy(&pool, &buyer, &mint, "100", now).await;

    let err = match crate::workers::evaluate_token_signals(
        &pool,
        ws,
        ChainKind::Solana,
        &mint,
        now,
        &crate::config::SignalsConfig::default(),
    )
    .await
    {
        Ok(v) => panic!("an unknown disposition must be an error; got {v:?}"),
        Err(err) => err,
    };
    assert!(
        format!("{err:#}").contains("teleport"),
        "the error must name the unknown disposition; got {err:#}"
    );

    let evaluations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signal_evaluations WHERE chain = 'solana' AND mint = $1",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count evaluations");
    assert_eq!(
        evaluations, 0,
        "an unreadable policy must not be recorded as a normal rejection"
    );
    let signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE chain = 'solana' AND mint = $1",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count signals");
    assert_eq!(signals, 0, "and no signal may be created");
}

/// A token whose lifecycle, age, liquidity, and market freshness all pass, so a
/// rejection can only come from the wallet/cluster gates under test.
async fn seed_tradable_token(pool: &PgPool, mint: &str, now: chrono::DateTime<Utc>) {
    // `swi_legacy.tokens` (resolved by the runtime search_path) is what
    // `evaluate_token_signals` reads, not the canonical `public.tokens`.
    sqlx::query(
        "INSERT INTO tokens (chain, mint, lifecycle_state, first_seen_at, risk_flags) \
         VALUES ('solana', $1, 'active', $2, '[]'::jsonb) ON CONFLICT DO NOTHING",
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
    .bind(format!("sig-{wallet}-{mint}"))
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

async fn seed_cluster_member(pool: &PgPool, address: &str, cluster: &str) {
    // `wallet_cluster_members.cluster_id` references `swi_legacy.wallet_clusters`,
    // whose only columns are `cluster_id` and `created_at` — the cluster name lives
    // nowhere, so each call mints a fresh cluster row.
    let _ = cluster;
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
}

// ---------------------------------------------------------------------------
// REV-067-F07 — a failed successful-login leaves no committed state
// ---------------------------------------------------------------------------

// The three successful-login writes were separately committed: a failure in the
// second or third returned 500 with no cookie while the earlier writes stayed. The
// store then described a login the client never received.
#[tokio::test]
async fn a_failed_successful_login_commits_nothing() {
    let password = configure_admin_auth();
    // The session INSERT is rejected, so the transaction must roll back the audit row
    // and the counter clear that ran before it.
    let denying = denying_pool("swi_rev067_f07d", "admin_sessions", "INSERT").await;
    let base = serve_admin(&denying).await;

    // A pre-existing failed attempt that `clear_attempts` would delete, plus the
    // success row `record_attempt` would insert: both must be absent/intact after.
    //
    // The rate-limit key is derived from the client IP, so the request carries an
    // `X-Forwarded-For` unique to this test. Counting `ip:unknown` would count rows
    // sibling login tests wrote for the same default key.
    let ip = format!("10.67.7.{}", (std::process::id() % 200) + 1);
    let key = format!("ip:{ip}");
    sqlx::query("INSERT INTO login_attempts (key, success, ip, user_agent) VALUES ($1, false, NULL, NULL)")
        .bind(&key)
        .execute(&denying)
        .await
        .expect("seed failed attempt");

    let count_rows = |pool: PgPool, key: String| async move {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT count(*) FILTER (WHERE NOT success), count(*) FILTER (WHERE success) \
               FROM login_attempts WHERE key = $1",
        )
        .bind(key)
        .fetch_one(&pool)
        .await
        .expect("count attempts")
    };
    let before = count_rows(denying.clone(), key.clone()).await;
    assert_eq!(before, (1, 0), "fixture: one failed attempt, no success rows");

    let response = reqwest::Client::new()
        .post(format!("{base}/login"))
        .header("x-forwarded-for", &ip)
        .form(&[("password", password)])
        .send()
        .await
        .expect("login request");
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a login whose session cannot be created must fail closed"
    );
    assert!(
        response.headers().get(reqwest::header::SET_COOKIE).is_none(),
        "no session cookie may be issued"
    );

    let after = count_rows(denying.clone(), key.clone()).await;
    assert_eq!(
        after, before,
        "the successful-login writes must be ONE transaction: a failed session \
         insert must leave neither a success audit row nor a cleared counter \
         (REV-067-F07)"
    );
}

// The auth error taxonomy must not be flattened at a single route: a validation-store
// failure is a 500 everywhere, and reporting it as 401 hides an outage behind an
// authentication error.
#[tokio::test]
async fn a_secret_write_reports_a_store_failure_as_500_not_401() {
    configure_admin_auth();
    let live = pool().await;
    let ws = workspace(&live, &tag("g67f07e")).await;
    let token = session_for(&live, ws).await;

    // `validate_session` UPDATEs `admin_sessions`; rejecting that is exactly the
    // store failure `require_auth` maps to 500.
    let denying = denying_pool("swi_rev067_f07e", "admin_sessions", "UPDATE").await;
    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, expires_at, workspace_id) \
         SELECT token_hash, expires_at, workspace_id FROM public.admin_sessions \
          WHERE workspace_id = $1",
    )
    .bind(ws)
    .execute(&denying)
    .await
    .expect("mirror session row");
    let base = serve_admin(&denying).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/api/settings/secrets"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .json(&serde_json::json!({ "name": "HELIUS_KEY_1", "value": "probe" }))
        .send()
        .await
        .expect("secret write");
    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "an unreachable session store must be a 500, never 401 (REV-067-F07)"
    );
}

// The deep-sync boundary. `models::Disposition` says `skip` is "excluded from
// deep-sync"; `sync_wallet` read no policy at all, so a `skip` wallet still fetched
// and persisted its whole history.
//
// The probe uses a provider pool with ZERO keys, which makes the observable
// difference unambiguous: a permitted wallet must REACH the provider (and fail there,
// "no providers configured"), while a `skip` wallet must return before any fetch. A
// test that only asserted "zero pages" could not tell refusal from an empty history.
#[tokio::test]
async fn deep_sync_is_refused_for_a_wallet_the_policy_excludes() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g67f06d")).await;
    let stem = tag("G67F06D");
    let helius = crate::helius::HeliusPool::new(vec![], crate::config::HeliusConfig::default());
    assert_eq!(helius.provider_count(), 0, "the probe needs an empty provider pool");
    let adapter = crate::chains::SolanaAdapter::new();

    let skipped_addr = format!("{stem}SKIP");
    seed_label_with(&pool, ws, &skipped_addr, "manual_block", "skip", true).await;
    let outcome = crate::ingest::sync_wallet(
        &pool, ws, &helius, ChainKind::Solana, &skipped_addr, &adapter, 3,
    )
    .await
    .expect("a policy refusal is not an error");
    assert!(
        outcome.skipped_by_policy && outcome.pages_fetched == 0,
        "a `skip` wallet must not be deep-synced (REV-067-F06)"
    );

    // A `score` wallet is allowed through, so it reaches the provider and fails
    // there — proving the gate opened rather than the fetch being unreachable.
    let allowed_addr = format!("{stem}SCORE");
    seed_label_with(&pool, ws, &allowed_addr, "manual_policy", "score", true).await;
    let err = match crate::ingest::sync_wallet(
        &pool, ws, &helius, ChainKind::Solana, &allowed_addr, &adapter, 3,
    )
    .await
    {
        Ok(o) => panic!(
            "a permitted wallet must reach the provider and fail there; got \
             skipped_by_policy={} pages={}",
            o.skipped_by_policy, o.pages_fetched
        ),
        Err(err) => err,
    };
    assert!(
        format!("{err:#}").contains("provider"),
        "the permitted path must fail at the PROVIDER, not at the policy gate; got {err:#}"
    );
}
