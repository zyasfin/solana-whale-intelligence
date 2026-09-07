//! Signal evaluation: entry/exit gates, rejection codes, and persistence.
//!
//! Every failed gate writes exactly one `signal_evaluations` row with one
//! rejection code. Accepted signals persist to `signals` with evidence.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::config::SignalsConfig;
use crate::models::{ChainKind, LifecycleState};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

/// Convert an f64 ratio into a Decimal via string parsing.
fn decimal_from_ratio(value: f64) -> Decimal {
    Decimal::from_str_exact(&format!("{value}")).unwrap_or(Decimal::ZERO)
}


/// Allowed rejection codes (one per failed gate).
pub const REJECTION_CODES: &[&str] = &[
    "insufficient_clusters",
    "insufficient_buys",
    "unsupported_lifecycle",
    "token_too_old",
    "insufficient_liquidity",
    "critical_risk",
    "wash_risk",
    "stale_market",
    "ineligible_skill",
    "ineligible_copyability",
    "incomplete_history",
    "no_cluster_sells",
    "no_fresh_transfer",
    "no_dev_distribution",
    "insufficient_liquidity_drop",
    "no_tracked_selling",
    "incomplete_history_replay",
    "stale_market_replay",
];

/// Signal kinds.
pub const SIGNAL_KIND_ENTRY: &str = "entry";
pub const SIGNAL_KIND_EXIT: &str = "exit";

/// Market snapshot used for gates.
#[derive(Clone, Debug)]
pub struct MarketGate {
    pub liquidity_usd: Option<Decimal>,
    pub observed_at: Option<DateTime<Utc>>,
}

/// Token facts used for gates.
#[derive(Clone, Debug)]
pub struct TokenGate {
    pub lifecycle: LifecycleState,
    pub age_hours: Option<i64>,
    pub risk_flags: Vec<String>,
}

/// Cluster buying summary.
#[derive(Clone, Debug, Default)]
pub struct ClusterSummary {
    pub eligible_clusters: u32,
    pub meaningful_buys: u32,
}

/// Wallet eligibility summary.
#[derive(Clone, Debug, Default)]
pub struct WalletEligibility {
    pub eligible_wallets: u32,
    pub max_skill: u32,
    pub max_copyability: u32,
    pub min_history_completeness: Decimal,
}

/// Record one evaluation row (accepted or rejected with one code).
///
/// REV-072-F06 (HIGH): `workspace_id` is REQUIRED. A rejection code is a POLICY
/// answer — which wallets were allowed to contribute alpha is decided per workspace
/// — so a row with no recorded owner attributes one tenant's policy outcome to
/// everybody. It is a parameter rather than a default because a defaulted tenant is
/// how the untenanted write got shipped in the first place.
#[allow(clippy::too_many_arguments)]
async fn record_evaluation(
    db: &mut sqlx::PgConnection,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    signal_kind: &str,
    evaluated_at: DateTime<Utc>,
    status: &str,
    rejection_code: Option<&str>,
    evidence: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO signal_evaluations
            (workspace_id, chain, mint, signal_kind, evaluated_at, status, rejection_code, evidence)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(workspace_id)
    .bind(chain.as_str())
    .bind(mint)
    .bind(signal_kind)
    .bind(evaluated_at)
    .bind(status)
    .bind(rejection_code)
    .bind(evidence)
    .execute(&mut *db)
    .await?;
    Ok(())
}

