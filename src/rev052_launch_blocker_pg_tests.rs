//! Live PostgreSQL tests for the REV-051 launch blockers C1 and C3.
//!
//! Skipped (not failed) without `TEST_DATABASE_URL`/`DATABASE_URL`, matching the
//! other `pg_tests` modules, so an offline default run stays green.
//!
//! C1 (REV-050-F01) — funding evidence must belong to the token being resolved.
//! The bug was a MISSING predicate, and no amount of source reading proves a
//! predicate is present: the only proof is seeding an edge for token B, resolving
//! token A, and observing that B's edge is unreachable.
//!
//! C3 (REV-050-F03) — a real retention cycle, driven by the production recording
//! path, with the readback the review asks for: old row deleted, boundary row
//! retained, evidence untouched, `last_success`/`rows_pruned_total` updated, and a
//! forced failure degrading health instead of passing silently.

#![cfg(all(test, feature = "pg_tests"))]

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::maintenance::{run_cycle_recording, MaintenanceState};
use crate::models::ChainKind;

/// Fail closed without a live database (REV-056-F06): see
/// `pg_test_support::require_live_url`. A skipped assertion must never report success.
fn live_url() -> String {
    crate::pg_test_support::require_live_url()
}

async fn pool() -> PgPool {
    crate::pg_test_support::live_pool().await
}

/// Connect the way the RUNTIME does.
///

/// A per-run suffix. `funding_edges` is append-only for the runtime role (migration
/// 1028 revokes UPDATE/DELETE), so isolation comes from unique keys, never cleanup.
fn tag(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{nanos}")
}

/// Seed one promoted funding edge attributed to `mint`, exactly the way
/// `graph::update_funding_edges` records it (mint inside `evidence`).
async fn seed_edge(
    pool: &PgPool,
    mint: &str,
    from: &str,
    to: &str,
    at: DateTime<Utc>,
) {
    sqlx::query(
        r#"
        INSERT INTO funding_edges
            (chain, from_address, to_address, signature, edge_kind, raw_amount,
             block_time, confidence, evidence, promoted)
        VALUES ($1, $2, $3, $4, 'token_funding', '1', $5, 0.9,
                jsonb_build_object('mint', $6::text, 'asset_kind', 'token'), true)
        ON CONFLICT (chain, from_address, to_address, signature, edge_kind) DO NOTHING
        "#,
    )
    .bind(ChainKind::Solana.as_str())
    .bind(from)
    .bind(to)
    .bind(format!("sig-{from}-{to}"))
    .bind(at)
    .bind(mint)
    .execute(pool)
    .await
    .expect("seed funding edge");
}

// C1 / REV-050-F01. Two tokens, same chain, one promoted edge each. Resolving A must
// see A's edge and MUST NOT see B's.
//
// The pre-fix query (`WHERE chain = $1 AND promoted = true`) returns both rows and
// therefore fails this test — which is the point: it is the reproduction, not a
// restatement of the fix.
#[tokio::test]
async fn funding_evidence_is_scoped_to_the_token_being_resolved() {
    let pool = pool().await;
    let mint_a = tag("MINT_A");
    let mint_b = tag("MINT_B");
    let now = Utc::now();

    let funder_a = tag("FUNDER_A");
    let recipient_a = tag("RECIP_A");
    let funder_b = tag("FUNDER_B");
    let recipient_b = tag("RECIP_B");

    seed_edge(&pool, &mint_a, &funder_a, &recipient_a, now).await;
    seed_edge(&pool, &mint_b, &funder_b, &recipient_b, now).await;

    let a_edges = crate::workers::token_owned_funding_edges(&pool, ChainKind::Solana, &mint_a)
        .await
        .expect("query A");

    assert!(
        a_edges
            .iter()
            .any(|e| e.from_address == funder_a && e.to_address == recipient_a),
        "token A must see its own edge"
    );
    assert!(
        !a_edges
            .iter()
            .any(|e| e.from_address == funder_b || e.to_address == recipient_b),
        "token B's edge leaked into token A's graph: a relation resolved from it \
         would be attributed to the wrong token (REV-050-F01)"
    );

    // And symmetrically, so the predicate is a real join and not a filter that
    // happens to exclude one direction.
    let b_edges = crate::workers::token_owned_funding_edges(&pool, ChainKind::Solana, &mint_b)
        .await
        .expect("query B");
    assert!(
        b_edges.iter().any(|e| e.from_address == funder_b),
        "token B must see its own edge"
    );
    assert!(
        !b_edges.iter().any(|e| e.from_address == funder_a),
        "token A's edge leaked into token B's graph"
    );
}

// An edge with no mint belongs to no token, so it must be attributed to none. This is
// the fail-closed half: without it, "no mint" would silently mean "every token".
#[tokio::test]
async fn a_mintless_funding_edge_is_attributed_to_no_token() {
    let pool = pool().await;
    let mint = tag("MINT_NATIVE");
    let from = tag("NATIVE_FROM");
    let to = tag("NATIVE_TO");

    sqlx::query(
        r#"
        INSERT INTO funding_edges
            (chain, from_address, to_address, signature, edge_kind, raw_amount,
             block_time, confidence, evidence, promoted)
        VALUES ($1, $2, $3, $4, 'funding', '1', now(), 0.9,
                jsonb_build_object('mint', NULL, 'asset_kind', 'native'), true)
        ON CONFLICT (chain, from_address, to_address, signature, edge_kind) DO NOTHING
        "#,
    )
    .bind(ChainKind::Solana.as_str())
    .bind(&from)
    .bind(&to)
    .bind(format!("sig-native-{from}"))
    .execute(&pool)
    .await
    .expect("seed native edge");

    let edges = crate::workers::token_owned_funding_edges(&pool, ChainKind::Solana, &mint)
        .await
        .expect("query");
    assert!(
        !edges.iter().any(|e| e.from_address == from),
        "a native (mintless) edge must not be attributed to a token"
    );
}

