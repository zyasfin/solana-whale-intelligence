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

use chrono::Utc;
use sqlx::PgPool;

/// The CANONICAL migration bundle: the one versioned inside this crate.
///
/// REV-092 item 4: this returned `<crate>/../swi-deploy/migrations`, the legacy
/// sibling checkout. Since commit a60647d the packaged `migrations/` directory is
/// the canonical artifact and the sibling is an unversioned leftover that can drift
/// from it silently — so every assertion in this module was being made against
/// bytes the production resolver would not choose. `CARGO_MANIFEST_DIR/migrations`
/// is the same directory `resolve_migration_dir` picks for a normal run.
fn migrations_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations")
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
/// The commit that first versioned the migration bundle inside this crate. Its
/// tree is a CONTENT-ADDRESSED artifact: git object ids are sha1 over the stored
/// bytes, so `git cat-file` at a fixed commit reproduces the exact bytes that
/// commit recorded — something a copy of the working tree can never establish.
const PREDECESSOR_COMMIT: &str = "a60647dac892afad18b03671bea0638b695c0cdf";

/// Materialize the migration bundle AS OF `PREDECESSOR_COMMIT`, straight out of
/// the object database.
///
/// REV-090-F01 item 3: `scratch_before` copies the CURRENT working tree, so the
/// "historical" lane it builds is today's bytes by construction and proves nothing
/// about provenance. This reads a named commit's blobs instead and verifies each
/// one against the digest recorded in that same commit's manifest, so a tampered
/// object store fails the fixture rather than silently passing.
///
/// Returns `None` when the commit is unreachable (a shallow or exported checkout),
/// so the caller can report the gap honestly instead of fabricating a pass.
fn predecessor_bundle(dest: &std::path::Path) -> Option<usize> {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let listing = std::process::Command::new("git")
        .args(["-C", root.to_str()?, "ls-tree", "--name-only",
               &format!("{PREDECESSOR_COMMIT}:migrations")])
        .output()
        .ok()?;
    if !listing.status.success() {
        return None;
    }
    let names: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    std::fs::create_dir_all(dest).expect("mkdir predecessor bundle");
    let mut written = 0usize;
    for name in &names {
        let blob = std::process::Command::new("git")
            .args(["-C", root.to_str()?, "cat-file", "blob",
                   &format!("{PREDECESSOR_COMMIT}:migrations/{name}")])
            .output()
            .ok()?;
        assert!(blob.status.success(), "cat-file failed for {name}");
        std::fs::write(dest.join(name), &blob.stdout).expect("write predecessor blob");
        if name.ends_with(".sql") {
            written += 1;
        }
    }
    // Self-authentication: every SQL blob must match the digest recorded in the
    // manifest of the SAME commit, and the manifest must be a strict bijection with
    // the SQL actually present (REV-092 item 3).
    let manifest =
        std::fs::read_to_string(dest.join("MANIFEST.sha256")).expect("predecessor manifest");
    let sql_names: Vec<String> = names
        .iter()
        .filter(|n| n.ends_with(".sql"))
        .cloned()
        .collect();
    verify_manifest_bijection(&manifest, &sql_names, dest);
    Some(written)
}

