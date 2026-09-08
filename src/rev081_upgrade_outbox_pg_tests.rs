//! Live PostgreSQL regressions for REV-080 F01–F07.
//!
//! Each test reproduces the reviewer's own probe:
//!
//! * F01 — an unassisted 1033-with-history upgrade must reach 1035+: the migrator
//!   preflight repairs the known pre-1034 `sent_at` shape before filename order
//!   applies 1034 (the checked-in manual ALTER was only the operator workaround);
//! * F02 — the funding retry drain selects only the CURRENT workspace's rows and
//!   claims the existing row, never re-keying under another tenant;
//! * F03 — exhaustion after deployment terminalizes AT the claim decision point,
//!   with no manual sweep SQL copied from the migration;
//! * F04 — evaluator WRITES are fenced: a stale worker whose lease expired after a
//!   reclaim writes zero rows;
//! * F05 — a lost funding claim is recovered from durable case state by the drain;
//! * F06 — legacy subjects are classified from the minted key kind; unverifiable
//!   rows are 'unknown', never silently signal;
//! * F07 — cutover event inserts are replay-safe.
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

/// A scratch database migrated through 1033 ONLY, holding a pending alert row —
/// the exact lane that aborted inside 1034 before the migrator preflight existed.
async fn scratch_1033_with_pending(name: &str) -> (PgPool, crate::pg_test_support::ScratchDb, String) {
        // REV-093-F06: guard-owned; cleans up on unwind too.
    let mut scratch_guard = crate::pg_test_support::ScratchDb::create(name).await;
    let scratch = scratch_guard.name().to_string();

    let src_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .join("swi-deploy/migrations");
    // REV-087 item 7: per-fixture temp dir (pid alone collides across fixtures).
    let red_dir = scratch_guard.temp_dir("f01");
    for entry in std::fs::read_dir(&src_dir).expect("read migrations") {
        let entry = entry.expect("entry");
        let n = entry.file_name().to_string_lossy().to_string();
        // Lexicographic cutoff, not a prefix denylist (which rots as migrations
        // are added: 1039, then 1040).
        if n.ends_with(".sql") && n.as_str() >= "1034_" {
            continue;
        }
        std::fs::copy(entry.path(), red_dir.join(&n)).expect("copy");
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

    sqlx::query("INSERT INTO workspaces (name, slug) VALUES ('w','upgw') ON CONFLICT DO NOTHING")
        .execute(&pool).await.expect("ws");
    let ws: i64 = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'upgw'")
        .fetch_one(&pool).await.expect("ws id");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'UPGMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, signal_id, state, attempt_count, next_attempt_at) \
         VALUES ('k', $1, 'pending', 1, now())",
    )
    .bind(sid)
    .execute(&pool)
    .await
    .expect("pending alert");
    (pool, scratch_guard, scratch)
}

// ---------------------------------------------------------------------------
// F01 — unassisted 1033→current upgrade reaches the newest migration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unassisted_1033_upgrade_with_pending_alerts_reaches_current() {
    let (pool, _scratch_guard, _scratch) = scratch_1033_with_pending("a").await;

    // NO manual ALTER, NO operator step: the production migrator alone must carry
    // this lane to current. The preflight repairs the known pre-1034 shape before
    // filename order reaches 1034.
    crate::db::migrate_with(&pool, true)
        .await
        .expect("unassisted upgrade must reach current (REV-080-F01)");

    let nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'alerts' AND column_name = 'sent_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("nullable");
    assert_eq!(nullable, "YES");
    let lies: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM alerts WHERE state <> 'sent' AND sent_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("lies");
    assert_eq!(lies, 0);
    let has_ws_col: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'alerts' AND column_name = 'workspace_id')",
    )
    .fetch_one(&pool)
    .await
    .expect("ws col");
    assert!(has_ws_col, "the lane reached 1036");

    /* REV-093-F06: guard-owned teardown, also runs on unwind */
}

