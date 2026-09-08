//! Live PostgreSQL regressions for REV-082 F01–F05.
//!
//! Each test reproduces the reviewer's own probe:
//!
//! * F01 — two workspaces ingest the same recipient: each gets its own case;
//!   `fetch_case` is workspace-scoped; `evaluate_radar_cases` is workspace-scoped;
//! * F02 — migration 1036 replay with a legacy nonnumeric dedup key (e.g.
//!   `signal:solana:MINT:entry`) succeeds: the regex guard routes the row to the
//!   default workspace instead of aborting on a `::bigint` cast;
//! * F03 — preflight on a pre-1033 database (alerts exists, no `state` column)
//!   succeeds: the `state`-column guard returns early instead of aborting;
//! * F04 — `pending_funding_alerts` uses the configured threshold, not a hardcoded
//!   70: a case at confidence=60 is found with threshold=50 but not with 80;
//! * F05 — concurrent attempt-7→8: terminalization does not kill a live claim;
//!   the second claim returns None; `mark_alert_sent` with the first claim's token
//!   succeeds.
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

// ---------------------------------------------------------------------------
// F01 — two workspaces ingest the same recipient: each gets its own case
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_recipient_funding_cases_are_workspace_independent() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("g82f01a")).await;
    let ws_b = workspace(&pool, &tag("g82f01b")).await;
    let recipient = tag("G82F01RECIPIENT");

    // Insert a wallet for the recipient.
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("wallet");

    // Insert a funding_radar_case for ws_a.
    let case_a: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, first_funding_native, \
              source_address, deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $2, now(), '100', '0.5', $3, now() + interval '1 day', \
                 'preparation', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(ws_a)
    .bind(tag("G82F01SRCA"))
    .fetch_one(&pool)
    .await
    .expect("case a");

    // Insert a funding_radar_case for ws_b with the SAME recipient.
    let case_b: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, first_funding_native, \
              source_address, deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $2, now(), '200', '1.0', $3, now() + interval '1 day', \
                 'preparation', 90, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(ws_b)
    .bind(tag("G82F01SRCB"))
    .fetch_one(&pool)
    .await
    .expect("case b");

    assert_ne!(case_a, case_b, "two workspaces must have distinct cases");

    // fetch_case scoped to ws_a returns ws_a's case, NOT ws_b's.
    let fetched_a = crate::funding_radar::fetch_case(
        &pool, ws_a, crate::models::ChainKind::Solana, &recipient,
    )
    .await
    .expect("fetch_case a")
    .expect("case exists for ws_a");
    assert_eq!(fetched_a.id, case_a, "ws_a must see its own case");

    // fetch_case scoped to ws_b returns ws_b's case.
    let fetched_b = crate::funding_radar::fetch_case(
        &pool, ws_b, crate::models::ChainKind::Solana, &recipient,
    )
    .await
    .expect("fetch_case b")
    .expect("case exists for ws_b");
    assert_eq!(fetched_b.id, case_b, "ws_b must see its own case");

    // evaluate_radar_cases with ctx for ws_a does not evaluate ws_b's case.
    let ctx_a = worker_ctx(&pool, ws_a, None);
    let evaluated_a = crate::workers::evaluate_radar_cases(&ctx_a)
        .await
        .expect("evaluate_radar_cases for ws_a");
    // ws_b's case should NOT be among those evaluated by ws_a's worker.
    // We verify by checking that ws_b's case stage/confidence is unchanged.
    let (_, _, _, _, _, _, _, _, _, _, stage_b, conf_b, _, _): (
        i64, String, String, chrono::DateTime<chrono::Utc>, Option<rust_decimal::Decimal>,
        rust_decimal::Decimal, String, Option<String>, i32, chrono::DateTime<chrono::Utc>,
        String, i32, serde_json::Value, chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as(
        "SELECT id, chain, recipient, first_funded_at, first_funding_usd, first_funding_native, \
                source_address, source_kind, fanout_count, deploy_window_ends_at, stage, \
                confidence, evidence, updated_at \
         FROM funding_radar_cases WHERE id = $1",
    )
    .bind(case_b)
    .fetch_one(&pool)
    .await
    .expect("ws_b case unchanged");
    assert_eq!(stage_b, "preparation", "ws_b case stage must be unchanged after ws_a evaluation");
    assert_eq!(conf_b, 90, "ws_b case confidence must be unchanged after ws_a evaluation");
    // evaluated_a counts only ws_a's open cases, not ws_b's.
    let _ = evaluated_a; // evaluated count is informational; the isolation assertion is above.
}

