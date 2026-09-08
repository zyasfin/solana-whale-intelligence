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

/// A disposable database (and any temp directories) that clean themselves up on
/// `Drop`, including when the test PANICS.
///
/// REV-093-F06: every live fixture used to finish with an explicit
/// `drop_scratch(...).await` on the SUCCESS path only. A failing assertion unwinds
/// before that line, so each RED probe stranded a database — the REV-095 round alone
/// leaked eight, and they were removed by hand afterwards. Manual sweeping is not a
/// mechanism: the next failing test leaks again. Ownership belongs to a guard.
///
/// Why a thread rather than `block_on`/`block_in_place`:
///
///   * `Drop` cannot `.await`;
///   * `#[tokio::test]` defaults to a CURRENT-THREAD runtime, where
///     `block_in_place` panics, and calling `Runtime::block_on` while already inside
///     a runtime panics too;
///   * during unwind a second panic would abort the process.
///
/// So cleanup runs on a fresh `std::thread` with its own single-thread runtime and is
/// joined before `Drop` returns. That is safe from any runtime flavour and from
/// inside a panic.
///
/// `DROP DATABASE ... WITH (FORCE)` terminates leftover backends itself, so a pool the
/// test still holds does not block teardown.
pub struct ScratchDb {
    name: String,
    admin_url: String,
    url: String,
    temp_dirs: Vec<std::path::PathBuf>,
    disarmed: bool,
}

impl ScratchDb {
    /// Create a uniquely named disposable database.
    ///
    /// The name carries the caller's label, the process id, and a monotonic counter,
    /// so two harness processes (lib + bin) and two tests in one process cannot
    /// collide — `std::process::id()` alone is not unique across the two harnesses
    /// `cargo test --features pg_tests` starts.
    pub async fn create(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let admin_url = require_live_url();
        let name = format!(
            "swi_scratch_{label}_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&admin_url)
            .await
            .expect("admin pool for scratch creation");
        sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(&admin)
            .await
            .expect("drop pre-existing scratch");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create scratch");
        admin.close().await;
        let url = format!(
            "{}/{}",
            admin_url.rsplitn(2, '/').nth(1).expect("database url base"),
            name
        );
        Self { name, admin_url, url, temp_dirs: Vec::new(), disarmed: false }
    }

    /// The scratch database's own URL.
    pub fn scratch_url(&self) -> &str {
        &self.url
    }

    /// The scratch database's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A pool connected the way the RUNTIME connects (see [`live_pool`]).
    pub async fn pool(&self) -> PgPool {
        crate::db::connect(&self.url, 2)
            .await
            .expect("connect to the scratch database")
    }

    /// A uniquely named temp directory whose lifetime is tied to this guard.
    pub fn temp_dir(&mut self, label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{}_{label}", self.name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch temp dir");
        self.temp_dirs.push(dir.clone());
        dir
    }

    /// Give up ownership WITHOUT cleaning up.
    ///
    /// Only for the regression that has to observe a leak; production fixtures never
    /// call this.
    #[cfg(test)]
    pub fn leak_for_tests(mut self) -> String {
        self.disarmed = true;
        self.name.clone()
    }

    /// Whether a database of this name still exists. Used by the cleanup regression.
    pub async fn exists(admin_url: &str, name: &str) -> bool {
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(admin_url)
            .await
            .expect("admin pool for existence probe");
        let found: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
                .bind(name)
                .fetch_one(&admin)
                .await
                .expect("existence probe");
        admin.close().await;
        found
    }

}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        for dir in &self.temp_dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
        if self.disarmed {
            return;
        }
        let name = self.name.clone();
        let admin_url = self.admin_url.clone();
        // Own runtime on its own thread: valid from a current-thread test, from a
        // multi-thread test, and from inside an unwind.
        let handle = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("scratch cleanup: could not build runtime for {name}: {e}");
                    return;
                }
            };
            rt.block_on(async {
                let admin = match sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&admin_url)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("scratch cleanup: could not connect to drop {name}: {e}");
                        return;
                    }
                };
                if let Err(e) = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                    .execute(&admin)
                    .await
                {
                    eprintln!("scratch cleanup: could not drop {name}: {e}");
                }
                admin.close().await;
            });
        });
        // Never panic from Drop: a second panic during unwind aborts the process.
        if handle.join().is_err() {
            eprintln!("scratch cleanup thread panicked for {}", self.name);
        }
    }
}
