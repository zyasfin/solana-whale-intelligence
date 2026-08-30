//! Portfolio/risk domain (Phase 1) and decision component classes.
//!
//! Canonical source: PLAN SWI §8.11 (Portfolio/Risk) and §12.2-12.3
//! (Component classes, Opportunity outcomes).

use serde::{Deserialize, Serialize};

/// Component classes (doc §12.2). `N/A`, missing, zero, and safe are distinct
/// (principle #3); no universal score.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentClass {
    MandatoryPass,
    SizingInput,
    StrategyInput,
    HaltInput,
}

/// A component value that distinguishes N/A, missing, zero, and safe
/// (doc §12.2 — these are NOT interchangeable).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentValue {
    Na,
    Missing,
    Zero,
    Safe,
    Present(serde_json::Value),
}

/// Opportunity outcomes (doc §12.3).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpportunityOutcome {
    Surface,
    Watch,
    Reject,
    Cooldown,
    PaperIntent,
    ConfirmIntent,
    AutoIntent,
    Halt,
}

/// Portfolio snapshot (doc §8.11): exposure per token/pool/chain/strategy,
/// correlated exposure, wallet reserve, total open notional, realized/unrealized
/// PnL, daily loss/drawdown, ambiguous execution exposure, treasury/hot-wallet
/// separation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub snapshot_at: String,
    pub exposure: Vec<Exposure>,
    pub total_open_notional: String,
    pub realized_pnl: String,
    pub unrealized_pnl: String,
    pub daily_loss: Option<String>,
    pub drawdown: Option<String>,
    pub ambiguous_execution_exposure: Option<String>,
    pub treasury_separated: bool, // treasury never in execution worker
}

/// A single exposure line (per token/pool/chain/strategy).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exposure {
    pub kind: String, // token | pool | chain | strategy
    pub key: String,
    pub notional: String,
    pub correlated: Option<Vec<String>>,
}

/// Risk finding (doc §8.11 + §10 risk_findings).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskFinding {
    pub finding_type: String,
    pub severity: Severity,
    pub status: FindingStatus,
    pub payload: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FindingStatus {
    Open,
    Acknowledged,
    Mitigated,
    Closed,
}
