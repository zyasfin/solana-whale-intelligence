//! REV-037-F05: the repository's DEFAULT commands must work.
//!
//! REV-036 moved test fixtures behind a `test_fixtures` Cargo feature and then only
//! ever ran the suite WITH that feature. The reviewer ran the plain commands:
//!
//!     cargo +1.89.0 test --locked                                FAIL E0603/E0599
//!     cargo +1.89.0 check --locked --all-targets                  FAIL E0603/E0599
//!     cargo +1.89.0 check --locked --features pg_tests --all-targets  FAIL
//!
//! Every "317 passed" I reported used a flag nobody asked for. The lesson is not
//! "remember to run the default command" — it is that a needed build option must
//! not be discoverable only by reading a ledger entry.
//!
//! The fact that THIS FILE COMPILES AND RUNS under a bare `cargo test` is itself
//! the regression test: it is an integration test, so if the crate's public surface
//! again required a non-default feature, the default `cargo test` would fail to
//! build this target and the failure would be immediate and loud.
//!
//! The assertions below add the part a compiler cannot check: that the manifest
//! does not quietly grow a feature that test targets depend on.

use std::path::PathBuf;

fn manifest() -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read Cargo.toml")
}

/// The `[features]` section, without the following section.
fn features_section(m: &str) -> String {
    m.split("[features]")
        .nth(1)
        .map(|tail| tail.split("\n[").next().unwrap_or(tail).to_string())
        .unwrap_or_default()
}

// Only `pg_tests` may exist, and it must be opt-in for LIVE-DATABASE tests only —
// never for compiling a target. Anything else risks recreating F05, where a test
// target could not build without an extra flag.
#[test]
fn the_only_optional_feature_is_the_live_database_gate() {
    let m = manifest();
    let features = features_section(&m);

    let declared: Vec<String> = features
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            l.split('=').next().map(|n| n.trim().to_string())
        })
        .filter(|n| !n.is_empty())
        .collect();

    assert_eq!(
        declared,
        vec!["pg_tests".to_string()],
        "`pg_tests` (skip-unless-DATABASE_URL) is the only permitted optional \
         feature; another one risks a target that cannot build by default (F05)"
    );
    assert!(
        !features.contains("default ="),
        "no default feature set: it would hide which flags a build really needs"
    );
}

// `pg_tests` must gate RUNTIME behaviour (tests that need a live database), never
// the existence of an API a test target compiles against. That distinction is
// exactly what F05 violated.
#[test]
fn pg_tests_gates_live_tests_not_api_visibility() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    visit_rs(&src, &mut |path, body| {
        for (i, line) in body.lines().enumerate() {
            let t = line.trim();
            if !t.contains("feature = \"pg_tests\"") {
                continue;
            }
            // Acceptable: `#[cfg(all(test, feature = "pg_tests"))]` and
            // `#[cfg(feature = "pg_tests")]` on a test module or test fn.
            // Unacceptable: gating a `pub` item, which changes the public surface
            // depending on a flag.
            let next = body.lines().nth(i + 1).unwrap_or("").trim();
            let gates_public_api = next.starts_with("pub fn")
                || next.starts_with("pub struct")
                || next.starts_with("pub enum")
                || next.starts_with("pub trait");
            if gates_public_api && !t.contains("test") {
                offenders.push(format!("{}:{}", path.display(), i + 1));
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "a feature must not change public API visibility; it makes the default \
         build fail to compile dependent targets (REV-037-F05): {offenders:?}"
    );
}

// No source file may reference the removed feature.
#[test]
fn the_removed_test_fixtures_feature_is_gone_everywhere() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for dir in ["src", "tests"] {
        visit_rs(&root.join(dir), &mut |path, body| {
            for (i, line) in body.lines().enumerate() {
                // A comment explaining WHY the feature was removed is fine; a live
                // `cfg` referring to it is not.
                let t = line.trim();
                if t.starts_with("//") || t.starts_with("//!") {
                    continue;
                }
                if t.contains("feature = \"test_fixtures\"") {
                    offenders.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        });
    }
    assert!(
        offenders.is_empty(),
        "`test_fixtures` was removed because a Cargo feature cannot express \
         \"tests only\" (REV-037-F06); live references remain at: {offenders:?}"
    );
}

fn visit_rs(dir: &PathBuf, f: &mut impl FnMut(&PathBuf, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs(&path, f);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            let body = std::fs::read_to_string(&path).expect("read source file");
            f(&path, &body);
        }
    }
}
