//! Temporal replay: re-evaluate signals using only evidence observed at or
//! before the evaluation time. Prevents look-ahead bias.
//!
//! Unknown historical availability yields `incomplete_history_replay` or
//! `stale_market_replay`; never current-data leakage.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::models::ChainKind;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

/// Replay guard outcome for one observation.
#[derive(Clone, Debug, PartialEq)]
pub enum GuardResult {
    /// Observation is usable (observed_at <= evaluation_time).
    Usable,
    /// Observation postdates evaluation time: excluded.
    Excluded,
    /// No observation exists at or before evaluation time.
    Unavailable,
}

/// Evaluate one observation against the temporal guard.
pub fn guard(observed_at: Option<DateTime<Utc>>, evaluation_time: DateTime<Utc>) -> GuardResult {
    match observed_at {
        Some(at) if at <= evaluation_time => GuardResult::Usable,
        Some(_) => GuardResult::Excluded,
        None => GuardResult::Unavailable,
    }
}

/// A market snapshot filtered by the temporal guard.
#[derive(Clone, Debug)]
pub struct ReplayMarket {
    pub liquidity_usd: Option<Decimal>,
    pub price_usd: Option<Decimal>,
    pub observed_at: Option<DateTime<Utc>>,
    pub status: &'static str,
}

/// Load the latest market snapshot usable at evaluation_time.
///
/// Returns `stale_market_replay` when no snapshot exists at or before the
/// evaluation time.
pub async fn market_at(
    db: &PgPool,
    chain: ChainKind,
    mint: &str,
    evaluation_time: DateTime<Utc>,
) -> Result<ReplayMarket> {
    let row: Option<(Option<Decimal>, Option<Decimal>, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT liquidity_usd, price_usd, observed_at
          FROM market_snapshots
         WHERE chain = $1 AND mint = $2 AND observed_at <= $3
         ORDER BY observed_at DESC
         LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(evaluation_time)
    .fetch_optional(db)
    .await?;
    match row {
        Some((liquidity, price, observed_at)) => Ok(ReplayMarket {
            liquidity_usd: liquidity,
            price_usd: price,
            observed_at,
            status: "usable",
        }),
        None => Ok(ReplayMarket {
            liquidity_usd: None,
            price_usd: None,
            observed_at: None,
            status: "stale_market_replay",
        }),
    }
}

/// A wallet score row filtered by the temporal guard.
#[derive(Clone, Debug)]
pub struct ReplayScore {
    pub skill_score: u32,
    pub copyability_score: u32,
    pub conviction: u32,
    pub history_completeness: Decimal,
    pub observed_at: Option<DateTime<Utc>>,
    pub status: &'static str,
}

/// Load the latest wallet score usable at evaluation_time.
pub async fn wallet_score_at(
    db: &PgPool,
    chain: ChainKind,
    address: &str,
    evaluation_time: DateTime<Utc>,
) -> Result<ReplayScore> {
    let row: Option<(i32, i32, i32, Decimal, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT skill_score, copyability_score, conviction, history_completeness, as_of
          FROM wallet_scores
         WHERE chain = $1 AND address = $2 AND as_of <= $3
         ORDER BY as_of DESC
         LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .bind(evaluation_time)
    .fetch_optional(db)
    .await?;
    match row {
        Some((skill, copyability, conviction, completeness, as_of)) => Ok(ReplayScore {
            skill_score: skill.max(0) as u32,
            copyability_score: copyability.max(0) as u32,
            conviction: conviction.max(0) as u32,
            history_completeness: completeness,
            observed_at: as_of,
            status: "usable",
        }),
        None => Ok(ReplayScore {
            skill_score: 0,
            copyability_score: 0,
            conviction: 0,
            history_completeness: Decimal::ZERO,
            observed_at: None,
            status: "incomplete_history_replay",
        }),
    }
}

/// A GMGN observation filtered by the temporal guard.
pub struct ReplayGmgn {
    pub payload: serde_json::Value,
    pub observed_at: Option<DateTime<Utc>>,
    pub usable: bool,
}

/// Load the latest GMGN token observation usable at evaluation_time.
pub async fn gmgn_token_at(
    db: &PgPool,
    chain: ChainKind,
    mint: &str,
    endpoint: &str,
    evaluation_time: DateTime<Utc>,
) -> Result<Option<ReplayGmgn>> {
    let row: Option<(serde_json::Value, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT payload, observed_at
          FROM gmgn_token_observations
         WHERE chain = $1 AND mint = $2 AND endpoint = $3 AND observed_at <= $4
         ORDER BY observed_at DESC
         LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(endpoint)
    .bind(evaluation_time)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(payload, observed_at)| ReplayGmgn {
        payload,
        observed_at,
        usable: true,
    }))
}

/// A Telegram message mention filtered by the temporal guard.
pub struct ReplayMention {
    pub mint: Option<String>,
    pub mention_kind: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub usable: bool,
}

/// Load Telegram mentions usable at evaluation_time.
pub async fn telegram_mentions_at(
    db: &PgPool,
    mint: &str,
    evaluation_time: DateTime<Utc>,
) -> Result<Vec<ReplayMention>> {
    let rows: Vec<(Option<String>, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT tm.mint, tm.mention_kind, m.observed_at
          FROM telegram_mentions tm
          JOIN telegram_messages m
            ON m.channel_key = tm.channel_key
           AND m.message_id = tm.message_id
           AND m.edit_version = tm.edit_version
         WHERE tm.mint = $1 AND m.observed_at <= $2
        "#,
    )
    .bind(mint)
    .bind(evaluation_time)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(mint, mention_kind, observed_at)| ReplayMention {
            mint,
            mention_kind,
            observed_at,
            usable: true,
        })
        .collect())
}

