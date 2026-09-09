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
) -> (PgPool, crate::pg_test_support::ScratchDb, String, std::path::PathBuf) {
    // REV-093-F06: the database and its temp directory are owned by a guard that
    // drops them on unwind too, so a failing assertion no longer strands them.
    let mut scratch = crate::pg_test_support::ScratchDb::create(label).await;
    let red_dir = scratch.temp_dir("mig");
    // REV-098-F03: the manifest is reduced ALONGSIDE the SQL. This used to copy the
    // full manifest next to a SUBSET of the files, which production now refuses:
    // `MigrationBundle::from_dir` requires an exact manifest/file bijection before
    // any SQL executes. A lane shipping a manifest that describes files it does not
    // have is not a bundle; the fixture was relying on production being lax.
    crate::pg_test_support::reduced_bundle(&migrations_dir(), &red_dir, cutoff);
    let pool = scratch.pool().await;
    crate::db::migrate_dir_with(&pool, &red_dir, true)
        .await
        .unwrap_or_else(|e| panic!("migrate below {cutoff}: {e:#}"));
    let name = scratch.name().to_string();
    (pool, scratch, name, red_dir)
}
/// The commit that first versioned the migration bundle inside this crate. Its
/// tree is a CONTENT-ADDRESSED artifact: git object ids are sha1 over the stored
/// bytes, so `git cat-file` at a fixed commit reproduces the exact bytes that
/// commit recorded — something a copy of the working tree can never establish.
const PREDECESSOR_COMMIT: &str = "a60647dac892afad18b03671bea0638b695c0cdf";

/// The last commit whose `migrations/` shipped the ORIGINAL 1041 (REV-098-F01).
///
/// A database migrated by this commit records 1041 with digest `d7a8984d…`; that is
/// the real predecessor state REV-097 could not upgrade.
const REV096_COMMIT: &str = "e1e0403ad9eff15766676c647e427ac2307db097";

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
fn bundle_at_commit(commit: &str, dest: &std::path::Path) -> Option<usize> {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let listing = std::process::Command::new("git")
        .args(["-C", root.to_str()?, "ls-tree", "--name-only",
               &format!("{commit}:migrations")])
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
                   &format!("{commit}:migrations/{name}")])
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

