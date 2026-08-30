//! Caller intelligence domain (Phase 2).
//!
//! Canonical source: PLAN SWI §8.5 (Caller Intelligence): Telegram MTProto
//! caller truth, CA resolution, immutable T0 snapshot, call timestamp + lead
//! time, MFE/MAE, realistic copy entry/PnL, outcome windows through +21d,
//! caller reputation by regime and sample confidence, propagation and
//! copy-caller graph.

use serde::{Deserialize, Serialize};

/// Caller platform (doc: Telegram is caller truth; X primary; Web/TikTok
/// token-triggered).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CallerPlatform {
    Telegram,
    X,
    Web,
    Tiktok,
}

/// A caller's signal/mention of a token, with immutable T0 snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub caller_id: String,
    pub platform: CallerPlatform,
    pub token: String, // contract address (CA resolution)
    pub occurred_at: String,
    pub lead_time: Option<String>, // time from call to entry
    pub immutable_t0_snapshot: String, // content hash of the T0 evidence
    pub outcome: Option<CallOutcome>,
}

/// Caller outcome metrics (doc §8.5): MFE/MAE, realistic copy entry/PnL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallOutcome {
    pub mfe: Option<f64>, // maximum favorable excursion
    pub mae: Option<f64>, // maximum adverse excursion
    pub copy_entry: Option<f64>, // realistic copy entry price
    pub copy_pnl: Option<f64>,   // realistic copy PnL
    pub outcome_window_days: u32, // fixed windows through +21d (doc)
}

/// Caller reputation, by regime and sample confidence (doc §8.5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallerReputation {
    pub caller_id: String,
    pub regime: String, // market regime the reputation is scoped to
    pub sample_size: u32,
    pub sample_confidence: Option<f64>,
    pub hit_rate: Option<f64>,
    pub avg_mfe: Option<f64>,
}

/// Propagation / copy-caller graph edge (doc §8.5). A caller propagates a call
/// originally made by another caller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallerPropagationEdge {
    pub from_caller_id: String, // original caller
    pub to_caller_id: String,   // copy caller
    pub token: String,
    pub occurred_at: String,
    pub truth_status: crate::sf::core::TruthStatus,
    pub confidence: Option<f64>,
}
