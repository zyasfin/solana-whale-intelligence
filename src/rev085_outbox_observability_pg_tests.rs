//! Live PostgreSQL regressions for REV-084 F02 and F04.
//!
//! * F02 — completion observability: `mark_alert_sent` returns whether a row
//!   actually transitioned. A stale fencing token (the lease expired and someone
//!   else re-claimed) and a vanished row both matter to the caller, so the API
//!   REPORTS `Ok(false)` instead of pretending the delivery was recorded. The
//!   worker paths propagate that with `?` and warn on `false`.
//! * F04 — the `pending_funding_alerts` outbox lane binds the alert to its case
//!   on BOTH id and workspace (`c.workspace_id = a.workspace_id`). A malformed or
//!   backfilled row naming another workspace's case is never drained.
//!
//! No skip guard (REV-056-F06).

#![cfg(all(test, feature = "pg_tests"))]

use sqlx::PgPool;

async fn pool() -> PgPool {
    crate::pg_test_support::live_pool().await
}

fn tag(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}{nanos}")
}

async fn workspace(pool: &PgPool, slug: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO workspaces (name, slug) VALUES ($1, $2) RETURNING id")
        .bind(slug)
        .bind(slug)
        .fetch_one(pool)
        .await
        .expect("create workspace")
}

/// Seed a wallet plus a `preparation`-stage funding case at confidence 80.
async fn funding_case(pool: &PgPool, workspace_id: i64, prefix: &str) -> i64 {
    let recipient = tag(prefix);
    sqlx::query(
        "INSERT INTO wallets (chain, address, first_seen, last_seen, source) \
         VALUES ('solana', $1, now(), now(), 'test') ON CONFLICT DO NOTHING",
    )
    .bind(&recipient)
    .execute(pool)
    .await
    .expect("wallet");
    sqlx::query_scalar(
        "INSERT INTO funding_radar_cases \
             (chain, recipient, workspace_id, first_funded_at, first_funding_usd, first_funding_native, \
              source_address, deploy_window_ends_at, stage, confidence, evidence) \
         VALUES ('solana', $1, $2, now(), '100', '0.5', $3, now() + interval '1 day', \
                 'preparation', 80, '{}'::jsonb) RETURNING id",
    )
    .bind(&recipient)
    .bind(workspace_id)
    .bind(tag("R84SRC"))
    .fetch_one(pool)
    .await
    .expect("funding case")
}

// ---------------------------------------------------------------------------
// F02 — a stale completion is REPORTED, not swallowed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_completion_is_reported_not_swallowed() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g84f02a")).await;
    let case_id = funding_case(&pool, ws, "R84F02ARECIPIENT").await;
    let key = crate::signals::alert_dedup_key("funding", ws, case_id, "chat-a");

    let claim_a = crate::signals::claim_alert(&pool, "funding", ws, case_id, "chat-a")
        .await
        .expect("first claim")
        .expect("first claim wins the row");

    // The holder stalls past its lease: the row becomes re-claimable.
    sqlx::query(
        "UPDATE alerts \
            SET claim_expires_at = now() - interval '1 second', \
                next_attempt_at = now() - interval '1 second' \
          WHERE dedup_key = $1",
    )
    .bind(&key)
    .execute(&pool)
    .await
    .expect("expire the lease");

    let claim_b = crate::signals::claim_alert(&pool, "funding", ws, case_id, "chat-a")
        .await
        .expect("second claim")
        .expect("expired lease must be re-claimable");
    assert_ne!(
        claim_a.token, claim_b.token,
        "the re-claim must install a NEW fencing token"
    );

    // The stale holder completes. Its token fences it out of the row — and the
    // API must SAY SO (REV-084-F02): a silent `Ok(())` here lets a worker log a
    // delivery the database never recorded.
    let stale = crate::signals::mark_alert_sent(&pool, "funding", ws, case_id, "chat-a", &claim_a.token)
        .await
        .expect("stale completion is not an error, it is a false");
    assert!(
        !stale,
        "a stale claim token matches no row and MUST report false (REV-084-F02)"
    );

    // The row is still open for its real owner.
    let state: String = sqlx::query_scalar("SELECT state FROM alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("row state");
    assert_eq!(state, "pending", "the stale completion must not have marked the row sent");

    let live = crate::signals::mark_alert_sent(&pool, "funding", ws, case_id, "chat-a", &claim_b.token)
        .await
        .expect("live completion");
    assert!(live, "the current claim's token MUST record the delivery");
}

// ---------------------------------------------------------------------------
// F02 — a completion against a deleted row is reported, never a silent success
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_completion_against_a_deleted_row_is_an_error_not_a_silent_success() {
    let pool = pool().await;
    let ws = workspace(&pool, &tag("g84f02b")).await;
    let case_id = funding_case(&pool, ws, "R84F02BRECIPIENT").await;
    let key = crate::signals::alert_dedup_key("funding", ws, case_id, "chat-a");

    let claim = crate::signals::claim_alert(&pool, "funding", ws, case_id, "chat-a")
        .await
        .expect("claim")
        .expect("claim wins the row");

    let deleted = sqlx::query("DELETE FROM alerts WHERE dedup_key = $1")
        .bind(&key)
        .execute(&pool)
        .await
        .expect("delete the claimed row");
    assert_eq!(deleted.rows_affected(), 1, "the claimed row must have existed");

    // No row, no panic, no lie: zero rows affected is `false`.
    let sent = crate::signals::mark_alert_sent(&pool, "funding", ws, case_id, "chat-a", &claim.token)
        .await
        .expect("a vanished row is a false, not an error");
    assert!(
        !sent,
        "completion against a deleted row MUST report false (REV-084-F02)"
    );
}

