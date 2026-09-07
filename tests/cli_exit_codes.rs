//! REV-045-F01/F02: operational commands must report failure through their EXIT
//! CODE, not only in their output.
//!
//! `db status` printed `schema NOT current: ...` and exited 0. Every consumer that
//! reads an exit code — systemd, CI, cron, a health probe, a deployment gate — would
//! have treated that as success. I added the command so the baseline boundary could
//! be inspected, then made it unreadable to the machines doing the inspecting.
//!
//! These tests assert the process exit code. Asserting on stdout would have passed
//! for the broken version, which is exactly why the reviewer asked for exit-code
//! coverage specifically.
//!
//! Live-database cases are skipped (not failed) when no disposable database is
//! reachable, so offline runs stay green.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    // The integration test binary lives next to the built CLI.
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop(); // deps/
    p.pop(); // debug/
    let name = if cfg!(windows) {
        "solana-whale-intelligence.exe"
    } else {
        "solana-whale-intelligence"
    };
    p.join(name)
}

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

/// Run the CLI with an explicit `DATABASE_URL`, returning (exit code, stdout+stderr).
fn run(url: &str, args: &[&str]) -> (Option<i32>, String) {
    let exe = bin();
    if !exe.exists() {
        return (None, String::new());
    }
    let out = Command::new(&exe)
        .args(args)
        .env("DATABASE_URL", url)
        .env("MIGRATION_DATABASE_URL", url)
        .output()
        .expect("spawn CLI");
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code(), text)
}

// An unreachable database is a failure, and it must be signalled as one. This case
// needs no live server, so it always runs.
#[test]
fn db_status_exits_non_zero_when_the_database_is_unreachable() {
    let exe = bin();
    if !exe.exists() {
        eprintln!("skipping: CLI binary not built at {}", exe.display());
        return;
    }
    // Port 1 is reserved and never listening.
    let (code, output) = run(
        "postgres://nobody:nobody@127.0.0.1:1/does_not_exist",
        &["db", "status"],
    );
    assert_ne!(
        code,
        Some(0),
        "`db status` must not exit 0 when it cannot reach the database; output was: {output}"
    );
}

// The reported case: a ledger that does not match the files on disk. `db status` must
// exit non-zero, and the ORIGINAL cause must still be visible.
#[test]
fn db_status_exits_non_zero_when_the_schema_is_not_current() {
    let Some(url) = live_database_url() else {
        eprintln!("skipping live status gate: no TEST_DATABASE_URL/DATABASE_URL");
        return;
    };
    let exe = bin();
    if !exe.exists() {
        eprintln!("skipping: CLI binary not built");
        return;
    }

    // A database with no ledger at all is "not current": nothing has been applied.
    // This avoids mutating a real ledger while still exercising the failure path.
    let (code, output) = run(&url, &["db", "status"]);
    if code == Some(0) {
        // The database IS current — then the assertion below is vacuous, so state
        // that rather than pretending the test proved something.
        assert!(
            output.contains("schema current") || output.contains("ACCEPTED BASELINE"),
            "exit 0 must mean a reported-good state, got: {output}"
        );
        eprintln!("note: live database is current; failure path covered by the unreachable case");
        return;
    }
    assert_ne!(code, Some(0), "a non-current schema must exit non-zero");
    assert!(
        output.contains("schema NOT current")
            || output.contains("pending")
            || output.contains("provenance")
            || output.contains("no longer match")
            || output.contains("no migration ledger")
            || output.contains("_migrations")
            || output.contains("db migrate"),
        "the original cause must remain visible, got: {output}"
    );
}

// A baseline is an accepted state, not a failure: it must exit 0 while still saying
// plainly that the rows are not verified. Getting this backwards would make operators
// unable to run at all on a legitimately upgraded database.
#[test]
fn db_status_exits_zero_for_an_accepted_baseline_but_says_so() {
    let Some(url) = live_database_url() else {
        eprintln!("skipping live baseline gate: no TEST_DATABASE_URL/DATABASE_URL");
        return;
    };
    let exe = bin();
    if !exe.exists() {
        eprintln!("skipping: CLI binary not built");
        return;
    }
    let (code, output) = run(&url, &["db", "status"]);
    if output.contains("ACCEPTED BASELINE") {
        assert_eq!(
            code,
            Some(0),
            "an accepted baseline is not an outage and must exit 0, got: {output}"
        );
        assert!(
            output.contains("not verified"),
            "a baseline must never be described as verified, got: {output}"
        );
    } else {
        eprintln!("note: live database has no baseline rows; nothing to assert here");
    }
}

// Source-level guard for the class: an authoritative schema/security query must not
// convert an error into an empty answer, because "empty" is the shape that means
// "nothing wrong".
#[test]
fn authoritative_ledger_queries_do_not_swallow_errors() {
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");

    let mut offenders: Vec<String> = Vec::new();
    let lines: Vec<&str> = db.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let swallows = t.contains("unwrap_or_default()")
            || t.contains("unwrap_or(false)")
            || t.contains("unwrap_or(0)");
        if !swallows {
            continue;
        }
        // Only care when the swallowed value came from a database round-trip.
        let window = lines[i.saturating_sub(6)..i].join(" ");
        if window.contains(".await") || window.contains("fetch_") {
            offenders.push(format!("db.rs:{}: {}", i + 1, t));
        }
    }

    assert!(
        offenders.is_empty(),
        "an authoritative schema query must propagate its error; an empty result is \
         indistinguishable from `verified` (REV-045-F02): {offenders:#?}"
    );
}

// And `db status` must not report a failure through stdout alone.
#[test]
fn db_status_propagates_errors_rather_than_printing_them() {
    let main = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("main.rs"),
    )
    .expect("read main.rs");

    assert!(
        !main.contains(r#"Err(e) => println!("schema NOT current: {e}")"#),
        "printing the failure and falling through exits 0; automation reads that as success"
    );
    assert!(
        main.contains(r#".context("schema NOT current")?"#),
        "the schema check must propagate so the exit code reflects the failure"
    );
    assert!(
        main.contains(r#".context("schema status unavailable")?"#),
        "an unavailable baseline query must fail the command, not read as `no baseline`"
    );
}
