//! REV-102-F01/F02 — the ledger's provenance domain must not depend on
//! collation, and it must not be escapable through table inheritance.
//!
//! F01: `digest_origin IN ('applied', 'baseline')` is an equality test, and
//! equality under a NONDETERMINISTIC collation is not byte equality. An ICU
//! case-insensitive column therefore admits `APPLIED` under a CHECK whose
//! expression deparses byte-for-byte canonically and which is `convalidated`.
//! F02: a canonical CHECK declared `NO INHERIT` is not carried by children, and
//! rows of a child of `public._migrations` are visible through every scan of the
//! parent. Provenance authority must cover the whole inheritance tree.

use anyhow::Result;

/// The canonical ledger bootstrap minus the provenance CHECK, so each test can
/// install its own adversarial shape. `collate` is the collation clause applied
/// to `digest_origin` (empty for the database default).
async fn bootstrap_ledger(pool: &sqlx::PgPool, collate: &str) -> Result<()> {
    sqlx::raw_sql(&format!(
        "CREATE TABLE public._migrations (\
           name text PRIMARY KEY, \
           applied_at timestamptz NOT NULL DEFAULT now(), \
           sha256 text, \
           digest_origin text{collate})"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

const CANONICAL_CHECK: &str =
    "CHECK (digest_origin IS NULL OR digest_origin IN ('applied', 'baseline'))";

/// True when the migration failed before any application schema was created.
async fn no_application_schema(pool: &sqlx::PgPool) -> bool {
    let workspaces: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
          WHERE table_schema = 'public' AND table_name = 'workspaces')",
    )
    .fetch_one(pool)
    .await
    .expect("schema probe");
    !workspaces
}

// ---------------------------------------------------------------------------
// REV-102-F01 — collation-dependent provenance
// ---------------------------------------------------------------------------

/// Reproduce the reviewer's collation bypass and require it to fail closed.
///
/// The CHECK installed here is byte-for-byte the canonical predicate and it is
/// VALIDATED, so both authority tests REV-101 added answer "canonical". The lie
/// is in the column: under `deterministic = false` the server's equality
/// operator is a collation-defined match, not a byte match, so `APPLIED` — a
/// provenance this binary never writes and never recognises — satisfies
/// `digest_origin IN ('applied', 'baseline')` and survives validation.
#[tokio::test]
async fn an_icu_case_insensitive_provenance_column_fails_the_migrator() {
    let scratch = crate::pg_test_support::ScratchDb::create("rev103f01").await;
    let pool = scratch.pool().await;

    sqlx::raw_sql(
        "CREATE COLLATION public._swi_rev103_ci \
           (provider = icu, locale = 'und-u-ks-level2', deterministic = false)",
    )
    .execute(&pool)
    .await
    .expect("create a nondeterministic ICU collation");
    bootstrap_ledger(&pool, " COLLATE public._swi_rev103_ci").await.expect("bootstrap");
    sqlx::query(
        "INSERT INTO public._migrations (name, sha256, digest_origin) \
         VALUES ('1001_rogue_seed.sql', 'deadbeef', 'APPLIED')",
    )
    .execute(&pool)
    .await
    .expect("seed a byte-distinct provenance row");
    sqlx::raw_sql(&format!(
        "ALTER TABLE public._migrations \
           ADD CONSTRAINT _migrations_digest_origin_check {CANONICAL_CHECK}"
    ))
    .execute(&pool)
    .await
    .expect("the canonical, VALIDATED expression is accepted over the rogue row");

    // The premise: the row is byte-distinct from every provenance this binary
    // writes, yet the validated canonical CHECK admits it.
    let bypassed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._migrations \
          WHERE convert_to(digest_origin, 'UTF8') \
                = convert_to('APPLIED', 'UTF8')",
    )
    .fetch_one(&pool)
    .await
    .expect("count byte-distinct rows");
    assert_eq!(bypassed, 1, "the bypass premise: a byte-distinct provenance row is present");

    let err = crate::db::migrate_with(&pool, true)
        .await
        .expect_err("a nondeterministic provenance column must fail the migration closed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("digest_origin") && msg.contains("collation"),
        "the failure must name the collation of the provenance column, got: {msg}"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool)
        .await
        .expect("ledger count");
    assert_eq!(rows, 1, "no migration may have been recorded");
    assert!(no_application_schema(&pool).await, "no migration SQL may have executed");

    pool.close().await;
}

/// A byte-distinct provenance value must be refused even when the column's
/// collation is deterministic — the row scan is authority in its own right, not
/// a side effect of the CHECK.
///
/// Here the CHECK is absent when the rogue row is written (so nothing rejects it
/// on the way in) and the canonical CHECK is then added `NOT VALID`-free by the
/// migrator itself. Without a row preflight the migrator's own `ADD CONSTRAINT`
/// is what fails, with a bare PostgreSQL 23514; the ledger must instead report
/// which value is out of domain.
#[tokio::test]
async fn a_byte_distinct_provenance_row_fails_the_migrator() {
    let scratch = crate::pg_test_support::ScratchDb::create("rev103rows").await;
    let pool = scratch.pool().await;

    bootstrap_ledger(&pool, "").await.expect("bootstrap");
    sqlx::query(
        "INSERT INTO public._migrations (name, sha256, digest_origin) \
         VALUES ('1001_rogue_seed.sql', 'deadbeef', 'forged')",
    )
    .execute(&pool)
    .await
    .expect("seed an out-of-domain provenance row");

    let err = crate::db::migrate_with(&pool, true)
        .await
        .expect_err("an out-of-domain provenance row must fail the migration closed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("forged") && msg.contains("1001_rogue_seed.sql"),
        "the failure must name the offending row and value, got: {msg}"
    );
    assert!(no_application_schema(&pool).await, "no migration SQL may have executed");

    pool.close().await;
}

