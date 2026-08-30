//! Ingestion orchestration: wallet sync, webhook normalization, raw-first
//! persistence, and provider observation storage.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::chains::transfer_to_funding_event;
use crate::db;
use crate::models::{
    ChainKind, Commitment, NormalizedTransfer, WalletLabelKind,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Persist normalized events with raw-first ordering and idempotency.
///
/// Returns counts of newly stored rows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IngestCounts {
    pub raw_new: u32,
    pub transfers_new: u32,
    pub trades_new: u32,
    pub malformed: u32,
}

/// Ingest one provider transaction: store the raw payload first, then
/// normalize. Malformed payloads count as `malformed` and never crash.
pub async fn ingest_transaction(
    pool: &PgPool,
    chain: ChainKind,
    source: &str,
    raw: &serde_json::Value,
    observed_at: DateTime<Utc>,
    adapter: &dyn crate::chains::ChainAdapter,
) -> Result<IngestCounts> {
    let mut counts = IngestCounts::default();
    let signature = raw
        .get("signature")
        .and_then(|v| v.as_str())
        .or_else(|| raw.get("transactionHash").and_then(|v| v.as_str()))
        .or_else(|| raw.get("hash").and_then(|v| v.as_str()))
        .map(str::to_string);

    // Raw-first: persist the payload before normalization.
    if let Some(signature) = &signature {
        let inserted = db::store_raw_event(pool, chain.as_str(), source, signature, raw, observed_at).await?;
        if inserted {
            counts.raw_new += 1;
        }
    }

    // Normalize; malformed payloads are counted, not fatal.
    match adapter.ingest_transaction(raw.clone(), observed_at).await {
        Ok(events) => {
            for transfer in &events.transfers {
                upsert_wallets_for_transfer(pool, transfer, source).await?;
                if db::store_transfer(pool, transfer).await.is_ok() {
                    counts.transfers_new += 1;
                }
                crate::graph::update_funding_edges(pool, transfer).await?;
            }
            for trade in &events.trades {
                db::upsert_wallet(pool, chain.as_str(), &trade.wallet, observed_at, source).await?;
                if db::store_trade(pool, trade).await.is_ok() {
                    counts.trades_new += 1;
                }
            }
        }
        Err(err) => {
            counts.malformed += 1;
            tracing::warn!(
                chain = chain.as_str(),
                source,
                error = %err,
                "malformed provider payload retained raw"
            );
        }
    }
    Ok(counts)
}

async fn upsert_wallets_for_transfer(
    pool: &PgPool,
    transfer: &NormalizedTransfer,
    source: &str,
) -> Result<()> {
    let at = transfer.block_time.unwrap_or(transfer.observed_at);
    db::upsert_wallet(pool, transfer.chain.as_str(), &transfer.from_address, at, source).await?;
    db::upsert_wallet(pool, transfer.chain.as_str(), &transfer.to_address, at, source).await?;
    Ok(())
}

/// Normalize and ingest a Helius webhook payload.
///
/// Duplicate deliveries deduplicate by signature: the raw insert returns
/// false, normalization still runs but `ON CONFLICT DO NOTHING` keeps
/// single event rows.
pub async fn ingest_webhook(
    pool: &PgPool,
    payload: &[serde_json::Value],
    source: &str,
    adapter: &dyn crate::chains::ChainAdapter,
) -> Result<IngestCounts> {
    let observed_at = Utc::now();
    let mut counts = IngestCounts::default();
    for transaction in payload {
        let single = ingest_transaction(
            pool,
            ChainKind::Solana,
            source,
            transaction,
            observed_at,
            adapter,
        )
        .await?;
        counts.raw_new += single.raw_new;
        counts.transfers_new += single.transfers_new;
        counts.trades_new += single.trades_new;
        counts.malformed += single.malformed;
    }
    Ok(counts)
}

