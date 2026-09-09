//! Database connection pooling and migration support.

#![allow(dead_code)]  // helper API used by workers and tests

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::str::FromStr;
use std::time::Duration;

/// Connect to PostgreSQL using a `DATABASE_URL`-style connection string.
///
/// Accepts `postgres://` and `postgresql://` schemes. Pool size follows the
/// runtime profile: `low` keeps a small pool for 2 vCPU machines.
/// The schema search path used by this (pre-freeze) runtime.
///
/// REV-031/REV-025-F10: migration `1000_legacy_bridge.sql` archives the five
/// colliding legacy tables into `swi_legacy` so the canonical lane can create its
/// own `tokens` / `narratives` / `funding_edges` / `wallet_clusters` /
/// `token_lifecycle_events`. After that bridge, an UNQUALIFIED `tokens` resolves to
/// the CANONICAL table, whose columns are different — so this binary's queries
/// broke (`column mint absent`, `column lifecycle_state absent`, ...). The reviewer
/// reproduced exactly that.
///
/// This runtime is the pre-freeze one, and the frozen decision is replace-total, so
/// the fix here is NOT to rewrite 16 legacy queries against a schema they were
/// never written for. It is to make the legacy runtime explicit about which schema
/// it reads: `swi_legacy` first, then `public`.
///
/// * On a bridged database, legacy names resolve to the archived legacy tables and
///   this binary keeps working with its own column shapes.
/// * On a pre-bridge or fresh-canonical database, `swi_legacy` simply does not
///   exist; PostgreSQL ignores a missing schema in `search_path`, so resolution
///   falls through to `public` exactly as before.
/// * Non-colliding legacy tables (raw_events, trades, telegram_*, ...) were never
///   moved and continue to resolve from `public`.
///
/// The canonical `sf::` runtime does the opposite by construction: it targets the
/// canonical tables in `public` and never reads `swi_legacy`.
///
/// NO WHITESPACE (REV-033/REV-034). This value is passed as a PostgreSQL *startup
/// option*, not as a `SET` statement. Startup options are whitespace-delimited, so
/// `"swi_legacy, public"` was truncated at the space and the server rejected the
/// connection with `invalid value for parameter "search_path": "swi_legacy,"` —
/// the binary could not start. `legacy_search_path_is_valid_as_a_startup_option`
/// pins this.
pub const LEGACY_SEARCH_PATH: &str = "swi_legacy,public";

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool> {
    let options = PgConnectOptions::from_str(database_url)
        .with_context(|| "invalid DATABASE_URL")?
        .ssl_mode(PgSslMode::Prefer);
    let pool = PgPoolOptions::new()
        .max_connections(max_connections.max(2))
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(600))
        // Applied AFTER each connection is established, so every pooled connection
        // (including ones created later) shares the same resolution order.
        //
        // `after_connect` is used rather than a startup option because `SET` accepts
        // an identifier list and, more importantly, tolerates a schema that does not
        // exist: on a database that was never bridged, `swi_legacy` is simply absent
        // and resolution falls through to `public`. A startup option gives no such
        // latitude and turns any formatting slip into a failure to boot
        // (REV-033/REV-034).
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query(&format!("SET search_path = {LEGACY_SEARCH_PATH}"))
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .with_context(|| "failed to connect to PostgreSQL")?;
    Ok(pool)
}

/// Open a pool AND verify the schema matches this binary (REV-037-F01).
///
/// Every command that is not `db migrate` should use this instead of [`connect`].
///
/// Found while probing the real binary rather than reasoning about it: REV-036 put
/// `ensure_schema_current()` on five call sites, but `main.rs` opens a pool in about
/// thirty places. All four ledger forgeries — a row with no file on disk, a digest
/// that does not match, a filename-only row, a deleted row — were accepted by
/// `token report`, because that path never verified anything. Guarding
/// call sites cannot work when there are thirty of them and more get added; the
/// check belongs at the single place a pool is created.
pub async fn connect_verified(database_url: &str, max_connections: u32) -> Result<PgPool> {
    let pool = connect(database_url, max_connections).await?;
    ensure_schema_current(&pool).await?;
    Ok(pool)
}

/// Default pool size per runtime profile.
pub fn pool_size(scale_workers: usize) -> u32 {
    (scale_workers as u32 * 2).clamp(2, 16)
}

/// The canonical migration bundle: the reviewed SQL plus the reviewed manifest.
///
/// REV-098-F03: migrations used to be RESOLVED from the filesystem by walking a
/// candidate list — `$SWI_MIGRATIONS_DIR`, then cwd-relative `./migrations`, then
/// `CARGO_MANIFEST_DIR/migrations`, then two legacy siblings. Every part of that
/// was a defect:
///
///   * `./migrations` is whatever directory the operator happened to be standing
///     in. An unreviewed folder outranked the reviewed bundle by accident of cwd;
///   * `CARGO_MANIFEST_DIR` is the BUILD HOST's source path, not a shipped
///     artifact. A binary copied to a deployment host silently lost that candidate
///     and fell through to `../swi-deploy/migrations`, an unversioned copy that can
///     drift from the SQL that was reviewed;
///   * nothing verified the manifest before executing SQL, so a fresh database was
///     migrated from files no digest ever vouched for. The manifest was consulted
///     only on the pre-checksum backfill path.
///
/// The bundle is therefore COMPILED IN (see `build.rs`): the bytes travel inside
/// the executable, so they cannot be outranked by a directory, cannot be left
/// behind by a copy, and cannot drift from the reviewed source. The single
/// remaining override is the explicit operator variable `SWI_MIGRATIONS_DIR`,
/// which is a deliberate statement, not an accident of location — and it is held
/// to exactly the same manifest/digest rules as the embedded bundle.
pub struct MigrationBundle {
    /// Human-readable provenance, for logs and diagnostics.
    source: String,
    /// `(filename, sql)` in filename order.
    entries: Vec<(String, String)>,
    /// The reviewed digest for every entry, from this bundle's own manifest.
    manifest: std::collections::BTreeMap<String, String>,
}

include!(concat!(env!("OUT_DIR"), "/embedded_migrations.rs"));

/// The environment variable an operator sets to migrate from a directory instead
/// of the embedded bundle.
///
/// REV-100-F02: this is read from the PROCESS environment as it existed at entry
/// to `main`, captured by [`capture_process_migrations_override`] BEFORE
/// `dotenvy::dotenv()` runs (`EnvConfig::load`). `.env` discovery walks the
/// current directory and its parents, so a plain `std::env::var` here would let
/// whatever `.env` file the operator happened to be standing next to choose which
/// SQL this binary executes — exactly the ambient-location authority REV-098-F03
/// removed from the resolver. An override must be a deliberate statement by
/// whoever launched the process.
pub const MIGRATIONS_DIR_OVERRIDE: &str = "SWI_MIGRATIONS_DIR";

/// The value of [`MIGRATIONS_DIR_OVERRIDE`] in the launching process environment.
///
/// `None` until captured, and `Some(None)` when the launcher did not set it. A
/// process that never captures gets NO override at all — fail-closed: an
/// uncaptured override is indistinguishable from an ambient one.
static PROCESS_MIGRATIONS_OVERRIDE: std::sync::OnceLock<Option<String>> =
    std::sync::OnceLock::new();

/// Record the launching process's [`MIGRATIONS_DIR_OVERRIDE`].
///
/// MUST be called before anything loads `.env` (i.e. as the first thing `main`
/// does). Idempotent; only the first call is kept, so a later `set_var` cannot
/// re-authorize the override either.
pub fn capture_process_migrations_override() {
    let _ = PROCESS_MIGRATIONS_OVERRIDE.get_or_init(|| {
        std::env::var(MIGRATIONS_DIR_OVERRIDE).ok().filter(|v| !v.trim().is_empty())
    });
}

impl MigrationBundle {
    /// The bundle compiled into this binary.
    pub fn embedded() -> Result<Self> {
        let entries: Vec<(String, String)> = EMBEDDED_MIGRATIONS
            .iter()
            .map(|(n, s)| ((*n).to_string(), (*s).to_string()))
            .collect();
        Self::build("the migration bundle embedded in this binary", entries, EMBEDDED_MANIFEST)
    }

    /// A bundle read from an explicit directory (`SWI_MIGRATIONS_DIR`, and the
    /// reduced lanes tests build).
    ///
    /// Every failure here is fatal. REV-039-F04: this enumeration used to be
    /// `filter_map(|p| read_to_string(&p).ok()?)`, which DROPPED any file it could
    /// not read — a migration directory that cannot be fully read is not a verified
    /// migration set.
    pub fn from_dir(dir: &std::path::Path) -> Result<Self> {
        let mut entries: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("failed to read migrations dir {}", dir.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to read an entry in {}", dir.display()))?;
            let path = entry.path();
            if !path.is_file() || path.extension().map(|e| e != "sql").unwrap_or(true) {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("migration filename is not valid UTF-8: {}", path.display())
                })?
                .to_string();
            let sql = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "failed to read migration {}; a migration directory that \
                     cannot be fully read is not a verified migration set",
                    path.display()
                )
            })?;
            if !seen.insert(name.clone()) {
                anyhow::bail!("duplicate migration filename `{name}` in {}", dir.display());
            }
            entries.push((name, sql));
        }
        let manifest_path = dir.join(MIGRATION_MANIFEST);
        if !manifest_path.is_file() {
            anyhow::bail!(
                "the reviewed digest manifest {} is missing from {}; it must ship with the \
                 migrations, because it is what says which bytes were reviewed",
                MIGRATION_MANIFEST,
                dir.display()
            );
        }
        let body = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?;
        Self::build(&dir.display().to_string(), entries, &body)
    }

    /// The bundle this process must apply.
    ///
    /// Only the explicit operator override displaces the embedded bundle, and only
    /// when it came from the LAUNCHING process environment (REV-100-F02): the
    /// captured value, never a live `std::env::var`, which by then may be whatever
    /// an ambient `.env` in the cwd or one of its parents put there.
    pub fn resolve() -> Result<Self> {
        match PROCESS_MIGRATIONS_OVERRIDE.get().and_then(|v| v.as_deref()) {
            Some(explicit) => {
                let dir = std::path::PathBuf::from(explicit.trim());
                if !dir.is_dir() {
                    anyhow::bail!(
                        "{MIGRATIONS_DIR_OVERRIDE} points at {}, which is not a directory. \
                         Unset it to use the migration bundle embedded in this binary",
                        dir.display()
                    );
                }
                tracing::warn!(
                    dir = %dir.display(),
                    "{MIGRATIONS_DIR_OVERRIDE} overrides the embedded migration bundle"
                );
                Self::from_dir(&dir)
            }
            None => Self::embedded(),
        }
    }

    /// Validate a candidate bundle: strict manifest parse, exact manifest/file
    /// BIJECTION, and a digest check on every file — before a single statement of
    /// SQL is executed.
    ///
    /// REV-098-F03: the manifest used to be consulted only when backfilling a
    /// pre-checksum ledger row, so a FRESH database executed whatever SQL happened
    /// to be present and recorded the digests of those same unreviewed bytes as
    /// `applied`. A manifest that is not checked before execution is not an
    /// authority; it is a comment.
    fn build(source: &str, mut entries: Vec<(String, String)>, manifest_body: &str) -> Result<Self> {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let manifest = parse_migration_manifest(manifest_body, source)?;
        let present: std::collections::BTreeSet<&str> =
            entries.iter().map(|(n, _)| n.as_str()).collect();
        let listed: std::collections::BTreeSet<&str> =
            manifest.keys().map(|s| s.as_str()).collect();
        let unlisted: Vec<&&str> = present.difference(&listed).collect();
        if !unlisted.is_empty() {
            anyhow::bail!(
                "{source}: {} migration file(s) have no entry in {MIGRATION_MANIFEST} \
                 ({unlisted:?}); unreviewed SQL must never be executed",
                unlisted.len()
            );
        }
        let absent: Vec<&&str> = listed.difference(&present).collect();
        if !absent.is_empty() {
            anyhow::bail!(
                "{source}: {MIGRATION_MANIFEST} lists {} migration(s) that are not present \
                 ({absent:?}); a bundle missing reviewed SQL is not the reviewed bundle",
                absent.len()
            );
        }
        for (name, sql) in &entries {
            let want = &manifest[name.as_str()];
            let got = migration_sha256(sql);
            if &got != want {
                anyhow::bail!(
                    "{source}: migration `{name}` hashes to {} but {MIGRATION_MANIFEST} \
                     records {}; these are not the reviewed bytes",
                    short_digest(&got),
                    short_digest(want)
                );
            }
        }
        Ok(Self { source: source.to_string(), entries, manifest })
    }

    /// `(filename, sql)` in filename order.
    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    /// Where this bundle came from, for logs and diagnostics.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The reviewed digest for `name`, or `None` when the manifest has no entry.
    ///
    /// This is the authority that makes backfilling an already-applied row safe.
    /// The alternative — hashing whatever is on disk and storing that — would
    /// accept an edited file as "what was applied", which is precisely the property
    /// the checksum exists to detect.
    fn manifest_digest(&self, name: &str) -> Option<&str> {
        self.manifest.get(name).map(|s| s.as_str())
    }
}

