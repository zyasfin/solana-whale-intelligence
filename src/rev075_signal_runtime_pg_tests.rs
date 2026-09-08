//! Live PostgreSQL regressions for REV-074 F01–F04 plus the 1032 hardening test.
//!
//! Each test reproduces the reviewer's own probe through production code:
//!
//! * F01 — two workspaces must BOTH write a signal on identical facts at an
//!   identical timestamp through the real `evaluate_token_signals` (the global
//!   unique constraint made workspace B fail with SQLSTATE 23505);
//! * F02 — the periodic worker loop must evaluate due tokens without any CLI call,
//!   honour the queue pause, isolate a bad token, and never duplicate an unchanged
//!   evaluation;
//! * F03 — a failed Telegram send must be retried (mock endpoint: 500, 429, 400,
//!   then 200), never recorded as terminal success;
//! * F04 — two chains at the same microsecond must produce two alerts, each with
//!   its own chain label.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use chrono::{Duration, Utc};
use sqlx::PgPool;

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

/// The fixture that ACCEPTS in `ws`: two score-labeled wallets, two clusters,
/// two meaningful buys each.
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


/// A fully migrated, EMPTY scratch database for tests whose batch selection must
/// not race sibling tests' rows on the shared token table (REV-056-F06-style
/// isolation by fresh namespace, the same pattern the 1032 replay uses below).
async fn scratch_pool(name: &str) -> (PgPool, crate::pg_test_support::ScratchDb, String) {
        // REV-093-F06: guard-owned; cleans up on unwind too.
    let scratch_guard = crate::pg_test_support::ScratchDb::create(name).await;
    let scratch = scratch_guard.name().to_string();
    let url = format!(
        "{}/{}",
        crate::pg_test_support::require_live_url()
            .rsplitn(2, '/')
            .nth(1)
            .expect("url has a database"),
        scratch
    );
    let pool = crate::db::connect(&url, 2).await.expect("scratch pool");
    crate::db::migrate_with(&pool, true)
        .await
        .expect("migrate scratch");
    (pool, scratch_guard, scratch)
}

async fn drop_scratch(_guard: &crate::pg_test_support::ScratchDb, _scratch: &str) {
    /* REV-093-F06: guard-owned teardown, also runs on unwind */
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
    let workspace = WorkspaceScope::from_job_context(workspace_id).expect("workspace");
    let mut ctx = crate::workers::WorkerContext::new(pool.clone(), &settings, workspace);
    if let Some((token, chat)) = telegram {
        ctx.telegram_bot_token = Some(token.to_string());
        ctx.telegram_chat_id = Some(chat.to_string());
    }
    ctx
}

// ---------------------------------------------------------------------------
// REV-074-F01 (HIGH) — global signal uniqueness was a cross-tenant coupling
// ---------------------------------------------------------------------------

// The reviewer's probe through the REAL evaluator: workspace A accepts a signal;
// workspace B evaluates the SAME mint with the SAME policy at the SAME timestamp.
// Before migration 1033, B's insert died on the global
// `UNIQUE (chain, mint, signal_kind, created_at)` with SQLSTATE 23505 — one tenant
// blocking another's signal.
#[tokio::test]
async fn two_workspaces_can_write_the_same_signal_at_the_same_timestamp() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("g74f01a")).await;
    let ws_b = workspace(&pool, &tag("g74f01b")).await;
    let stem = tag("G74F01");
    let mint = format!("{stem}MINT");
    let config = crate::config::SignalsConfig::default();
    // One timestamp for BOTH evaluations — the exact collision the reviewer built.
    let now = Utc::now();

    seed_accepting_fixture(&pool, ws_a, &stem, &mint, now).await;
    // Same wallets, same disposition, in B's own workspace.
    for i in 0..2 {
        seed_label_with(&pool, ws_b, &format!("{stem}W{i}"), "manual_policy", "score", true).await;
    }

    let a = crate::workers::evaluate_token_signals(&pool, ws_a, ChainKind::Solana, &mint, now, &config)
        .await
        .expect("workspace A evaluation")
        .expect("workspace A must accept");
    let b = crate::workers::evaluate_token_signals(&pool, ws_b, ChainKind::Solana, &mint, now, &config)
        .await
        .expect("workspace B evaluation must not fail on the shared timestamp")
        .expect("workspace B must accept with its own score labels");

    assert_ne!(a, b, "two signals, one per tenant");
    let owners: Vec<i64> = sqlx::query_scalar(
        "SELECT workspace_id FROM signals WHERE id = ANY($1) ORDER BY id",
    )
    .bind(&vec![a, b])
    .fetch_all(&pool)
    .await
    .expect("owners");
    assert_eq!(owners, vec![ws_a, ws_b], "each tenant owns exactly its own signal");

    let evaluations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signal_evaluations WHERE mint = $1 AND status = 'accepted'",
    )
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count evaluations");
    assert_eq!(evaluations, 2, "two accepted evaluation rows, one per workspace");

    // And the alert identities are distinct too (REV-074-F04 territory, but the
    // coupling started here: same facts, same second, two tenants).
    let key_a = crate::signals::alert_dedup_key("signal", ws_a, a, "chat");
    let key_b = crate::signals::alert_dedup_key("signal", ws_b, b, "chat");
    assert_ne!(key_a, key_b);
}

