#![allow(dead_code)]  // profile/chain fields are consumed by runtime workers

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Runtime capacity profile.
///
/// `low` targets 2 vCPU / 4 GB machines with local PostgreSQL: reduced
/// concurrency, no LaserStream gRPC, filtered WebSocket/RPC monitoring only,
/// short raw retention, and historical backfill paused on queue lag.
/// `scale` targets 8 vCPU / 16 GB app workers with a separate PostgreSQL node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeProfile {
    Low,
    Scale,
}

impl RuntimeProfile {
    /// Telegram global history-fetch concurrency.
    pub fn telegram_concurrency(self) -> usize {
        match self {
            RuntimeProfile::Low => 4,
            RuntimeProfile::Scale => 32,
        }
    }

    /// Provider HTTP worker concurrency.
    pub fn provider_workers(self) -> usize {
        match self {
            RuntimeProfile::Low => 2,
            RuntimeProfile::Scale => 8,
        }
    }

    /// Raw event retention in days.
    pub fn raw_retention_days(self) -> i64 {
        match self {
            RuntimeProfile::Low => 14,
            RuntimeProfile::Scale => 90,
        }
    }

    /// Whether LaserStream gRPC is permitted (Business/Professional plans).
    pub fn laserstream_grpc(self) -> bool {
        matches!(self, RuntimeProfile::Scale)
    }

