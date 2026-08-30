//! Chain adapters: Solana (Helius canonical) and Robinhood Chain (EVM JSON-RPC).
//!
//! All adapters normalize into the shared event model keyed by
//! `(chain, signature, event_index, ...)`. Raw payloads are stored before
//! normalization by callers; adapters never mutate provider payloads.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::models::{
    AssetKind, ChainKind, Commitment, FundingEvent, FundingPage, HistoryPage, NormalizedEvents,
    NormalizedTrade, NormalizedTransfer, TradeSide, WalletLabelKind,
};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashSet;

/// Lamports per SOL.
pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;


/// Compute 10^decimals as a Decimal without float math.
fn ten_pow(decimals: u32) -> Decimal {
    let mut result = Decimal::ONE;
    for _ in 0..decimals {
        result *= Decimal::from(10);
    }
    result
}

#[async_trait]
pub trait ChainAdapter: Send + Sync {
    fn chain(&self) -> ChainKind;
    fn validate_address(&self, address: &str) -> Result<()>;
    async fn ingest_transaction(
        &self,
        raw: serde_json::Value,
        observed_at: DateTime<Utc>,
    ) -> Result<NormalizedEvents>;
    async fn fetch_history(&self, address: &str, cursor: Option<&str>) -> Result<HistoryPage>;
    async fn scan_funding(&self, cursor: &str) -> Result<FundingPage>;
}

// ---------------------------------------------------------------------------
// Solana
// ---------------------------------------------------------------------------

/// Solana adapter operating on Helius raw/enhanced transaction payloads.
pub struct SolanaAdapter;

