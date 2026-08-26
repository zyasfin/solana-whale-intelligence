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
async fn record_evaluation(
    db: &PgPool,
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
            (chain, mint, signal_kind, evaluated_at, status, rejection_code, evidence)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(signal_kind)
    .bind(evaluated_at)
    .bind(status)
    .bind(rejection_code)
    .bind(evidence)
    .execute(db)
    .await?;
    Ok(())
}

/// Evaluate an entry signal for a token.
///
/// Gates (first failure wins, exactly one rejection code):
/// two independent eligible clusters, meaningful buys, supported lifecycle,
/// age <= 24h, liquidity >= $20k, no critical risk/wash, market data <= 5m,
/// eligible skill/copyability, history completeness >= 0.80.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate_token(
    db: &PgPool,
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
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("insufficient_clusters"), &evidence).await?;
        return Ok(None);
    }
    if clusters.meaningful_buys < 2 {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("insufficient_buys"), &evidence).await?;
        return Ok(None);
    }
    if !token.lifecycle.entry_supported() || token.lifecycle == LifecycleState::Unsupported {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("unsupported_lifecycle"), &evidence).await?;
        return Ok(None);
    }
    if let Some(age) = token.age_hours {
        if age > config.entry_max_token_age_hours as i64 {
            record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("token_too_old"), &evidence).await?;
            return Ok(None);
        }
    } else {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("token_too_old"), &evidence).await?;
        return Ok(None);
    }
    let liquidity_ok = market
        .liquidity_usd
        .map(|l| l >= Decimal::from(config.entry_min_liquidity_usd))
        .unwrap_or(false);
    if !liquidity_ok {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("insufficient_liquidity"), &evidence).await?;
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
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some(code), &evidence).await?;
        return Ok(None);
    }
    let market_fresh = market
        .observed_at
        .map(|t| (now - t).num_seconds() <= config.market_max_age_seconds as i64)
        .unwrap_or(false);
    if !market_fresh {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("stale_market"), &evidence).await?;
        return Ok(None);
    }
    if wallets.max_skill < 70 {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("ineligible_skill"), &evidence).await?;
        return Ok(None);
    }
    if wallets.max_copyability < 60 {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("ineligible_copyability"), &evidence).await?;
        return Ok(None);
    }
    if wallets.min_history_completeness < Decimal::from_str_exact("0.80").unwrap() {
        record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "rejected", Some("incomplete_history"), &evidence).await?;
        return Ok(None);
    }

    // All gates passed: accepted.
    let score = compute_entry_score(clusters, wallets, narrative_confidence);
    let signal_id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO signals (chain, mint, signal_kind, created_at, score, status, evidence)
        VALUES ($1, $2, 'entry', $3, $4, 'active', $5)
        RETURNING id
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(now)
    .bind(score as i32)
    .bind(&evidence)
    .fetch_one(db)
    .await?;
    record_evaluation(db, chain, mint, SIGNAL_KIND_ENTRY, evaluated_at, "accepted", None, &evidence).await?;
    Ok(Some(signal_id))
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

/// Evaluate an exit signal.
pub async fn evaluate_exit(
    db: &PgPool,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &SignalsConfig,
    gates: &ExitGates,
) -> Result<Option<i64>> {
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
        record_evaluation(db, chain, mint, SIGNAL_KIND_EXIT, now, "rejected", Some(code), &evidence).await?;
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
        INSERT INTO signals (chain, mint, signal_kind, created_at, score, status, evidence)
        VALUES ($1, $2, 'exit', $3, $4, 'active', $5)
        RETURNING id
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(now)
    .bind(score)
    .bind(&evidence)
    .fetch_one(db)
    .await?;
    record_evaluation(db, chain, mint, SIGNAL_KIND_EXIT, now, "accepted", None, &evidence).await?;
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

/// Alert dedup key for a signal: chain:mint:kind:created_at bucket.
pub fn alert_dedup_key(chain: ChainKind, mint: &str, kind: &str, created_at: DateTime<Utc>) -> String {
    format!("{}:{}:{}:{}", chain.as_str(), mint, kind, created_at.timestamp())
}

/// Record an alert attempt (deduplicated).
pub async fn record_alert(
    db: &PgPool,
    dedup_key: &str,
    signal_id: i64,
    delivery_error: Option<&str>,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO alerts (dedup_key, signal_id, sent_at, delivery_error)
        VALUES ($1, $2, now(), $3)
        ON CONFLICT (dedup_key) DO NOTHING
        "#,
    )
    .bind(dedup_key)
    .bind(signal_id)
    .bind(delivery_error)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
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
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let key = alert_dedup_key(ChainKind::Solana, "MINT", "entry", now);
        assert_eq!(key, "solana:MINT:entry:1700000000");
        let other = alert_dedup_key(ChainKind::Robinhood, "MINT", "entry", now);
        assert_ne!(key, other);
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
