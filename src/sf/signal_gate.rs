//! Runtime logic: entry-signal gate model (Phase 1).
//!
//! Canonical concept reference: pre-freeze `signals.rs::evaluate_token` (gate
//! model: first-failure-wins, exactly one rejection code per evaluation, gate
//! order matters, fail-closed on missing data). Concept reused, code not copied
//! (principle #13).
//!
//! Aligns with PLAN SWI principles: #3 (no opaque universal score — every gate
//! yields a distinct rejection code) and #7 (fail-closed: missing mandatory
//! data blocks, never treated as safe).

use serde::{Deserialize, Serialize};

/// Rejection codes (closed set). One code per failed evaluation.
/// Derived from pre-freeze `REJECTION_CODES`, narrowed to the entry-gate set.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    InsufficientClusters,
    InsufficientBuys,
    UnsupportedLifecycle,
    TokenTooOld,
    InsufficientLiquidity,
    CriticalRisk,
    WashRisk,
    StaleMarket,
    IneligibleSkill,
    IneligibleCopyability,
    IncompleteHistory,
}

/// Gate thresholds (config-driven, mirrors pre-freeze `SignalsConfig`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct GateThresholds {
    pub min_eligible_clusters: u32,      // 2
    pub min_meaningful_buys: u32,        // 2
    pub max_token_age_hours: i64,        // 24
    pub min_liquidity_usd: u64,          // 20_000
    pub max_market_age_seconds: i64,     // 300 (5m)
    pub min_skill: u32,                  // 70
    pub min_copyability: u32,            // 60
    pub min_history_completeness: f64,   // 0.80
}

impl Default for GateThresholds {
    fn default() -> Self {
        Self {
            min_eligible_clusters: 2,
            min_meaningful_buys: 2,
            max_token_age_hours: 24,
            min_liquidity_usd: 20_000,
            max_market_age_seconds: 300,
            min_skill: 70,
            min_copyability: 60,
            min_history_completeness: 0.80,
        }
    }
}

/// Inputs to the entry gate (mirrors pre-freeze gate inputs).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntryGateInput {
    pub eligible_clusters: u32,
    pub meaningful_buys: u32,
    pub lifecycle_supported: bool,
    pub token_age_hours: Option<i64>, // None = unknown (fail-closed)
    pub liquidity_usd: Option<u64>,   // None = unknown (fail-closed)
    pub risk_flags: Vec<RiskFlag>,
    pub market_age_seconds: Option<i64>, // None = unknown (fail-closed)
    pub max_skill: u32,
    pub max_copyability: u32,
    pub min_history_completeness: f64,
}

/// Risk flags that gate entry.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskFlag {
    CriticalRisk,
    WashTrading,
    Honeypot,
    Rugged,
}

/// Result of an entry-gate evaluation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum GateOutcome {
    Accepted,
    Rejected(RejectionCode),
}

/// Evaluate the entry signal gate. First failure wins (gate order matters);
/// missing mandatory data (None) fails closed. Returns the rejection code for
/// the FIRST failing gate, or `Accepted` if all pass.
pub fn evaluate_entry_gate(
    input: &EntryGateInput,
    t: &GateThresholds,
) -> GateOutcome {
    // 1. eligible clusters
    if input.eligible_clusters < t.min_eligible_clusters {
        return GateOutcome::Rejected(RejectionCode::InsufficientClusters);
    }
    // 2. meaningful buys
    if input.meaningful_buys < t.min_meaningful_buys {
        return GateOutcome::Rejected(RejectionCode::InsufficientBuys);
    }
    // 3. lifecycle
    if !input.lifecycle_supported {
        return GateOutcome::Rejected(RejectionCode::UnsupportedLifecycle);
    }
    // 4. age (fail-closed on None)
    match input.token_age_hours {
        Some(age) if age <= t.max_token_age_hours => {}
        _ => return GateOutcome::Rejected(RejectionCode::TokenTooOld),
    }
    // 5. liquidity (fail-closed on None)
    match input.liquidity_usd {
        Some(l) if l >= t.min_liquidity_usd => {}
        _ => return GateOutcome::Rejected(RejectionCode::InsufficientLiquidity),
    }
    // 6. risk (wash vs critical)
    if input.risk_flags.contains(&RiskFlag::WashTrading) {
        return GateOutcome::Rejected(RejectionCode::WashRisk);
    }
    if input.risk_flags.iter().any(|f| matches!(f, RiskFlag::CriticalRisk | RiskFlag::Honeypot | RiskFlag::Rugged)) {
        return GateOutcome::Rejected(RejectionCode::CriticalRisk);
    }
    // 7. market freshness (fail-closed on None)
    match input.market_age_seconds {
        Some(a) if a <= t.max_market_age_seconds => {}
        _ => return GateOutcome::Rejected(RejectionCode::StaleMarket),
    }
    // 8. skill
    if input.max_skill < t.min_skill {
        return GateOutcome::Rejected(RejectionCode::IneligibleSkill);
    }
    // 9. copyability
    if input.max_copyability < t.min_copyability {
        return GateOutcome::Rejected(RejectionCode::IneligibleCopyability);
    }
    // 10. history completeness
    if input.min_history_completeness < t.min_history_completeness {
        return GateOutcome::Rejected(RejectionCode::IncompleteHistory);
    }

    GateOutcome::Accepted
}