/// Evaluate an entry signal for a token.
///
/// Gates (first failure wins, exactly one rejection code):
/// two independent eligible clusters, meaningful buys, supported lifecycle,
/// age <= 24h, liquidity >= $20k, no critical risk/wash, market data <= 5m,
/// eligible skill/copyability, history completeness >= 0.80.
///
/// REV-072-F06 (HIGH): every row this writes is OWNED by `workspace_id`. Which
/// wallets may supply the cluster, buy, and score inputs is workspace-scoped policy,
/// so the same facts are legitimately accepted in one tenant and rejected in
/// another. Writing the outcome to a global table published one tenant's policy
/// decision to every other tenant, and no reader could filter ownership that was
/// never stored.
/// REV-080-F04: the writes below are issued through [`evaluate_token_fenced`]
/// when the caller holds an evaluation claim. REV-084-F06: this unfenced entry
/// point is crate-internal only — production callers must use the fenced path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn evaluate_token(
    db: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &SignalsConfig,
    token: &TokenGate,
    market: &MarketGate,
    clusters: &ClusterSummary,
    wallets: &WalletEligibility,
    narrative_confidence: Option<u32>,
) -> Result<Option<i64>> {
    let mut conn = db.acquire().await?;
    evaluate_token_on(
        &mut conn, workspace_id, chain, mint, now, config, token, market, clusters,
        wallets, narrative_confidence,
    )
    .await
}

/// The gate logic on ONE explicit connection, so [`evaluate_token_fenced`] can
/// verify the caller's claim in the same transaction as the writes (REV-080-F04).
#[allow(clippy::too_many_arguments)]
async fn evaluate_token_on(
    conn: &mut sqlx::PgConnection,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &SignalsConfig,
    token: &TokenGate,
    market: &MarketGate,
    clusters: &ClusterSummary,
    wallets: &WalletEligibility,
    narrative_confidence: Option<u32>,
) -> Result<Option<i64>> {
    let evaluated_at = now;
    let evidence = serde_json::json!({
        "clusters": clusters.eligible_clusters,
        "meaningful_buys": clusters.meaningful_buys,
        "lifecycle": token.lifecycle.as_str(),
        "age_hours": token.age_hours,
        "liquidity_usd": market.liquidity_usd,
        "market_age_seconds": market.observed_at.map(|t| (now - t).num_seconds()),
        "risk_flags": token.risk_flags,
        "max_skill": wallets.max_skill,
        "max_copyability": wallets.max_copyability,
        "min_history_completeness": wallets.min_history_completeness,
        "narrative_confidence": narrative_confidence,
    });

    // Gate order matters: one rejection code per evaluation.
    if clusters.eligible_clusters < 2 {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("insufficient_clusters"), &evidence).await?;
        return Ok(None);
    }
    if clusters.meaningful_buys < 2 {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("insufficient_buys"), &evidence).await?;
        return Ok(None);
    }
    if !token.lifecycle.entry_supported() || token.lifecycle == LifecycleState::Unsupported {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("unsupported_lifecycle"), &evidence).await?;
        return Ok(None);
    }
    if let Some(age) = token.age_hours {
        if age > config.entry_max_token_age_hours as i64 {
            record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("token_too_old"), &evidence).await?;
            return Ok(None);
        }
    } else {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("token_too_old"), &evidence).await?;
        return Ok(None);
    }
    let liquidity_ok = market
        .liquidity_usd
        .map(|l| l >= Decimal::from(config.entry_min_liquidity_usd))
        .unwrap_or(false);
    if !liquidity_ok {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("insufficient_liquidity"), &evidence).await?;
        return Ok(None);
    }
    let critical = token
        .risk_flags
        .iter()
        .any(|f| f == "critical_risk" || f == "wash_trading" || f == "honeypot" || f == "rugged");
    if critical {
        let code = if token.risk_flags.iter().any(|f| f == "wash_trading") {
            "wash_risk"
        } else {
            "critical_risk"
        };
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some(code), &evidence).await?;
        return Ok(None);
    }
    let market_fresh = market
        .observed_at
        .map(|t| (now - t).num_seconds() <= config.market_max_age_seconds as i64)
        .unwrap_or(false);
    if !market_fresh {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("stale_market"), &evidence).await?;
        return Ok(None);
    }
    if wallets.max_skill < 70 {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("ineligible_skill"), &evidence).await?;
        return Ok(None);
    }
    if wallets.max_copyability < 60 {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("ineligible_copyability"), &evidence).await?;
        return Ok(None);
    }
    if wallets.min_history_completeness < Decimal::from_str_exact("0.80").unwrap() {
        record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("incomplete_history"), &evidence).await?;
        return Ok(None);
    }

    // All gates passed: accepted.
    let score = compute_entry_score(clusters, wallets, narrative_confidence);
    let signal_id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status, evidence)
        VALUES ($1, $2, $3, 'entry', $4, $5, 'active', $6)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(chain.as_str())
    .bind(mint)
    .bind(now)
    .bind(score as i32)
    .bind(&evidence)
    .fetch_one(&mut *conn)
    .await?;
    record_evaluation(conn, workspace_id, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "accepted", None, &evidence).await?;
    Ok(Some(signal_id))
}

