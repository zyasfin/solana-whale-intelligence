//! REV-043-F01: system-catalog queries must identify objects by TYPE, never by name
//! alone.
//!
//! WHY THIS FILE EXISTS AT ALL.
//!
//! My 1029 guard matched constraints with `conname LIKE '%lifecycle%'`. On
//! PostgreSQL 18 that broke fresh installs: PG18 records NOT NULL constraints in
//! `pg_constraint` with `contype = 'n'`, so the legitimate metadata row
//! `strategy_versions_lifecycle_state_not_null` matched the pattern and the guard
//! aborted migration after 1000..1028 had already applied.
//!
//! I could not reproduce it: PostgreSQL 17.11 does not record NOT NULL in
//! `pg_constraint` at all, so the query returns nothing there. Every cycle I have
//! written "PostgreSQL 17.11, not 18" in the ledger as a known weakness — and then
//! shipped a query whose correctness depends on exactly that difference.
//!
//! So the fix is not only `contype = 'c'` in one place. A live probe cannot catch
//! this class on my machine, and a live probe on the REVIEWER's machine catches it
//! only after it ships. What CAN catch it here is a static property of the SQL:
//! a catalog query that filters on a name without also filtering on an object type
//! is making a version-dependent assumption. That is checkable offline, on any
//! PostgreSQL version, including versions that do not exist yet.
//!
//! This test therefore encodes the LESSON rather than the instance.

use std::path::PathBuf;

fn migrations_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sibling = manifest
        .parent()
        .expect("crate has a parent dir")
        .join("swi-deploy")
        .join("migrations");
    assert!(
        sibling.is_dir(),
        "canonical migrations dir not found at {}",
        sibling.display()
    );
    sibling
}

fn migration_sql() -> Vec<(String, String)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(migrations_dir())
        .expect("read migrations dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "sql").unwrap_or(false))
        .collect();
    files.sort();
    files
        .iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_str().unwrap().to_string(),
                std::fs::read_to_string(p).expect("read migration"),
            )
        })
        .collect()
}

/// SQL with `--` line comments removed, so documentation discussing a bad pattern
/// does not count as using it.
fn code_only(sql: &str) -> String {
    sql.lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Catalogs whose row set has changed (or may change) between PostgreSQL versions,
/// mapped to the column that identifies the KIND of object in that catalog.
fn catalog_type_columns() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        // PG18 added contype='n' rows for NOT NULL. This is the one that bit me.
        ("pg_constraint", vec!["contype"]),
        ("pg_class", vec!["relkind"]),
        ("pg_proc", vec!["prokind", "proname ="]),
        (
            "pg_index",
            vec!["indisunique", "indisprimary", "indisvalid", "indexrelid ="],
        ),
        ("pg_trigger", vec!["tgisinternal", "tgname ="]),
    ]
}

/// Column references that mean "this query selects by NAME".
const NAME_PREDICATES: [&str; 5] = ["conname", "relname", "proname", "tgname", "indexrelid"];

