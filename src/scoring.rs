//! Wallet scoring: FIFO trade matching, skill and copyability scores.
//!
//! `skill_score` and `copyability_score` are separate deterministic metrics.
//! Ultra-fast profitable wallets can score high skill but low copyability.
//! Conviction is capped at 49 while history completeness is below 0.80.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::config::ScoringConfig;
use crate::models::{ChainKind, TradeSide};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

/// Convert an f64 threshold into a Decimal via string parsing (no float math).
pub fn decimal_from_f64(value: f64) -> Decimal {
    Decimal::from_str_exact(&format!("{value}")).unwrap_or(Decimal::ZERO)
}


/// One trade used in scoring.
#[derive(Clone, Debug)]
pub struct ScoredTrade {
    pub mint: String,
    pub side: TradeSide,
    pub token_amount: Decimal,
    pub usd_value: Option<Decimal>,
    pub block_time: Option<DateTime<Utc>>,
}

/// FIFO matched position for one token.
#[derive(Clone, Debug)]
pub struct MatchedPosition {
    pub mint: String,
    pub realized_pnl_usd: Option<Decimal>,
    pub roi: Option<Decimal>,
    pub hold_seconds: Option<i64>,
}

/// FIFO match trades by (wallet, mint): buys open, sells close.
pub fn fifo_match(trades: &[ScoredTrade]) -> Vec<MatchedPosition> {
    use std::collections::HashMap;
    struct OpenLot {
        amount: Decimal,
        usd: Decimal,
        time: Option<DateTime<Utc>>,
    }
    let mut open: HashMap<String, Vec<OpenLot>> = HashMap::new();
    let mut matched: Vec<MatchedPosition> = Vec::new();
    let mut totals: HashMap<String, (Decimal, Decimal)> = HashMap::new(); // mint -> (cost, proceeds)

    for trade in trades {
        match trade.side {
            TradeSide::Buy => {
                let usd = trade
                    .usd_value
                    .or_else(|| trade.token_amount.checked_mul(Decimal::ZERO))
                    .unwrap_or(Decimal::ZERO);
                open.entry(trade.mint.clone()).or_default().push(OpenLot {
                    amount: trade.token_amount,
                    usd,
                    time: trade.block_time,
                });
            }
            TradeSide::Sell => {
                let lots = open.entry(trade.mint.clone()).or_default();
                let mut remaining = trade.token_amount;
                let mut cost = Decimal::ZERO;
                let mut open_time: Option<DateTime<Utc>> = None;
                while remaining > Decimal::ZERO {
                    let Some(lot) = lots.first_mut() else { break };
                    let take = remaining.min(lot.amount);
                    if lot.usd != Decimal::ZERO && lot.amount != Decimal::ZERO {
                        cost += lot.usd * (take / lot.amount);
                    }
                    if open_time.is_none() {
                        open_time = lot.time;
                    }
                    lot.amount -= take;
                    remaining -= take;
                    if lot.amount == Decimal::ZERO {
                        lots.remove(0);
                    }
                }
                let proceeds = trade.usd_value.unwrap_or(Decimal::ZERO);
                let entry = totals.entry(trade.mint.clone()).or_insert((Decimal::ZERO, Decimal::ZERO));
                entry.0 += cost;
                entry.1 += proceeds;
                matched.push(MatchedPosition {
                    mint: trade.mint.clone(),
                    realized_pnl_usd: Some(proceeds - cost),
                    roi: if cost != Decimal::ZERO {
                        Some((proceeds - cost) / cost)
                    } else {
                        None
                    },
                    hold_seconds: match (open_time, trade.block_time) {
                        (Some(opened), Some(closed)) => Some((closed - opened).num_seconds().max(0)),
                        _ => None,
                    },
                });
            }
        }
    }
    matched
}