// ---------------------------------------------------------------------------
// F02 — migration 1036 replay with legacy nonnumeric dedup key succeeds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn legacy_nonnumeric_dedup_key_upgrade_succeeds() {
    // REV-093-F06: guard-owned teardown, also on unwind.
    let mut admin = crate::pg_test_support::ScratchDb::create("f02up").await;
    let scratch = admin.name().to_string();
    let src_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .join("swi-deploy/migrations");
    // REV-087 item 7: per-fixture temp dir (pid alone collides across fixtures).
    let red_dir = admin.temp_dir("f02");
    for entry in std::fs::read_dir(&src_dir).expect("read migrations") {
        let entry = entry.expect("entry");
        let n = entry.file_name().to_string_lossy().to_string();
        // Migrate through 1035; a lexicographic cutoff, not a prefix denylist that
        // rots as migrations are added (1039, then 1040).
        if n.ends_with(".sql") && n.as_str() >= "1036_" {
            continue;
        }
        std::fs::copy(entry.path(), red_dir.join(&n)).expect("copy");
    }
    let url = format!(
        "{}/{}",
        crate::pg_test_support::require_live_url()
            .rsplitn(2, '/')
            .nth(1)
            .expect("db url"),
        scratch
    );
    let pool = crate::db::connect(&url, 2).await.expect("scratch pool");
    crate::db::migrate_dir_with(&pool, &red_dir, true)
        .await
        .expect("migrate through 1035");

    // Seed a workspace and an alert with a legacy nonnumeric dedup key.
    sqlx::query("INSERT INTO workspaces (name, slug) VALUES ('w','f02w') ON CONFLICT DO NOTHING")
        .execute(&pool)
        .await
        .expect("ws");
    let ws: i64 = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'f02w'")
        .fetch_one(&pool)
        .await
        .expect("ws id");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'F02MINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, attempt_count, next_attempt_at) \
         VALUES ('signal:solana:F02MINT:entry', 'signal', $1, 'chat-legacy', 'pending', 1, now())",
    )
    .bind(sid)
    .execute(&pool)
    .await
    .expect("legacy alert");

    // Now apply 1036 (and beyond) using the FULL migration set.
    let full_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .join("swi-deploy/migrations");
    crate::db::migrate_dir_with(&pool, &full_dir, true)
        .await
        .expect("1036 with nonnumeric dedup key must succeed (REV-082-F02)");

    // The legacy alert's workspace_id must be the default workspace.
    let default_ws: i64 = sqlx::query_scalar(
        "SELECT id FROM workspaces WHERE slug = 'default'",
    )
    .fetch_one(&pool)
    .await
    .expect("default workspace");
    let alert_ws: i64 = sqlx::query_scalar(
        "SELECT workspace_id FROM alerts WHERE dedup_key = 'signal:solana:F02MINT:entry'",
    )
    .fetch_one(&pool)
    .await
    .expect("alert workspace_id");
    assert_eq!(
        alert_ws, default_ws,
        "nonnumeric second segment must fall through to the default workspace (REV-082-F02)"
    );

    // REV-093-F06: the guard drops the database, on success and on unwind alike.
    drop(admin);
}

