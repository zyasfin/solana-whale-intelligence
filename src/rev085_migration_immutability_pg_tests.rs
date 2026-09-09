//! Live PostgreSQL regressions for REV-084-F01 — migration immutability and the
//! legacy dedup-key upgrade lane.
//!
//! REV-082-F02 correctly found that 1036's
//! `nullif(split_part(dedup_key, ':', 2), '')::bigint` aborts on a legacy key
//! whose second segment is nonnumeric. REV-083 fixed it by EDITING 1036 — a
//! shipped, reviewed, digest-recorded migration. That is the defect REV-084-F01
//! reports: the repair belongs in the migrator preflight
//! (`db::preflight_repair_legacy_dedup_keys`) plus a forward migration (1039),
//! never in the bytes of a migration operators already applied.
//!
//! * digest guard — 1036 must hash to its reviewed digest, and `MANIFEST.sha256`
//!   must record that same digest, so a future round cannot quietly re-edit it;
//! * a database stopped at the reviewed 1036 must upgrade to current;
//! * a pre-1036 database carrying nonnumeric dedup keys must upgrade UNASSISTED
//!   (preflight repair + byte-identical 1036), with the legacy row owned by the
//!   default workspace.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// The digest REV-082 independently reviewed for 1036, LF-normalized.
const REVIEWED_1036_DIGEST: &str =
    "6e2ff8e0e7637eb092d36fab5cb3540c743741a5f804fd4cf07da364cf4218e8";
const MIGRATION_1036: &str = "1036_rev080_alert_workspace_subject_cutover.sql";

fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent")
        .join("swi-deploy/migrations")
}

/// LF-normalized digest of a migration file, the same normalization the migrator
/// applies before comparing against the reviewed manifest.
fn lf_digest(path: &std::path::Path) -> String {
    let raw = std::fs::read(path).expect("read migration");
    let normalized = String::from_utf8(raw)
        .expect("utf-8 migration")
        .replace("\r\n", "\n");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

/// A scratch database migrated through `last_kept` only: every migration whose
/// filename sorts after it is withheld from a temp directory, so the upgrade
/// lane under test starts from a genuine historical schema.
async fn scratch_through(name: &str, last_kept: &str) -> (PgPool, crate::pg_test_support::ScratchDb, String) {
        // REV-093-F06: guard-owned; cleans up on unwind too.
    let mut scratch_guard = crate::pg_test_support::ScratchDb::create(name).await;
    let scratch = scratch_guard.name().to_string();

    let src_dir = migrations_dir();
    let red_dir = scratch_guard.temp_dir("r85");
    // A filtered MANIFEST.sha256 travels with the SQL: the migrator fails closed
    // without one, and REV-098-F03 makes it reject a manifest that describes files
    // the bundle does not have. `last_kept` is inclusive, so the cutoff is the next
    // representable name.
    crate::pg_test_support::reduced_bundle(&src_dir, &red_dir, &format!("{last_kept}\u{0}"));

    let url = format!(
        "{}/{}",
        crate::pg_test_support::require_live_url()
            .rsplitn(2, '/')
            .nth(1)
            .expect("db url base"),
        scratch
    );
    let pool = crate::db::connect(&url, 2).await.expect("scratch pool");
    // REV-087 item 7: the reduced set is passed EXPLICITLY. Mutating the
    // process-global SWI_MIGRATIONS_DIR raced with the other fixtures in this
    // harness (and with the lib harness running concurrently).
    crate::db::migrate_dir_with(&pool, &red_dir, true)
        .await
        .unwrap_or_else(|e| panic!("migrate through {last_kept}: {e:#}"));

    (pool, scratch_guard, scratch)
}

async fn drop_scratch(_guard: &crate::pg_test_support::ScratchDb, pool: PgPool, _scratch: &str) {
    pool.close().await;
    /* REV-093-F06: guard-owned teardown, also runs on unwind */
}

// ---------------------------------------------------------------------------
// A shipped migration is immutable: bytes and manifest must both still agree
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_1036_matches_its_reviewed_digest() {
    let path = migrations_dir().join(MIGRATION_1036);
    let digest = lf_digest(&path);
    assert_eq!(
        digest, REVIEWED_1036_DIGEST,
        "{MIGRATION_1036} was edited after review (REV-084-F01). Shipped migrations \
         are immutable — corrections go into a NEW forward migration plus, where the \
         old file would abort, a migrator preflight repair."
    );

    let manifest = std::fs::read_to_string(migrations_dir().join(crate::db::MIGRATION_MANIFEST))
        .expect("read MANIFEST.sha256");
    let recorded = manifest
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let digest = parts.next()?;
            let name = parts.next()?;
            (name == MIGRATION_1036).then(|| digest.to_string())
        })
        .expect("MANIFEST.sha256 records 1036");
    assert_eq!(
        recorded, REVIEWED_1036_DIGEST,
        "the reviewed manifest must still record 1036's reviewed digest"
    );
}

