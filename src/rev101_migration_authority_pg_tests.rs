//! REV-100-F01/F02 — the two authority boundaries of the migrator.
//!
//! F01: a CHECK whose EXPRESSION is canonical but which is `NOT VALID` says
//! nothing about the rows already in the table. The migrator must refuse it.
//! F02: the migration-source override must come from the launching process, not
//! from whatever `.env` happens to sit in the current directory or a parent.

use anyhow::Result;

/// The canonical ledger bootstrap, as `migrate_bundle_with` would create it —
/// minus the provenance CHECK, which each test installs in its own shape.
async fn bootstrap_ledger(pool: &sqlx::PgPool) -> Result<()> {
    sqlx::raw_sql(
        "CREATE TABLE public._migrations (\
           name text PRIMARY KEY, \
           applied_at timestamptz NOT NULL DEFAULT now(), \
           sha256 text, \
           digest_origin text)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// REV-100-F01 — canonical expression, NOT VALID, rogue row
// ---------------------------------------------------------------------------

/// Reproduce the reviewer's bypass and require it to fail closed.
///
/// The constraint installed here is byte-for-byte the canonical predicate, so an
/// expression-only comparison accepts it (that is exactly what REV-100 reproduced:
/// `constraint=false|((digest_origin IS NULL) OR ...)`, `rogue_before=1`,
/// `migrate_rc=0`). `NOT VALID` is what makes it a lie: PostgreSQL never checked
/// the existing rows, so the `rogue` provenance survives underneath a predicate
/// that claims it cannot exist. Canonicality must therefore include
/// `convalidated`.
#[tokio::test]
async fn a_not_valid_canonical_check_over_a_rogue_row_fails_the_migrator() {
    let scratch = crate::pg_test_support::ScratchDb::create("rev101f01").await;
    let pool = scratch.pool().await;

    bootstrap_ledger(&pool).await.expect("bootstrap the ledger");
    sqlx::query(
        "INSERT INTO public._migrations (name, sha256, digest_origin) \
         VALUES ('1001_rogue_seed.sql', 'deadbeef', 'rogue')",
    )
    .execute(&pool)
    .await
    .expect("seed a rogue provenance row");
    // Canonical EXPRESSION, unvalidated: accepted by the server precisely because
    // it declines to look at the row above.
    sqlx::raw_sql(
        "ALTER TABLE public._migrations \
           ADD CONSTRAINT _migrations_digest_origin_check \
           CHECK (digest_origin IS NULL OR digest_origin IN ('applied', 'baseline')) NOT VALID",
    )
    .execute(&pool)
    .await
    .expect("install the canonical expression NOT VALID");

    let rogue_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM public._migrations WHERE digest_origin = 'rogue'")
            .fetch_one(&pool)
            .await
            .expect("count rogue rows");
    assert_eq!(rogue_before, 1, "the bypass premise: a rogue row is present");

    let err = crate::db::migrate_with(&pool, true)
        .await
        .expect_err("an unvalidated ledger CHECK must fail the migration closed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("_migrations_digest_origin_check"),
        "the failure must name the ledger CHECK, got: {msg}"
    );

    // ... and it must fail BEFORE any migration work: no rows added, no canonical
    // schema created.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool)
        .await
        .expect("ledger count");
    assert_eq!(rows, 1, "no migration may have been recorded");
    let workspaces: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
          WHERE table_schema = 'public' AND table_name = 'workspaces')",
    )
    .fetch_one(&pool)
    .await
    .expect("schema probe");
    assert!(!workspaces, "no migration SQL may have executed");

    pool.close().await;
}