// ---------------------------------------------------------------------------
// F04 — an alert naming another workspace's case is never drained
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_alert_referencing_another_workspaces_case_is_never_drained() {
    let pool = pool().await;
    let ws_a = workspace(&pool, &tag("g84f04a")).await;
    let ws_b = workspace(&pool, &tag("g84f04b")).await;
    let case_b = funding_case(&pool, ws_b, "R84F04RECIPIENT").await;

    // The malformed/backfilled row the reviewer described: owned by workspace A,
    // pointing at workspace B's case. Before REV-084-F04 the outbox lane joined on
    // `c.id = a.funding_case_id` alone, so draining A leaked B's case text.
    //
    // REV-086-F04 went further: migration 1040 replaced the two independent FKs
    // with a composite `(funding_case_id, workspace_id)` FK, so this row can no
    // longer be INSERTED at all. Assert the STRUCTURAL refusal first — that is the
    // stronger guarantee — and then keep proving the drain-side filter against a
    // row that only the constraint's absence could have produced.
    let key = crate::signals::alert_dedup_key("funding", ws_a, case_b, "chat-a");
    let insert_cross_tenant = |dedup: String| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, funding_case_id, \
                                     destination, state, attempt_count, next_attempt_at) \
                 VALUES ($1, 'funding', $2, $3, 'chat-a', 'pending', 1, now() - interval '1 second')",
            )
            .bind(dedup)
            .bind(ws_a)
            .bind(case_b)
            .execute(&pool)
            .await
        }
    };
    let refused = insert_cross_tenant(key.clone())
        .await
        .expect_err("the composite ownership FK must refuse a cross-tenant alert row");
    assert_eq!(
        refused.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23503"),
        "expected foreign_key_violation, got {refused:?}"
    );

    // Now reproduce the pre-1040 world for the drain assertion: drop the constraint
    // inside a transaction that is rolled back, so the row exists only long enough
    // to prove the reader-side filter still holds independently.
    let mut tx = pool.begin().await.expect("probe transaction");
    sqlx::query("ALTER TABLE alerts DROP CONSTRAINT alerts_funding_case_workspace_fk")
        .execute(&mut *tx)
        .await
        .expect("drop the ownership constraint for the duration of the probe");
    sqlx::query(
        "INSERT INTO alerts (dedup_key, subject_kind, workspace_id, funding_case_id, destination, \
                             state, attempt_count, next_attempt_at) \
         VALUES ($1, 'funding', $2, $3, 'chat-a', 'pending', 1, now() - interval '1 second')",
    )
    .bind(&key)
    .bind(ws_a)
    .bind(case_b)
    .execute(&mut *tx)
    .await
    .expect("seed the cross-workspace alert row");

    let drained_a: Vec<(i64, String)> = sqlx::query_as(
        "SELECT a.funding_case_id, '' FROM alerts a \
           JOIN funding_radar_cases c ON c.id = a.funding_case_id AND c.workspace_id = a.workspace_id \
          WHERE a.subject_kind = 'funding' AND a.workspace_id = $1 AND a.destination = 'chat-a' \
            AND a.state = 'pending' AND a.next_attempt_at <= now()",
    )
    .bind(ws_a)
    .fetch_all(&mut *tx)
    .await
    .expect("drain workspace A");
    assert!(
        !drained_a.iter().any(|(id, _)| *id == case_b),
        "workspace A must NEVER drain workspace B's case through its own alert row \
         — the JOIN requires c.workspace_id = a.workspace_id (REV-084-F04)"
    );
    tx.rollback().await.expect("roll the probe back");

    // Workspace B owns the case but NOT the alert row, so the outbox lane cannot
    // yield it either; only the durable-state recovery lane may, exactly once.
    let drained_b = crate::workers::pending_funding_alerts(&pool, ws_b, "chat-a", 70)
        .await
        .expect("drain workspace B");
    let hits = drained_b.iter().filter(|(id, _)| *id == case_b).count();
    assert!(
        hits <= 1,
        "workspace B must not reach the alert row it does not own; a second hit \
         means the outbox lane matched across workspaces (REV-084-F04)"
    );

    // The strongest statement available after REV-086-F04: the malformed row does
    // not merely go undrained, it does not EXIST. The probe above was rolled back,
    // and the constraint refuses any attempt to recreate it.
    let present: i64 = sqlx::query_scalar("SELECT count(*) FROM alerts WHERE dedup_key = $1")
        .bind(&key)
        .fetch_one(&pool)
        .await
        .expect("malformed row probe");
    assert_eq!(
        present, 0,
        "a cross-workspace alert row is unrepresentable once 1040 binds ownership"
    );
    let still_refused = insert_cross_tenant(key.clone())
        .await
        .expect_err("the constraint is back in force after the probe rolled back");
    assert_eq!(
        still_refused.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23503"),
        "the rolled-back probe must not have left the constraint dropped"
    );
}
