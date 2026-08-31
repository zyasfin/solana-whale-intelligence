//! Source/provider domain: registry, provider pool, health.
//!
//! Canonical source: PLAN SWI §7.3, "Provider pools" (lines 307-329), §8.12,
//! and §10 "Sources/providers".

use serde::{Deserialize, Serialize};

use super::core::SourceHealthState;

/// Capability role (doc principle #4: capability mandatory, vendor replaceable).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityRole {
    Mandatory,
    Vendor,
    Fallback,
}

/// A logical upstream source / vendor family (e.g. Helius, Birdeye, DEX Screener).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub family: String,
    pub kind: String, // rpc | api | scraper | stream
    pub platform: String,
    pub capabilities: Vec<String>,
    pub capability_role: CapabilityRole,
    pub enabled: bool,
}

/// Provider credential reference — external encrypted reference only. The UI
/// sees fingerprint/status only (doc line 637); no plaintext secret is stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCredentialRef {
    pub source_name: String,
    pub name: String,
    pub fingerprint: String,
    pub status: CredentialStatus,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStatus {
    Active,
    Disabled,
    AuthFailed,
    Invalid,
}

/// Provider health (doc §8.12). Includes last request/event/success, expected
/// cadence, parser success, schema staleness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub state: SourceHealthState,
    pub last_request_at: Option<String>,
    pub last_event_at: Option<String>,
    pub last_success_at: Option<String>,
    pub expected_cadence: Option<String>,
    pub parser_success_rate: Option<f64>,
    pub schema_stale: bool,
    pub consecutive_failures: u32,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        // REV-007-F08: a fresh provider has no request/event/success yet, so it
        // must NOT default to UP (connected-but-silent is not healthy). Start
        // SILENT until the first on-time success is observed.
        Self {
            state: SourceHealthState::Silent,
            last_request_at: None,
            last_event_at: None,
            last_success_at: None,
            expected_cadence: None,
            parser_success_rate: None,
            schema_stale: false,
            consecutive_failures: 0,
        }
    }
}

/// Source dependence (doc §7.3): two vendors repeating one upstream event are
/// NOT two independent confirmations.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceRelation {
    DerivesFrom,
    Mirrors,
    Resells,
}

// ============================================================================
// Provider tie-break (blocker #3 — RESOLVED)
// Deterministic provider selection: eligibility filter -> deterministic ranking
// -> final source_id lexicographic tie-break. No blind round-robin; no quota
// evasion. Encoded per PLAN SWI "Provider pools" rules (lines 307-329).
// ============================================================================

/// Why a provider is ineligible (fail-closed, principle #7).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum IneligibleReason {
    Disabled,
    CredentialAuthFailed,
    Cooldown,
    CircuitBreakerOpen,
    HealthDown,
    OutOfQuota,
    ChainIdInvalid,
}

/// A candidate provider for a request, with the fields tie-break needs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCandidate {
    pub source_id: String,
    pub capability_role: CapabilityRole,
    pub health_state: SourceHealthState,
    pub weight: f64,
    pub latency_ms: Option<u64>,
    pub cost_per_request: Option<f64>,
    pub enabled: bool,
    pub credential_active: bool,
    pub in_cooldown: bool,
    pub breaker_open: bool,
    pub has_quota: bool,
    pub chain_id_valid: bool,
}

impl ProviderCandidate {
    /// Stage 0 — eligibility filter (hard, fail-closed). Returns `None` if the
    /// provider is eligible; otherwise the reason it is excluded.
    pub fn ineligible_reason(&self) -> Option<IneligibleReason> {
        if !self.enabled {
            return Some(IneligibleReason::Disabled);
        }
        if !self.credential_active {
            return Some(IneligibleReason::CredentialAuthFailed);
        }
        if self.in_cooldown {
            return Some(IneligibleReason::Cooldown);
        }
        if self.breaker_open {
            return Some(IneligibleReason::CircuitBreakerOpen);
        }
        if matches!(self.health_state, SourceHealthState::Down | SourceHealthState::Disabled) {
            return Some(IneligibleReason::HealthDown);
        }
        if !self.has_quota {
            return Some(IneligibleReason::OutOfQuota);
        }
        if !self.chain_id_valid {
            return Some(IneligibleReason::ChainIdInvalid);
        }
        None
    }
}

/// Stage 1+2 — deterministic ranking. Returns an `Ordering` key tuple that,
/// when sorted ascending, yields the frozen preference order. The final
/// component (`source_id` lexicographic) makes the order stable/reproducible.
fn rank_key(c: &ProviderCandidate) -> (u8, u8, u64, u64, u64, String) {
    // capability_role: mandatory > vendor > fallback (higher rank first).
    let role_rank = match c.capability_role {
        CapabilityRole::Mandatory => 0, // lower key = earlier in ascending sort
        CapabilityRole::Vendor => 1,
        CapabilityRole::Fallback => 2,
    };
    // health: UP > RECOVERING > DEGRADED > SILENT (higher rank first).
    let health_rank = c.health_state.health_rank();
    // Invert to sort ascending with "best first": use 255 - rank.
    let health_key = 255u8.saturating_sub(health_rank);
    // weight descending (higher weight first) -> invert; use a fixed scale.
    let weight_key = (1000.0 - (c.weight * 1000.0).round()).max(0.0) as u64;
    // latency ascending (faster first); None -> worst (max).
    let latency_key = c.latency_ms.unwrap_or(u64::MAX);
    // cost ascending (cheaper first); None -> worst (max).
    let cost_key = (c.cost_per_request.unwrap_or(f64::MAX) * 1_000_000.0) as u64;
    // final stable tie-break: source_id lexicographic ascending.
    (role_rank, health_key, weight_key, latency_key, cost_key, c.source_id.clone())
}

/// Select the best eligible provider (frozen 3-stage scheme). Returns `None` if
/// no provider is eligible. Deterministic: same input -> same output.
pub fn select_provider(candidates: &[ProviderCandidate]) -> Option<ProviderCandidate> {
    candidates
        .iter()
        .filter(|c| c.ineligible_reason().is_none())
        .min_by_key(|c| rank_key(c))
        .cloned()
}