/// Entry evaluation with the caller's evaluation claim verified INSIDE the
/// write transaction (REV-080-F04).
///
/// Worker A can exceed its lease; worker B reclaims; under the unfenced shape A
/// still wrote the signal and evaluation rows — duplicate logical output the
/// timestamp identity cannot absorb. Here, the claim row (workspace, chain,
/// mint, claimed_by token, unexpired) is locked and verified in the SAME
/// transaction as the writes: a stale owner matches nothing, the whole
/// transaction aborts, and zero rows land. The claim is then released in the
/// same transaction so a completed evaluation never lingers as a live claim.
///
/// The gate computation itself is delegated to [`evaluate_token`]'s logic via a
/// shared inner: this function first verifies the fence, and only then runs the
/// ordinary evaluation. The verification and the writes share one connection,
/// so no reclaim can slip between them.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate_token_fenced(
    db: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &SignalsConfig,
    token: &TokenGate,
    market: &MarketGate,
    clusters: &ClusterSummary,
    wallets: &WalletEligibility,
    narrative_confidence: Option<u32>,
    claim_token: &str,
) -> Result<Option<i64>> {
    let mut tx = db.begin().await?;
    // Lock and verify the claim in-transaction. FOR UPDATE serializes against a
    // concurrent reclaimer: if our lease is still valid we hold the row for the
    // whole write; if it is not, this matches nothing and we abort.
    let owned: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT workspace_id FROM signal_eval_claims
         WHERE workspace_id = $1 AND chain = $2 AND mint = $3
           AND claimed_by = $4 AND expires_at > now()
         FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(chain.as_str())
    .bind(mint)
    .bind(claim_token)
    .fetch_optional(&mut *tx)
    .await?;
    if owned.is_none() {
        anyhow::bail!(
            "evaluation claim not held (stale or reclaimed); refusing to write {chain:?}:{mint}"
        );
    }
    let out = evaluate_token_on(
        &mut *tx, workspace_id, chain, mint, now, config, token, market, clusters,
        wallets, narrative_confidence,
    )
    .await?;
    // Release in the same transaction: completed work leaves no live claim.
    release_eval_claim(&mut tx, workspace_id, chain, mint, claim_token).await?;
    tx.commit().await?;
    Ok(out)
}

/// Release a durable evaluation claim inside the transaction that wrote the rows.
///
/// Fenced by `claim_token`: a claimant whose lease already expired and was taken
/// over by another worker deletes nothing, so it can never revoke the live owner's
/// claim. Both fenced evaluators (entry and exit) share this one implementation.
async fn release_eval_claim(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    claim_token: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM signal_eval_claims \
          WHERE workspace_id = $1 AND chain = $2 AND mint = $3 AND claimed_by = $4",
    )
    .bind(workspace_id)
    .bind(chain.as_str())
    .bind(mint)
    .bind(claim_token)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Exit gate inputs.
#[derive(Clone, Debug, Default)]
pub struct ExitGates {
    pub clusters_selling: u32,
    pub material_fresh_transfer: bool,
    pub dev_distribution: bool,
    pub liquidity_drop_ratio: Option<Decimal>,
    pub tracked_selling: bool,
}