// ---------------------------------------------------------------------------
// F03 — pre-1033 alerts table survives preflight
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_1033_alerts_table_survives_preflight() {
    // REV-093-F06: guard-owned teardown, also on unwind.
    let mut admin = crate::pg_test_support::ScratchDb::create("f03up").await;
    let scratch = admin.name().to_string();
    let src_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .join("swi-deploy/migrations");
    let red_dir = admin.temp_dir("f03");
    for entry in std::fs::read_dir(&src_dir).expect("read migrations") {
        let entry = entry.expect("entry");
        let n = entry.file_name().to_string_lossy().to_string();
        // Migrate through 1032 only: alerts exists, no state column.
        if n.ends_with(".sql") && n.as_str() >= "1033_" {
            continue;
        }
        std::fs::copy(entry.path(), red_dir.join(&n)).expect("copy");
    }
    let url = format!(
        "{}/{}",
        crate::pg_test_support::require_live_url()
            .rsplitn(2, '/')
            .nth(1)
            .expect("db url"),
        scratch
    );
    let pool = crate::db::connect(&url, 2).await.expect("scratch pool");
    crate::db::migrate_dir_with(&pool, &red_dir, true)
        .await
        .expect("migrate through 1032");

    // Verify pre-1033 shape: alerts exists but has no `state` column.
    let has_state: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'alerts' AND column_name = 'state')",
    )
    .fetch_one(&pool)
    .await
    .expect("check state column");
    assert!(!has_state, "pre-1033 lane must not have state column yet");

    // Run preflight + remaining migrations — must succeed (REV-082-F03).
    let full_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .join("swi-deploy/migrations");
    crate::db::migrate_dir_with(&pool, &full_dir, true)
        .await
        .expect("preflight on pre-1033 database must succeed (REV-082-F03)");

    // After full migration, the state column exists.
    let has_state_after: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'alerts' AND column_name = 'state')",
    )
    .fetch_one(&pool)
    .await
    .expect("check state column after");
    assert!(has_state_after, "post-migration lane must have state column");

    // REV-093-F06: the guard drops the database, on success and on unwind alike.
    drop(admin);
}

// ---------------------------------------------------------------------------
// F04 — funding recovery uses the configured threshold
// ---------------------------------------------------------------------------

#[tokio::test]
async fn funding_recovery_uses_configured_threshold() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g82f04a")).await;
    let recipient = tag("G82F04RECIPIENT");

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
         VALUES ('solana', $1, $2, now(), '100', '0.5', $3, now() + interval '1 day', \
                 'preparation', 60, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(ws)
    .bind(tag("G82F04SRC"))
    .fetch_one(&pool)
    .await
    .expect("case");

    // Threshold 50 < confidence 60: the case is found.
    let found = crate::workers::pending_funding_alerts(&pool, ws, "chat-a", 50)
        .await
        .expect("drain with threshold 50");
    assert!(
        found.iter().any(|(id, _)| *id == case_id),
        "confidence=60 must be found with threshold=50 (REV-082-F04)"
    );

    // Threshold 80 > confidence 60: the case is NOT found.
    let not_found = crate::workers::pending_funding_alerts(&pool, ws, "chat-a", 80)
        .await
        .expect("drain with threshold 80");
    assert!(
        !not_found.iter().any(|(id, _)| *id == case_id),
        "confidence=60 must NOT be found with threshold=80 (REV-082-F04)"
    );
}

// ---------------------------------------------------------------------------
// F05 — terminalization preserves a live claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn terminalization_preserves_a_live_claim() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g82f05a")).await;
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .bind(tag("G82F05MINT"))
    .fetch_one(&pool)
    .await
    .expect("signal");
    let key = crate::signals::alert_dedup_key("signal", ws, sid, "chat");

    // Seed an alert row at attempt_count=7 (one below the cap), state=pending,
    // next_attempt_at=now() — eligible for claim.
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, $3, 'chat', 'pending', 7, now())",
    )
    .bind(&key)
    .bind(ws)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed attempt-7 row");

    // First claim: attempt 8, gets a live lease.
    let claim_a = crate::signals::claim_alert(&pool, "signal", ws, sid, "chat")
        .await
        .expect("first claim")
        .expect("first claim wins attempt 8");
    assert_eq!(claim_a.attempt, 8, "first claim must be attempt 8");

    // Second claim with the same key: the terminalization UPDATE must NOT kill
    // the live claim. The claim must return None (row is claimed).
    let claim_b = crate::signals::claim_alert(&pool, "signal", ws, sid, "chat")
        .await
        .expect("second claim call");
    assert!(
        claim_b.is_none(),
        "second claim must return None — the row is claimed (REV-082-F05)"
    );

    // The row must still be pending (not dead), with the first claim's token.
    let (state, token): (String, Option<String>) = sqlx::query_as(
        "SELECT state, claim_token FROM alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("row state");
    assert_eq!(state, "pending", "live claim must NOT be terminalized to dead");
    assert_eq!(
        token.as_deref(),
        Some(claim_a.token.as_str()),
        "claim token must belong to the first claimant"
    );

    // mark_alert_sent with the first claim's token succeeds.
    let sent = crate::signals::mark_alert_sent(&pool, "signal", ws, sid, "chat", &claim_a.token)
        .await
        .expect("mark sent");
    assert!(sent, "mark_alert_sent with the live claim's token must succeed");
}
