//! REV-104-F01/F02 — the READ-ONLY ledger boundary must apply the same
//! provenance authority the migrator applies.
//!
//! REV-103 taught `db migrate` to refuse a ledger whose provenance domain is
//! nondeterministic (F01) or escapable through inheritance (F02). It did not
//! teach `ensure_schema_current`, so the reviewer observed `db migrate` rc=1 and
//! `db status` rc=0 against the very same poisoned database: every service start
//! and every deploy gate that reads that exit code was still trusting a ledger
//! the migrator had already condemned.
//!
//! Both tests drive the REAL production binary, because the defect is about what
//! an operator's exit code says, not about what a library function returns.

use std::process::Command;

/// The production binary next to the test harness, not a re-linked test copy.
fn production_binary() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop(); // deps/
    p.pop(); // debug/
    let exe = p.join(if cfg!(windows) {
        "solana-whale-intelligence.exe"
    } else {
        "solana-whale-intelligence"
    });
    assert!(exe.exists(), "the production binary must be built at {}", exe.display());
    exe
}

/// `db status` against `url`, as an operator runs it.
fn db_status(url: &str) -> std::process::Output {
    Command::new(production_binary())
        .args(["db", "status"])
        .env("DATABASE_URL", url)
        .env_remove("MIGRATION_DATABASE_URL")
        .output()
        .expect("spawn the production binary")
}

/// A fully migrated scratch database plus its pool.
async fn migrated(label: &str) -> (crate::pg_test_support::ScratchDb, sqlx::PgPool) {
    let scratch = crate::pg_test_support::ScratchDb::create(label).await;
    let pool = scratch.pool().await;
    crate::db::migrate_with(&pool, true).await.expect("baseline migration must succeed");
    (scratch, pool)
}

// ---------------------------------------------------------------------------
// REV-104-F01 — nondeterministic provenance is not provenance
// ---------------------------------------------------------------------------

/// Poison a HEALTHY, fully migrated ledger the way the reviewer did, and require
/// the read-only command to notice.
///
/// The database is migrated first, so the `rc=0` half of the test is real: this
/// is a database `db status` genuinely blesses. Then `digest_origin` is retyped
/// under an ICU nondeterministic collation and one row is rewritten to `APPLIED`
/// — byte-distinct from every provenance this binary writes, yet accepted by the
/// canonical CHECK because equality under that collation is a locale match. The
/// second `db status` must fail: nothing about the schema changed, only who is
/// allowed to answer for it.
#[tokio::test]
async fn db_status_rejects_a_nondeterministic_provenance_column() {
    let (scratch, pool) = migrated("rev105f01").await;

    let clean = db_status(scratch.scratch_url());
    assert!(
        clean.status.success(),
        "a healthy migrated ledger must pass `db status`: {}{}",
        String::from_utf8_lossy(&clean.stdout),
        String::from_utf8_lossy(&clean.stderr)
    );

    sqlx::raw_sql(
        "CREATE COLLATION public._swi_rev105_ci \
           (provider = icu, locale = 'und-u-ks-level2', deterministic = false); \
         ALTER TABLE public._migrations \
           ALTER COLUMN digest_origin TYPE text COLLATE public._swi_rev105_ci",
    )
    .execute(&pool)
    .await
    .expect("retype the provenance column under a nondeterministic collation");
    let poisoned = sqlx::query(
        "UPDATE public._migrations SET digest_origin = 'APPLIED' \
          WHERE name = (SELECT min(name) FROM public._migrations)",
    )
    .execute(&pool)
    .await
    .expect("the canonical CHECK admits `APPLIED` under this collation");
    assert_eq!(poisoned.rows_affected(), 1, "the bypass premise: one row now reads `APPLIED`");

    let out = db_status(scratch.scratch_url());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "`db status` must exit nonzero on a nondeterministic provenance column, got rc=0: \
         {combined}"
    );
    assert!(
        combined.contains("digest_origin") && combined.contains("collation"),
        "the failure must name the collation of the provenance column, got: {combined}"
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// REV-104-F02 — an inherited ledger is not the ledger
// ---------------------------------------------------------------------------

/// Move every ledger row into an inherited child under a `NO INHERIT` CHECK and
/// require the read-only command to refuse.
///
/// The parent is left EMPTY. Every unqualified scan — including the one
/// `ensure_schema_current` performs — still returns all 54 rows, because they
/// come from the child; but the child is bound by no provenance CHECK, so it can
/// carry `rogue`. Before this fix the name and digest of each row matched the
/// embedded bundle, so the schema read as current while the authority that makes
/// "current" mean anything had been replaced.
#[tokio::test]
async fn db_status_rejects_an_inherited_rogue_ledger() {
    let (scratch, pool) = migrated("rev105f02").await;

    sqlx::raw_sql(
        "ALTER TABLE public._migrations DROP CONSTRAINT _migrations_digest_origin_check; \
         ALTER TABLE public._migrations \
           ADD CONSTRAINT _migrations_digest_origin_check \
           CHECK (digest_origin IS NULL OR digest_origin IN ('applied', 'baseline')) \
           NO INHERIT; \
         CREATE TABLE public._migrations_shadow () INHERITS (public._migrations); \
         INSERT INTO public._migrations_shadow (name, applied_at, sha256, digest_origin) \
           SELECT name, applied_at, sha256, 'rogue' FROM ONLY public._migrations; \
         DELETE FROM ONLY public._migrations",
    )
    .execute(&pool)
    .await
    .expect("install the inheritance bypass");

    let parent: i64 = sqlx::query_scalar("SELECT count(*) FROM ONLY public._migrations")
        .fetch_one(&pool)
        .await
        .expect("parent count");
    let visible: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._migrations WHERE digest_origin = 'rogue'",
    )
    .fetch_one(&pool)
    .await
    .expect("visible rogue count");
    assert_eq!(parent, 0, "the bypass premise: the parent itself holds no rows");
    assert!(
        visible > 0,
        "the bypass premise: the child's `rogue` rows are visible through the parent"
    );

    let out = db_status(scratch.scratch_url());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "`db status` must exit nonzero on an inherited ledger, got rc=0: {combined}"
    );
    assert!(
        combined.contains("descendant") || combined.contains("NO INHERIT"),
        "the failure must name the inheritance defect, got: {combined}"
    );

    pool.close().await;
}