// A catalog query that matches names by PATTERN (LIKE / ~ / SIMILAR TO) without a
// type filter is version-dependent by construction: any future catalog row whose
// name happens to match will be swept in. An EXACT name match is a different case,
// handled by the test below.
#[test]
fn catalog_pattern_matches_must_filter_by_object_type() {
    let mut offenders: Vec<String> = Vec::new();

    for (file, sql) in migration_sql() {
        let code = code_only(&sql).to_ascii_lowercase();
        for (catalog, type_cols) in catalog_type_columns() {
            let mut from = 0usize;
            while let Some(idx) = code[from..].find(catalog) {
                let start = from + idx;
                // Look at the statement region following the catalog reference.
                let end = code[start..]
                    .find(';')
                    .map(|e| start + e)
                    .unwrap_or(code.len());
                let region = &code[start..end];
                from = start + catalog.len();

                let pattern_match = region.contains(" like ")
                    || region.contains(" similar to ")
                    || region.contains(" ~ ")
                    || region.contains("~~");
                if !pattern_match {
                    continue;
                }
                let names_something = NAME_PREDICATES.iter().any(|p| region.contains(p));
                if !names_something {
                    continue;
                }
                let has_type_filter = type_cols.iter().any(|c| region.contains(c));
                if !has_type_filter {
                    offenders.push(format!(
                        "{file}: pattern-matches a name in `{catalog}` with no {} filter",
                        type_cols[0]
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a catalog query that pattern-matches names without filtering the object type \
         will sweep in rows that a future PostgreSQL version adds. PG18 added \
         `pg_constraint.contype = 'n'` for NOT NULL and broke migration 1029 exactly \
         this way (REV-043-F01). Offenders: {offenders:#?}"
    );
}

// The specific reconciliation that failed: it must select CHECK constraints on a
// named COLUMN, so neither a renamed constraint nor future catalog metadata can
// change its meaning.
#[test]
fn lifecycle_reconciliation_selects_check_constraints_on_the_column() {
    let sql = std::fs::read_to_string(
        migrations_dir().join("1029_rev041_reconcile_renamed_constraints.sql"),
    )
    .expect("read 1029");
    let code = code_only(&sql);

    // EVERY query against pg_constraint in this file must carry both restrictions.
    //
    // My first version asserted `count >= 2`, which passed when I deliberately
    // removed the filter from one of the two sites — because a third occurrence
    // elsewhere kept the total above the threshold. An aggregate count cannot
    // express "each one", which is the very failure mode this test exists to catch.
    // Each query region is now checked individually.
    let mut unguarded: Vec<String> = Vec::new();
    let lower = code.to_ascii_lowercase();
    let mut from = 0usize;
    let mut regions = 0usize;
    while let Some(idx) = lower[from..].find("from pg_constraint") {
        let start = from + idx;
        let end = lower[start..]
            .find(';')
            .map(|e| start + e)
            .unwrap_or(lower.len());
        let region = &lower[start..end];
        from = start + "from pg_constraint".len();
        regions += 1;

        if !region.contains("contype = 'c'") {
            unguarded.push(format!(
                "region {regions}: queries pg_constraint without `contype = 'c'`"
            ));
        }
        if !region.contains("attname = 'lifecycle_state'") && !region.contains("conname =") {
            unguarded.push(format!(
                "region {regions}: neither restricted to the lifecycle_state column nor \
                 to one exact constraint name"
            ));
        }
    }
    assert!(
        regions >= 2,
        "expected the reconciliation loop and the assertion guard to both query \
         pg_constraint; found {regions} region(s)"
    );
    assert!(
        unguarded.is_empty(),
        "every pg_constraint query must restrict to CHECK constraints on a known \
         column (or one exact name). Leaving one site unguarded is how PostgreSQL 18 \
         broke: {unguarded:#?}"
    );

    // Name-based discovery must be gone from the executable SQL.
    assert!(
        !code.contains("conname LIKE '%lifecycle%'"),
        "discovery by name pattern is what broke PostgreSQL 18"
    );
    // And the canonical constraint's DEFINITION must be asserted, not just its name:
    // a future edit could keep the name and narrow the permitted set, which is the
    // original 1013 failure mode.
    assert!(
        code.contains("pg_get_constraintdef"),
        "the guard must assert what the canonical constraint MEANS, not only that a \
         constraint with that name exists"
    );
}

// PG18's NOT NULL metadata name is derivable: `<table>_<column>_not_null`. For every
// column this repository declares NOT NULL, assert no catalog query could mistake
// that generated name for one of our own objects.
//
// This is the version-independent stand-in for the live PG18 probe I cannot run.
#[test]
fn generated_not_null_names_cannot_collide_with_our_patterns() {
    // Columns declared NOT NULL, as `(table, column)`.
    let mut declared: Vec<(String, String)> = Vec::new();
    for (_, sql) in migration_sql() {
        let code = code_only(&sql);
        let mut current_table: Option<String> = None;
        for line in code.lines() {
            let t = line.trim();
            let lower = t.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("create table") {
                let rest = rest.replace("if not exists", "");
                if let Some(name) = rest.split_whitespace().next() {
                    current_table = Some(
                        name.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                            .to_string(),
                    );
                }
                continue;
            }
            if t.starts_with(')') {
                current_table = None;
                continue;
            }
            if !lower.contains("not null") {
                continue;
            }
            let Some(table) = current_table.as_ref() else { continue };
            if let Some(col) = t.split_whitespace().next() {
                let col = col.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                if !col.is_empty() && !table.is_empty() {
                    declared.push((table.clone(), col.to_string()));
                }
            }
        }
    }
    assert!(
        !declared.is_empty(),
        "the NOT NULL scan found nothing; the parser is broken and this test would \
         pass vacuously"
    );

    // Every pattern this repository applies to a catalog NAME column.
    //
    // The left-hand side matters. A `LIKE '%canary%'` applied to
    // `pg_get_constraintdef(...)` inspects a constraint's DEFINITION and cannot be
    // confused by a generated name — my first version of this test flagged exactly
    // that and would have been a guard that cries wolf, which is a guard that gets
    // deleted. Only patterns whose left-hand side is a name column count.
    let mut patterns: Vec<(String, String)> = Vec::new();
    for (file, sql) in migration_sql() {
        let code = code_only(&sql);
        let lower = code.to_ascii_lowercase();
        let mut from = 0usize;
        while let Some(idx) = lower[from..].find("like '") {
            let at = from + idx;
            from = at + 6;
            let Some(pat) = code[at + 6..].split('\'').next() else { continue };
            if !pat.contains('%') {
                continue;
            }
            // Inspect the ~80 characters before `LIKE` for the operand.
            let lhs_start = at.saturating_sub(80);
            let lhs = &lower[lhs_start..at];
            let is_name_operand = NAME_PREDICATES.iter().any(|p| lhs.contains(p));
            if is_name_operand {
                patterns.push((file.clone(), pat.to_string()));
            }
        }
    }

    let mut collisions: Vec<String> = Vec::new();
    for (table, column) in &declared {
        // PostgreSQL 18 generates this name for NOT NULL metadata.
        let generated = format!("{table}_{column}_not_null");
        for (file, pat) in &patterns {
            let core = pat.trim_matches('%');
            if core.is_empty() {
                continue;
            }
            if generated.contains(core) {
                collisions.push(format!(
                    "{file}: pattern `{pat}` matches PG18-generated `{generated}`"
                ));
            }
        }
    }

    assert!(
        collisions.is_empty(),
        "PostgreSQL 18 records NOT NULL constraints in pg_constraint as \
         `<table>_<column>_not_null` with contype='n'. These name patterns would \
         match such rows, which is how migration 1029 aborted a fresh PG18 install. \
         Either drop the pattern or add a type filter. Collisions: {collisions:#?}"
    );
}