impl SolanaAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Validate a base58 Solana address (32 bytes decoded).
    pub fn validate_solana_address(address: &str) -> Result<()> {
        if address.len() < 32 || address.len() > 44 {
            bail!("solana address length {len} outside 32..=44", len = address.len());
        }
        let decoded = bs58::decode(address)
            .into_vec()
            .map_err(|e| anyhow!("invalid base58 solana address: {e}"))?;
        if decoded.len() != 32 {
            bail!("solana address decodes to {len} bytes, expected 32", len = decoded.len());
        }
        Ok(())
    }

    /// Convert lamports to SOL using Decimal math only.
    pub fn lamports_to_sol(lamports: u64) -> Decimal {
        Decimal::from(lamports) / Decimal::from(LAMPORTS_PER_SOL)
    }

    /// Convert raw SPL token amount with decimals to a Decimal amount.
    pub fn raw_token_to_amount(raw: &str, decimals: u32) -> Result<Decimal> {
        let raw = Decimal::from_str_exact(raw.trim()).map_err(|e| anyhow!("bad token amount: {e}"))?;
        let scale = ten_pow(decimals);
        Ok(raw / scale)
    }

    /// Extract native SOL transfers and SPL transfers from a Helius
    /// enhanced transaction payload.
    fn normalize_helius_transaction(
        raw: &serde_json::Value,
        observed_at: DateTime<Utc>,
    ) -> Result<NormalizedEvents> {
        let mut events = NormalizedEvents::default();
        let signature = raw
            .get("signature")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("transactionHash").and_then(|v| v.as_str()))
            .ok_or_else(|| anyhow!("helius payload missing signature"))?
            .to_string();

        let slot = raw.get("slot").and_then(|v| v.as_u64());
        let block_time = raw
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .and_then(|ts| DateTime::from_timestamp(ts, 0));
        let commitment = raw
            .get("commitment")
            .and_then(|v| v.as_str())
            .and_then(Commitment::parse)
            .unwrap_or(Commitment::Confirmed);

        // Native SOL transfers: helius "nativeTransfers".
        if let Some(natives) = raw.get("nativeTransfers").and_then(|v| v.as_array()) {
            for (index, transfer) in natives.iter().enumerate() {
                let from = transfer.get("fromUserAccount").and_then(|v| v.as_str());
                let to = transfer.get("toUserAccount").and_then(|v| v.as_str());
                let lamports = transfer.get("amount").and_then(|v| v.as_u64());
                let (from, to, lamports) = match (from, to, lamports) {
                    (Some(f), Some(t), Some(a)) if a > 0 => (f, t, a),
                    _ => continue,
                };
                if Self::validate_solana_address(from).is_err()
                    || Self::validate_solana_address(to).is_err()
                {
                    continue;
                }
                events.transfers.push(NormalizedTransfer {
                    chain: ChainKind::Solana,
                    signature: signature.clone(),
                    event_index: index as i32,
                    from_address: from.to_string(),
                    to_address: to.to_string(),
                    asset_kind: AssetKind::Native,
                    mint: String::new(),
                    raw_amount: lamports.to_string(),
                    amount: Self::lamports_to_sol(lamports),
                    slot,
                    block_time,
                    observed_at,
                    source: "helius".to_string(),
                    commitment,
                });
            }
        }

        // SPL token transfers: helius "tokenTransfers".
        if let Some(tokens) = raw.get("tokenTransfers").and_then(|v| v.as_array()) {
            for (offset, transfer) in tokens.iter().enumerate() {
                let from = transfer
                    .get("fromUserAccount")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let to = transfer
                    .get("toUserAccount")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let mint = transfer.get("mint").and_then(|v| v.as_str()).unwrap_or_default();
                let raw_amount = transfer
                    .get("tokenAmount")
                    .and_then(|v| v.as_f64())
                    .or_else(|| transfer.get("rawTokenAmount").and_then(|v| v.as_f64()));
                let decimals = transfer
                    .get("decimals")
                    .or_else(|| transfer.get("rawTokenAmount").and_then(|r| r.get("decimals")))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(6) as u32;
                if from.is_empty() || to.is_empty() || mint.is_empty() {
                    continue;
                }
                if Self::validate_solana_address(from).is_err()
                    || Self::validate_solana_address(to).is_err()
                    || Self::validate_solana_address(mint).is_err()
                {
                    continue;
                }
                let Some(amount_raw) = raw_amount else { continue };
                // Round-trip through string to keep Decimal precision.
                let amount_str = crate::chains::format_amount_string(amount_raw);
                let amount = Self::raw_token_to_amount(&amount_str, decimals)?;
                events.transfers.push(NormalizedTransfer {
                    chain: ChainKind::Solana,
                    signature: signature.clone(),
                    event_index: 1_000_000 + offset as i32,
                    from_address: from.to_string(),
                    to_address: to.to_string(),
                    asset_kind: AssetKind::SplToken,
                    mint: mint.to_string(),
                    raw_amount: amount_str.clone(),
                    amount,
                    slot,
                    block_time,
                    observed_at,
                    source: "helius".to_string(),
                    commitment,
                });
            }
        }

        // Swaps: helius "events.swap" contains token inputs/outputs.
        if let Some(swap) = raw
            .get("events")
            .and_then(|e| e.get("swap"))
            .and_then(|s| s.as_object())
        {
            let fee_payer = raw
                .get("feePayer")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let dex = raw.get("dexId").and_then(|v| v.as_str()).map(str::to_string);
            let native_inputs = swap.get("nativeInput").and_then(|n| n.get("amount")).and_then(|v| v.as_str());
            let native_output = swap
                .get("nativeOutput")
                .and_then(|n| n.get("amount"))
                .and_then(|v| v.as_str());
            let token_inputs = swap.get("tokenInputs").and_then(|v| v.as_array());
            let token_outputs = swap.get("tokenOutputs").and_then(|v| v.as_array());

            // Buy: native in, token out.
            if let (Some(native_in), Some(outputs)) = (native_inputs, token_outputs) {
                if let Some(buy) = Self::build_trade(
                    &signature,
                    fee_payer,
                    TradeSide::Buy,
                    native_in,
                    outputs.first(),
                    dex.as_deref(),
                    slot,
                    block_time,
                    observed_at,
                ) {
                    events.trades.push(buy);
                }
            }
            // Sell: token in, native out.
            if let (Some(native_out), Some(inputs)) = (native_output, token_inputs) {
                if let Some(sell) = Self::build_trade(
                    &signature,
                    fee_payer,
                    TradeSide::Sell,
                    native_out,
                    inputs.first(),
                    dex.as_deref(),
                    slot,
                    block_time,
                    observed_at,
                ) {
                    events.trades.push(sell);
                }
            }
        }

        Ok(events)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_trade(
        signature: &str,
        wallet: &str,
        side: TradeSide,
        native_raw: &str,
        token_leg: Option<&serde_json::Value>,
        dex: Option<&str>,
        slot: Option<u64>,
        block_time: Option<DateTime<Utc>>,
        observed_at: DateTime<Utc>,
    ) -> Option<NormalizedTrade> {
        if wallet.is_empty() || Self::validate_solana_address(wallet).is_err() {
            return None;
        }
        let leg = token_leg?;
        let mint = leg.get("mint").and_then(|v| v.as_str())?;
        let raw_token = leg.get("rawTokenAmount").and_then(|r| r.get("tokenAmount")).and_then(|v| v.as_str());
        let decimals = leg
            .get("rawTokenAmount")
            .and_then(|r| r.get("decimals"))
            .and_then(|v| v.as_u64())
            .unwrap_or(6) as u32;
        let raw_token = raw_token?;
        let token_amount = Self::raw_token_to_amount(raw_token, decimals).ok()?;
        let native_amount = Self::raw_token_to_amount(native_raw, 9).ok()?;
        Some(NormalizedTrade {
            chain: ChainKind::Solana,
            signature: signature.to_string(),
            event_index: 0,
            wallet: wallet.to_string(),
            mint: mint.to_string(),
            side,
            raw_native_amount: native_raw.to_string(),
            raw_token_amount: raw_token.to_string(),
            native_amount,
            token_amount,
            usd_value: None,
            slot,
            block_time,
            observed_at,
            dex_id: dex.map(str::to_string),
            source: "helius".to_string(),
        })
    }
}