/// The bundle as of [`PREDECESSOR_COMMIT`].
fn predecessor_bundle(dest: &std::path::Path) -> Option<usize> {
    bundle_at_commit(PREDECESSOR_COMMIT, dest)
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
    // REV-093-F04: parsing is the PRODUCTION parser. This used to be a second,
    // stricter implementation living only in the test module, so the strict rules
    // were never applied at the runtime trust boundary and these assertions proved
    // nothing about `db migrate`. One parser, one policy.
    let recorded = crate::db::parse_migration_manifest_for_tests(manifest, "manifest")
        .unwrap_or_else(|e| panic!("{e:#}"));

    // Bijection is asserted HERE rather than in the parser, because production also
    // parses reduced lanes that deliberately ship a subset.
    let present: std::collections::BTreeSet<&str> = sql_names.iter().map(|s| s.as_str()).collect();
    let listed: std::collections::BTreeSet<&str> = recorded.keys().map(|s| s.as_str()).collect();
    let missing: Vec<&&str> = present.difference(&listed).collect();
    let extra: Vec<&&str> = listed.difference(&present).collect();
    assert!(
        missing.is_empty(),
        "manifest has no entry for {} SQL file(s): {missing:?}",
        missing.len()
    );
    assert!(
        extra.is_empty(),
        "manifest lists {} entr(y/ies) with no SQL file present: {extra:?}",
        extra.len()
    );
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

/// Explicit end-of-test release.
///
/// REV-093-F06: teardown is now owned by [`ScratchDb`]'s `Drop`, which also runs when
/// a test unwinds. This helper only closes the pool early and then lets the guard go;
/// it is kept so the success path still reads as a deliberate teardown.
async fn drop_scratch(
    _guard: &crate::pg_test_support::ScratchDb,
    pool: PgPool,
    _scratch: &str,
    _dir: &std::path::Path,
) {
    pool.close().await;
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

    // Layer 1 (REV-098-F03): the bundle no longer matches its own reviewed manifest,
    // so it is refused BEFORE any SQL executes and before the ledger is consulted.
    let err = crate::db::migrate_dir_with(&pool, &dir, true)
        .await
        .expect_err("an edited applied migration must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not the reviewed bytes"),
        "expected a manifest refusal, got: {msg}"
    );

    // Layer 2 (REV-087-F01): re-point the MANIFEST at the tampered bytes, so the
    // bundle is internally self-consistent and layer 1 has nothing to say. The
    // ledger must still refuse the same-name drift — otherwise anyone who edits a
    // shipped migration and re-runs the manifest generator would be blessed.
    let manifest_path = dir.join(crate::db::MIGRATION_MANIFEST);
    let manifest = std::fs::read_to_string(&manifest_path).expect("read manifest");
    let tampered_digest = crate::db::migration_sha256_for_tests(
        &String::from_utf8_lossy(&tampered),
    );
    let victim_name = "1033_rev074_signal_uniqueness_and_alert_outbox.sql";
    let rewritten: String = manifest
        .lines()
        .map(|l| {
            if l.split_whitespace().nth(1) == Some(victim_name) {
                format!("{tampered_digest}  {victim_name}\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    std::fs::write(&manifest_path, &rewritten).expect("write re-pointed manifest");

    let err = crate::db::migrate_dir_with(&pool, &dir, true)
        .await
        .expect_err("a self-consistent but drifted bundle must still be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no longer matches the file on disk"),
        "expected a same-name drift refusal, got: {msg}"
    );

    // Restoring the bytes restores the lane: the refusal is about the CONTENT, not
    // about having run twice.
    std::fs::write(&victim, &original).expect("restore migration");
    std::fs::write(&manifest_path, &manifest).expect("restore manifest");
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
    // REV-097-F06: guard-owned. This fixture previously created its database and
    // temp dir by hand, so an assertion failure anywhere below stranded both.
    let mut guard = crate::pg_test_support::ScratchDb::create("predconv").await;
    let pred_dir = guard.temp_dir("pred");
    let Some(sql_count) = predecessor_bundle(&pred_dir) else {
        panic!(
            "the predecessor commit {PREDECESSOR_COMMIT} is unreachable from this checkout; \
             provenance cannot be authenticated here. Fetch full history rather than \
             substituting a copy of the current tree"
        );
    };
    assert!(sql_count > 0, "the predecessor bundle must contain SQL");
    let pool = guard.pool().await;

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

    // REV-097-F06: guard-owned teardown, also on unwind.
    pool.close().await;
    drop(guard);
}

// ---------------------------------------------------------------------------
// REV-098-F01 — a REV-096 database must upgrade to current
// ---------------------------------------------------------------------------

/// The exact predecessor lane REV-097 broke: apply the AUTHENTICATED `e1e0403`
/// bundle (whose 1041 is the original `d7a8984d…`), then upgrade with the bundle
/// this binary carries.
///
/// REV-097 edited 1041 IN PLACE, so this lane died with
/// `applied migration 1041… no longer matches the file on disk`, and no database
/// that had ever run REV-095/096 could be upgraded. The correction had to move to
/// forward migration 1042; this fixture is what makes that a rule rather than an
/// intention. It asserts four things a "just restore the file" fix could each fail:
///
///   1. the upgrade SUCCEEDS from the real predecessor state;
///   2. 1041's ledger digest is STILL `d7a8984d…` afterwards — the shipped bytes
///      were never rewritten;
///   3. 1042 ran, and the invariant is installed correctly on BOTH tables;
///   4. a second pass is an exact no-op.
///
/// The decoy is the other half: before upgrading, a same-named constraint is
/// planted on an unrelated table and the REAL `workspaces` constraint is replaced
/// with a WRONG definition. That is precisely the state original-1041 could produce
/// and could not detect (`conname` alone, existence only), so 1042 must refuse it —
/// and then accept the lane once the operator reconciles.
#[tokio::test]
async fn a_rev096_database_upgrades_through_1042_without_rewriting_1041() {
    let mut guard = crate::pg_test_support::ScratchDb::create("f01upg").await;
    let pred_dir = guard.temp_dir("rev096");
    let sql_count = bundle_at_commit(REV096_COMMIT, &pred_dir).unwrap_or_else(|| {
        panic!(
            "commit {REV096_COMMIT} must be reachable: this lane is the whole point of \
             the fixture and a working-tree copy cannot substitute for it"
        )
    });
    let pool = guard.pool().await;

    // Phase 1: the predecessor bundle, applied by the production migrator.
    crate::db::migrate_dir_with(&pool, &pred_dir, true)
        .await
        .expect("the authenticated REV-096 bundle must apply");

    const ORIGINAL_1041: &str =
        "d7a8984db0c30eecc42421c6209fe126ab3baa7315dd310afa19d21dd3da7db4";
    let recorded: String = sqlx::query_scalar(
        "SELECT sha256 FROM public._migrations \
          WHERE name = '1041_rev093_positive_id_invariant.sql'",
    )
    .fetch_one(&pool)
    .await
    .expect("1041 must be applied by the predecessor bundle");
    assert_eq!(
        recorded, ORIGINAL_1041,
        "the predecessor lane must record the ORIGINAL 1041 digest, or this fixture \
         is not standing on the state REV-098-F01 describes"
    );

    // The decoy state original-1041 could produce: the real constraint replaced by a
    // plausible-but-wrong one, and a same-named constraint on another table.
    sqlx::raw_sql(
        "ALTER TABLE public.workspaces DROP CONSTRAINT workspaces_id_positive_check; \
         ALTER TABLE public.workspaces ADD CONSTRAINT workspaces_id_positive_check \
           CHECK (id >= 0); \
         CREATE TABLE public._decoy_ws (id bigint); \
         ALTER TABLE public._decoy_ws ADD CONSTRAINT funding_radar_cases_id_positive_check \
           CHECK (id > 0);",
    )
    .execute(&pool)
    .await
    .expect("plant the decoy / wrong-definition state");

    // Phase 2a: the upgrade must FAIL CLOSED on the wrong definition rather than
    // accept a constraint that does not enforce the invariant.
    let err = crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect_err("1042 must refuse a wrong `workspaces_id_positive_check`");
    let err = format!("{err:#}");
    assert!(
        err.contains("unexpected definition") && err.contains("workspaces"),
        "expected the named wrong-definition refusal, got: {err}"
    );

    // Reconcile exactly what the operator would: remove the wrong constraint. The
    // decoy on `_decoy_ws` STAYS — a same-named constraint on another table must not
    // stand in for the real one, which is the other half of the finding.
    sqlx::raw_sql("ALTER TABLE public.workspaces DROP CONSTRAINT workspaces_id_positive_check")
        .execute(&pool)
        .await
        .expect("reconcile the wrong constraint");

    // Phase 2b: the real upgrade.
    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a REV-096 database must upgrade to current");

    // 1041's applied bytes were never rewritten.
    let after: String = sqlx::query_scalar(
        "SELECT sha256 FROM public._migrations \
          WHERE name = '1041_rev093_positive_id_invariant.sql'",
    )
    .fetch_one(&pool)
    .await
    .expect("1041 ledger row after the upgrade");
    assert_eq!(
        after, ORIGINAL_1041,
        "the shipped 1041 must still be the bytes that were applied; a correction \
         belongs in a forward migration"
    );

    // 1042 ran, and the invariant is installed on BOTH tables with the exact
    // definition — the decoy on `_decoy_ws` did not satisfy it.
    let repaired: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._migrations \
          WHERE name = '1042_rev098_positive_id_invariant_repair.sql' \
            AND digest_origin = 'applied'",
    )
    .fetch_one(&pool)
    .await
    .expect("1042 ledger probe");
    assert_eq!(repaired, 1, "the forward repair migration must have been applied");
    for (table, conname) in [
        ("public.workspaces", "workspaces_id_positive_check"),
        ("public.funding_radar_cases", "funding_radar_cases_id_positive_check"),
    ] {
        let def: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
              WHERE conname = $1 AND conrelid = $2::regclass",
        )
        .bind(conname)
        .bind(table)
        .fetch_optional(&pool)
        .await
        .expect("constraint probe");
        assert_eq!(
            def.as_deref(),
            Some("CHECK ((id > 0))"),
            "{conname} must be installed on {table} with the exact definition"
        );
    }

    // The whole current bundle is present and verified.
    let (total, applied): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE digest_origin = 'applied') \
           FROM public._migrations",
    )
    .fetch_one(&pool)
    .await
    .expect("ledger fingerprint");
    let current =
        crate::db::MigrationBundle::embedded().expect("embedded bundle").entries().len() as i64;
    assert_eq!(total, current, "the upgraded lane holds every current migration");
    assert_eq!(applied, total, "every row is digest-verified as applied");
    assert!(
        (sql_count as i64) < current,
        "the predecessor bundle must be strictly older than current ({sql_count} vs {current})"
    );

    // Second pass: an exact no-op.
    let before: Vec<(String, Option<String>, Option<String>, chrono::DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT name, sha256, digest_origin, applied_at \
               FROM public._migrations ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .expect("ledger before the second pass");
    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a second pass must converge");
    let after_rows: Vec<(String, Option<String>, Option<String>, chrono::DateTime<Utc>)> =
        sqlx::query_as(
            "SELECT name, sha256, digest_origin, applied_at \
               FROM public._migrations ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .expect("ledger after the second pass");
    assert_eq!(before, after_rows, "the second pass must not touch the ledger");

    pool.close().await;
    drop(guard);
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
    //
    // REV-093-F01 added the signed and whitespace-padded rows: PostgreSQL's bigint
    // input function accepts a leading `+` and surrounding whitespace, and four
    // successive hand-written regexes each failed to model some corner of that
    // domain. The predicate now asks the parser instead of approximating it, so the
    // table below is a statement about PostgreSQL's domain, not about a regex.
    let cases: &[(&str, bool, &str)] = &[
        ("01000000000000000000", true, "padded 19-digit value = 1000000000000000000"),
        ("00000000000000000001", true, "padded 1"),
        ("1", true, "minimum positive"),
        ("9223372036854775807", true, "i64::MAX exactly"),
        ("0000000009223372036854775807", true, "padded i64::MAX"),
        ("+1000000000000000000", true, "explicit plus sign (REV-093-F01)"),
        ("  +00000000000000000001  ", true, "plus sign, padding, surrounding whitespace"),
        ("+9223372036854775807", true, "signed i64::MAX"),
        (" 42 ", true, "surrounding whitespace alone"),
        ("+0", false, "signed zero is still not a positive id"),
        ("-1", false, "negative is not a positive id"),
        ("-9223372036854775808", false, "i64::MIN is not a positive id"),
        ("0", false, "zero is not a positive id"),
        ("00", false, "all-zero run is not a positive id"),
        ("0000000000000000000000", false, "long all-zero run is not a positive id"),
        ("9223372036854775808", false, "i64::MAX + 1 overflows"),
        ("+9223372036854775808", false, "signed overflow still overflows"),
        ("09223372036854775808", false, "padded overflow still overflows"),
        ("99999999999999999999", false, "20 significant digits overflow"),
        ("solana", false, "nonnumeric"),
        ("", false, "empty"),
        ("   ", false, "whitespace only"),
        ("12x4", false, "mixed"),
        ("1.5", false, "not an integer"),
        ("+ 1", false, "sign detached from digits"),
        ("++1", false, "double sign"),
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

        // Cross-check against PostgreSQL itself, in BOTH directions. This is what
        // makes the guard agree with the cast it protects rather than merely with
        // itself: SAFE must mean the cast succeeds and yields a positive id, and
        // UNSAFE must mean the cast would either fail or yield a non-positive id.
        let cast: Result<i64, _> = sqlx::query_scalar("SELECT ($1)::bigint")
            .bind(value)
            .fetch_one(&pool)
            .await;
        if *expect_safe {
            let cast = cast.unwrap_or_else(|e| {
                panic!("value {value:?} judged safe but ::bigint failed: {e}")
            });
            assert!(cast > 0, "a safe segment must denote a positive id, got {cast}");
        } else {
            match cast {
                Err(_) => {} // unparseable: correctly refused before the cast
                Ok(v) => assert!(
                    v <= 0,
                    "value {value:?} ({why}) was judged UNSAFE, but it casts cleanly to \
                     the positive id {v} — the guard is stricter than PostgreSQL and \
                     would sweep a real owner to the default workspace"
                ),
            }
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
    // REV-097-F06: guard-owned; the panicking cases below must not strand it.
    let dir_guard = crate::pg_test_support::ScratchDir::create("manifest");
    let dir = dir_guard.path().to_path_buf();
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
            "is malformed",
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
            "64-character hex",
        ),
        (
            "three fields",
            format!("{digest}  0001_a.sql extra\n{digest}  0002_b.sql\n"),
            "is malformed",
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

}

// ---------------------------------------------------------------------------
// REV-093-F01 / REV-094 — signed segments are PostgreSQL-valid and must be
// treated as such by the lane, not just by the predicate
// ---------------------------------------------------------------------------

/// A `+`-signed workspace segment denotes a real owner, so the preflight must not
/// sweep it into the default workspace.
///
/// This is the reviewer's exact evidence: `+1000000000000000000` parsed by
/// PostgreSQL as `1000000000000000000`, judged `false` by the guard, and the run
/// reported `preflight repair complete rows=1` with the row landing in `default`
/// instead of its real owner.
#[tokio::test]
async fn a_plus_signed_workspace_segment_keeps_its_owner() {
    let (pool, admin, scratch, dir) = scratch_before("plusws", "1036_").await;

    const WS19: i64 = 1_000_000_000_000_000_000;
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug) OVERRIDING SYSTEM VALUE VALUES ($1, 'w', $2)",
    )
    .bind(WS19)
    .bind(tag("r94plusws"))
    .execute(&pool)
    .await
    .expect("19-digit workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R94PLUSMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(WS19)
    .fetch_one(&pool)
    .await
    .expect("signal");

    // The key carries the SIGNED spelling of a real workspace id.
    let key = format!("signal:+{WS19}:{sid}:chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, 'chat-a', 'pending', 1, now())",
    )
    .bind(&key)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed signed-workspace alert");

    // Precondition: PostgreSQL itself accepts this segment and resolves it to the
    // real workspace. Without this the assertion below could pass for the wrong
    // reason (e.g. if the segment were simply unparseable everywhere).
    let parsed: i64 = sqlx::query_scalar(
        "SELECT (split_part(dedup_key, ':', 2))::bigint FROM public.alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("PostgreSQL must accept the signed segment");
    assert_eq!(parsed, WS19, "the signed segment must denote the real workspace id");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a signed but valid workspace segment must not abort the lane");

    let owner: i64 = sqlx::query_scalar("SELECT workspace_id FROM public.alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("read back owner");
    let default_ws: Option<i64> =
        sqlx::query_scalar("SELECT id FROM workspaces WHERE slug = 'default'")
            .fetch_optional(&pool)
            .await
            .expect("default workspace probe");
    assert_eq!(
        owner, WS19,
        "a `+`-signed but valid workspace must keep its owner (REV-093-F01)"
    );
    assert!(
        default_ws != Some(owner),
        "the signed row must NOT have been reassigned to the default workspace"
    );

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// A `+`-signed funding SUBJECT segment is valid bigint input, so the preflight's
/// named refusal must not fire on it. The refusal is reserved for segments that
/// genuinely cannot cast.
#[tokio::test]
async fn a_plus_signed_funding_subject_is_not_refused() {
    let (pool, admin, scratch, dir) = scratch_before("plussubj", "1036_").await;

    let ws: i64 =
        sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id")
            .bind(tag("r94plussub"))
            .fetch_one(&pool)
            .await
            .expect("workspace");
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
         VALUES ($1, 'solana', 'R94PLUSSUBMINT', 'entry', now(), 80, 'active') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await
    .expect("signal");

    // funding-kind key whose SUBJECT segment is signed AND whitespace-padded.
    let key = format!("funding:{ws}: +{sid} :chat-a");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                             attempt_count, next_attempt_at) \
         VALUES ($1, 'signal', $2, 'chat-a', 'pending', 1, now())",
    )
    .bind(&key)
    .bind(sid)
    .execute(&pool)
    .await
    .expect("seed signed-subject alert");

    let parsed: i64 = sqlx::query_scalar(
        "SELECT (split_part(dedup_key, ':', 3))::bigint FROM public.alerts WHERE dedup_key = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .expect("PostgreSQL must accept the signed subject segment");
    assert_eq!(parsed, sid, "the signed subject must denote the real id");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("a signed but valid funding subject must NOT be refused (REV-093-F01)");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM public.alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("row survives");
    assert_eq!(rows, 1, "the row must survive the lane untouched");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// Invalid input must never reach the cast, even inside a single scan that mixes
/// valid and invalid segments.
///
/// A bare `pg_input_is_valid(x, 'bigint') AND x::bigint > 0` reads as safe but
/// leaves the planner free to hoist the cast, which aborts the entire migration on
/// the first junk row. The `CASE` form pins the evaluation order; this proves it
/// against real rows rather than trusting the shape.
#[tokio::test]
async fn invalid_segments_never_reach_the_bigint_cast() {
    let (pool, admin, scratch, dir) = scratch_before("nocast", "1036_").await;

    let ws: i64 =
        sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id")
            .bind(tag("r94nocast"))
            .fetch_one(&pool)
            .await
            .expect("workspace");
    // One valid signed row and several that would abort a hoisted cast, all in the
    // same table so any single scan sees the mix.
    for (idx, seg) in ["+1", "solana", "9223372036854775808", "", "-1", "1.5"]
        .iter()
        .enumerate()
    {
        let sid: i64 = sqlx::query_scalar(
            "INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status) \
             VALUES ($1, 'solana', $2, 'entry', now(), 80, 'active') RETURNING id",
        )
        .bind(ws)
        .bind(format!("R94NOCAST{idx}"))
        .fetch_one(&pool)
        .await
        .expect("signal");
        sqlx::query(
            "INSERT INTO alerts (dedup_key, subject_kind, signal_id, destination, state, \
                                 attempt_count, next_attempt_at) \
             VALUES ($1, 'signal', $2, 'chat-a', 'pending', 1, now())",
        )
        .bind(format!("signal:{seg}:{sid}:chat-a"))
        .bind(sid)
        .execute(&pool)
        .await
        .expect("seed mixed segment");
    }

    // The predicate must evaluate over every row without raising, both projected
    // and as a WHERE filter — the two shapes the preflight actually uses.
    let predicate = crate::db::safe_int8_predicate_for_tests("split_part(dedup_key, ':', 2)");
    let safe_count: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM public.alerts WHERE {predicate}"
    ))
    .fetch_one(&pool)
    .await
    .expect("the guard must not raise on invalid input in a WHERE filter");
    assert_eq!(safe_count, 1, "exactly the `+1` row is a valid positive id");

    let unsafe_count: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM public.alerts WHERE NOT {predicate}"
    ))
    .fetch_one(&pool)
    .await
    .expect("the negated guard must not raise either");
    assert_eq!(unsafe_count, 5, "the other five segments are refused, not cast");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-093-F02 — the positive-ID invariant is enforced by the schema, and a
// pre-existing violation aborts instead of being silently reassigned
// ---------------------------------------------------------------------------

/// Migration 1041 installs validated CHECKs on exactly the ID domains
/// `safe_int8_predicate` relies on, so the predicate's positivity assumption is a
/// schema fact rather than a hope.
#[tokio::test]
async fn the_positive_id_invariant_is_enforced_by_the_schema() {
    let (pool, admin, scratch, dir) = scratch_before("posid", "9999_").await;

    for (table, constraint) in [
        ("workspaces", "workspaces_id_positive_check"),
        ("funding_radar_cases", "funding_radar_cases_id_positive_check"),
    ] {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = $1 \
               AND conrelid = ($2::text)::regclass AND contype = 'c' AND convalidated)",
        )
        .bind(constraint)
        .bind(format!("public.{table}"))
        .fetch_one(&pool)
        .await
        .expect("constraint probe");
        assert!(present, "{table} must carry a VALIDATED {constraint} (REV-093-F02)");
    }

    // The invariant must actually bite: an explicit zero id is refused by the
    // database, so no schema-legal-but-non-positive owner can exist for the guard
    // to disagree with.
    let err = sqlx::query(
        "INSERT INTO workspaces (id, name, slug) OVERRIDING SYSTEM VALUE VALUES (0, 'w', $1)",
    )
    .bind(tag("r95zero"))
    .execute(&pool)
    .await
    .expect_err("id 0 must be refused by the schema");
    let db_err = err.as_database_error().expect("a database error");
    assert_eq!(
        db_err.code().as_deref(),
        Some("23514"),
        "expected check_violation, got {db_err:?}"
    );

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// A database that ALREADY holds a non-positive id must abort with the named
/// diagnostic, and must not be repaired behind the operator's back.
#[tokio::test]
async fn a_preexisting_non_positive_id_aborts_with_a_named_diagnostic() {
    // Stop before 1041 so the offending row can be planted first.
    let (pool, admin, scratch, dir) = scratch_before("posidbad", "1041_").await;

    sqlx::query("INSERT INTO workspaces (id, name, slug) OVERRIDING SYSTEM VALUE VALUES (0, 'w', $1)")
        .bind(tag("r95bad"))
        .execute(&pool)
        .await
        .expect("a pre-1041 lane must still accept id 0 — that is the whole problem");

    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE id <= 0")
        .fetch_one(&pool)
        .await
        .expect("count before");
    assert_eq!(before, 1, "the offending row must exist before the upgrade");

    let err = crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect_err("1041 must refuse to install the invariant over violating data");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("positive-ID invariant violated"),
        "expected the named diagnostic, got: {msg}"
    );
    assert!(
        msg.contains("workspaces.id: 0"),
        "the diagnostic must name the offending table and id, got: {msg}"
    );

    // Fail CLOSED: the row is untouched, not reassigned/deleted/renumbered, and the
    // constraint was not installed over bad data.
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE id <= 0")
        .fetch_one(&pool)
        .await
        .expect("count after");
    assert_eq!(after, 1, "the offending row must be left exactly as it was");
    let installed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'workspaces_id_positive_check')",
    )
    .fetch_one(&pool)
    .await
    .expect("constraint probe");
    assert!(!installed, "no constraint may be installed while data violates it");

    // Operator reconciles; the lane then completes.
    sqlx::query("DELETE FROM workspaces WHERE id <= 0")
        .execute(&pool)
        .await
        .expect("operator reconciliation");
    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("after reconciliation the invariant installs cleanly");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-093-F03 — the ledger CHECK is not recreated every pass, and two migrators
