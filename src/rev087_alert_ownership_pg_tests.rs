//! Live PostgreSQL regressions for REV-086-F02, F04 and F06.
//!
//! * F02 — the signal drain discarded `mark_alert_failed() == Ok(false)`, so a
//!   completion that matched no row (our lease was lost mid-send) was silent.
//! * F04 — `alerts.workspace_id` and `alerts.funding_case_id` were independent
//!   FKs, so a cross-tenant pairing was insertable and only a reader-side join
//!   predicate stood between it and a cross-tenant delivery.
//! * F06 — the exit writer was public and unfenced.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use sqlx::PgPool;

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

/// An in-memory log sink for asserting that an incident was actually reported.
///
/// `Arc<Mutex<Vec<u8>>>` does not satisfy `MakeWriter` (the blanket `Arc<W>` impl
/// needs `&W: io::Write`, and `&Mutex<Vec<u8>>` is not), so the handle is wrapped
/// in a local newtype that locks per write.
#[derive(Clone)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn workspace(pool: &PgPool, slug: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ($1, $2) RETURNING id")
        .bind(slug)
        .bind(slug)
        .fetch_one(pool)
        .await
        .expect("create workspace")
}

async fn funding_case(pool: &PgPool, ws: i64) -> (i64, String) {
    let recipient = tag("R87OWNRECIP");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(pool)
    .await
    .expect("wallet");
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, \
              first_funding_native, source_address, deploy_window_ends_at, stage, \
              confidence, evidence) \
         VALUES ('solana', $1, $2, now(), '100', '0.5', $3, now() + interval '1 day', \
                 'funded', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(ws)
    .bind(tag("R87OWNSRC"))
    .fetch_one(pool)
    .await
    .expect("funding case");
    (id, recipient)
}

fn worker_ctx(
    pool: &PgPool,
    workspace_id: i64,
    telegram: Option<(&str, &str)>,
) -> crate::workers::WorkerContext {
    let settings = crate::config::Settings {
        config: crate::config::AppConfig::default(),
        env: crate::config::EnvConfig::load(),
    };
    let workspace =
        solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope::from_job_context(workspace_id)
            .expect("workspace");
    let mut ctx = crate::workers::WorkerContext::new(pool.clone(), &settings, workspace);
    if let Some((token, chat)) = telegram {
        ctx.telegram_bot_token = Some(token.to_string());
        ctx.telegram_chat_id = Some(chat.to_string());
    }
    ctx
}

// ---------------------------------------------------------------------------
// REV-086-F04 — ownership is refused at the boundary AND by the schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cross_tenant_funding_alert_is_refused_at_the_claim_boundary() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("r87owna")).await;
    let ws_b = workspace(&pool, &tag("r87ownb")).await;
    let (case_a, _recipient) = funding_case(&pool, ws_a).await;

    // Assert the VALID case FIRST. Without this the refusal below could pass
    // vacuously — e.g. if claim_alert were broken for every input.
    let ok = crate::signals::claim_alert(&pool, "funding", ws_a, case_a, "chat-a")
        .await
        .expect("the owning workspace may claim its own case");
    assert!(ok.is_some(), "the legitimate claim must win");

    // `AlertClaim` is not `Debug`, so match rather than `expect_err`.
    let msg = match crate::signals::claim_alert(&pool, "funding", ws_b, case_a, "chat-a").await {
        Ok(_) => panic!("a workspace must not claim another workspace's case"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        msg.contains("is not owned by workspace"),
        "expected a named ownership refusal, got: {msg}"
    );

    // No row was minted for the cross-tenant pairing.
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alerts WHERE workspace_id = $1 AND funding_case_id = $2",
    )
    .bind(ws_b)
    .bind(case_a)
    .fetch_one(&pool)
    .await
    .expect("leak probe");
    assert_eq!(leaked, 0, "the refused claim wrote nothing");
}

/// The Rust guard is a boundary check; the constraint is the invariant. A writer
/// that bypasses `claim_alert` must still be unable to create the row.
#[tokio::test]
async fn the_composite_ownership_constraint_rejects_a_mismatched_insert() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("r87fka")).await;
    let ws_b = workspace(&pool, &tag("r87fkb")).await;
    let (case_a, _recipient) = funding_case(&pool, ws_a).await;

    // Precondition: the constraint 1040 installs actually exists on this database.
    let present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint \
          WHERE conname = 'alerts_funding_case_workspace_fk')",
    )
    .fetch_one(&pool)
    .await
    .expect("constraint probe");
    assert!(present, "migration 1040 must have installed the composite FK");

    // The matching pair inserts fine — so a failure below is about OWNERSHIP, not
    // about the statement being malformed.
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, funding_case_id, \
                             destination, state, attempt_count, next_attempt_at) \
         VALUES ($1, 'funding', $2, $3, 'chat-ok', 'pending', 1, now())",
    )
    .bind(crate::signals::alert_dedup_key("funding", ws_a, case_a, "chat-ok"))
    .bind(ws_a)
    .bind(case_a)
    .execute(&pool)
    .await
    .expect("the owning workspace's row is valid");

    let err = sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, funding_case_id, \
                             destination, state, attempt_count, next_attempt_at) \
         VALUES ($1, 'funding', $2, $3, 'chat-bad', 'pending', 1, now())",
    )
    .bind(crate::signals::alert_dedup_key("funding", ws_b, case_a, "chat-bad"))
    .bind(ws_b)
    .bind(case_a)
    .execute(&pool)
    .await
    .expect_err("a cross-tenant pairing must violate the composite FK");
    let db_err = err.as_database_error().expect("a database error");
    assert_eq!(
        db_err.code().as_deref(),
        Some("23503"),
        "expected foreign_key_violation, got {db_err:?}"
    );
    assert!(
        db_err.constraint() == Some("alerts_funding_case_workspace_fk"),
        "the composite ownership FK must be the constraint that rejected it: {db_err:?}"
    );
}