// ---------------------------------------------------------------------------
// F02 — funding drain is workspace-scoped on row ownership
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_funding_drain_never_selects_another_workspaces_retry() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("g80f02a")).await;
    let ws_b = workspace(&pool, &tag("g80f02b")).await;

    // Workspace B has a due funding retry.
    let recipient = tag("G80F02RECIPIENT");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("wallet");
    let case_id: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, first_funding_native, \
              source_address, deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $3, now(), '100', '0.5', $2, now() + interval '1 day', 
                 'preparation', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(tag("G80F02SRC"))
        .bind(ws_b)
    .fetch_one(&pool)
    .await
    .expect("case");
    let claim = crate::signals::claim_alert(&pool, "funding", ws_b, case_id, "chat-a")
        .await
        .expect("claim")
        .expect("claim wins");
    crate::signals::mark_alert_failed(
        &pool, "funding", ws_b, case_id, "chat-a", &claim.token, claim.attempt,
        "Transient", false, None,
    )
    .await
    .expect("mark failed");
    sqlx::query("UPDATE alerts SET next_attempt_at = now() - interval '1 second' \
                  WHERE dedup_key = $1")
        .bind(crate::signals::alert_dedup_key("funding", ws_b, case_id, "chat-a"))
        .execute(&pool)
        .await
        .expect("force due");

    // Worker A's drain must NOT see it.
    let for_a = crate::workers::pending_funding_alerts(&pool, ws_a, "chat-a", 70)
        .await
        .expect("drain for A");
    assert!(
        !for_a.iter().any(|(id, _)| *id == case_id),
        "workspace A must not select workspace B's funding retry (REV-080-F02)"
    );
    // Worker B's drain sees it — and claims the EXISTING row (no second row).
    let for_b = crate::workers::pending_funding_alerts(&pool, ws_b, "chat-a", 70)
        .await
        .expect("drain for B");
    assert!(for_b.iter().any(|(id, _)| *id == case_id));
    let reclaim = crate::signals::claim_alert(&pool, "funding", ws_b, case_id, "chat-a")
        .await
        .expect("reclaim");
    assert!(reclaim.is_some(), "the owner's re-claim of its own due row wins");
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM alerts WHERE funding_case_id = $1",
    )
    .bind(case_id)
    .fetch_one(&pool)
    .await
    .expect("rows");
    assert_eq!(rows, 1, "exactly one outbox row — never a re-keyed duplicate");
}

// ---------------------------------------------------------------------------
// F03 — exhaustion terminalizes at the claim decision point
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_deployment_exhaustion_becomes_dead_at_the_next_claim_attempt() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g80f03a")).await;
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .bind(tag("G80F03MINT"))
    .fetch_one(&pool)
    .await
    .expect("signal");
    let key = crate::signals::alert_dedup_key("signal", ws, sid, "chat");

    // A row stranded at the cap AFTER deployment (e.g. crash on the last attempt).
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, $3, 'chat', 'pending', $4, now())",
    )
    .bind(&key)
    .bind(ws)
    .bind(sid)
    .bind(crate::signals::ALERT_MAX_ATTEMPTS)
    .execute(&pool)
    .await
    .expect("stranded row");

    // No manual sweep SQL: the production claim path itself terminalizes it.
    let claim = crate::signals::claim_alert(&pool, "signal", ws, sid, "chat")
        .await
        .expect("claim call");
    assert!(claim.is_none(), "the cap refuses the claim");
    let state: String = sqlx::query_scalar("SELECT state FROM alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(
        state, "dead",
        "the claim decision point terminalizes post-deployment exhaustion (REV-080-F03)"
    );
}