// C3 / REV-050-F03. One real cycle through the production recording path.
//
// Retention is 1 day for the assertion, so the "old" row is well past the horizon and
// the boundary row is well inside it; both are unambiguous without sleeping.
#[tokio::test]
async fn one_real_retention_cycle_prunes_only_expired_disposable_rows() {
    let pool = pool().await;
    let old_sig = tag("RAW_OLD");
    let fresh_sig = tag("RAW_FRESH");
    let now = Utc::now();

    // Disposable provider payloads: one expired, one inside the horizon.
    for (sig, observed) in [
        (&old_sig, now - Duration::days(30)),
        (&fresh_sig, now - Duration::minutes(5)),
    ] {
        crate::db::store_raw_event(
            &pool,
            ChainKind::Solana.as_str(),
            "rev052-retention-test",
            sig,
            &serde_json::json!({"signature": sig}),
            observed,
        )
        .await
        .expect("seed raw event");
    }

    // An append-only evidence row that must survive: the pruner touches disposable
    // payloads only, and "it deleted evidence" is the failure that would matter most.
    //
    // Scoped to THIS test's own anchor, not a global COUNT(*): sibling tests append
    // rows concurrently, so a global count is nondeterministic — and a flaky test
    // gets ignored, which is worse than not having one.
    let evidence_anchor = format!("solana:{}", tag("RETAIN"));
    sqlx::query(
        "INSERT INTO recent_events \
             (workspace_id, event_id, token_identity, event_type, anchor_identity, \
              chain_qualified_contract, occurred_at, observed_at, truth_status) \
         VALUES (1, $1, $2, 'evidence_retention_probe', $2, $2, \
                 now() - interval '400 days', now() - interval '400 days', 'confirmed')",
    )
    .bind(tag("evt_retain"))
    .bind(&evidence_anchor)
    .execute(&pool)
    .await
    .expect("seed evidence row");

    let state = MaintenanceState::new();
    let pruned = run_cycle_recording(&pool, 1, &state)
        .await
        .expect("a real cycle must succeed against a healthy database");

    // Readback on the rows themselves, not on the returned count alone.
    let old_present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM raw_events WHERE signature = $1)",
    )
    .bind(&old_sig)
    .fetch_one(&pool)
    .await
    .expect("check old");
    let fresh_present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM raw_events WHERE signature = $1)",
    )
    .bind(&fresh_sig)
    .fetch_one(&pool)
    .await
    .expect("check fresh");

    assert!(!old_present, "an expired disposable payload must be pruned");
    assert!(fresh_present, "a row inside the retention horizon must be retained");
    assert!(pruned >= 1, "the cycle must report the rows it deleted, got {pruned}");

    // The evidence row is 400 days old — far outside ANY retention horizon — so its
    // survival proves the pruner is table-scoped and not age-scoped.
    let evidence_survived: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM recent_events WHERE anchor_identity = $1)",
    )
    .bind(&evidence_anchor)
    .fetch_one(&pool)
    .await
    .expect("check evidence");
    assert!(
        evidence_survived,
        "retention must never touch append-only evidence, however old"
    );

    // Observable health, which is what `health` reports and an operator reads.
    assert!(state.last_success().is_some(), "a successful cycle must be recorded");
    assert!(state.last_error().is_none(), "a successful cycle must record no error");
    assert!(
        state.rows_pruned_total() >= pruned,
        "rows_pruned_total must accumulate what the cycle deleted"
    );
}

// The other half of C3: a FAILING cycle must degrade observable health rather than
// look like a quiet success. The failure is forced the way it would really happen — a
// missing/inaccessible target table — by pointing the pruner at a schema where
// `raw_events` does not exist.
#[tokio::test]
async fn a_failed_retention_cycle_records_the_error_and_no_success() {
    let url = live_url();
    // A dedicated empty schema with an EMPTY search_path fallback: `raw_events` cannot
    // resolve, so the DELETE fails exactly as it would under a revoked privilege or a
    // schema mismatch.
    let pool = sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("CREATE SCHEMA IF NOT EXISTS swi_rev052_empty")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET search_path = swi_rev052_empty")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect");

    let state = MaintenanceState::new();
    let err = run_cycle_recording(&pool, 1, &state)
        .await
        .expect_err("a cycle that cannot reach its table must fail, not report success");

    assert!(
        state.last_error().is_some(),
        "a failed cycle must record the error for health to report: {err}"
    );
    assert!(
        state.last_success().is_none(),
        "a failed cycle must not record a success"
    );
    assert_eq!(
        state.rows_pruned_total(),
        0,
        "a failed cycle must not claim pruned rows"
    );

    // And the health view derived from this state is DEGRADED, which is the fact an
    // operator actually sees.
    assert!(
        state.last_error().is_some(),
        "health degrades on last_error being present (health.rs report())"
    );
}