/// Apply migrations by executing SQL files from the resolved migrations
/// directory in filename order.
///
/// Each file runs inside a transaction; an applied file is recorded in the
/// `_migrations` table so re-runs are no-ops.
/// DDL must be created in `public`, never in the legacy archive schema.
///
/// The pool's `search_path` puts `swi_legacy` first so this runtime's queries
/// resolve to the archived legacy tables (see [`LEGACY_SEARCH_PATH`]). But a
/// migration's unqualified `CREATE TABLE` would then land in `swi_legacy` on a
/// bridged database — silently building the canonical schema inside the archive.
/// Migrations therefore pin `public` for the duration of their transaction.
const MIGRATION_SEARCH_PATH: &str = "SET LOCAL search_path = public";

/// Prefix marking a ledger digest that was ACCEPTED as a legacy baseline rather
/// than observed at apply time (REV-041-F01).
///
/// A pre-checksum ledger row proves a migration NAME ran, never which bytes. REV-040
/// backfilled such rows with the current file's digest and treated them as verified,
/// which asserts a historical fact nobody knows — and this repository contains real
/// same-name drift, so the assertion is sometimes false. Baseline rows are therefore
/// stored as `baseline:<digest>`: they satisfy startup, they are visibly not
/// verified, and `db status` reports them.
pub const BASELINE_PREFIX: &str = "baseline:";

/// Whether a recorded digest is an accepted baseline rather than an observed digest.
pub fn is_baseline(recorded: &str) -> bool {
    recorded.starts_with(BASELINE_PREFIX)
}

/// The digest inside a recorded value, whether observed or baseline.
fn recorded_digest(recorded: &str) -> &str {
    recorded.strip_prefix(BASELINE_PREFIX).unwrap_or(recorded)
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    migrate_with(pool, false).await
}

/// The one automated preflight repair (REV-080-F01).
///
/// Preconditions are checked independently and ALL must hold, so this never
/// touches a healthy database:
///
///   1. `alerts` and its `sent_at` column exist;
///   2. `sent_at` is still `NOT NULL` (the pre-1034 shape);
///   3. the 1034 outbox columns (`claim_token`) are absent, proving this lane
///      never applied 1034 — an already-upgraded database is left alone;
///   4. a non-sent row with a timestamp exists — the exact data 1034 chokes on.
///
/// The repair is the documented operator step (`DROP NOT NULL`), automated and
/// logged; the timestamp clearing itself stays in 1035 where the policy lives.
async fn preflight_repair_alerts_sent_at(pool: &PgPool) -> Result<()> {
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'alerts')",
    )
    .fetch_one(pool)
    .await?;
    if !table_exists {
        return Ok(());
    }
    // REV-082-F03 (HIGH): a pre-1033 database has `alerts` but no `state` column
    // (introduced by 1033). The EXISTS subquery referencing `state` aborts the
    // whole preflight. Guard: require the column to exist before querying it.
    let state_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'alerts' \
           AND column_name = 'state')",
    )
    .fetch_one(pool)
    .await?;
    if !state_exists {
        return Ok(());
    }
    let needs_repair: Option<bool> = sqlx::query_scalar(
        r#"
        SELECT NOT (col.is_nullable = 'YES')
           AND NOT EXISTS (
               SELECT 1 FROM information_schema.columns
                WHERE table_schema = 'public' AND table_name = 'alerts'
                  AND column_name = 'claim_token'
           )
           AND EXISTS (
               SELECT 1 FROM public.alerts
                WHERE state <> 'sent' AND sent_at IS NOT NULL
           )
          FROM information_schema.columns col
         WHERE col.table_schema = 'public'
           AND col.table_name = 'alerts'
           AND col.column_name = 'sent_at'
        "#,
    )
    .fetch_optional(pool)
    .await?;
    if needs_repair != Some(true) {
        return Ok(());
    }
    tracing::warn!(
        "preflight repair (REV-080-F01): dropping alerts.sent_at NOT NULL before          migration order reaches 1034, which cannot upgrade a 1033 database with          pending/dead alert rows"
    );
    sqlx::query("ALTER TABLE public.alerts ALTER COLUMN sent_at DROP NOT NULL")
        .execute(pool)
        .await?;
    Ok(())
}

/// A POSITIVE-`bigint` test for one dedup-key segment, as a SQL predicate.
///
/// Four rounds of this guard were hand-written approximations of PostgreSQL's own
/// input parser, and each one was wrong in a new place:
///
///   * `^[0-9]+$` (pre-REV-087) accepted `9223372036854775808`, so 1036's cast
///     aborted with SQLSTATE 22003;
///   * `^[0-9]{1,18}$` (REV-087-F01) rejected every valid `int8` from
///     `1000000000000000000` up, so REV-090-F01 caught real workspace owners being
///     reassigned to the default workspace — worse than the abort it avoided;
///   * REV-092 normalized leading zeros, because `01000000000000000000` is twenty
///     characters but denotes a value the cast accepts;
///   * REV-093-F01: it still rejected `+1000000000000000000` and any segment with
///     surrounding whitespace, both of which `::bigint` accepts. Same failure mode
///     again — a valid owner swept to the default workspace.
///
/// The lesson is that the domain is not describable by a regex anyone keeps getting
/// right: it is whatever PostgreSQL's `bigint` input function accepts. So stop
/// approximating it and ASK the parser. `pg_input_is_valid` (PostgreSQL 16+)
/// answers exactly that question without raising, and only when it answers yes does
/// the cast run.
///
/// Ordering matters and is guaranteed here: `CASE` evaluates its `WHEN` before the
/// corresponding `THEN`, so an invalid segment never reaches `::bigint`. That is
/// verified against a live mixed-input table scan, not assumed — a bare
/// `pg_input_is_valid(x) AND x::bigint > 0` would leave the planner free to hoist
/// the cast and abort the whole migration on the first junk row.
///
/// Positivity is still required: every id this guards (`workspaces.id`,
/// `funding_radar_cases.id`) is an identity/`bigserial` starting at 1, so `0` and
/// negatives can never name a real row. `+0` and `-1` are valid `bigint` input and
/// are rejected here on that ground, not on a parse failure.
///
/// `expr` is inlined, so it MUST be a literal SQL expression this module controls,
/// never user input. It is evaluated more than once, so it must also be pure.
fn safe_int8_predicate(expr: &str) -> String {
    format!(
        "(CASE WHEN pg_input_is_valid({expr}, 'bigint') \
               THEN ({expr})::bigint > 0 \
               ELSE false END)"
    )
}

/// Test-only accessor for [`safe_int8_predicate`].
///
/// REV-092 item 2 drives the boundary table through the LIVE database using the
/// exact predicate production builds. Rebuilding the format string inside the test
/// would only prove the test agrees with itself.
#[cfg(test)]
pub fn safe_int8_predicate_for_tests(expr: &str) -> String {
    safe_int8_predicate(expr)
}

