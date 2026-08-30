//! Runtime logic: FIFO cost-basis matching (Phase 1).
//!
//! Canonical concept reference: pre-freeze `scoring.rs::fifo_match` (FIFO trade
//! matching: buys open lots, sells close oldest-first; cost is proportional to
//! the consumed lot amount; hold time is measured from the oldest lot touched;
//! a sell exceeding open amount stops without synthesizing a short). Concept
//! reused, code not copied (principle #13).
//!
//! Precision: `rust_decimal::Decimal` (no float rounding); wire form is
//! `serde-with-str` decimal strings.

use std::collections::HashMap;

use rust_decimal::Decimal;

use super::wallet::{Swap, SwapDirection};

/// A swap enriched with its USD value (price is external to the frozen `Swap`
/// type, so the runtime entry point carries it explicitly).
#[derive(Clone, Debug, PartialEq)]
pub struct ScoredSwap {
    pub swap: Swap,
    pub usd_value: Decimal,
}

/// One realized (closed) position from FIFO matching.
#[derive(Clone, Debug, PartialEq)]
pub struct MatchedPosition {
    pub token: String,
    pub realized_pnl: Decimal,
    pub roi: Option<Decimal>,       // None when cost basis is zero
    pub hold_seconds: Option<i64>,  // None when open time unknown
    pub unmatched_sell_amount: Decimal, // sell amount beyond open lots (fail-closed, never short)
}

/// Aggregate residual (still-open) cost basis.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostBasisResidual {
    /// token -> (open_amount, open_cost_usd)
    pub per_token: HashMap<String, (Decimal, Decimal)>,
}

impl CostBasisResidual {
    pub fn total_cost(&self) -> Decimal {
        self.per_token.values().map(|(_, c)| *c).sum()
    }
}

/// FIFO-match scored swaps into realized positions + residual open cost basis.
///
/// - Buys append lots to a per-token FIFO queue.
/// - Sells consume lots oldest-first; cost = `lot.cost * take / lot.amount`.
/// - Sell exceeding open amount consumes what exists and records the unmatched
///   remainder (never invent negative inventory — fail-closed).
/// - Hold time = oldest lot touched -> sell time.
pub fn fifo_match(swaps: &[ScoredSwap]) -> (Vec<MatchedPosition>, CostBasisResidual) {
    // Group by (chain, wallet, token) so wallets never consume each other's
    // inventory (bug #2 fixed).
    let mut by_key: HashMap<(String, String, String), Vec<&ScoredSwap>> = HashMap::new();
    for s in swaps {
        let key = (
            s.swap.chain.clone(),
            s.swap.wallet.clone(),
            s.swap.token.clone(),
        );
        by_key.entry(key).or_default().push(s);
    }

    let mut positions = Vec::new();
    let mut residual = CostBasisResidual::default();

    for ((_chain, _wallet, token), mut scored) in by_key {
        // Chronological order (unknown timestamp -> earliest). Stable.
        scored.sort_by_key(|s| s.swap.timestamp_secs().unwrap_or(0));

        let mut open: Vec<OpenLot> = Vec::new();

        for s in &scored {
            match s.swap.direction {
                SwapDirection::Buy => {
                    // Buy opens a lot sized by the TOKEN quantity received
                    // (amount_out), not the asset paid (amount_in). Bug #1 fixed.
                    open.push(OpenLot {
                        amount: parse_decimal(&s.swap.amount_out),
                        cost_usd: s.usd_value,
                        time: s.swap.timestamp_secs(),
                    });
                }
                SwapDirection::Sell => {
                    let sell_amount = parse_decimal(&s.swap.amount_in);
                    let mut remaining = sell_amount;
                    let mut cost = Decimal::ZERO;
                    let mut open_time: Option<i64> = None;
                    let mut matched_amount = Decimal::ZERO;

                    while remaining > Decimal::ZERO {
                        let Some(lot) = open.first_mut() else { break };
                        let take = remaining.min(lot.amount);
                        if lot.amount != Decimal::ZERO {
                            let taken_cost = lot.cost_usd * (take / lot.amount);
                            cost += taken_cost;
                            lot.cost_usd -= taken_cost;
                        }
                        if open_time.is_none() {
                            open_time = lot.time;
                        }
                        lot.amount -= take;
                        remaining -= take;
                        matched_amount += take;
                        if lot.amount == Decimal::ZERO {
                            open.remove(0);
                        }
                    }

                    // Proceeds are proportional to the matched amount (bug #3
                    // fixed): oversell only realizes proceeds for the portion
                    // actually backed by inventory.
                    let matched_proceeds = if sell_amount != Decimal::ZERO {
                        s.usd_value * (matched_amount / sell_amount)
                    } else {
                        Decimal::ZERO
                    };
                    let pnl = matched_proceeds - cost;
                    positions.push(MatchedPosition {
                        token: token.clone(),
                        realized_pnl: pnl,
                        roi: if cost != Decimal::ZERO { Some(pnl / cost) } else { None },
                        hold_seconds: match (open_time, s.swap.timestamp_secs()) {
                            (Some(o), Some(c)) => Some((c - o).max(0)),
                            _ => None,
                        },
                        unmatched_sell_amount: remaining,
                    });
                }
            }
        }

        let (amt, cost) = open
            .iter()
            .fold((Decimal::ZERO, Decimal::ZERO), |(a, c), l| (a + l.amount, c + l.cost_usd));
        residual.per_token.insert(token, (amt, cost));
    }

    (positions, residual)
}