// may run concurrently
// ---------------------------------------------------------------------------

/// A second pass must leave the constraint's OID untouched.
///
/// REV-092 claimed an "exact no-op" while the migrator unconditionally dropped and
/// re-added this CHECK on every run. The old snapshot compared `(conname, contype)`,
/// which cannot see a drop-and-recreate; the OID can.
#[tokio::test]
async fn a_second_pass_preserves_the_ledger_constraint_oid() {
    let (pool, admin, scratch, dir) = scratch_before("oid", "9999_").await;

    let (oid_before, def_before): (i64, String) = sqlx::query_as(
        "SELECT oid::bigint, pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conname = '_migrations_digest_origin_check' \
            AND conrelid = 'public._migrations'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("constraint must exist after the first pass");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("second pass");

    let (oid_after, def_after): (i64, String) = sqlx::query_as(
        "SELECT oid::bigint, pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conname = '_migrations_digest_origin_check' \
            AND conrelid = 'public._migrations'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("constraint after");

    assert_eq!(
        oid_before, oid_after,
        "the ledger CHECK was dropped and recreated: the OID changed, so the second \
         pass is not the no-op it claims to be (REV-093-F03)"
    );
    assert_eq!(def_before, def_after, "and its definition must be unchanged");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}


// ---------------------------------------------------------------------------
// REV-093-F04 — every malformed manifest class fails through PRODUCTION
// ---------------------------------------------------------------------------

/// Drive the production migrator, not a test helper, against each malformed
/// manifest shape. The strict rules must live at the runtime trust boundary.
#[tokio::test]
async fn production_migration_refuses_every_malformed_manifest_class() {
    // The manifest is production's authority for BACKFILLING a pre-checksum ledger
    // row, so it is consulted only when a migration is recorded as applied with no
    // digest. That is the trust boundary; the test has to actually stand on it.
    // (A fully-migrated database never reads the manifest at all, so asserting
    // against a plain second pass would assert nothing.)
    let (pool, admin, scratch, dir) = scratch_before("prodman", "1036_").await;

    let good = std::fs::read_to_string(dir.join("MANIFEST.sha256")).expect("manifest");
    let victim = "1035_rev078_outbox_grants_subjects_sweep.sql";
    let sample = good
        .lines()
        .find(|l| l.contains(victim))
        .unwrap_or_else(|| panic!("manifest must record {victim}"))
        .to_string();
    let digest = sample.split_whitespace().next().expect("digest").to_string();

    // Force the backfill path: strip the digest from an applied row so the next
    // pass must consult the manifest to re-establish provenance.
    async fn strip(p: &sqlx::PgPool, victim: &str) {
        sqlx::query(
            "UPDATE public._migrations SET sha256 = NULL, digest_origin = NULL WHERE name = $1",
        )
        .bind(victim)
        .execute(p)
        .await
        .expect("strip digest");
    }

    // Sanity: with a WELL-FORMED manifest the backfill path succeeds. Without this
    // the refusals below could pass for the wrong reason.
    strip(&pool, victim).await;
    crate::db::migrate_dir_with(&pool, &dir, true)
        .await
        .expect("a well-formed manifest must let the backfill path succeed");

    let cases: &[(&str, String, &str)] = &[
        ("malformed line", format!("{good}\nthis-is-not-a-manifest-line\n"), "is malformed"),
        ("extra field", format!("{good}\n{digest}  9999_ghost.sql extra\n"), "is malformed"),
        ("short digest", format!("{good}\nabc123  9999_ghost.sql\n"), "64-character hex"),
        (
            "non-hex digest",
            format!("{good}\n{}  9999_ghost.sql\n", "z".repeat(64)),
            "64-character hex",
        ),
        ("duplicate filename", format!("{good}\n{digest}  {victim}\n"), "more than once"),
    ];

    for (label, body, expect) in cases {
        std::fs::write(dir.join("MANIFEST.sha256"), body).expect("write manifest");
        strip(&pool, victim).await;
        let err = crate::db::migrate_dir_with(&pool, &dir, true)
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains(expect),
            "{label}: production must refuse with a message naming {expect:?}, got {err:?}"
        );
    }

    // Restored manifest: the lane recovers, proving the refusals were about the
    // manifest and not about the database being wedged.
    std::fs::write(dir.join("MANIFEST.sha256"), &good).expect("restore manifest");
    strip(&pool, victim).await;
    crate::db::migrate_dir_with(&pool, &dir, true)
        .await
        .expect("the restored manifest verifies again");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-098-F03 — the bundle is EMBEDDED; no directory can outrank it
// ---------------------------------------------------------------------------

/// The embedded bundle self-validates and equals the reviewed source bundle.
///
/// REV-095/REV-097 asserted the ORDER of a candidate Vec, which is a statement
/// about a list, not about which bytes run. There is no candidate list any more:
/// the SQL is compiled into the binary, so the only thing worth asserting here is
/// that the compiled-in bundle is exactly the reviewed one and that constructing it
/// enforces the manifest. The behavioural proof — a RELOCATED binary with hostile
/// directories planted around it — is
/// `the_relocated_binary_uses_only_its_embedded_bundle`.
#[test]
fn the_embedded_bundle_is_the_reviewed_bundle() {
    let embedded = crate::db::MigrationBundle::embedded()
        .expect("the embedded bundle must satisfy its own manifest");
    let from_source = crate::db::MigrationBundle::from_dir(&migrations_dir())
        .expect("the reviewed source bundle must satisfy its manifest");
    assert_eq!(
        embedded.entries(),
        from_source.entries(),
        "the compiled-in bundle must be byte-identical to the reviewed migrations/"
    );
}

// ---------------------------------------------------------------------------
// REV-093-F06 — disposable databases and temp dirs are cleaned up on the
// FAILURE path, not only on success
// ---------------------------------------------------------------------------

/// A panicking test must still leave no database and no temp directory behind.
///
/// Every fixture used to end with an explicit `drop_scratch(...).await` on the
/// success path only, so a failing assertion unwound past it and stranded the
/// scratch database. The REV-095 round leaked eight that way and they were swept by
/// hand afterwards — which is not a mechanism, because the next failing test leaks
/// again. Ownership now sits in `ScratchDb::drop`.
///
/// The proof runs a real panic inside `catch_unwind`, then asserts from OUTSIDE the
/// unwound scope that both the database and the temp dir are gone. Names are
/// captured first so they can be probed after the guard is destroyed.
#[tokio::test]
async fn a_panicking_test_still_cleans_up_its_scratch_database() {
    let admin_url = crate::pg_test_support::require_live_url();

    // Build the guard, remember what it owns, then panic while holding it.
    let (name, dir) = {
        let mut guard = crate::pg_test_support::ScratchDb::create("f06panic").await;
        let dir = guard.temp_dir("probe");
        let name = guard.name().to_string();
        std::fs::write(dir.join("marker.txt"), b"probe").expect("write marker");

        // Precondition: both really exist while the guard is alive, otherwise the
        // assertions below would pass vacuously.
        assert!(
            crate::pg_test_support::ScratchDb::exists(&admin_url, &name).await,
            "the scratch database must exist before the panic"
        );
        assert!(dir.join("marker.txt").exists(), "the temp dir must exist before the panic");

        // Unwind with the guard owned by the panicking scope. `Drop` runs during the
        // unwind; it must not itself panic (that would abort the process).
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owned = guard;
            panic!("REV-093-F06 deliberate failure-path probe");
        }));
        assert!(unwound.is_err(), "the probe must actually have panicked");
        (name, dir)
    };

    // The guard was dropped during the unwind, so both resources are gone.
    assert!(
        !crate::pg_test_support::ScratchDb::exists(&admin_url, &name).await,
        "database {name} survived a panicking test: failure-path cleanup is not \
         wired up (REV-093-F06)"
    );
    assert!(
        !dir.exists(),
        "temp dir {} survived a panicking test (REV-093-F06)",
        dir.display()
    );
}