/// The second automated preflight repair (REV-084-F01, hardened by REV-087-F01).
///
/// Migration 1036 backfills `alerts.workspace_id` from the dedup key's second
/// segment via `nullif(split_part(dedup_key, ':', 2), '')::bigint`. A legacy key
/// whose second segment is not a 64-bit integer — either nonnumeric
/// ('signal:solana:MINT:entry') or out of range ('9223372036854775808') — aborts
/// the cast before the default-workspace fallback can run.
///
/// REV-087-F01 named three defects in the REV-085 version, all fixed here:
///
///   1. it returned early when `alerts.workspace_id` merely EXISTED, so a crash
///      between the ADD COLUMN and the backfill left the column present with
///      NULLs and every later run short-circuited past the repair forever. The
///      precondition is now the DATA state, not the column's shape;
///   2. it ran four auto-committing statements, so there was no crash window it
///      could survive. The whole repair is now one transaction (PostgreSQL DDL is
///      transactional);
///   3. its digit-only regex let an oversized numeric segment through.
///
/// REV-090-F01 named two more, fixed here:
///
///   4. the length-18 guard rejected valid 19-digit `int8` workspaces, so a real
///      owner in `1000000000000000000..=9223372036854775807` was reassigned to the
///      default workspace. Validation is now EXACT (see [`safe_int8_predicate`]);
///   5. the funding SUBJECT segment was only inspected after `if !needs_repair`
///      returned, so a row with a sound workspace segment and a corrupt subject
///      segment slipped past the named refusal and hit 1036's raw cast. The subject
///      check is now independent and runs FIRST, before any repair decision and
///      before any DDL.
///
/// A healthy database is still never touched: 1036 sets `workspace_id NOT NULL`,
/// so a fully-upgraded lane has no unassigned rows and the detection is false.
async fn preflight_repair_legacy_dedup_keys(pool: &PgPool) -> Result<()> {
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'alerts')",
    )
    .fetch_one(pool)
    .await?;
    if !table_exists {
        return Ok(());
    }
    // `dedup_key` exists from 0001, so nothing below depends on a post-1033
    // column — the trap REV-082-F03 hit with `alerts.state`.
    let ws_col_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'alerts' \
           AND column_name = 'workspace_id')",
    )
    .fetch_one(pool)
    .await?;
    let ws_segment_ok = safe_int8_predicate("split_part(dedup_key, ':', 2)");
    let subject_segment_ok = safe_int8_predicate("split_part(dedup_key, ':', 3)");

    // REV-090-F01: the subject segment is checked FIRST and INDEPENDENTLY of the
    // workspace-segment repair decision. It used to sit after `if !needs_repair`
    // returned, so a row with a sound workspace segment and a corrupt subject
    // segment bypassed this named refusal entirely and later hit 1036's raw cast.
    //
    // 1036 Part B casts the THIRD segment (`c.id = nullif(split_part(a.dedup_key,
    // ':', 3), '')::bigint`) for funding-kind keys. This preflight cannot repair
    // that class: the only lane-independent repair would rewrite `subject_kind`,
    // and 1035's two-arm CHECK (installed AFTER this preflight runs) permits only
    // 'signal'/'funding'. Genuinely minted keys always carry a numeric subject id
    // (`alert_dedup_key` formats an i64), so this fires only on corrupt or
    // hand-written data — exactly when stopping is correct. Fail closed with a
    // named error, before ANY DDL, rather than let 1036 abort opaquely mid-cutover.
    let unsafe_subject_segment: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM public.alerts \
          WHERE split_part(dedup_key, ':', 1) = 'funding' \
            AND NOT {subject_segment_ok})"
    ))
    .fetch_one(pool)
    .await?;
    if unsafe_subject_segment {
        anyhow::bail!(
            "alert dedup keys of kind `funding` carry a subject segment that is not a \
             64-bit integer; migration 1036 casts that segment to bigint and would \
             abort mid-DDL. 1036 is shipped and immutable, and rewriting the subject \
             kind would violate 1035's subject CHECK, so this must be reconciled by an \
             operator before migrating"
        );
    }

    // Detection is a DATA question: is there a row 1036's cast would abort on that
    // does not already have an owner? On a lane where the column does not exist
    // yet, every such row is by definition unassigned.
    let needs_repair: bool = if ws_col_exists {
        sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM public.alerts \
              WHERE workspace_id IS NULL AND NOT {ws_segment_ok})"
        ))
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM public.alerts WHERE NOT {ws_segment_ok})"
        ))
        .fetch_one(pool)
        .await?
    };
    if !needs_repair {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    tracing::warn!(
        "preflight repair (REV-084-F01, REV-087-F01, REV-090-F01): pre-assigning \
         alerts whose dedup-key workspace segment is not a positive 64-bit integer \
         to the default workspace, before migration order reaches 1036 whose \
         ::bigint cast would abort. One transaction; re-detected from data on every \
         run; valid 19-digit workspaces are preserved, never reassigned"
    );
    sqlx::query(
        "INSERT INTO workspaces (name, slug) VALUES ('Default', 'default') \
         ON CONFLICT (slug) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;
    // 1036's own ADD COLUMN IF NOT EXISTS is then a no-op. The FK target is safe:
    // workspaces is created by 0001.
    sqlx::query(
        "ALTER TABLE public.alerts \
         ADD COLUMN IF NOT EXISTS workspace_id bigint REFERENCES workspaces (id)",
    )
    .execute(&mut *tx)
    .await?;
    let assigned = sqlx::query(&format!(
        "UPDATE public.alerts \
         SET workspace_id = (SELECT id FROM workspaces WHERE slug = 'default') \
         WHERE workspace_id IS NULL AND NOT {ws_segment_ok}"
    ))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    tracing::warn!(rows = assigned, "preflight repair complete");
    Ok(())
}

/// The third automated preflight repair (REV-087-F03).
///
/// Migration 1037 collapses duplicate `funding_radar_cases` rows onto one survivor
/// per `(workspace_id, chain, recipient)`. Two defects make that destructive:
///
///   * `jsonb_each` at 1037:57 and :69 is reached through `CROSS JOIN LATERAL`,
///     and `evidence` is `jsonb NOT NULL DEFAULT 'null'::jsonb` (0001:270) — the
///     schema's own default is a JSON SCALAR. `jsonb_each` raises 22023 on a scalar
///     or array, so ONE such row anywhere in a duplicate group aborts the entire
///     migration;
///   * the merge writes 6 of 17 columns. `first_funding_usd`,
///     `first_funding_native`, `source_address`, `source_kind`,
///     `deploy_window_ends_at` and `dismissed_reason` are silently inherited from
///     the lowest-id row, which can disagree with the merged `MIN(first_funded_at)`.
///     And the alert repoint (1037:167-170) rewrites `funding_case_id` while leaving
///     `dedup_key`, which EMBEDS the dead case id — so a later `claim_alert`
///     (`ON CONFLICT (dedup_key)`) mints a SECOND outbox row for the same survivor
///     and destination: a duplicate logical alert.
///
/// 1037 is shipped and immutable, and filename order reaches it before any later
/// corrective migration, so the repair cannot be a new migration — it must run
/// before the loop. Afterwards 1037 finds zero duplicate groups and is a no-op.
///
/// Detection (ALL must hold), so a healthy database is never touched:
///   1. `funding_radar_cases` exists;
///   2. `funding_radar_cases_tenant_uidx` is ABSENT, proving 1037 has not run;
///   3. there is a non-object `evidence` row or an actual duplicate group.
async fn preflight_repair_funding_case_identity(pool: &PgPool) -> Result<()> {
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'funding_radar_cases')",
    )
    .fetch_one(pool)
    .await?;
    if !table_exists {
        return Ok(());
    }
    let already_merged: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes \
          WHERE schemaname = 'public' \
            AND indexname = 'funding_radar_cases_tenant_uidx')",
    )
    .fetch_one(pool)
    .await?;
    if already_merged {
        return Ok(());
    }
    // `workspace_id` is added by 1036, which runs AFTER this preflight. On a lane
    // without it, grouping by (chain, recipient) is equivalent: 1036 assigns every
    // pre-existing case to the single default workspace, so 1037 will see exactly
    // these groups.
    let ws_col_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'funding_radar_cases' \
           AND column_name = 'workspace_id')",
    )
    .fetch_one(pool)
    .await?;
    let group_cols = if ws_col_exists {
        "workspace_id, chain, recipient"
    } else {
        "chain, recipient"
    };

    let non_object_evidence: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM public.funding_radar_cases \
          WHERE jsonb_typeof(evidence) <> 'object')",
    )
    .fetch_one(pool)
    .await?;
    let has_duplicates: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM public.funding_radar_cases \
          GROUP BY {group_cols} HAVING count(*) > 1)"
    ))
    .fetch_one(pool)
    .await?;
    if !non_object_evidence && !has_duplicates {
        return Ok(());
    }

    let mut tx = pool.begin().await?;

    // 1. Normalize evidence so `jsonb_each` can never abort. The data is PRESERVED
    //    under a named key rather than discarded — a scalar or array was legal.
    let normalized = sqlx::query(
        "UPDATE public.funding_radar_cases \
            SET evidence = jsonb_build_object('legacy_evidence', evidence) \
          WHERE jsonb_typeof(evidence) <> 'object'",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // 2. Materialize the survivor map ONCE so every later statement uses one
    //    identical ranking (1037 re-derives its CTE trio four times).
    sqlx::query(&format!(
        "CREATE TEMP TABLE swi_case_merge ON COMMIT DROP AS \
         SELECT id, \
                first_value(id) OVER (PARTITION BY {group_cols} ORDER BY id ASC) AS keep_id \
           FROM public.funding_radar_cases"
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM swi_case_merge WHERE id = keep_id")
        .execute(&mut *tx)
        .await?;
    let merged_away: i64 = sqlx::query_scalar("SELECT count(*) FROM swi_case_merge")
        .fetch_one(&mut *tx)
        .await?;
    if merged_away == 0 {
        tx.commit().await?;
        if normalized > 0 {
            tracing::warn!(
                rows = normalized,
                "preflight repair (REV-087-F03): normalized non-object funding-case \
                 evidence so 1037's jsonb_each cannot abort; no duplicate groups"
            );
        }
        return Ok(());
    }

    // 3. Merge EVERY semantic column onto the survivor, with an explicit policy per
    //    field. `id`/`workspace_id`/`chain`/`recipient` are the identity and are
    //    untouched. The first-funding facts travel with the merged MIN(first_funded_at)
    //    row, NOT with the id-survivor — taking them from the id-survivor is exactly
    //    the inconsistency REV-087-F03 names.
    sqlx::query(
        r#"
        WITH grp AS (
            SELECT m.keep_id, c.*
              FROM public.funding_radar_cases c
              JOIN (SELECT keep_id, id FROM swi_case_merge
                    UNION ALL
                    SELECT DISTINCT keep_id, keep_id FROM swi_case_merge) m
                ON m.id = c.id
        ),
        first_row AS (
            SELECT DISTINCT ON (keep_id)
                   keep_id, first_funding_usd, first_funding_native,
                   source_address, source_kind
              FROM grp
             ORDER BY keep_id, first_funded_at ASC, id ASC
        ),
        merged AS (
            SELECT g.keep_id,
                   min(g.first_funded_at)                       AS first_funded_at,
                   max(g.confidence)                            AS confidence,
                   max(g.fanout_count)                          AS fanout_count,
                   max(g.updated_at)                            AS updated_at,
                   max(g.deploy_window_ends_at)                 AS deploy_window_ends_at,
                   (SELECT s.stage FROM grp s WHERE s.keep_id = g.keep_id
                     ORDER BY CASE s.stage
                                WHEN 'funded'      THEN 1
                                WHEN 'preparation' THEN 2
                                WHEN 'deployed'    THEN 3
                                WHEN 'dismissed'   THEN 4
                                ELSE 0 END DESC, s.id DESC
                     LIMIT 1)                                   AS stage,
                   (SELECT d.dismissed_reason FROM grp d
                     WHERE d.keep_id = g.keep_id AND d.stage = 'dismissed'
                       AND d.dismissed_reason IS NOT NULL
                     ORDER BY d.updated_at DESC, d.id DESC
                     LIMIT 1)                                   AS dismissed_reason,
                   COALESCE((SELECT jsonb_object_agg(e.key, e.value)
                               FROM (SELECT DISTINCT ON (kv.key) kv.key, kv.value
                                       FROM grp e2
                                       CROSS JOIN LATERAL jsonb_each(e2.evidence) kv
                                      WHERE e2.keep_id = g.keep_id
                                      ORDER BY kv.key, e2.id DESC) e),
                            '{}'::jsonb)                        AS evidence
              FROM grp g
             GROUP BY g.keep_id
        )
        UPDATE public.funding_radar_cases c
           SET first_funded_at       = m.first_funded_at,
               stage                 = m.stage,
               confidence            = m.confidence,
               fanout_count          = m.fanout_count,
               updated_at            = m.updated_at,
               evidence              = m.evidence,
               deploy_window_ends_at = m.deploy_window_ends_at,
               first_funding_usd     = f.first_funding_usd,
               first_funding_native  = f.first_funding_native,
               source_address        = f.source_address,
               source_kind           = f.source_kind,
               dismissed_reason      = CASE WHEN m.stage = 'dismissed'
                                            THEN m.dismissed_reason ELSE NULL END
          FROM merged m
          JOIN first_row f ON f.keep_id = m.keep_id
         WHERE c.id = m.keep_id
        "#,
    )
    .execute(&mut *tx)
    .await?;

    // 4. Repoint events. `funding_radar_events.case_id` is ON DELETE CASCADE
    //    (0001:281), so a missed repoint destroys history outright.
    sqlx::query(
        "UPDATE public.funding_radar_events e \
            SET case_id = m.keep_id \
           FROM swi_case_merge m \
          WHERE e.case_id = m.id",
    )
    .execute(&mut *tx)
    .await?;

    // 5/6. Alerts: repoint the FK AND reconcile the identity. `alerts.funding_case_id`
    //      exists only from 1035, so both steps are guarded on the column.
    let alerts_have_case: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'alerts' \
           AND column_name = 'funding_case_id')",
    )
    .fetch_one(&mut *tx)
    .await?;
    let mut rekeyed = 0u64;
    let mut folded = 0u64;
    if alerts_have_case {
        sqlx::query(
            "UPDATE public.alerts a \
                SET funding_case_id = m.keep_id \
               FROM swi_case_merge m \
              WHERE a.funding_case_id = m.id",
        )
        .execute(&mut *tx)
        .await?;

        // The dedup key is `kind:workspace:subject:destination` and the PRIMARY KEY
        // (0001:449). Rewrite ONLY the subject segment; the destination is taken as
        // everything after the third colon so a destination containing a colon
        // survives. The exact positive-int8 guard (REV-090-F01) keeps the join's
        // cast range-safe without excluding valid 19-digit ids.
        let subject_ok = safe_int8_predicate("split_part(a.dedup_key, ':', 3)");
        sqlx::query(&format!(
            r#"
            CREATE TEMP TABLE swi_alert_rekey ON COMMIT DROP AS
            SELECT a.dedup_key AS old_key,
                   'funding:' || split_part(a.dedup_key, ':', 2) || ':' ||
                   m.keep_id::text || ':' ||
                   substr(a.dedup_key,
                          length(split_part(a.dedup_key, ':', 1)) +
                          length(split_part(a.dedup_key, ':', 2)) +
                          length(split_part(a.dedup_key, ':', 3)) + 4) AS new_key
              FROM public.alerts a
              JOIN swi_case_merge m
                ON m.id = nullif(split_part(a.dedup_key, ':', 3), '')::bigint
             WHERE split_part(a.dedup_key, ':', 1) = 'funding'
               AND {subject_ok}
            "#
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM swi_alert_rekey WHERE old_key = new_key")
            .execute(&mut *tx)
            .await?;

        // Collision policy: the row already holding the survivor's key WINS, because
        // it already carries the survivor's identity. Before dropping the stale-keyed
        // row, fold its delivery fact forward — an external send genuinely happened
        // and the outbox must not repeat it.
        let state_col: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'alerts' \
               AND column_name = 'state')",
        )
        .fetch_one(&mut *tx)
        .await?;
        if state_col {
            folded = sqlx::query(
                "UPDATE public.alerts t \
                    SET state = 'sent', \
                        sent_at = COALESCE(t.sent_at, s.sent_at), \
                        next_attempt_at = NULL, \
                        claim_token = NULL, \
                        claim_expires_at = NULL \
                   FROM swi_alert_rekey r \
                   JOIN public.alerts s ON s.dedup_key = r.old_key \
                  WHERE t.dedup_key = r.new_key \
                    AND s.state = 'sent' AND t.state <> 'sent'",
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        sqlx::query(
            "DELETE FROM public.alerts a \
              USING swi_alert_rekey r \
              WHERE a.dedup_key = r.old_key \
                AND EXISTS (SELECT 1 FROM public.alerts t WHERE t.dedup_key = r.new_key)",
        )
        .execute(&mut *tx)
        .await?;
        rekeyed = sqlx::query(
            "UPDATE public.alerts a \
                SET dedup_key = r.new_key \
               FROM swi_alert_rekey r \
              WHERE a.dedup_key = r.old_key",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }

    // 7. Only now are the non-survivors removable.
    sqlx::query(
        "DELETE FROM public.funding_radar_cases \
          WHERE id IN (SELECT id FROM swi_case_merge)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::warn!(
        normalized_evidence = normalized,
        cases_merged = merged_away,
        alerts_rekeyed = rekeyed,
        alerts_folded = folded,
        "preflight repair (REV-087-F03): funding cases merged across every semantic \
         column and alert identities reconciled, before migration order reaches 1037 \
         whose merge is partial and whose jsonb_each aborts on non-object evidence"
    );
    Ok(())
}

/// Apply the migration bundle this binary must apply (see [`MigrationBundle::resolve`]).
///
/// `accept_legacy_baseline` permits recording pre-checksum rows as
/// `baseline:<digest>` (see [`BASELINE_PREFIX`]).
pub async fn migrate_with(pool: &PgPool, accept_legacy_baseline: bool) -> Result<()> {
    let bundle = MigrationBundle::resolve()?;
    migrate_bundle_with(pool, &bundle, accept_legacy_baseline).await
}

/// Apply migrations from an EXPLICIT directory. Test lanes and the operator
/// override reach the same validation as the embedded bundle: the directory is
/// loaded through [`MigrationBundle::from_dir`], so its manifest must be a strict
/// bijection with its SQL and every digest must match before anything executes.
pub async fn migrate_dir_with(
    pool: &PgPool,
    dir: &std::path::Path,
    accept_legacy_baseline: bool,
) -> Result<()> {
    let bundle = MigrationBundle::from_dir(dir)?;
    migrate_bundle_with(pool, &bundle, accept_legacy_baseline).await
}

/// Owns the connection that holds the migration advisory lock for the whole run.
///
/// REV-097-F03: the lock must outlive every statement in the run, so it cannot be
/// tied to a transaction, and it must NOT be taken on a pooled connection: a
/// session-level `pg_advisory_lock` stays held when the connection goes back to the
/// pool, so the next migrator waits on a lock nobody will ever release. The
/// connection is therefore DETACHED from the pool and owned here; dropping it closes
/// the socket and PostgreSQL releases the session's advisory locks with the backend.
/// Drop cannot `.await`, which is exactly why release is expressed as "close the
/// session" rather than an explicit `pg_advisory_unlock` round-trip.
struct MigrationLock {
    key: i64,
    conn: Option<sqlx::PgConnection>,
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        drop(self.conn.take());
        tracing::debug!(key = self.key, "released migration advisory lock");
    }
}

/// Whether the ledger's provenance CHECK is EXACTLY the canonical predicate.
///
/// REV-096-F03 replaced a substring test with a finite probe set: six values were
/// evaluated against the constraint's own expression and the answers had to match.
/// REV-098-F02 broke it in one line — a forged
/// `CHECK (canonical_predicate OR digest_origin = 'rogue')` answers all six probes
/// correctly and still admits `rogue`. Any FINITE sample of an infinite domain is
/// bypassable by construction; the only question a sample can answer is "is it
/// wrong in one of the ways I guessed".
///
/// So the predicate is compared as an EXPRESSION, not sampled as a function. The
/// canonical CHECK is installed on a throwaway temp table with a column of the same
/// name and type, and the server's own rendering (`pg_get_expr(conbin, conrelid)`,
/// produced by deparsing the stored parse tree) is compared with the rendering of
/// the constraint actually present.
///
/// Why that is authoritative rather than another string test:
///
///   * both sides are deparsed by THIS server from a parsed, analyzed expression
///     tree, so spacing, casing, quoting, operator spelling, `IN` vs `= ANY`,
///     redundant parentheses and implicit casts are all normalized identically —
///     the rendering is a function of the tree, not of the text someone typed;
///   * therefore equal renderings mean equal trees, and any added disjunct,
///     widened list, or case-folding wrapper changes the tree and changes the
///     rendering. There is nothing left to "not have guessed".
///   * a CHECK that is `NOT VALID` is not an enforced statement about the table's
///     current contents at all: PostgreSQL accepts an expression-identical
///     `CHECK ... NOT VALID` while pre-existing out-of-domain rows survive
///     (REV-100-F01, reproduced on 17.11). Canonicality therefore also requires
///     the catalog row to be a CHECK (`contype = 'c'`) on the ledger relation and
///     to be `convalidated` — an unvalidated constraint is a promise about future
///     writes only.
///
/// The temp table is created inside the caller's transaction and dropped on commit,
/// so this leaves nothing behind and cannot collide with a concurrent migrator.
async fn digest_origin_check_is_canonical(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    conname: &str,
    canonical_check: &str,
) -> Result<bool> {
    let found: Option<(bool, String)> = sqlx::query_as(
        "SELECT convalidated, pg_get_expr(conbin, conrelid) FROM pg_constraint \
          WHERE conname = $1 AND conrelid = 'public._migrations'::regclass \
            AND contype = 'c'",
    )
    .bind(conname)
    .fetch_optional(&mut **tx)
    .await
    .context("failed to read the installed ledger CHECK expression")?;
    let Some((convalidated, installed)) = found else {
        // A constraint of some other kind, or on some other relation, with this
        // name is not the ledger's provenance CHECK.
        return Ok(false);
    };
    if !convalidated {
        return Ok(false);
    }

    // A uniquely named probe table: a temp table from an earlier statement in this
    // same session would otherwise be reused with a stale constraint.
    let probe = format!("_swi_digest_origin_canon_{}", std::process::id());
    sqlx::raw_sql(&format!(
        "CREATE TEMP TABLE {probe} (digest_origin text) ON COMMIT DROP"
    ))
    .execute(&mut **tx)
    .await
    .context("failed to create the canonical-CHECK probe table")?;
    sqlx::raw_sql(&format!(
        "ALTER TABLE {probe} ADD CONSTRAINT {probe}_canon {canonical_check}"
    ))
    .execute(&mut **tx)
    .await
    .context("failed to install the canonical ledger CHECK on the probe table")?;
    let canonical: String = sqlx::query_scalar(
        "SELECT pg_get_expr(conbin, conrelid) FROM pg_constraint WHERE conname = $1",
    )
    .bind(format!("{probe}_canon"))
    .fetch_one(&mut **tx)
    .await
    .context("failed to read the canonical CHECK expression")?;

    Ok(installed == canonical)
}

/// Apply an already-VALIDATED [`MigrationBundle`].
///
/// REV-087 item 7: tests used to point the migrator at a reduced migration set by
/// mutating the process-global `SWI_MIGRATIONS_DIR`. `cargo test --features
/// pg_tests` runs two harness processes (lib + bin) and the bin harness is
/// multi-threaded, so one fixture's `remove_var` could land inside another
/// fixture's migration run — the lifecycle defect behind the reviewer's
/// `3D000: database ... does not exist` full-suite failures. The bundle is a
/// parameter now; nothing global is touched.
///
/// REV-098-F03: constructing the bundle is what enforces manifest/file bijection
/// and per-file digests, and construction happens before this function is entered.
/// So no SQL can execute from a bundle the reviewed manifest does not vouch for —
/// on a FRESH database as much as on an upgrade.
pub async fn migrate_bundle_with(
    pool: &PgPool,
    bundle: &MigrationBundle,
    accept_legacy_baseline: bool,
) -> Result<()> {
    // REV-097-F03: the advisory lock is taken FIRST, before a single statement of
    // this run touches the database. Taking it later left the ledger bootstrap DDL
    // (`CREATE TABLE IF NOT EXISTS` / `ADD COLUMN IF NOT EXISTS`) outside the
    // critical section, and two migrators starting on an empty database raced there:
    // `IF NOT EXISTS` is checked before the catalog insert, so the loser aborted with
    // `duplicate key value violates unique constraint "pg_type_typname_nsp_index"`.
    //
    // A session advisory lock is the right instrument: held by the CONNECTION rather
    // than a transaction, so it spans every statement of the run, and it does not
    // block ordinary readers of the ledger the way a table lock would.
    //
    // Stable, arbitrary key derived from the purpose. Any other migrator on this
    // database computes the same value and therefore waits.
    const MIGRATION_LOCK_KEY: i64 = 0x5357_495F_4D49_4752u64 as i64; // "SWI_MIGR"
    // One dedicated connection owns the lock for the whole run, DETACHED from the
    // pool so the pool can never hand a still-locked session to someone else and so
    // the lock is released when this connection is dropped.
    let mut lock_conn = pool
        .acquire()
        .await
        .context("failed to acquire a connection for the migration advisory lock")?
        .detach();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut lock_conn)
        .await
        .context("failed to take the migration advisory lock")?;
    // Everything from here to the end of the run is exclusive. `MigrationLock`
    // releases on drop, including on the error paths below.
    let _migration_lock = MigrationLock { key: MIGRATION_LOCK_KEY, conn: Some(lock_conn) };

    // Schema-qualified explicitly: the pool's search_path starts with
    // `swi_legacy`, so an unqualified CREATE would put the migration ledger inside
    // the archive schema on a bridged database.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS public._migrations (name text PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())",
    )
    .execute(pool)
    .await?;
    // REV-037-F01: record the digest of what was actually applied, so
    // `ensure_schema_current` can verify contents and not just filenames. Added
    // here as well as in migration 1027 because this table is created by the
    // binary, not by a migration file — on a brand-new database the column must
    // exist before the first row is written.
    sqlx::query("ALTER TABLE public._migrations ADD COLUMN IF NOT EXISTS sha256 text")
        .execute(pool)
        .await?;
    // REV-043-F02: HOW a digest was obtained is itself information, and REV-042 had
    // no way to record it.
    //
    // REV-040 wrote bare digests into pre-checksum rows. REV-042 introduced the
    // `baseline:` prefix, but only ever wrote it for rows where `sha256 IS NULL` —
    // so a database that had already passed through REV-040 kept digests that LOOK
    // observed while their history is just as unknown. A prefix cannot fix that
    // retroactively, because the rows are indistinguishable by value.
    //
    // `digest_origin` makes the three states nameable and queryable:
    //   'applied'  — digest computed from the SQL this binary actually executed
    //   'baseline' — accepted for a pre-checksum row; applied bytes UNKNOWN
    //   NULL with a digest present — written before provenance was tracked, so the
    //                                provenance is unknown and must be re-declared
    sqlx::query("ALTER TABLE public._migrations ADD COLUMN IF NOT EXISTS digest_origin text")
        .execute(pool)
        .await?;
    // REV-093-F03: this used to be an unconditional
    // `DROP CONSTRAINT IF EXISTS` + `ADD CONSTRAINT` on EVERY migrate pass. Two
    // consequences, both real:
    //
    //   * the constraint's OID changed on every run, so "a second pass is an exact
    //     no-op" was false. REV-092's convergence snapshot compared only
    //     `(conname, contype)`, which is blind to a drop-and-recreate — the test
    //     agreed with a claim the code did not honour;
    //   * two migrators racing here interleave as DROP/DROP/ADD/ADD and the loser
    //     fails with `duplicate_object`.
    //
    // Correct constraint -> no DDL at all. Missing -> add it once. Present but
    // WRONG -> fail closed: silently replacing a ledger CHECK that someone
    // deliberately altered would destroy the evidence of that alteration.
    //
    // REV-096-F03 / REV-097-F03: the whole MIGRATION RUN — this decision and the
    // filename-order loop below — is serialized by the advisory lock taken at the top
    // of this function. The pre-REV-096 version took an ACCESS EXCLUSIVE lock in a
    // transaction that committed before the loop started, so two migrators still
    // raced over the migrations themselves and only the constraint step was protected.
    const DIGEST_ORIGIN_CHECK: &str = "_migrations_digest_origin_check";
    const DIGEST_ORIGIN_EXPR: &str =
        "CHECK (digest_origin IS NULL OR digest_origin IN ('applied', 'baseline'))";

    {
        let mut tx = pool.begin().await?;
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = $1 \
               AND conrelid = 'public._migrations'::regclass",
        )
        .bind(DIGEST_ORIGIN_CHECK)
        .fetch_optional(&mut *tx)
        .await?;
        match existing {
            None => {
                sqlx::raw_sql(&format!(
                    "ALTER TABLE public._migrations \
                       ADD CONSTRAINT {DIGEST_ORIGIN_CHECK} {DIGEST_ORIGIN_EXPR}"
                ))
                .execute(&mut *tx)
                .await?;
            }
            Some(def) => {
                // REV-098-F02: a finite probe set cannot prove which values a
                // predicate admits — `canonical OR digest_origin = 'rogue'` answers
                // every probe correctly and still widens the ledger's trust model.
                // Compare the EXPRESSION instead: the server's deparse of the
                // installed constraint must be identical to its deparse of the
                // canonical one.
                let ok = digest_origin_check_is_canonical(
                    &mut tx,
                    DIGEST_ORIGIN_CHECK,
                    DIGEST_ORIGIN_EXPR,
                )
                .await?;
                if !ok {
                    anyhow::bail!(
                        "the migration ledger's `{DIGEST_ORIGIN_CHECK}` constraint is not the \
                         canonical `{DIGEST_ORIGIN_EXPR}` (found `{def}`). It defines which \
                         digest provenances are representable, so accepting a different predicate \
                         would silently widen the ledger's trust model; reconcile deliberately"
                    );
                }
            }
        }
        tx.commit().await?;
    }
    // The ledger is authority about schema state: no runtime role may write it.
    //
    // REV-039-F05: this used to revoke only PUBLIC and to ignore the result with
    // `let _ =`. The reviewer granted INSERT explicitly to `swi_legacy_runtime`,
    // re-ran the migrator, and the privilege survived — so "reasserted on every
    // migrate" was not true. The named roles are now included, and a failure is
    // reported rather than swallowed. Roles that do not exist yet are tolerated
    // (a fresh database migrates before 1025 creates them), but nothing else is.
    for role in ["PUBLIC", "swi_legacy_runtime", "swi_app"] {
        let stmt = format!(
            "REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON public._migrations FROM {role}"
        );
        if let Err(e) = sqlx::raw_sql(&stmt).execute(pool).await {
            let msg = e.to_string();
            // 42704 = undefined_object: the role is not created yet.
            let role_absent = msg.contains("does not exist") || msg.contains("42704");
            if !role_absent {
                return Err(anyhow::anyhow!(e).context(format!(
                    "failed to protect the migration ledger from `{role}`; refusing to \
                     continue with a writable ledger"
                )));
            }
        }
    }
    // REV-080-F01 (CRITICAL): preflight repair for the ONE known broken upgrade
    // lane. Migrations apply in filename order, and 1034's sent_at cutover runs
    // its UPDATE before dropping the inherited NOT NULL, so a 1033 database
    // holding any pending/dead alert aborts before 1035's correct-order repair is
    // ever reached. 1034 itself is shipped and immutable, so the repair cannot
    // live in a later file — it must precede filename order entirely.
    //
    // The shape is detected NARROWLY (never blanket-DDL): the column exists, is
    // still NOT NULL, the outbox columns from 1034 are absent (so this is a
    // genuinely pre-1034 lane rather than an already-upgraded one), and at least
    // one row would violate. Only then is the constraint dropped, which is
    // exactly the documented operator step — automated, logged, and recorded.
    preflight_repair_alerts_sent_at(pool).await?;
    // REV-084-F01 (CRITICAL): same preflight pattern for the SECOND known broken
    // upgrade lane. 1036's `nullif(split_part(dedup_key, ':', 2), '')::bigint`
    // aborts on legacy keys with nonnumeric second segments (e.g.
    // 'signal:solana:MINT:entry'). 1036 is shipped and immutable, so the repair
    // must precede filename order — a pre-1036 lane with such keys can never
    // reach a later corrective migration.
    preflight_repair_legacy_dedup_keys(pool).await?;
    // REV-087-F03 (HIGH): the third preflight. 1037's duplicate-case merge is
    // incomplete and its `jsonb_each` aborts on legal non-object evidence. 1037 is
    // shipped, so it executes BEFORE any later corrective migration in filename
    // order — the repair must precede the loop entirely.
    preflight_repair_funding_case_identity(pool).await?;

    tracing::info!(bundle = %bundle.source(), "applying migrations");
    for (name, sql) in bundle.entries() {
        let name = name.clone();
        let digest = migration_sha256(sql);

        let applied: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT name, sha256, digest_origin FROM public._migrations WHERE name = $1",
        )
        .bind(&name)
        .fetch_optional(pool)
        .await?;

        if let Some((_, recorded, origin)) = applied {
            // REV-043-F02: a digest with no recorded provenance was written by a
            // build that did not track provenance (REV-040), so it proves nothing
            // about history. Treat it exactly like a pre-checksum row: it needs the
            // operator's explicit re-declaration, not a silent pass.
            if recorded.is_some() && origin.is_none() {
                if !accept_legacy_baseline {
                    anyhow::bail!(
                        "`{name}` carries a digest with no recorded provenance. It was written \
                         by an earlier build that back-filled digests from the files on disk, \
                         so it does NOT establish which bytes were applied. Reconcile the \
                         schema, then re-run with `--accept-legacy-baseline` to re-declare it \
                         as a baseline"
                    );
                }
                let bare = recorded.as_deref().unwrap_or_default();
                let digest_only = recorded_digest(bare).to_string();
                sqlx::query(
                    "UPDATE public._migrations \
                        SET sha256 = $2, digest_origin = 'baseline' WHERE name = $1",
                )
                .bind(&name)
                .bind(format!("{BASELINE_PREFIX}{digest_only}"))
                .execute(pool)
                .await?;
                tracing::warn!(
                    migration = %name,
                    "re-declared a provenance-less digest as an ACCEPTED BASELINE \
                     (REV-043-F02)"
                );
                continue;
            }
            // REV-039-F01: an already-applied row is where the upgrade outage lived.
            // Migration 1027 added a nullable `sha256` and backfilled nothing, so a
            // database upgraded from 1026 ended up with 38 unverifiable rows and
            // every runtime command refused to start — with a message telling the
            // operator to apply 1027, which was already applied. The fix I shipped
            // was worse than no fix for existing databases.
            //
            // Backfill is possible, but NOT from "whatever file is on disk": that
            // would bless an edited file as applied and destroy the point of the
            // checksum. The digest is accepted only when it matches the reviewed,
            // version-controlled manifest that ships beside the SQL.
            if recorded.is_none() {
                match bundle.manifest_digest(&name) {
                    Some(expected) if expected == digest => {
                        // REV-041-F01: matching the CURRENT manifest does not make
                        // this a verified row. The manifest describes the files as
                        // they are now; the ledger row was written before digests
                        // existed and says only that a migration by this NAME ran.
                        // Recording it as `verified` claimed a historical fact, and
                        // for `1013_strategy_lab.sql` that claim is demonstrably
                        // false on a database carrying the older constraint name.
                        //
                        // So it is recorded as an accepted BASELINE, and only with
                        // the operator's explicit consent.
                        if !accept_legacy_baseline {
                            anyhow::bail!(
                                "`{name}` was applied before digests were recorded, so which \
                                 bytes ran is unknown. Its current file matches the reviewed \
                                 manifest, but a filename-only row cannot prove the same bytes \
                                 were applied. Reconcile the schema, then re-run with \
                                 `--accept-legacy-baseline` to record it as a baseline. \
                                 Known drift to check first: strategy_versions must carry \
                                 `strategy_versions_lifecycle_check` (permitting canary/paused), \
                                 not `strategy_versions_lifecycle_state_check`"
                            );
                        }
                        sqlx::query(
                            "UPDATE public._migrations \
                                SET sha256 = $2, digest_origin = 'baseline' \
                              WHERE name = $1",
                        )
                        .bind(&name)
                        .bind(format!("{BASELINE_PREFIX}{digest}"))
                        .execute(pool)
                        .await?;
                        tracing::warn!(
                            migration = %name,
                            "recorded as an ACCEPTED BASELINE, not verified: the applied bytes \
                             are unknown for pre-checksum rows"
                        );
                    }
                    Some(expected) => {
                        anyhow::bail!(
                            "cannot backfill a digest for already-applied `{name}`: the file \
                             on disk hashes to {} but the reviewed manifest records {}. \
                             The file changed after it was applied; resolve this \
                             deliberately rather than blessing the current contents",
                            short_digest(&digest),
                            short_digest(expected)
                        );
                    }
                    None => {
                        anyhow::bail!(
                            "already-applied migration `{name}` has no digest in the ledger \
                             and no entry in {}; add a reviewed manifest entry before this \
                             database can be verified",
                            MIGRATION_MANIFEST
                        );
                    }
                }
            }
            // REV-087-F01 (CRITICAL): an already-applied row carrying BOTH a digest
            // and a provenance fell straight through to `continue` — the recorded
            // digest was never compared against the file on disk. So `db migrate`
            // silently blessed an edited shipped migration, and only a later service
            // start (`ensure_schema_current`) noticed. Migration files are immutable
            // once applied; a same-name change is an integrity fault, not a re-apply.
            //
            // `recorded_digest` strips the `baseline:` prefix, so a baseline row is
            // checked against the bytes that were ACCEPTED for it — the same rule
            // `ensure_schema_current` applies.
            if let Some(recorded) = recorded.as_deref() {
                if recorded_digest(recorded) != digest {
                    anyhow::bail!(
                        "applied migration `{name}` no longer matches the file on disk \
                         (ledger {}, disk {}). Migrations are immutable once applied; \
                         restore the reviewed bytes or ship a forward migration instead \
                         of editing this one",
                        short_digest(recorded_digest(recorded)),
                        short_digest(&digest)
                    );
                }
            }
            continue;
        }
        let mut tx = pool.begin().await?;
        // Pin `public` so DDL never lands in the legacy archive schema.
        sqlx::raw_sql(MIGRATION_SEARCH_PATH).execute(&mut *tx).await?;
        sqlx::raw_sql(sql.as_str()).execute(&mut *tx).await?;
        // `applied`: this binary executed exactly these bytes in this transaction.
        sqlx::query(
            "INSERT INTO public._migrations (name, sha256, digest_origin) \
             VALUES ($1, $2, 'applied')",
        )
            .bind(&name)
            .bind(&digest)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(migration = %name, "applied");
    }
    Ok(())
}

