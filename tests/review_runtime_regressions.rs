use rust_decimal::Decimal;
use std::str::FromStr;
use solana_whale_intelligence::sf::cost_basis::{fifo_match, ScoredSwap};
use solana_whale_intelligence::sf::ingest::Chain;
use solana_whale_intelligence::sf::ingest_runtime::{run_pipeline, InMemoryIdempotency, RawPayload};
use solana_whale_intelligence::sf::wallet::{Swap, SwapDirection};

fn d(v: &str) -> Decimal { Decimal::from_str(v).unwrap() }
fn scored(wallet: &str, direction: SwapDirection, amount_in: &str, amount_out: &str, usd: &str, ts: &str, tx: &str) -> ScoredSwap {
    ScoredSwap { swap: Swap { chain: "solana".into(), wallet: wallet.into(), token: "TOKEN".into(), direction, amount_in: amount_in.into(), amount_out: amount_out.into(), timestamp: ts.into(), tx_hash: tx.into() }, usd_value: d(usd) }
}

#[test]
fn buy_quantity_is_token_amount_out() {
    let swaps = vec![
        scored("A", SwapDirection::Buy, "100", "10", "100", "2026-01-01T00:00:00Z", "b"),
        scored("A", SwapDirection::Sell, "10", "120", "120", "2026-01-01T01:00:00Z", "s"),
    ];
    let (_, residual) = fifo_match(&swaps);
    assert_eq!(residual.per_token["TOKEN"].0, Decimal::ZERO, "buy must open amount_out token quantity");
}

#[test]
fn fifo_isolated_by_chain_wallet_token() {
    let swaps = vec![
        scored("A", SwapDirection::Buy, "10", "10", "100", "2026-01-01T00:00:00Z", "a-buy"),
        scored("B", SwapDirection::Sell, "10", "10", "200", "2026-01-01T01:00:00Z", "b-sell"),
    ];
    let (positions, _) = fifo_match(&swaps);
    assert_eq!(positions[0].unmatched_sell_amount, d("10"), "wallet B cannot consume wallet A inventory");
}

#[test]
fn oversell_recognizes_only_matched_proceeds() {
    let swaps = vec![
        scored("A", SwapDirection::Buy, "5", "5", "50", "2026-01-01T00:00:00Z", "buy"),
        scored("A", SwapDirection::Sell, "10", "10", "100", "2026-01-01T01:00:00Z", "sell"),
    ];
    let (positions, _) = fifo_match(&swaps);
    assert_eq!(positions[0].realized_pnl, Decimal::ZERO, "only 5/10 of proceeds is matched: 50 proceeds - 50 cost");
    assert_eq!(positions[0].unmatched_sell_amount, d("5"));
}

#[test]
fn fallback_idempotency_preserves_entity_and_time_context() {
    let mut idem = InMemoryIdempotency::default();
    let one = RawPayload { chain: Chain::Solana, source_name: "source".into(), source_event_id: None, event_type: "transfer".into(), payload_schema_version: "1".into(), raw_hash: "same".into(), observed_at: 0, payload: serde_json::json!({"token":"A"}) };
    let two = RawPayload { chain: Chain::Solana, source_name: "source".into(), source_event_id: None, event_type: "transfer".into(), payload_schema_version: "1".into(), raw_hash: "same".into(), observed_at: 3600, payload: serde_json::json!({"token":"B"}) };
    assert!(run_pipeline(&one, &mut idem).accepted);
    let outcome = run_pipeline(&two, &mut idem);
    assert!(!outcome.deduped, "fallback key requires entity + event type + time bucket + raw hash");
}