/// Aggregate scoring inputs.
#[derive(Clone, Debug)]
pub struct ScoreInputs {
    pub meaningful_trades: u32,
    pub tokens_traded: u32,
    pub realized_pnl_usd: Decimal,
    pub size_weighted_roi: Decimal,
    pub meaningful_win_rate: Decimal,
    pub early_entry_rate: Decimal,
    pub history_completeness: Decimal,
    pub mev_likelihood: Decimal,
    pub avg_hold_seconds: Option<i64>,
    pub gmgn_agreement: Option<Decimal>,
    pub liquidity_available: Option<Decimal>,
}

impl Default for ScoreInputs {
    fn default() -> Self {
        Self {
            meaningful_trades: 0,
            tokens_traded: 0,
            realized_pnl_usd: Decimal::ZERO,
            size_weighted_roi: Decimal::ZERO,
            meaningful_win_rate: Decimal::ZERO,
            early_entry_rate: Decimal::ZERO,
            history_completeness: Decimal::ZERO,
            mev_likelihood: Decimal::ZERO,
            avg_hold_seconds: None,
            gmgn_agreement: None,
            liquidity_available: None,
        }
    }
}

/// Compute the deterministic `skill_score`.
///
/// Weights: ROI 30%, win rate 20%, early entry 15%, consistency 10%,
/// freshness/sample 10%, GMGN/Helius agreement 15%.
pub fn skill_score(inputs: &ScoreInputs) -> u32 {
    let roi_component = normalize_ratio(inputs.size_weighted_roi) * Decimal::from(30);
    let win_component = inputs.meaningful_win_rate.clamp(Decimal::ZERO, Decimal::ONE) * Decimal::from(20);
    let early_component = inputs.early_entry_rate.clamp(Decimal::ZERO, Decimal::ONE) * Decimal::from(15);
    // Consistency: win rate near 0.5 with many trades is consistent behavior.
    let consistency = Decimal::ONE - (inputs.meaningful_win_rate - Decimal::from_str_exact("0.5").unwrap()).abs();
    let consistency_component = consistency.clamp(Decimal::ZERO, Decimal::ONE) * Decimal::from(10);
    // Freshness/sample: scaled by trade count up to the minimum requirement.
    let sample = Decimal::from(inputs.meaningful_trades.min(50)) / Decimal::from(50);
    let sample_component = sample * Decimal::from(10);
    let agreement_component = inputs
        .gmgn_agreement
        .unwrap_or(Decimal::from_str_exact("0.5").unwrap())
        .clamp(Decimal::ZERO, Decimal::ONE)
        * Decimal::from(15);

    let total = roi_component + win_component + early_component + consistency_component + sample_component + agreement_component;
    total.clamp(Decimal::ZERO, Decimal::from(100)).to_u32().unwrap_or(0)
}

/// Copyability penalties (each recorded in evidence).
#[derive(Clone, Debug, Default)]
pub struct CopyabilityPenalties {
    pub hold_penalty: u32,
    pub liquidity_penalty: u32,
    pub drift_penalty: u32,
    pub sell_race_penalty: u32,
    pub bot_penalty: u32,
}