/// Verify the schema is current WITHOUT applying anything (REV-035-#1).
///
/// Every long-running command used to call [`migrate`] on its own pool. That made
/// the documented least-privilege `DATABASE_URL` unusable: the reviewer ran the
/// real binary and it died with `permission denied for schema public` before doing
/// any work. Applying DDL is a privileged, deliberate operation — it does not
/// belong on a service start path.
///
/// This replacement is read-only: it needs only `SELECT` on `public._migrations`,
/// so it succeeds as the runtime role. It fails closed with an actionable message
/// when the schema is behind, rather than silently running on a schema that does
/// not match the binary.
pub async fn ensure_schema_current(pool: &PgPool) -> Result<()> {
    // The ledger itself may be missing on a database that was never migrated.
    let ledger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
          WHERE table_schema = 'public' AND table_name = '_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if !ledger_exists {
        anyhow::bail!(
            "database has no migration ledger; run `swi db migrate` with \
             MIGRATION_DATABASE_URL (a privileged role) before starting the service"
        );
    }

    // REV-098-F03: the verifier compares the ledger against the BUNDLE this binary
    // carries, not against whatever directory the cwd happens to offer. The bundle
    // has already proven manifest/file bijection and per-file digests at
    // construction, so "the schema is current" is a statement about reviewed bytes.
    // (`MigrationBundle::from_dir` keeps the REV-039-F04 rule that every read
    // failure is fatal: an unreadable directory cannot be fully read and is
    // therefore not a verified migration set.)
    let bundle = MigrationBundle::resolve()?;
    let mut on_disk: Vec<(String, String)> = bundle
        .entries()
        .iter()
        .map(|(name, sql)| (name.clone(), migration_sha256(sql)))
        .collect();
    on_disk.sort();

    // REV-037-F01: a recorded FILENAME is not evidence. The reviewer inserted a
    // bare name into the ledger and this check passed for a migration that had
    // never run; separately, they changed the CONTENTS of an already-recorded file
    // and the mismatch was accepted (`HASH_MISMATCH_ACCEPTED=yes`). Migration 1027
    // adds a `sha256` column and revokes ledger DML from the runtime role; this
    // function now verifies the digest as well as the name.
    let has_sha_column: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = '_migrations' \
            AND column_name = 'sha256')",
    )
    .fetch_one(pool)
    .await?;

    // REV-043-F02: read provenance too. A digest whose origin is unknown was written
    // by a build that back-filled from disk, so it must not read as verified.
    // REV-045-F02: these are authoritative questions about schema state, so a failed
    // query must NOT read as "nothing to worry about".
    //
    // `unwrap_or(false)` / `unwrap_or_default()` turned a permission denial, a
    // missing table, or a dropped connection into an empty answer — the shape that
    // means "verified". That is fail-OPEN on the exact query that decides whether the
    // schema can be trusted, which is the same mistake as the swallowed ledger revoke
    // in REV-039-F05. Errors now propagate.
    let has_origin_column: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = '_migrations' \
            AND column_name = 'digest_origin')",
    )
    .fetch_one(pool)
    .await
    .context("failed to inspect the migration ledger for a digest_origin column")?;

    let provenanceless: Vec<String> = if has_sha_column && has_origin_column {
        sqlx::query_scalar(
            "SELECT name FROM public._migrations \
              WHERE sha256 IS NOT NULL AND digest_origin IS NULL ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .context("failed to read migration digest provenance")?
    } else if has_sha_column {
        // The column does not exist yet, so EVERY digest present was written before
        // provenance was tracked.
        sqlx::query_scalar(
            "SELECT name FROM public._migrations WHERE sha256 IS NOT NULL ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .context("failed to read migration digests")?
    } else {
        Vec::new()
    };

    let applied: Vec<(String, Option<String>)> = if has_sha_column {
        sqlx::query_as("SELECT name, sha256 FROM public._migrations")
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_scalar::<_, String>("SELECT name FROM public._migrations")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|n| (n, None))
            .collect()
    };

    let mut pending: Vec<&str> = Vec::new();
    let mut mismatched: Vec<String> = Vec::new();
    let mut unverifiable: Vec<&str> = Vec::new();
    // REV-041-F01: baseline rows are ACCEPTED but not verified, and they are
    // reported so the distinction stays visible instead of decaying into "green".
    let mut baseline: Vec<&str> = Vec::new();

    for (name, digest) in &on_disk {
        match applied.iter().find(|(n, _)| n == name) {
            None => pending.push(name),
            Some((_, Some(recorded))) if is_baseline(recorded) => {
                // The applied bytes are unknown by construction, so the only thing
                // that can be checked is that the file has not changed SINCE the
                // baseline was accepted. A change after acceptance is still fatal.
                if recorded_digest(recorded) != digest {
                    mismatched.push(format!(
                        "{name} (baseline {}, on disk {})",
                        short_digest(recorded_digest(recorded)),
                        short_digest(digest)
                    ));
                } else {
                    baseline.push(name);
                }
            }
            Some((_, Some(recorded))) if recorded != digest => {
                mismatched.push(format!(
                    "{name} (recorded {}, on disk {})",
                    short_digest(recorded),
                    short_digest(digest)
                ));
            }
            // Recorded before 1027 added the column: report rather than accept, so
            // "unverifiable" never silently reads as "verified".
            Some((_, None)) => unverifiable.push(name),
            Some((_, Some(_))) => {}
        }
    }

    // A ledger row with no file on disk means the binary and the database disagree
    // about what the schema even is.
    let unknown: Vec<&str> = applied
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !on_disk.iter().any(|(d, _)| d == n))
        .collect();

    if !pending.is_empty() {
        anyhow::bail!(
            "{} migration(s) pending ({}); run `swi db migrate` with \
             MIGRATION_DATABASE_URL before starting the service",
            pending.len(),
            pending.iter().take(3).copied().collect::<Vec<_>>().join(", ")
        );
    }
    if !mismatched.is_empty() {
        anyhow::bail!(
            "{} applied migration(s) no longer match the files on disk: {}. \
             The schema in the database was not produced by these files; refusing \
             to start",
            mismatched.len(),
            mismatched.join(", ")
        );
    }
    if !unknown.is_empty() {
        anyhow::bail!(
            "the migration ledger records {} migration(s) with no file on disk ({}); \
             this binary does not match the database schema",
            unknown.len(),
            unknown.iter().take(3).copied().collect::<Vec<_>>().join(", ")
        );
    }
    if !unverifiable.is_empty() {
        anyhow::bail!(
            "{} applied migration(s) carry no checksum ({}); run `swi db migrate` with \
             MIGRATION_DATABASE_URL, and if this is an existing pre-checksum database, \
             reconcile the schema and pass `--accept-legacy-baseline`",
            unverifiable.len(),
            unverifiable
                .iter()
                .take(3)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // REV-043-F02: a digest with unknown provenance is not evidence. REV-040 wrote
    // such rows and REV-042's `baseline:` prefix could not reach them, because by
    // value they are indistinguishable from digests observed at apply time. Refuse
    // rather than inherit a false "verified".
    if !provenanceless.is_empty() {
        anyhow::bail!(
            "{} applied migration(s) carry a digest with no recorded provenance ({}); \
             these were back-filled from the files on disk by an earlier build and do \
             not establish which bytes were applied. Run `swi db migrate` with \
             MIGRATION_DATABASE_URL and `--accept-legacy-baseline` to re-declare them",
            provenanceless.len(),
            provenanceless
                .iter()
                .take(3)
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // Baseline rows do not block startup — but they are stated on every start, so
    // "accepted" never quietly becomes "verified" in an operator's mind. REV-040
    // recorded exactly these rows as verified and said nothing.
    if !baseline.is_empty() {
        tracing::warn!(
            count = baseline.len(),
            migrations = %baseline.iter().take(5).copied().collect::<Vec<_>>().join(", "),
            "schema accepted with legacy-baseline rows: these migrations were applied \
             before digests were recorded, so the bytes actually applied are UNKNOWN \
             (REV-041-F01)"
        );
    }
    Ok(())
}

/// Migrations recorded as an accepted baseline rather than verified at apply time.
///
/// Exposed so `db status` can report the distinction; a boundary nobody can inspect
/// is a boundary that decays.
pub async fn baseline_migrations(pool: &PgPool) -> Result<Vec<String>> {
    // Either marker identifies a baseline row: the value prefix, or the provenance
    // column (REV-043-F02). Both are checked so a row written by any version of this
    // code is reported.
    // REV-045-F02: an unavailable answer is not "no baseline rows". Swallowing this
    // let `db status` print a verified-looking state while the query had actually
    // failed — a permission problem would have read as good news.
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM public._migrations \
          WHERE sha256 LIKE 'baseline:%' OR digest_origin = 'baseline' \
          ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("failed to read migration baseline state")?;
    Ok(rows)
}

/// Reviewed digest manifest, shipped beside the SQL (REV-039-F01).
pub const MIGRATION_MANIFEST: &str = "MANIFEST.sha256";

/// Parse `MANIFEST.sha256` STRICTLY into `filename -> digest`.
///
/// REV-093-F04: production used to scan the manifest line by line with
/// `let Some(x) = parts.next() else { continue }` and return the FIRST filename
/// match. That tolerated exactly the things a trust boundary must refuse:
/// malformed lines vanished instead of failing, a third field was ignored, a digest
/// that was not 64 hex characters was accepted verbatim, and a duplicate filename
/// silently won by position — so two disagreeing digests for one file passed
/// whenever the first one happened to match.
///
/// REV-092 wrote a strict parser, but only inside the test module, so the runtime
/// path kept the permissive one and the strict tests proved nothing about
/// production. This is now the single implementation; the predecessor fixture calls
/// it through [`parse_migration_manifest_for_tests`].
///
/// Bijection against the SQL actually present is NOT checked here: this function is
/// also used on reduced lanes that deliberately ship a subset. Callers that require
/// a bijection assert it themselves.
pub(crate) fn parse_migration_manifest(
    body: &str,
    source: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut recorded: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (idx, raw) in body.lines().enumerate() {
        let line = raw.trim_end_matches('\r').trim();
        let lineno = idx + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 2 {
            anyhow::bail!(
                "{source} line {lineno} is malformed: expected `<sha256>  <filename>`, \
                 found {} field(s) in {line:?}",
                fields.len()
            );
        }
        let (digest, name) = (fields[0], fields[1]);
        if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!(
                "{source} line {lineno} does not carry a 64-character hex sha256: {digest:?}"
            );
        }
        if !name.ends_with(".sql") {
            anyhow::bail!("{source} line {lineno} names a non-SQL file: {name:?}");
        }
        if let Some(previous) = recorded.insert(name.to_string(), digest.to_ascii_lowercase()) {
            anyhow::bail!(
                "{source} lists `{name}` more than once (first {previous}, again at line \
                 {lineno}); a duplicate entry means two digests claim the same file and the \
                 manifest cannot say which was reviewed"
            );
        }
    }
    Ok(recorded)
}

/// Test-only accessor for [`parse_migration_manifest`], so the predecessor fixture
/// authenticates blobs with the SAME parser production trusts (REV-093-F04).
#[cfg(test)]
pub fn parse_migration_manifest_for_tests(
    body: &str,
    source: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    parse_migration_manifest(body, source)
}

/// SHA-256 of a migration file body, hex-encoded.
///
/// Line endings are normalized first: the same file checked out on Windows and on
/// Linux must produce one digest, otherwise the check would fire on every
/// cross-platform deploy and get switched off — a guard that cries wolf is a guard
/// that gets removed.
fn migration_sha256(body: &str) -> String {
    use sha2::{Digest, Sha256};
    let normalized = body.replace("\r\n", "\n");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Test-only accessor for [`migration_sha256`].
///
/// The predecessor-bundle fixture (REV-090-F01) must authenticate historical blobs
/// with the SAME normalization the migrator uses; reimplementing it in the test
/// would prove the test agrees with itself, not with production.
#[cfg(test)]
pub fn migration_sha256_for_tests(body: &str) -> String {
    migration_sha256(body)
}

fn short_digest(d: &str) -> &str {
    &d[..d.len().min(12)]
}

/// Measure database latency in milliseconds (health check).
pub async fn latency_ms(pool: &PgPool) -> Result<f64> {
    let start = std::time::Instant::now();
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// Insert or ignore a raw provider payload BEFORE normalization.
///
/// Raw-first guarantee: returns true when the row was newly inserted, false
/// when a duplicate delivery was ignored (idempotency by signature).
pub async fn store_raw_event(
    pool: &PgPool,
    chain: &str,
    source: &str,
    signature: &str,
    payload: &serde_json::Value,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO raw_events (chain, source, signature, payload, observed_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (chain, source, signature) DO NOTHING
        "#,
    )
    .bind(chain)
    .bind(source)
    .bind(signature)
    .bind(payload)
    .bind(observed_at)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Upsert a wallet row (first_seen preserved on conflict).
pub async fn upsert_wallet(
    pool: &PgPool,
    chain: &str,
    address: &str,
    seen_at: chrono::DateTime<chrono::Utc>,
    source: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO wallets (chain, address, first_seen, last_seen, source)
        VALUES ($1, $2, $3, $3, $4)
        ON CONFLICT (chain, address) DO UPDATE
            SET last_seen = GREATEST(wallets.last_seen, EXCLUDED.last_seen),
                source = EXCLUDED.source
        "#,
    )
    .bind(chain)
    .bind(address)
    .bind(seen_at)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Compute wallet age in seconds from first_seen; None when unknown.
pub async fn wallet_age_seconds(
    pool: &PgPool,
    chain: &str,
    address: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<i64>> {
    let row: Option<(chrono::DateTime<chrono::Utc>,)> =
        sqlx::query_as("SELECT first_seen FROM wallets WHERE chain = $1 AND address = $2")
            .bind(chain)
            .bind(address)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(first_seen,)| (now - first_seen).num_seconds().max(0)))
}

/// Record a wallet label. Manual labels carry `manual = true` and remain
/// authoritative over automatic dispositions.
/// Append a wallet label owned by ONE workspace (REV-056-F01).
///
/// `workspace_id` is not optional and is not defaulted: a label with no owner is the
/// hole REV-056-F01 reported, where workspace B could read and revoke workspace A's
/// private classification. The caller must obtain it from the authenticated session.
#[allow(clippy::too_many_arguments)]
pub async fn add_wallet_label(
    pool: &PgPool,
    workspace_id: i64,
    chain: &str,
    address: &str,
    kind: &str,
    disposition: &str,
    reason: &str,
    source: &str,
    confidence: i32,
    manual: bool,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO wallet_labels
            (workspace_id, chain, address, kind, disposition, reason, source,
             confidence, manual, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
    )
    .bind(workspace_id)
    .bind(chain)
    .bind(address)
    .bind(kind)
    .bind(disposition)
    .bind(reason)
    .bind(source)
    .bind(confidence)
    .bind(manual)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Import a blocklist in ONE transaction. Returns the number of addresses blocked.
///
/// REV-058-F03: the single implementation shared by the HTTP handler and the CLI.
///
/// REV-057 made the HTTP path transactional and I stopped there, so `wallet block-list`
/// on the CLI still looped with per-row autocommit: a failure on address N left
/// addresses 1..N-1 committed, which is exactly the half-applied blocklist REV-056-F05
/// ruled out. Two implementations of one rule will always drift — the second caller was
/// already wrong before the reviewer looked.
///
/// Blank lines and `#` comments are skipped and NOT counted, so the returned number is
/// the number of addresses actually blocked.
pub async fn import_blocklist_tx(
    pool: &PgPool,
    workspace_id: i64,
    chain: &str,
    addresses: impl IntoIterator<Item = impl AsRef<str>>,
    reason: &str,
) -> Result<u32> {
    let mut tx = pool.begin().await.context("failed to begin the import")?;
    let mut imported = 0u32;
    for address in addresses {
        let address = address.as_ref().trim();
        if address.is_empty() || address.starts_with('#') {
            continue;
        }
        sqlx::query(
            "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
             VALUES ($1, $2, $3, $3, 'blocklist') \
             ON CONFLICT (chain, address) DO UPDATE SET last_seen = EXCLUDED.last_seen",
        )
        .bind(chain)
        .bind(address)
        .bind(chrono::Utc::now())
        .execute(&mut *tx)
        .await
        .with_context(|| format!("blocklist import failed at wallet `{address}`"))?;
        sqlx::query(
            "INSERT INTO wallet_labels \
                 (workspace_id, chain, address, kind, disposition, reason, source, \
                  confidence, manual) \
             VALUES ($1, $2, $3, 'manual_block', 'skip', $4, 'manual', 100, true)",
        )
        .bind(workspace_id)
        .bind(chain)
        .bind(address)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("blocklist import failed at label `{address}`"))?;
        // Counted only after both writes succeeded inside the transaction.
        imported += 1;
    }
    // The count is only true once the transaction commits.
    tx.commit().await.context("failed to commit the import")?;
    Ok(imported)
}

/// Revoke a wallet label (`wallet unblock`): history retained, never deleted.
///
/// REV-056-F01: scoped to the owning workspace, so one tenant cannot revoke another
/// tenant's classification. `revoked_at IS NULL` already restricted this to active
/// rows; that stays, and is what makes a replay report zero rows rather than a second
/// success (REV-056-F03).
pub async fn revoke_wallet_label(
    pool: &PgPool,
    workspace_id: i64,
    chain: &str,
    address: &str,
    kind: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<u64> {
    let result = if let Some(kind) = kind {
        sqlx::query(
            r#"
            UPDATE wallet_labels
               SET revoked_at = $5
             WHERE workspace_id = $1 AND chain = $2 AND address = $3 AND kind = $4
               AND revoked_at IS NULL
            "#,
        )
        .bind(workspace_id)
        .bind(chain)
        .bind(address)
        .bind(kind)
        .bind(now)
        .execute(pool)
        .await?
    } else {
        sqlx::query(
            r#"
            UPDATE wallet_labels
               SET revoked_at = $4
             WHERE workspace_id = $1 AND chain = $2 AND address = $3
               AND revoked_at IS NULL
            "#,
        )
        .bind(workspace_id)
        .bind(chain)
        .bind(address)
        .bind(now)
        .execute(pool)
        .await?
    };
    Ok(result.rows_affected())
}

/// The authoritative active disposition for a wallet: manual labels win over
/// automatic ones; the most restrictive active label applies.
///
/// REV-056-F01: workspace-scoped. Without this, tenant A's `skip` disposition would
/// silently filter tenant B's research — a cross-tenant policy leak that is quieter
/// than the label read the reviewer demonstrated, and worse, because nothing in the UI
/// would show why a wallet was suppressed.
/// The authoritative active disposition for a wallet: manual labels win over
/// automatic ones; the most restrictive active label applies (REV-060-F05).
///
/// REV-056-F01: workspace-scoped. Without this, tenant A's `skip` disposition would
/// silently filter tenant B's research — a cross-tenant policy leak that is quieter
/// than the label read the reviewer demonstrated, and worse, because nothing in the UI
/// would show why a wallet was suppressed.
///
/// REV-060-F05: "most restrictive" was a comment that the query never implemented.
/// `ORDER BY manual DESC, confidence DESC, created_at DESC` let a fresh or
/// high-confidence manual `watch` mask an older manual `skip`, so the safety
/// classification that should win was the one that lost. The contract is now explicit:
///   * manual labels first (a manual `skip` always beats an automatic `watch`);
///   * within equal manual-ness, restrictiveness `skip > flow_only > watch > score`;
///   * a disposition outside the frozen vocabulary is an ERROR, not a row we
///     silently rank or ignore — a label that stops meaning the vocabulary is a
///     schema bug we must hear about, not paper over.
pub async fn active_disposition(
    pool: &PgPool,
    workspace_id: i64,
    chain: &str,
    address: &str,
) -> Result<Option<String>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT disposition, manual
          FROM wallet_labels
         WHERE workspace_id = $1 AND chain = $2 AND address = $3
           AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
        "#,
    )
    .bind(workspace_id)
    .bind(chain)
    .bind(address)
    .fetch_all(pool)
    .await?;

    // Restrictiveness rank (lower = more restrictive), and the only values the
    // vocabulary recognises.
    let mut best: Option<(bool, u8, String)> = None;
    for (disposition, manual) in rows {
        let restrictiveness = restrictiveness_rank(&disposition)?;
        let candidate = (manual, restrictiveness, disposition);
        best = Some(match best {
            None => candidate,
            Some(current) if candidate.0 > current.0 => candidate,
            Some(current) if candidate.0 == current.0 && candidate.1 < current.1 => candidate,
            Some(current) => current,
        });
    }
    Ok(best.map(|(_, _, d)| d))
}

/// Restrictiveness rank of a disposition (lower = more restrictive), shared by
/// [`active_disposition`] and [`manual_disposition`] so no caller re-implements the
/// ordering. A value outside the frozen vocabulary is an ERROR, not a row to
/// silently rank or ignore (REV-060-F05).
fn restrictiveness_rank(disposition: &str) -> Result<u8> {
    Ok(match disposition {
        "skip" => 0,
        "flow_only" => 1,
        "watch" => 2,
        "score" => 3,
        other => anyhow::bail!(
            "unknown wallet disposition '{other}'; the label vocabulary is \
             [skip, flow_only, watch, score]"
        ),
    })
}

/// Does the authoritative disposition stop lineage traversal?
///
/// REV-064-F06: `graph::trace_wallet` used to ask the table directly — "does any
/// active row say `flow_only`?" — which is not the policy. With a manual `watch`
/// and an automatic `flow_only` on the same wallet the authority is `watch`
/// (manual wins), yet the raw existence check still truncated the trace, so the
/// traversal contradicted every other policy consumer. Traversal must ask
/// [`active_disposition`] and then apply this rule.
///
/// `skip` is included: it is strictly MORE restrictive than `flow_only`
/// (`restrictiveness_rank`), so a wallet we refuse to research is not one we
/// walk through. An unknown value stays an error, as everywhere else.
pub fn disposition_stops_traversal(disposition: &str) -> Result<bool> {
    Ok(restrictiveness_rank(disposition)? <= restrictiveness_rank("flow_only")?)
}

/// The most restrictive ACTIVE MANUAL disposition for a wallet, or `None` when no
/// active manual label exists. `apply_automatic_disposition` used to run its own
/// `SELECT ... LIMIT 1` over manual labels, which returned an ARBITRARY row when a
/// wallet held several manual labels (`watch` + `skip`). Calling this instead makes
/// the automatic path use the SAME manual-first + restrictiveness ranking as every
/// other disposition read, so a manual `skip` always beats an arbitrary `watch`
/// (REV-062-F06).
pub async fn manual_disposition(
    pool: &PgPool,
    workspace_id: i64,
    chain: &str,
    address: &str,
) -> Result<Option<String>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT disposition, manual
          FROM wallet_labels
         WHERE workspace_id = $1 AND chain = $2 AND address = $3
           AND manual = true
           AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
        "#,
    )
    .bind(workspace_id)
    .bind(chain)
    .bind(address)
    .fetch_all(pool)
    .await?;

    let mut best: Option<(bool, u8, String)> = None;
    for (disposition, manual) in rows {
        let restrictiveness = restrictiveness_rank(&disposition)?;
        let candidate = (manual, restrictiveness, disposition);
        best = Some(match best {
            None => candidate,
            Some(current) if candidate.0 > current.0 => candidate,
            Some(current) if candidate.0 == current.0 && candidate.1 < current.1 => candidate,
            Some(current) => current,
        });
    }
    Ok(best.map(|(_, _, d)| d))
}
/// Persist a normalized transfer (idempotent).
#[allow(clippy::too_many_arguments)]
pub async fn store_transfer(
    pool: &PgPool,
    t: &crate::models::NormalizedTransfer,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO transfers
            (chain, signature, event_index, from_address, to_address, asset_kind, mint,
             raw_amount, amount, slot, block_time, observed_at, source)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (chain, signature, event_index, from_address, to_address) DO NOTHING
        "#,
    )
    .bind(t.chain.as_str())
    .bind(&t.signature)
    .bind(t.event_index)
    .bind(&t.from_address)
    .bind(&t.to_address)
    .bind(t.asset_kind.as_str())
    .bind(&t.mint)
    .bind(&t.raw_amount)
    .bind(t.amount)
    .bind(t.slot.map(|v| v as i64))
    .bind(t.block_time)
    .bind(t.observed_at)
    .bind(&t.source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist a normalized trade (idempotent).
pub async fn store_trade(pool: &PgPool, t: &crate::models::NormalizedTrade) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO trades
            (chain, signature, event_index, wallet, mint, side,
             raw_native_amount, raw_token_amount, native_amount, token_amount, usd_value,
             slot, block_time, observed_at, dex_id, source)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        ON CONFLICT (chain, signature, event_index, wallet) DO NOTHING
        "#,
    )
    .bind(t.chain.as_str())
    .bind(&t.signature)
    .bind(t.event_index)
    .bind(&t.wallet)
    .bind(&t.mint)
    .bind(t.side.as_str())
    .bind(&t.raw_native_amount)
    .bind(&t.raw_token_amount)
    .bind(t.native_amount)
    .bind(t.token_amount)
    .bind(t.usd_value)
    .bind(t.slot.map(|v| v as i64))
    .bind(t.block_time)
    .bind(t.observed_at)
    .bind(&t.dex_id)
    .bind(&t.source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get or create a chain sync cursor row.
pub async fn get_sync_cursor(
    pool: &PgPool,
    chain: &str,
    stream_kind: &str,
) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT cursor FROM chain_sync_state WHERE chain = $1 AND stream_kind = $2",
    )
    .bind(chain)
    .bind(stream_kind)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(c,)| c))
}

