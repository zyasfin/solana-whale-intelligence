//! Core domain types shared across modules.
//!
//! Financial values use [`Decimal`] everywhere. Raw integer amounts are kept
//! alongside normalized decimal amounts; `f64` is never persisted for
//! financial values.

#![allow(dead_code)]  // shared domain model API

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Canonical chain identifiers. All persisted rows are keyed by `(chain, ...)`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ChainKind {
    Solana,
    Robinhood,
}

impl ChainKind {
    /// Stable lowercase identifier persisted in database `chain` columns.
    pub fn as_str(self) -> &'static str {
        match self {
            ChainKind::Solana => "solana",
            ChainKind::Robinhood => "robinhood",
        }
    }

    /// Parse from persisted identifier or CLI `--chain` values (`sol`, `solana`, `robinhood`).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "solana" | "sol" => Some(ChainKind::Solana),
            "robinhood" | "rh" => Some(ChainKind::Robinhood),
            _ => None,
        }
    }
}

impl std::fmt::Display for ChainKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Asset kind for transfers and funding events.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Native,
    SplToken,
    Erc20,
}

impl AssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AssetKind::Native => "native",
            AssetKind::SplToken => "spl_token",
            AssetKind::Erc20 => "erc20",
        }
    }
}

/// Commitment level for chain events.
///
/// `processed` is allowed only for early `funding_watch` candidates;
/// `preparation`/`deployed` promotion requires `confirmed`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Commitment {
    Processed,
    Confirmed,
    Finalized,
}

impl Commitment {
    pub fn as_str(self) -> &'static str {
        match self {
            Commitment::Processed => "processed",
            Commitment::Confirmed => "confirmed",
            Commitment::Finalized => "finalized",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "processed" => Some(Commitment::Processed),
            "confirmed" => Some(Commitment::Confirmed),
            "finalized" => Some(Commitment::Finalized),
            _ => None,
        }
    }
}

/// Wallet label taxonomy from GMGN plus local classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalletLabelKind {
    SmartDegen,
    Sniper,
    Bundler,
    RatTrader,
    DexBot,
    MevBot,
    FreshWallet,
    Dev,
    Insider,
    Arbitrage,
    CopyTrader,
    MarketMaker,
    Exchange,
    Bridge,
    KuiperExchange,
    Project,
}

impl WalletLabelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WalletLabelKind::SmartDegen => "smart_degen",
            WalletLabelKind::Sniper => "sniper",
            WalletLabelKind::Bundler => "bundler",
            WalletLabelKind::RatTrader => "rat_trader",
            WalletLabelKind::DexBot => "dex_bot",
            WalletLabelKind::MevBot => "mev_bot",
            WalletLabelKind::FreshWallet => "fresh_wallet",
            WalletLabelKind::Dev => "dev",
            WalletLabelKind::Insider => "insider",
            WalletLabelKind::Arbitrage => "arbitrage",
            WalletLabelKind::CopyTrader => "copy_trader",
            WalletLabelKind::MarketMaker => "market_maker",
            WalletLabelKind::Exchange => "exchange",
            WalletLabelKind::Bridge => "bridge",
            WalletLabelKind::KuiperExchange => "kuiper_exchange",
            WalletLabelKind::Project => "project",
        }
    }

    /// Parse a label string from GMGN payloads or local classification.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "smart_degen" | "smartdegen" => Some(WalletLabelKind::SmartDegen),
            "sniper" => Some(WalletLabelKind::Sniper),
            "bundler" => Some(WalletLabelKind::Bundler),
            "rat_trader" | "rattrader" => Some(WalletLabelKind::RatTrader),
            "dex_bot" | "dexbot" => Some(WalletLabelKind::DexBot),
            "mev_bot" | "mevbot" | "mev" => Some(WalletLabelKind::MevBot),
            "fresh_wallet" | "freshwallet" => Some(WalletLabelKind::FreshWallet),
            "dev" | "developer" | "creator" => Some(WalletLabelKind::Dev),
            "insider" => Some(WalletLabelKind::Insider),
            "arbitrage" => Some(WalletLabelKind::Arbitrage),
            "copy_trader" | "copytrader" => Some(WalletLabelKind::CopyTrader),
            "market_maker" | "marketmaker" => Some(WalletLabelKind::MarketMaker),
            "exchange" | "cex" => Some(WalletLabelKind::Exchange),
            "bridge" => Some(WalletLabelKind::Bridge),
            "kuiper_exchange" | "kuiper" => Some(WalletLabelKind::KuiperExchange),
            "project" => Some(WalletLabelKind::Project),
            _ => None,
        }
    }
}