/// The guard is what does the cleaning: disarm it and the database survives.
///
/// Without this, the test above could pass because something ELSE removed the
/// database — a stray sweep, or a name that was never created. Proving the negative
/// pins the causality on `Drop`.
#[tokio::test]
async fn the_scratch_guard_is_what_performs_the_cleanup() {
    let admin_url = crate::pg_test_support::require_live_url();
    let guard = crate::pg_test_support::ScratchDb::create("f06disarm").await;
    let name = guard.leak_for_tests(); // consumes the guard WITHOUT dropping the DB

    assert!(
        crate::pg_test_support::ScratchDb::exists(&admin_url, &name).await,
        "a disarmed guard must leave the database behind — otherwise the cleanup \
         assertions elsewhere prove nothing about the guard"
    );

    // Clean up this deliberate leak by hand so the run leaves no residue.
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("admin pool");
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop the deliberately leaked database");
    admin.close().await;
    assert!(
        !crate::pg_test_support::ScratchDb::exists(&admin_url, &name).await,
        "the deliberate leak must not outlive this test"
    );
}

/// The whole pg lane must leave no `swi_scratch_%` database behind.
///
/// A per-guard proof does not establish that every FIXTURE adopted the guard. This
/// one is a class check: after this test's own guard is gone, no scratch database
/// from any fixture in this process may remain. It runs last by name ordering
/// within the module and is deliberately cheap.
#[tokio::test]
async fn no_scratch_database_outlives_the_fixtures_that_made_it() {
    let admin_url = crate::pg_test_support::require_live_url();
    let name = {
        let guard = crate::pg_test_support::ScratchDb::create("f06sweep").await;
        guard.name().to_string()
    }; // dropped here

    assert!(
        !crate::pg_test_support::ScratchDb::exists(&admin_url, &name).await,
        "a guard dropped on the SUCCESS path must also remove its database"
    );

    // And nothing this process created is still around at this instant — measured
    // over EVERY fixture naming family, not just the guard's own prefix.
    //
    // REV-097-F06: the previous probe matched `swi_scratch_%_<pid>_%` only, so a
    // fixture that created a database or directory under any other spelling (the
    // historical `swi_r85_*`, `swi_r87f03_*`, `swi_upg_*`, `swi_f0N_*` families)
    // could leak without this class check noticing. The scan is now over the whole
    // `swi%` namespace, restricted to THIS process id so a concurrent harness (the
    // lib and bin test binaries run as separate processes) is never blamed.
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("admin pool");
    let pid = std::process::id().to_string();
    let residue: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database \
          WHERE datname LIKE 'swi%' AND datname LIKE '%' || $1 || '%' ORDER BY datname",
    )
    .bind(&pid)
    .fetch_all(&admin)
    .await
    .expect("residue probe");
    admin.close().await;
    assert!(
        residue.is_empty(),
        "these scratch databases from this process were not cleaned up: {residue:?}"
    );

    // Temp directories are the other half of the same ownership rule: a fixture that
    // builds a reduced migration bundle must not strand it either.
    let dir_residue: Vec<String> = std::fs::read_dir(std::env::temp_dir())
        .expect("read temp dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("swi") && n.contains(&pid))
        .collect();
    assert!(
        dir_residue.is_empty(),
        "these temp directories from this process were not cleaned up: {dir_residue:?}"
    );
}

