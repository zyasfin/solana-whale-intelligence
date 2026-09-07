#![allow(dead_code)]  // profile/chain fields are consumed by runtime workers

use anyhow::{Context, Result};
use serde::Deserialize;
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

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_runtime_profile")]
    pub runtime_profile: RuntimeProfile,
    /// Workspace this process operates in (REV-046-A4).
    ///
    /// The job context for worker runs: bound once at startup and validated, so no
    /// provider payload or request body can steer which tenant resolved
    /// intelligence lands in. Defaults to the `default` workspace created by
    /// migration 1019.
    #[serde(default = "default_workspace_id")]
    pub workspace_id: i64,
    #[serde(default)]
    pub chains: ChainsConfig,
    #[serde(default)]
    pub helius: HeliusConfig,
    #[serde(default)]
    pub gmgn: GmgnConfig,
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

/// The `default` workspace seeded by migration 1019.
fn default_workspace_id() -> i64 {
    1
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            runtime_profile: default_runtime_profile(),
            workspace_id: default_workspace_id(),
            chains: ChainsConfig::default(),
            helius: HeliusConfig::default(),
            gmgn: GmgnConfig::default(),
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
    /// Credentials used ONLY to apply migrations (DDL + role creation).
    ///
    /// REV-035-#1: `DATABASE_URL` is documented as the least-privilege runtime
    /// role, but every startup path — including `Command::Run` — called
    /// `db::migrate()` on that same pool, so the documented credential made the
    /// service fail to start with `permission denied for schema public`. Migration
    /// is a privileged, occasional operation and now carries its own credential.
    /// When unset, `db migrate` falls back to `DATABASE_URL` so a single-role
    /// development setup keeps working.
    pub migration_database_url: Option<String>,
    pub helius_keys: Vec<String>,
    pub gmgn_api_key: Option<String>,
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
        let mut helius_keys = Vec::new();
        // HELIUS_KEY_1..N, contiguous numbering, user-owned keys only.
        let mut index = 1;
        while let Ok(key) = std::env::var(format!("HELIUS_KEY_{index}")) {
            if !key.trim().is_empty() {
                helius_keys.push(key.trim().to_string());
            }
            index += 1;
        }
        let parse_i64 = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<i64>().ok());
        Self {
            database_url: std::env::var("DATABASE_URL").ok().filter(|v| !v.trim().is_empty()),
            migration_database_url: std::env::var("MIGRATION_DATABASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            helius_keys,
            gmgn_api_key: std::env::var("GMGN_API_KEY").ok().filter(|v| !v.trim().is_empty()),
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
}