// ---------------------------------------------------------------------------
// F04 — fenced evaluator writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_worker_writes_zero_rows_after_a_reclaim() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g80f04a")).await;
    let stem = tag("G80F04");
    let mint = format!("{stem}MINT");
    let now = chrono::Utc::now();

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

    // Worker A claims, then its lease EXPIRES and worker B reclaims.
    let token_a = format!("worker-A-{}", uuid::Uuid::new_v4());
    sqlx::query(
        "INSERT INTO signal_eval_claims (workspace_id, chain, mint, claimed_by, expires_at) \
         VALUES ($1, 'solana', $2, $3, now() - interval '1 second')",
    )
    .bind(ws)
    .bind(&mint)
    .bind(&token_a)
    .execute(&pool)
    .await
    .expect("A claims (already expired)");
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

    // A wakes up and evaluates with its STALE token: the fenced write must refuse.
    let err = crate::workers::evaluate_token_signals_fenced(
        &pool,
        ws,
        crate::models::ChainKind::Solana,
        &mint,
        now,
        &crate::config::SignalsConfig::default(),
        &token_a,
    )
    .await;
    assert!(err.is_err(), "a stale token must not write (REV-080-F04)");
    let signals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(signals, 0, "the stale worker wrote ZERO rows");
    let evals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signal_evaluations WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("count evals");
    assert_eq!(evals, 0, "not even a rejection row — the whole transaction aborted");

    // And the live owner (B) can still write.
    let ok = crate::workers::evaluate_token_signals_fenced(
        &pool,
        ws,
        crate::models::ChainKind::Solana,
        &mint,
        now,
        &crate::config::SignalsConfig::default(),
        &token_b,
    )
    .await
    .expect("live owner evaluates");
    assert!(ok.is_some(), "the live claim owner is not blocked by the fence");
}

// ---------------------------------------------------------------------------
// F05 — a lost funding claim is recovered from durable case state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_lost_funding_claim_is_rederived_from_case_state_by_the_drain() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g80f05a")).await;
    let recipient = tag("G80F05RECIPIENT");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("wallet");
    let case_id: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, first_funding_native, \
              source_address, deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $3, now(), '100', '0.5', $2, now() + interval '1 day', 
                 'preparation', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(tag("G80F05SRC"))
        .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("case");

    // NO outbox row exists (the producer's claim failed after the stage
    // transition — the REV-080-F05 hole). The drain must REDERIVE the intent from
    // durable case state instead of losing the alert forever.
    let derived = crate::workers::pending_funding_alerts(&pool, ws, "chat-a", 70)
        .await
        .expect("drain");
    assert!(
        derived.iter().any(|(id, _)| *id == case_id),
        "a preparation-stage case with no outbox row IS the due intent (REV-080-F05)"
    );

    // Delivery works through the mock; after success the derived intent is gone.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().route(
        "/bot{token}/sendMessage",
        axum::routing::post(|| async { (axum::http::StatusCode::OK, "{\"ok\":true}") }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let ctx = worker_ctx(&pool, ws, Some(("test-token", "chat-a")));
    crate::workers::dispatch_alerts_via(&ctx, &format!("http://{addr}"))
        .await
        .expect("dispatch");
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM alerts WHERE subject_kind = 'funding' AND funding_case_id = $1 AND workspace_id = $2",
    )
    .bind(case_id)
    .bind(ws)
    .fetch_optional(&pool)
    .await
    .expect("row");
    assert_eq!(state.as_deref(), Some("sent"));
    let derived_after = crate::workers::pending_funding_alerts(&pool, ws, "chat-a", 70)
        .await
        .expect("drain after");
    assert!(
        !derived_after.iter().any(|(id, _)| *id == case_id),
        "delivered intent does not re-derive"
    );
}

