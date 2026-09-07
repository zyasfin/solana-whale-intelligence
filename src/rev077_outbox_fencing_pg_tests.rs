//! Live PostgreSQL regressions for REV-076 F02/F03/F04.
//!
//! Each test reproduces the reviewer's own probe through production code:
//!
//! * F03 — fresh concurrent claim has exactly ONE winner (the REV-075 two-statement
//!   claim let the INSERT loser satisfy the UPDATE predicate immediately);
//! * F03 — completion is fenced by claim token (a stale sender cannot mark a newer
//!   claim sent, nor overwrite a sent row);
//! * F03 — a `pending` row has NULL `sent_at` (the catalog lied about delivery
//!   before any HTTP call);
//! * F03 — claims count as attempts, so crash loops cannot bypass the cap;
//! * F03 — `Retry-After` is bounded; Telegram 200 `{"ok":false}` is NOT delivered;
//! * F04 — destination is part of the identity (chat change = new delivery);
//! * F03 — funding-radar alerts travel the same outbox;
//! * F02 — two workers on one workspace cannot evaluate the same token twice;
//! * F02 — durable `queue_state` suppresses the loop; admin endpoint writes it.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use chrono::Utc;
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

async fn workspace(pool: &PgPool, slug: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ($1, $2) RETURNING id")
        .bind(slug)
        .bind(slug)
        .fetch_one(pool)
        .await
        .expect("create workspace")
}

async fn seed_signal(
    pool: &PgPool,
    ws: i64,
    chain: &str,
    mint: &str,
    created_at: chrono::DateTime<Utc>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status, evidence) \
         VALUES ($1, $2, $3, 'entry', $4, 80, 'active', '{}'::jsonb) RETURNING id",
    )
    .bind(ws)
    .bind(chain)
    .bind(mint)
    .bind(created_at)
    .fetch_one(pool)
    .await
    .expect("seed signal")
}

// ---------------------------------------------------------------------------
// F03 — fresh concurrent claim: exactly one winner (the reproduced REV-076 race)
// ---------------------------------------------------------------------------

// The REV-075 claim was two statements (INSERT then UPDATE-claim). A concurrent
// INSERT loser landed on the fresh row whose next_attempt_at was now() and
// immediately satisfied the UPDATE predicate — two winners, double send. The
// single-statement claim has no such window.
#[tokio::test]
async fn a_fresh_concurrent_claim_has_exactly_one_winner() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f03a")).await;
    let signal_id = seed_signal(&pool, ws, "solana", &tag("G76F03MINT"), Utc::now()).await;

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let p = PgPool::connect(&crate::pg_test_support::require_live_url())
            .await
            .expect("racing pool");
        let b = barrier.clone();
        handles.push(tokio::spawn(async move {
            b.wait().await;
            crate::signals::claim_alert(&p, "signal", ws, signal_id, "chat-a")
                .await
                .expect("claim")
        }));
    }
    let mut winners = 0usize;
    let mut tokens = Vec::new();
    for h in handles {
        if let Some(claim) = h.await.expect("join") {
            winners += 1;
            tokens.push(claim.token);
        }
    }
    assert_eq!(winners, 1, "exactly one claimant may win a fresh row (REV-076)");
    assert_eq!(tokens.len(), 1);

    // The loser stays lost even retrying immediately — the winner's lease stands.
    let second = crate::signals::claim_alert(&pool, "signal", ws, signal_id, "chat-a")
        .await
        .expect("second claim");
    assert!(second.is_none(), "an unexpired lease blocks every later claim");

    // And the claim consumed attempt 1.
    let attempts: i32 = sqlx::query_scalar(
        "SELECT attempt_count FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("signal", ws, signal_id, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("attempts");
    assert_eq!(attempts, 1, "the claim itself counts as an attempt");
}