impl Default for SolanaAdapter {
    fn default() -> Self {
        Self::new()
    }

}

#[async_trait]
impl ChainAdapter for SolanaAdapter {
    fn chain(&self) -> ChainKind {
        ChainKind::Solana
    }

    fn validate_address(&self, address: &str) -> Result<()> {
        Self::validate_solana_address(address)
    }

    async fn ingest_transaction(
        &self,
        raw: serde_json::Value,
        observed_at: DateTime<Utc>,
    ) -> Result<NormalizedEvents> {
        Self::normalize_helius_transaction(&raw, observed_at)
    }

    /// History fetching is performed by the Helius pool in `helius.rs`;
    /// the adapter exposes normalization only.
    async fn fetch_history(&self, _address: &str, _cursor: Option<&str>) -> Result<HistoryPage> {
        bail!("solana history is served by the helius provider pool")
    }

    async fn scan_funding(&self, _cursor: &str) -> Result<FundingPage> {
        bail!("solana funding scan is served by the helius provider pool")
    }
}

/// Format an f64 amount from provider payloads without precision surprises.
pub fn format_amount_string(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

// ---------------------------------------------------------------------------
// Robinhood Chain (generic EVM JSON-RPC adapter)
// ---------------------------------------------------------------------------

/// Standard ERC-20 `Transfer(address,address,uint256)` event topic hash.
pub const ERC20_TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// JSON-RPC transport abstraction so tests can inject a mock EVM node.
#[async_trait]
pub trait EvmRpc: Send + Sync {
    /// Perform a JSON-RPC call and return the parsed `result`.
    async fn rpc_call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value>;
}
/// Production JSON-RPC transport over HTTP for EVM chains.
pub struct HttpEvmRpc {
    client: reqwest::Client,
    url: String,
}

impl HttpEvmRpc {
    pub fn new(url: String, timeout_seconds: u64) -> Result<Self> {
        if url.trim().is_empty() {
            bail!("evm rpc url is empty");
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_seconds.max(1)))
            .build()
            .map_err(|e| anyhow!("failed to build http client: {e}"))?;
        Ok(Self { client, url })
    }
}

#[async_trait]
impl EvmRpc for HttpEvmRpc {
    async fn rpc_call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": format!("swi-{}", uuid::Uuid::new_v4()),
            "method": method,
            "params": params,
        });
        let response = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("evm rpc network error: {e}"))?;
        if !response.status().is_success() {
            bail!("evm rpc http status {}", response.status());
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| anyhow!("evm rpc decode error: {e}"))?;
        if let Some(error) = value.get("error") {
            bail!("evm rpc error: {error}");
        }
        Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }
}

/// Generic EVM adapter for Robinhood Chain.
///
/// No Robinhood network metadata is hard-coded: the RPC URL and chain ID come
/// from configuration, and `eth_chainId` is validated at startup.
pub struct RobinhoodAdapter<R: EvmRpc> {
    rpc: R,
    chain_id: String,
    native_decimals: u32,
    #[allow(dead_code)]
    start_block: u64,
    validated: bool,
}

impl<R: EvmRpc> RobinhoodAdapter<R> {
    /// Create an adapter without contacting the chain yet.
    pub fn new(rpc: R, chain_id: String, native_decimals: u32, start_block: u64) -> Self {
        Self {
            rpc,
            chain_id,
            native_decimals,
            start_block,
            validated: false,
        }
    }

    /// Validate `eth_chainId` against the configured chain ID.
    ///
    /// A mismatch or unreachable RPC must disable ONLY the Robinhood adapter.
    pub async fn validate_chain_id(&mut self) -> Result<()> {
        let value = self.rpc.rpc_call("eth_chainId", serde_json::json!([])).await?;
        let hex = value
            .as_str()
            .ok_or_else(|| anyhow!("eth_chainId returned non-string result"))?;
        let returned = parse_hex_quantity(hex)
            .ok_or_else(|| anyhow!("eth_chainId returned malformed value {hex}"))?;
        let expected = parse_hex_quantity(&format!("0x{}", self.chain_id))
            .ok_or_else(|| anyhow!("configured robinhood chain_id is malformed"))?;
        if returned != expected {
            bail!(
                "robinhood chain id mismatch: rpc returned {returned:#x}, configured {expected:#x}"
            );
        }
        self.validated = true;
        Ok(())
    }

    pub fn is_validated(&self) -> bool {
        self.validated
    }