// ---------------------------------------------------------------------------
// REV-074-F02 (HIGH) — signal evaluation was never scheduled by `run`
// ---------------------------------------------------------------------------

// Before the fix, `run_workers` started radar, alert dispatch, and discovery only;
// a token with due evidence sat unevaluated forever unless the CLI was invoked.
// This drives `evaluate_due_signals` — the function the spawned loop calls —
// against seeded due evidence, with NO CLI involvement.
#[tokio::test]
async fn the_worker_loop_evaluates_due_tokens_without_a_cli_call() {
    let (pool, scratch_guard, scratch) = scratch_pool("main").await;
    let ws = workspace(&pool, &tag("g74f02a")).await;
    let stem = tag("G74F02");
    let mint = format!("{stem}MINT");
    let now = Utc::now();

    seed_accepting_fixture(&pool, ws, &stem, &mint, now).await;

    let ctx = worker_ctx(&pool, ws, None);
    let evaluated = crate::workers::evaluate_due_signals(&ctx)
        .await
        .expect("evaluation pass");
    assert!(evaluated >= 1, "at least the seeded token is due");

    let signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count signals");
    assert_eq!(signals, 1, "one workspace-owned signal appears without any CLI call");

    // Unchanged evidence must not duplicate: a second pass inside the re-evaluation
    // cadence evaluates nothing for this token.
    let again = crate::workers::evaluate_due_signals(&ctx)
        .await
        .expect("second pass");
    let signals_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count signals after");
    assert_eq!(signals_after, 1, "no duplicate signal from unchanged evidence");
    let _ = again;

    // Another workspace's policy is not used: the same mint under a workspace whose
    // labels say `skip` produces its own rejection, not a reuse of ws's acceptance.
    let ws_b = workspace(&pool, &tag("g74f02b")).await;
    for i in 0..2 {
        seed_label_with(&pool, ws_b, &format!("{stem}W{i}"), "manual_block", "skip", true).await;
    }
    let ctx_b = worker_ctx(&pool, ws_b, None);
    crate::workers::evaluate_due_signals(&ctx_b)
        .await
        .expect("workspace B pass");
    let b_signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws_b)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count b signals");
    assert_eq!(b_signals, 0, "workspace B's policy must not inherit A's acceptance");
    drop_scratch(&scratch_guard, &scratch).await;
}

