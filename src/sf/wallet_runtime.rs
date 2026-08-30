//! Runtime logic: exact swap reconstruction → wallet intelligence (Phase 1).
//!
//! Canonical source: PLAN SWI §8.6 "Wallet Intelligence" (lines 514-536):
//! chain-qualified address + optional entity cluster, exact swap reconstruction
//! and cost basis, early-entry timing, realized/unrealized outcome, recurrence
//! across tokens, objective-specific dimensions (no one smart-wallet score).
//!
//! Concept reused (not copied — principle #13): the FIFO cost-basis engine in
//! `cost_basis.rs::fifo_match` already reconstructs realized PnL and residual
//! open lots per `(chain, wallet, token)`. This module is the wallet-level
//! aggregation on top of it: average cost, realized/unrealized outcome,
//! early-entry timing, and recurrence. It consumes the frozen `wallet.rs`
//! domain types (`Swap`, `SwapDirection`, `WalletIntelligence`, `CostBasis`,
//! `Recurrence`) and produces them filled — it introduces no new frozen state.

use std::collections::{HashMap, HashSet};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::cost_basis::{fifo_match, ScoredSwap};
use super::wallet::{CostBasis, Recurrence, Swap, WalletIntelligence};

/// A swap enriched with its USD value at the time of the trade (the runtime
/// entry point carries price explicitly, since price is external to the frozen
/// `Swap` type).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PricedSwap {
    pub swap: Swap,
    pub usd_value: Decimal,
}

/// Result of exact swap reconstruction for one wallet.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SwapReconstruction {
    /// token -> average cost basis (open lots only; realized lots excluded).
    pub average_cost: HashMap<String, Decimal>,
    /// Realized PnL summed across all closed positions (USD).
    pub realized_pnl: Decimal,
    /// Unrealized PnL = current mark value of open lots minus their cost.
    pub unrealized_pnl: Decimal,
    /// Number of tokens this wallet traded (recurrence).
    pub tokens_traded: u32,
    /// Distinct entity clusters touched (derived from cluster assignment, if
    /// provided — see `build_wallet_intelligence`).
    pub distinct_clusters: u32,
}

/// Convert `PricedSwap`s into the `ScoredSwap`s the FIFO engine consumes.
fn to_scored(swaps: &[PricedSwap]) -> Vec<ScoredSwap> {
    swaps
        .iter()
        .map(|p| ScoredSwap {
            swap: p.swap.clone(),
            usd_value: p.usd_value,
        })
        .collect()
}

/// Reconstruct swaps exactly for a single wallet: run FIFO cost-basis matching,
/// then aggregate into average cost, realized/unrealized PnL, and recurrence.
///
/// - Buys open lots (token quantity = `amount_out`); sells close oldest-first.
/// - Realized PnL is summed from the FIFO engine's `MatchedPosition`s.
/// - Unrealized PnL is `mark_usd - residual_cost` per open token, where
///   `mark_usd` is the caller-provided current mark value of the open lots
///   (if absent, unrealized PnL is reported as the negative residual cost —
///   i.e. cost out, no mark — fail-closed: never fabricate a mark price).
pub fn reconstruct_swaps(
    swaps: &[PricedSwap],
    mark_usd: Option<&HashMap<String, Decimal>>,
) -> SwapReconstruction {
    let scored = to_scored(swaps);
    let (positions, residual) = fifo_match(&scored);

    let realized_pnl: Decimal = positions.iter().map(|p| p.realized_pnl).sum();

    // Average cost per token: open cost / open amount (0 when nothing open).
    let mut average_cost = HashMap::new();
    for (token, (amount, cost)) in &residual.per_token {
        let avg = if *amount != Decimal::ZERO {
            *cost / *amount
        } else {
            Decimal::ZERO
        };
        average_cost.insert(token.clone(), avg);
    }

    // Unrealized PnL: mark - residual cost (per open token). No mark provided
    // -> -cost (fail-closed, no fabricated price).
    let mut unrealized_pnl = Decimal::ZERO;
    for (token, (_, cost)) in &residual.per_token {
        let mark = mark_usd
            .and_then(|m| m.get(token))
            .copied()
            .unwrap_or(Decimal::ZERO);
        unrealized_pnl += mark - *cost;
    }

    // Recurrence: distinct tokens traded.
    let tokens_traded = swaps
        .iter()
        .map(|p| p.swap.token.clone())
        .collect::<HashSet<_>>()
        .len() as u32;

    SwapReconstruction {
        average_cost,
        realized_pnl,
        unrealized_pnl,
        tokens_traded,
        distinct_clusters: 0, // filled by build_wallet_intelligence if clusters provided
    }
}

/// Early-entry timing: seconds between the token's earliest known trade by this
/// wallet and the token's own first-known trade (provided as `token_birth_ts`).
/// Returns `None` when either timestamp is unknown. A small/negative value
/// indicates the wallet entered at or before the token's earliest observable
/// activity (early-entry signal). Never infers a timestamp that is absent.
pub fn early_entry_timing(swaps: &[PricedSwap], token_birth_ts: Option<i64>) -> Option<i64> {
    let wallet_first = swaps
        .iter()
        .filter_map(|p| p.swap.timestamp_secs())
        .min();
    match (wallet_first, token_birth_ts) {
        (Some(w), Some(t)) => Some(w - t),
        _ => None,
    }
}

