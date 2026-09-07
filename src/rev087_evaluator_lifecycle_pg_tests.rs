//! Live PostgreSQL regressions for REV-086-F05 (evaluator failure lifecycle).
//!
//! The finding: when a fenced evaluation FAILS, its in-transaction claim release
//! rolls back with the rest of the transaction, so the durable claim stayed live
//! for the full 10-minute lease and no other worker could retry the token. The
//! same loop also incremented `evaluated` on failure, so the return value
//! overstated the work done.
//!
//! Failure is injected at the database, not through a test-only code hook: a
//! trigger makes the evaluation write fail exactly the way a constraint violation
//! or a lost connection would. The production code path is otherwise unmodified.
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

fn worker_ctx(pool: &PgPool, workspace_id: i64) -> crate::workers::WorkerContext {
    let settings = crate::config::Settings {
        config: crate::config::AppConfig::default(),
        env: crate::config::EnvConfig::load(),
    };
    let workspace =
        solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope::from_job_context(workspace_id)
            .expect("workspace");
    crate::workers::WorkerContext::new(pool.clone(), &settings, workspace)
}

/// Make every `signal_evaluations` insert fail, the way a constraint violation or
/// a mid-transaction server error would.
async fn arm_failure(pool: &PgPool) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION r87_fail_evaluation() RETURNS trigger AS $fn$ \
         BEGIN RAISE EXCEPTION 'r87 injected evaluation failure'; END; $fn$ LANGUAGE plpgsql",
    )
    .execute(pool)
    .await
    .expect("create failure function");
    sqlx::query(
        "CREATE TRIGGER r87_fail_evaluation_trg BEFORE INSERT ON public.signal_evaluations \
         FOR EACH ROW EXECUTE FUNCTION r87_fail_evaluation()",
    )
    .execute(pool)
    .await
    .expect("arm failure trigger");
}

async fn disarm_failure(pool: &PgPool) {
    sqlx::query("DROP TRIGGER IF EXISTS r87_fail_evaluation_trg ON public.signal_evaluations")
        .execute(pool)
        .await
        .expect("disarm failure trigger");
    sqlx::query("DROP FUNCTION IF EXISTS r87_fail_evaluation()")
        .execute(pool)
        .await
        .expect("drop failure function");
}

// ---------------------------------------------------------------------------
// REV-086-F05 — a failed evaluation releases its claim and is not counted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_evaluation_releases_its_claim_and_is_not_counted() {
    let pool = pool().await;
    let ws: i64 = sqlx::query_scalar(
        "INSERT INTO workspaces (name, slug) VALUES ('w', $1) RETURNING id",
    )
    .bind(tag("r87f05"))
    .fetch_one(&pool)
    .await
    .expect("workspace");
    let mint = tag("R87F05MINT");
    // A token the due-selection will pick: recent, in a live lifecycle state.
    sqlx::query(
        "INSERT INTO tokens (chain, mint, first_seen_at, lifecycle_state) \
         VALUES ('solana', $1, now(), 'new_creation') \
         ON CONFLICT (chain, mint) DO UPDATE SET first_seen_at = now(), \
                                                 lifecycle_state = 'new_creation'",
    )
    .bind(&mint)
    .execute(&pool)
    .await
    .expect("due token");

    let ctx = worker_ctx(&pool, ws);

    // Precondition: with nothing armed, this token IS selected and evaluated, so a
    // zero below cannot mean "the selection simply found nothing".
    let baseline = crate::workers::evaluate_due_signals(&ctx)
        .await
        .expect("baseline pass");
    assert!(baseline >= 1, "the seeded token must be due, got {baseline}");
    // The successful pass left no claim (the fenced evaluator releases in-transaction).
    let after_success: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM signal_eval_claims WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("claim count");
    assert_eq!(after_success, 0, "a successful evaluation leaves no claim");

    // Make the token due again (the baseline pass recorded an evaluation), then
    // arm the failure.
    sqlx::query("DELETE FROM signal_evaluations WHERE workspace_id = $1 AND mint = $2")
        .bind(ws)
        .bind(&mint)
        .execute(&pool)
        .await
        .expect("make due again");
    arm_failure(&pool).await;
    let evaluated = crate::workers::evaluate_due_signals(&ctx).await;
    disarm_failure(&pool).await;

    let evaluated = evaluated.expect("a per-token failure must not abort the batch");
    assert_eq!(
        evaluated, 0,
        "a failed evaluation must not be counted as evaluated work"
    );

    let stranded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM signal_eval_claims WHERE workspace_id = $1 AND mint = $2",
    )
    .bind(ws)
    .bind(&mint)
    .fetch_one(&pool)
    .await
    .expect("claim count");
    assert_eq!(
        stranded, 0,
        "a failed evaluation must release its claim immediately, not wedge the token \
         for the full 10-minute lease (REV-086-F05)"
    );

    // And the token is immediately retryable by the next pass.
    let retry = crate::workers::evaluate_due_signals(&ctx)
        .await
        .expect("retry pass");
    assert!(retry >= 1, "the released token is retryable at once, got {retry}");
}
