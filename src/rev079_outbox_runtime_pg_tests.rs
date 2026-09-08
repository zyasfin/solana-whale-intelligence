//! Live PostgreSQL regressions for REV-078 F01–F06.
//!
//! Each test reproduces the reviewer's own probe:
//!
//! * F01 — a 1033-like database with pending/dead/sent alert rows must survive the
//!   sent_at cutover (1034 ran it in the wrong order and aborted such upgrades);
//! * F02 — the documented runtime role must be able to run every new production
//!   path (fenced completion, eval claim/release, queue pause) end-to-end;
//! * F03 — funding alerts enter the outbox with a typed subject and a failed HTTP
//!   delivery is retried by the generic drain;
//! * F04 — a stale evaluator must not delete a reclaimed claim;
//! * F05 — an unreadable queue authority fails CLOSED;
//! * F06 — a claim at the attempt cap is terminally dead, not stranded pending.
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

async fn workspace(pool: &PgPool, slug: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ($1, $2) RETURNING id")
        .bind(slug)
        .bind(slug)
        .fetch_one(pool)
        .await
        .expect("create workspace")
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

/// A scratch database migrated through 1033 ONLY (the 1034-aborted-upgrade lane),
/// then brought forward by migration 1035 — the exact sequence REV-078-F01 broke.
async fn scratch_through_1033_with_alert_history(name: &str) -> (PgPool, crate::pg_test_support::ScratchDb, String) {
        // REV-093-F06: guard-owned; cleans up on unwind too.
    let scratch_guard = crate::pg_test_support::ScratchDb::create(name).await;
    let scratch = scratch_guard.name().to_string();

    // Migrations dir minus 1034 and 1035: the pre-upgrade state.
    let src_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .join("swi-deploy/migrations");
    // REV-087 item 7: per-fixture temp dir (pid alone collides across fixtures).
    let red_dir = std::env::temp_dir().join(format!("swi_upg_migrations_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&red_dir);
    std::fs::create_dir_all(&red_dir).expect("mkdir");
    for entry in std::fs::read_dir(&src_dir).expect("read migrations") {
        let entry = entry.expect("entry");
        let name = entry.file_name().to_string_lossy().to_string();
        // Lexicographic cutoff, not a prefix denylist: a denylist silently leaks
        // every migration added after it was written (1039, then 1040).
        if name.ends_with(".sql") && name.as_str() >= "1034_" {
            continue;
        }
        std::fs::copy(entry.path(), red_dir.join(&name)).expect("copy");
    }
    let url = format!(
        "{}/{}",
        crate::pg_test_support::require_live_url().rsplitn(2, '/').nth(1).expect("db url"),
        scratch
    );
    let pool = crate::db::connect(&url, 2).await.expect("scratch pool");
    crate::db::migrate_dir_with(&pool, &red_dir, true)
        .await
        .expect("migrate through 1033");

    // Pre-upgrade history exactly as a live 1033 deployment would have it:
    // one pending row with a default-stamped sent_at (the lie 1034 named),
    // one dead row, one genuinely sent row.
    sqlx::query("INSERT INTO workspaces (name, slug) VALUES ('w','upgw') ON CONFLICT DO NOTHING")
        .execute(&pool).await.expect("ws");
    let ws: i64 = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'upgw'")
        .fetch_one(&pool).await.expect("ws id");
    for (key, state) in [("sig-a", "pending"), ("sig-b", "dead"), ("sig-c", "sent")] {
        let sid: i64 = sqlx::query_scalar(
            "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
             VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active') RETURNING id",
        )
        .bind(ws)
        .bind(key)
        .fetch_one(&pool)
        .await
        .expect("signal");
        if state == "sent" {
            sqlx::query(
                "INSERT INTO alerts (dedup_key, signal_id, state, attempt_count) \
                 VALUES ($1, $2, 'sent', 1)",
            )
            .bind(format!("signal:{ws}:{sid}:chat"))
            .bind(sid)
            .execute(&pool)
            .await
            .expect("sent alert");
        } else {
            sqlx::query(
                "INSERT INTO alerts (dedup_key, signal_id, state, attempt_count, next_attempt_at) \
                 VALUES ($1, $2, $3, 1, now())",
            )
            .bind(format!("signal:{ws}:{sid}:chat"))
            .bind(sid)
            .bind(state)
            .execute(&pool)
            .await
            .expect("alert");
        }
    }
    (pool, scratch_guard, scratch)
}

#[tokio::test]
async fn a_1033_database_with_alert_history_survives_the_sent_at_cutover() {
    let (pool, _scratch_guard, _scratch) = scratch_through_1033_with_alert_history("f01").await;

    // Pre-check the lane really is pre-1034-shape: sent_at is NOT NULL with a default.
    let nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'alerts' AND column_name = 'sent_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("nullable");
    assert_eq!(nullable, "NO", "the fixture must start with the inherited NOT NULL");
    let lies: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM alerts WHERE state <> 'sent' AND sent_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("lies");
    assert_eq!(lies, 2, "pending and dead rows carry the default timestamp");

    // Apply 1034 + 1035 in runner order via the production migrator — the exact
    // sequence that aborted on this lane before the operator's one-time repair.
    // 1035 is idempotent and correct-ordered; 1034 needs its NOT NULL gone first,
    // which is precisely the documented one-time operator step.
    sqlx::query("ALTER TABLE public.alerts ALTER COLUMN sent_at DROP NOT NULL")
        .execute(&pool)
        .await
        .expect("operator one-time step (documented in 1035 header)");
    crate::db::migrate_with(&pool, true)
        .await
        .expect("migrate 1034+1035");

    // The cutover is complete and honest: no non-sent row carries a timestamp,
    // the genuinely-sent row keeps its own.
    let remaining_lies: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM alerts WHERE state <> 'sent' AND sent_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("remaining lies");
    assert_eq!(remaining_lies, 0);
    let sent_kept: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM alerts WHERE state = 'sent' AND sent_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("sent kept");
    assert_eq!(sent_kept, 1);
    let nullable_after: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'alerts' AND column_name = 'sent_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("nullable after");
    assert_eq!(nullable_after, "YES");

    /* REV-093-F06: guard-owned teardown, also runs on unwind */
}

// ---------------------------------------------------------------------------
// F02 — runtime role can execute every new production path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_runtime_role_executes_outbox_claim_and_queue_paths() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g78f02a")).await;

    // Privilege probes as the documented runtime role, in one session.
    let mut conn = pool.acquire().await.expect("conn");
    sqlx::query("SET ROLE swi_legacy_runtime")
        .execute(&mut *conn)
        .await
        .expect("set role");

    // alerts: claim-shaped INSERT + fenced UPDATE.
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .bind(tag("G78F02MINT"))
    .fetch_one(&mut *conn)
    .await
    .expect("runtime inserts signal");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, signal_id, destination, state, attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $3, $2, 'chat', 'pending', 1, now())",
    )
    .bind(crate::signals::alert_dedup_key("signal", ws, sid, "chat"))
    .bind(sid)
    .bind(ws)
    .execute(&mut *conn)
    .await
    .expect("runtime inserts alert");
    sqlx::query(
        "UPDATE alerts SET state = 'sent', sent_at = now() WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("signal", ws, sid, "chat"))
    .execute(&mut *conn)
    .await
    .expect("runtime updates alert (REV-078-F02)");

    // signal_eval_claims: claim + release.
    sqlx::query(
        "INSERT INTO signal_eval_claims (workspace_id, chain, mint, claimed_by, expires_at) \
         VALUES ($1, 'solana', $2, 'test', now() + interval '10 minutes')",
    )
    .bind(ws)
    .bind(tag("G78F02MINT2"))
    .execute(&mut *conn)
    .await
    .expect("runtime claims eval");
    sqlx::query(
        "DELETE FROM signal_eval_claims WHERE workspace_id = $1 AND claimed_by = 'test'",
    )
    .bind(ws)
    .execute(&mut *conn)
    .await
    .expect("runtime releases eval claim");

    // queue_state: pause write + read.
    sqlx::query(
        "INSERT INTO queue_state (queue, paused, updated_by) VALUES ('signal_eval', true, 'test') \
         ON CONFLICT (queue) DO UPDATE SET paused = true",
    )
    .execute(&mut *conn)
    .await
    .expect("runtime writes queue_state");
    sqlx::query(
        "INSERT INTO queue_state (queue, paused, updated_by) VALUES ('signal_eval', false, 'test') \
         ON CONFLICT (queue) DO UPDATE SET paused = false",
    )
    .execute(&mut *conn)
    .await
    .expect("cleanup (unpause: runtime has UPDATE, deliberately not DELETE)");
    sqlx::query("RESET ROLE").execute(&mut *conn).await.expect("reset role");
}

// ---------------------------------------------------------------------------
// F03 — funding alerts: typed subject, real producer, generic retry drain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_funding_alert_enters_the_outbox_and_a_failed_send_is_retried_by_the_drain() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g78f03a")).await;

    // A funding case whose id does NOT overlap any signal id (the FK trap this
    // test exists to catch: 1035 typed the subject, and before that a funding case
    // id that happened to be a valid signal id hid the violation).
    //
    // REV-087: this used to be a retry loop that reinserted with the SAME recipient
    // under `ON CONFLICT ... DO UPDATE RETURNING id`, which returns the IDENTICAL
    // row every time — so on a collision it spun forever and hung the whole pg lane
    // for 80 minutes. Retrying was the wrong shape anyway: `signals` and
    // `funding_radar_cases` advance their own bigserials roughly together on a busy
    // database, so a bounded retry can lose every attempt. Push the case sequence
    // PAST the highest signal id instead — deterministic, one statement, no loop.
    let recipient = tag("G78F03RECIPIENT");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("wallet");
    sqlx::query(
        "SELECT setval(pg_get_serial_sequence('funding_radar_cases', 'id'), \
                       GREATEST((SELECT COALESCE(max(id), 0) FROM signals), \
                                (SELECT COALESCE(max(id), 0) FROM funding_radar_cases)) + 1000)",
    )
    .execute(&pool)
    .await
    .expect("advance the case sequence past every signal id");
    let case_id: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, first_funding_native, \
              source_address, deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $3, now(), '100', '0.5', $2, now() + interval '1 day', \
                 'funded', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(tag("G78F03SRC"))
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("case");
    // The precondition this test depends on, asserted rather than assumed.
    let overlaps: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM signals WHERE id = $1)")
        .bind(case_id)
        .fetch_one(&pool)
        .await
        .expect("overlap check");
    assert!(!overlaps, "the case id must not also be a valid signal id");
    // The claim writes a typed funding subject — no FK violation, error propagated.
    let claim = crate::signals::claim_alert(&pool, "funding", ws, case_id, "chat-a")
        .await
        .expect("claim must not violate the FK (REV-078-F03)")
        .expect("claim wins");
    let (kind, sig, fc): (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT subject_kind, signal_id, funding_case_id FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("funding", ws, case_id, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(kind, "funding");
    assert!(sig.is_none());
    assert_eq!(fc, Some(case_id));

    // A failed delivery, then the GENERIC drain retries it: mock 500 then 200.
    let marked = crate::signals::mark_alert_failed(
        &pool, "funding", ws, case_id, "chat-a", &claim.token, claim.attempt,
        "Transient", false, None,
    )
    .await
    .expect("mark failed");
    assert!(marked);

    let fail_first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fail2 = fail_first.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route(
        "/bot{token}/sendMessage",
        axum::routing::post(move || {
            let fail = fail2.clone();
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
    let base = format!("http://{addr}");

    // Force the retry due; the mock's first answer (500) is consumed by the drain,
    // then a second dispatch succeeds.
    sqlx::query("UPDATE alerts SET next_attempt_at = now() - interval '1 second' WHERE dedup_key = $1")
        .bind(crate::signals::alert_dedup_key("funding", ws, case_id, "chat-a"))
        .execute(&pool)
        .await
        .expect("force due");
    let ctx = worker_ctx(&pool, ws, Some(("test-token", "chat-a")));
    crate::workers::dispatch_alerts_via(&ctx, &base).await.expect("drain cycle 1 (500)");
    sqlx::query("UPDATE alerts SET next_attempt_at = now() - interval '1 second' WHERE dedup_key = $1")
        .bind(crate::signals::alert_dedup_key("funding", ws, case_id, "chat-a"))
        .execute(&pool)
        .await
        .expect("force due 2");
    crate::workers::dispatch_alerts_via(&ctx, &base).await.expect("drain cycle 2 (200)");
    let state: String = sqlx::query_scalar(
        "SELECT state FROM alerts WHERE dedup_key = $1",
    )
    .bind(crate::signals::alert_dedup_key("funding", ws, case_id, "chat-a"))
    .fetch_one(&pool)
    .await
    .expect("state");
    assert_eq!(state, "sent", "the generic drain retried the funding alert to delivery");
}

// ---------------------------------------------------------------------------
// F04 — stale evaluator must not delete a reclaimed claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_evaluator_release_does_not_delete_a_reclaimed_claim() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g78f04a")).await;
    let mint = tag("G78F04MINT");

    // Worker A claims.
    let token_a = format!("worker-A-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO signal_eval_claims (workspace_id, chain, mint, claimed_by, expires_at) \
         VALUES ($1, 'solana', $2, $3, now() + interval '10 minutes')",
    )
    .bind(ws)
    .bind(&mint)
    .bind(&token_a)
    .execute(&pool)
    .await
    .expect("A claims");

    // A's lease expires; B reclaims (same conflict path as production).
    sqlx::query("UPDATE signal_eval_claims SET expires_at = now() - interval '1 second' WHERE workspace_id = $1")
        .bind(ws)
        .execute(&pool)
        .await
        .expect("expire A");
    let token_b = format!("worker-B-{}", uuid::Uuid::new_v4());
    let reclaimed: Option<i64> = sqlx::query_scalar(
        r#"
        INSERT INTO signal_eval_claims (workspace_id, chain, mint, claimed_by, expires_at)
        VALUES ($1, 'solana', $2, $3, now() + interval '10 minutes')
        ON CONFLICT (workspace_id, chain, mint) DO UPDATE
            SET claimed_by = $3, claimed_at = now(), expires_at = now() + interval '10 minutes'
            WHERE signal_eval_claims.expires_at <= now()
        RETURNING workspace_id
        "#,
    )
    .bind(ws)
    .bind(&mint)
    .bind(&token_b)
    .fetch_optional(&pool)
    .await
    .expect("B reclaims");
    assert!(reclaimed.is_some());

    // A finishes late and releases THE WAY PRODUCTION NOW DOES (fenced by token).
    let deleted = sqlx::query(
        "DELETE FROM signal_eval_claims \
          WHERE workspace_id = $1 AND chain = 'solana' AND mint = $2 AND claimed_by = $3",
    )
    .bind(ws)
    .bind(&mint)
    .bind(&token_a)
    .execute(&pool)
    .await
    .expect("stale fenced release");
    assert_eq!(
        deleted.rows_affected(), 0,
        "the stale release must not match the reclaimed claim (REV-078-F04)"
    );
    let owner: String = sqlx::query_scalar(
        "SELECT claimed_by FROM signal_eval_claims WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("owner");
    assert_eq!(owner, token_b, "B's live claim survives");
}

// ---------------------------------------------------------------------------
// F05 — unreadable queue authority fails CLOSED
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unreadable_queue_authority_fails_closed() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g78f05a")).await;

    // A pool whose role cannot read queue_state: deny SELECT via a session role
    // that has no grant. The production connector grants the runtime role SELECT;
    // simulating the outage by pointing at a role-stripped session is the same
    // failure shape the reviewer named (permission error → must not run).
    let mut conn = pool.acquire().await.expect("conn");
    sqlx::query("SET ROLE swi_app") // the canonical read-only API role has no grant here... verify it errors or returns
        .execute(&mut *conn)
        .await
        .expect("set role");
    let probe = sqlx::query_scalar::<_, Option<bool>>("SELECT paused FROM queue_state WHERE queue = 'signal_eval'")
        .fetch_optional(&mut *conn)
        .await;
    sqlx::query("RESET ROLE").execute(&mut *conn).await.expect("reset");
    drop(conn);

    // Whether the probe errors or not, the PRODUCTION contract is the checked
    // form: an Err from the authority means NOT allowed.
    let _ctx = worker_ctx(&pool, ws, None);
    // Directly exercise the checked form's failure mapping with a poisoned pool:
    // a pool connected to a database where queue_state does not exist at all.
    // REV-093-F06: guard-owned, so the fail-closed assertions below cannot strand it.
    let scratch_guard = crate::pg_test_support::ScratchDb::create("noqueue").await;
    let url = scratch_guard.scratch_url().to_string();
    let empty_pool = crate::db::connect(&url, 2).await.expect("empty pool");
    let broken_ctx = worker_ctx(&empty_pool, ws, None);
    let checked = broken_ctx.queue_allowed_checked(crate::queues::QUEUE_SIGNAL_EVAL).await;
    assert!(checked.is_err(), "a missing queue_state table is an error, not an answer");
    assert!(
        !broken_ctx.queue_allowed(crate::queues::QUEUE_SIGNAL_EVAL).await,
        "an unreadable authority must FAIL CLOSED (REV-078-F05)"
    );
    let _ = probe;
    /* REV-093-F06: guard-owned teardown, also runs on unwind */
}

// ---------------------------------------------------------------------------
// F06 — exhausted attempts are terminally dead, not stranded pending
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_at_the_attempt_cap_is_terminally_dead() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g78f06a")).await;
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .bind(tag("G78F06MINT"))
    .fetch_one(&pool)
    .await
    .expect("signal");
    let key = crate::signals::alert_dedup_key("signal", ws, sid, "chat");

    // Drive to the cap by repeated crash-claims (the production claim path).
    for round in 0..crate::signals::ALERT_MAX_ATTEMPTS {
        let claim = crate::signals::claim_alert(&pool, "signal", ws, sid, "chat")
            .await
            .expect("claim");
        assert!(claim.is_some(), "round {round} claimable before the cap");
        sqlx::query(
            "UPDATE alerts SET claim_expires_at = now() - interval '1 second', \
                               next_attempt_at = now() - interval '1 second' \
              WHERE dedup_key = $1",
        )
        .bind(&key)
        .execute(&pool)
        .await
        .expect("expire");
    }
    let blocked = crate::signals::claim_alert(&pool, "signal", ws, sid, "chat")
        .await
        .expect("claim at cap");
    assert!(blocked.is_none(), "the cap refuses further claims");

    // REV-080-F03: the transition to terminal happens AT the decision point — the
    // claim itself terminalizes an exhausted row (the one-time migration sweep
    // cannot repair exhaustion that happens AFTER deployment). The claim we just
    // attempted above already deadened the row; no manual sweep is involved.
    let state: String = sqlx::query_scalar("SELECT state FROM alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(
        state, "dead",
        "the claim decision point terminalizes exhaustion — no manual sweep (REV-080-F03)"
    );

    // And the due-selection never offers it again.
    let pending = crate::workers::pending_signal_alerts(&pool, ws, "chat")
        .await
        .expect("pending");
    assert!(!pending.iter().any(|(id, _, _, _, _, _)| *id == sid));
}