/// Disposition describing how a wallet participates in analysis.
///
/// - `score`: full deep-sync, scoring, and signal contribution.
/// - `watch`: light monitoring; no scoring contribution yet.
/// - `flow_only`: transfers retained as graph edges; excluded from alpha.
/// - `skip`: excluded from deep-sync/scoring; direct funding edges retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Score,
    Watch,
    FlowOnly,
    Skip,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Score => "score",
            Disposition::Watch => "watch",
            Disposition::FlowOnly => "flow_only",
            Disposition::Skip => "skip",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "score" => Some(Disposition::Score),
            "watch" => Some(Disposition::Watch),
            "flow_only" | "flow-only" => Some(Disposition::FlowOnly),
            "skip" | "block" => Some(Disposition::Skip),
            _ => None,
        }
    }
}

/// Token lifecycle states.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    NewCreation,
    BondingCurve,
    NearGraduation,
    Graduated,
    PostMigration,
    Unsupported,
}

impl LifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleState::NewCreation => "new_creation",
            LifecycleState::BondingCurve => "bonding_curve",
            LifecycleState::NearGraduation => "near_graduation",
            LifecycleState::Graduated => "graduated",
            LifecycleState::PostMigration => "post_migration",
            LifecycleState::Unsupported => "unsupported",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "new_creation" => Some(LifecycleState::NewCreation),
            "bonding_curve" => Some(LifecycleState::BondingCurve),
            "near_graduation" => Some(LifecycleState::NearGraduation),
            "graduated" => Some(LifecycleState::Graduated),
            "post_migration" => Some(LifecycleState::PostMigration),
            _ => None,
        }
    }

    /// Lifecycle states eligible for entry signals.
    pub fn entry_supported(self) -> bool {
        matches!(
            self,
            LifecycleState::NewCreation
                | LifecycleState::BondingCurve
                | LifecycleState::NearGraduation
                | LifecycleState::Graduated
                | LifecycleState::PostMigration
        )
    }
}

/// Narrative taxonomy. Telegram/social text is untrusted provenance only.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NarrativeCategory {
    Ai,
    Defi,
    DepIn,
    Meme,
    Celebrity,
    Political,
    Gaming,
    Rwa,
    SolanaEcosystem,
    Launchpad,
    CommunityTakeover,
    Unknown,
}

impl NarrativeCategory {
    pub fn slug(self) -> &'static str {
        match self {
            NarrativeCategory::Ai => "ai",
            NarrativeCategory::Defi => "defi",
            NarrativeCategory::DepIn => "dep_in",
            NarrativeCategory::Meme => "meme",
            NarrativeCategory::Celebrity => "celebrity",
            NarrativeCategory::Political => "political",
            NarrativeCategory::Gaming => "gaming",
            NarrativeCategory::Rwa => "rwa",
            NarrativeCategory::SolanaEcosystem => "solana_ecosystem",
            NarrativeCategory::Launchpad => "launchpad",
            NarrativeCategory::CommunityTakeover => "community_takeover",
            NarrativeCategory::Unknown => "unknown",
        }
    }

    pub fn parse_slug(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ai" => Some(NarrativeCategory::Ai),
            "defi" => Some(NarrativeCategory::Defi),
            "dep_in" | "depin" => Some(NarrativeCategory::DepIn),
            "meme" => Some(NarrativeCategory::Meme),
            "celebrity" => Some(NarrativeCategory::Celebrity),
            "political" => Some(NarrativeCategory::Political),
            "gaming" => Some(NarrativeCategory::Gaming),
            "rwa" => Some(NarrativeCategory::Rwa),
            "solana_ecosystem" => Some(NarrativeCategory::SolanaEcosystem),
            "launchpad" => Some(NarrativeCategory::Launchpad),
            "community_takeover" => Some(NarrativeCategory::CommunityTakeover),
            "unknown" => Some(NarrativeCategory::Unknown),
            _ => None,
        }
    }
}