// ---------------------------------------------------------------------------
// REV-086-F02 — a stale signal-drain failure is observed, never silent
// ---------------------------------------------------------------------------

/// The dispatcher claims a delivery, the send fails, and while it was in flight
/// another dispatcher took the lease over. `mark_alert_failed` then matches zero
/// rows and returns `false`. That `false` used to be dropped on the floor.
///
/// The lease is stolen IN BAND by the mock Bot API handler, so the race is real
/// rather than simulated after the fact.
#[tokio::test]
async fn a_stale_signal_drain_failure_is_observed_and_not_counted() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("r87f02")).await;
    let mint = tag("R87F02MINT");
    let signal_id: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status, evidence) \
         VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active', '{}'::jsonb) RETURNING id",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("signal");

    let key = crate::signals::alert_dedup_key("signal", ws, signal_id, "chat-a");
    let steal_pool = pool.clone();
    let steal_key = key.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route(
        "/bot{token}/sendMessage",
        axum::routing::post(move || {
            let pool = steal_pool.clone();
            let key = steal_key.clone();
            async move {
                // A newer dispatcher takes the row while our send is in flight.
                sqlx::query(
                    "UPDATE alerts SET claim_token = 'r87-other-dispatcher', \
                            claim_expires_at = now() + interval '2 minutes' \
                      WHERE dedup_key = $1",
                )
                .bind(&key)
                .execute(&pool)
                .await
                .expect("steal the lease");
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(SharedBuf(buffer.clone()))
        .with_ansi(false)
        .finish();
    let ctx = worker_ctx(&pool, ws, Some(("test-token", "chat-a")));
    let sent = {
        use tracing::instrument::WithSubscriber;
        crate::workers::dispatch_alerts_via(&ctx, &format!("http://{addr}"))
            .with_subscriber(subscriber)
            .await
            .expect("dispatch must not error on a stale completion")
    };
    assert_eq!(sent, 0, "a failed send is never counted as delivered");

    let logged = String::from_utf8(buffer.lock().expect("log buffer").clone()).expect("utf8");
    assert!(
        logged.contains("signal alert failure update matched no row (stale claim)"),
        "the stale completion must be observable; captured log was: {logged}"
    );

    // The newer claimant still owns the row: our stale write changed nothing.
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT claim_token FROM alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("owner");
    assert_eq!(
        owner.as_deref(),
        Some("r87-other-dispatcher"),
        "the stale dispatcher must not have cleared the live claim"
    );
}

// ---------------------------------------------------------------------------
// REV-086-F06 — the exit writer is fenced by a durable claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_claim_token_cannot_write_an_exit_signal() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("r87f06")).await;
    let mint = tag("R87F06MINT");
    let chain = crate::models::ChainKind::Solana;
    let config = crate::config::SignalsConfig {
        entry_max_token_age_hours: 24,
        entry_min_liquidity_usd: 20_000,
        market_max_age_seconds: 300,
        exit_liquidity_drop_ratio: 0.30,
    };
    // Two selling clusters trigger the exit gate, so a write is genuinely attempted
    // and the refusal cannot come from the gate being closed.
    let gates = crate::signals::ExitGates {
        clusters_selling: 2,
        ..Default::default()
    };

    sqlx::query(
        "INSERT INTO signal_eval_claims (workspace_id, chain, mint, claimed_by, expires_at) \
         VALUES ($1, $2, $3, 'r87-owner-A', now() + interval '10 minutes')",
    )
    .bind(ws)
    .bind(chain.as_str())
    .bind(&mint)
    .execute(&pool)
    .await
    .expect("claim held by A");

    let err = crate::signals::evaluate_exit_fenced(
        &pool, ws, chain, &mint, chrono::Utc::now(), &config, &gates, "r87-stale-B",
    )
    .await
    .expect_err("a stale token must not write");
    assert!(
        format!("{err:#}").contains("evaluation claim not held"),
        "expected the fencing refusal, got: {err:#}"
    );
    let written: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM signals WHERE workspace_id = $1 AND mint = $2 AND signal_kind = 'exit'",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("signal count");
    assert_eq!(written, 0, "the refused evaluation wrote nothing");

    // The live owner writes, and its claim is released in the same transaction.
    let id = crate::signals::evaluate_exit_fenced(
        &pool, ws, chain, &mint, chrono::Utc::now(), &config, &gates, "r87-owner-A",
    )
    .await
    .expect("the claim holder may write");
    assert!(id.is_some(), "the exit gate triggered, so a signal must exist");
    let still_claimed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM signal_eval_claims WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("claim count");
    assert_eq!(still_claimed, 0, "completed work leaves no live claim");
}