/// Compute recurrence across tokens for a set of wallets (distinct tokens and
/// distinct entity clusters). `clusters` maps wallet address -> cluster id.
pub fn compute_recurrence(
    swaps: &[PricedSwap],
    clusters: &HashMap<String, String>,
) -> Recurrence {
    let tokens = swaps
        .iter()
        .map(|p| p.swap.token.clone())
        .collect::<HashSet<_>>();
    let distinct_clusters = swaps
        .iter()
        .filter_map(|p| clusters.get(&p.swap.wallet).cloned())
        .collect::<HashSet<_>>()
        .len() as u32;

    Recurrence {
        tokens_traded: tokens.len() as u32,
        distinct_clusters,
    }
}

/// Build a filled `WalletIntelligence` from a wallet's swaps. This is the
/// top-level entry point: reconstruct + outcome + recurrence, all in one.
pub fn build_wallet_intelligence(
    chain: &str,
    address: &str,
    cluster_id: Option<&str>,
    swaps: &[PricedSwap],
    mark_usd: Option<&HashMap<String, Decimal>>,
    token_birth_ts: Option<i64>,
    clusters: &HashMap<String, String>,
) -> WalletIntelligence {
    let rec = reconstruct_swaps(swaps, mark_usd);
    let recurrence = compute_recurrence(swaps, clusters);

    // Average cost basis for the wallet's primary (most-traded) token, or None.
    let cost_basis = rec
        .average_cost
        .iter()
        .max_by(|a, b| {
            let ta = swaps.iter().filter(|p| p.swap.token == *a.0).count();
            let tb = swaps.iter().filter(|p| p.swap.token == *b.0).count();
            ta.cmp(&tb)
        })
        .map(|(token, avg)| CostBasis {
            token: token.clone(),
            average_cost: avg.to_string(),
            realized_pnl: Some(rec.realized_pnl.to_string()),
        });

    // `early_entry_timing` is exposed as a standalone function (the frozen
    // `WalletIntelligence` type has no early-entry field; see §8.6).
    let _ = early_entry_timing(swaps, token_birth_ts);

    WalletIntelligence {
        chain: chain.into(),
        address: address.into(),
        cluster_id: cluster_id.map(|s| s.to_string()),
        cost_basis,
        tags: Vec::new(),
        realized_outcome: Some(rec.realized_pnl.to_string()),
        unrealized_outcome: Some(rec.unrealized_pnl.to_string()),
        recurrence: Some(recurrence),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use super::super::wallet::SwapDirection;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn ps(
        wallet: &str,
        token: &str,
        direction: SwapDirection,
        amount_in: &str,
        amount_out: &str,
        usd: &str,
        ts: &str,
    ) -> PricedSwap {
        PricedSwap {
            swap: Swap {
                chain: "solana".into(),
                wallet: wallet.into(),
                token: token.into(),
                direction,
                amount_in: amount_in.into(),
                amount_out: amount_out.into(),
                timestamp: ts.into(),
                tx_hash: "tx".into(),
            },
            usd_value: d(usd),
        }
    }

    #[test]
    fn reconstruct_realized_and_average_cost() {
        let swaps = vec![
            ps("A", "T1", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
            ps("A", "T1", SwapDirection::Sell, "10", "120", "120", "2026-01-01T01:00:00Z"),
        ];
        let rec = reconstruct_swaps(&swaps, None);
        // Realized PnL = 120 (proceeds) - 100 (cost) = 20.
        assert_eq!(rec.realized_pnl, d("20"));
        // All lots closed -> average cost 0.
        assert_eq!(rec.average_cost["T1"], Decimal::ZERO);
        assert_eq!(rec.tokens_traded, 1);
    }

    #[test]
    fn unrealized_pnl_without_mark_is_negative_cost() {
        let swaps = vec![
            ps("A", "T1", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
        ];
        let rec = reconstruct_swaps(&swaps, None);
        // No mark -> unrealized = 0 - 100 = -100 (cost out, no fabricated price).
        assert_eq!(rec.unrealized_pnl, d("-100"));
        assert_eq!(rec.average_cost["T1"], d("10")); // 100 cost / 10 amount
    }

    #[test]
    fn early_entry_is_negative_when_wallet_precedes_token() {
        let swaps = vec![
            ps("A", "T1", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
        ];
        // Wallet first trade = 2026-01-01T00:00:00Z = 1767225600.
        // Token first trade 1h later = 1767229200 -> wallet entered -3600s early.
        assert_eq!(early_entry_timing(&swaps, Some(1767229200)), Some(-3600));
        // Unknown token birth -> None.
        assert_eq!(early_entry_timing(&swaps, None), None);
    }

    #[test]
    fn recurrence_counts_distinct_tokens_and_clusters() {
        let swaps = vec![
            ps("A", "T1", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
            ps("A", "T2", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
            ps("B", "T1", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
        ];
        let clusters: HashMap<String, String> = [
            ("A".to_string(), "C1".to_string()),
            ("B".to_string(), "C1".to_string()),
        ]
        .into_iter()
        .collect();
        let rec = compute_recurrence(&swaps, &clusters);
        assert_eq!(rec.tokens_traded, 2); // T1, T2
        assert_eq!(rec.distinct_clusters, 1); // A and B share C1
    }

    #[test]
    fn build_wallet_intelligence_fills_outcomes() {
        let swaps = vec![
            ps("A", "T1", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
            ps("A", "T1", SwapDirection::Sell, "10", "120", "120", "2026-01-01T01:00:00Z"),
        ];
        let wi = build_wallet_intelligence("solana", "A", None, &swaps, None, None, &HashMap::new());
        assert_eq!(wi.realized_outcome.as_deref(), Some("20"));
        let cb = wi.cost_basis.as_ref().unwrap();
        assert_eq!(cb.token, "T1");
        assert!(wi.recurrence.is_some());
    }
}