/// Parse a `MANIFEST.sha256` STRICTLY and prove it is an exact bijection with
/// `sql_names`, then verify every recorded digest against the bytes on disk.
///
/// REV-092 item 3: the previous parser used `filter_map`, so a malformed line was
/// silently dropped rather than rejected — a manifest could lose an entry to a typo
/// and still "verify". A duplicate filename silently overwrote the earlier one, so
/// two disagreeing digests for the same file passed as long as the last one matched.
/// Extra entries naming files that do not exist were never noticed at all. None of
/// those is a manifest a provenance claim can rest on.
///
/// Panics with a specific message per failure class, so a broken fixture says which
/// invariant broke instead of just "mismatch".
fn verify_manifest_bijection(manifest: &str, sql_names: &[String], dir: &std::path::Path) {
    use std::collections::BTreeMap;

    let mut recorded: BTreeMap<String, String> = BTreeMap::new();
    for (idx, raw) in manifest.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let lineno = idx + 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // Exactly two whitespace-separated fields: <64 hex digits> <filename>.
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(
            fields.len(),
            2,
            "manifest line {lineno} is malformed (expected `<sha256>  <filename>`, \
             got {} field(s)): {line:?}",
            fields.len()
        );
        let (digest, name) = (fields[0], fields[1]);
        assert!(
            digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
            "manifest line {lineno} does not carry a 64-hex sha256: {digest:?}"
        );
        assert!(
            name.ends_with(".sql"),
            "manifest line {lineno} names a non-SQL file: {name:?}"
        );
        if let Some(previous) = recorded.insert(name.to_string(), digest.to_ascii_lowercase()) {
            panic!(
                "manifest lists `{name}` more than once (first {previous}, again at \
                 line {lineno}); a duplicate entry means two digests claim the same file"
            );
        }
    }

    // Exact bijection, reported in both directions so the failure names itself.
    let present: std::collections::BTreeSet<&str> =
        sql_names.iter().map(|s| s.as_str()).collect();
    let listed: std::collections::BTreeSet<&str> =
        recorded.keys().map(|s| s.as_str()).collect();
    let missing: Vec<&&str> = present.difference(&listed).collect();
    let extra: Vec<&&str> = listed.difference(&present).collect();
    assert!(
        missing.is_empty(),
        "manifest has no entry for {} SQL file(s): {missing:?}"
    , missing.len());
    assert!(
        extra.is_empty(),
        "manifest lists {} entr(y/ies) with no SQL file present: {extra:?}"
    , extra.len());
    assert_eq!(
        recorded.len(),
        sql_names.len(),
        "manifest entry count must equal the SQL file count exactly"
    );

    for (name, want) in &recorded {
        let bytes = std::fs::read(dir.join(name)).expect("read migration for digest check");
        let got = crate::db::migration_sha256_for_tests(&String::from_utf8_lossy(&bytes));
        assert_eq!(
            &got, want,
            "blob {name} does not match the digest its own manifest records"
        );
    }
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

// ---------------------------------------------------------------------------
// REV-090-F01 — exact positive-int8 validation, independent subject check,
// and an AUTHENTICATED predecessor replay
// ---------------------------------------------------------------------------