/// Compute the deterministic `copyability_score`.
///
/// `clamp(100 - hold_penalty - liquidity_penalty - drift_penalty -
/// sell_race_penalty - bot_penalty, 0, 100)` with exact penalties recorded.
pub fn copyability_score(inputs: &ScoreInputs) -> (u32, CopyabilityPenalties) {
    let mut penalties = CopyabilityPenalties::default();

    // Hold penalty: ultra-short average holds are not copyable.
    if let Some(hold) = inputs.avg_hold_seconds {
        if hold <= 5 {
            penalties.hold_penalty = 60;
        } else if hold <= 60 {
            penalties.hold_penalty = 30;
        } else if hold <= 300 {
            penalties.hold_penalty = 10;
        }
    }

    // Liquidity penalty: thin liquidity cannot absorb copied orders.
    if let Some(liquidity) = inputs.liquidity_available {
        if liquidity < Decimal::from(10_000) {
            penalties.liquidity_penalty = 25;
        } else if liquidity < Decimal::from(50_000) {
            penalties.liquidity_penalty = 10;
        }
    }

    // Drift penalty: high MEV likelihood means price moves before copies fill.
    if inputs.mev_likelihood >= Decimal::from_str_exact("0.8").unwrap() {
        penalties.drift_penalty = 25;
    } else if inputs.mev_likelihood >= Decimal::from_str_exact("0.5").unwrap() {
        penalties.drift_penalty = 10;
    }

    // Sell race penalty: wallets that sell before copies can fill.
    if let Some(hold) = inputs.avg_hold_seconds {
        if hold <= 5 {
            penalties.sell_race_penalty = 15;
        }
    }

    // Bot penalty: MEV/bot behavior is not copyable.
    if inputs.mev_likelihood >= Decimal::from_str_exact("0.9").unwrap() {
        penalties.bot_penalty = 30;
    }

    let total_penalty = penalties.hold_penalty
        + penalties.liquidity_penalty
        + penalties.drift_penalty
        + penalties.sell_race_penalty
        + penalties.bot_penalty;
    let score = 100i64.saturating_sub(total_penalty as i64).clamp(0, 100);
    (score as u32, penalties)
}

/// Normalize a ratio-like ROI into a 0..1 scale for scoring.
fn normalize_ratio(value: Decimal) -> Decimal {
    // ROI above 5x saturates; below -100% floors at zero.
    let clamped = value.clamp(Decimal::from(-1), Decimal::from(5));
    (clamped + Decimal::ONE) / Decimal::from(6)
}

use rust_decimal::prelude::ToPrimitive;

/// Full scoring outcome for one wallet.
#[derive(Clone, Debug)]
pub struct WalletScoreResult {
    pub skill: u32,
    pub copyability: u32,
    pub conviction: u32,
    pub provisional: bool,
    pub history_completeness: Decimal,
    pub penalties: CopyabilityPenalties,
}

/// Compute the full score result including conviction cap.
///
/// Conviction is capped at `conviction_cap_below_completeness` (49) while
/// history completeness is below the floor (0.80). Provisional status applies
/// below the minimum meaningful-trade/token requirements.
pub fn compute_wallet_score(inputs: &ScoreInputs, config: &ScoringConfig) -> WalletScoreResult {
    let skill = skill_score(inputs);
    let (copyability, penalties) = copyability_score(inputs);
    let provisional = inputs.meaningful_trades < config.min_meaningful_trades
        || inputs.tokens_traded < config.min_tokens_traded;

    let base_conviction = ((Decimal::from(skill) + Decimal::from(copyability)) / Decimal::from(2))
        .to_u32()
        .unwrap_or(0);

    let conviction = if inputs.history_completeness < Decimal::from_str_exact(&format!("{}", config.history_completeness_floor)).unwrap_or_else(|_| Decimal::from_str_exact("0.8").unwrap()) {
        base_conviction.min(config.conviction_cap_below_completeness)
    } else {
        base_conviction.min(100)
    };

    WalletScoreResult {
        skill,
        copyability,
        conviction,
        provisional,
        history_completeness: inputs.history_completeness,
        penalties,
    }
}

/// Whether a wallet can contribute fully to entry signals.
pub fn full_entry_eligible(result: &WalletScoreResult, config: &ScoringConfig) -> bool {
    result.skill >= config.full_skill_score
        && result.copyability >= config.full_copyability_score
        && !result.provisional
}