// ---------------------------------------------------------------------------
// REV-098-F04 — a FAILING test PROCESS leaves nothing behind, in any namespace
// ---------------------------------------------------------------------------

/// The fixture the gate below runs in a CHILD PROCESS. It creates exactly what a
/// real live fixture creates — a migrated scratch database and a temp directory —
/// and then FAILS.
///
/// Inert unless `SWI_F04_FAIL_PROBE=1`, so a normal run passes; the parent gate is
/// what sets the variable. A test that fails only when asked is the only way to
/// observe process-exit cleanup without failing the suite.
#[tokio::test]
async fn f04_failure_probe_child() {
    if std::env::var("SWI_F04_FAIL_PROBE").as_deref() != Ok("1") {
        return;
    }
    let (_guard, pool) = crate::pg_test_support::migrated_scratch("f04child").await;
    let mut owner = crate::pg_test_support::ScratchDb::create("f04childdir").await;
    let dir = owner.temp_dir("probe");
    std::fs::write(dir.join("marker.txt"), b"probe").expect("write marker");
    let _ = pool;
    panic!("SWI_F04_FAIL_PROBE: deliberate failure with live resources held");
}

/// Run a REAL failing test in a CHILD PROCESS and require zero residue after it
/// exits — across every namespace a fixture in this repo can create.
///
/// REV-098-F04: the in-band scan above measures THIS process at THIS instant, so it
/// cannot see what a crashed or panicking process leaves behind, and it looked only
/// at `swi%`. Ten fixtures used `#[sqlx::test]`, whose `_sqlx_test_*` databases are
/// dropped only after a test body RETURNS — a failing one leaked outside both the
/// naming family and the timing window of the gate. Those fixtures now own a
/// `ScratchDb`; this gate is what keeps that true, and it still scans `_sqlx_test_%`
/// so a reintroduced `#[sqlx::test]` cannot leak unnoticed.
///
/// The child's PID scopes the probes, so a concurrent harness is never blamed.
#[tokio::test]
async fn a_failing_child_test_process_leaves_no_residue_anywhere() {
    let admin_url = crate::pg_test_support::require_live_url();
    let exe = std::env::current_exe().expect("test exe path");

    let child = std::process::Command::new(&exe)
        .args([
            "--exact",
            "rev087_migration_integrity_pg_tests::f04_failure_probe_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SWI_F04_FAIL_PROBE", "1")
        .env("TEST_DATABASE_URL", &admin_url)
        .env("DATABASE_URL", &admin_url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the failing child test process");
    let child_pid = child.id().to_string();
    let out = child.wait_with_output().expect("await the child test process");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "the child must actually FAIL, or this gate proves nothing: {combined}"
    );
    assert!(
        combined.contains("SWI_F04_FAIL_PROBE"),
        "the child must have failed in the probe, not before reaching it: {combined}"
    );
    assert!(
        combined.contains("1 failed"),
        "the child must report exactly the probe failure: {combined}"
    );

    // The child is GONE. Anything it created that still exists is a leak by
    // definition — no timing window, no in-band measurement.
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("admin pool");
    let residue: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database \
          WHERE (datname LIKE 'swi%' OR datname LIKE '\\_sqlx\\_test%') \
            AND datname LIKE '%' || $1 || '%' ORDER BY datname",
    )
    .bind(&child_pid)
    .fetch_all(&admin)
    .await
    .expect("child residue probe");
    // `_sqlx_test_*` names are randomized rather than PID-tagged, so they are also
    // counted globally: this suite creates none, so any at all is a regression.
    let sqlx_owned: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database WHERE datname LIKE '\\_sqlx\\_test%' ORDER BY datname",
    )
    .fetch_all(&admin)
    .await
    .expect("sqlx residue probe");
    admin.close().await;
    assert!(
        residue.is_empty(),
        "the failing child left these databases behind: {residue:?}"
    );
    assert!(
        sqlx_owned.is_empty(),
        "no `#[sqlx::test]` scratch database may exist after this suite: {sqlx_owned:?}"
    );

    let dir_residue: Vec<String> = std::fs::read_dir(std::env::temp_dir())
        .expect("read temp dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("swi") && n.contains(&child_pid))
        .collect();
    assert!(
        dir_residue.is_empty(),
        "the failing child left these temp directories behind: {dir_residue:?}"
    );
}