/// Evaluate an exit signal, FENCED by a durable evaluation claim.
///
/// REV-072-F06: workspace-owned for the same reason as the entry path — the rows
/// land in the same tenanted tables.
///
/// REV-087-F06: this was `pub` and unfenced. The crate is a binary
/// (`lib.rs` exports only `sf`), so `pub` bought nothing, but the deeper problem
/// was that the entry path verifies a claim token before writing and the exit path
/// did not — wiring an exit producer would have reintroduced the duplicate-write
/// class REV-076-F02 closed. The claim token is now REQUIRED, verified inside the
/// same transaction as the writes, and released there on completion: exactly the
/// `evaluate_token_fenced` contract.
pub(crate) async fn evaluate_exit_fenced(
    db: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &SignalsConfig,
    gates: &ExitGates,
    claim_token: &str,
) -> Result<Option<i64>> {
    // REV-084-F06 (LOW): the accepted-signal INSERT and its evaluation row were
    // two statements on a pooled connection, each auto-committing. An accepted
    // signal could survive a later evaluation-row failure, leaving a signal with
    // no audit record of why it fired. Both writes share ONE transaction, so the
    // pair is all-or-nothing.
    let mut tx = db.begin().await?;
    // REV-087-F06: ownership is verified INSIDE the transaction that writes, and
    // the row is locked, so a lease that expires mid-evaluation cannot let a
    // reclaimed token's writes land.
    let owned: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT workspace_id FROM signal_eval_claims
         WHERE workspace_id = $1 AND chain = $2 AND mint = $3
           AND claimed_by = $4 AND expires_at > now()
         FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(chain.as_str())
    .bind(mint)
    .bind(claim_token)
    .fetch_optional(&mut *tx)
    .await?;
    if owned.is_none() {
        anyhow::bail!(
            "evaluation claim not held (stale or reclaimed); refusing to write {chain:?}:{mint}"
        );
    }
    let evidence = serde_json::json!({
        "clusters_selling": gates.clusters_selling,
        "material_fresh_transfer": gates.material_fresh_transfer,
        "dev_distribution": gates.dev_distribution,
        "liquidity_drop_ratio": gates.liquidity_drop_ratio,
        "tracked_selling": gates.tracked_selling,
    });

    let liquidity_drop_ok = gates
        .liquidity_drop_ratio
        .map(|r| r >= decimal_from_ratio(config.exit_liquidity_drop_ratio))
        .unwrap_or(false);

    // Exit gates: any single qualifying condition suffices.
    let triggered = gates.clusters_selling >= 2
        || gates.material_fresh_transfer
        || gates.dev_distribution
        || (liquidity_drop_ok && gates.tracked_selling);

    if !triggered {
        let code = if gates.clusters_selling == 1 {
            "no_cluster_sells"
        } else if !gates.material_fresh_transfer {
            "no_fresh_transfer"
        } else {
            "insufficient_liquidity_drop"
        };
        record_evaluation(&mut *tx, workspace_id, chain, mint, SIGNAL_KIND_EXIT, now, "rejected", Some(code), &evidence).await?;
        release_eval_claim(&mut tx, workspace_id, chain, mint, claim_token).await?;
        tx.commit().await?;
        return Ok(None);
    }

    let score = if gates.clusters_selling >= 2 {
        80
    } else if gates.dev_distribution {
        75
    } else if gates.material_fresh_transfer {
        70
    } else {
        65
    };
    let signal_id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO signals (workspace_id, chain, mint, signal_kind, created_at, score, status, evidence)
        VALUES ($1, $2, $3, 'exit', $4, $5, 'active', $6)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(chain.as_str())
    .bind(mint)
    .bind(now)
    .bind(score)
    .bind(&evidence)
    .fetch_one(&mut *tx)
    .await?;
    record_evaluation(&mut *tx, workspace_id, chain, mint, SIGNAL_KIND_EXIT, now, "accepted", None, &evidence).await?;
    release_eval_claim(&mut tx, workspace_id, chain, mint, claim_token).await?;
    tx.commit().await?;
    Ok(Some(signal_id))
}

/// Deterministic entry score from gate evidence.
fn compute_entry_score(
    clusters: &ClusterSummary,
    wallets: &WalletEligibility,
    narrative_confidence: Option<u32>,
) -> u32 {
    let cluster_component = (clusters.eligible_clusters.min(5) * 8) as u32;
    let wallet_component = (wallets.max_skill.min(100) + wallets.max_copyability.min(100)) / 4;
    let narrative_component = narrative_confidence.unwrap_or(0).min(100) / 5;
    (cluster_component + wallet_component + narrative_component).min(100)
}