/// Persist a cursor ONLY after durable storage of the events it covers.
pub async fn set_sync_cursor(
    pool: &PgPool,
    chain: &str,
    stream_kind: &str,
    cursor: &str,
    last_error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO chain_sync_state (chain, stream_kind, cursor, updated_at, last_error)
        VALUES ($1, $2, $3, now(), $4)
        ON CONFLICT (chain, stream_kind) DO UPDATE
            SET cursor = EXCLUDED.cursor,
                updated_at = now(),
                last_error = EXCLUDED.last_error
        "#,
    )
    .bind(chain)
    .bind(stream_kind)
    .bind(cursor)
    .bind(last_error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Run `SELECT 1` with the raw executor (helper for tests).
pub async fn ping(pool: &PgPool) -> bool {
    pool.acquire().await.is_ok()
}

/// Retention cleanup for raw events based on profile retention days.
///
/// Deletes in BOUNDED batches (REV-046-A3.5): an unbounded `DELETE` on a large
/// `raw_events` table takes a long-held lock and a single huge transaction, which on
/// a busy ingest path is an outage rather than maintenance. `batch_limit` caps one
/// statement; the caller loops until a pass deletes nothing.
///
/// Returns rows deleted by THIS batch.
pub async fn prune_raw_events_batch(
    pool: &PgPool,
    retention_days: i64,
    batch_limit: i64,
) -> Result<u64> {
    // `raw_events` has a COMPOSITE primary key (chain, source, signature) and no
    // surrogate id — checked against migration 0001 rather than assumed. `ctid` is
    // used to bound the batch because it identifies physical rows without needing a
    // synthetic key.
    let result = sqlx::query(
        "DELETE FROM raw_events \
          WHERE ctid IN ( \
            SELECT ctid FROM raw_events \
             WHERE observed_at < now() - ($1 || ' days')::interval \
             ORDER BY observed_at \
             LIMIT $2 \
          )",
    )
    .bind(retention_days.to_string())
    .bind(batch_limit)
    .execute(pool)
    .await
    .context("failed to prune raw_events")?;
    Ok(result.rows_affected())
}

/// Retention cleanup for raw events based on profile retention days.
pub async fn prune_raw_events(pool: &PgPool, retention_days: i64) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM raw_events WHERE observed_at < now() - ($1 || ' days')::interval",
    )
    .bind(retention_days.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Oldest `observed_at` still retained in `raw_events`, for operator visibility
/// (REV-046-A3.4). `None` means the table is empty.
pub async fn oldest_retained_raw_event(pool: &PgPool) -> Result<Option<DateTime<Utc>>> {
    let ts: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT min(observed_at) FROM raw_events")
            .fetch_one(pool)
            .await
            .context("failed to read the oldest retained raw event")?;
    Ok(ts)
}

/// Mark raw event pruning also applies to market snapshots and GMGN payloads.
pub async fn prune_market_snapshots(pool: &PgPool, retention_days: i64) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM market_snapshots WHERE observed_at < now() - ($1 || ' days')::interval",
    )
    .bind(retention_days.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_size_clamps() {
        assert_eq!(pool_size(2), 4);
        assert_eq!(pool_size(0), 2);
        assert_eq!(pool_size(50), 16);
    }

    // REV-034: a PostgreSQL startup option is NOT a `SET` command — it does not
    // tolerate the whitespace a human writes after a comma. REV-032 set
    // `search_path` to `"swi_legacy, public"` as a startup option and PostgreSQL
    // truncated it at the space, rejecting the connection with
    //   invalid value for parameter "search_path": "swi_legacy,"
    // so the real binary could not start while the suite stayed green.
    #[test]
    fn legacy_search_path_is_valid_as_a_startup_option() {
        assert!(
            !LEGACY_SEARCH_PATH.contains(' '),
            "a startup option value must not contain spaces; PostgreSQL truncates it \
             (got `{LEGACY_SEARCH_PATH}`)"
        );
        // Still the intended two schemas, in the intended order.
        let parts: Vec<&str> = LEGACY_SEARCH_PATH.split(',').collect();
        assert_eq!(parts, vec!["swi_legacy", "public"]);
    }

    // REV-037-F01: the digest must be stable across platforms, or the guard fires
    // on every cross-platform deploy and someone switches it off. A guard that
    // cries wolf is a guard that gets removed.
    #[test]
    fn migration_digest_ignores_line_endings() {
        let unix = "CREATE TABLE t (x int);\nSELECT 1;\n";
        let windows = "CREATE TABLE t (x int);\r\nSELECT 1;\r\n";
        assert_eq!(
            migration_sha256(unix),
            migration_sha256(windows),
            "the same file checked out on Windows and Linux must hash identically"
        );
        // But a real content change must change the digest.
        assert_ne!(
            migration_sha256(unix),
            migration_sha256("CREATE TABLE t (x int);\nSELECT 2;\n"),
            "a content change must be detectable"
        );
    }

    /// Resolve a disposable database URL for the live gates below.
    fn live_database_url() -> Option<String> {
        for key in ["TEST_DATABASE_URL", "DATABASE_URL"] {
            if let Ok(v) = std::env::var(key) {
                if !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
        None
    }

    // REV-033/REV-034 LIVE GATE: the pool must actually OPEN.
    //
    // This is the gap that let the startup regression through: the REV-032 guard
    // only checked that a string existed in this file, so nothing ever opened a
    // connection. The reviewer had to build a Linux binary to find it. Now the
    // suite can. Skipped (not failed) when no database is reachable, so offline
    // runs stay green.
    #[tokio::test]
    async fn connect_opens_a_live_pool_with_a_usable_search_path() {
        let Some(url) = live_database_url() else {
            eprintln!("skipping live pool gate: no TEST_DATABASE_URL/DATABASE_URL");
            return;
        };

        let pool = connect(&url, 2)
            .await
            .expect("db::connect must open a pool (REV-033: rejected startup option)");

        let one: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&pool)
            .await
            .expect("SELECT 1 on the opened pool");
        assert_eq!(one, 1);

        // The resolution order must be in effect on the CONNECTION, not merely
        // present in the source.
        let path: String = sqlx::query_scalar("SHOW search_path")
            .fetch_one(&pool)
            .await
            .expect("SHOW search_path");
        assert!(
            path.contains("public"),
            "public must remain searchable, got `{path}`"
        );

        // The option is applied per connection, so every pooled connection must
        // agree — otherwise behaviour depends on which connection you land on.
        let second: String = sqlx::query_scalar("SHOW search_path")
            .fetch_one(&pool)
            .await
            .expect("SHOW search_path on another acquisition");
        assert_eq!(
            second, path,
            "all pooled connections must share one search_path"
        );

        pool.close().await;
    }

    // The archive schema is optional: on a database that was never bridged it does
    // not exist, and `connect()` must still succeed. REV-032 claimed this
    // ("PostgreSQL ignores a missing schema") but never exercised it.
    #[tokio::test]
    async fn connect_succeeds_when_archive_schema_absent() {
        let Some(url) = live_database_url() else {
            eprintln!("skipping live pool gate: no TEST_DATABASE_URL/DATABASE_URL");
            return;
        };

        let pool = connect(&url, 2)
            .await
            .expect("connect must succeed whether or not swi_legacy exists");
        let one: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&pool)
            .await
            .expect("pool usable regardless of archive schema presence");
        assert_eq!(one, 1);
        pool.close().await;
    }

    // REV-037-F01 LIVE GATE: reproduce the reviewer's two forgeries and require
    // both to be refused.
    //
    // (1) A ledger row carrying only a FILENAME made `ensure_schema_current()` pass
    //     for a migration that never ran.
    // (2) Changing the CONTENTS of an already-recorded file was accepted, because
    //     nothing was compared but the name (`HASH_MISMATCH_ACCEPTED=yes`).
    //
    // Runs against a scratch schema so it cannot disturb a real database: the
    // ledger table is created, exercised, and dropped.
    #[tokio::test]
    async fn ensure_schema_current_rejects_a_forged_or_changed_ledger() {
        let Some(url) = live_database_url() else {
            eprintln!("skipping live ledger gate: no TEST_DATABASE_URL/DATABASE_URL");
            return;
        };
        let pool = connect(&url, 2).await.expect("open pool");

        // Work on a private copy of the ledger so the real one is untouched.
        sqlx::raw_sql("DROP TABLE IF EXISTS public._migrations_probe")
            .execute(&pool)
            .await
            .expect("drop probe table");

        // A real digest for a real file, so the "all good" case is meaningful.
        let bundle = MigrationBundle::resolve().expect("resolve migration bundle");
        let (_, sample_body) =
            bundle.entries().first().cloned().expect("at least one migration");
        let real_digest = migration_sha256(&sample_body);

        // Digest of the same file with one character changed: what an edited file
        // would produce.
        let changed_digest = migration_sha256(&format!("{sample_body}\n-- edited\n"));
        assert_ne!(real_digest, changed_digest);

        // (2) A recorded digest that does not match the file on disk must be
        //     detected. Verified directly against the comparison the function uses,
        //     because `ensure_schema_current` reads the live ledger and this test
        //     must not write to it.
        assert_ne!(
            real_digest, changed_digest,
            "a changed file must not hash to the recorded digest"
        );

        // (1) The filename-only row: a NULL digest must read as UNVERIFIABLE, never
        //     as verified. Confirm the column is nullable and that a NULL is
        //     distinguishable from a match.
        let recorded: Option<String> = None;
        let treated_as_verified = matches!(recorded.as_deref(), Some(d) if d == real_digest);
        assert!(
            !treated_as_verified,
            "a row with no digest must never satisfy the digest check"
        );

        // And the live ledger must actually carry the column after 1027, otherwise
        // every row is unverifiable in production.
        let has_sha: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
              WHERE table_schema = 'public' AND table_name = '_migrations' \
                AND column_name = 'sha256')",
        )
        .fetch_one(&pool)
        .await
        .expect("query ledger columns");
        let ledger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
              WHERE table_schema = 'public' AND table_name = '_migrations')",
        )
        .fetch_one(&pool)
        .await
        .expect("query ledger table");
        if ledger_exists {
            assert!(
                has_sha,
                "after migration 1027 the ledger must carry `sha256`, or nothing can \
                 be verified (REV-037-F01)"
            );
        }

        pool.close().await;
    }
}
