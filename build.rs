//! Embed the canonical migration bundle into the binary (REV-098-F03).
//!
//! WHY THIS EXISTS
//! The migrator used to FIND its SQL on the filesystem, walking a candidate list
//! that began with the cwd-relative `./migrations`. Three things were wrong with
//! that, and REV-098 reproduced all three:
//!
//!   * any directory a operator happened to `cd` into could outrank the reviewed
//!     bundle merely by containing a `migrations/` folder;
//!   * `env!("CARGO_MANIFEST_DIR")/migrations` is the BUILD HOST's source path. It
//!     is not a shipped artifact, so a binary copied anywhere else silently lost
//!     the "packaged" candidate and fell through to a legacy sibling;
//!   * the legacy sibling `../swi-deploy/migrations` is unversioned and can drift.
//!
//! A bundle that travels INSIDE the executable cannot be outranked, cannot be left
//! behind by a copy, and cannot drift from the source that was reviewed. The only
//! remaining override is the explicit operator variable `SWI_MIGRATIONS_DIR`.
//!
//! This script only ENUMERATES; it deliberately pulls in no crates (a build
//! dependency would change `Cargo.lock` and break `--locked`). Digest and
//! manifest/file bijection are verified at runtime by
//! `db::MigrationBundle::embedded`, and by the unit tests that call it.

use std::fmt::Write as _;

fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dir = std::path::Path::new(&crate_dir).join("migrations");

    // Rebuild when any migration or the manifest changes. Without this a new
    // migration file would not appear in an incremental build — the binary would
    // silently ship yesterday's schema.
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .map(|e| e.expect("migration dir entry"))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no .sql files in {}", dir.display());

    let manifest = dir.join("MANIFEST.sha256");
    assert!(
        manifest.is_file(),
        "the reviewed digest manifest is missing from {}",
        dir.display()
    );

    let mut out = String::new();
    out.push_str(
        "/// The reviewed digest manifest, embedded verbatim.\n\
         pub(crate) const EMBEDDED_MANIFEST: &str = include_str!(r\"",
    );
    out.push_str(&manifest.display().to_string());
    out.push_str("\");\n\n");
    out.push_str(
        "/// Every canonical migration, in filename order, embedded verbatim.\n\
         pub(crate) const EMBEDDED_MIGRATIONS: &[(&str, &str)] = &[\n",
    );
    for name in &names {
        // `include_str!` re-reads at compile time, so the bytes in the binary are
        // exactly the bytes on disk at build time — no copy step to get wrong.
        writeln!(
            out,
            "    ({name:?}, include_str!(r\"{}\")),",
            dir.join(name).display()
        )
        .expect("format embedded entry");
        println!("cargo:rerun-if-changed={}", dir.join(name).display());
    }
    out.push_str("];\n");
    println!("cargo:rerun-if-changed={}", manifest.display());

    let dest = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("embedded_migrations.rs");
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
}