    /// Validate an EVM hex address (0x + 40 hex chars).
    pub fn validate_evm_address(address: &str) -> Result<()> {
        if !address.starts_with("0x") || address.len() != 42 {
            bail!("evm address must be 0x + 40 hex characters");
        }
        let body = &address[2..];
        if !body.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("evm address contains non-hex characters");
        }
        // Normalize case-insensitive addresses to lowercase internally.
        if body.chars().any(|c| c.is_ascii_uppercase()) {
            // Checksummed addresses accepted but stored lowercased by callers.
        }
        Ok(())
    }

    /// Convert wei-style raw amount to a Decimal using decimals.
    pub fn raw_to_decimal(raw: &str, decimals: u32) -> Result<Decimal> {
        let trimmed = raw.trim();
        let value = if let Some(hex) = trimmed.strip_prefix("0x") {
            u128::from_str_radix(hex, 16).map_err(|e| anyhow!("bad hex amount: {e}"))?
        } else {
            trimmed
                .parse::<u128>()
                .map_err(|e| anyhow!("bad decimal amount: {e}"))?
        };
        let scale = ten_pow(decimals);
        Ok(Decimal::from(value) / scale)
    }

    /// Normalize one EVM block into transfers and trades.
    ///
    /// Native transfers come from transaction values; ERC-20 transfers come
    /// from receipt logs with the standard Transfer topic.
    pub fn normalize_block(
        block: &serde_json::Value,
        receipts: &[serde_json::Value],
        observed_at: DateTime<Utc>,
    ) -> Result<NormalizedEvents> {
        let mut events = NormalizedEvents::default();
        let block_number = block
            .get("number")
            .and_then(|v| v.as_str())
            .and_then(parse_hex_quantity)
            .ok_or_else(|| anyhow!("block missing number"))?;
        let timestamp = block
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_hex_quantity)
            .and_then(|ts| DateTime::from_timestamp(ts as i64, 0));
        let slot = Some(block_number);

        let mut seen: HashSet<(String, i32)> = HashSet::new();

        // Native transfers from transactions with value > 0.
        if let Some(txs) = block.get("transactions").and_then(|v| v.as_array()) {
            for (_index, tx) in txs.iter().enumerate() {
                let hash = tx.get("hash").and_then(|v| v.as_str()).unwrap_or_default();
                let from = tx.get("from").and_then(|v| v.as_str()).unwrap_or_default();
                let to = tx.get("to").and_then(|v| v.as_str()).unwrap_or_default();
                let value_hex = tx.get("value").and_then(|v| v.as_str()).unwrap_or("0x0");
                let value = parse_hex_quantity(value_hex).unwrap_or(0);
                if hash.is_empty() || from.is_empty() || to.is_empty() || value == 0 {
                    continue;
                }
                if Self::validate_evm_address(from).is_err() || Self::validate_evm_address(to).is_err() {
                    continue;
                }
                if seen.insert((hash.to_string(), 0)) {
                    events.transfers.push(NormalizedTransfer {
                        chain: ChainKind::Robinhood,
                        signature: hash.to_string(),
                        event_index: 0,
                        from_address: from.to_ascii_lowercase(),
                        to_address: to.to_ascii_lowercase(),
                        asset_kind: AssetKind::Native,
                        mint: String::new(),
                        raw_amount: value.to_string(),
                        amount: Self::raw_to_decimal(&value.to_string(), 18)?,
                        slot,
                        block_time: timestamp,
                        observed_at,
                        source: "robinhood_rpc".to_string(),
                        commitment: Commitment::Confirmed,
                    });
                }
            }
        }

        // ERC-20 Transfer logs from receipts.
        for receipt in receipts {
            let hash = receipt
                .get("transactionHash")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if hash.is_empty() {
                continue;
            }
            let logs = match receipt.get("logs").and_then(|v| v.as_array()) {
                Some(logs) => logs,
                None => continue,
            };
            for (log_index, log) in logs.iter().enumerate() {
                let topics = match log.get("topics").and_then(|v| v.as_array()) {
                    Some(t) if t.len() >= 3 => t,
                    _ => continue,
                };
                let topic0 = topics[0].as_str().unwrap_or_default().to_ascii_lowercase();
                if topic0 != ERC20_TRANSFER_TOPIC {
                    continue;
                }
                let Some(from) = decode_topic_address(topics[1].as_str().unwrap_or_default()) else {
                    continue;
                };
                let Some(to) = decode_topic_address(topics[2].as_str().unwrap_or_default()) else {
                    continue;
                };
                let mint = log
                    .get("address")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let data = log.get("data").and_then(|v| v.as_str()).unwrap_or("0x0");
                let raw_value = parse_hex_quantity(data).unwrap_or(0);
                if raw_value == 0 || mint.is_empty() {
                    continue;
                }
                let event_index = 1_000_000 + log_index as i32;
                if seen.insert((hash.to_string(), event_index)) {
                    events.transfers.push(NormalizedTransfer {
                        chain: ChainKind::Robinhood,
                        signature: hash.to_string(),
                        event_index,
                        from_address: from,
                        to_address: to,
                        asset_kind: AssetKind::Erc20,
                        mint,
                        raw_amount: raw_value.to_string(),
                        // Decimals come from token metadata; default 18 until fetched.
                        amount: Self::raw_to_decimal(&raw_value.to_string(), 18)?,
                        slot,
                        block_time: timestamp,
                        observed_at,
                        source: "robinhood_rpc".to_string(),
                        commitment: Commitment::Confirmed,
                    });
                }
            }
        }

        Ok(events)
    }

    /// Fetch and normalize one block by number through the RPC transport.
    pub async fn fetch_block_events(
        &self,
        block_number: u64,
        observed_at: DateTime<Utc>,
    ) -> Result<NormalizedEvents> {
        let hex_block = format!("{block_number:#x}");
        let block = self
            .rpc
            .rpc_call("eth_getBlockByNumber", serde_json::json!([hex_block, true]))
            .await?;
        if !block.is_object() {
            bail!("eth_getBlockByNumber returned non-object");
        }
        let mut receipts = Vec::new();
        if let Some(txs) = block.get("transactions").and_then(|v| v.as_array()) {
            for tx in txs {
                if let Some(hash) = tx.get("hash").and_then(|v| v.as_str()) {
                    let receipt = self
                        .rpc
                        .rpc_call("eth_getTransactionReceipt", serde_json::json!([hash]))
                        .await?;
                    receipts.push(receipt);
                }
            }
        }
        Self::normalize_block(&block, &receipts, observed_at)
    }

    /// Fetch the current block height.
    pub async fn current_block(&self) -> Result<u64> {
        let value = self.rpc.rpc_call("eth_blockNumber", serde_json::json!([])).await?;
        let hex = value.as_str().ok_or_else(|| anyhow!("eth_blockNumber non-string"))?;
        parse_hex_quantity(hex).ok_or_else(|| anyhow!("eth_blockNumber malformed"))
    }

    /// Fetch logs with an optional address/topic filter.
    pub async fn fetch_logs(
        &self,
        from_block: u64,
        to_block: u64,
        address: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let mut filter = serde_json::json!({
            "fromBlock": format!("{from_block:#x}"),
            "toBlock": format!("{to_block:#x}"),
        });
        if let Some(address) = address {
            filter["address"] = serde_json::json!(address);
        }
        let value = self.rpc.rpc_call("eth_getLogs", serde_json::json!([filter])).await?;
        value
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("eth_getLogs returned non-array"))
    }
}

