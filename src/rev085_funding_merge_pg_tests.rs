//! Live PostgreSQL regression for REV-084-F03.
//!
//! Migration 1037 enforces `(workspace_id, chain, recipient)` as the funding case
//! identity. Its first shape kept the OLDEST duplicate and DELETED every newer one,
//! so a case that had advanced to `preparation` with confidence 90, fanout 9 and
//! fresh evidence was destroyed and the stale `funded`/55/1 row survived in its
//! place — silent, irreversible loss of the state the radar exists to produce.
//!
//! The fix made the dedup a SEMANTIC MERGE: the survivor absorbs the earliest
//! `first_funded_at`, the most advanced stage, the max confidence/fanout, the union
//! of evidence and the latest `updated_at`, and every FK reference
//! (`funding_radar_events.case_id`, `alerts.funding_case_id`) is repointed to it
//! before the duplicates are deleted.
//!
//! The probe migrates a scratch database through 1036 ONLY — the point where
//! `workspace_id` exists but the unique index does not, so duplicates are still
//! insertable — seeds the exact pair the reviewer described, then runs the FULL set
//! and asserts nothing was lost.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use sqlx::PgPool;

fn tag(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}{nanos}")
}

/// A scratch database migrated through 1036 ONLY: `funding_radar_cases.workspace_id`
/// exists and is NOT NULL, but `funding_radar_cases_tenant_uidx` (created by 1037)
/// does not, so the duplicate pair below is insertable.
async fn scratch_through_1036(
    name: &str,
) -> (PgPool, crate::pg_test_support::ScratchDb, String) {
    // REV-093-F06: the scratch database and its reduced-migration temp dir are owned
    // by a guard that also cleans up when a test unwinds.
    let mut scratch = crate::pg_test_support::ScratchDb::create(name).await;
    let red_dir = scratch.temp_dir("mig");
    let src_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&src_dir).expect("read migrations") {
        let entry = entry.expect("entry");
        let n = entry.file_name().to_string_lossy().to_string();
        // Everything from 1037 up is what this test is about; hold it back. A
        // lexicographic cutoff, not a prefix denylist: REV-087 added 1040, and a
        // denylist silently leaks every migration added after it was written.
        if n.ends_with(".sql") && n.as_str() >= "1037_" {
            continue;
        }
        std::fs::copy(entry.path(), red_dir.join(&n)).expect("copy");
    }
    let pool = scratch.pool().await;
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

    let nm = scratch.name().to_string();
    (pool, scratch, nm)
}