/// Non-vacuity counterpart for both row and collation authority: a ledger whose
/// provenance column carries an EXPLICIT deterministic collation, with the
/// canonical validated CHECK and legal rows, still migrates completely.
#[tokio::test]
async fn a_deterministic_provenance_column_is_still_accepted() {
    let scratch = crate::pg_test_support::ScratchDb::create("rev103f01ok").await;
    let pool = scratch.pool().await;

    bootstrap_ledger(&pool, " COLLATE \"C\"").await.expect("bootstrap");
    sqlx::raw_sql(&format!(
        "ALTER TABLE public._migrations \
           ADD CONSTRAINT _migrations_digest_origin_check {CANONICAL_CHECK}"
    ))
    .execute(&pool)
    .await
    .expect("install the canonical expression, validated");

    crate::db::migrate_with(&pool, true)
        .await
        .expect("a deterministic provenance column is the reconciled state");

    let embedded =
        crate::db::MigrationBundle::embedded().expect("embedded bundle").entries().len() as i64;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool)
        .await
        .expect("ledger count");
    assert_eq!(applied, embedded, "every embedded migration must have been applied");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// REV-102-F02 — inheritance
// ---------------------------------------------------------------------------

/// Reproduce the reviewer's inheritance bypass and require it to fail closed.
///
/// `NO INHERIT` means the CHECK is not copied to children, and a child created
/// with `INHERITS (public._migrations)` contributes its rows to every scan of
/// the parent — including `ensure_schema_current`'s. So a `rogue` provenance is
/// simultaneously visible as ledger truth and outside the constraint that is
/// supposed to define ledger truth. Constraint authority is only authority if it
/// covers the whole inheritance tree.
#[tokio::test]
async fn a_no_inherit_check_with_a_rogue_child_fails_the_migrator() {
    let scratch = crate::pg_test_support::ScratchDb::create("rev103f02").await;
    let pool = scratch.pool().await;

    bootstrap_ledger(&pool, "").await.expect("bootstrap");
    sqlx::raw_sql(&format!(
        "ALTER TABLE public._migrations \
           ADD CONSTRAINT _migrations_digest_origin_check {CANONICAL_CHECK} NO INHERIT"
    ))
    .execute(&pool)
    .await
    .expect("install the canonical expression, validated, NO INHERIT");
    sqlx::raw_sql("CREATE TABLE public._migrations_rogue () INHERITS (public._migrations)")
        .execute(&pool)
        .await
        .expect("create an inheriting child");
    sqlx::query(
        "INSERT INTO public._migrations_rogue (name, sha256, digest_origin) \
         VALUES ('1001_rogue_child.sql', 'deadbeef', 'rogue')",
    )
    .execute(&pool)
    .await
    .expect("seed a rogue row in the child");

    // The premise: the rogue provenance is visible as ledger content.
    let through_parent: i64 =
        sqlx::query_scalar("SELECT count(*) FROM public._migrations WHERE digest_origin = 'rogue'")
            .fetch_one(&pool)
            .await
            .expect("count rogue rows through the parent");
    assert_eq!(through_parent, 1, "the bypass premise: a child rogue row reads as ledger truth");

    let err = crate::db::migrate_with(&pool, true)
        .await
        .expect_err("an inheritance-escaped ledger must fail the migration closed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("_migrations_rogue") || msg.contains("NO INHERIT"),
        "the failure must name the inheritance escape, got: {msg}"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ONLY public._migrations")
        .fetch_one(&pool)
        .await
        .expect("ledger count");
    assert_eq!(rows, 0, "no migration may have been recorded");
    assert!(no_application_schema(&pool).await, "no migration SQL may have executed");

    pool.close().await;
}

/// Non-vacuity counterpart for F02: the canonical CHECK is INHERITABLE (the
/// default) and the ledger has no descendants, so migration proceeds. Without
/// this the test above would also pass if the migrator rejected every ledger
/// that already carries a constraint.
#[tokio::test]
async fn an_inheritable_check_over_a_childless_ledger_is_accepted() {
    let scratch = crate::pg_test_support::ScratchDb::create("rev103f02ok").await;
    let pool = scratch.pool().await;

    bootstrap_ledger(&pool, "").await.expect("bootstrap");
    sqlx::raw_sql(&format!(
        "ALTER TABLE public._migrations \
           ADD CONSTRAINT _migrations_digest_origin_check {CANONICAL_CHECK}"
    ))
    .execute(&pool)
    .await
    .expect("install the canonical expression, validated, inheritable");

    let noinherit: bool = sqlx::query_scalar(
        "SELECT connoinherit FROM pg_constraint \
          WHERE conname = '_migrations_digest_origin_check' \
            AND conrelid = 'public._migrations'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("read connoinherit");
    assert!(!noinherit, "the accepted shape must be the inheritable one");

    crate::db::migrate_with(&pool, true).await.expect("a childless ledger migrates");

    let embedded =
        crate::db::MigrationBundle::embedded().expect("embedded bundle").entries().len() as i64;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool)
        .await
        .expect("ledger count");
    assert_eq!(applied, embedded, "every embedded migration must have been applied");

    pool.close().await;
}
