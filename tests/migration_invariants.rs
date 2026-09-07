//! REV-030 regression: structural invariants of the canonical migration set.
//!
//! These are static guards over the shipped SQL. They cannot replace a live
//! PostgreSQL replay, but they pin the properties that were repeatedly lost
//! across REV-019 → REV-022 → REV-029, each time re-introduced by an edit that
//! *looked* correct in isolation:
//!
//!   * the intent-status authority must not be anything a caller can produce
//!     (a session GUC in 1019, an insertable audit row in 1022);
//!   * frozen reservation semantics must match `sf::intent::reservation_action`;
//!   * the legacy and canonical lanes must not both create the same table;
//!   * migration filenames must sort into the intended lane order.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use solana_whale_intelligence::sf::intent::{reservation_action, IntentState, ReservationAction};

fn migrations_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has a parent dir")
        .join("swi-deploy")
        .join("migrations");
    assert!(dir.is_dir(), "migrations dir not found at {}", dir.display());
    dir
}

/// Migration file names in the exact order the runner applies them.
fn migration_files() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(migrations_dir())
        .expect("read migrations dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    names.sort();
    names
}

fn read_migration(name: &str) -> String {
    std::fs::read_to_string(migrations_dir().join(name)).expect("read migration")
}

fn all_sql() -> String {
    migration_files()
        .iter()
        .map(|n| read_migration(n))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tables created by a set of migrations, mapped to the file that creates them.
fn created_tables(files: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in files {
        let sql = read_migration(name);
        for line in sql.lines() {
            let t = line.trim_start();
            if t.starts_with("--") {
                continue;
            }
            let lower = t.to_ascii_lowercase();
            let Some(rest) = lower.strip_prefix("create table ") else {
                continue;
            };
            let rest = rest.trim_start_matches("if not exists ").trim();
            let table: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !table.is_empty() {
                out.entry(table).or_insert_with(|| name.clone());
            }
        }
    }
    out
}

// REV-029/REV-025-F05: the authority for a status change must be an identity the
// caller cannot assume. Two previous attempts failed because the authority was
// caller-producible; both anti-patterns are pinned here.
#[test]
fn intent_status_authority_is_not_caller_forgeable() {
    let sql = all_sql();

    // The final guard must key on `current_user`, i.e. the SECURITY DEFINER owner.
    let guard_start = sql
        .rfind("FUNCTION guard_intent_status()")
        .expect("guard_intent_status must exist");
    let guard = &sql[guard_start..];
    let guard_body_end = guard.find("$$ LANGUAGE plpgsql").unwrap_or(guard.len());
    let guard_body = &guard[..guard_body_end];

    assert!(
        guard_body.contains("current_user"),
        "the guard must authorize on current_user (the definer role), not on caller-supplied state"
    );

    // Anti-pattern 1 (1019): a settable session GUC.
    assert!(
        !guard_body.contains("app.transition_intent"),
        "the guard must not read a caller-settable GUC"
    );

    // Anti-pattern 2 (1022): an audit row the caller can insert itself.
    assert!(
        !guard_body.contains("pg_current_xact_id"),
        "the guard must not treat a same-transaction audit row as authority; a caller can insert one"
    );

    // The transition function must be SECURITY DEFINER with a pinned search_path
    // and owned by a role nobody can enter.
    // Anchor on the last DEFINITION, not on a later ALTER/GRANT that also names
    // the function.
    let fn_start = sql
        .rfind("CREATE OR REPLACE FUNCTION transition_intent(")
        .expect("transition_intent must be defined");
    let definition_end = sql[fn_start..]
        .find("\nALTER FUNCTION")
        .map(|i| fn_start + i)
        .unwrap_or(sql.len());
    let definer = &sql[fn_start..definition_end];
    assert!(
        definer.contains("SECURITY DEFINER"),
        "transition_intent must be SECURITY DEFINER"
    );
    assert!(
        definer.contains("SET search_path"),
        "transition_intent must pin search_path"
    );
    assert!(
        sql.contains("OWNER TO swi_transition_owner"),
        "transition_intent must be owned by the dedicated authority role"
    );
    assert!(
        sql.contains("CREATE ROLE swi_transition_owner NOLOGIN"),
        "the authority role must be NOLOGIN so it cannot be logged into"
    );

    // The caller must lose both halves of the forged path.
    assert!(
        sql.contains("REVOKE INSERT, UPDATE, DELETE ON intent_transitions FROM PUBLIC"),
        "callers must not be able to write audit rows directly"
    );
    assert!(
        sql.contains("REVOKE UPDATE ON trade_intents FROM PUBLIC"),
        "callers must not be able to update trade_intents directly"
    );

    // Anti-pattern 3 (1023): an authoritative function that anyone may CALL.
    // REV-031 invoked it with an ordinary read-only role. The effective grant is
    // the LAST one in migration order, so check the tail of the SQL.
    let last_public_grant = sql.rfind("GRANT EXECUTE ON FUNCTION transition_intent(bigint, text, text) TO PUBLIC");
    let last_revoke_public = sql.rfind("REVOKE ALL ON FUNCTION transition_intent(bigint, text, text) FROM PUBLIC");
    match (last_public_grant, last_revoke_public) {
        (Some(grant), Some(revoke)) => assert!(
            revoke > grant,
            "EXECUTE on transition_intent is still granted to PUBLIC after the last revoke"
        ),
        (Some(_), None) => panic!("EXECUTE on transition_intent is granted to PUBLIC and never revoked"),
        _ => {}
    }
    assert!(
        sql.contains("GRANT EXECUTE ON FUNCTION transition_intent(bigint, text, text) TO swi_executor"),
        "EXECUTE must be granted to the explicit execution role, not PUBLIC"
    );
}

// REV-031/REV-025-F05: holding EXECUTE must not be sufficient. The function has to
// authorize the CALLER (workspace membership), the governing policy, and kill
// switches before it mutates state — otherwise an authorized-but-unscoped role can
// transition any intent in the database.
#[test]
fn transition_function_authorizes_the_caller() {
    let sql = all_sql();
    let fn_start = sql
        .rfind("CREATE OR REPLACE FUNCTION transition_intent(")
        .expect("transition_intent must be defined");
    let end = sql[fn_start..]
        .find("\nALTER FUNCTION")
        .map(|i| fn_start + i)
        .unwrap_or(sql.len());
    let body = &sql[fn_start..end];

    for (needle, why) in [
        (
            "session_user",
            "the caller must be identified by session_user; current_user is the definer",
        ),
        (
            "execution_role_workspaces",
            "the caller's workspace authority must be checked",
        ),
        (
            "kill_switches",
            "an active kill switch must block the transition",
        ),
        (
            "automation_policies",
            "the governing automation policy must be active",
        ),
        (
            "actor_role",
            "the audit row must record which role performed the transition",
        ),
    ] {
        assert!(
            body.contains(needle),
            "transition_intent is missing `{needle}`: {why}"
        );
    }

    // `current_user` must NOT be used to identify the caller inside the definer.
    assert!(
        !body.contains("= current_user") && !body.contains("current_user ="),
        "current_user is the definer inside SECURITY DEFINER; it cannot identify the caller"
    );
}

// REV-027/REV-025-F05: the DB reservation table must mirror the frozen Rust rule
// exactly — Hold for every nonterminal forward state and UNKNOWN_RECONCILIATION,
// Release only for the three terminal states.
#[test]
fn reservation_actions_mirror_frozen_rust_rule() {
    use IntentState::*;
    let states = [
        (Proposed, "proposed"),
        (Approved, "approved"),
        (Reserved, "reserved"),
        (Built, "built"),
        (Simulated, "simulated"),
        (Signed, "signed"),
        (Submitted, "submitted"),
        (Confirmed, "confirmed"),
        (FailedSafe, "failed_safe"),
        (UnknownReconciliation, "unknown_reconciliation"),
        (Cancelled, "cancelled"),
    ];

    let sql = all_sql();
    let table_start = sql
        .find("INSERT INTO intent_reservation_actions")
        .expect("intent_reservation_actions must be seeded");
    let block_end = sql[table_start..]
        .find("ON CONFLICT")
        .map(|i| table_start + i)
        .unwrap_or(sql.len());
    let block = &sql[table_start..block_end];

    for (state, wire) in states {
        let expected = match reservation_action(state) {
            ReservationAction::Hold => "hold",
            ReservationAction::Release => "release",
        };
        // The seeded row for this state, e.g. ('approved',  'hold')
        let row = block
            .lines()
            .find(|l| l.contains(&format!("('{wire}'")))
            .unwrap_or_else(|| panic!("no seeded reservation action for state `{wire}`"));
        assert!(
            row.contains(&format!("'{expected}'")),
            "state `{wire}`: frozen rule says `{expected}`, migration says `{}`",
            row.trim()
        );
    }
}

// REV-029/REV-025-F10: the legacy and canonical lanes must not both create the
// same table, or a legacy-then-canonical replay aborts (it did, at 1005).
#[test]
fn legacy_and_canonical_lanes_do_not_collide() {
    let files = migration_files();
    let legacy: Vec<String> = files.iter().filter(|n| n.starts_with('0')).cloned().collect();
    let canonical: Vec<String> = files
        .iter()
        .filter(|n| n.starts_with('1') && !n.starts_with("1000_"))
        .cloned()
        .collect();
    assert!(!legacy.is_empty() && !canonical.is_empty());

    let legacy_tables = created_tables(&legacy);
    let canonical_tables = created_tables(&canonical);

    let bridge = read_migration("1000_legacy_bridge.sql");
    let mut unbridged: Vec<String> = Vec::new();
    for (table, canonical_file) in &canonical_tables {
        if !legacy_tables.contains_key(table) {
            continue;
        }
        // A collision is acceptable only when the bridge archives the legacy table
        // away, or the canonical side creates it with IF NOT EXISTS (same shape).
        let archived = bridge.contains(&format!("'{table}'"));
        let idempotent = read_migration(canonical_file)
            .to_ascii_lowercase()
            .contains(&format!("create table if not exists {table}"));
        if !archived && !idempotent {
            unbridged.push(format!("{table} (canonical: {canonical_file})"));
        }
    }
    assert!(
        unbridged.is_empty(),
        "legacy/canonical table collisions with no bridge entry: {unbridged:?}"
    );
}

// The bridge must run AFTER the legacy baseline and BEFORE the canonical schema,
// and 1019a must run before 1020. Filename sort order is the runner's order, so
// this pins the lane sequencing that the fixes depend on.
#[test]
fn migration_filenames_sort_into_lane_order() {
    let files = migration_files();
    let pos = |prefix: &str| {
        files
            .iter()
            .position(|n| n.starts_with(prefix))
            .unwrap_or_else(|| panic!("migration `{prefix}` not found"))
    };

    assert!(pos("0010_") < pos("1000_"), "bridge must follow the legacy baseline");
    assert!(pos("1000_") < pos("1001_"), "bridge must precede the canonical schema");
    assert!(
        pos("1019a_") < pos("1020_"),
        "the admin_sessions baseline must precede the migration that alters it"
    );
    assert!(pos("1019_") < pos("1019a_"), "1019a must follow 1019");
}

// REV-031/REV-025-F10: the bridge makes the SQL lanes replay, but this binary must
// still be able to READ its own tables afterwards. The legacy runtime therefore
// pins a search_path that finds the archived legacy tables, while migrations pin
// `public` so canonical DDL never lands inside the archive schema.
#[test]
fn legacy_runtime_resolves_archived_tables_and_migrations_pin_public() {
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");

    // REV-034: this guard used to pin the literal `"swi_legacy, public"` and the
    // `.options([...])` startup-option call — i.e. it pinned the exact formulation
    // that stopped the binary from booting. A source-string guard cannot see that a
    // startup option is truncated at whitespace; only opening a pool can. The
    // runtime proof now lives in `db::tests::connect_opens_a_live_pool_with_a_usable_search_path`
    // (feature `pg_tests`); what remains here is the ORDER invariant, which is a
    // property of the string itself.
    assert!(
        db.contains(r#"pub const LEGACY_SEARCH_PATH: &str = "swi_legacy,public""#),
        "the legacy runtime must resolve swi_legacy before public, or its queries \
         hit canonical tables with different columns after the bridge"
    );
    assert!(
        db.contains("SET search_path = {LEGACY_SEARCH_PATH}"),
        "the search_path must be applied per connection via after_connect, so every \
         pooled connection agrees and a missing archive schema is tolerated"
    );
    assert!(
        !db.contains(r#".options([("search_path""#),
        "search_path must NOT be a startup option: PostgreSQL truncates such a value \
         at the first space and refuses the connection"
    );
    assert!(
        db.contains("SET LOCAL search_path = public"),
        "migrations must pin `public` so DDL never lands in the archive schema"
    );
    // The migration ledger itself must be schema-qualified for the same reason.
    assert!(
        db.contains("public._migrations"),
        "the _migrations ledger must be schema-qualified to public"
    );
    assert!(
        !db.contains("FROM _migrations WHERE") && !db.contains("INSERT INTO _migrations"),
        "no unqualified _migrations reference may remain"
    );

    // And the archive schema the runtime depends on must actually be created by the
    // bridge, with the same name.
    let bridge = read_migration("1000_legacy_bridge.sql");
    assert!(
        bridge.contains("CREATE SCHEMA IF NOT EXISTS swi_legacy"),
        "the bridge must create the schema the legacy runtime reads from"
    );
    assert!(
        bridge.contains("SET SCHEMA swi_legacy"),
        "the bridge must move colliding tables into that schema"
    );
}

// REV-029 current-view regression: the SQL retraction filter must validate the
// retraction the same way the Rust validator does. REV-028 accepted any row
// carrying a target id, so an `unknown` retraction anchored to another token
// could hide a valid relation.
#[test]
fn sql_retraction_filter_validates_like_the_rust_validator() {
    let store = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("sf")
            .join("recent_store.rs"),
    )
    .expect("read recent_store.rs");

    let start = store
        .find("fn fetch_relations")
        .expect("fetch_relations must exist");
    let body = &store[start..];

    for (needle, why) in [
        (
            "r.token_identity = e.token_identity",
            "a retraction must be anchored to the same token as its target",
        ),
        (
            "IN ('superseded', 'erroneous')",
            "only the two legal retraction statuses may suppress a target",
        ),
        (
            "r.truth_status = 'confirmed'",
            "an unknown/disputed retraction row must not suppress anything",
        ),
    ] {
        assert!(
            body.contains(needle),
            "fetch_relations retraction filter is missing `{needle}`: {why}"
        );
    }

    // And the legal-status set must match the Rust side exactly.
    let runtime = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("sf")
            .join("recent_runtime.rs"),
    )
    .expect("read recent_runtime.rs");
    let legal: BTreeSet<&str> = ["Superseded", "Erroneous"].into_iter().collect();
    for status in &legal {
        assert!(
            runtime.contains(&format!("TruthStatus::{status}")),
            "apply_retraction must treat {status} as a legal retraction status"
        );
    }
}

// ---------------------------------------------------------------------------
// REV-035 regressions.
// ---------------------------------------------------------------------------

// REV-035-#3: PLAN SWI §19 freezes "Reconciliation continues" under a halt.
// Migration 1025 classified risk from the intent's ORIGINAL action and gated EVERY
// transition, so the reviewer measured reconciliation being refused exactly when it
// matters most:
//     HALT     + submitted SELL -> unknown_reconciliation  REJECTED
//     EXIT_ONLY + submitted BUY -> confirmed               REJECTED
// Both intents stayed `submitted` with the reservation held. 1026 keys the gate on
// the TRANSITION purpose instead. This test pins the classification so a future
// edit cannot quietly re-gate settlement or unwind.
#[test]
fn every_legal_transition_has_a_purpose_and_only_entry_is_gated() {
    let sql = all_sql();

    // Every edge in the frozen state machine must be classified. An unclassified
    // edge is rejected at runtime by 1026, so a gap is a hard failure, not a
    // silent widening.
    let edges = frozen_pairs(&sql, "INSERT INTO intent_transition_edges (from_state, to_state) VALUES");
    let purposes = frozen_triples(&sql);
    assert!(!edges.is_empty(), "the frozen edge list must be discoverable");

    for (from, to) in &edges {
        let found = purposes.iter().find(|(f, t, _)| f == from && t == to);
        assert!(
            found.is_some(),
            "transition {from} -> {to} has no frozen purpose; 1026 refuses it at runtime"
        );
    }

    // Reconciliation and unwind must NOT be classified `entry`, because `entry` is
    // the only purpose the kill-switch gate applies to.
    let must_not_be_entry = [
        ("submitted", "confirmed"),
        ("submitted", "unknown_reconciliation"),
        ("unknown_reconciliation", "confirmed"),
        ("unknown_reconciliation", "failed_safe"),
        ("unknown_reconciliation", "cancelled"),
        ("submitted", "failed_safe"),
        ("proposed", "cancelled"),
        ("approved", "cancelled"),
    ];
    for (from, to) in must_not_be_entry {
        let (_, _, purpose) = purposes
            .iter()
            .find(|(f, t, _)| f == from && t == to)
            .unwrap_or_else(|| panic!("{from} -> {to} must be classified"));
        assert_ne!(
            purpose, "entry",
            "{from} -> {to} records or unwinds an existing position; gating it under a \
             kill switch blinds or strands the operator (PLAN SWI §19)"
        );
    }

    // And the forward path MUST be `entry`, or the kill switch stops nothing.
    for (from, to) in [
        ("proposed", "approved"),
        ("approved", "reserved"),
        ("reserved", "built"),
        ("built", "simulated"),
        ("simulated", "signed"),
        ("signed", "submitted"),
    ] {
        let (_, _, purpose) = purposes
            .iter()
            .find(|(f, t, _)| f == from && t == to)
            .unwrap_or_else(|| panic!("{from} -> {to} must be classified"));
        assert_eq!(
            purpose, "entry",
            "{from} -> {to} advances toward new on-chain commitment and must be gated"
        );
    }

    // The gate itself must be conditional on the purpose, not applied globally.
    let f = read_migration("1026_rev035_reconciliation_and_grants.sql");
    assert!(
        f.contains("IF v_purpose = 'entry' THEN"),
        "the kill-switch gate must apply to entry transitions only"
    );
}

// REV-035-#2/#5: 1025 granted blanket `ALL TABLES IN SCHEMA public` DML, so the
// runtime role could rewrite the very inputs `transition_intent()` trusts. The
// reviewer proved each capability live: it inserted its own workspace mapping,
// paused a policy, deleted kill switches, forged a social identity. Each REVOKE
// below corresponds to a measured capability.
#[test]
fn runtime_roles_cannot_write_their_own_authorization_inputs() {
    let f = read_migration("1026_rev035_reconciliation_and_grants.sql");
    for table in [
        "execution_role_workspaces", // could grant ITSELF a workspace
        "automation_policies",       // could pause the policy gating it
        "automation_policy_versions",
        "kill_switches",             // could delete the switch restraining it
        "intent_action_classes",
        "intent_transition_purposes",
        "intent_transition_edges",
        "intent_reservation_actions",
    ] {
        let revoked = f.lines().any(|l| {
            l.starts_with("REVOKE")
                && l.contains(table)
                && (l.contains("INSERT") || f.contains(&format!("ON {table}\n    FROM")))
        }) || f.contains(&format!("ON {table}\n    FROM swi_legacy_runtime, swi_app"));
        assert!(
            revoked,
            "the runtime roles must not be able to write `{table}`: it is an input to \
             the authorization decision, not an output of it"
        );
    }

    // Identity truth: INSERT is the whole forgery; 1025 revoked only UPDATE/DELETE.
    assert!(
        f.contains("REVOKE INSERT ON social_identities"),
        "a forged social identity promotes funding to Reconstructed; INSERT must be revoked"
    );
    // Credentials and sessions are not readable by an API/read role. These tables
    // come from the LEGACY baseline, so the REVOKE is issued conditionally through
    // a DO block (a canonical-fresh install has no `secret_store`); assert on the
    // table names appearing in that guarded block rather than on a bare statement.
    assert!(
        f.contains("'secret_store'") && f.contains("REVOKE ALL ON public.%I FROM swi_app"),
        "no runtime role may read the secret store"
    );
    assert!(
        f.contains("'admin_sessions'"),
        "a read-only API role must not read session material"
    );
    assert!(
        f.contains("REVOKE ALL ON public.%I FROM swi_legacy_runtime"),
        "the legacy runtime must lose the secret store too"
    );
}

/// Extract `('a', 'b')` pairs following an anchor INSERT.
fn frozen_pairs(sql: &str, anchor: &str) -> Vec<(String, String)> {
    let Some(start) = sql.find(anchor) else { return Vec::new() };
    let block = &sql[start + anchor.len()..];
    let end = block.find("ON CONFLICT").unwrap_or(block.len());
    let block = &block[..end];
    block
        .lines()
        .filter_map(|line| {
            let mut quoted = line.split('\'').skip(1).step_by(2);
            let a = quoted.next()?.to_string();
            let b = quoted.next()?.to_string();
            Some((a, b))
        })
        .collect()
}

/// Extract `('from', 'to', 'purpose')` triples from the 1026 purpose seed.
fn frozen_triples(sql: &str) -> Vec<(String, String, String)> {
    let anchor = "INSERT INTO intent_transition_purposes (from_state, to_state, purpose) VALUES";
    let Some(start) = sql.find(anchor) else { return Vec::new() };
    let block = &sql[start + anchor.len()..];
    let end = block.find("ON CONFLICT").unwrap_or(block.len());
    let block = &block[..end];
    block
        .lines()
        .filter_map(|line| {
            let mut quoted = line.split('\'').skip(1).step_by(2);
            let a = quoted.next()?.to_string();
            let b = quoted.next()?.to_string();
            let c = quoted.next()?.to_string();
            Some((a, b, c))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// REV-037 regressions.
// ---------------------------------------------------------------------------

// REV-037-F01: the migration ledger is authority about schema state, so no runtime
// role may write it, and a recorded FILENAME must not be accepted as proof that the
// file on disk is what ran.
//
// I found this gap myself in REV-036 and closed only half: I added `GRANT SELECT`
// and never revoked the INSERT/UPDATE/DELETE inherited from 1025's blanket grant.
// The reviewer inserted a bare filename and `ensure_schema_current()` then passed
// for a migration that had never been applied; separately they changed the contents
// of an already-recorded file and the mismatch was accepted.
#[test]
fn the_migration_ledger_is_write_protected_and_checksum_bound() {
    let f = read_migration("1027_rev037_ledger_purpose_scope_allowlist.sql");

    assert!(
        f.contains("REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON public._migrations"),
        "no runtime role may claim a migration was applied"
    );
    assert!(
        f.contains("FROM swi_legacy_runtime, swi_app, PUBLIC"),
        "the ledger revoke must cover PUBLIC as well as the named roles"
    );
    assert!(
        f.contains("ADD COLUMN 'sha256'") || f.contains("ADD COLUMN sha256 text"),
        "the ledger must be able to record a digest, not just a filename"
    );

    // The Rust side must verify the digest, and must not treat a missing digest as
    // verified — "unverifiable" and "verified" are different answers.
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");
    assert!(
        db.contains("fn migration_sha256"),
        "ensure_schema_current must compute a digest of each migration file"
    );
    assert!(
        db.contains("no longer match the files on disk"),
        "a content mismatch must refuse startup"
    );
    assert!(
        db.contains("carry no checksum"),
        "a row with no digest must be reported, never silently accepted"
    );
    assert!(
        db.contains("with no file on disk"),
        "a ledger row with no corresponding file means binary/schema disagreement"
    );
    // And migrate() must WRITE the digest, or nothing can ever be verified. Since
    // REV-043-F02 it also records HOW the digest was obtained, so the assertion
    // checks the columns rather than one exact statement string.
    assert!(
        db.contains("INSERT INTO public._migrations (name, sha256, digest_origin)"),
        "migrate() must record the digest of what it applied, and its provenance"
    );
    assert!(
        db.contains("VALUES ($1, $2, 'applied')"),
        "a digest computed from SQL this binary executed must be marked `applied`"
    );
}

// REV-037-F02: the frozen edge/purpose must be resolved BEFORE the policy test.
// 1026 tested the policy first, so a rotated or paused policy — an ordinary
// operation — blocked settlement and unwind. Same symptom as REV-035, different
// cause, and I missed it because I only tested the active/current-policy path.
#[test]
fn purpose_is_resolved_before_the_policy_gate() {
    let f = read_migration("1027_rev037_ledger_purpose_scope_allowlist.sql");

    let purpose_at = f
        .find("SELECT purpose INTO v_purpose")
        .expect("the function must resolve the frozen purpose");
    let entry_policy_at = f
        .find("is not the active current version")
        .expect("entry must still require the current active policy");
    assert!(
        purpose_at < entry_policy_at,
        "the transition purpose must be known before the policy strength is chosen, \
         otherwise settlement/unwind inherit the entry rule (REV-037-F02)"
    );

    // Settlement/unwind keep a weaker but real requirement: the persisted policy
    // must exist and belong to the same workspace.
    assert!(
        f.contains("does not belong to workspace"),
        "settlement/unwind must still verify the policy belongs to this workspace, \
         so audit evidence cannot be borrowed from another tenant"
    );
    // The mandatory-policy check must NOT be weakened for any purpose.
    assert!(
        f.contains("carries no automation policy version"),
        "every intent must carry a policy version regardless of purpose"
    );
}

// REV-037-F04: kill switches are workspace-scoped. `kill_switches.workspace_id`
// exists and both queries I wrote in 1026 ignored it, so a global row belonging to
// workspace 2 halted workspace 1.
#[test]
fn kill_switches_are_scoped_to_their_workspace() {
    let f = read_migration("1027_rev037_ledger_purpose_scope_allowlist.sql");

    let scope_clause = "(ks.system_scope OR ks.workspace_id = v_workspace_id)";
    let occurrences = f.matches(scope_clause).count();
    assert_eq!(
        occurrences, 2,
        "BOTH the halt and the exit_only query must be workspace-scoped; found \
         {occurrences} scoped clause(s)"
    );
    assert!(
        f.contains("ADD COLUMN IF NOT EXISTS system_scope"),
        "a platform-wide stop must be stated explicitly, not implied by a NULL tenant id"
    );
    // Upgrade safety: rows that previously behaved as system-wide must keep doing
    // so, or the upgrade silently narrows a live safety control.
    assert!(
        f.contains("SET system_scope = true") && f.contains("WHERE workspace_id IS NULL"),
        "pre-existing NULL-workspace switches must be migrated to explicit system scope"
    );
}

// REV-037-F03: an allowlist, not a denylist. 1026 subtracted the six tables the
// reviewer named and left `decision_bundles` and `signer_checks` fully mutable
// (reject -> approve -> deleted). Subtraction can never be complete, and any table
// added by a future migration would start writable.
#[test]
fn runtime_privileges_are_an_allowlist_not_a_denylist() {
    let f = read_migration("1027_rev037_ledger_purpose_scope_allowlist.sql");

    // Everything is withdrawn first ...
    let withdraw_at = f
        .find("REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON ALL TABLES IN SCHEMA public")
        .expect("the migration must withdraw blanket DML before granting anything back");
    // ... and only then granted back per table.
    let grant_at = f
        .find("GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO swi_legacy_runtime")
        .expect("privileges must be granted back per table, not per schema");
    assert!(
        withdraw_at < grant_at,
        "the withdrawal must precede the per-table grants, or the allowlist is \
         layered on top of a blanket grant and means nothing"
    );

    // Decision/signer truth must lose UPDATE/DELETE/TRUNCATE explicitly.
    for table in ["decision_bundles", "signer_checks"] {
        assert!(
            f.contains(table),
            "`{table}` is authoritative point-in-time truth and must be named \
             explicitly; the reviewer drove reject -> approve -> deleted"
        );
    }
    assert!(
        f.contains("REVOKE UPDATE, DELETE, TRUNCATE ON public.%I"),
        "append-only tables must lose UPDATE/DELETE/TRUNCATE"
    );

    // The legacy runtime must KEEP the admin tables its own routes write, or 1026's
    // blanket revoke would have shipped a broken admin UI. A boundary that breaks
    // legitimate work is an outage, not a boundary.
    assert!(
        f.contains("'secret_store', 'admin_sessions'"),
        "the admin tables the legacy runtime itself writes must be handled explicitly"
    );
    assert!(
        f.contains("GRANT SELECT, INSERT, UPDATE, DELETE ON public.%I TO swi_legacy_runtime', t)"),
        "the legacy runtime must retain DML on its own admin tables"
    );
}

// ---------------------------------------------------------------------------
// REV-039 regressions.
// ---------------------------------------------------------------------------

// REV-039-F01: a database migrated before 1027 has ledger rows with no digest, and
// my verifier correctly refuses them — so without a backfill path every runtime
// command on an EXISTING database refused to start, telling the operator to apply a
// migration that was already applied. I tested a fresh install and stopped.
//
// The backfill must not trust the file on disk: hashing whatever is there would
// bless an edited file as "what was applied". It is accepted only when it matches
// the reviewed manifest that ships with the SQL.
#[test]
fn pre_checksum_ledger_rows_can_be_backfilled_only_from_the_reviewed_manifest() {
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");

    assert!(
        db.contains("fn manifest_digest"),
        "there must be a reviewed-manifest lookup, or pre-1027 rows can never be verified"
    );
    // Backfill writes both the digest and its provenance (REV-043-F02). Matched on
    // the SET clause rather than a full statement string, so line-wrapping in the
    // source does not decide whether this test passes.
    assert!(
        db.contains("SET sha256 = $2, digest_origin = 'baseline'"),
        "an already-applied row with no digest must be backfillable, and recorded as \
         a baseline rather than as an observed digest"
    );
    assert!(
        db.contains("cannot backfill a digest for already-applied"),
        "a file that changed after it was applied must be refused, not blessed"
    );
    assert!(
        db.contains("no entry in"),
        "a row with no manifest entry must be refused rather than trusted"
    );

    // The manifest itself must exist, cover every migration, and be digest-shaped.
    let dir = migrations_dir();
    let manifest = std::fs::read_to_string(dir.join("MANIFEST.sha256"))
        .expect("the reviewed digest manifest must ship with the migrations");
    for name in migration_files() {
        assert!(
            manifest.contains(&name),
            "migration `{name}` has no manifest entry; an existing database could \
             never verify it"
        );
    }
    // Entries are `<64 hex>  <filename>`.
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts.next().unwrap_or("");
        assert_eq!(
            digest.len(),
            64,
            "manifest digest must be a full sha256 hex string, got `{digest}`"
        );
        assert!(
            digest.chars().all(|c| c.is_ascii_hexdigit()),
            "manifest digest must be hex, got `{digest}`"
        );
    }
}

// REV-039-F04: an unreadable `.sql` file used to be silently dropped from the disk
// manifest (`read_to_string(&p).ok()?`), so verification passed while ignoring it.
// A migration directory that cannot be fully read is not a verified set.
#[test]
fn unreadable_migration_files_are_fatal_not_ignored() {
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");

    // Only EXECUTABLE lines count: the comment explaining why the old form was
    // wrong must be allowed to name it, or documenting the fix would fail the guard.
    // (My first version of this test failed on its own explanatory comment — the
    // same class of mistake as the string guards the reviewer has flagged twice.)
    let offending: Vec<usize> = db
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim();
            !t.starts_with("//") && t.contains("read_to_string(&p).ok()?")
        })
        .map(|(i, _)| i + 1)
        .collect();
    assert!(
        offending.is_empty(),
        "an unreadable migration must not be skipped; that is how verification \
         passed over a file it could not read (lines {offending:?})"
    );
    // Matched on a short fragment: the message is line-wrapped in the source, so a
    // long literal would only be testing my own formatting.
    assert!(
        db.contains("cannot be fully read"),
        "the failure must say why an unreadable directory is fatal"
    );
    assert!(
        db.contains("migration filename is not valid UTF-8"),
        "a non-UTF-8 filename must be fatal"
    );
    assert!(
        db.contains("duplicate migration filename"),
        "a duplicate filename must be fatal"
    );
}

// REV-039-F05: `db::migrate()` reasserted ledger protection against PUBLIC only, and
// swallowed the result with `let _ =`. An explicit grant to the named role survived
// a remigrate.
#[test]
fn migrate_reasserts_ledger_protection_against_named_roles() {
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");

    assert!(
        db.contains(r#"for role in ["PUBLIC", "swi_legacy_runtime", "swi_app"]"#),
        "the named runtime roles must be revoked too, not only PUBLIC"
    );
    assert!(
        db.contains("refusing to \\\n                     continue with a writable ledger")
            || db.contains("continue with a writable ledger"),
        "a failed revoke must be reported, not swallowed"
    );
    assert!(
        !db.contains(
            "let _ = sqlx::raw_sql(\n        \"REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON public._migrations FROM PUBLIC\","
        ),
        "the swallowed single-role revoke must be gone"
    );
}

// REV-039-F02/F03: privileges are per OPERATION and cover the CREDENTIAL class and
// the ARCHIVE schema, not only the tables the reviewer happened to name.
//
// Written from a mechanical enumeration of the source tree, which found more than
// the review reported: `gmgn_pubkeys.private_key_pem` (a signing key), `invites`,
// `sessions`, and 18 append-only tables rather than 5.
#[test]
fn credentials_and_append_only_are_protected_as_classes() {
    let f = read_migration("1028_rev039_per_operation_and_credentials.sql");

    // Credential class — including the three the review did not name.
    for t in [
        "gmgn_keys",
        "helius_keys",
        "gmgn_pubkeys", // private_key_pem: a SIGNING KEY
        "invites",
        "sessions",
        "secret_store",
        "admin_sessions",
    ] {
        assert!(
            f.contains(&format!("'{t}'")),
            "credential/session table `{t}` must be handled explicitly"
        );
    }
    // A signing key is not readable by ANY runtime role.
    assert!(
        f.contains("REVOKE ALL ON public.gmgn_pubkeys\n                 FROM swi_app, swi_legacy_runtime, PUBLIC")
            || f.contains("REVOKE ALL ON public.gmgn_pubkeys FROM swi_legacy_runtime"),
        "the signing key must be denied to the runtime role as well as the API role"
    );

    // Append-only class gets SELECT+INSERT, and loses UPDATE/DELETE explicitly.
    assert!(
        f.contains("GRANT SELECT, INSERT ON public.%I TO swi_legacy_runtime"),
        "append-only tables must get SELECT+INSERT, not all DML"
    );
    assert!(
        f.contains("REVOKE UPDATE, DELETE, TRUNCATE ON public.%I"),
        "append-only tables must lose UPDATE/DELETE/TRUNCATE"
    );
    for t in [
        "gmgn_token_observations",
        "gmgn_wallet_observations",
        "narrative_evidence",
        "signal_evaluations",
        "funding_radar_events",
        "recent_events",
        "tokens",
        "trades",
    ] {
        assert!(
            f.contains(&format!("'{t}'")),
            "append-only table `{t}` must appear in the enumerated list"
        );
    }

    // The tables the binary genuinely mutates must KEEP their DML, each with a
    // recorded reason. Denying one of these is an outage, which is exactly what
    // 1026 did to `secret_store`.
    for t in [
        "wallets",
        "wallet_labels",
        "chain_sync_state",
        "telegram_channels",
        "funding_radar_cases",
        "raw_events",
        "market_snapshots",
        "admin_settings",
        "login_attempts",
    ] {
        assert!(
            f.contains(&format!("ARRAY['{t}',")),
            "`{t}` is mutated by the source and must stay mutable, with its reason \
             recorded; denying it would be an outage"
        );
    }

    // The ARCHIVE schema must get the same per-operation posture. Protecting only
    // `public` protects the schema the bridged runtime does not resolve first.
    assert!(
        f.contains("REVOKE ALL ON ALL TABLES IN SCHEMA swi_legacy"),
        "the archive schema must be withdrawn before being granted back"
    );
    assert!(
        f.contains("GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA swi_legacy"),
        "archived tables are append-only in the source and must not get blanket DML"
    );
    assert!(
        !f.contains("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA swi_legacy"),
        "the archive must not receive blanket DML: on a bridged database the archive \
         is what the runtime's search_path resolves FIRST, so a leak there bypasses \
         every public-schema protection"
    );
}

// ---------------------------------------------------------------------------
// REV-041 regressions.
// ---------------------------------------------------------------------------

// REV-041-F01: a pre-checksum ledger row must never be labelled `verified`.
//
// REV-040 backfilled such rows with the CURRENT file's digest and treated them as
// verified. That asserts a historical fact nobody knows, and this repository
// contains a real counter-example: `1013_strategy_lab.sql` exists in two revisions
// that create DIFFERENTLY NAMED constraints, so a database carrying the older name
// still rejects `canary`/`paused` while the ledger claims the current file ran.
//
// The distinction is now explicit (`baseline:<digest>`), gated on operator consent,
// and reportable. My mistake here was not "fixed one place" or "stopped at the first
// green" — it was accepting the premise that one filename means one set of bytes,
// which a single grep of this repo disproves.
#[test]
fn pre_checksum_rows_are_baseline_not_verified() {
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");

    assert!(
        db.contains(r#"pub const BASELINE_PREFIX: &str = "baseline:""#),
        "a pre-checksum row must be recorded as an accepted baseline, distinguishable \
         from a digest observed at apply time"
    );
    assert!(
        db.contains("{BASELINE_PREFIX}{digest}"),
        "the backfill must write the baseline marker, not a bare digest"
    );
    // Consent is required: silently certifying unknown history is the bug.
    assert!(
        db.contains("accept_legacy_baseline"),
        "recording a legacy baseline must require explicit operator consent"
    );
    // Short fragment on purpose: the message is line-wrapped in the source, so a
    // long literal would test my own formatting rather than the behaviour.
    assert!(
        db.contains("applied before digests were recorded"),
        "the refusal must state plainly that the applied bytes are unknown"
    );
    // The known drift must be named in the guidance, so an operator knows what to
    // reconcile rather than being told only that something is wrong.
    assert!(
        db.contains("strategy_versions_lifecycle_check"),
        "the refusal should point at the known drift to check first"
    );
    // A baseline row whose file changes AFTER acceptance is still fatal: acceptance
    // covers unknown history, not future edits.
    assert!(
        db.contains("baseline {}, on disk {}"),
        "a baseline row must still be compared against the file on disk"
    );
    // And the distinction must be inspectable, or it decays into \"green\".
    assert!(
        db.contains("pub async fn baseline_migrations"),
        "baseline rows must be reportable (db status), not just recorded"
    );

    let main = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("main.rs"),
    )
    .expect("read main.rs");
    assert!(
        main.contains("DbAction::Status"),
        "there must be a command that reports ledger verification state"
    );
    assert!(
        main.contains("ACCEPTED BASELINE"),
        "the report must not present baseline rows as verified"
    );
}

// REV-041-F01 remediation #2: the rename-on-drop CLASS is reconciled, not just the
// one reported row.
//
// Enumerating every `DROP CONSTRAINT IF EXISTS x` with no matching `ADD CONSTRAINT x`
// across all migrations found two candidates. Only `1013_strategy_lab.sql` is drift;
// `1019_rev022_corrective.sql` drops a constraint it deliberately replaces with a
// partial unique index. This test pins that enumeration so a NEW rename cannot be
// introduced without either re-adding the name or reconciling it forward.
#[test]
fn renamed_constraints_are_reconciled_or_deliberate() {
    let files = migration_files();
    // (migration, dropped-name) pairs that never re-add the same name.
    let mut renames: Vec<(String, String)> = Vec::new();
    for name in &files {
        let sql = read_migration(name);
        let lower = sql.to_ascii_lowercase();
        let dropped: Vec<String> = lower
            .split("drop constraint if exists")
            .skip(1)
            .filter_map(|tail| {
                tail.split(|c: char| c == ';' || c.is_whitespace())
                    .find(|t| !t.is_empty())
                    .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_').to_string())
            })
            .collect();
        for d in dropped {
            if d.is_empty() {
                continue;
            }
            if !lower.contains(&format!("add constraint {d}")) {
                renames.push((name.clone(), d));
            }
        }
    }

    // Every rename must be either reconciled forward (1029) or documented as a
    // deliberate removal.
    let reconcile = read_migration("1029_rev041_reconcile_renamed_constraints.sql");
    let deliberate_removals = ["social_identities_platform_user_key"];

    for (file, dropped) in &renames {
        let handled = reconcile.contains(dropped.as_str())
            || deliberate_removals.contains(&dropped.as_str());
        assert!(
            handled,
            "`{file}` drops constraint `{dropped}` and never re-adds that name. On a \
             database that ran a different revision of the same filename, the drop \
             misses and the old definition survives. Either reconcile it in a forward \
             migration or record it as a deliberate removal"
        );
    }

    // And the reconciliation must drop BOTH spellings and assert the end state, so
    // it is correct whichever revision a given database ran.
    assert!(
        reconcile.contains("strategy_versions_lifecycle_state_check")
            && reconcile.contains("strategy_versions_lifecycle_check"),
        "the reconciliation must handle both historical constraint names"
    );
    assert!(
        reconcile.contains("stale lifecycle constraint(s) remain"),
        "the migration must assert the reconciled end state, because a source test \
         cannot see which revision a live database actually ran"
    );
    assert!(
        reconcile.contains("'canary'") && reconcile.contains("'paused'"),
        "the canonical constraint must permit the states §13 freezes"
    );
    // Widening a constraint while rows violate the target set would turn a schema
    // problem into silent data acceptance.
    assert!(
        reconcile.contains("resolve the data before reconciling the constraint"),
        "the reconciliation must refuse to widen over violating rows"
    );
}

// REV-043-F02: a digest whose provenance is unknown must not read as verified.
//
// REV-040 wrote bare digests into pre-checksum rows. REV-042 introduced the
// `baseline:` prefix but only applied it where `sha256 IS NULL`, so a database that
// had already passed through REV-040 kept digests that LOOK observed while their
// history is equally unknown. A value prefix cannot fix that retroactively: by value
// those rows are indistinguishable from digests recorded at apply time.
//
// So provenance is recorded in its own column, and a row without it is refused
// rather than inherited as green.
#[test]
fn digests_without_recorded_provenance_are_refused() {
    let db = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("db.rs"),
    )
    .expect("read db.rs");

    assert!(
        db.contains("ADD COLUMN IF NOT EXISTS digest_origin text"),
        "provenance must be recorded in its own column; a value prefix cannot be \
         applied retroactively to rows an earlier build already wrote"
    );
    // Only the two meaningful values, enforced by the database itself.
    assert!(
        db.contains("digest_origin IN ('applied', 'baseline')"),
        "the provenance vocabulary must be constrained in the schema"
    );
    // The verifier must refuse a digest with no provenance ...
    assert!(
        db.contains("carry a digest with no recorded provenance"),
        "startup must refuse rows whose digest provenance is unknown"
    );
    // ... and migrate must offer a way to re-declare them, gated on consent.
    assert!(
        db.contains("carries a digest with no recorded provenance"),
        "migrate must explain how to re-declare a provenance-less digest"
    );
    assert!(
        db.contains("re-declared a provenance-less digest as an ACCEPTED BASELINE"),
        "re-declaration must be logged as a baseline, never as a verification"
    );
    // Reporting must find baseline rows by EITHER marker, so rows written by any
    // version of this code are visible.
    assert!(
        db.contains("sha256 LIKE 'baseline:%' OR digest_origin = 'baseline'"),
        "baseline reporting must cover both the value prefix and the provenance column"
    );
}