// ---------------------------------------------------------------------------
// A database holding the reviewed 1036 upgrades to current
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_database_holding_the_reviewed_1036_digest_upgrades_to_current() {
    let (pool, scratch_guard, scratch) = scratch_through("at1036", MIGRATION_1036).await;

    let ledger: String =
        sqlx::query_scalar("SELECT sha256 FROM public._migrations WHERE name = $1")
            .bind(MIGRATION_1036)
            .fetch_one(&pool)
            .await
            .expect("ledger row for 1036");
    // REV-087-F01: this was `ends_with`, which a `baseline:<digest>` prefix
    // satisfies — a row whose applied bytes are UNKNOWN would have passed as proof
    // that the reviewed bytes ran. The lane applies 1036 for real, so the recorded
    // value must be the bare observed digest.
    assert_eq!(
        ledger, REVIEWED_1036_DIGEST,
        "ledger recorded {ledger}, expected the reviewed 1036 digest observed at apply time"
    );

    // The upgrade: full migration set, from the reviewed 1036 forward.
    crate::db::migrate_with(&pool, true)
        .await
        .expect("upgrade from the reviewed 1036 must reach current (REV-084-F01)");
    // Re-run: digest verification of every already-applied migration must pass.
    crate::db::migrate_with(&pool, true)
        .await
        .expect("re-running the migrator must verify recorded digests, not fail");

    let applied_head: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM public._migrations \
          WHERE name = '1040_rev086_alert_case_ownership.sql' \
            AND digest_origin = 'applied')",
    )
    .fetch_one(&pool)
    .await
    .expect("head migration applied");
    assert!(applied_head, "the lane reached the newest migration");

    drop_scratch(&scratch_guard, pool, &scratch).await;
}

// ---------------------------------------------------------------------------
// A pre-1036 database with nonnumeric dedup keys upgrades unassisted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pre_1036_database_with_nonnumeric_dedup_keys_upgrades_unassisted() {
    let (pool, scratch_guard, scratch) =
        scratch_through("legacykey", "1035_rev078_outbox_grants_subjects_sweep.sql").await;

    sqlx::query("INSERT INTO workspaces (name, slug) VALUES ('w','legacyws') ON CONFLICT DO NOTHING")
        .execute(&pool)
        .await
        .expect("workspace");
    let ws: i64 = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'legacyws'")
        .fetch_one(&pool)
        .await
        .expect("workspace id");
    let signal_id: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'LEGACYMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");

    // The REV-082-F02 shape: the second segment is 'solana', not a workspace id.
    // 1036's ::bigint cast aborts on it; the preflight must pre-assign the row.
    sqlx::query(
        "INSERT INTO alerts \
           (dedup_key, subject_kind, signal_id, destination, state, attempt_count, next_attempt_at) \
         VALUES ('signal:solana:MINT:entry', 'signal', $1, 'chat-legacy', 'pending', 1, now())",
    )
    .bind(signal_id)
    .execute(&pool)
    .await
    .expect("legacy alert");

    // NO manual repair, NO edit to 1036: the migrator alone must carry this lane.
    crate::db::migrate_with(&pool, true)
        .await
        .expect("unassisted upgrade past unmodified 1036 must succeed (REV-084-F01)");

    let (owner, default_ws): (i64, i64) = sqlx::query_as(
        "SELECT a.workspace_id, (SELECT id FROM workspaces WHERE slug = 'default') \
           FROM alerts a WHERE a.dedup_key = 'signal:solana:MINT:entry'",
    )
    .fetch_one(&pool)
    .await
    .expect("legacy alert ownership");
    assert_eq!(
        owner, default_ws,
        "a legacy key whose workspace segment does not parse belongs to the default \
         workspace, exactly as 1036's own fallback intends"
    );

    drop_scratch(&scratch_guard, pool, &scratch).await;
}