// ---------------------------------------------------------------------------
// REV-096-F02 — 1041's constraint guards are table-qualified and definition-exact
// ---------------------------------------------------------------------------

/// A same-named constraint on ANOTHER table must not satisfy 1041's guard.
///
/// `conname` is not unique across a database. The guard used to look the name up
/// without `conrelid`, so an unrelated table carrying `workspaces_id_positive_check`
/// made 1041 skip installing the invariant on the table that actually needs it —
/// and `safe_int8_predicate` would then be relying on a rule nothing enforced.
#[tokio::test]
async fn a_same_named_constraint_on_another_table_does_not_satisfy_1041() {
    let (pool, admin, scratch, dir) = scratch_before("f02qual", "1041_").await;

    // A decoy table carrying BOTH constraint names 1041 looks for.
    sqlx::query("CREATE TABLE public.r97_decoy (id bigint)")
        .execute(&pool)
        .await
        .expect("decoy table");
    sqlx::query(
        "ALTER TABLE public.r97_decoy \
           ADD CONSTRAINT workspaces_id_positive_check CHECK (id > 0)",
    )
    .execute(&pool)
    .await
    .expect("decoy constraint a");
    sqlx::query(
        "ALTER TABLE public.r97_decoy \
           ADD CONSTRAINT funding_radar_cases_id_positive_check CHECK (id > 0)",
    )
    .execute(&pool)
    .await
    .expect("decoy constraint b");

    // Precondition: an unqualified lookup WOULD be satisfied by the decoys.
    let unqualified: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conname = 'workspaces_id_positive_check'",
    )
    .fetch_one(&pool)
    .await
    .expect("unqualified probe");
    assert_eq!(unqualified, 1, "exactly the decoy exists before 1041 runs");

    crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect("1041 must still install the invariant on the real tables");

    for (table, constraint) in [
        ("workspaces", "workspaces_id_positive_check"),
        ("funding_radar_cases", "funding_radar_cases_id_positive_check"),
    ] {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = $1 \
               AND conrelid = ($2::text)::regclass AND contype = 'c' AND convalidated)",
        )
        .bind(constraint)
        .bind(format!("public.{table}"))
        .fetch_one(&pool)
        .await
        .expect("qualified probe");
        assert!(
            present,
            "{constraint} must exist ON public.{table}; a decoy of the same name on \
             another table must not satisfy the guard (REV-096-F02)"
        );
    }

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// A constraint of the right NAME on the right TABLE but with the wrong definition
/// must fail closed, even when that definition mentions the expected column.
///
/// REV-098-F01: the refusal now comes from forward migration 1042. Shipped 1041
/// looks the name up unqualified and checks EXISTENCE only, so it passes silently
/// here — which is the defect. 1041 is applied and immutable, so the guard belongs
/// in the forward file, and this fixture drives the whole bundle.
#[tokio::test]
async fn a_wrong_but_plausible_1041_constraint_definition_fails_closed() {
    let (pool, admin, scratch, dir) = scratch_before("f02def", "1041_").await;

    // `id >= 0` admits zero: it contains the same column and looks right, but it is
    // NOT the invariant safe_int8_predicate relies on.
    sqlx::query(
        "ALTER TABLE public.workspaces \
           ADD CONSTRAINT workspaces_id_positive_check CHECK (id >= 0)",
    )
    .execute(&pool)
    .await
    .expect("plant a wrong-but-plausible constraint");

    let err = crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect_err("1041 must refuse a constraint whose definition is not id > 0");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unexpected definition"),
        "expected the named definition refusal, got: {msg}"
    );

    // Fail CLOSED: the wrong constraint is untouched and the cutover was not recorded.
    let still: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conname = 'workspaces_id_positive_check' \
            AND conrelid = 'public.workspaces'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("constraint still present");
    assert!(still.contains(">="), "the operator's constraint must be left as it was");
    let cutover: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM schema_cutover_events \
          WHERE cutover = 'positive_id_invariant_repair'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    assert_eq!(cutover, 0, "no cutover may be recorded for a migration that aborted");
    let repaired: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._migrations \
          WHERE name = '1042_rev098_positive_id_invariant_repair.sql'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    assert_eq!(repaired, 0, "the aborted migration must not be recorded as applied");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-096-F05 — dual-table invalid IDs: both diagnostics, no partial state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_ids_in_both_tables_are_both_reported_with_no_partial_state() {
    let (pool, admin, scratch, dir) = scratch_before("f05dual", "1041_").await;

    sqlx::query("INSERT INTO workspaces (id, name, slug) OVERRIDING SYSTEM VALUE VALUES (0, 'w', $1)")
        .bind(tag("r97dualws"))
        .execute(&pool)
        .await
        .expect("pre-1041 lane accepts id 0");
    let recipient = tag("R97DUALRECIP");
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("wallet");
    sqlx::query(
        "INSERT INTO funding_radar_cases \
             (id, chain, recipient, workspace_id, first_funded_at, first_funding_usd, \
              first_funding_native, source_address, deploy_window_ends_at, stage, \
              confidence, evidence) \
         VALUES (0, 'solana', $1, (SELECT id FROM workspaces WHERE id = 0), now(), '1', '1', \
                 'SRC', now() + interval '1 day', 'funded', 50, '{}'::jsonb)",
    )
    .bind(&recipient)
    .execute(&pool)
    .await
    .expect("pre-1041 lane accepts case id 0");

    let err = crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect_err("1041 must refuse to install the invariant over violating data");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("positive-ID invariant violated"),
        "expected the named diagnostic, got: {msg}"
    );
    // BOTH tables must be named, not just the first one checked.
    assert!(
        msg.contains("workspaces.id: 0"),
        "the workspaces violation must be reported: {msg}"
    );
    assert!(
        msg.contains("funding_radar_cases.id: 0"),
        "the funding_radar_cases violation must be reported too (REV-096-F05): {msg}"
    );

    // No partial state: neither constraint installed, no cutover row, rows untouched.
    let constraints: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conname IN \
           ('workspaces_id_positive_check', 'funding_radar_cases_id_positive_check')",
    )
    .fetch_one(&pool)
    .await
    .expect("constraint count");
    assert_eq!(constraints, 0, "no constraint may be installed while data violates it");
    let cutover: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM schema_cutover_events WHERE cutover = 'positive_id_invariant'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    assert_eq!(cutover, 0, "no cutover may be recorded for an aborted migration");
    let survivors: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM workspaces WHERE id <= 0) \
              + (SELECT count(*) FROM funding_radar_cases WHERE id <= 0)",
    )
    .fetch_one(&pool)
    .await
    .expect("survivor count");
    assert_eq!(survivors, 2, "both offending rows must be left exactly as they were");

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