// ---------------------------------------------------------------------------
// REV-084-F03 — duplicate funding cases merge; newer state is never deleted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_funding_cases_merge_instead_of_losing_newer_state() {
    let (pool, admin, _scratch) = scratch_through_1036("a").await;

    let slug = tag("rev085ws");
    let ws: i64 = sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ($1, $2) RETURNING id")
        .bind(&slug)
        .bind(&slug)
        .fetch_one(&pool)
        .await
        .expect("workspace");

    let recipient = tag("REC");
    let source = tag("SRC");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("recipient wallet");

    // OLD: inserted first, so it wins the `ORDER BY id ASC` survivor slot — and under
    // the destructive dedup its stale values were ALL that remained.
    let old_case: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, \
              first_funding_native, source_address, deploy_window_ends_at, stage, \
              confidence, fanout_count, evidence, updated_at) \
         VALUES ('solana', $1, $2, timestamptz '2024-01-01 00:00:00+00', '100', '0.5', $3, \
                 now() + interval '1 day', 'funded', 55, 1, '{\"old\": true}'::jsonb, \
                 timestamptz '2024-01-02 00:00:00+00') \
         RETURNING id",
    )
    .bind(&recipient)
    .bind(ws)
    .bind(&source)
    .fetch_one(&pool)
    .await
    .expect("old case");

    // NEW: the advanced state the reviewer watched disappear.
    let new_case: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, \
              first_funding_native, source_address, deploy_window_ends_at, stage, \
              confidence, fanout_count, evidence, updated_at) \
         VALUES ('solana', $1, $2, timestamptz '2024-06-01 00:00:00+00', '200', '1.0', $3, \
                 now() + interval '1 day', 'preparation', 90, 9, '{\"new\": true}'::jsonb, \
                 timestamptz '2024-06-02 00:00:00+00') \
         RETURNING id",
    )
    .bind(&recipient)
    .bind(ws)
    .bind(&source)
    .fetch_one(&pool)
    .await
    .expect("new case");

    assert!(new_case > old_case, "the newer case must carry the higher id");

    // FK references hanging off the NEW case: the destructive dedup cascaded the
    // event away and nulled the alert's subject.
    let event_id: i64 = sqlx::query_scalar(
        "INSERT INTO funding_radar_events (case_id, event_kind, observed_at, evidence) \
         VALUES ($1, 'funded', now(), '{\"e\": 1}'::jsonb) RETURNING id",
    )
    .bind(new_case)
    .fetch_one(&pool)
    .await
    .expect("radar event");

    let dedup = format!("funding:{ws}:{new_case}:chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, funding_case_id, \
                             destination, state, attempt_count, next_attempt_at) \
         VALUES ($1, 'funding', $2, $3, 'chat-a', 'pending', 1, now())",
    )
    .bind(&dedup)
    .bind(ws)
    .bind(new_case)
    .execute(&pool)
    .await
    .expect("funding alert");

    // The whole point: 1037 must merge, not amputate.
    crate::db::migrate_with(&pool, true)
        .await
        .expect("full migration set must apply over the duplicate pair (REV-084-F03)");

    let survivors: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM funding_radar_cases \
          WHERE workspace_id = $1 AND chain = 'solana' AND recipient = $2",
    )
    .bind(ws)
    .bind(&recipient)
    .fetch_one(&pool)
    .await
    .expect("survivor count");
    assert_eq!(survivors, 1, "the identity must collapse to exactly one case");

    let (survivor_id, stage, confidence, fanout, early_funded, evidence): (
        i64,
        String,
        i32,
        i32,
        bool,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT id, stage, confidence, fanout_count, \
                first_funded_at = timestamptz '2024-01-01 00:00:00+00', evidence \
           FROM funding_radar_cases \
          WHERE workspace_id = $1 AND chain = 'solana' AND recipient = $2",
    )
    .bind(ws)
    .bind(&recipient)
    .fetch_one(&pool)
    .await
    .expect("survivor row");

    assert_eq!(survivor_id, old_case, "the lowest id stays the survivor row");
    assert_eq!(
        stage, "preparation",
        "the most advanced stage must survive the merge, not the oldest row's 'funded'"
    );
    assert_eq!(confidence, 90, "max confidence wins the merge");
    assert_eq!(fanout, 9, "max fanout_count wins the merge");
    assert!(early_funded, "the EARLIEST first_funded_at is the case's true origin");
    assert_eq!(
        evidence.get("old").and_then(serde_json::Value::as_bool),
        Some(true),
        "the survivor's own evidence must not be dropped"
    );
    assert_eq!(
        evidence.get("new").and_then(serde_json::Value::as_bool),
        Some(true),
        "the deleted duplicate's evidence must be absorbed"
    );

    // FK references follow the survivor rather than cascading into nothing.
    let event_case: i64 = sqlx::query_scalar("SELECT case_id FROM funding_radar_events WHERE id = $1")
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .expect("event still exists and is repointed");
    assert_eq!(event_case, survivor_id, "funding_radar_events.case_id must be repointed");

    // REV-086-F03: the FK repoint alone was not enough. `dedup_key` EMBEDS the case
    // id, so a repointed row kept the dead id in its identity and a later
    // `claim_alert` (ON CONFLICT (dedup_key)) minted a SECOND row for the same
    // survivor and destination. The identity is reconciled now, so the row is found
    // under the SURVIVOR's key and the old key is gone.
    let survivor_key = crate::signals::alert_dedup_key("funding", ws, survivor_id, "chat-a");
    let alert_case: Option<i64> =
        sqlx::query_scalar("SELECT funding_case_id FROM alerts WHERE dedup_key = $1")
            .bind(&survivor_key)
            .fetch_one(&pool)
            .await
            .expect("the alert survives under the survivor's identity");
    assert_eq!(
        alert_case,
        Some(survivor_id),
        "alerts.funding_case_id must be repointed, not nulled by ON DELETE SET NULL"
    );
    let stale: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE dedup_key = $1")
        .bind(&dedup)
        .fetch_one(&pool)
        .await
        .expect("stale key probe");
    assert_eq!(
        stale, 0,
        "the dead case id must not survive inside an alert identity (REV-086-F03)"
    );

    // REV-093-F06: teardown is the guard's, and it also runs on unwind.
    pool.close().await;
    drop(admin);
}