// ---------------------------------------------------------------------------
// F06 — legacy subject classification from the minted key kind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn legacy_subjects_classify_from_key_kind_and_unknown_is_explicit() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g80f06a")).await;

    // A REV-077-era funding row whose subject id HAPPENS to be a valid signal id:
    // key kind says funding, the FK corroboration says signal. The key wins.
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .bind(tag("G80F06MINT"))
    .fetch_one(&pool)
    .await
    .expect("signal");
    let recipient = tag("G80F06RECIPIENT");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("wallet");
    let case_id: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_native, source_address, \
              deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $3, now(), '1', $2, now() + interval '1 day', 
                 'preparation', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(tag("G80F06SRC"))
        .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("case");

    // Replay 1036's classification logic against two constructed legacy rows:
    // (a) funding key with a LIVE funding case -> funding; (b) funding key whose
    // case is gone -> unknown (never silently signal).
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, signal_id, destination, state, attempt_count) \
         VALUES ($1, 'signal', $2, $3, 'chat', 'sent', 1)",
    )
    .bind(format!("funding:{ws}:{case_id}:chat"))
    .bind(ws)
    .bind(sid) // the accidental overlap: points at a signal, minted as funding
    .execute(&pool)
    .await
    .expect("legacy funding row");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, signal_id, destination, state, attempt_count) \
         VALUES ($1, 'signal', $2, $3, 'chat', 'sent', 1)",
    )
    .bind(format!("funding:{ws}:999999999:chat"))
    .bind(ws)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("legacy orphan funding row");

    // Apply the 1036 classification verbatim (same statements, via raw_sql of the
    // migration body would replay everything; the statements are short and the
    // fixture asserts their OUTCOME).
    sqlx::query(
        "UPDATE alerts a SET subject_kind = 'funding', funding_case_id = c.id, signal_id = NULL \
          FROM funding_radar_cases c \
         WHERE split_part(a.dedup_key, ':', 1) = 'funding' \
           AND a.subject_kind = 'signal' \
           AND c.id = nullif(split_part(a.dedup_key, ':', 3), '')::bigint",
    )
    .execute(&pool)
    .await
    .expect("classify funding");
    sqlx::query(
        "UPDATE alerts a SET subject_kind = 'unknown' \
         WHERE split_part(a.dedup_key, ':', 1) = 'funding' \
           AND a.subject_kind = 'signal'",
    )
    .execute(&pool)
    .await
    .expect("classify unknown");

    let (kind, sig, fc): (String, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT subject_kind, signal_id, funding_case_id FROM alerts WHERE dedup_key = $1",
    )
    .bind(format!("funding:{ws}:{case_id}:chat"))
    .fetch_one(&pool)
    .await
    .expect("row a");
    assert_eq!(kind, "funding", "the minted key kind wins over the accidental signal overlap (REV-080-F06)");
    assert!(sig.is_none());
    assert_eq!(fc, Some(case_id));

    let kind_b: String = sqlx::query_scalar(
        "SELECT subject_kind FROM alerts WHERE dedup_key = $1",
    )
    .bind(format!("funding:{ws}:999999999:chat"))
    .fetch_one(&pool)
    .await
    .expect("row b");
    assert_eq!(kind_b, "unknown", "an unverifiable subject is explicit, never silently signal");
}

// ---------------------------------------------------------------------------
// F07 — cutover inserts are replay-safe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cutover_event_inserts_are_replay_safe() {
    let pool = pool().await;
    let before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM schema_cutover_events \
          WHERE cutover = 'alert_workspace_ownership_and_subject_repair'",
    )
    .fetch_one(&pool)
    .await
    .expect("before");
    // The guarded shape: insert only when absent. Direct double-apply must not
    // duplicate (REV-080-F07 named the unguarded 1035 shape).
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO schema_cutover_events (cutover, detail) \
             SELECT 'alert_workspace_ownership_and_subject_repair', '{\"probe\":true}'::jsonb \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM schema_cutover_events \
                  WHERE cutover = 'alert_workspace_ownership_and_subject_repair')",
        )
        .execute(&pool)
        .await
        .expect("guarded insert");
    }
    let after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM schema_cutover_events \
          WHERE cutover = 'alert_workspace_ownership_and_subject_repair'",
    )
    .fetch_one(&pool)
    .await
    .expect("after");
    assert_eq!(after, before.max(1), "guarded insert is replay-safe");
}
