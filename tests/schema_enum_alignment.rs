//! REV-028 regression: SQL CHECK constraints must cover every frozen Rust enum
//! variant.
//!
//! Four enum drifts reached review because the Rust side was widened while the
//! migration CHECK constraint was not (REV-028-F02..F05):
//!   * `ComponentClass` grew to four classes; `decision_components` allowed two.
//!   * `BrowserPlatform` gained `Web`; `browser_captures` allowed x/tiktok.
//!   * `LpAction` has nine values; `autonomous_lp_cycles` allowed five.
//!
//! These tests derive the expected values from the enums themselves (via their
//! `Serialize` wire form, so a rename cannot silently pass) and assert the
//! effective CHECK constraint permits exactly that set. Adding a Rust variant
//! without a forward migration now fails the suite instead of failing at INSERT
//! time in production.

use std::collections::BTreeSet;
use std::path::PathBuf;

use solana_whale_intelligence::sf::browser::BrowserPlatform;
use solana_whale_intelligence::sf::execution::LpAction;
use solana_whale_intelligence::sf::portfolio::ComponentClass;

/// Resolve the canonical migrations directory (sibling deploy repo).
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

/// Concatenate every migration in filename order (the order the runner uses).
fn all_migrations_sql() -> String {
    let mut files: Vec<PathBuf> = std::fs::read_dir(migrations_dir())
        .expect("read migrations dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "sql").unwrap_or(false))
        .collect();
    files.sort();
    files
        .iter()
        .map(|p| std::fs::read_to_string(p).expect("read migration"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the quoted values of the LAST `CHECK (... IN (...))` that follows the
/// named constraint. "Last" matters: a later corrective migration supersedes an
/// earlier definition, and the effective constraint is the one applied last.
fn effective_check_values(sql: &str, constraint_name: &str) -> BTreeSet<String> {
    let anchor = format!("ADD CONSTRAINT {constraint_name}");
    let start = sql
        .rfind(&anchor)
        .unwrap_or_else(|| panic!("constraint {constraint_name} not found in migrations"));
    let tail = &sql[start..];
    let open = tail
        .find("IN (")
        .unwrap_or_else(|| panic!("constraint {constraint_name} has no IN (...) list"));
    let after = &tail[open + 4..];
    let close = after
        .find(')')
        .unwrap_or_else(|| panic!("constraint {constraint_name} has an unterminated IN list"));
    let list = &after[..close];

    list.split(',')
        .map(|item| item.trim().trim_matches('\'').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// The `Serialize` wire form of an enum variant — the exact string persisted.
fn wire<T: serde::Serialize>(value: T) -> String {
    match serde_json::to_value(value).expect("enum serializes") {
        serde_json::Value::String(s) => s,
        other => panic!("expected a string wire form, got {other}"),
    }
}

// REV-028-F02: all four frozen component classes must be persistable, under the
// canonical `mandatory_pass` name (not the old `mandatory`).
#[test]
fn decision_components_covers_every_component_class() {
    let expected: BTreeSet<String> = [
        ComponentClass::MandatoryPass,
        ComponentClass::SizingInput,
        ComponentClass::StrategyInput,
        ComponentClass::HaltInput,
    ]
    .into_iter()
    .map(wire)
    .collect();

    let actual = effective_check_values(
        &all_migrations_sql(),
        "decision_components_component_class_check",
    );
    assert_eq!(
        actual, expected,
        "decision_components.component_class must permit exactly the four frozen ComponentClass values"
    );
}

// REV-028-F03: BrowserPlatform::Web must be persistable.
#[test]
fn browser_captures_covers_every_platform() {
    let expected: BTreeSet<String> =
        [BrowserPlatform::X, BrowserPlatform::Tiktok, BrowserPlatform::Web]
            .into_iter()
            .map(wire)
            .collect();

    let actual =
        effective_check_values(&all_migrations_sql(), "browser_captures_platform_check");
    assert_eq!(
        actual, expected,
        "browser_captures.platform must permit exactly the frozen BrowserPlatform values"
    );
}

// REV-028-F04: all nine frozen LP actions must be persistable. CREATE_POOL /
// CREATE_TOKEN are deliberately NOT LpAction variants (§1 LP semantics), so
// deriving from the enum also proves they stay out.
#[test]
fn autonomous_lp_cycles_covers_every_lp_action() {
    let expected: BTreeSet<String> = [
        LpAction::OpenPosition,
        LpAction::AddLiquidity,
        LpAction::ClaimFees,
        LpAction::CompoundFees,
        LpAction::PartialWithdraw,
        LpAction::ClosePosition,
        LpAction::ReseedPosition,
        LpAction::SwapResiduals,
        LpAction::EmergencyExit,
    ]
    .into_iter()
    .map(wire)
    .collect();
    assert_eq!(expected.len(), 9, "LpAction has nine frozen values");

    let actual =
        effective_check_values(&all_migrations_sql(), "autonomous_lp_cycles_action_check");
    assert_eq!(
        actual, expected,
        "autonomous_lp_cycles.action must permit exactly the nine frozen LpAction values"
    );
}

// REV-028-F05: every §17 signer field modelled in Rust must have a column, so a
// full checklist is auditable.
#[test]
fn signer_checks_has_every_frozen_signer_field() {
    let sql = all_migrations_sql();
    for column in [
        "workspace_binding_valid",
        "wallet_binding_valid",
        "policy_binding_valid",
        "idempotency_binding_valid",
        "factory_allowed",
        "manager_allowed",
        "pool_verified",
        "authority_verified",
        "gas_ok",
        "priority_fee_ok",
        "tip_ok",
        "rent_ok",
        "writable_accounts_allowed",
        "approvals_bounded",
        "instructions_decoded",
        "no_unrelated_operations",
    ] {
        assert!(
            sql.contains(column),
            "signer_checks is missing the frozen §17 column `{column}`"
        );
    }
}

// REV-028-F01: the dropped `rpc_providers` table must have no live query left.
// Migration 0006 dropped it, so any remaining reference is a runtime error.
// Comment lines are ignored on purpose: the removal is *documented* in comments
// so the feature is not silently reintroduced, and documenting it must not fail
// the guard. Only executable references count.
#[test]
fn no_source_references_the_dropped_rpc_providers_table() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    visit_rs(&src, &mut |path, body| {
        for (lineno, line) in body.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue; // documentation, not a query
            }
            if line.contains("rpc_providers") {
                offenders.push(format!("{}:{}", path.display(), lineno + 1));
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "rpc_providers was dropped by migration 0006 but is still referenced in code at: {offenders:?}"
    );
}

fn visit_rs(dir: &PathBuf, f: &mut impl FnMut(&PathBuf, &str)) {
    for entry in std::fs::read_dir(dir).expect("read src dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs(&path, f);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            let body = std::fs::read_to_string(&path).expect("read source file");
            f(&path, &body);
        }
    }
}

// REV-034 (REV-033 remediation #4): `intent_action_classes` (migration 1025) is
// the DB-side mirror of the Rust EXIT_ONLY rule. Migration 1025 rejects an
// unclassified action, so a Rust action missing from the table is not a silent
// widening — it is a hard transition failure at runtime. Derive the expected rows
// from `parse_action` itself so adding a Rust action without a forward migration
// fails here instead of in production.
//
// The risk class is derived from `is_risk_reducing`/`is_lp_risk_reducing`, i.e. the
// same predicates `action_permitted` uses, so the SQL and Rust EXIT_ONLY answers
// cannot disagree.
#[test]
fn intent_action_classes_mirror_the_frozen_rust_exit_only_rule() {
    use solana_whale_intelligence::sf::execution::Action;
    use solana_whale_intelligence::sf::execution_runtime::{
        is_lp_risk_reducing, is_risk_reducing, parse_action,
    };

    // Every canonical snake_case action string the validator accepts.
    let action_strings = [
        "buy",
        "sell",
        "partial_sell",
        "close",
        "emergency_exit",
        "lp_emergency_exit",
        "open_position",
        "add_liquidity",
        "claim_fees",
        "compound_fees",
        "partial_withdraw",
        "close_position",
        "reseed_position",
        "swap_residuals",
    ];

    let sql = all_migrations_sql();
    // The INSERT block of 1025: `('action', 'risk_class')` pairs.
    let start = sql
        .rfind("INSERT INTO intent_action_classes")
        .expect("migration 1025 must seed intent_action_classes");
    let block = &sql[start..];
    let end = block
        .find("ON CONFLICT")
        .expect("the seed must be idempotent via ON CONFLICT");
    let block = &block[..end];

    for s in action_strings {
        let parsed = parse_action(s).unwrap_or_else(|| panic!("`{s}` must parse as an action"));
        let expected_class = match parsed {
            Action::Trade(t) if is_risk_reducing(t) => "reducing",
            Action::Lp(l) if is_lp_risk_reducing(l) => "reducing",
            _ => "adding",
        };
        // The row must exist AND carry the class Rust computes.
        let row_present = block
            .lines()
            .filter(|l| l.contains(&format!("'{s}'")))
            .any(|l| l.contains(&format!("'{expected_class}'")));
        assert!(
            row_present,
            "intent_action_classes must classify `{s}` as `{expected_class}` \
             (migration 1025 rejects any action it cannot classify)"
        );
    }

    // And nothing may be classified that Rust does not recognise: an unknown row
    // would let a free-text action through the EXIT_ONLY gate.
    for line in block.lines() {
        let Some(open) = line.find('(') else { continue };
        let rest = &line[open + 1..];
        let Some(q1) = rest.find('\'') else { continue };
        let after = &rest[q1 + 1..];
        let Some(q2) = after.find('\'') else { continue };
        let action = &after[..q2];
        if action.is_empty() || action == "reducing" || action == "adding" {
            continue;
        }
        assert!(
            parse_action(action).is_some(),
            "intent_action_classes carries `{action}`, which `parse_action` does not \
             recognise — the SQL and Rust action vocabularies have drifted"
        );
    }
}
