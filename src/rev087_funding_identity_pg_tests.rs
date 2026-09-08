//! Live PostgreSQL regressions for REV-086-F03 (funding-case merge completeness).
//!
//! The finding named three gaps in shipped migration 1037:
//!
//!   1. it merges 6 of the 17 case columns, so `first_funding_usd`,
//!      `first_funding_native`, `source_address`, `source_kind`,
//!      `deploy_window_ends_at` and `dismissed_reason` are silently taken from the
//!      lowest-id row even when that row is not the earliest-funded one;
//!   2. its `jsonb_each` is reached by `CROSS JOIN LATERAL` and raises 22023 on
//!      `null`/array evidence — and `'null'::jsonb` is the column's own DEFAULT;
//!   3. its alert repoint rewrites `funding_case_id` but leaves `dedup_key`, which
//!      embeds the dead case id, so a later claim mints a duplicate logical alert.
//!
//! 1037 is shipped and runs BEFORE any later migration in filename order, so the
//! repair lives in `db::preflight_repair_funding_case_identity`. This module proves
//! the whole lane end to end on a real reduced database.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use sqlx::PgPool;

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent")
        .join("swi-deploy/migrations")
}

fn tag(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}{nanos}")
}

/// A scratch database migrated through 1036 — `funding_radar_cases` carries a
/// workspace but 1037's identity index does not exist yet, so duplicates are
/// insertable. The reduced set is passed explicitly; no env var is mutated.
async fn scratch_through_1036(_label: &str) -> (PgPool, crate::pg_test_support::ScratchDb, String, std::path::PathBuf) {
        // REV-093-F06: guard-owned; cleans up on unwind too.
    let scratch_guard = crate::pg_test_support::ScratchDb::create("swi_r87f03").await;
    let scratch = scratch_guard.name().to_string();

    let red_dir = std::env::temp_dir().join(format!("swi_r87f03_mig_{scratch}"));
    let _ = std::fs::remove_dir_all(&red_dir);
    std::fs::create_dir_all(&red_dir).expect("mkdir");
    for entry in std::fs::read_dir(migrations_dir()).expect("read migrations") {
        let entry = entry.expect("entry");
        let n = entry.file_name().to_string_lossy().to_string();
        if n.ends_with(".sql") && n.as_str() >= "1037_" {
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
        .expect("migrate through 1036");

    let uidx: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname = 'public' \
           AND indexname = 'funding_radar_cases_tenant_uidx')",
    )
    .fetch_one(&pool)
    .await
    .expect("uidx probe");
    assert!(!uidx, "the reduced set must stop BEFORE 1037 creates the identity index");

    (pool, scratch_guard, scratch, red_dir)
}

async fn drop_scratch(
    _guard: &crate::pg_test_support::ScratchDb,
    pool: PgPool,
    _scratch: &str,
    _dir: &std::path::Path,
) {
    // REV-093-F06: teardown belongs to the guard, which also runs on unwind.
    pool.close().await;
}

#[allow(clippy::too_many_arguments)]
async fn seed_case(
    pool: &PgPool,
    ws: i64,
    recipient: &str,
    funded_at: &str,
    usd: &str,
    native: &str,
    source_address: &str,
    source_kind: Option<&str>,
    window_ends: &str,
    stage: &str,
    confidence: i32,
    fanout: i32,
    evidence: &str,
    updated_at: &str,
    dismissed_reason: Option<&str>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, \
              first_funding_native, source_address, source_kind, deploy_window_ends_at, \
              stage, confidence, fanout_count, evidence, updated_at, dismissed_reason) \
         VALUES ('solana', $1, $2, $3::timestamptz, $4::numeric, $5::numeric, $6, $7, \
                 $8::timestamptz, $9, $10, $11, $12::jsonb, $13::timestamptz, $14) \
         RETURNING id",
    )
    .bind(recipient)
    .bind(ws)
    .bind(funded_at)
    .bind(usd)
    .bind(native)
    .bind(source_address)
    .bind(source_kind)
    .bind(window_ends)
    .bind(stage)
    .bind(confidence)
    .bind(fanout)
    .bind(evidence)
    .bind(updated_at)
    .bind(dismissed_reason)
    .fetch_one(pool)
    .await
    .expect("seed funding case")
}