#[derive(Clone, Debug)]
struct OpenLot {
    amount: Decimal,
    cost_usd: Decimal,
    time: Option<i64>,
}

/// Parse a decimal-string amount, tolerating empty/absent -> 0.
fn parse_decimal(s: &str) -> Decimal {
    s.parse::<Decimal>().unwrap_or(Decimal::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn scored(
        wallet: &str,
        direction: SwapDirection,
        amount_in: &str,
        amount_out: &str,
        usd: &str,
        ts: &str,
    ) -> ScoredSwap {
        ScoredSwap {
            swap: Swap {
                chain: "solana".into(),
                wallet: wallet.into(),
                token: "TOKEN".into(),
                direction,
                amount_in: amount_in.into(),
                amount_out: amount_out.into(),
                timestamp: ts.into(),
                tx_hash: "tx".into(),
            },
            usd_value: d(usd),
        }
    }

    // Regression #1: buy quantity is the TOKEN amount received (amount_out),
    // not the asset paid (amount_in).
    #[test]
    fn buy_opens_amount_out_quantity() {
        let swaps = vec![
            scored("A", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z"),
            scored("A", SwapDirection::Sell, "10", "120", "120", "2026-01-01T01:00:00Z"),
        ];
        let (_, residual) = fifo_match(&swaps);
        // buy opened 10 token (amount_out); sell closed all 10 -> residual zero
        assert_eq!(residual.per_token["TOKEN"].0, Decimal::ZERO);
    }

    // Regression #2: wallets are isolated — one wallet cannot consume another's
    // inventory.
    #[test]
    fn fifo_isolated_by_wallet() {
        let swaps = vec![
            scored("A", SwapDirection::Buy, "10", "10", "100", "2026-01-01T00:00:00Z"),
            scored("B", SwapDirection::Sell, "10", "10", "200", "2026-01-01T01:00:00Z"),
        ];
        let (positions, _) = fifo_match(&swaps);
        assert_eq!(positions[0].unmatched_sell_amount, d("10"));
    }

    // Regression #3: oversell realizes only matched proceeds.
    #[test]
    fn oversell_realizes_only_matched_proceeds() {
        let swaps = vec![
            scored("A", SwapDirection::Buy, "5", "5", "50", "2026-01-01T00:00:00Z"),
            scored("A", SwapDirection::Sell, "10", "10", "100", "2026-01-01T01:00:00Z"),
        ];
        let (positions, _) = fifo_match(&swaps);
        assert_eq!(positions[0].realized_pnl, Decimal::ZERO);
        assert_eq!(positions[0].unmatched_sell_amount, d("5"));
    }
}