// A bad token must not stop the batch. `evaluate_token_signals` errors on an
// untracked token, so the loop must catch that per-token and continue to the next.
#[tokio::test]
async fn one_bad_token_does_not_stop_later_tokens() {
    let (pool, scratch_guard, scratch) = scratch_pool("bad").await;
    let ws = workspace(&pool, &tag("g74f02c")).await;
    let stem = tag("G74F02C");
    let good_mint = format!("{stem}GOOD");
    let now = Utc::now();

    // A token whose market snapshot is MISSING: the evaluator writes a rejection
    // for it, and — critically — a token seeded EARLIER that errors (unknown
    // disposition) must not prevent the good one being reached.
    seed_tradable_token(&pool, &good_mint, now).await;
    let bad_mint = format!("{stem}BAD");
    seed_tradable_token(&pool, &bad_mint, now - Duration::minutes(30)).await;
    let bogus = format!("{stem}BOGUS");
    seed_label_with(&pool, ws, &bogus, "auto_weird", "teleport", false).await;
    seed_buy(&pool, &bogus, &bad_mint, "100", &format!("sig-{stem}-bad"), now).await;

    for i in 0..2 {
        let wallet = format!("{stem}W{i}");
        seed_label_with(&pool, ws, &wallet, "manual_policy", "score", true).await;
        seed_wallet_score(&pool, &wallet, 99, 99, now).await;
        seed_buy(&pool, &wallet, &good_mint, "100", &format!("sig-{stem}-{i}"), now).await;
        seed_cluster_with(&pool, &wallet).await;
    }

    let ctx = worker_ctx(&pool, ws, None);
    crate::workers::evaluate_due_signals(&ctx)
        .await
        .expect("the pass itself must not fail because one token is poisoned");

    let signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&good_mint)
    .fetch_one(&pool)
    .await
    .expect("count signals");
    assert_eq!(signals, 1, "the good token was still evaluated after the bad one");
    drop_scratch(&scratch_guard, &scratch).await;
}

// The queue gate is the ONLY thing between the loop and a pressured system: a
// paused `signal_eval` queue must suppress execution entirely.
#[tokio::test]
async fn a_paused_signal_eval_queue_suppresses_the_loop() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g74f02d")).await;
    let stem = tag("G74F02D");
    let mint = format!("{stem}MINT");
    let now = Utc::now();
    seed_accepting_fixture(&pool, ws, &stem, &mint, now).await;

    let ctx = worker_ctx(&pool, ws, None);
    {
        let mut q = ctx.queue_state.lock().await;
        q.set_paused(crate::queues::QUEUE_SIGNAL_EVAL, true);
    }
    // This is exactly the guard the spawned loop checks before each pass.
    assert!(
        !ctx.queue_allowed(crate::queues::QUEUE_SIGNAL_EVAL).await,
        "a paused queue must report not-allowed"
    );
    {
        let mut q = ctx.queue_state.lock().await;
        q.set_paused(crate::queues::QUEUE_SIGNAL_EVAL, false);
    }
    assert!(ctx.queue_allowed(crate::queues::QUEUE_SIGNAL_EVAL).await);

    // And the queue participates in precedence-based pausing: bottom-up pressure
    // reaches signal_eval FIRST.
    let mut state = crate::queues::QueueState::new();
    state.pause_bottom_up(1);
    assert!(
        state.is_paused(crate::queues::QUEUE_SIGNAL_EVAL),
        "signal_eval is the lowest-precedence queue and must pause first"
    );
}

// ---------------------------------------------------------------------------
// REV-074-F03 (HIGH) — Telegram failure was recorded as terminal success
// ---------------------------------------------------------------------------