// ---------------------------------------------------------------------------
// REV-086-F03 — every semantic column merges; legal JSON shapes never abort;
// alert identity is reconciled with a deterministic collision policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_cases_merge_every_column_and_reconcile_alert_identity() {
    let (pool, admin, scratch, dir) = scratch_through_1036("merge").await;

    let ws: i64 = sqlx::query_scalar(
        "INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id",
    )
    .bind(tag("r87f03"))
    .fetch_one(&pool)
    .await
    .expect("workspace");
    // funding_radar_cases (chain, recipient) references wallets (chain, address).
    let recipient = tag("R87F03RECIP");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("wallet");

    // Three duplicates of ONE identity. The id order deliberately disagrees with
    // the funding order, so taking the unmerged columns from the id-survivor (what
    // 1037 does) produces a different answer than taking them from the
    // earliest-funded row (what the merge policy requires).
    //
    // lowest id  — LATEST funded, evidence is the schema's scalar default;
    // middle id  — EARLIEST funded, evidence is a legal JSON ARRAY;
    // highest id — dismissed with a reason, widest deploy window, newest update.
    let low = seed_case(
        &pool, ws, &recipient,
        "2026-03-03T00:00:00Z", "1", "1", "SRC_LATE", None,
        "2026-03-04T00:00:00Z", "funded", 55, 1,
        "null", "2026-03-03T00:00:00Z", None,
    ).await;
    let mid = seed_case(
        &pool, ws, &recipient,
        "2026-03-01T00:00:00Z", "42", "7", "SRC_FIRST", Some("cex"),
        "2026-03-05T00:00:00Z", "preparation", 90, 9,
        "[1, 2]", "2026-03-04T00:00:00Z", None,
    ).await;
    let high = seed_case(
        &pool, ws, &recipient,
        "2026-03-02T00:00:00Z", "5", "2", "SRC_MID", Some("bridge"),
        "2026-03-09T00:00:00Z", "dismissed", 60, 4,
        r#"{"new": true}"#, "2026-03-08T00:00:00Z", Some("noise"),
    ).await;
    assert!(low < mid && mid < high, "ids must ascend for the survivor to be `low`");

    // Precondition: the fixture can genuinely detect the defect.
    let dupes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM funding_radar_cases WHERE workspace_id = $1 AND recipient = $2",
    )
    .bind(ws)
    .bind(&recipient)
    .fetch_one(&pool)
    .await
    .expect("duplicate count");
    assert_eq!(dupes, 3, "three rows share one identity before the merge");
    let non_object: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM funding_radar_cases \
          WHERE workspace_id = $1 AND recipient = $2 AND jsonb_typeof(evidence) <> 'object'",
    )
    .bind(ws)
    .bind(&recipient)
    .fetch_one(&pool)
    .await
    .expect("evidence shapes");
    assert_eq!(non_object, 2, "a scalar and an array must be present, or 1037 never aborts");

    // History on a NON-survivor: an event, and two alerts.
    sqlx::query(
        "INSERT INTO funding_radar_events (case_id, event_kind, observed_at, evidence) \
         VALUES ($1, 'funding', now(), '{}'::jsonb)",
    )
    .bind(high)
    .execute(&pool)
    .await
    .expect("event");
    // (a) an alert keyed to the doomed case, already DELIVERED;
    let stale_key = crate::signals::alert_dedup_key("funding", ws, high, "chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, funding_case_id, \
                             destination, state, attempt_count, sent_at) \
         VALUES ($1, 'funding', $2, $3, 'chat-a', 'sent', 1, now())",
    )
    .bind(&stale_key)
    .bind(ws)
    .bind(high)
    .execute(&pool)
    .await
    .expect("stale-keyed sent alert");
    // (b) an alert ALREADY holding the survivor's key, still pending — the
    //     collision the rewrite has to resolve.
    let survivor_key = crate::signals::alert_dedup_key("funding", ws, low, "chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, funding_case_id, \
                             destination, state, attempt_count, next_attempt_at) \
         VALUES ($1, 'funding', $2, $3, 'chat-a', 'pending', 1, now())",
    )
    .bind(&survivor_key)
    .bind(ws)
    .bind(low)
    .execute(&pool)
    .await
    .expect("survivor-keyed pending alert");

    // The whole lane: preflight repair, then 1037..head.
    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("non-object evidence must not abort the merge (REV-086-F03)");

    let (
        remaining, stage, confidence, fanout, funded_at, updated_at,
        usd, native, src_addr, src_kind, window_ends, reason, evidence,
    ): (
        i64, String, i32, i32, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>,
        Option<rust_decimal::Decimal>, rust_decimal::Decimal, String, Option<String>,
        chrono::DateTime<chrono::Utc>, Option<String>, serde_json::Value,
    ) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM funding_radar_cases WHERE workspace_id = $1 AND recipient = $2), \
                stage, confidence, fanout_count, first_funded_at, updated_at, \
                first_funding_usd, first_funding_native, source_address, source_kind, \
                deploy_window_ends_at, dismissed_reason, evidence \
           FROM funding_radar_cases WHERE workspace_id = $1 AND recipient = $2",
    )
    .bind(ws)
    .bind(&recipient)
    .fetch_one(&pool)
    .await
    .expect("survivor");

    assert_eq!(remaining, 1, "exactly one case survives");
    // The six fields 1037 already merged.
    assert_eq!(stage, "dismissed", "the most advanced stage wins");
    assert_eq!(confidence, 90, "confidence is the maximum observed");
    assert_eq!(fanout, 9, "fanout is the maximum observed");
    assert_eq!(funded_at.to_rfc3339(), "2026-03-01T00:00:00+00:00", "earliest funding wins");
    assert_eq!(updated_at.to_rfc3339(), "2026-03-08T00:00:00+00:00", "latest update wins");
    // The six fields REV-086-F03 says are silently dropped. These are the
    // assertions that fail against the unrepaired lane.
    assert_eq!(
        usd.map(|d| d.normalize().to_string()),
        Some("42".to_string()),
        "first_funding_usd must travel with the earliest funding, not the lowest id"
    );
    assert_eq!(native.normalize().to_string(), "7", "first_funding_native likewise");
    assert_eq!(src_addr, "SRC_FIRST", "the source address of the FIRST funding");
    assert_eq!(src_kind.as_deref(), Some("cex"), "the source kind of the FIRST funding");
    assert_eq!(
        window_ends.to_rfc3339(), "2026-03-09T00:00:00+00:00",
        "the widest still-open deploy window survives"
    );
    assert_eq!(
        reason.as_deref(), Some("noise"),
        "a dismissal reason is kept when the merged stage is dismissed"
    );
    // Evidence: the object key survives AND the two non-object shapes are preserved
    // under a named key rather than discarded.
    assert_eq!(evidence.get("new").and_then(|v| v.as_bool()), Some(true));
    assert!(
        evidence.get("legacy_evidence").is_some(),
        "scalar/array evidence is preserved, not dropped: {evidence}"
    );

    // The event followed the survivor (its FK is ON DELETE CASCADE, so a missed
    // repoint would have destroyed it outright).
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM funding_radar_events e \
           JOIN funding_radar_cases c ON c.id = e.case_id \
          WHERE c.workspace_id = $1 AND c.recipient = $2",
    )
    .bind(ws)
    .bind(&recipient)
    .fetch_one(&pool)
    .await
    .expect("event count");
    assert_eq!(events, 1, "the event was repointed, not cascaded away");

    // ONE logical alert for the survivor+destination, keyed to the SURVIVOR, and
    // carrying the delivery fact forward: the stale-keyed row had been sent, so
    // re-sending it would be a duplicate external message.
    let alerts: Vec<(String, Option<i64>, String)> = sqlx::query_as(
        "SELECT dedup_key, funding_case_id, state FROM alerts \
          WHERE workspace_id = $1 AND subject_kind = 'funding' AND destination = 'chat-a' \
          ORDER BY dedup_key",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .expect("alerts");
    assert_eq!(alerts.len(), 1, "one logical alert, not two: {alerts:?}");
    assert_eq!(alerts[0].0, survivor_key, "the identity names the survivor");
    assert_eq!(alerts[0].1, Some(low), "the FK points at the survivor");
    assert_eq!(alerts[0].2, "sent", "the delivery fact was folded forward, not lost");

    // And the identity index 1037 exists to create is now in place.
    let uidx: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname = 'public' \
           AND indexname = 'funding_radar_cases_tenant_uidx')",
    )
    .fetch_one(&pool)
    .await
    .expect("uidx probe");
    assert!(uidx, "1037 completed once the duplicates were resolved");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}