/// Persist a wallet score row (idempotent per as_of).
#[allow(clippy::too_many_arguments)]
pub async fn store_wallet_score(
    db: &PgPool,
    chain: ChainKind,
    address: &str,
    as_of: DateTime<Utc>,
    inputs: &ScoreInputs,
    result: &WalletScoreResult,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO wallet_scores
            (chain, address, as_of, realized_pnl_usd, size_weighted_roi, meaningful_win_rate,
             early_entry_rate, skill_score, copyability_score, history_completeness,
             mev_likelihood, conviction, provisional, evidence)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        ON CONFLICT (chain, address, as_of) DO NOTHING
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .bind(as_of)
    .bind(inputs.realized_pnl_usd)
    .bind(inputs.size_weighted_roi)
    .bind(inputs.meaningful_win_rate)
    .bind(inputs.early_entry_rate)
    .bind(result.skill as i32)
    .bind(result.copyability as i32)
    .bind(inputs.history_completeness)
    .bind(inputs.mev_likelihood)
    .bind(result.conviction as i32)
    .bind(result.provisional)
    .bind(serde_json::json!({
        "penalties": {
            "hold": result.penalties.hold_penalty,
            "liquidity": result.penalties.liquidity_penalty,
            "drift": result.penalties.drift_penalty,
            "sell_race": result.penalties.sell_race_penalty,
            "bot": result.penalties.bot_penalty,
        },
        "meaningful_trades": inputs.meaningful_trades,
        "tokens_traded": inputs.tokens_traded,
    }))
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ScoringConfig {
        ScoringConfig {
            min_meaningful_trades: 20,
            min_tokens_traded: 5,
            full_skill_score: 70,
            full_copyability_score: 60,
            history_completeness_floor: 0.80,
            conviction_cap_below_completeness: 49,
        }
    }

    fn trade(mint: &str, side: TradeSide, amount: Decimal, usd: Decimal, seconds: i64) -> ScoredTrade {
        ScoredTrade {
            mint: mint.to_string(),
            side,
            token_amount: amount,
            usd_value: Some(usd),
            block_time: DateTime::from_timestamp(1_700_000_000 + seconds, 0),
        }
    }

    #[test]
    fn fifo_matching_computes_pnl_and_roi() {
        let trades = vec![
            trade("MINTA", TradeSide::Buy, Decimal::from(100), Decimal::from(50), 0),
            trade("MINTA", TradeSide::Sell, Decimal::from(100), Decimal::from(80), 3600),
        ];
        let matched = fifo_match(&trades);
        assert_eq!(matched.len(), 1);
        let position = &matched[0];
        assert_eq!(position.realized_pnl_usd, Some(Decimal::from(30)));
        assert_eq!(position.roi, Some(Decimal::from_str_exact("0.6").unwrap()));
        assert_eq!(position.hold_seconds, Some(3600));
    }

    #[test]
    fn large_trades_dominate_over_tiny_ones() {
        // One large profitable trade and many tiny rapid losses: the score
        // inputs must be driven by meaningful trades.
        let mut trades = vec![
            trade("MINTA", TradeSide::Buy, Decimal::from(1_000), Decimal::from(2_000), 0),
            trade("MINTA", TradeSide::Sell, Decimal::from(1_000), Decimal::from(4_000), 60),
        ];
        for i in 0..10 {
            trades.push(trade("TINY", TradeSide::Buy, Decimal::from(1), Decimal::new(1, 2), i));
            trades.push(trade("TINY", TradeSide::Sell, Decimal::from(1), Decimal::new(5, 3), i + 1));
        }
        let matched = fifo_match(&trades);
        let pnl_sum: Decimal = matched
            .iter()
            .filter_map(|m| m.realized_pnl_usd)
            .sum();
        // Meaningful trade PnL (2000) dominates tiny losses (10 * -0.005).
        assert!(pnl_sum > Decimal::from(1_900));
    }

    #[test]
    fn provisional_below_minimum_trades() {
        let inputs = ScoreInputs {
            meaningful_trades: 10,
            tokens_traded: 5,
            history_completeness: Decimal::ONE,
            ..Default::default()
        };
        let result = compute_wallet_score(&inputs, &config());
        assert!(result.provisional);

        let inputs = ScoreInputs {
            meaningful_trades: 25,
            tokens_traded: 6,
            history_completeness: Decimal::ONE,
            ..Default::default()
        };
        let result = compute_wallet_score(&inputs, &config());
        assert!(!result.provisional);
    }

    #[test]
    fn incomplete_history_caps_conviction_at_49() {
        let inputs = ScoreInputs {
            meaningful_trades: 30,
            tokens_traded: 8,
            history_completeness: Decimal::from_str_exact("0.5").unwrap(),
            size_weighted_roi: Decimal::from(3),
            meaningful_win_rate: Decimal::from_str_exact("0.8").unwrap(),
            early_entry_rate: Decimal::from_str_exact("0.7").unwrap(),
            gmgn_agreement: Some(Decimal::ONE),
            ..Default::default()
        };
        let result = compute_wallet_score(&inputs, &config());
        assert!(result.conviction <= 49, "conviction must be capped below completeness floor");
    }

    #[test]
    fn complete_history_allows_full_conviction() {
        let inputs = ScoreInputs {
            meaningful_trades: 30,
            tokens_traded: 8,
            history_completeness: Decimal::ONE,
            size_weighted_roi: Decimal::from(3),
            meaningful_win_rate: Decimal::from_str_exact("0.8").unwrap(),
            early_entry_rate: Decimal::from_str_exact("0.7").unwrap(),
            gmgn_agreement: Some(Decimal::ONE),
            ..Default::default()
        };
        let result = compute_wallet_score(&inputs, &config());
        assert!(result.conviction > 49);
    }

    #[test]
    fn ultra_fast_profitable_wallet_scores_high_skill_low_copyability() {
        let inputs = ScoreInputs {
            meaningful_trades: 40,
            tokens_traded: 10,
            history_completeness: Decimal::ONE,
            size_weighted_roi: Decimal::from(4),
            meaningful_win_rate: Decimal::from_str_exact("0.9").unwrap(),
            early_entry_rate: Decimal::from_str_exact("0.9").unwrap(),
            avg_hold_seconds: Some(5),
            gmgn_agreement: Some(Decimal::ONE),
            ..Default::default()
        };
        let result = compute_wallet_score(&inputs, &config());
        assert!(result.skill >= 70, "high skill expected, got {}", result.skill);
        assert!(result.copyability < 60, "low copyability expected, got {}", result.copyability);
        assert!(!full_entry_eligible(&result, &config()));
    }

    #[test]
    fn slower_profitable_wallet_can_be_copyable() {
        let inputs = ScoreInputs {
            meaningful_trades: 40,
            tokens_traded: 10,
            history_completeness: Decimal::ONE,
            size_weighted_roi: Decimal::from(2),
            meaningful_win_rate: Decimal::from_str_exact("0.7").unwrap(),
            early_entry_rate: Decimal::from_str_exact("0.5").unwrap(),
            avg_hold_seconds: Some(600),
            liquidity_available: Some(Decimal::from(100_000)),
            gmgn_agreement: Some(Decimal::from_str_exact("0.8").unwrap()),
            ..Default::default()
        };
        let result = compute_wallet_score(&inputs, &config());
        assert!(result.copyability >= 60, "copyable expected, got {}", result.copyability);
    }

    #[test]
    fn skill_weights_sum_to_one() {
        assert!(
            (0.30f64 + 0.20 + 0.15 + 0.10 + 0.10 + 0.15 - 1.0).abs() < 1e-9,
            "skill weights must sum to 1.0"
        );
    }

    #[test]
    fn copyability_penalties_recorded() {
        let inputs = ScoreInputs {
            avg_hold_seconds: Some(5),
            liquidity_available: Some(Decimal::from(5_000)),
            mev_likelihood: Decimal::from_str_exact("0.95").unwrap(),
            ..Default::default()
        };
        let (score, penalties) = copyability_score(&inputs);
        assert!(penalties.hold_penalty > 0);
        assert!(penalties.liquidity_penalty > 0);
        assert!(penalties.drift_penalty > 0);
        assert!(penalties.sell_race_penalty > 0);
        assert!(penalties.bot_penalty > 0);
        assert!(score < 40);
    }
}