/// Funding radar case stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadarStage {
    Funded,
    Preparation,
    Deployed,
    Dismissed,
}

impl RadarStage {
    pub fn as_str(self) -> &'static str {
        match self {
            RadarStage::Funded => "funded",
            RadarStage::Preparation => "preparation",
            RadarStage::Deployed => "deployed",
            RadarStage::Dismissed => "dismissed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "funded" => Some(RadarStage::Funded),
            "preparation" => Some(RadarStage::Preparation),
            "deployed" => Some(RadarStage::Deployed),
            "dismissed" => Some(RadarStage::Dismissed),
            _ => None,
        }
    }
}

/// Normalized transfer (native or token) produced by chain adapters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormalizedTransfer {
    pub chain: ChainKind,
    /// Transaction/signature identifier unique per chain.
    pub signature: String,
    /// Index of the event within the transaction (idempotency component).
    pub event_index: i32,
    pub from_address: String,
    pub to_address: String,
    pub asset_kind: AssetKind,
    /// Mint/token contract; empty for native asset.
    pub mint: String,
    /// Raw integer amount as reported by the chain (lamports, base units).
    pub raw_amount: String,
    /// Normalized decimal amount (e.g. SOL with 9 decimals applied).
    pub amount: Decimal,
    pub slot: Option<u64>,
    pub block_time: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub source: String,
    pub commitment: Commitment,
}

/// Normalized trade event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormalizedTrade {
    pub chain: ChainKind,
    pub signature: String,
    pub event_index: i32,
    pub wallet: String,
    pub mint: String,
    pub side: TradeSide,
    pub raw_native_amount: String,
    pub raw_token_amount: String,
    pub native_amount: Decimal,
    pub token_amount: Decimal,
    pub usd_value: Option<Decimal>,
    pub slot: Option<u64>,
    pub block_time: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub dex_id: Option<String>,
    pub source: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeSide {
    Buy,
    Sell,
}

impl TradeSide {
    pub fn as_str(self) -> &'static str {
        match self {
            TradeSide::Buy => "buy",
            TradeSide::Sell => "sell",
        }
    }
}

/// Funding event handed to the funding radar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FundingEvent {
    pub chain: ChainKind,
    pub signature: String,
    pub slot: Option<u64>,
    pub observed_at: DateTime<Utc>,
    pub commitment: Commitment,
    pub from_address: String,
    pub to_address: String,
    pub asset_kind: AssetKind,
    pub mint: String,
    pub raw_amount: String,
    /// Native-asset amount normalized (SOL units, not lamports).
    pub native_amount: Decimal,
    /// Event-time USD value; required for USD thresholds and token funding.
    pub amount_usd: Option<Decimal>,
    /// Event-time native/USD price used for `amount_usd`, when known.
    pub native_usd_price: Option<Decimal>,
    pub recipient_age_seconds: Option<i64>,
    /// Classification of the sending address, when known.
    pub source_kind: Option<WalletLabelKind>,
    pub raw: serde_json::Value,
}

