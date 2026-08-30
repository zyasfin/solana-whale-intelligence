//! Job model (outbox/queue).
//!
//! Canonical source: PLAN SWI §11 "Job model" (lines 762-780).

use serde::{Deserialize, Serialize};

/// A queued job. Workers claim via `FOR UPDATE SKIP LOCKED` (doc §11).
/// Backoff, dead-letter incident, and operator retry are explicit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub job_type: String,
    pub entity_key: String,
    pub priority: i32,
    pub available_at: String,
    pub lease_owner: Option<String>,
    pub lease_until: Option<String>,
    pub attempts: u32,
    pub max_attempts: u32,
    pub dedupe_key: Option<String>,
    pub payload_version: Option<String>,
}

/// Priority triggers (doc §11): first meaningful liquidity, migration/graduation,
/// credible caller, smart-wallet entry, fresh-wallet/cabal burst, volume/trade
/// activation, dormant wake, LP economics/range change, source-health degradation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PriorityTrigger {
    FirstLiquidity,
    Migration,
    CredibleCaller,
    SmartWalletEntry,
    FreshWalletBurst,
    VolumeActivation,
    DormantWake,
    LpEconomics,
    SourceHealthDegradation,
}