// ---------------------------------------------------------------------------
// REV-096-F03 — a forged ledger CHECK that merely CONTAINS the two words
// ---------------------------------------------------------------------------

/// `digest_origin IN ('applied','baseline','forged')` contains both expected words
/// and is therefore accepted by a substring test, while admitting a third provenance
/// the ledger's trust model does not define.
#[tokio::test]
async fn a_forged_ledger_check_containing_both_values_is_refused() {
    let (pool, admin, scratch, dir) = scratch_before("f03forge", "9999_").await;

    sqlx::query("ALTER TABLE public._migrations DROP CONSTRAINT _migrations_digest_origin_check")
        .execute(&pool)
        .await
        .expect("drop the real constraint");
    sqlx::query(
        "ALTER TABLE public._migrations ADD CONSTRAINT _migrations_digest_origin_check \
           CHECK (digest_origin IS NULL OR digest_origin IN ('applied', 'baseline', 'forged'))",
    )
    .execute(&pool)
    .await
    .expect("plant the forged constraint");

    // Precondition: a substring test WOULD accept this.
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conname = '_migrations_digest_origin_check' \
            AND conrelid = 'public._migrations'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("forged def");
    assert!(
        def.contains("applied") && def.contains("baseline"),
        "the forged constraint must contain both words, or this proves nothing"
    );

    let err = crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect_err("a widened provenance CHECK must be refused");
    assert!(
        format!("{err:#}").contains("is not the canonical"),
        "expected the canonical-expression refusal, got: {err:#}"
    );

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// REV-098-F02: a forged CHECK that answers every finite probe correctly and STILL
/// widens the admitted set must be refused before any migration work happens.
///
/// `canonical OR digest_origin = 'rogue'` is the exact bypass the reviewer
/// reproduced: `NULL`, `applied`, `baseline` are admitted (correct); `forged`, `''`
/// and `APPLIED` are rejected (correct) — and `rogue` is admitted, which no probe
/// set that did not already guess the word `rogue` can see. The migrator must not
/// depend on having guessed.
#[tokio::test]
async fn a_forged_ledger_check_with_an_unprobed_extra_value_is_refused() {
    let (pool, admin, scratch, dir) = scratch_before("f02rogue", "9999_").await;

    sqlx::query("ALTER TABLE public._migrations DROP CONSTRAINT _migrations_digest_origin_check")
        .execute(&pool)
        .await
        .expect("drop the real constraint");
    sqlx::query(
        "ALTER TABLE public._migrations ADD CONSTRAINT _migrations_digest_origin_check \
           CHECK ((digest_origin IS NULL OR digest_origin IN ('applied', 'baseline')) \
                  OR digest_origin = 'rogue')",
    )
    .execute(&pool)
    .await
    .expect("plant the rogue-disjunct constraint");

    // Precondition A: the forgery really does admit an extra value. Without this the
    // refusal below could be about something else entirely.
    sqlx::query(
        "INSERT INTO public._migrations (name, sha256, digest_origin) \
         VALUES ('9999_probe.sql', 'x', 'rogue')",
    )
    .execute(&pool)
    .await
    .expect("the forged CHECK must admit `rogue`, or this test proves nothing");
    sqlx::query("DELETE FROM public._migrations WHERE name = '9999_probe.sql'")
        .execute(&pool)
        .await
        .expect("remove the probe row");

    // Precondition B: the REV-097 finite-probe validator would have PASSED this
    // constraint — each of its six probes gets the canonical answer.
    for (literal, want) in [
        ("NULL::text", true),
        ("'applied'::text", true),
        ("'baseline'::text", true),
        ("'forged'::text", false),
        ("''::text", false),
        ("'APPLIED'::text", false),
    ] {
        let admitted: bool = sqlx::query_scalar(&format!(
            "SELECT COALESCE((({literal} IS NULL OR {literal} IN ('applied','baseline')) \
                              OR {literal} = 'rogue'), true)"
        ))
        .fetch_one(&pool)
        .await
        .expect("probe evaluation");
        assert_eq!(
            admitted, want,
            "the forgery must answer probe {literal} exactly as the canonical CHECK does"
        );
    }

    let err = crate::db::migrate_dir_with(&pool, &migrations_dir(), true)
        .await
        .expect_err("a CHECK with an unprobed extra value must be refused");
    assert!(
        format!("{err:#}").contains("is not the canonical"),
        "expected the canonical-expression refusal, got: {err:#}"
    );

    drop_scratch(&admin, pool, &scratch, &dir).await;
}

/// Two migrators racing on a database with NOTHING applied yet.
///
/// REV-096-F03: the previous concurrency test used `scratch_before(.., "9999_")`,
/// which applies every migration BEFORE the race starts — so both callers found an
/// empty work list and the test was false-green. The real question is whether two
/// processes can apply the SAME migrations at the same time. Here the scratch
/// database is untouched, so both callers genuinely contend over the whole run.
#[tokio::test]
async fn two_concurrent_migrators_on_an_empty_database_both_succeed() {
    let scratch = crate::pg_test_support::ScratchDb::create("f03race").await;

    // Precondition: nothing is applied, so the race is over real work.
    let pool_probe = scratch.pool().await;
    let ledger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
          WHERE table_schema = 'public' AND table_name = '_migrations')",
    )
    .fetch_one(&pool_probe)
    .await
    .expect("ledger probe");
    assert!(!ledger_exists, "the race must start from an EMPTY database");
    pool_probe.close().await;

    let a = scratch.pool().await;
    let b = scratch.pool().await;
    let dir_a = migrations_dir();
    let dir_b = migrations_dir();
    let (ra, rb) = tokio::join!(
        crate::db::migrate_dir_with(&a, &dir_a, true),
        crate::db::migrate_dir_with(&b, &dir_b, true),
    );
    ra.expect("first concurrent migrator must succeed on an empty database");
    rb.expect("second concurrent migrator must succeed on an empty database (REV-096-F03)");

    let pool = scratch.pool().await;
    // Ledger state must be correct, not merely non-erroring: one row per file, each
    // recorded exactly once, none duplicated by the racing pass.
    let on_disk = std::fs::read_dir(migrations_dir())
        .expect("read migrations")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sql"))
        .count() as i64;
    let (total, distinct, applied, baseline): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT name), \
                count(*) FILTER (WHERE digest_origin = 'applied'), \
                count(*) FILTER (WHERE digest_origin = 'baseline') \
           FROM public._migrations",
    )
    .fetch_one(&pool)
    .await
    .expect("ledger fingerprint");
    assert_eq!(total, distinct, "a racing migrator must not double-record any migration");
    assert_eq!(total, on_disk, "every migration must be recorded exactly once");
    assert_eq!(applied, total, "every row must be digest-verified as applied");
    assert_eq!(baseline, 0, "a fresh race records no baseline rows");

    // And exactly one ledger CHECK survives the race.
    let checks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conname = '_migrations_digest_origin_check' \
           AND conrelid = 'public._migrations'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("constraint count");
    assert_eq!(checks, 1, "exactly one ledger CHECK must remain after the race");

    a.close().await;
    b.close().await;
    pool.close().await;
}