/// Replay report aggregating evaluation outcomes.
#[derive(Clone, Debug, Default)]
pub struct ReplayReport {
    pub evaluated: u32,
    pub accepted: u32,
    pub rejected: u32,
    pub rejections_by_code: std::collections::BTreeMap<String, u32>,
    pub incomplete_history: u32,
    pub stale_market: u32,
}

/// Paper-trade outcome metrics for one replayed signal.
#[derive(Clone, Debug, Default)]
pub struct PaperMetrics {
    pub latency_fill_seconds: Option<i64>,
    pub slippage: Option<Decimal>,
    pub max_favorable_excursion: Option<Decimal>,
    pub max_adverse_excursion: Option<Decimal>,
    pub liquidity_1m_usd: Option<Decimal>,
    pub liquidity_5m_usd: Option<Decimal>,
    pub liquidity_15m_usd: Option<Decimal>,
    pub sell_before_copy: bool,
}

/// Compute paper metrics for a signal using only post-signal observations.
pub async fn paper_metrics(
    db: &PgPool,
    chain: ChainKind,
    mint: &str,
    signal_at: DateTime<Utc>,
) -> Result<PaperMetrics> {
    let rows: Vec<(Option<Decimal>, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT liquidity_usd, observed_at
          FROM market_snapshots
         WHERE chain = $1 AND mint = $2 AND observed_at > $3
         ORDER BY observed_at ASC
         LIMIT 50
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(signal_at)
    .fetch_all(db)
    .await?;

    let mut metrics = PaperMetrics::default();
    for (liquidity, observed_at) in &rows {
        let elapsed = observed_at.map(|t| (t - signal_at).num_seconds()).unwrap_or(i64::MAX);
        let liquidity = *liquidity;
        if elapsed <= 60 && metrics.liquidity_1m_usd.is_none() {
            metrics.liquidity_1m_usd = liquidity;
        }
        if elapsed <= 300 && metrics.liquidity_5m_usd.is_none() {
            metrics.liquidity_5m_usd = liquidity;
        }
        if elapsed <= 900 && metrics.liquidity_15m_usd.is_none() {
            metrics.liquidity_15m_usd = liquidity;
        }
    }
    Ok(metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_excludes_future_observations() {
        let evaluation = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let before = DateTime::from_timestamp(1_699_999_000, 0).unwrap();
        let after = DateTime::from_timestamp(1_700_001_000, 0).unwrap();

        assert_eq!(guard(Some(before), evaluation), GuardResult::Usable);
        assert_eq!(guard(Some(evaluation), evaluation), GuardResult::Usable);
        assert_eq!(guard(Some(after), evaluation), GuardResult::Excluded);
        assert_eq!(guard(None, evaluation), GuardResult::Unavailable);
    }

    #[test]
    fn replay_report_tracks_codes() {
        let mut report = ReplayReport::default();
        report.evaluated += 1;
        report.rejected += 1;
        *report.rejections_by_code.entry("insufficient_clusters".into()).or_insert(0) += 1;
        assert_eq!(report.rejections_by_code.get("insufficient_clusters"), Some(&1));
        assert_eq!(report.evaluated, 1);
        assert_eq!(report.accepted, 0);
    }

    #[test]
    fn paper_metrics_defaults_when_no_snapshots() {
        let metrics = PaperMetrics::default();
        assert!(metrics.liquidity_1m_usd.is_none());
        assert!(!metrics.sell_before_copy);
    }
}