/// The counterpart: the SAME expression, VALIDATED, over a clean ledger is still
/// accepted. Without this the test above would also pass if the migrator simply
/// rejected every pre-existing constraint.
#[tokio::test]
async fn a_validated_canonical_check_is_still_accepted() {
    let scratch = crate::pg_test_support::ScratchDb::create("rev101f01ok").await;
    let pool = scratch.pool().await;

    bootstrap_ledger(&pool).await.expect("bootstrap the ledger");
    sqlx::raw_sql(
        "ALTER TABLE public._migrations \
           ADD CONSTRAINT _migrations_digest_origin_check \
           CHECK (digest_origin IS NULL OR digest_origin IN ('applied', 'baseline'))",
    )
    .execute(&pool)
    .await
    .expect("install the canonical expression, validated");

    crate::db::migrate_with(&pool, true)
        .await
        .expect("a validated canonical CHECK is the reconciled state");

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
// REV-100-F02 — an ambient `.env` may not choose the migration source
// ---------------------------------------------------------------------------

/// Plant a hostile, self-consistent migration bundle and point a CWD `.env` at it.
///
/// `Settings::load` calls `dotenvy::dotenv()`, which walks the cwd and its parents,
/// so before this fix the resolver's `std::env::var(SWI_MIGRATIONS_DIR)` could read
/// a value that no operator ever supplied — ambient location authority re-entering
/// by the back door after REV-098-F03 removed it from the candidate list. The
/// bundle is internally valid (manifest bijection + digests), so nothing downstream
/// would reject it: only provenance keeps it out.
///
/// The second half proves this is a trust boundary, not a hard-wire: the same
/// override supplied by the LAUNCHER is still honoured.
#[tokio::test]
async fn an_ambient_dotenv_cannot_choose_the_migration_source() {
    let built = {
        let mut p = std::env::current_exe().expect("test exe path");
        p.pop(); // deps/
        p.pop(); // debug/
        p.join(if cfg!(windows) {
            "solana-whale-intelligence.exe"
        } else {
            "solana-whale-intelligence"
        })
    };
    assert!(built.exists(), "the production binary must be built at {}", built.display());

    let root = crate::pg_test_support::ScratchDir::create("rev101dotenv");
    let bindir = root.path().join("opt").join("swi").join("bin");
    std::fs::create_dir_all(&bindir).expect("bin dir");
    let exe = bindir.join(built.file_name().expect("exe name"));
    std::fs::copy(&built, &exe).expect("relocate the binary");

    let cwd = root.path().join("workdir");
    let hostile = cwd.join("hostile-migrations");
    std::fs::create_dir_all(&hostile).expect("hostile dir");
    let hostile_sql = "-- HOSTILE .env BUNDLE: this must never be applied\n\
                       CREATE TABLE public._hostile_dotenv_marker (id int);\n";
    std::fs::write(hostile.join("0001_hostile_dotenv.sql"), hostile_sql).expect("hostile sql");
    std::fs::write(
        hostile.join(crate::db::MIGRATION_MANIFEST),
        format!(
            "{}  0001_hostile_dotenv.sql\r\n",
            crate::db::migration_sha256_for_tests(hostile_sql)
        ),
    )
    .expect("hostile manifest");

    // The ambient authority under test.
    let hostile_env_value = hostile.display().to_string().replace('\\', "/");
    std::fs::write(
        cwd.join(".env"),
        format!("{}={hostile_env_value}\n", crate::db::MIGRATIONS_DIR_OVERRIDE),
    )
    .expect("hostile .env");

    let scratch = crate::pg_test_support::ScratchDb::create("rev101ambient").await;
    let out = std::process::Command::new(&exe)
        .args(["db", "migrate", "--accept-legacy-baseline"])
        .current_dir(&cwd)
        .env("DATABASE_URL", scratch.scratch_url())
        .env("MIGRATION_DATABASE_URL", scratch.scratch_url())
        .env_remove(crate::db::MIGRATIONS_DIR_OVERRIDE)
        .output()
        .expect("spawn the relocated migrator");
    assert!(
        out.status.success(),
        "migrate with a hostile .env failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let pool = scratch.pool().await;
    let embedded =
        crate::db::MigrationBundle::embedded().expect("embedded bundle").entries().len() as i64;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool)
        .await
        .expect("ledger count");
    assert_eq!(
        applied, embedded,
        "the embedded bundle must win over an ambient .env ({embedded} migrations)"
    );
    let intruder: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._migrations WHERE name = '0001_hostile_dotenv.sql'",
    )
    .fetch_one(&pool)
    .await
    .expect("intruder probe");
    assert_eq!(intruder, 0, "no file from the .env-nominated directory may be applied");
    let marker: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
          WHERE table_schema = 'public' AND table_name = '_hostile_dotenv_marker')",
    )
    .fetch_one(&pool)
    .await
    .expect("marker probe");
    assert!(!marker, "the hostile bundle must not have executed");

    // The launcher's own override — the one authority that may displace the
    // embedded bundle — still works, from the same hostile cwd.
    let scratch2 = crate::pg_test_support::ScratchDb::create("rev101explicit").await;
    let out2 = std::process::Command::new(&exe)
        .args(["db", "migrate", "--accept-legacy-baseline"])
        .current_dir(&cwd)
        .env("DATABASE_URL", scratch2.scratch_url())
        .env("MIGRATION_DATABASE_URL", scratch2.scratch_url())
        .env(crate::db::MIGRATIONS_DIR_OVERRIDE, &hostile)
        .output()
        .expect("spawn the explicitly overridden migrator");
    assert!(
        out2.status.success(),
        "explicit override failed: {}{}",
        String::from_utf8_lossy(&out2.stdout),
        String::from_utf8_lossy(&out2.stderr)
    );
    let pool2 = scratch2.pool().await;
    let overridden: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool2)
        .await
        .expect("override ledger count");
    assert_eq!(
        overridden, 1,
        "a process-supplied {} must remain the operator override",
        crate::db::MIGRATIONS_DIR_OVERRIDE
    );

    pool.close().await;
    pool2.close().await;
}