    /// Independent worker sets for Telegram / funding radar / providers / signals.
    pub fn independent_workers(self) -> bool {
        matches!(self, RuntimeProfile::Scale)
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct SolanaChainConfig {
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct RobinhoodChainConfig {
    pub enabled: bool,
    /// Hex chain id WITHOUT the `0x` prefix. Empty means unconfigured.
    #[serde(default)]
    pub chain_id: String,
    /// Helius multi-chain path slug used to derive the EVM endpoint
    /// (`{helius.base_url}/v1/{helius_slug}/?api-key=...`).
    #[serde(default = "default_robinhood_helius_slug")]
    pub helius_slug: String,
    #[serde(default)]
    pub native_symbol: String,
    #[serde(default = "default_native_decimals")]
    pub native_decimals: u32,
    #[serde(default)]
    pub explorer_url: String,
    #[serde(default)]
    pub start_block: u64,
}

fn default_native_decimals() -> u32 {
    18
}
fn default_robinhood_helius_slug() -> String {
    "robinhood".to_string()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChainsConfig {
    #[serde(default)]
    pub solana: SolanaChainConfig,
    #[serde(default)]
    pub robinhood: RobinhoodChainConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct HeliusConfig {
    /// Shared multi-chain Helius RPC base URL (https scheme, no trailing slash).
    /// Solana JSON-RPC/enhanced/wallet APIs and per-chain EVM endpoints are
    /// derived from this single value; keys are appended as `?api-key=`.
    #[serde(default = "default_helius_base_url")]
    pub base_url: String,
    #[serde(default = "default_helius_rpc_rate")]
    pub rpc_rate_per_second: u32,
    #[serde(default = "default_helius_enhanced_rate")]
    pub enhanced_rate_per_second: u32,
    #[serde(default = "default_helius_enhanced_rate")]
    pub wallet_rate_per_second: u32,
    #[serde(default = "default_helius_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_helius_timeout")]
    pub timeout_seconds: u64,
}

fn default_helius_rpc_rate() -> u32 {
    10
}
pub(crate) fn default_helius_base_url() -> String {
    "https://mainnet.helius-rpc.com".to_string()
}
fn default_helius_enhanced_rate() -> u32 {
    2
}
fn default_helius_max_retries() -> u32 {
    3
}
fn default_helius_timeout() -> u64 {
    30
}

impl Default for HeliusConfig {
    fn default() -> Self {
        Self {
            base_url: default_helius_base_url(),
            rpc_rate_per_second: default_helius_rpc_rate(),
            enhanced_rate_per_second: default_helius_enhanced_rate(),
            wallet_rate_per_second: default_helius_enhanced_rate(),
            max_retries: default_helius_max_retries(),
            timeout_seconds: default_helius_timeout(),
        }
    }
}

impl HeliusConfig {
    /// Solana JSON-RPC / Enhanced / Wallet HTTP endpoint for one API key.
    pub fn http_url(&self, key: &str) -> String {
        format!("{}/?api-key={key}", self.base_url())
    }

    /// Solana WebSocket endpoint for one API key: same host/path as
    /// `http_url`, only the scheme changes (https->wss, http->ws).
    pub fn ws_url(&self, key: &str) -> String {
        let base = self.base_url();
        let base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}/?api-key={key}")
    }

    /// Multi-chain EVM JSON-RPC endpoint for one API key. Helius exposes
    /// each supported EVM chain as a path under the same account host:
    /// `{base}/v1/{slug}/?api-key={key}` (e.g. slug "robinhood").
    pub fn evm_http_url(&self, chain: &str, key: &str) -> String {
        format!("{}/v1/{chain}/?api-key={key}", self.base_url())
    }

    /// Normalized base URL: trimmed, no trailing slash.
    fn base_url(&self) -> &str {
        self.base_url.trim().trim_end_matches('/')
    }
    /// Enhanced Transactions API: parse endpoint for a stored key.
    /// `"{base}/v0/transactions/?api-key={key}"`.
    pub fn parse_tx_url(&self, key: &str) -> String {
        format!("{}/v0/transactions/?api-key={key}", self.base_url())
    }

    /// Enhanced Transactions API: per-address history endpoint for a stored key.
    /// `"{base}/v0/addresses/{address}/transactions/?api-key={key}"`.
    pub fn history_url(&self, key: &str, address: &str) -> String {
        format!(
            "{}/v0/addresses/{address}/transactions/?api-key={key}",
            self.base_url()
        )
    }

    /// Parse a user-supplied Helius key or URL into a raw API key.
    ///
    /// Accepts a bare key (`abc-def...`) or any URL carrying an `api-key`
    /// (or `api_key`) query parameter. Returns None when nothing parses.
    pub fn parse_key_or_url(input: &str) -> Option<String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(pos) = trimmed.find(['?', '&']) {
            // URL form: scan query parameters for api-key / api_key.
            for pair in trimmed[pos + 1..].split('&') {
                let mut kv = pair.splitn(2, '=');
                let name = kv.next().unwrap_or_default().trim();
                let value = kv.next().unwrap_or_default().trim();
                if matches!(name, "api-key" | "api_key") && !value.is_empty() {
                    return Some(value.trim_end_matches('/').to_string());
                }
            }
        }
        // Bare key: single token — no scheme, path, query, or whitespace, and
        // at most one dot (real Helius keys are UUIDs or hex-ish tokens).
        let looks_like_key = !trimmed.contains("://")
            && !trimmed.contains('/')
            && !trimmed.contains(['?', '&', '=', '#'])
            && !trimmed.chars().any(char::is_whitespace)
            && trimmed.matches('.').count() <= 1;
        if looks_like_key {
            return Some(trimmed.to_string());
        }
        None
    }

    /// Normalize a user-supplied key or URL into (api_key, canonical rpc_url).
    /// The rpc_url always uses the configured base URL: `{base}/?api-key={key}`.
    pub fn normalize_key_or_url(&self, input: &str) -> Option<(String, String)> {
        let key = Self::parse_key_or_url(input)?;
        let url = self.http_url(&key);
        Some((key, url))
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct GmgnConfig {
    #[serde(default = "default_gmgn_base_url")]
    pub base_url: String,
    #[serde(default = "default_gmgn_capacity")]
    pub bucket_capacity: u32,
    #[serde(default = "default_gmgn_refill")]
    pub bucket_refill_per_second: u32,
    #[serde(default = "default_gmgn_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub enabled_routes: Vec<String>,
}

fn default_gmgn_base_url() -> String {
    "https://openapi.gmgn.ai".to_string()
}
fn default_gmgn_capacity() -> u32 {
    20
}
fn default_gmgn_refill() -> u32 {
    20
}
fn default_gmgn_timeout() -> u64 {
    30
}

impl Default for GmgnConfig {
    fn default() -> Self {
        Self {
            base_url: default_gmgn_base_url(),
            bucket_capacity: default_gmgn_capacity(),
            bucket_refill_per_second: default_gmgn_refill(),
            timeout_seconds: default_gmgn_timeout(),
            enabled_routes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default = "default_telegram_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_backfill_limit")]
    pub backfill_limit_per_channel: u32,
}

fn default_telegram_concurrency() -> usize {
    4
}
fn default_backfill_limit() -> u32 {
    1000
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowlist: Vec::new(),
            concurrency: default_telegram_concurrency(),
            backfill_limit_per_channel: default_backfill_limit(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct FundingRadarConfig {
    #[serde(default = "default_min_sol")]
    pub large_funding_min_sol: u64,
    #[serde(default = "default_min_usd")]
    pub large_funding_min_usd: u64,
    #[serde(default = "default_percentile_floor_sol")]
    pub percentile_floor_sol: u64,
    #[serde(default = "default_percentile")]
    pub percentile: f64,
    #[serde(default = "default_recipient_age")]
    pub recipient_max_age_days: u32,
    #[serde(default = "default_preparation_window")]
    pub preparation_window_days: u32,
    #[serde(default = "default_fanout")]
    pub fanout_threshold: u32,
    #[serde(default = "default_preparation_evidence")]
    pub preparation_evidence_required: u32,
    #[serde(default = "default_preparation_confidence")]
    pub preparation_alert_confidence: u32,
}

fn default_min_sol() -> u64 {
    100
}
fn default_min_usd() -> u64 {
    10_000
}
fn default_percentile_floor_sol() -> u64 {
    10
}
fn default_percentile() -> f64 {
    0.99
}
fn default_recipient_age() -> u32 {
    7
}
fn default_preparation_window() -> u32 {
    7
}
fn default_fanout() -> u32 {
    3
}
fn default_preparation_evidence() -> u32 {
    2
}
fn default_preparation_confidence() -> u32 {
    70
}

impl Default for FundingRadarConfig {
    fn default() -> Self {
        Self {
            large_funding_min_sol: default_min_sol(),
            large_funding_min_usd: default_min_usd(),
            percentile_floor_sol: default_percentile_floor_sol(),
            percentile: default_percentile(),
            recipient_max_age_days: default_recipient_age(),
            preparation_window_days: default_preparation_window(),
            fanout_threshold: default_fanout(),
            preparation_evidence_required: default_preparation_evidence(),
            preparation_alert_confidence: default_preparation_confidence(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScoringConfig {
    #[serde(default = "default_min_meaningful_trades")]
    pub min_meaningful_trades: u32,
    #[serde(default = "default_min_tokens")]
    pub min_tokens_traded: u32,
    #[serde(default = "default_full_skill")]
    pub full_skill_score: u32,
    #[serde(default = "default_full_copyability")]
    pub full_copyability_score: u32,
    #[serde(default = "default_completeness_floor")]
    pub history_completeness_floor: f64,
    #[serde(default = "default_conviction_cap")]
    pub conviction_cap_below_completeness: u32,
}

fn default_min_meaningful_trades() -> u32 {
    20
}
fn default_min_tokens() -> u32 {
    5
}
fn default_full_skill() -> u32 {
    70
}
fn default_full_copyability() -> u32 {
    60
}
fn default_completeness_floor() -> f64 {
    0.80
}
fn default_conviction_cap() -> u32 {
    49
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            min_meaningful_trades: default_min_meaningful_trades(),
            min_tokens_traded: default_min_tokens(),
            full_skill_score: default_full_skill(),
            full_copyability_score: default_full_copyability(),
            history_completeness_floor: default_completeness_floor(),
            conviction_cap_below_completeness: default_conviction_cap(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct SignalsConfig {
    #[serde(default = "default_entry_max_age")]
    pub entry_max_token_age_hours: u32,
    #[serde(default = "default_entry_min_liquidity")]
    pub entry_min_liquidity_usd: u64,
    #[serde(default = "default_market_max_age")]
    pub market_max_age_seconds: u64,
    #[serde(default = "default_exit_liquidity_drop")]
    pub exit_liquidity_drop_ratio: f64,
}

fn default_entry_max_age() -> u32 {
    24
}
fn default_entry_min_liquidity() -> u64 {
    20_000
}
fn default_market_max_age() -> u64 {
    300
}
fn default_exit_liquidity_drop() -> f64 {
    0.30
}

impl Default for SignalsConfig {
    fn default() -> Self {
        Self {
            entry_max_token_age_hours: default_entry_max_age(),
            entry_min_liquidity_usd: default_entry_min_liquidity(),
            market_max_age_seconds: default_market_max_age(),
            exit_liquidity_drop_ratio: default_exit_liquidity_drop(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct NarrativeConfig {
    #[serde(default = "default_narrative_cap")]
    pub gmgn_only_confidence_cap: u32,
}

fn default_narrative_cap() -> u32 {
    49
}

impl Default for NarrativeConfig {
    fn default() -> Self {
        Self {
            gmgn_only_confidence_cap: default_narrative_cap(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct RetentionConfig {
    pub raw_events_days_low: Option<i64>,
    pub raw_events_days_scale: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct QueuesConfig {
    #[serde(default = "default_queue_precedence")]
    pub precedence: Vec<String>,
    #[serde(default = "default_backfill_pause_lag")]
    pub historical_backfill_pause_lag_seconds: u64,
}

fn default_queue_precedence() -> Vec<String> {
    vec![
        "live_watch".to_string(),
        "funding_radar".to_string(),
        "telegram_ingest".to_string(),
        "seed_sync".to_string(),
        "historical_backfill".to_string(),
    ]
}
fn default_backfill_pause_lag() -> u64 {
    60
}

impl Default for QueuesConfig {
    fn default() -> Self {
        Self {
            precedence: default_queue_precedence(),
            historical_backfill_pause_lag_seconds: default_backfill_pause_lag(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ServerConfig {
    pub webhook_bind: Option<String>,
    pub webhook_auth_token: Option<String>,
}

/// One promotion tier. ORDER = priority: the enrich loop assigns the FIRST
/// (strictest) group whose thresholds a wallet fully meets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SmartWalletGroup {
    pub name: String,
    /// Minimum win rate (0-1) over `stats_period`.
    pub min_win_rate: f64,
    /// Minimum realized profit (USD) over `stats_period`.
    pub min_realized_pnl_usd: f64,
    /// Minimum trade count (buys+sells) over `stats_period`.
    pub min_trades: u32,
    /// wallet_stats period passed to GMGN (e.g. "7d" / "30d").
    #[serde(default = "default_group_stats_period")]
    pub stats_period: String,
    /// Optional: require Twitter followers_count >= N.
    #[serde(default)]
    pub min_followers: Option<i64>,
    /// Optional: require Twitter blue-verified badge.
    #[serde(default)]
    pub require_blue_verified: bool,
    /// Optional: minimum average trade size (USD) = total_cost / trade count,
    /// computed from the stats payload when available.
    #[serde(default)]
    pub min_avg_trade_usd: Option<f64>,
}

fn default_group_stats_period() -> String {
    "30d".to_string()
}

impl SmartWalletGroup {
    /// Construct a group with only the core thresholds; custom rules default
    /// (stats_period "30d", no follower/blue/avg requirements).
    pub fn new(name: &str, min_win_rate: f64, min_realized_pnl_usd: f64, min_trades: u32) -> Self {
        Self {
            name: name.to_string(),
            min_win_rate,
            min_realized_pnl_usd,
            min_trades,
            stats_period: default_group_stats_period(),
            min_followers: None,
            require_blue_verified: false,
            min_avg_trade_usd: None,
        }
    }
}

/// Extract a metric from a wallet_stats payload: win rate, realized pnl
/// (USD), trade count, followers, blue-verified, avg trade usd.
/// Defensive path probing; values may be strings or numbers.
#[derive(Clone, Copy, Debug, Default)]
pub struct WalletStatsMetrics {
    pub win_rate: Option<f64>,
    pub realized_pnl: Option<f64>,
    pub trades: Option<i64>,
    pub followers_count: Option<i64>,
    pub is_blue_verified: Option<bool>,
    pub avg_trade_usd: Option<f64>,
}

/// Parse the metrics out of a raw wallet_stats payload.
pub fn parse_wallet_stats_metrics(stats: &serde_json::Value) -> WalletStatsMetrics {
    let num = |v: &serde_json::Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()));
    let get = |paths: &[&str]| paths.iter().find_map(|p| stats.pointer(p).and_then(num));
    let win_rate = get(&["/pnl_stat/winrate", "/winrate", "/win_rate"]);
    let realized_pnl = get(&["/realized_profit", "/realized_profit_usd", "/realizedPnl"]);
    let buy = get(&["/buy", "/buy_count"]).unwrap_or(0.0);
    let sell = get(&["/sell", "/sell_count"]).unwrap_or(0.0);
    let trades = get(&["/total_trades", "/trade_count"]).map(|t| t as i64).or(Some((buy + sell) as i64));
    let total_cost = get(&["/total_cost", "/bought_cost"]);
    let avg_trade_usd = match (total_cost, trades) {
        (Some(cost), Some(t)) if t > 0 => Some(cost / t as f64),
        _ => None,
    };
    let followers_count = stats
        .pointer("/common/followers_count")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok())))
        .or_else(|| stats.pointer("/common/twitter_fans_num").and_then(|v| v.as_i64()));
    let is_blue_verified = stats.pointer("/common/is_blue_verified").and_then(|v| v.as_bool());
    WalletStatsMetrics {
        win_rate,
        realized_pnl,
        trades,
        followers_count,
        is_blue_verified,
        avg_trade_usd,
    }
}

impl SmartWalletGroup {
    /// Whether a wallet's metrics meet every threshold of this group.
    pub fn matches(&self, win_rate: Option<f64>, realized_pnl: Option<f64>, trades: Option<i64>) -> bool {
        win_rate.map(|w| w >= self.min_win_rate).unwrap_or(false)
            && realized_pnl.map(|p| p >= self.min_realized_pnl_usd).unwrap_or(false)
            && trades.map(|t| t >= self.min_trades as i64).unwrap_or(false)
    }

    /// Full evaluation against a fetched wallet_stats payload, including the
    /// optional custom rules. A required-but-missing stat field means the
    /// criterion is NOT met; `None` options are skipped entirely.
    pub fn matches_stats(&self, stats: &serde_json::Value) -> bool {
        let m = parse_wallet_stats_metrics(stats);
        // Core thresholds (required).
        if !self.matches(m.win_rate, m.realized_pnl, m.trades) {
            return false;
        }
        // min_followers (optional): required field must be present and >= N.
        if let Some(min_f) = self.min_followers {
            if m.followers_count.map(|f| f >= min_f).unwrap_or(false) == false {
                return false;
            }
        }
        // require_blue_verified: must be present and true.
        if self.require_blue_verified && m.is_blue_verified != Some(true) {
            return false;
        }
        // min_avg_trade_usd (optional): required field must be present and >= N.
        if let Some(min_avg) = self.min_avg_trade_usd {
            if m.avg_trade_usd.map(|a| a >= min_avg).unwrap_or(false) == false {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct SmartWalletConfig {
    /// Poll interval for the smartmoney/kol trade feed (seconds).
    #[serde(default = "default_smart_wallet_poll_interval")]
    pub poll_interval_seconds: u64,
    /// Max wallets enriched per 15-min cycle.
    #[serde(default = "default_smart_wallet_enrich_batch")]
    pub enrich_batch: usize,
    /// Consecutive failed enriches before a candidate is dismissed.
    #[serde(default = "default_smart_wallet_dismiss_after_fails")]
    pub dismiss_after_fails: u32,
    /// Promotion tiers, strictest first. When empty, `effective_groups()`
    /// synthesizes a single "default" group from the legacy flat fields (or
    /// the old defaults when those are absent too).
    #[serde(default)]
    pub groups: Vec<SmartWalletGroup>,
    // Legacy flat thresholds (pre-groups config). Optional; used only when
    // `groups` is empty.
    /// Legacy: minimum 30d win rate.
    #[serde(default)]
    pub min_win_rate: Option<f64>,
    /// Legacy: minimum 30d realized profit (USD).
    #[serde(default)]
    pub min_realized_pnl_usd: Option<f64>,
    /// Legacy: minimum 30d trade count.
    #[serde(default)]
    pub min_trades: Option<u32>,
}

fn default_smart_wallet_poll_interval() -> u64 {
    60
}
fn default_smart_wallet_enrich_batch() -> usize {
    10
}
fn default_smart_wallet_dismiss_after_fails() -> u32 {
    3
}

impl Default for SmartWalletConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: default_smart_wallet_poll_interval(),
            enrich_batch: default_smart_wallet_enrich_batch(),
            dismiss_after_fails: default_smart_wallet_dismiss_after_fails(),
            groups: Vec::new(),
            min_win_rate: None,
            min_realized_pnl_usd: None,
            min_trades: None,
        }
    }
}

impl SmartWalletConfig {
    /// The groups to evaluate, in priority order. Falls back to a single
    /// "default" group built from the legacy flat fields when `groups` is
    /// empty (legacy values, else the old defaults 0.5 / 10000 / 10).
    pub fn effective_groups(&self) -> Vec<SmartWalletGroup> {
        if !self.groups.is_empty() {
            return self.groups.clone();
        }
        vec![SmartWalletGroup::new(
            "default",
            self.min_win_rate.unwrap_or(0.5),
            self.min_realized_pnl_usd.unwrap_or(10_000.0),
            self.min_trades.unwrap_or(10),
        )]
    }

    /// Assign the first (strictest) group whose thresholds are all met.
    /// Returns the group name, or "" when no group matches.
    pub fn assign_group(&self, win_rate: Option<f64>, realized_pnl: Option<f64>, trades: Option<i64>) -> String {
        for group in self.effective_groups() {
            if group.matches(win_rate, realized_pnl, trades) {
                return group.name;
            }
        }
        String::new()
    }

    /// Assign a group by evaluating each group against a wallet_stats payload
    /// for that group's `stats_period`. `payloads` maps period -> payload (one
    /// entry per distinct stats_period among the effective groups).
    /// Returns the first (strictest) matching group name, or "" if none.
    pub fn assign_group_stats(
        &self,
        payloads: &std::collections::HashMap<String, serde_json::Value>,
    ) -> String {
        for group in self.effective_groups() {
            if let Some(payload) = payloads.get(&group.stats_period) {
                if group.matches_stats(payload) {
                    return group.name;
                }
            }
            // If the period's payload wasn't fetched, the group can't match.
        }
        String::new()
    }

    /// The distinct stats_periods across the effective groups (deduped,
    /// stable order). The enrich loop fetches one wallet_stats payload per
    /// distinct period (usually just 1).
    pub fn distinct_stats_periods(&self) -> Vec<String> {
        let mut periods: Vec<String> = Vec::new();
        for g in self.effective_groups() {
            if !periods.contains(&g.stats_period) {
                periods.push(g.stats_period);
            }
        }
        periods
    }
}

/// Parse and validate a JSON array of smart-wallet promotion groups (as stored
/// in `admin_settings` under `smart_wallet_groups`). Returns a clear error
/// message on any invalid entry so the admin endpoint can surface it verbatim.
///
/// An empty array is a valid input (meaning "revert to config.toml"), but the
/// caller is expected to treat it as a delete signal rather than persist it.
pub fn parse_smart_wallet_groups(value: &serde_json::Value) -> Result<Vec<SmartWalletGroup>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| "smart_wallet_groups must be a JSON array".to_string())?;
    let mut groups = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let group: SmartWalletGroup = serde_json::from_value(item.clone())
            .map_err(|e| format!("group[{i}]: {e}"))?;
        if group.name.trim().is_empty() {
            return Err(format!("group[{i}]: 'name' must be non-empty"));
        }
        if !(0.0..=1.0).contains(&group.min_win_rate) {
            return Err(format!(
                "group[{i}] ({:?}): 'min_win_rate' must be between 0 and 1",
                group.name
            ));
        }
        if group.stats_period.trim().is_empty() {
            return Err(format!(
                "group[{i}] ({:?}): 'stats_period' must be non-empty",
                group.name
            ));
        }
        groups.push(group);
    }
    Ok(groups)
}

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_runtime_profile")]
    pub runtime_profile: RuntimeProfile,
    #[serde(default)]
    pub chains: ChainsConfig,
    #[serde(default)]
    pub helius: HeliusConfig,
    #[serde(default)]
    pub gmgn: GmgnConfig,
    #[serde(default)]
    pub smart_wallet: SmartWalletConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub funding_radar: FundingRadarConfig,
    #[serde(default)]
    pub scoring: ScoringConfig,
    #[serde(default)]
    pub signals: SignalsConfig,
    #[serde(default)]
    pub narrative: NarrativeConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub queues: QueuesConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

fn default_runtime_profile() -> RuntimeProfile {
    RuntimeProfile::Low
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            runtime_profile: default_runtime_profile(),
            chains: ChainsConfig::default(),
            helius: HeliusConfig::default(),
            gmgn: GmgnConfig::default(),
            smart_wallet: SmartWalletConfig::default(),
            telegram: TelegramConfig::default(),
            funding_radar: FundingRadarConfig::default(),
            scoring: ScoringConfig::default(),
            signals: SignalsConfig::default(),
            narrative: NarrativeConfig::default(),
            retention: RetentionConfig::default(),
            queues: QueuesConfig::default(),
            server: ServerConfig::default(),
        }
    }
}

/// Environment-provided settings (secrets and endpoints that never live in config.toml).
#[derive(Clone, Debug, Default)]
pub struct EnvConfig {
    pub database_url: Option<String>,
    pub tg_api_id: Option<i64>,
    pub tg_api_hash: Option<String>,
    pub tg_session_path: Option<String>,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
}

impl EnvConfig {
    /// Load environment settings from `.env` (if present) and process environment.
    /// Missing secrets are tolerated: the corresponding providers start disabled.
    pub fn load() -> Self {
        let _ = dotenvy::dotenv();
        let parse_i64 = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<i64>().ok());
        Self {
            database_url: std::env::var("DATABASE_URL").ok().filter(|v| !v.trim().is_empty()),
            tg_api_id: parse_i64("TG_API_ID"),
            tg_api_hash: std::env::var("TG_API_HASH").ok().filter(|v| !v.trim().is_empty()),
            tg_session_path: std::env::var("TG_SESSION_PATH").ok().filter(|v| !v.trim().is_empty()),
            telegram_bot_token: std::env::var("TELEGRAM_BOT_TOKEN").ok().filter(|v| !v.trim().is_empty()),
            telegram_chat_id: std::env::var("TELEGRAM_CHAT_ID").ok().filter(|v| !v.trim().is_empty()),
        }
    }
}

/// Full runtime configuration: file config plus environment settings.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config: AppConfig,
    pub env: EnvConfig,
}

impl Settings {
    /// Load `config.toml` from the given path (or `./config.toml`) plus environment.
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = match path {
            Some(p) => p,
            None => PathBuf::from("config.toml"),
        };
        let config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("failed to parse config file {}", path.display()))?
        } else {
            tracing::warn!(path = %path.display(), "config file missing; using defaults");
            AppConfig::default()
        };
        Ok(Self {
            config,
            env: EnvConfig::load(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_profile_concurrency_matches_plan() {
        assert_eq!(RuntimeProfile::Low.telegram_concurrency(), 4);
        assert_eq!(RuntimeProfile::Low.provider_workers(), 2);
        assert!(!RuntimeProfile::Low.laserstream_grpc());
        assert_eq!(RuntimeProfile::Low.raw_retention_days(), 14);
    }

    #[test]
    fn scale_profile_concurrency_matches_plan() {
        assert_eq!(RuntimeProfile::Scale.telegram_concurrency(), 32);
        assert_eq!(RuntimeProfile::Scale.provider_workers(), 8);
        assert!(RuntimeProfile::Scale.laserstream_grpc());
        assert_eq!(RuntimeProfile::Scale.raw_retention_days(), 90);
    }

    #[test]
    fn parses_reference_config_toml() {
        let raw = std::fs::read_to_string("config.toml").expect("config.toml present");
        let config: AppConfig = toml::from_str(&raw).expect("config parses");
        assert_eq!(config.runtime_profile, RuntimeProfile::Low);
        assert_eq!(config.funding_radar.large_funding_min_sol, 100);
        assert_eq!(config.funding_radar.large_funding_min_usd, 10_000);
        assert_eq!(config.queues.precedence.len(), 5);
        assert_eq!(config.queues.precedence[0], "live_watch");
        assert_eq!(config.queues.precedence[4], "historical_backfill");
        assert_eq!(config.narrative.gmgn_only_confidence_cap, 49);
        // Smart wallet groups parse from the reference config (3 tiers).
        let groups = config.smart_wallet.effective_groups();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "elite");
        assert_eq!(groups[1].name, "solid");
        assert_eq!(groups[2].name, "watch");
        assert_eq!(groups[0].min_win_rate, 0.7);
        assert_eq!(config.smart_wallet.poll_interval_seconds, 60);
        assert_eq!(config.smart_wallet.enrich_batch, 10);
        assert_eq!(config.smart_wallet.dismiss_after_fails, 3);
    }
    #[test]
    fn smart_wallet_legacy_flat_fields_become_default_group() {
        // Legacy flat thresholds -> one "default" group preserving old behavior.
        let raw = r#"
            [smart_wallet]
            min_win_rate = 0.6
            min_realized_pnl_usd = 5000.0
            min_trades = 7
        "#;
        let config: AppConfig = toml::from_str(raw).expect("parses");
        let groups = config.smart_wallet.effective_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "default");
        assert_eq!(groups[0].min_win_rate, 0.6);
        assert_eq!(groups[0].min_realized_pnl_usd, 5000.0);
        assert_eq!(groups[0].min_trades, 7);
    }

    #[test]
    fn smart_wallet_empty_config_uses_old_defaults() {
        // No groups, no legacy fields -> one "default" group with old defaults.
        let config = SmartWalletConfig::default();
        let groups = config.effective_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "default");
        assert_eq!(groups[0].min_win_rate, 0.5);
        assert_eq!(groups[0].min_realized_pnl_usd, 10_000.0);
        assert_eq!(groups[0].min_trades, 10);
    }

    #[test]
    fn smart_wallet_group_assignment_picks_strictest_match() {
        let config = SmartWalletConfig {
            groups: vec![
                SmartWalletGroup::new("elite", 0.7, 100_000.0, 50),
                SmartWalletGroup::new("solid", 0.5, 10_000.0, 10),
                SmartWalletGroup::new("watch", 0.4, 1_000.0, 5),
            ],
            ..SmartWalletConfig::default()
        };
        // Meets elite (and lower) -> elite (strictest first).
        assert_eq!(config.assign_group(Some(0.8), Some(150_000.0), Some(60)), "elite");
        // Fails elite trades but meets solid -> solid.
        assert_eq!(config.assign_group(Some(0.6), Some(20_000.0), Some(20)), "solid");
        // Only meets watch -> watch.
        assert_eq!(config.assign_group(Some(0.45), Some(1_500.0), Some(6)), "watch");
        // Meets none -> "" (candidate).
        assert_eq!(config.assign_group(Some(0.1), Some(10.0), Some(1)), "");
        // Missing metrics -> "".
        assert_eq!(config.assign_group(None, None, None), "");
    }
    fn stats_payload(win: f64, pnl: &str, buy: i64, sell: i64, followers: i64, blue: bool, total_cost: &str) -> serde_json::Value {
        serde_json::json!({
            "realized_profit": pnl,
            "buy": buy,
            "sell": sell,
            "total_cost": total_cost,
            "pnl_stat": { "winrate": win },
            "common": { "followers_count": followers, "is_blue_verified": blue }
        })
    }

    #[test]
    fn matches_stats_evaluates_custom_rules() {
        let mut g = SmartWalletGroup::new("elite", 0.5, 1000.0, 10);
        // Base metrics pass.
        let payload = stats_payload(0.6, "5000", 30, 10, 20000, true, "40000");
        assert!(g.matches_stats(&payload), "no custom rules -> base match");
        // min_followers met.
        g.min_followers = Some(10000);
        assert!(g.matches_stats(&payload));
        g.min_followers = Some(99999);
        assert!(!g.matches_stats(&payload), "followers below requirement");
        g.min_followers = None;
        // require_blue_verified.
        g.require_blue_verified = true;
        assert!(g.matches_stats(&payload));
        let not_blue = stats_payload(0.6, "5000", 30, 10, 20000, false, "40000");
        assert!(!g.matches_stats(&not_blue));
        g.require_blue_verified = false;
        // min_avg_trade_usd = total_cost/trades = 40000/40 = 1000.
        g.min_avg_trade_usd = Some(500.0);
        assert!(g.matches_stats(&payload));
        g.min_avg_trade_usd = Some(5000.0);
        assert!(!g.matches_stats(&payload));
    }

    #[test]
    fn matches_stats_missing_required_field_not_met() {
        let mut g = SmartWalletGroup::new("elite", 0.5, 1000.0, 10);
        g.min_followers = Some(100);
        // Payload WITHOUT common/followers_count -> criterion NOT met.
        let payload = serde_json::json!({
            "realized_profit": "5000", "buy": 30, "sell": 10,
            "pnl_stat": { "winrate": 0.6 }
        });
        assert!(!g.matches_stats(&payload));
    }

    #[test]
    fn distinct_stats_periods_dedupes() {
        let config = SmartWalletConfig {
            groups: vec![
                { let mut g = SmartWalletGroup::new("a", 0.7, 100_000.0, 50); g.stats_period = "30d".into(); g },
                { let mut g = SmartWalletGroup::new("b", 0.5, 10_000.0, 10); g.stats_period = "7d".into(); g },
                { let mut g = SmartWalletGroup::new("c", 0.4, 1_000.0, 5); g.stats_period = "30d".into(); g },
            ],
            ..SmartWalletConfig::default()
        };
        assert_eq!(config.distinct_stats_periods(), vec!["30d".to_string(), "7d".to_string()]);
        // Default single group -> one period.
        assert_eq!(SmartWalletConfig::default().distinct_stats_periods(), vec!["30d".to_string()]);
    }

    #[test]
    fn assign_group_stats_uses_per_period_payload() {
        let config = SmartWalletConfig {
            groups: vec![
                { let mut g = SmartWalletGroup::new("elite", 0.7, 100_000.0, 50); g.stats_period = "30d".into(); g },
                { let mut g = SmartWalletGroup::new("fast", 0.5, 100.0, 3); g.stats_period = "7d".into(); g },
            ],
            ..SmartWalletConfig::default()
        };
        let mut payloads = std::collections::HashMap::new();
        // 30d payload fails elite; 7d payload meets fast.
        payloads.insert("30d".to_string(), stats_payload(0.6, "50000", 20, 10, 0, false, "1000"));
        payloads.insert("7d".to_string(), stats_payload(0.6, "500", 5, 2, 0, false, "100"));
        assert_eq!(config.assign_group_stats(&payloads), "fast");
        // If the 7d payload is missing, fast can't match -> "".
        let mut only30 = std::collections::HashMap::new();
        only30.insert("30d".to_string(), stats_payload(0.6, "50000", 20, 10, 0, false, "1000"));
        assert_eq!(config.assign_group_stats(&only30), "");
    }

    #[test]
    fn default_helius_rates_match_free_plan() {
        let helius = HeliusConfig::default();
        assert_eq!(helius.rpc_rate_per_second, 10);
        assert_eq!(helius.enhanced_rate_per_second, 2);
        assert_eq!(helius.wallet_rate_per_second, 2);
    }

    #[test]
    fn helius_url_builders_share_configured_base() {
        let helius = HeliusConfig::default();
        assert_eq!(
            helius.http_url("k1"),
            "https://mainnet.helius-rpc.com/?api-key=k1"
        );
        assert_eq!(
            helius.ws_url("k1"),
            "wss://mainnet.helius-rpc.com/?api-key=k1"
        );
        assert_eq!(
            helius.evm_http_url("robinhood", "k1"),
            "https://mainnet.helius-rpc.com/v1/robinhood/?api-key=k1"
        );
    }

    #[test]
    fn helius_url_builders_trim_trailing_slash() {
        let helius = HeliusConfig {
            base_url: "https://example.helius.io/".to_string(),
            ..HeliusConfig::default()
        };
        assert_eq!(helius.http_url("k"), "https://example.helius.io/?api-key=k");
        assert_eq!(helius.ws_url("k"), "wss://example.helius.io/?api-key=k");
        assert_eq!(
            helius.evm_http_url("robinhood", "k"),
            "https://example.helius.io/v1/robinhood/?api-key=k"
        );
    }
    #[test]
    fn helius_parse_key_or_url_accepts_bare_key() {
        assert_eq!(
            HeliusConfig::parse_key_or_url("  abc-def-123 "),
            Some("abc-def-123".to_string())
        );
        assert_eq!(HeliusConfig::parse_key_or_url(""), None);
        assert_eq!(HeliusConfig::parse_key_or_url("   "), None);
        // Garbage / multi-word / multi-dot inputs are rejected.
        assert_eq!(HeliusConfig::parse_key_or_url("this is not a key"), None);
        assert_eq!(HeliusConfig::parse_key_or_url("not.a.key"), None);
        assert_eq!(HeliusConfig::parse_key_or_url("a b"), None);
    }

    #[test]
    fn helius_parse_key_or_url_extracts_from_urls() {
        assert_eq!(
            HeliusConfig::parse_key_or_url("https://mainnet.helius-rpc.com/?api-key=k9"),
            Some("k9".to_string())
        );
        assert_eq!(
            HeliusConfig::parse_key_or_url(
                "https://mainnet.helius-rpc.com/v0/addresses/Addr/transactions/?api-key=k10&foo=bar"
            ),
            Some("k10".to_string())
        );
        assert_eq!(
            HeliusConfig::parse_key_or_url("wss://mainnet.helius-rpc.com/?api_key=k11"),
            Some("k11".to_string())
        );
        // URL without a key parameter is not a bare key.
        assert_eq!(
            HeliusConfig::parse_key_or_url("https://mainnet.helius-rpc.com/v0/transactions/"),
            None
        );
    }

    #[test]
    fn helius_normalize_derives_canonical_urls() {
        let helius = HeliusConfig::default();
        let (key, rpc_url) = helius
            .normalize_key_or_url("https://mainnet.helius-rpc.com/v1/robinhood/?api-key=zz")
            .unwrap();
        assert_eq!(key, "zz");
        assert_eq!(rpc_url, "https://mainnet.helius-rpc.com/?api-key=zz");
        assert_eq!(
            helius.parse_tx_url(&key),
            "https://mainnet.helius-rpc.com/v0/transactions/?api-key=zz"
        );
        assert_eq!(
            helius.history_url(&key, "Addr123"),
            "https://mainnet.helius-rpc.com/v0/addresses/Addr123/transactions/?api-key=zz"
        );
    }
}
