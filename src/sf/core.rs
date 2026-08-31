//! Core domain: event/evidence envelope, entity ids, idempotency keys.
//!
//! Canonical source: PLAN SWI §7.1 "Canonical event envelope" (lines 363-401)
//! and §9 evidence model.

use serde::{Deserialize, Serialize};

/// Canonical event envelope (doc §7.1). Every field maps verbatim to the
/// frozen `EventEnvelope` block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event_id: String,
    pub workspace_id: Option<String>, // nullable for global public data
    pub chain: Option<String>,        // chain/platform
    pub event_type: String,
    pub entity_keys: Vec<String>,
    pub source_id: String,
    pub source_event_id: Option<String>,
    pub occurred_at: Option<String>, // nullable
    pub observed_at: String,
    pub ingested_at: String,
    pub raw_hash: String, // content hash of raw evidence
    pub raw_ref: String,  // content-addressed storage reference
    pub parser_version: Option<String>,
    pub payload_schema_version: String,
    pub truth_status: TruthStatus,
    pub confidence: Option<f64>,
}

/// Truth status of an observation/edge/envelope (doc evidence model).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TruthStatus {
    Unknown,
    Confirmed,
    Disputed,
    Superseded,
    Erroneous,
}

/// Source health states (doc §8.12). Connected-but-silent is not healthy.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SourceHealthState {
    Up,
    Silent,
    Degraded,
    Down,
    Recovering,
    Disabled,
}

impl SourceHealthState {
    /// Deterministic ranking for provider tie-break (frozen blocker #3).
    /// Higher = preferred: UP > RECOVERING > DEGRADED > SILENT.
    /// DOWN/DISABLED are ineligible (filtered before ranking).
    pub fn health_rank(self) -> u8 {
        match self {
            SourceHealthState::Up => 4,
            SourceHealthState::Recovering => 3,
            SourceHealthState::Degraded => 2,
            SourceHealthState::Silent => 1,
            SourceHealthState::Down => 0,
            SourceHealthState::Disabled => 0,
        }
    }
}

/// Idempotency key. Two frozen modes (doc §7.1):
/// - `StableId`: source_id + source_event_id + payload_schema_version
/// - `Fallback`: source_id + normalized entity + event type + time bucket + raw hash
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdempotencyKey {
    StableId {
        source_id: String,
        source_event_id: String,
        payload_schema_version: String,
    },
    Fallback {
        source_id: String,
        normalized_entity: String,
        event_type: String,
        time_bucket: String,
        raw_hash: String,
    },
}

impl EventEnvelope {
    /// Derive the idempotency key per the frozen defaults (doc §7.1). Returns
    /// `None` when the fallback path has a malformed/empty `observed_at`
    /// (REV-009-F07: an invalid mandatory timestamp must be rejected, not
    /// silently bucketed to an empty key).
    pub fn idempotency_key(&self) -> Option<IdempotencyKey> {
        match &self.source_event_id {
            Some(source_event_id) => Some(IdempotencyKey::StableId {
                source_id: self.source_id.clone(),
                source_event_id: source_event_id.clone(),
                payload_schema_version: self.payload_schema_version.clone(),
            }),
            None => {
                let bucket = time_bucket(&self.observed_at)?;
                Some(IdempotencyKey::Fallback {
                    source_id: self.source_id.clone(),
                    normalized_entity: self.entity_keys.join(","),
                    event_type: self.event_type.clone(),
                    time_bucket: bucket,
                    raw_hash: self.raw_hash.clone(),
                })
            }
        }
    }
}

/// An hourly time-bucket for the fallback idempotency key (doc §7.1). Returns
/// `None` for a malformed/empty timestamp (REV-009-F07), so the caller can
/// reject the envelope instead of using an empty bucket.
fn time_bucket(observed_at: &str) -> Option<String> {
    observed_at
        .parse::<chrono::DateTime<chrono::Utc>>()
        .ok()
        .map(|dt| (dt.timestamp().div_euclid(3600)).to_string())
}