impl FundingEvent {
    /// True when the sending address is infrastructure (exchange/bridge/MEV/Dex bot).
    /// Infrastructure is retained as provenance but excluded from alpha confidence.
    pub fn source_is_infrastructure(&self) -> bool {
        matches!(
            self.source_kind,
            Some(
                WalletLabelKind::Exchange
                    | WalletLabelKind::Bridge
                    | WalletLabelKind::KuiperExchange
                    | WalletLabelKind::MevBot
                    | WalletLabelKind::DexBot
            )
        )
    }
}

/// A normalized event batch from one transaction.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEvents {
    pub transfers: Vec<NormalizedTransfer>,
    pub trades: Vec<NormalizedTrade>,
    pub mints_created: Vec<String>,
    pub token_accounts_created: Vec<(String, String)>,
}

/// Page of wallet history.
#[derive(Clone, Debug, Default)]
pub struct HistoryPage {
    pub events: NormalizedEvents,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

/// Page of funding scan results.
#[derive(Clone, Debug, Default)]
pub struct FundingPage {
    pub events: Vec<FundingEvent>,
    pub next_cursor: Option<String>,
}

/// Funding radar case record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FundingRadarCase {
    pub id: i64,
    pub chain: ChainKind,
    pub recipient: String,
    pub first_funded_at: DateTime<Utc>,
    pub first_funding_usd: Option<Decimal>,
    pub first_funding_native: Decimal,
    pub source_address: String,
    pub source_kind: Option<WalletLabelKind>,
    pub fanout_count: u32,
    pub deploy_window_ends_at: DateTime<Utc>,
    pub stage: RadarStage,
    pub confidence: u32,
    pub evidence: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}

/// Radar evaluation decision emitted by `evaluate_radar_case`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FundingRadarDecision {
    pub case_id: i64,
    pub chain: ChainKind,
    pub recipient: String,
    pub stage: RadarStage,
    pub previous_stage: Option<RadarStage>,
    pub confidence: u32,
    pub alert: Option<RadarAlert>,
    pub reason: String,
    pub evidence_count: u32,
}

/// Two-stage radar alert kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadarAlertKind {
    FundingWatch,
    PreparationAlert,
}

impl RadarAlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RadarAlertKind::FundingWatch => "funding_watch",
            RadarAlertKind::PreparationAlert => "preparation_alert",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RadarAlert {
    pub kind: RadarAlertKind,
    pub case_id: i64,
    pub chain: ChainKind,
    pub recipient: String,
    pub confidence: u32,
    /// Wording is fixed: `possible project preparation`; never claims of insider activity.
    pub message: String,
    pub evidence: serde_json::Value,
}

impl RadarAlert {
    pub fn funding_watch(case: &FundingRadarCase, evidence: serde_json::Value) -> Self {
        Self {
            kind: RadarAlertKind::FundingWatch,
            case_id: case.id,
            chain: case.chain,
            recipient: case.recipient.clone(),
            confidence: case.confidence,
            message: format!(
                "funding_watch: {} received {} {} from {}; possible project preparation",
                short_addr(&case.recipient),
                case.first_funding_native.normalize(),
                case.chain,
                short_addr(&case.source_address)
            ),
            evidence,
        }
    }

    pub fn preparation(case: &FundingRadarCase, evidence: serde_json::Value) -> Self {
        Self {
            kind: RadarAlertKind::PreparationAlert,
            case_id: case.id,
            chain: case.chain,
            recipient: case.recipient.clone(),
            confidence: case.confidence,
            message: format!(
                "preparation_alert: {} shows possible project preparation (confidence {})",
                short_addr(&case.recipient),
                case.confidence
            ),
            evidence,
        }
    }
}