// ---------------------------------------------------------------------------
// REV-098-F03 — a RELOCATED binary applies only its embedded bundle
// ---------------------------------------------------------------------------

/// Copy ONLY the built binary to an isolated directory, plant every hostile
/// directory the old resolver would have preferred, and prove the real CLI applies
/// the embedded bundle regardless.
///
/// Why this shape and not the REV-097 one: that fixture built a fake crate root
/// containing a COPY of the canonical `migrations/`, and ran with the real source
/// tree still present, so "the packaged bundle won" could be satisfied by the very
/// path the finding says is not shippable (`env!("CARGO_MANIFEST_DIR")`). Here the
/// binary lives somewhere else entirely, the cwd holds a DIVERGENT `./migrations`
/// complete with its own internally valid manifest, and both legacy sibling
/// spellings exist and diverge too. The old resolver would have chosen the cwd copy
/// first; the embedded bundle has no such option.
#[tokio::test]
async fn the_relocated_binary_uses_only_its_embedded_bundle() {
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

    let root = crate::pg_test_support::ScratchDir::create("relocated");
    let bindir = root.path().join("opt").join("swi").join("bin");
    std::fs::create_dir_all(&bindir).expect("bin dir");
    let exe = bindir.join(built.file_name().expect("exe name"));
    std::fs::copy(&built, &exe).expect("relocate the binary");

    // A divergent bundle in the CWD, with a manifest that is internally valid — so
    // if it were chosen it would be ACCEPTED, and the only thing keeping it out is
    // that the binary never looks at a directory.
    let cwd = root.path().join("workdir");
    let hostile = cwd.join("migrations");
    std::fs::create_dir_all(&hostile).expect("hostile dir");
    let hostile_sql = "-- HOSTILE CWD BUNDLE: this must never be applied\n\
                       CREATE TABLE public._hostile_cwd_marker (id int);\n";
    std::fs::write(hostile.join("0001_hostile_cwd.sql"), hostile_sql).expect("hostile sql");
    std::fs::write(
        hostile.join(crate::db::MIGRATION_MANIFEST),
        format!(
            "{}  0001_hostile_cwd.sql\r\n",
            crate::db::migration_sha256_for_tests(hostile_sql)
        ),
    )
    .expect("hostile manifest");

    // Both legacy sibling spellings the old resolver knew: cwd-relative and
    // binary-relative.
    for sibling in [
        root.path().join("swi-deploy").join("migrations"),
        bindir.join("..").join("..").join("swi-deploy").join("migrations"),
    ] {
        std::fs::create_dir_all(&sibling).expect("sibling dir");
        std::fs::write(
            sibling.join("9998_divergent_sibling.sql"),
            "-- DIVERGENT LEGACY SIBLING: this must never be chosen\nSELECT 1;\n",
        )
        .expect("write sibling");
    }

    let scratch = crate::pg_test_support::ScratchDb::create("f03reloc").await;
    let out = std::process::Command::new(&exe)
        .args(["db", "migrate", "--accept-legacy-baseline"])
        .current_dir(&cwd)
        .env("DATABASE_URL", scratch.scratch_url())
        .env("MIGRATION_DATABASE_URL", scratch.scratch_url())
        .env_remove("SWI_MIGRATIONS_DIR")
        .output()
        .expect("spawn the relocated migrator");
    assert!(
        out.status.success(),
        "relocated db migrate failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let pool = scratch.pool().await;
    let canonical = std::fs::read_dir(migrations_dir())
        .expect("read canonical")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sql"))
        .count() as i64;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM public._migrations")
        .fetch_one(&pool)
        .await
        .expect("ledger count");
    assert_eq!(
        applied, canonical,
        "the relocated binary must apply its embedded bundle ({canonical} migrations)"
    );
    let intruders: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._migrations \
          WHERE name IN ('0001_hostile_cwd.sql', '9998_divergent_sibling.sql')",
    )
    .fetch_one(&pool)
    .await
    .expect("intruder probe");
    assert_eq!(intruders, 0, "no file from a planted directory may have been applied");
    let hostile_table: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
          WHERE table_schema = 'public' AND table_name = '_hostile_cwd_marker')",
    )
    .fetch_one(&pool)
    .await
    .expect("hostile table probe");
    assert!(!hostile_table, "the hostile cwd bundle must not have executed");
    let invariant: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_constraint \
           WHERE conname = 'workspaces_id_positive_check' \
             AND conrelid = 'public.workspaces'::regclass)",
    )
    .fetch_one(&pool)
    .await
    .expect("invariant probe");
    assert!(invariant, "the applied bundle must be the canonical one");

    // And the explicit operator override — the ONE thing that may displace the
    // embedded bundle — is still honoured, so this is precedence, not a hard-wire.
    let scratch2 = crate::pg_test_support::ScratchDb::create("f03override").await;
    let out2 = std::process::Command::new(&exe)
        .args(["db", "migrate", "--accept-legacy-baseline"])
        .current_dir(&cwd)
        .env("DATABASE_URL", scratch2.scratch_url())
        .env("MIGRATION_DATABASE_URL", scratch2.scratch_url())
        .env("SWI_MIGRATIONS_DIR", &hostile)
        .output()
        .expect("spawn the overridden migrator");
    assert!(
        out2.status.success(),
        "overridden db migrate failed: {}{}",
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
        "SWI_MIGRATIONS_DIR must remain the explicit operator override"
    );

    pool.close().await;
    pool2.close().await;
}
