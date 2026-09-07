//! Shared setup for live PostgreSQL tests (REV-053-F04).
//!
//! THE PROBLEM
//! `#[sqlx::test]` provisions an isolated scratch database per test and, by default,
//! applies `./migrations` relative to the crate root. This crate has no such
//! directory: the canonical SQL is a sibling deployment artifact
//! (`../swi-deploy/migrations`), resolved at runtime by `db::resolve_migration_dir()`.
//! So every `#[sqlx::test]` ran against an EMPTY schema and failed with
//! `type "recent_relation" does not exist` / `relation "funding_observations" does not
//! exist` — ten failures that had nothing to do with the code under test.
//!
//! WHY NOT `migrations = "../swi-deploy/migrations"`
//! sqlx's own loader parses the filename version as an `i64`
//! (`sqlx-core-0.8.6/src/migrate/source.rs:104`: `let version: i64 = parts[0].parse()`),
//! and this migration set deliberately contains `1019a_admin_sessions_baseline.sql` —
//! named to sort BETWEEN 1019 and 1020 because shipped migrations are immutable
//! (REV-027-F10). sqlx would reject it outright. The lane ordering is a frozen
//! decision; bending it to satisfy a test harness would be the wrong trade.
//!
//! THE FIX
//! Disable sqlx's migrator (`migrations = false`) and run THIS project's migration
//! runner against the scratch database. That is strictly better than teaching the
//! harness a second way to migrate: the tests now exercise the same
//! `db::migrate_with()` that production uses, including its ledger and digest
//! behaviour, so a migration that would fail in production fails here too.

#![cfg(all(test, feature = "pg_tests"))]

use sqlx::PgPool;

/// Apply the canonical migration set to a scratch database from `#[sqlx::test]`.
///
/// `accept_legacy_baseline = true` because a scratch database has no ledger history at
/// all: every migration is applied fresh and recorded with `digest_origin = 'applied'`,
/// so the flag never actually accepts an unverified row here. It is passed for the case
/// where a developer points `DATABASE_URL` at a pre-existing database.
pub async fn migrate_scratch(pool: &PgPool) {
    crate::db::migrate_with(pool, true)
        .await
        .expect("apply the canonical migration set to the scratch database");
}

/// The live database URL for a `pg_tests` run, or PANIC (REV-056-F06).
///
/// Every live module used to do:
///
/// ```text
/// let Some(pool) = pool().await else { eprintln!("skipping: no live database"); return; };
/// ```
///
/// which reports `test result: ok` after asserting nothing. The reviewer measured it:
/// four REV-054 tests "passed" in 0.00s with both DB variables unset. A gate that is
/// green when it did not run is worse than a red one, because CI stops telling you
/// anything.
///
/// The `pg_tests` feature is the caller's statement that a database IS available, so
/// its absence is a configuration ERROR, not a reason to skip. Developers who want
/// offline runs simply omit the feature — that is what the feature is for.
pub fn require_live_url() -> String {
    for k in ["TEST_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    panic!(
        "the `pg_tests` feature is enabled but neither TEST_DATABASE_URL nor \
         DATABASE_URL is set. These tests assert against a live PostgreSQL database; \
         skipping them would report success for assertions that never ran \
         (REV-056-F06). Set a disposable database, or run without `--features pg_tests`."
    );
}

/// A pool connected the way the RUNTIME connects (`search_path = swi_legacy,public`).
///
/// Panics rather than returning `Option`: see [`require_live_url`]. Using the
/// production connector matters because a bare `PgPool::connect` resolves legacy table
/// names against `public`, where the CANONICAL tables of the same name live — so the
/// test would exercise a different schema than production.
pub async fn live_pool() -> PgPool {
    let url = require_live_url();
    crate::db::connect(&url, 2)
        .await
        .expect("connect to the live test database")
}

/// The plaintext admin password every live module logs in with.
pub const ADMIN_TEST_PASSWORD: &str = "swi-pg-test-probe";

/// Configure `ADMIN_PASSWORD_HASH` for this PROCESS, once.
///
/// `require_write_auth` returns 403 for EVERY mutation while the hash is unset —
/// correct production behaviour, and the reason a mutation test must configure it:
/// without this the tenancy assertions pass for the wrong reason (403 for everyone
/// rather than 404 for the wrong tenant), which is exactly the kind of vacuous green
/// this review loop is about.
///
/// It lives HERE rather than per module because the hash is process-global: two
/// modules each running their own `Once` with their own password meant whichever
/// ran first decided the password, and the other module's login probes then failed
/// for a reason that had nothing to do with the code under test. One password, one
/// initialization.
pub fn configure_admin_auth() -> &'static str {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let hash = crate::auth::hash_password(ADMIN_TEST_PASSWORD).expect("hash password");
        // Single-threaded initialization guarded by `Once`, before any request is
        // served in this process.
        std::env::set_var("ADMIN_PASSWORD_HASH", hash);
    });
    assert!(
        crate::auth::auth_configured(),
        "admin auth must be configured, or mutations return 403 and the assertions \
         that depend on them pass vacuously"
    );
    ADMIN_TEST_PASSWORD
}
