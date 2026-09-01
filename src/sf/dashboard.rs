//! Dashboard information architecture (Phase 1, dashboard core).
//!
//! Canonical source: PLAN SWI §22 "Dashboard information architecture" and §21
//! "Data retention" (hot/warm/cold tiers). Operational dashboard is
//! opportunity-first; investigation workspace is research-first.

use serde::{Deserialize, Serialize};

/// Top-level dashboard sections (doc §22).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DashboardSection {
    CommandCenter,
    Signals,
    Entities,
    Strategies,
    Lab,
    Operations,
}

/// Key dashboard surfaces (doc §22). These are query/view projections over the
/// evidence graph; no surface exposes a private key (gate #8).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    SignalSpine,
    LifecycleRail,
    EvidenceStrips,
    ConfidenceFreshnessTexture,
    NarrativeRiver,
    OriginTrace,
    TokenFamilyConstellation,
    CallerPropagationTree,
    RevivalSeismicView,
    LpRangeChamber,
    AutomationPolicies,
    ExecutionTape,
    SourceHealthCost,
    DecisionExplanation,
    MissedRunnerReview,
    // REV-020 recent-intelligence surfaces.
    TokenRecentTimeline,
    DeployerSocialReuse,
}

/// Data retention tier (doc §21).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RetentionTier {
    Hot,  // active opportunities/positions/intents, recent projections
    Warm, // full event/evidence, outcome windows, decision bundles
    Cold, // compressed raw evidence, tombstones, historical edges, closed executions
}

/// A dashboard view/query descriptor: which section + surface this projection
/// serves. The dashboard is opportunity-first (operational) or research-first
/// (investigation) depending on context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DashboardView {
    pub section: DashboardSection,
    pub surface: Surface,
    pub retention: RetentionTier,
    pub opportunity_first: bool, // operational vs research-first
}

/// Hot-tier operational projection (doc §21 "Active opportunities/positions/
/// intents"). Fast query index; updated by scalar projections, never mutated
/// in place of history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HotProjection {
    pub active_opportunities: Vec<String>,
    pub active_positions: Vec<String>,
    pub active_intents: Vec<String>,
    pub refreshed_at: String,
}