pub const ALERT_MAX_ATTEMPTS: i32 = 8;

/// Hard ceiling for a provider-supplied `Retry-After` (REV-076-F03).
///
/// An unbounded value could park work indefinitely or overflow the interval
/// conversion and abort the whole dispatch pass. Ten minutes is far beyond any
/// honest rate limit and still finite.
pub const ALERT_MAX_RETRY_AFTER_SECONDS: u64 = 600;

/// How long one dispatcher's claim on a delivery lasts (REV-076-F03 lease).
///
/// A crashed dispatcher forfeits the claim at expiry; a living one finishes well
/// inside it. While the lease stands, no other dispatcher may re-claim — that is
/// what bounds the duplicate-delivery window when the provider accepted a send we
/// crashed after.
pub const ALERT_CLAIM_LEASE_SECONDS: i64 = 120;

/// Identity for one alert delivery: kind + subject + workspace + destination.
///
/// REV-074-F04 replaced the second-truncated timestamp with the immutable signal
/// id. REV-076-F04 completes it: the destination is part of the identity, so a
/// chat-id change or a second destination is a NEW delivery, not a swallowed one.
pub fn alert_dedup_key(kind: &str, workspace_id: i64, subject_id: i64, destination: &str) -> String {
    format!("{kind}:{workspace_id}:{subject_id}:{destination}")
}

/// The outcome of one claim attempt (REV-076-F03).
pub struct AlertClaim {
    /// Fencing token for this claim; completion must present it.
    pub token: String,
    /// The attempt number this delivery is on (claims count as attempts: a crash
    /// after the claim but before completion still consumed one).
    pub attempt: i32,
}