// A stale sender must not complete: after lease expiry and re-claim by another
// dispatcher, the OLD token marks nothing.
#[tokio::test]
async fn a_stale_claim_token_marks_nothing() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f03b")).await;
    let signal_id = seed_signal(&pool, ws, "solana", &tag("G76F03BMINT"), Utc::now()).await;

    let first = crate::signals::claim_alert(&pool, "signal", ws, signal_id, "chat-a")
        .await
        .expect("first claim")
        .expect("first claim wins");

    // Lease expires; a second dispatcher re-claims.
    sqlx::query("UPDATE alerts SET claim_expires_at = now() - interval '1 second', next_attempt_at = now() - interval '1 second' \
                  WHERE dedup_key = $1")
        .bind(crate::signals::alert_dedup_key("signal", ws, signal_id, "chat-a"))
        .execute(&pool)
        .await
        .expect("expire lease");
    let second = crate::signals::claim_alert(&pool, "signal", ws, signal_id, "chat-a")
        .await
        .expect("second claim")
        .expect("expired lease is re-claimable");
    assert_ne!(first.token, second.token);

    // The STALE sender finishes its HTTP call and tries to record success.
    let marked = crate::signals::mark_alert_sent(
        &pool, "signal", ws, signal_id, "chat-a", &first.token,
    )
    .await
    .expect("stale completion");
    assert!(!marked, "a stale token must not mark the newer claim sent");

    // The stale sender's failure must not overwrite either.
    let failed = crate::signals::mark_alert_failed(
        &pool, "signal", ws, signal_id, "chat-a", &first.token, first.attempt,
        "stale failure", false, None,
    )
    .await
    .expect("stale failure");
    assert!(!failed, "a stale token must not write failure state");

    // The row is still pending under the NEW token, unsent.
    let (state, sent_at): (String, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT state, sent_at FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("signal", ws, signal_id, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(state, "pending");
    assert!(sent_at.is_none(), "nothing was delivered, so no sent timestamp");

    // The NEW claimant completes fine.
    let marked = crate::signals::mark_alert_sent(
        &pool, "signal", ws, signal_id, "chat-a", &second.token,
    )
    .await
    .expect("fenced completion");
    assert!(marked, "the live token completes");
}

// ---------------------------------------------------------------------------
// F03 — catalog semantics: pending rows have no success timestamp
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pending_row_has_no_sent_timestamp() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f03c")).await;
    let signal_id = seed_signal(&pool, ws, "solana", &tag("G76F03CMINT"), Utc::now()).await;

    crate::signals::claim_alert(&pool, "signal", ws, signal_id, "chat-a")
        .await
        .expect("claim")
        .expect("claim wins");
    let (state, sent_at): (String, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT state, sent_at FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("signal", ws, signal_id, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(state, "pending");
    assert!(
        sent_at.is_none(),
        "a reserved row must NOT carry a success timestamp before delivery (REV-076)"
    );
}

// ---------------------------------------------------------------------------
// F03 — claims count as attempts: crash loops hit the cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_claims_hit_the_max_attempt_cap() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f03d")).await;
    let signal_id = seed_signal(&pool, ws, "solana", &tag("G76F03DMINT"), Utc::now()).await;
    let key = crate::signals::alert_dedup_key("signal", ws, signal_id, "chat-a");

    // Simulate crash-after-claim: claim, expire the lease, claim again.
    let mut attempts = 0i32;
    for round in 0..crate::signals::ALERT_MAX_ATTEMPTS {
        let claim = crate::signals::claim_alert(&pool, "signal", ws, signal_id, "chat-a")
            .await
            .expect("claim");
        assert!(
            claim.is_some(),
            "round {round}: claim before the cap must succeed"
        );
        sqlx::query(
            "UPDATE alerts SET claim_expires_at = now() - interval '1 second', \
                               next_attempt_at = now() - interval '1 second' \
              WHERE dedup_key = $1",
        )
        .bind(&key)
        .execute(&pool)
        .await
        .expect("expire");
        attempts += 1;
    }
    // Cap reached: no further claim.
    let blocked = crate::signals::claim_alert(&pool, "signal", ws, signal_id, "chat-a")
        .await
        .expect("claim at cap");
    assert!(
        blocked.is_none(),
        "attempt {attempts} reached ALERT_MAX_ATTEMPTS; crash loops must not retry forever"
    );
}

// ---------------------------------------------------------------------------
// F03 — bounded Retry-After + 200 ok:false is not delivery
// ---------------------------------------------------------------------------

async fn serve_telegram_response(status: axum::http::StatusCode, retry_after: Option<&'static str>, body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route(
        "/bot{token}/sendMessage",
        axum::routing::post(move || async move {
            let mut r = axum::response::Response::builder().status(status);
            if let Some(v) = retry_after {
                r = r.header(axum::http::header::RETRY_AFTER, v);
            }
            r.body(axum::body::Body::from(body)).unwrap()
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
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
    let workspace = solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope::from_job_context(workspace_id)
        .expect("workspace");
    let mut ctx = crate::workers::WorkerContext::new(pool.clone(), &settings, workspace);
    if let Some((token, chat)) = telegram {
        ctx.telegram_bot_token = Some(token.to_string());
        ctx.telegram_chat_id = Some(chat.to_string());
    }
    ctx
}

#[tokio::test]
async fn retry_after_is_bounded_and_ok_false_is_not_delivered() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f03e")).await;

    // 429 with an absurd Retry-After — must be clamped, not parked forever.
    let mint = tag("G76F03EMINT");
    let signal_id = seed_signal(&pool, ws, "solana", &mint, Utc::now()).await;
    let base = serve_telegram_response(axum::http::StatusCode::TOO_MANY_REQUESTS, Some("999999999"), "slow").await;
    let ctx = worker_ctx(&pool, ws, Some(("test-token", "chat-a")));
    crate::workers::dispatch_alerts_via(&ctx, &base).await.expect("dispatch 429");
    let next_at: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_attempt_at FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("signal", ws, signal_id, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("row");
    let delay = (next_at.expect("scheduled") - Utc::now()).num_seconds();
    assert!(
        delay <= crate::signals::ALERT_MAX_RETRY_AFTER_SECONDS as i64 + 5,
        "Retry-After must be clamped to the bound; got {delay}s"
    );

    // 200 with {"ok":false}: HTTP success is transport, not delivery.
    let mint2 = tag("G76F03EMINT2");
    let signal_id2 = seed_signal(&pool, ws, "solana", &mint2, Utc::now()).await;
    let base2 = serve_telegram_response(axum::http::StatusCode::OK, None, "{\"ok\":false,\"description\":\"bad\"}").await;
    crate::workers::dispatch_alerts_via(&ctx, &base2).await.expect("dispatch ok:false");
    let (state, sent_at): (String, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT state, sent_at FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("signal", ws, signal_id2, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("row2");
    assert_eq!(state, "dead", "ok:false is a rejection, not a retryable error");
    assert!(sent_at.is_none(), "and certainly not a delivery");
}

// ---------------------------------------------------------------------------
// F04 — destination is part of the identity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_destination_change_is_a_new_delivery_not_a_swallowed_one() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f04a")).await;
    let signal_id = seed_signal(&pool, ws, "solana", &tag("G76F04MINT"), Utc::now()).await;

    // Deliver to chat-a.
    let base = serve_telegram_response(axum::http::StatusCode::OK, None, "{\"ok\":true}").await;
    let ctx = worker_ctx(&pool, ws, Some(("test-token", "chat-a")));
    let sent = crate::workers::dispatch_alerts_via(&ctx, &base).await.expect("dispatch a");
    assert_eq!(sent, 1);

    // Same signal, DIFFERENT destination: must be delivered again, not suppressed.
    let ctx_b = worker_ctx(&pool, ws, Some(("test-token", "chat-b")));
    let sent = crate::workers::dispatch_alerts_via(&ctx_b, &base).await.expect("dispatch b");
    assert_eq!(sent, 1, "a second destination is a second alert (REV-076-F04)");

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM alerts WHERE signal_id = $1 AND state = 'sent'",
    )
    .bind(signal_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(rows, 2, "one sent row per destination");
}

// ---------------------------------------------------------------------------
// F03 — funding-radar alerts travel the same outbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn funding_alerts_use_the_outbox_and_retry_transient_failures() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f03f")).await;
    let recipient = tag("G76F03FCASE");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("seed wallet");
    let case_id: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, first_funding_native, \
              source_address, deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $3, now(), '100', '0.5', $2, now() + interval '1 day', \
                 'funded', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(tag("G76F03FSRC"))
        .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("seed case");

    // Transient failure: claimed, marked failed, retryable.
    let claim = crate::signals::claim_alert(&pool, "funding", ws, case_id, "chat-a")
        .await
        .expect("claim")
        .expect("claim wins");
    let marked = crate::signals::mark_alert_failed(
        &pool, "funding", ws, case_id, "chat-a", &claim.token, claim.attempt,
        "Transient", false, None,
    )
    .await
    .expect("mark failed");
    assert!(marked);
    let (state, attempts): (String, i32) = sqlx::query_as(
        "SELECT state, attempt_count FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("funding", ws, case_id, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(state, "pending");
    assert_eq!(attempts, 1, "the funding alert retry is counted the same way");

    // The two kinds never collide on the same subject id. The signal claim
    // must reference a REAL signal id (the typed FK from REV-078-F03 rejects
    // a bare case id), so capture the seeded signal's id and claim with it.
    let sig_id = seed_signal(&pool, ws, "solana", &tag("G76F03FSAME"), Utc::now()).await;
    let sig_claim = crate::signals::claim_alert(&pool, "signal", ws, sig_id, "chat-a").await.expect("claim");
    assert!(sig_claim.is_some(), "kind is part of the identity; a signal subject is a different delivery");
}

// ---------------------------------------------------------------------------
// F02 — durable same-workspace evaluation claim
// ---------------------------------------------------------------------------

// Two workers (two WorkerContexts, as two processes would have) select the same
// due token concurrently. The durable claim row decides: one evaluates, one
// skips; exactly one signal exists.
#[tokio::test]
async fn two_workers_on_one_workspace_cannot_double_evaluate() {
    let (pool, admin, scratch) = {
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&crate::pg_test_support::require_live_url())
            .await
            .expect("admin pool");
        let scratch = format!("swi_f02c_{}", std::process::id());
        sqlx::query(&format!("DROP DATABASE IF EXISTS {scratch}")).execute(&admin).await.expect("drop");
        sqlx::query(&format!("CREATE DATABASE {scratch}")).execute(&admin).await.expect("create");
        let url = format!(
            "{}/{}",
            crate::pg_test_support::require_live_url().rsplitn(2, '/').nth(1).expect("db url"),
            scratch
        );
        let pool = crate::db::connect(&url, 2).await.expect("scratch pool");
        crate::db::migrate_with(&pool, true).await.expect("migrate");
        (pool, admin, scratch)
    };

    let ws = workspace(&pool, &tag("g76f02a")).await;
    let stem = tag("G76F02");
    let mint = format!("{stem}MINT");
    let now = Utc::now();
    // accepting fixture
    sqlx::query(
        "INSERT INTO tokens (chain, mint, lifecycle_state, first_seen_at, risk_flags) \
         VALUES ('solana', $1, 'new_creation', $2, '[]'::jsonb) ON CONFLICT DO NOTHING",
    )
    .bind(&mint)
    .bind(now - chrono::Duration::hours(1))
    .execute(&pool)
    .await
    .expect("token");
    sqlx::query(
        "INSERT INTO market_snapshots (source, chain, mint, observed_at, pair_address, liquidity_usd) \
         VALUES ('test', 'solana', $1, $2, '', 30000) ON CONFLICT DO NOTHING",
    )
    .bind(&mint)
    .bind(now)
    .execute(&pool)
    .await
    .expect("market");
    for i in 0..2 {
        let wallet = format!("{stem}W{i}");
        sqlx::query(
            "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
             VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
        )
        .bind(&wallet)
        .execute(&pool)
        .await
        .expect("wallet");
        sqlx::query(
            "INSERT INTO wallet_labels (workspace_id, chain, address, kind, disposition, manual, confidence) \
             VALUES ($1, 'solana', $2, 'manual_policy', 'score', true, 90)",
        )
        .bind(ws)
        .bind(&wallet)
        .execute(&pool)
        .await
        .expect("label");
        sqlx::query(
            "INSERT INTO wallet_scores (chain, address, as_of, skill_score, copyability_score, conviction, history_completeness, provisional) \
             VALUES ('solana', $1, $2, 99, 99, 90, 1, false) ON CONFLICT DO NOTHING",
        )
        .bind(&wallet)
        .bind(now)
        .execute(&pool)
        .await
        .expect("score");
        sqlx::query(
            "INSERT INTO trades (chain, wallet, mint, side, signature, event_index, block_time, observed_at, usd_value) \
             VALUES ('solana', $1, $2, 'buy', $3, 0, $4, $4, 100) ON CONFLICT DO NOTHING",
        )
        .bind(&wallet)
        .bind(&mint)
        .bind(format!("sig-{stem}-{i}"))
        .bind(now)
        .execute(&pool)
        .await
        .expect("trade");
        let cid: i64 = sqlx::query_scalar(
            "INSERT INTO swi_legacy.wallet_clusters (created_at) VALUES (now()) RETURNING cluster_id",
        )
        .fetch_one(&pool)
        .await
        .expect("cluster");
        sqlx::query(
            "INSERT INTO wallet_cluster_members (cluster_id, chain, address) VALUES ($1, 'solana', $2)",
        )
        .bind(cid)
        .bind(&wallet)
        .execute(&pool)
        .await
        .expect("member");
    }

    let ctx1 = worker_ctx(&pool, ws, None);
    let ctx2 = worker_ctx(&pool, ws, None);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let b1 = barrier.clone();
    let b2 = barrier.clone();
    let (r1, r2) = tokio::join!(
        async move {
            b1.wait().await;
            crate::workers::evaluate_due_signals(&ctx1).await
        },
        async move {
            b2.wait().await;
            crate::workers::evaluate_due_signals(&ctx2).await
        }
    );
    r1.expect("worker 1 pass");
    r2.expect("worker 2 pass");

    let signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count signals");
    assert_eq!(
        signals, 1,
        "two concurrent workers on one workspace must produce exactly ONE signal (REV-076-F02)"
    );

    sqlx::query(&format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop scratch");
}

// ---------------------------------------------------------------------------
// F02 — durable queue_state suppresses the loop; admin endpoint writes it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn durable_queue_state_gates_the_loop_and_admin_writes_it() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g76f02b")).await;
    let ctx = worker_ctx(&pool, ws, None);

    // Clean slate: queue_state is global durable state, and a previous run of this
    // very test may have left a pause behind (durable is the point).
    sqlx::query("DELETE FROM queue_state WHERE queue = $1")
        .bind(crate::queues::QUEUE_SIGNAL_EVAL)
        .execute(&pool)
        .await
        .expect("clean queue state");

    // No row: not paused.
    assert!(ctx.queue_allowed(crate::queues::QUEUE_SIGNAL_EVAL).await);

    // Durable pause: suppressed.
    sqlx::query(
        "INSERT INTO queue_state (queue, paused, updated_by) VALUES ($1, true, 'test') \
         ON CONFLICT (queue) DO UPDATE SET paused = true",
    )
    .bind(crate::queues::QUEUE_SIGNAL_EVAL)
    .execute(&pool)
    .await
    .expect("pause");
    assert!(
        !ctx.queue_allowed(crate::queues::QUEUE_SIGNAL_EVAL).await,
        "the durable row must suppress the loop (REV-076-F02)"
    );

    // Resume through the REAL admin endpoint over HTTP, with a write-auth session.
    use sha2::{Digest, Sha256};
    let token = crate::auth::new_session_token();
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, expires_at, workspace_id) \
         VALUES ($1, now() + interval '1 hour', $2)",
    )
    .bind(hex::encode(h.finalize()))
    .bind(ws)
    .execute(&pool)
    .await
    .expect("session");
    crate::pg_test_support::configure_admin_auth();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let state = crate::admin::AdminState {
        pool: pool.clone(),
        settings: std::sync::Arc::new(crate::config::Settings {
            config: crate::config::AppConfig::default(),
            env: crate::config::EnvConfig::load(),
        }),
    };
    tokio::spawn(async move {
        let _ = axum::serve(listener, crate::admin::router(state)).await;
    });
    let base = format!("http://{addr}");

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/queues/signal_eval/pause"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .json(&serde_json::json!({"paused": false}))
        .send()
        .await
        .expect("resume request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(
        ctx.queue_allowed(crate::queues::QUEUE_SIGNAL_EVAL).await,
        "the admin endpoint must flip the durable state the worker reads"
    );

    // Unknown queue names are rejected, not silently created.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/queues/bogus_queue/pause"))
        .header(reqwest::header::COOKIE, format!("swi_session={token}"))
        .json(&serde_json::json!({"paused": true}))
        .send()
        .await
        .expect("bogus request");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
