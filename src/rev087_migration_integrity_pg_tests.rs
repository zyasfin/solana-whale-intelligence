//! Live PostgreSQL regressions for REV-086-F01 / REV-087-F01.
//!
//! The finding: the migrator accepted an edited shipped migration silently, and
//! the preflight that guards 1036's `::bigint` cast was shape-keyed,
//! non-transactional, and digit-only.
//!
//! What each test proves, and what it does NOT:
//!
//! * `an_edited_shipped_migration_is_refused` is a CURRENT-STATE proof — it edits
//!   a real applied migration on disk and shows `migrate_dir_with` refuses. This is
//!   the "authenticated predecessor" the reviewer asked for: the ledger digest is
//!   the authenticated artifact and the on-disk bytes are the divergence, which is
//!   exactly the drift a same-name edit produces on a live database.
//! * the three upgrade-lane tests are current-state proofs on a real reduced lane.
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

/// A scratch database plus a reduced migration directory that stops BEFORE
/// `cutoff` (a lexicographic filename prefix, so it never rots as migrations are
/// added). The directory is returned and passed EXPLICITLY to the migrator — no
/// process-global `SWI_MIGRATIONS_DIR` is touched, which is what let concurrent
/// fixtures corrupt each other's runs.
async fn scratch_before(
    label: &str,
    cutoff: &str,
) -> (PgPool, PgPool, String, std::path::PathBuf) {
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&crate::pg_test_support::require_live_url())
        .await
        .expect("admin pool");
    let scratch = format!("swi_r87_{label}_{}", std::process::id());
    sqlx::query(&format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop scratch");
    sqlx::query(&format!("CREATE DATABASE {scratch}"))
        .execute(&admin)
        .await
        .expect("create scratch");

    let red_dir = std::env::temp_dir().join(format!("swi_r87_mig_{scratch}"));
    let _ = std::fs::remove_dir_all(&red_dir);
    std::fs::create_dir_all(&red_dir).expect("mkdir reduced migrations");
    for entry in std::fs::read_dir(migrations_dir()).expect("read migrations") {
        let entry = entry.expect("dir entry");
        let n = entry.file_name().to_string_lossy().to_string();
        // MANIFEST.sha256 must travel with the SQL: the migrator fails closed
        // without it.
        if n.ends_with(".sql") && n.as_str() >= cutoff {
            continue;
        }
        std::fs::copy(entry.path(), red_dir.join(&n)).expect("copy migration");
    }

    let url = format!(
        "{}/{}",
        crate::pg_test_support::require_live_url()
            .rsplitn(2, '/')
            .nth(1)
            .expect("db url base"),
        scratch
    );
    let pool = crate::db::connect(&url, 2).await.expect("scratch pool");
    crate::db::migrate_dir_with(&pool, &red_dir, true)
        .await
        .unwrap_or_else(|e| panic!("migrate below {cutoff}: {e:#}"));
    (pool, admin, scratch, red_dir)
}