/// Claim one delivery row atomically (REV-076-F03).
///
/// ONE statement decides it, for all three row states:
///
///   * no row yet        → INSERT pending with the fencing token;
///   * failed row, retry due → UPDATE re-claim (state/attempt/next_attempt_at
///     checked in the WHERE, so a concurrent claimant's write makes the predicate
///     false and this one matches nothing);
///   * row currently claimed (unexpired lease), sent, or dead → no row matches.
///
/// REV-076 fixed the race REV-075 shipped: the old two-statement claim let an
/// INSERT loser immediately satisfy the UPDATE predicate because the fresh row's
/// `next_attempt_at` was `now()`. One statement has no such window.
///
/// The claim increments `attempt_count`: a crash after the HTTP call but before
/// completion consumed an attempt, and counting it is the only thing that makes
/// `ALERT_MAX_ATTEMPTS` a real cap rather than a per-process one.
pub async fn claim_alert(
    db: &PgPool,
    kind: &str,
    workspace_id: i64,
    subject_id: i64,
    destination: &str,
) -> Result<Option<AlertClaim>> {
    let key = alert_dedup_key(kind, workspace_id, subject_id, destination);
    let token = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    // REV-080-F03 (HIGH): exhaustion is terminalized AT the decision point, not by
    // a one-time migration sweep. A crash on the final attempt leaves a pending
    // row at the cap; the migration cannot repair future exhaustion. Before this
    // claim is evaluated, this row (and only rows like it) transitions
    // pending → dead atomically, so the outbox never again lies about open work.
    //
    // REV-082-F05 (HIGH): the terminalization must NOT kill a row with a LIVE
    // claim. Two dispatchers can preselect an attempt-7 row: A claims attempt 8
    // with a live lease; B enters claim_alert and an unconditional cap UPDATE
    // would mark A's live row dead and clear A's token. A can deliver externally
    // but cannot record sent. Guard: only terminalize when there is no active
    // claim (claim_expires_at is null or expired).
    sqlx::query(
        r#"
        UPDATE alerts
           SET state = 'dead', next_attempt_at = NULL,
               claim_token = NULL, claim_expires_at = NULL
         WHERE dedup_key = $1
           AND state = 'pending'
           AND attempt_count >= $2
           AND (claim_expires_at IS NULL OR claim_expires_at <= now())
        "#,
    )
    .bind(&key)
    .bind(ALERT_MAX_ATTEMPTS)
    .execute(db)
    .await?;
    // REV-078-F03: the subject is TYPED. `signal_id` references signals(id), so a
    // funding case id inserted there violated the FK and the funding path never
    // entered the outbox at all. Exactly one of the two columns is set.
    let (signal_id, funding_case_id) = match kind {
        "signal" => (Some(subject_id), None),
        "funding" => (None, Some(subject_id)),
        other => anyhow::bail!("unknown alert subject kind {other:?}"),
    };
    // REV-087-F04 (HIGH): the caller supplies the workspace and the subject id from
    // two DIFFERENT sources — workers.rs pairs `ctx.workspace.id()` with a case id
    // read off the case row. Nothing bound them together, so a malformed pairing
    // could mint an outbox row that delivers another tenant's case text through this
    // tenant's destination. Migration 1040 makes that a database error; this check
    // makes it a NAMED refusal at the boundary that created it, instead of an opaque
    // 23503 surfacing from the outbox.
    if let Some(case_id) = funding_case_id {
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM funding_radar_cases \
              WHERE id = $1 AND workspace_id = $2)",
        )
        .bind(case_id)
        .bind(workspace_id)
        .fetch_one(db)
        .await?;
        if !owned {
            anyhow::bail!(
                "funding case {case_id} is not owned by workspace {workspace_id}; \
                 refusing to mint a cross-tenant alert"
            );
        }
    }
    let row: Option<i32> = sqlx::query_scalar(
        r#"
        INSERT INTO alerts
            (dedup_key, subject_kind, workspace_id, signal_id, funding_case_id, destination, state,
             attempt_count, next_attempt_at, claim_token, claim_expires_at)
        SELECT $1, $2, $9, $3, $4, $5, 'pending', 1, now() + make_interval(secs => $6),
               $7, now() + make_interval(secs => $6)
        ON CONFLICT (dedup_key) DO UPDATE
            SET claim_token = $7,
                claim_expires_at = now() + make_interval(secs => $6),
                next_attempt_at = now() + make_interval(secs => $6),
                attempt_count = alerts.attempt_count + 1
            WHERE alerts.state = 'pending'
              AND alerts.next_attempt_at <= now()
              AND (alerts.claim_expires_at IS NULL OR alerts.claim_expires_at <= now())
              AND alerts.attempt_count < $8
        RETURNING attempt_count
        "#,
    )
    .bind(&key)
    .bind(kind)
    .bind(signal_id)
    .bind(funding_case_id)
    .bind(destination)
    .bind(ALERT_CLAIM_LEASE_SECONDS as f64)
    .bind(&token)
    .bind(ALERT_MAX_ATTEMPTS)
    .bind(workspace_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|attempt| AlertClaim { token, attempt }))
}

/// Mark a claimed alert delivered. Fenced: only the row still held by THIS claim
/// token updates, and exactly one row must update — zero means the claim was
/// stale (expired and re-claimed by someone else) and the caller must NOT treat
/// the delivery as recorded.
pub async fn mark_alert_sent(
    db: &PgPool,
    kind: &str,
    workspace_id: i64,
    subject_id: i64,
    destination: &str,
    claim_token: &str,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        UPDATE alerts
           SET state = 'sent', sent_at = now(), last_error = NULL,
               claim_token = NULL, claim_expires_at = NULL, next_attempt_at = NULL
         WHERE dedup_key = $1
           AND state = 'pending'
           AND claim_token = $2
        "#,
    )
    .bind(alert_dedup_key(kind, workspace_id, subject_id, destination))
    .bind(claim_token)
    .execute(db)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Record a failed delivery attempt. Fenced like `mark_alert_sent`.