/// Short address rendering for logs/alerts; never secrets.
pub fn short_addr(address: &str) -> String {
    let chars: Vec<char> = address.chars().collect();
    if chars.len() <= 10 {
        address.to_string()
    } else {
        let head: String = chars.iter().take(5).collect();
        let tail: String = chars.iter().skip(chars.len() - 4).collect();
        format!("{head}…{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_kind_roundtrip() {
        for chain in [ChainKind::Solana, ChainKind::Robinhood] {
            assert_eq!(ChainKind::parse(chain.as_str()), Some(chain));
        }
        assert_eq!(ChainKind::parse("sol"), Some(ChainKind::Solana));
        assert_eq!(ChainKind::parse("SOLANA"), Some(ChainKind::Solana));
        assert_eq!(ChainKind::parse("rh"), Some(ChainKind::Robinhood));
        assert_eq!(ChainKind::parse("ethereum"), None);
    }

    #[test]
    fn wallet_label_parse_covers_gmgn_taxonomy() {
        for label in [
            "smart_degen",
            "sniper",
            "bundler",
            "rat_trader",
            "dex_bot",
            "mev_bot",
            "fresh_wallet",
            "dev",
            "insider",
            "arbitrage",
            "copy_trader",
            "market_maker",
            "exchange",
            "bridge",
            "kuiper_exchange",
            "project",
        ] {
            assert!(
                WalletLabelKind::parse(label).is_some(),
                "label {label} must parse"
            );
        }
        assert_eq!(WalletLabelKind::parse("unknown_label"), None);
    }

    #[test]
    fn infrastructure_sources_detected() {
        let event = |kind: Option<WalletLabelKind>| FundingEvent {
            chain: ChainKind::Solana,
            signature: "sig".into(),
            slot: None,
            observed_at: Utc::now(),
            commitment: Commitment::Confirmed,
            from_address: "from".into(),
            to_address: "to".into(),
            asset_kind: AssetKind::Native,
            mint: String::new(),
            raw_amount: "1".into(),
            native_amount: Decimal::ONE,
            amount_usd: None,
            native_usd_price: None,
            recipient_age_seconds: None,
            source_kind: kind,
            raw: serde_json::Value::Null,
        };
        assert!(event(Some(WalletLabelKind::Exchange)).source_is_infrastructure());
        assert!(event(Some(WalletLabelKind::Bridge)).source_is_infrastructure());
        assert!(event(Some(WalletLabelKind::MevBot)).source_is_infrastructure());
        assert!(event(Some(WalletLabelKind::DexBot)).source_is_infrastructure());
        assert!(!event(Some(WalletLabelKind::Dev)).source_is_infrastructure());
        assert!(!event(None).source_is_infrastructure());
    }

    #[test]
    fn alert_wording_avoids_insider_claims() {
        let case = FundingRadarCase {
            id: 1,
            chain: ChainKind::Solana,
            recipient: "RecipientWalletAddressThatIsLong".into(),
            first_funded_at: Utc::now(),
            first_funding_usd: Some(Decimal::from(20_000)),
            first_funding_native: Decimal::from(150),
            source_address: "SourceWalletAddressAlsoLong".into(),
            source_kind: None,
            fanout_count: 0,
            deploy_window_ends_at: Utc::now(),
            stage: RadarStage::Funded,
            confidence: 55,
            evidence: serde_json::Value::Null,
            updated_at: Utc::now(),
        };
        let watch = RadarAlert::funding_watch(&case, serde_json::Value::Null);
        let prep = RadarAlert::preparation(&case, serde_json::Value::Null);
        for alert in [watch, prep] {
            let msg = alert.message.to_lowercase();
            assert!(msg.contains("possible project preparation"));
            assert!(!msg.contains("insider"));
            assert!(!msg.contains("project confirmed"));
        }
    }

    #[test]
    fn commitment_ordering() {
        assert!(Commitment::Processed < Commitment::Confirmed);
        assert!(Commitment::Confirmed < Commitment::Finalized);
    }

    #[test]
    fn short_addr_truncates_long_addresses() {
        assert_eq!(short_addr("short"), "short");
        let long = "VeryLongWalletAddressForTests";
        assert!(short_addr(long).starts_with("VeryL"));
        assert!(short_addr(long).ends_with("ests"));
    }
}