/// Parse a hex quantity string like `0x1a` into u64.
pub fn parse_hex_quantity(value: &str) -> Option<u64> {
    let hex = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X"))?;
    if hex.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(hex, 16).ok()
}

/// Decode an address topic (`0x`-padded 32-byte value) into a `0x` address.
pub fn decode_topic_address(topic: &str) -> Option<String> {
    if topic.len() != 66 {
        return None;
    }
    let address = &topic[26..];
    if !address.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{}", address.to_ascii_lowercase()))
}

/// Convert a normalized transfer into a funding radar event.
pub fn transfer_to_funding_event(
    transfer: &NormalizedTransfer,
    native_usd_price: Option<Decimal>,
    recipient_age_seconds: Option<i64>,
    source_kind: Option<WalletLabelKind>,
) -> FundingEvent {
    let native_amount = match transfer.asset_kind {
        AssetKind::Native => transfer.amount,
        _ => Decimal::ZERO,
    };
    let amount_usd = match (transfer.asset_kind, native_usd_price) {
        (AssetKind::Native, Some(price)) => Some(transfer.amount * price),
        // Token funding requires a known event-time USD price; None until enriched.
        _ => None,
    };
    FundingEvent {
        chain: transfer.chain,
        signature: transfer.signature.clone(),
        slot: transfer.slot,
        observed_at: transfer.observed_at,
        commitment: transfer.commitment,
        from_address: transfer.from_address.clone(),
        to_address: transfer.to_address.clone(),
        asset_kind: transfer.asset_kind,
        mint: transfer.mint.clone(),
        raw_amount: transfer.raw_amount.clone(),
        native_amount,
        amount_usd,
        native_usd_price,
        recipient_age_seconds,
        source_kind,
        raw: serde_json::to_value(transfer).unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_solana_address() -> String {
        // Deterministic 32-byte address rendered in base58.
        let bytes = [7u8; 32];
        bs58::encode(bytes).into_string()
    }

    #[test]
    fn solana_address_validation() {
        assert!(SolanaAdapter::validate_solana_address(&valid_solana_address()).is_ok());
        assert!(SolanaAdapter::validate_solana_address("short").is_err());
        assert!(SolanaAdapter::validate_solana_address(&"A".repeat(45)).is_err());
        // 44 chars but non-base58 characters.
        assert!(SolanaAdapter::validate_solana_address("0OIl!@#$%^&*()_+-=[]{};':\",./<>?zkl").is_err());
    }

    #[test]
    fn lamports_conversion_uses_decimal() {
        let sol = SolanaAdapter::lamports_to_sol(1_500_000_000_000);
        assert_eq!(sol, Decimal::from(1500));
        assert_eq!(SolanaAdapter::lamports_to_sol(1), Decimal::from_str_exact("0.000000001").unwrap());
    }

    #[test]
    fn helius_native_transfer_normalized() {
        let addr = valid_solana_address();
        let payload = json!({
            "signature": "sig_native_1",
            "slot": 1000,
            "timestamp": 1700000000,
            "commitment": "confirmed",
            "feePayer": addr,
            "nativeTransfers": [
                { "fromUserAccount": addr, "toUserAccount": addr, "amount": 150_000_000_000u64 }
            ]
        });
        let events = SolanaAdapter::normalize_helius_transaction(&payload, Utc::now()).unwrap();
        assert_eq!(events.transfers.len(), 1);
        let transfer = &events.transfers[0];
        assert_eq!(transfer.amount, Decimal::from(150));
        assert_eq!(transfer.asset_kind, AssetKind::Native);
        assert_eq!(transfer.commitment, Commitment::Confirmed);
        assert_eq!(transfer.signature, "sig_native_1");
    }

    #[test]
    fn helius_malformed_payload_reports_error() {
        let payload = json!({ "noSignature": true });
        assert!(SolanaAdapter::normalize_helius_transaction(&payload, Utc::now()).is_err());
    }

    #[test]
    fn helius_invalid_addresses_skipped() {
        let payload = json!({
            "signature": "sig_bad",
            "nativeTransfers": [
                { "fromUserAccount": "notbase58!!", "toUserAccount": "also_bad", "amount": 100 }
            ]
        });
        let events = SolanaAdapter::normalize_helius_transaction(&payload, Utc::now()).unwrap();
        assert!(events.transfers.is_empty());
    }

    #[test]
    fn evm_address_validation() {
        assert!(RobinhoodAdapter::<MockRpc>::validate_evm_address(
            "0x1234567890abcdef1234567890abcdef12345678"
        )
        .is_ok());
        assert!(RobinhoodAdapter::<MockRpc>::validate_evm_address("0x123").is_err());
        assert!(RobinhoodAdapter::<MockRpc>::validate_evm_address(
            "1234567890abcdef1234567890abcdef12345678"
        )
        .is_err());
        assert!(
            RobinhoodAdapter::<MockRpc>::validate_evm_address("0x1234567890abcdef1234567890abcdef1234567g")
                .is_err()
        );
    }

    #[test]
    fn hex_quantity_parsing() {
        assert_eq!(parse_hex_quantity("0x1a"), Some(26));
        assert_eq!(parse_hex_quantity("0x0"), Some(0));
        assert_eq!(parse_hex_quantity("0x"), Some(0));
        assert_eq!(parse_hex_quantity("1a"), None);
        assert_eq!(parse_hex_quantity("0xzz"), None);
    }

    #[test]
    fn topic_address_decoding() {
        let topic = "0x0000000000000000000000001234567890abcdef1234567890abcdef12345678";
        assert_eq!(
            decode_topic_address(topic),
            Some("0x1234567890abcdef1234567890abcdef12345678".to_string())
        );
        assert_eq!(decode_topic_address("0x1234"), None);
    }

    struct MockRpc;

    #[async_trait]
    impl EvmRpc for MockRpc {
        async fn rpc_call(&self, _method: &str, _params: serde_json::Value) -> Result<serde_json::Value> {
            Ok(json!("0xdeadbeef"))
        }
    }

    #[tokio::test]
    async fn chain_id_mismatch_disables_adapter() {
        let mut adapter = RobinhoodAdapter::new(MockRpc, "1a2b3c".to_string(), 18, 0);
        let err = adapter.validate_chain_id().await.unwrap_err();
        assert!(err.to_string().contains("chain id mismatch"));
        assert!(!adapter.is_validated());
    }

    struct ChainIdRpc(String);

    #[async_trait]
    impl EvmRpc for ChainIdRpc {
        async fn rpc_call(&self, method: &str, _params: serde_json::Value) -> Result<serde_json::Value> {
            if method == "eth_chainId" {
                Ok(json!(format!("0x{}", self.0)))
            } else {
                Ok(json!(null))
            }
        }
    }

    #[tokio::test]
    async fn chain_id_match_enables_adapter() {
        let mut adapter = RobinhoodAdapter::new(ChainIdRpc("1a2b3c".to_string()), "1a2b3c".to_string(), 18, 0);
        adapter.validate_chain_id().await.unwrap();
        assert!(adapter.is_validated());
    }

    #[test]
    fn evm_block_normalization_native_and_erc20() {
        let block = json!({
            "number": "0x64",
            "timestamp": "0x6567b400",
            "transactions": [
                {
                    "hash": "0xabc0000000000000000000000000000000000000000000000000000000000001",
                    "from": "0x1111111111111111111111111111111111111111",
                    "to": "0x2222222222222222222222222222222222222222",
                    "value": "0xde0b6b3a7640000"
                }
            ]
        });
        let receipt = json!({
            "transactionHash": "0xabc0000000000000000000000000000000000000000000000000000000000001",
            "logs": [
                {
                    "address": "0xTokenContract00000000000000000000000000000001",
                    "topics": [
                        ERC20_TRANSFER_TOPIC,
                        "0x0000000000000000000000001111111111111111111111111111111111111111",
                        "0x0000000000000000000000003333333333333333333333333333333333333333"
                    ],
                    "data": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000"
                }
            ]
        });
        let events = RobinhoodAdapter::<MockRpc>::normalize_block(&block, &[receipt], Utc::now()).unwrap();
        assert_eq!(events.transfers.len(), 2);
        let native = events
            .transfers
            .iter()
            .find(|t| t.asset_kind == AssetKind::Native)
            .expect("native transfer");
        assert_eq!(native.amount, Decimal::ONE);
        assert_eq!(native.to_address, "0x2222222222222222222222222222222222222222");
        let erc20 = events
            .transfers
            .iter()
            .find(|t| t.asset_kind == AssetKind::Erc20)
            .expect("erc20 transfer");
        assert_eq!(erc20.mint, "0xtokencontract00000000000000000000000000000001");
        assert_eq!(erc20.to_address, "0x3333333333333333333333333333333333333333");
        assert_eq!(erc20.event_index, 1_000_000);
    }

    #[test]
    fn evm_block_idempotent_event_keys() {
        let block = json!({
            "number": "0x64",
            "transactions": [
                {
                    "hash": "0xabc0000000000000000000000000000000000000000000000000000000000001",
                    "from": "0x1111111111111111111111111111111111111111",
                    "to": "0x2222222222222222222222222222222222222222",
                    "value": "0x1"
                }
            ]
        });
        let first = RobinhoodAdapter::<MockRpc>::normalize_block(&block, &[], Utc::now()).unwrap();
        let second = RobinhoodAdapter::<MockRpc>::normalize_block(&block, &[], Utc::now()).unwrap();
        // Same normalization twice yields identical event keys (idempotency).
        assert_eq!(first.transfers.len(), 1);
        assert_eq!(second.transfers.len(), 1);
        assert_eq!(first.transfers[0].signature, second.transfers[0].signature);
        assert_eq!(first.transfers[0].event_index, second.transfers[0].event_index);
    }

    #[test]
    fn zero_value_native_transfers_skipped() {
        let block = json!({
            "number": "0x64",
            "transactions": [
                {
                    "hash": "0xabc0000000000000000000000000000000000000000000000000000000000002",
                    "from": "0x1111111111111111111111111111111111111111",
                    "to": "0x2222222222222222222222222222222222222222",
                    "value": "0x0"
                }
            ]
        });
        let events = RobinhoodAdapter::<MockRpc>::normalize_block(&block, &[], Utc::now()).unwrap();
        assert!(events.transfers.is_empty());
    }

    #[test]
    fn funding_event_from_native_transfer_computes_usd() {
        let addr = valid_solana_address();
        let price = Decimal::from_str_exact("200").unwrap();
        let transfer = NormalizedTransfer {
            chain: ChainKind::Solana,
            signature: "sig".into(),
            event_index: 0,
            from_address: addr.clone(),
            to_address: addr.clone(),
            asset_kind: AssetKind::Native,
            mint: String::new(),
            raw_amount: "150000000000".into(),
            amount: Decimal::from(150),
            slot: Some(1),
            block_time: None,
            observed_at: Utc::now(),
            source: "helius".into(),
            commitment: Commitment::Confirmed,
        };
        let event = transfer_to_funding_event(&transfer, Some(price), Some(86_400), None);
        assert_eq!(event.amount_usd, Some(Decimal::from(30_000)));
        assert_eq!(event.native_amount, Decimal::from(150));
    }

    #[test]
    fn funding_event_without_price_has_no_usd() {
        let addr = valid_solana_address();
        let transfer = NormalizedTransfer {
            chain: ChainKind::Solana,
            signature: "sig".into(),
            event_index: 0,
            from_address: addr.clone(),
            to_address: addr.clone(),
            asset_kind: AssetKind::SplToken,
            mint: addr,
            raw_amount: "1000".into(),
            amount: Decimal::ONE,
            slot: None,
            block_time: None,
            observed_at: Utc::now(),
            source: "helius".into(),
            commitment: Commitment::Confirmed,
        };
        let event = transfer_to_funding_event(&transfer, None, None, None);
        assert_eq!(event.amount_usd, None);
    }


    struct HttpEvmRpcInvalidUrl;

    #[tokio::test]
    async fn http_rpc_rejects_empty_url() {
        assert!(HttpEvmRpc::new(String::new(), 30).is_err());
    }

    #[tokio::test]
    async fn http_rpc_constructs_with_valid_url() {
        let rpc = HttpEvmRpc::new("http://127.0.0.1:1".to_string(), 5).unwrap();
        let err = rpc
            .rpc_call("eth_chainId", serde_json::json!([]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("network error") || err.to_string().contains("http"));
    }

    /// Mock blockchain serving eth_chainId, eth_blockNumber, one block, and receipts.
    struct MockBlockchainRpc {
        chain_id: String,
        block: serde_json::Value,
        receipt: serde_json::Value,
    }

    #[async_trait]
    impl EvmRpc for MockBlockchainRpc {
        async fn rpc_call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
            match method {
                "eth_chainId" => Ok(json!(format!("0x{}", self.chain_id))),
                "eth_blockNumber" => Ok(json!("0x65")),
                "eth_getBlockByNumber" => {
                    let want = params[0].as_str().unwrap_or_default();
                    if want == "0x64" {
                        Ok(self.block.clone())
                    } else {
                        Ok(json!(null))
                    }
                }
                "eth_getTransactionReceipt" => Ok(self.receipt.clone()),
                _ => Ok(json!(null)),
            }
        }
    }

    fn mock_blockchain() -> MockBlockchainRpc {
        MockBlockchainRpc {
            chain_id: "1a2b3c".to_string(),
            block: json!({
                "number": "0x64",
                "timestamp": "0x6567b400",
                "transactions": [
                    {
                        "hash": "0xabc0000000000000000000000000000000000000000000000000000000000009",
                        "from": "0x1111111111111111111111111111111111111111",
                        "to": "0x2222222222222222222222222222222222222222",
                        "value": "0x8ac7230489e80000"
                    }
                ]
            }),
            receipt: json!({
                "transactionHash": "0xabc0000000000000000000000000000000000000000000000000000000000009",
                "logs": [
                    {
                        "address": "0xTokenContract00000000000000000000000000000009",
                        "topics": [
                            ERC20_TRANSFER_TOPIC,
                            "0x0000000000000000000000001111111111111111111111111111111111111111",
                            "0x0000000000000000000000004444444444444444444444444444444444444444"
                        ],
                        "data": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000"
                    }
                ]
            }),
        }
    }

    #[tokio::test]
    async fn robinhood_mock_rpc_end_to_end() {
        let mut adapter = RobinhoodAdapter::new(mock_blockchain(), "1a2b3c".to_string(), 18, 100);
        // Startup validation against the mock RPC.
        adapter.validate_chain_id().await.unwrap();
        assert!(adapter.is_validated());

        // Current height and one block of events.
        assert_eq!(adapter.current_block().await.unwrap(), 0x65);
        let observed_at = Utc::now();
        let events = adapter.fetch_block_events(0x64, observed_at).await.unwrap();
        assert_eq!(events.transfers.len(), 2);

        let native = events
            .transfers
            .iter()
            .find(|t| t.asset_kind == AssetKind::Native)
            .expect("native transfer present");
        assert_eq!(native.amount, Decimal::from(10));
        assert_eq!(native.chain, ChainKind::Robinhood);
        assert_eq!(native.source, "robinhood_rpc");

        let erc20 = events
            .transfers
            .iter()
            .find(|t| t.asset_kind == AssetKind::Erc20)
            .expect("erc20 transfer present");
        assert_eq!(erc20.mint, "0xtokencontract00000000000000000000000000000009");
        assert_eq!(erc20.to_address, "0x4444444444444444444444444444444444444444");

        // Idempotent re-fetch produces identical event keys.
        let again = adapter.fetch_block_events(0x64, observed_at).await.unwrap();
        assert_eq!(again.transfers.len(), 2);
        for (first, second) in events.transfers.iter().zip(again.transfers.iter()) {
            assert_eq!(first.signature, second.signature);
            assert_eq!(first.event_index, second.event_index);
        }

        // Wrong chain id disables the adapter without inventing metadata.
        let mut wrong = RobinhoodAdapter::new(mock_blockchain(), "deadbeef".to_string(), 18, 100);
        assert!(wrong.validate_chain_id().await.is_err());
        assert!(!wrong.is_validated());
    }
}