/// Sync one wallet's history through the Helius pool.
pub struct WalletSyncOutcome {
    pub pages_fetched: u32,
    pub transfers_new: u32,
    pub trades_new: u32,
    pub completed: bool,
}

/// Fetch and ingest wallet history page by page (single-address API).
pub async fn sync_wallet(
    pool: &PgPool,
    helius: &crate::helius::HeliusPool,
    chain: ChainKind,
    address: &str,
    adapter: &dyn crate::chains::ChainAdapter,
    max_pages: u32,
) -> Result<WalletSyncOutcome> {
    let mut outcome = WalletSyncOutcome {
        pages_fetched: 0,
        transfers_new: 0,
        trades_new: 0,
        completed: false,
    };
    let mut before: Option<String> = None;
    for _ in 0..max_pages {
        let _ = chain; // chain is recorded per-transaction by the adapters.
        let page = helius
            .get_transactions_for_address(address, before.as_deref())
            .await?;
        let transactions = page
            .as_array()
            .cloned()
            .unwrap_or_default();
        if transactions.is_empty() {
            outcome.completed = true;
            break;
        }
        let counts = ingest_webhook(pool, &transactions, "helius_history", adapter).await?;
        outcome.pages_fetched += 1;
        outcome.transfers_new += counts.transfers_new;
        outcome.trades_new += counts.trades_new;
        // Cursor: the oldest signature of the page.
        before = transactions
            .iter()
            .find_map(|t| t.get("signature").and_then(|s| s.as_str()).map(str::to_string));
        if before.is_none() {
            outcome.completed = true;
            break;
        }
        if counts.raw_new == 0 && counts.transfers_new == 0 && counts.trades_new == 0 {
            // Fully deduplicated page: history exhausted.
            outcome.completed = true;
            break;
        }
    }
    Ok(outcome)
}

/// Store a GMGN token observation (raw-first enrichment).
pub async fn store_gmgn_token_observation(
    pool: &PgPool,
    chain: ChainKind,
    mint: &str,
    endpoint: &str,
    payload: &serde_json::Value,
    observed_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO gmgn_token_observations (chain, mint, observed_at, endpoint, payload)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (chain, mint, observed_at, endpoint) DO NOTHING
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(observed_at)
    .bind(endpoint)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

/// Store a GMGN wallet observation (raw-first enrichment).
pub async fn store_gmgn_wallet_observation(
    pool: &PgPool,
    chain: ChainKind,
    address: &str,
    endpoint: &str,
    period: &str,
    payload: &serde_json::Value,
    observed_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO gmgn_wallet_observations (chain, address, observed_at, endpoint, period, payload)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (chain, address, observed_at, endpoint, period) DO NOTHING
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .bind(observed_at)
    .bind(endpoint)
    .bind(period)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

/// Ingest one transfer into the funding radar (when thresholds qualify).
pub async fn ingest_funding_transfer(
    pool: &PgPool,
    transfer: &NormalizedTransfer,
    native_usd_price: Option<rust_decimal::Decimal>,
    config: &crate::config::FundingRadarConfig,
    percentile_threshold: Option<rust_decimal::Decimal>,
) -> Result<Option<crate::models::FundingRadarCase>> {
    let recipient_age = db::wallet_age_seconds(
        pool,
        transfer.chain.as_str(),
        &transfer.to_address,
        transfer.observed_at,
    )
    .await?;
    let source_kind = None::<WalletLabelKind>;
    let event = transfer_to_funding_event(transfer, native_usd_price, recipient_age, source_kind);
    // Match commitment from the transfer's payload.
    let mut event = event;
    event.commitment = transfer.commitment.max(Commitment::Processed);
    crate::funding_radar::ingest_funding_event(pool, event, config, percentile_threshold).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_counts_default_zero() {
        let counts = IngestCounts::default();
        assert_eq!(counts.raw_new, 0);
        assert_eq!(counts.transfers_new, 0);
        assert_eq!(counts.trades_new, 0);
        assert_eq!(counts.malformed, 0);
    }
}