/// A valid 19-digit workspace id must keep its owner.
///
/// The REV-087 guard was `^[0-9]{1,18}$`: sufficient, not exact. Every valid int8
/// from 1000000000000000000 to 9223372036854775807 has 19 digits and was therefore
/// classified unsafe, so the preflight reassigned a REAL owner to the default
/// workspace — tenancy corruption, strictly worse than the abort it avoided.
#[tokio::test]
async fn a_valid_nineteen_digit_workspace_segment_keeps_its_owner() {
    let (pool, admin, scratch, dir) = scratch_before("ws19", "1036_").await;

    // A workspace whose id is literally the 19-digit boundary value.
    const WS19: i64 = 1_000_000_000_000_000_000;
    sqlx::query("INSERT INTO workspaces (id, name, slug) OVERRIDING SYSTEM VALUE VALUES ($1, 'w', $2)")
        .bind(WS19)
        .bind(tag("r90ws19"))
        .execute(&pool)
        .await
        .expect("19-digit workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R90WS19MINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(WS19)
    .fetch_one(&pool)
    .await
    .expect("signal");
    let key = format!("signal:{WS19}:{sid}:chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, 'chat-a', 'pending', 1, now())",
    )
    .bind(&key)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed 19-digit workspace alert");

    // Precondition: the value is genuinely 19 digits AND genuinely a valid int8,
    // so the assertion below cannot pass for the wrong reason.
    let (digits, castable): (i32, bool) = sqlx::query_as(
        "SELECT length(split_part(dedup_key, ':', 2))::int, \
                (split_part(dedup_key, ':', 2)::numeric <= 9223372036854775807) \
           FROM public.alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("segment shape");
    assert_eq!(digits, 19, "the probe value must be 19 digits");
    assert!(castable, "the probe value must be a valid positive int8");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a valid 19-digit workspace must not abort the lane");

    let owner: i64 = sqlx::query_scalar("SELECT workspace_id FROM public.alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("read back owner");
    let default_ws: Option<i64> = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'default'")
        .fetch_optional(&pool)
        .await
        .expect("default workspace probe");
    assert_eq!(
        owner, WS19,
        "a valid 19-digit workspace must keep its owner (REV-090-F01)"
    );
    assert!(
        default_ws != Some(owner),
        "the row must NOT have been reassigned to the default workspace"
    );

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// A corrupt funding SUBJECT segment must be refused by the NAMED preflight error
/// even when the workspace segment is perfectly sound, and must leave the database
/// byte-for-byte unchanged.
///
/// REV-090-F01 item 2: the subject check used to sit AFTER `if !needs_repair`
/// returned. With a sound workspace segment, `needs_repair` is false, the function
/// returned early, and the row sailed through to 1036's raw cast.
#[tokio::test]
async fn a_corrupt_funding_subject_segment_is_refused_before_any_ddl() {
    let (pool, admin, scratch, dir) = scratch_before("subject", "1036_").await;

    let ws: i64 = sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id")
        .bind(tag("r90subj"))
        .fetch_one(&pool)
        .await
        .expect("workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R90SUBJMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");
    // VALID workspace segment, OUT-OF-RANGE subject segment.
    let key = format!("funding:{ws}:9223372036854775808:chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, 'chat-a', 'pending', 1, now())",
    )
    .bind(&key)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed corrupt-subject alert");

    // Precondition: the workspace segment IS sound, so `needs_repair` is false and
    // the old control flow would have returned before ever looking at the subject.
    let ws_sound: bool = sqlx::query_scalar(
        "SELECT split_part(dedup_key, ':', 2) ~ '^[0-9]{1,18}$' FROM public.alerts \
          WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("workspace segment shape");
    assert!(ws_sound, "the workspace segment must be sound, or this tests the wrong path");

    let before: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'alerts' ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .expect("schema snapshot");

    let err = crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect_err("a corrupt funding subject segment must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("carry a subject segment that is not a"),
        "expected the NAMED preflight refusal, got: {msg}"
    );
    assert!(
        !msg.contains("22003") && !msg.contains("out of range"),
        "the refusal must come from the preflight, not from 1036's raw cast: {msg}"
    );

    // Rollback intact: no partial cutover, and 1036 never ran.
    let after: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'alerts' ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .expect("schema snapshot after");
    assert_eq!(before, after, "the refused preflight must leave the schema unchanged");
    let applied_1036: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM public._migrations WHERE name LIKE '1036%')",
    )
    .fetch_one(&pool)
    .await
    .expect("1036 probe");
    assert!(!applied_1036, "1036 must not have been recorded as applied");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.alerts")
        .fetch_one(&pool)
        .await
        .expect("row count");
    assert_eq!(rows, 1, "the seeded row must survive untouched");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// An AUTHENTICATED predecessor bundle, replayed from the git object database,
/// then brought forward to current and re-run for convergence.
///
/// REV-090-F01 item 3: `scratch_before` copies the CURRENT working tree, so its
/// "historical" lane is today's bytes by construction. This reads the bundle out of
/// a named commit and verifies every blob against the digest recorded in THAT
/// commit's manifest, so the provenance is content-addressed rather than asserted.
#[tokio::test]
async fn an_authenticated_predecessor_bundle_converges_to_current() {
    let pred_dir = std::env::temp_dir().join(format!("swi_r90_pred_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&pred_dir);
    let Some(sql_count) = predecessor_bundle(&pred_dir) else {
        panic!(
            "the predecessor commit {PREDECESSOR_COMMIT} is unreachable from this checkout; \
             provenance cannot be authenticated here. Fetch full history rather than \
             substituting a copy of the current tree"
        );
    };
    assert!(sql_count > 0, "the predecessor bundle must contain SQL");

    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&crate::pg_test_support::require_live_url())
        .await
        .expect("admin pool");
    let scratch = format!("swi_r90_pred_{}", std::process::id());
    sqlx::query(&format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop scratch");
    sqlx::query(&format!("CREATE DATABASE {scratch}"))
        .execute(&admin)
        .await
        .expect("create scratch");
    let url = format!(
        "{}/{}",
        crate::pg_test_support::require_live_url()
            .rsplitn(2, '/')
            .nth(1)
            .expect("db url base"),
        scratch
    );
    let pool = crate::db::connect(&url, 2).await.expect("scratch pool");

    // Phase 1: the authenticated historical bytes.
    crate::db::migrate_dir_with(&pool, &pred_dir, true)
        .await
        .expect("the predecessor bundle must apply");
    let after_pred: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool)
        .await
        .expect("predecessor ledger");
    assert_eq!(
        after_pred as usize, sql_count,
        "every predecessor migration must be recorded"
    );

    // Phase 2: forward to current. Any digest disagreement between the historical
    // bytes and today's files would surface here as a drift refusal.
    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("the authenticated predecessor lane must upgrade to current");

    // REV-092 item 4: snapshot the FULL ledger and a semantic slice of the schema
    // BEFORE the second pass. The old test only counted rows afterwards, so a
    // second pass that rewrote a digest, re-stamped `applied_at`, or re-ran a
    // migration's DDL would still have satisfied it. "Converges" has to mean the
    // second pass changed nothing, not that the totals still look plausible.
    let ledger_before: Vec<(String, Option<String>, Option<String>, chrono::DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT name, sha256, digest_origin, applied_at \
               FROM public._migrations ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .expect("ledger snapshot before the second pass");
    let schema_before: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text, data_type::text \
           FROM information_schema.columns WHERE table_schema = 'public' \
          ORDER BY table_name, column_name",
    )
    .fetch_all(&pool)
    .await
    .expect("schema snapshot before the second pass");
    let constraints_before: Vec<(String, String)> = sqlx::query_as(
        "SELECT conname::text, contype::text FROM pg_constraint c \
           JOIN pg_namespace n ON n.oid = c.connamespace \
          WHERE n.nspname = 'public' ORDER BY conname",
    )
    .fetch_all(&pool)
    .await
    .expect("constraint snapshot before the second pass");
    let cutovers_before: Vec<(String,)> = sqlx::query_as(
        "SELECT cutover::text FROM schema_cutover_events ORDER BY cutover, id",
    )
    .fetch_all(&pool)
    .await
    .expect("cutover snapshot before the second pass");

    // Phase 3: convergence — a second pass must be an EXACT no-op.
    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a second pass over current must converge");

    let ledger_after: Vec<(String, Option<String>, Option<String>, chrono::DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT name, sha256, digest_origin, applied_at \
               FROM public._migrations ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .expect("ledger snapshot after the second pass");
    let schema_after: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text, data_type::text \
           FROM information_schema.columns WHERE table_schema = 'public' \
          ORDER BY table_name, column_name",
    )
    .fetch_all(&pool)
    .await
    .expect("schema snapshot after the second pass");
    let constraints_after: Vec<(String, String)> = sqlx::query_as(
        "SELECT conname::text, contype::text FROM pg_constraint c \
           JOIN pg_namespace n ON n.oid = c.connamespace \
          WHERE n.nspname = 'public' ORDER BY conname",
    )
    .fetch_all(&pool)
    .await
    .expect("constraint snapshot after the second pass");
    let cutovers_after: Vec<(String,)> = sqlx::query_as(
        "SELECT cutover::text FROM schema_cutover_events ORDER BY cutover, id",
    )
    .fetch_all(&pool)
    .await
    .expect("cutover snapshot after the second pass");

    assert_eq!(
        ledger_before, ledger_after,
        "the second pass must not touch the ledger: not the digest, not the \
         provenance, not even applied_at"
    );
    assert_eq!(
        schema_before, schema_after,
        "the second pass must not alter any column"
    );
    assert_eq!(
        constraints_before, constraints_after,
        "the second pass must not add, drop, or recreate a constraint"
    );
    assert_eq!(
        cutovers_before, cutovers_after,
        "the second pass must not append a duplicate cutover event"
    );

    let (total, applied, baseline): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), \
                count(*) FILTER (WHERE digest_origin = 'applied'), \
                count(*) FILTER (WHERE digest_origin = 'baseline') \
           FROM public._migrations",
    )
    .fetch_one(&pool)
    .await
    .expect("ledger fingerprint");
    let on_disk = std::fs::read_dir(migrations_dir())
        .expect("read migrations")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sql"))
        .count() as i64;
    assert_eq!(
        total, on_disk,
        "the converged lane holds every current migration"
    );
    assert_eq!(baseline, 0, "an authenticated replay records no baseline rows");
    assert_eq!(applied, total, "every row is digest-verified as applied");

    pool.close().await;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop scratch");
    let _ = std::fs::remove_dir_all(&pred_dir);
}

// ---------------------------------------------------------------------------
// REV-092 item 1/2 — leading-zero normalization and the positive-int8 boundary
// ---------------------------------------------------------------------------

/// Every boundary REV-092 names, exercised against the LIVE guard by driving the
/// real migration lane, not by unit-testing the format string.
///
/// The padded cases are the point: `01000000000000000000` is twenty characters but
/// denotes `1000000000000000000`, and PostgreSQL's `::bigint` accepts it. A guard
/// that measures the written form instead of the value rejects a legitimate owner —
/// the same class of defect as REV-090-F01, one layer down.
#[tokio::test]
async fn padded_and_boundary_int8_segments_are_classified_by_value() {
    let (pool, admin, scratch, dir) = scratch_before("padded", "1036_").await;

    // Drive the exact SQL predicate the preflight uses, through the database, so
    // this cannot drift from production behaviour.
    let cases: &[(&str, bool, &str)] = &[
        ("01000000000000000000", true, "padded 19-digit value = 1000000000000000000"),
        ("00000000000000000001", true, "padded 1"),
        ("1", true, "minimum positive"),
        ("9223372036854775807", true, "i64::MAX exactly"),
        ("0000000009223372036854775807", true, "padded i64::MAX"),
        ("0", false, "zero is not a positive id"),
        ("00", false, "all-zero run is not a positive id"),
        ("0000000000000000000000", false, "long all-zero run is not a positive id"),
        ("9223372036854775808", false, "i64::MAX + 1 overflows"),
        ("09223372036854775808", false, "padded overflow still overflows"),
        ("99999999999999999999", false, "20 significant digits overflow"),
        ("solana", false, "nonnumeric"),
        ("", false, "empty"),
        ("12x4", false, "mixed"),
    ];

    for (value, expect_safe, why) in cases {
        // `$1` is the segment under test; the predicate is built exactly as the
        // preflight builds it.
        let predicate = crate::db::safe_int8_predicate_for_tests("$1");
        let got: bool = sqlx::query_scalar(&format!("SELECT {predicate}"))
            .bind(value)
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("predicate failed for {value:?}: {e}"));
        assert_eq!(
            got, *expect_safe,
            "segment {value:?} ({why}) classified wrong: expected safe={expect_safe}"
        );

        // Cross-check against PostgreSQL itself: whenever we call a value SAFE, the
        // cast the guard protects must actually succeed, and vice versa for the
        // numeric-overflow class. This is what makes the guard agree with the cast
        // rather than merely with itself.
        if *expect_safe {
            let cast: i64 = sqlx::query_scalar("SELECT ($1)::bigint")
                .bind(value)
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("value {value:?} judged safe but ::bigint failed: {e}"));
            assert!(cast > 0, "a safe segment must denote a positive id, got {cast}");
        }
    }

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// A zero-padded WORKSPACE segment must keep its owner, exactly like the unpadded
/// form — it must not be swept into the default workspace.
#[tokio::test]
async fn a_zero_padded_workspace_segment_keeps_its_owner() {
    let (pool, admin, scratch, dir) = scratch_before("padws", "1036_").await;

    const WS19: i64 = 1_000_000_000_000_000_000;
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug) OVERRIDING SYSTEM VALUE VALUES ($1, 'w', $2)",
    )
    .bind(WS19)
    .bind(tag("r92padws"))
    .execute(&pool)
    .await
    .expect("19-digit workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R92PADMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(WS19)
    .fetch_one(&pool)
    .await
    .expect("signal");

    // The key carries the PADDED spelling of a real workspace id.
    let key = format!("signal:0{WS19}:{sid}:chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, 'chat-a', 'pending', 1, now())",
    )
    .bind(&key)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed padded-workspace alert");

    // Precondition: the segment really is padded and really is 20 characters, so a
    // length-based guard would reject it.
    let (len, trimmed): (i32, String) = sqlx::query_as(
        "SELECT length(split_part(dedup_key, ':', 2))::int, \
                ltrim(split_part(dedup_key, ':', 2), '0') FROM public.alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("segment shape");
    assert_eq!(len, 20, "the probe segment must be 20 characters");
    assert_eq!(trimmed, WS19.to_string(), "and must denote the real workspace id");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a padded but valid workspace segment must not abort the lane");

    let owner: i64 = sqlx::query_scalar("SELECT workspace_id FROM public.alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("read back owner");
    let default_ws: Option<i64> = sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'default'")
        .fetch_optional(&pool)
        .await
        .expect("default workspace probe");
    assert_eq!(
        owner, WS19,
        "a zero-padded but valid workspace must keep its owner (REV-092)"
    );
    assert!(
        default_ws != Some(owner),
        "the padded row must NOT have been reassigned to the default workspace"
    );

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// A zero-padded FUNDING SUBJECT segment denotes a valid id, so the preflight must
/// NOT refuse it. The refusal is reserved for segments that genuinely cannot cast.
#[tokio::test]
async fn a_zero_padded_funding_subject_is_not_refused() {
    let (pool, admin, scratch, dir) = scratch_before("padsubj", "1036_").await;

    let ws: i64 = sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id")
        .bind(tag("r92padsub"))
        .fetch_one(&pool)
        .await
        .expect("workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R92PADSUBMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");

    // funding-kind key whose SUBJECT segment is padded but perfectly valid.
    // 25 leading zeros: the raw string is >19 characters, so a guard that measures
    // the WRITTEN form rejects it, while the value it denotes is a small valid id.
    let key = format!("funding:{ws}:0000000000000000000000000{sid}:chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, 'chat-a', 'pending', 1, now())",
    )
    .bind(&key)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed padded-subject alert");

    let trimmed: String = sqlx::query_scalar(
        "SELECT ltrim(split_part(dedup_key, ':', 3), '0') FROM public.alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("subject shape");
    assert_eq!(trimmed, sid.to_string(), "the padded subject must denote the real id");
    let raw_len: i32 = sqlx::query_scalar(
        "SELECT length(split_part(dedup_key, ':', 3))::int FROM public.alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("subject length");
    assert!(
        raw_len > 19,
        "the probe subject must be longer than 19 characters, or a length-based guard \
         would accept it anyway and this test would not discriminate (got {raw_len})"
    );

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a padded but valid funding subject must NOT be refused (REV-092)");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("row survives");
    assert_eq!(rows, 1, "the row must survive the lane untouched");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-092 item 3 — the manifest parser must reject, not tolerate
// ---------------------------------------------------------------------------

/// Each malformed manifest shape must be REJECTED with a message naming the class.
///
/// The previous parser used `filter_map`, so a malformed line vanished instead of
/// failing; a duplicate filename silently overwrote its predecessor; and an entry
/// naming a file that does not exist was never noticed. A manifest that tolerates
/// those is not evidence of anything.
#[test]
fn the_manifest_parser_rejects_every_malformed_shape() {
    let dir = std::env::temp_dir().join(format!("swi_r92_man_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let body = "-- probe\n";
    std::fs::write(dir.join("0001_a.sql"), body).expect("write a");
    std::fs::write(dir.join("0002_b.sql"), body).expect("write b");
    let digest = crate::db::migration_sha256_for_tests(body);
    let names = vec!["0001_a.sql".to_string(), "0002_b.sql".to_string()];

    // The HAPPY case must pass first, or every rejection below proves nothing.
    let good = format!("# header\n\n{digest}  0001_a.sql\n{digest}  0002_b.sql\n");
    verify_manifest_bijection(&good, &names, &dir);

    let cases: &[(&str, String, &str)] = &[
        (
            "malformed line",
            format!("{digest}  0001_a.sql\nnot-a-manifest-line\n{digest}  0002_b.sql\n"),
            "malformed",
        ),
        (
            "duplicate filename",
            format!("{digest}  0001_a.sql\n{digest}  0001_a.sql\n{digest}  0002_b.sql\n"),
            "more than once",
        ),
        (
            "missing entry",
            format!("{digest}  0001_a.sql\n"),
            "no entry for",
        ),
        (
            "extra entry",
            format!("{digest}  0001_a.sql\n{digest}  0002_b.sql\n{digest}  9999_ghost.sql\n"),
            "no SQL file present",
        ),
        (
            "short digest",
            format!("abc123  0001_a.sql\n{digest}  0002_b.sql\n"),
            "64-hex",
        ),
        (
            "three fields",
            format!("{digest}  0001_a.sql extra\n{digest}  0002_b.sql\n"),
            "malformed",
        ),
        (
            "wrong digest",
            format!("{}  0001_a.sql\n{digest}  0002_b.sql\n", "0".repeat(64)),
            "does not match the digest",
        ),
    ];

    for (label, manifest, expect) in cases {
        let manifest = manifest.clone();
        let names = names.clone();
        let dir = dir.clone();
        let err = std::panic::catch_unwind(move || {
            verify_manifest_bijection(&manifest, &names, &dir)
        })
        .expect_err(&format!("{label} must be rejected, not tolerated"));
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(
            msg.contains(expect),
            "{label}: expected a message naming {expect:?}, got {msg:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