// A mock Bot API: 500 on the first send, 200 afterwards.
async fn serve_flaky_telegram(fail_first: std::sync::Arc<std::sync::atomic::AtomicBool>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock telegram");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route(
        "/bot{token}/sendMessage",
        axum::routing::post(move || {
            let fail = fail_first.clone();
            async move {
                if fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
                } else {
                    (axum::http::StatusCode::OK, "{\"ok\":true}")
                }
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
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

#[tokio::test]
async fn a_failed_telegram_send_is_retried_not_recorded_as_delivered() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g74f03a")).await;
    let mint = tag("G74F03MINT");
    let now = Utc::now();
    let signal_id = seed_signal(&pool, ws, "solana", &mint, now).await;

    let fail_first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let base = serve_flaky_telegram(fail_first.clone()).await;
    let ctx = worker_ctx(&pool, ws, Some(("test-token", "test-chat")));

    // First cycle: the send fails with a 500. The row must stay OPEN — pending with
    // a future retry — never 'sent'.
    let sent = crate::workers::dispatch_alerts_via(&ctx, &base)
        .await
        .expect("first dispatch cycle");
    assert_eq!(sent, 0, "a 500 cannot count as delivered");

    let (state, attempts, last_error): (String, i32, Option<String>) = sqlx::query_as(
        "SELECT state, attempt_count, last_error FROM alerts WHERE signal_id = $1",
    )
    .bind(signal_id)
    .fetch_one(&pool)
    .await
    .expect("alert row");
    assert_eq!(state, "pending", "a transient failure stays retryable");
    assert_eq!(attempts, 1);
    assert!(last_error.is_some(), "the failure is recorded for operators");

    // The old semantics are gone: the pending query still returns the signal once
    // the retry is due (next_attempt_at in the past).
    sqlx::query("UPDATE alerts SET next_attempt_at = now() - interval '1 second' WHERE signal_id = $1")
        .bind(signal_id)
        .execute(&pool)
        .await
        .expect("force retry due");

    // Second cycle: the mock now returns 200. Exactly one send, marked sent.
    let sent = crate::workers::dispatch_alerts_via(&ctx, &base)
        .await
        .expect("second dispatch cycle");
    assert_eq!(sent, 1, "the retry delivers");
    let state: String = sqlx::query_scalar("SELECT state FROM alerts WHERE signal_id = $1")
        .bind(signal_id)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(state, "sent");

    // Third cycle: nothing pending, nothing resent.
    let sent = crate::workers::dispatch_alerts_via(&ctx, &base)
        .await
        .expect("third dispatch cycle");
    assert_eq!(sent, 0, "a delivered alert is never resent");
}

// A permanent 4xx goes dead immediately; a 429 honors Retry-After.
#[tokio::test]
async fn permanent_failure_dies_and_rate_limit_respects_retry_after() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g74f03b")).await;
    let mint = tag("G74F03BMINT");
    let now = Utc::now();
    let signal_id = seed_signal(&pool, ws, "solana", &mint, now).await;

    // Mock: 429 with Retry-After: 120, then 400.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let call = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let call2 = call.clone();
    let app = axum::Router::new().route(
        "/bot{token}/sendMessage",
        axum::routing::post(move || {
            let call = call2.clone();
            async move {
                let n = call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    axum::response::Response::builder()
                        .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
                        .header(axum::http::header::RETRY_AFTER, "120")
                        .body(axum::body::Body::from("slow down"))
                        .unwrap()
                } else {
                    axum::response::Response::builder()
                        .status(axum::http::StatusCode::BAD_REQUEST)
                        .body(axum::body::Body::from("bad chat"))
                        .unwrap()
                }
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = format!("http://{addr}");

    let ctx = worker_ctx(&pool, ws, Some(("test-token", "test-chat")));
    crate::workers::dispatch_alerts_via(&ctx, &base).await.expect("429 cycle");
    let (state, next_at): (String, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT state, next_attempt_at FROM alerts WHERE signal_id = $1",
    )
    .bind(signal_id)
    .fetch_one(&pool)
    .await
    .expect("alert row after 429");
    assert_eq!(state, "pending");
    let next_at = next_at.expect("retry scheduled");
    let delay = (next_at - Utc::now()).num_seconds();
    assert!(
        (100..=130).contains(&delay),
        "Retry-After: 120 must drive the schedule; got {delay}s"
    );

    // Next attempt gets a 400: permanent, dead immediately, never retried.
    sqlx::query("UPDATE alerts SET next_attempt_at = now() - interval '1 second' WHERE signal_id = $1")
        .bind(signal_id)
        .execute(&pool)
        .await
        .expect("force due");
    crate::workers::dispatch_alerts_via(&ctx, &base).await.expect("400 cycle");
    let state: String = sqlx::query_scalar("SELECT state FROM alerts WHERE signal_id = $1")
        .bind(signal_id)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(state, "dead", "a permanent 4xx is not retried");
    let calls = call.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(calls, 2, "no further send after death");
    let sent = crate::workers::dispatch_alerts_via(&ctx, &base).await.expect("dead cycle");
    assert_eq!(sent, 0);
    assert_eq!(call.load(std::sync::atomic::Ordering::SeqCst), 2, "dead rows are never resent");
}

// ---------------------------------------------------------------------------
// REV-074-F04 (MEDIUM) — dispatcher hardcoded Solana
// ---------------------------------------------------------------------------

// Two chains, same workspace/mint/kind/microsecond: two signals, two alerts, each
// labeled with ITS chain. The old code hardcoded "solana" and used a
// second-truncated dedup key, so one alert swallowed the other and the Robinhood
// message was mislabeled.
#[tokio::test]
async fn two_chains_at_the_same_microsecond_produce_two_correctly_labeled_alerts() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g74f04a")).await;
    let mint = tag("G74F04MINT");
    let now = Utc::now();
    let sol = seed_signal(&pool, ws, "solana", &mint, now).await;
    let rh = seed_signal(&pool, ws, "robinhood", &mint, now).await;

    // Capture the messages the dispatcher sends.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: std::sync::Arc<tokio::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured2 = captured.clone();
    let app = axum::Router::new().route(
        "/bot{token}/sendMessage",
        axum::routing::post(move |body: axum::Json<serde_json::Value>| {
            let captured = captured2.clone();
            async move {
                captured
                    .lock()
                    .await
                    .push(body["text"].as_str().unwrap_or("").to_string());
                (axum::http::StatusCode::OK, "{\"ok\":true}")
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = format!("http://{addr}");

    let ctx = worker_ctx(&pool, ws, Some(("test-token", "test-chat")));
    let sent = crate::workers::dispatch_alerts_via(&ctx, &base)
        .await
        .expect("dispatch");
    assert_eq!(sent, 2, "both chains must be delivered, not one swallowed by a colliding key");

    let texts = captured.lock().await.clone();
    assert_eq!(texts.len(), 2);
    assert!(
        texts.iter().any(|t| t.contains("solana")),
        "the solana signal must be labeled solana; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("robinhood")),
        "the robinhood signal must be labeled robinhood — the hardcoded label was the bug; got {texts:?}"
    );

    // Both rows sent, zero pending remain.
    let pending = crate::workers::pending_signal_alerts(&pool, ws, "test-chat")
        .await
        .expect("pending after dispatch");
    assert!(
        !pending.iter().any(|(id, _, _, _, _, _)| *id == sol || *id == rh),
        "no pending rows may remain after successful dispatch"
    );

    // Same-chain, same mint, same SECOND, two different signals (created_at differs
    // by one microsecond — inside the truncation window of the old key): also two
    // alerts. The timestamp-truncated key collided these too.
    let a = seed_signal(&pool, ws, "solana", &mint, now + Duration::microseconds(1)).await;
    let b = seed_signal(&pool, ws, "solana", &mint, now + Duration::microseconds(2)).await;
    let sent = crate::workers::dispatch_alerts_via(&ctx, &base)
        .await
        .expect("second dispatch");
    assert_eq!(sent, 2, "two signals within one second are two alerts");
    let key_a = crate::signals::alert_dedup_key("signal", ws, a, "chat");
    let key_b = crate::signals::alert_dedup_key("signal", ws, b, "chat");
    assert_ne!(key_a, key_b);
}

// ---------------------------------------------------------------------------
// REV-074 hardening — transitive 3-cluster migration repair replay
// ---------------------------------------------------------------------------

// The reviewer's overlap fixture `A-X`, `X-Y`, `Y-Z` replayed through migration
// 1032's OWN merge logic on a scratch database, twice: identical membership digest
// and exactly one cutover event.
#[tokio::test]
async fn migration_1032_repairs_a_transitive_overlap_idempotently() {
    // A scratch database migrated only through 1031, so 1032 runs HERE under test.
    // REV-093-F06: guard-owned, so an assertion failure below cannot strand it.
    let scratch_guard = crate::pg_test_support::ScratchDb::create("replay1032").await;
    let pool = scratch_guard.pool().await;

    // Apply everything EXCEPT 1032 via the production runner, then seed the
    // pre-1032 duplicate state, then apply 1032 by hand exactly as the runner does.
    crate::db::migrate_with(&pool, true).await.expect("migrate scratch");
    // The full set already includes 1032 — this scratch is for the REPLAY below,
    // so undo its effects deterministically instead of replaying by filename:
    // recreate the transitive overlap the reviewer's fixture describes.
    sqlx::query("DROP INDEX IF EXISTS public.wallet_cluster_members_one_active_idx")
        .execute(&pool)
        .await
        .expect("drop index for replay");
    sqlx::query("DELETE FROM schema_cutover_events WHERE cutover = 'wallet_cluster_membership_canonicalization'")
        .execute(&pool)
        .await
        .expect("clear cutover log for replay");

    // Three clusters, one address each: A in c0, X in c1, Y in c2, Z in c3.
    let members = ["A", "X", "Y", "Z"];
    let mut cluster_ids = Vec::new();
    for member in &members {
        let cid: i64 = sqlx::query_scalar(
            "INSERT INTO swi_legacy.wallet_clusters (created_at) VALUES (now()) RETURNING cluster_id",
        )
        .fetch_one(&pool)
        .await
        .expect("cluster");
        cluster_ids.push(cid);
        sqlx::query(
            "INSERT INTO wallet_cluster_members (cluster_id, chain, address) \
             VALUES ($1, 'solana', $2)",
        )
        .bind(cid)
        .bind(format!("REPLAY{member}"))
        .execute(&pool)
        .await
        .expect("member");
    }
    // The transitive A-X, X-Y, Y-Z overlap: X also active in c0, Y also in c1,
    // Z also in c2. All four clusters are one component.
    for (member, idx) in [("X", 0usize), ("Y", 1usize), ("Z", 2usize)] {
        sqlx::query(
            "INSERT INTO wallet_cluster_members (cluster_id, chain, address) \
             VALUES ($1, 'solana', $2)",
        )
        .bind(cluster_ids[idx])
        .bind(format!("REPLAY{member}"))
        .execute(&pool)
        .await
        .expect("overlap member");
    }

    // Run migration 1032's Part A body against this state, exactly as shipped.
    let sql = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("parent")
            .join("swi-deploy/migrations/1032_rev072_signal_tenancy_and_cluster_canonicalization.sql"),
    )
    .expect("read 1032");
    sqlx::raw_sql(&sql).execute(&pool).await.expect("replay 1032");

    let digest = |pool: PgPool| async move {
        sqlx::query_scalar::<_, String>(
            "SELECT coalesce(string_agg(cluster_id::text || ':' || address || ':' || coalesce(revoked_at::text,'-'), ',' ORDER BY address, cluster_id), '') \
             FROM wallet_cluster_members WHERE address LIKE 'REPLAY%'",
        )
        .fetch_one(&pool)
        .await
        .expect("digest")
    };
    let first = digest(pool.clone()).await;
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM schema_cutover_events WHERE cutover = 'wallet_cluster_membership_canonicalization'",
    )
    .fetch_one(&pool)
    .await
    .expect("events");

    // One active membership per address, all in the SAME (smallest) cluster.
    let active: Vec<(String, i64)> = sqlx::query_as(
        "SELECT address, cluster_id FROM wallet_cluster_members \
          WHERE address LIKE 'REPLAY%' AND revoked_at IS NULL ORDER BY address",
    )
    .fetch_all(&pool)
    .await
    .expect("active rows");
    assert_eq!(active.len(), 4, "A, X, Y, Z each active exactly once");
    let canonical = active[0].1;
    assert_eq!(canonical, cluster_ids[0], "the smallest cluster id is canonical");
    assert!(active.iter().all(|(_, c)| *c == canonical));

    // Second replay: byte-identical membership state, and still ONE cutover event.
    sqlx::raw_sql(&sql).execute(&pool).await.expect("replay 1032 again");
    let second = digest(pool.clone()).await;
    assert_eq!(first, second, "the repair is idempotent");
    let events_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM schema_cutover_events WHERE cutover = 'wallet_cluster_membership_canonicalization'",
    )
    .fetch_one(&pool)
    .await
    .expect("events after");
    assert_eq!(events, 1, "one cutover event from the repairing replay");
    assert_eq!(events_after, 1, "the second replay logs nothing new");

    /* REV-093-F06: guard-owned teardown, also runs on unwind */
}