async fn drop_scratch(admin: &PgPool, pool: PgPool, scratch: &str, dir: &std::path::Path) {
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"))
        .execute(admin)
        .await
        .expect("drop scratch");
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// REV-086-F01 — same-name digest drift must be refused BY THE MIGRATOR
// ---------------------------------------------------------------------------

/// The migrator's applied-row branch compared a recorded digest against the file
/// on disk only when the digest was MISSING. A row carrying both a digest and a
/// provenance fell through to `continue`, so `db migrate` blessed an edited
/// shipped migration and only a later service start noticed.
#[tokio::test]
async fn an_edited_shipped_migration_is_refused_by_the_migrator() {
    let (pool, admin, scratch, dir) = scratch_before("drift", "1034_").await;

    // Precondition: re-running an UNMODIFIED set is a clean no-op. Without this the
    // negative assertion below could pass for the wrong reason.
    crate::db::migrate_dir_with(&pool, &dir, true)
        .await
        .expect("re-running an unmodified migration set must be a no-op");

    // Now edit a migration that this lane has already applied. A trailing comment
    // keeps the file valid SQL, so the ONLY thing that can reject it is the digest.
    let victim = dir.join("1033_rev074_signal_uniqueness_and_alert_outbox.sql");
    let original = std::fs::read(&victim).expect("read applied migration");
    let mut tampered = original.clone();
    tampered.extend_from_slice(b"\n-- REV-087 drift probe\n");
    std::fs::write(&victim, &tampered).expect("write tampered migration");

    let err = crate::db::migrate_dir_with(&pool, &dir, true)
        .await
        .expect_err("an edited applied migration must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no longer matches the file on disk"),
        "expected a same-name drift refusal, got: {msg}"
    );

    // Restoring the bytes restores the lane: the refusal is about the CONTENT, not
    // about having run twice.
    std::fs::write(&victim, &original).expect("restore migration");
    crate::db::migrate_dir_with(&pool, &dir, true)
        .await
        .expect("the restored set verifies again");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-086-F01 — range safety: a digit-only guard is not an int8 guard
// ---------------------------------------------------------------------------

/// `9223372036854775808` is `i64::MAX + 1`: all digits, so the old `^[0-9]+$`
/// preflight guard let it through, and 1036's `nullif(split_part(...))::bigint`
/// then aborted the whole migration with 22003 `numeric_value_out_of_range`.
#[tokio::test]
async fn an_oversized_numeric_workspace_segment_survives_the_1036_cast() {
    let (pool, admin, scratch, dir) = scratch_before("bigint", "1036_").await;

    let ws: i64 = sqlx::query_scalar(
        "INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id",
    )
    .bind(tag("r87big"))
    .fetch_one(&pool)
    .await
    .expect("workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R87BIGMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");
    // The second segment is where 1036 reads the workspace id.
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ('signal:9223372036854775808:R87BIGMINT:chat-a', 'signal', $1, 'chat-a', \
                 'pending', 1, now())",
    )
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed oversized-segment alert");

    // Precondition: the fixture really is capable of detecting the defect — the
    // segment is digit-only, so a `^[0-9]+$` guard would classify it as SAFE.
    let digit_only: bool = sqlx::query_scalar(
        "SELECT split_part(dedup_key, ':', 2) ~ '^[0-9]+$' FROM public.alerts \
          WHERE dedup_key LIKE 'signal:9223372036854775808:%'",
    )
    .fetch_one(&pool)
    .await
    .expect("segment shape");
    assert!(digit_only, "the probe value must be digit-only, or it proves nothing");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("an oversized numeric segment must not abort 1036 (REV-086-F01)");

    let (assigned, default_ws): (i64, i64) = sqlx::query_as(
        "SELECT a.workspace_id, (SELECT id FROM workspaces WHERE slug = 'default') \
           FROM public.alerts a WHERE a.dedup_key LIKE 'signal:9223372036854775808:%'",
    )
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert_eq!(
        assigned, default_ws,
        "an unparseable workspace segment lands in the default workspace"
    );

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-086-F01 — the preflight must be idempotent across a crash window
// ---------------------------------------------------------------------------

/// The REV-085 preflight returned early when `alerts.workspace_id` merely EXISTED.
/// A crash between its `ADD COLUMN` and its backfill therefore left the column
/// present with NULLs, and every later run short-circuited past the repair — so
/// 1036's cast aborted forever. The precondition is now the DATA state.
#[tokio::test]
async fn a_half_finished_preflight_is_completed_on_the_next_run() {
    let (pool, admin, scratch, dir) = scratch_before("halfway", "1036_").await;

    let ws: i64 = sqlx::query_scalar(
        "INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id",
    )
    .bind(tag("r87half"))
    .fetch_one(&pool)
    .await
    .expect("workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R87HALFMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ('signal:solana:R87HALFMINT:entry', 'signal', $1, 'chat-a', 'pending', 1, now())",
    )
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed legacy-key alert");

    // Simulate the crash window: the column exists, nothing was backfilled.
    sqlx::query(
        "ALTER TABLE public.alerts \
         ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id)",
    )
    .execute(&pool)
    .await
    .expect("half-finished preflight");

    // Precondition: the row really is unassigned, so the repair has work to do.
    let unassigned: bool = sqlx::query_scalar(
        "SELECT workspace_id IS NULL FROM public.alerts \
          WHERE dedup_key = 'signal:solana:R87HALFMINT:entry'",
    )
    .fetch_one(&pool)
    .await
    .expect("assignment state");
    assert!(unassigned, "the crash window must leave the row unassigned");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a half-finished preflight must be completed, not skipped (REV-086-F01)");

    let assigned: Option<i64> = sqlx::query_scalar(
        "SELECT workspace_id FROM public.alerts \
          WHERE dedup_key = 'signal:solana:R87HALFMINT:entry'",
    )
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert!(assigned.is_some(), "the repair completed on the next run");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-086-F01 — a pre-1033 lane must not trip over post-1033 columns
// ---------------------------------------------------------------------------

/// The preflight runs before EVERY migration, including on a database whose
/// `alerts` predates 1033 and has no `state` column. Referencing one would abort
/// the preflight itself (the REV-082-F03 trap).
#[tokio::test]
async fn a_pre_1033_alert_history_upgrades_through_the_preflight() {
    let (pool, admin, scratch, dir) = scratch_before("pre1033", "1033_").await;

    let has_state: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'alerts' AND column_name = 'state')",
    )
    .fetch_one(&pool)
    .await
    .expect("state column probe");
    assert!(!has_state, "the pre-1033 lane must not have alerts.state yet");

    let ws: i64 = sqlx::query_scalar(
        "INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id",
    )
    .bind(tag("r87pre"))
    .fetch_one(&pool)
    .await
    .expect("workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R87PREMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, signal_id) \
         VALUES ('signal:solana:R87PREMINT:entry', $1)",
    )
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed pre-1033 alert");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a pre-1033 lane with legacy keys must upgrade unassisted");

    let assigned: Option<i64> = sqlx::query_scalar(
        "SELECT workspace_id FROM public.alerts \
          WHERE dedup_key = 'signal:solana:R87PREMINT:entry'",
    )
    .fetch_one(&pool)
    .await
    .expect("read back");
    assert!(assigned.is_some(), "the legacy row was assigned an owner");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}