///
/// Transient failures schedule the next retry with bounded exponential backoff
/// (2^attempt minutes, capped at one hour); a permanent failure is `dead`
/// immediately. The attempt count was already incremented by the claim.
pub async fn mark_alert_failed(
    db: &PgPool,
    kind: &str,
    workspace_id: i64,
    subject_id: i64,
    destination: &str,
    claim_token: &str,
    attempt: i32,
    error: &str,
    permanent: bool,
    retry_after_seconds: Option<u64>,
) -> Result<bool> {
    let backoff_seconds: u64 = retry_after_seconds
        .map(|s| s.min(ALERT_MAX_RETRY_AFTER_SECONDS))
        .unwrap_or_else(|| {
            let exp = (attempt as u32).min(6);
            (60u64 << exp).min(3600)
        });
    let dead = permanent || attempt >= ALERT_MAX_ATTEMPTS;
    let result = sqlx::query(
        r#"
        UPDATE alerts
           SET last_error = $3,
               state = CASE WHEN $4 THEN 'dead' ELSE 'pending' END,
               next_attempt_at = CASE WHEN $4 THEN NULL
                                      ELSE now() + make_interval(secs => $5::double precision) END,
               claim_token = NULL,
               claim_expires_at = NULL
         WHERE dedup_key = $1
           AND state = 'pending'
           AND claim_token = $2
        "#,
    )
    .bind(alert_dedup_key(kind, workspace_id, subject_id, destination))
    .bind(claim_token)
    .bind(error)
    .bind(dead)
    .bind(backoff_seconds as f64)
    .execute(db)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SignalsConfig {
        SignalsConfig {
            entry_max_token_age_hours: 24,
            entry_min_liquidity_usd: 20_000,
            market_max_age_seconds: 300,
            exit_liquidity_drop_ratio: 0.30,
        }
    }

    #[test]
    fn rejection_codes_unique() {
        let mut sorted = REJECTION_CODES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), REJECTION_CODES.len(), "rejection codes must be unique");
    }

    #[test]
    fn alert_dedup_key_includes_all_parts() {
        // REV-076-F04: identity is kind + workspace + subject + destination —
        // immutable, so two chains, two signals, or two destinations can never
        // collide (the second-truncated key dropped the second alert; a
        // destination change suppressed legitimate delivery).
        let key = alert_dedup_key("signal", 7, 42, "chat-a");
        assert_eq!(key, "signal:7:42:chat-a");
        assert_ne!(key, alert_dedup_key("signal", 7, 43, "chat-a"));
        assert_ne!(key, alert_dedup_key("signal", 8, 42, "chat-a"));
        assert_ne!(key, alert_dedup_key("signal", 7, 42, "chat-b"));
        assert_ne!(key, alert_dedup_key("funding", 7, 42, "chat-a"));
    }

    #[test]
    fn entry_score_deterministic() {
        let clusters = ClusterSummary {
            eligible_clusters: 2,
            meaningful_buys: 5,
        };
        let wallets = WalletEligibility {
            eligible_wallets: 3,
            max_skill: 80,
            max_copyability: 70,
            min_history_completeness: Decimal::ONE,
        };
        let s1 = compute_entry_score(&clusters, &wallets, Some(60));
        let s2 = compute_entry_score(&clusters, &wallets, Some(60));
        assert_eq!(s1, s2);
        assert!(s1 > 0 && s1 <= 100);
    }

    #[test]
    fn exit_gates_computed_from_summary() {
        let _config = config();
        let gates = ExitGates {
            clusters_selling: 2,
            material_fresh_transfer: false,
            dev_distribution: false,
            liquidity_drop_ratio: None,
            tracked_selling: false,
        };
        // Two clusters selling triggers without liquidity data.
        let liquidity_drop_ok = gates
            .liquidity_drop_ratio
            .map(|r| r >= Decimal::from(30).checked_div(Decimal::from(100)).unwrap())
            .unwrap_or(false);
        let triggered = gates.clusters_selling >= 2
            || gates.material_fresh_transfer
            || gates.dev_distribution
            || (liquidity_drop_ok && gates.tracked_selling);
        assert!(triggered);
    }
}
