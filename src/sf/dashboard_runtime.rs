//! Runtime logic: dashboard projection assembly and retention routing (Phase 1).
//!
//! Canonical source: PLAN SWI §22 "Dashboard information architecture" and §21
//! "Data retention" (hot/warm/cold tiers). Operational dashboard is
//! opportunity-first; investigation workspace is research-first. Hot-tier
//! projections are query-indexed and updated by scalar projections, never
//! mutated in place of history (archive-not-delete, principle #6).
//!
//! This module assembles the frozen `dashboard.rs` types (`DashboardView`,
//! `HotProjection`, `RetentionTier`) and introduces no new frozen state.

use super::dashboard::{DashboardSection, DashboardView, HotProjection, RetentionTier, Surface};

/// Assign a retention tier from an entity's recency/activity (doc §21):
/// - active opportunities/positions/intents -> HOT
/// - full event/evidence + outcome windows + decision bundles -> WARM
/// - compressed raw evidence + tombstones + closed executions -> COLD
///
/// `is_active` (has an active opportunity/position/intent) selects HOT;
/// otherwise `has_open_window` (still within an outcome window) selects WARM;
/// otherwise COLD.
pub fn retention_tier(is_active: bool, has_open_window: bool) -> RetentionTier {
    if is_active {
        RetentionTier::Hot
    } else if has_open_window {
        RetentionTier::Warm
    } else {
        RetentionTier::Cold
    }
}

/// Assemble a dashboard view descriptor. `opportunity_first` is `true` for the
/// operational (opportunity-first) dashboard and `false` for the research-first
/// investigation workspace.
pub fn dashboard_view(
    section: DashboardSection,
    surface: Surface,
    retention: RetentionTier,
    opportunity_first: bool,
) -> DashboardView {
    DashboardView {
        section,
        surface,
        retention,
        opportunity_first,
    }
}

/// Build a hot-tier projection from active entity keys. The projection is a
/// fast query index; `refreshed_at` is set by the caller.
pub fn build_hot_projection(
    opportunities: Vec<String>,
    positions: Vec<String>,
    intents: Vec<String>,
    refreshed_at: &str,
) -> HotProjection {
    HotProjection {
        active_opportunities: opportunities,
        active_positions: positions,
        active_intents: intents,
        refreshed_at: refreshed_at.to_string(),
    }
}

/// Whether a dashboard view should surface to the operational (opportunity-first)
/// dashboard. Operational surfaces are HOT-tier; research surfaces may be WARM/
/// COLD. A HOT projection that is also opportunity-first is operational.
pub fn is_operational(view: &DashboardView) -> bool {
    view.opportunity_first && view.retention == RetentionTier::Hot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_tier_routing() {
        assert_eq!(retention_tier(true, true), RetentionTier::Hot);
        assert_eq!(retention_tier(false, true), RetentionTier::Warm);
        assert_eq!(retention_tier(false, false), RetentionTier::Cold);
    }

    #[test]
    fn operational_is_hot_and_opportunity_first() {
        let v = dashboard_view(
            DashboardSection::CommandCenter,
            Surface::SignalSpine,
            RetentionTier::Hot,
            true,
        );
        assert!(is_operational(&v));

        let research = dashboard_view(
            DashboardSection::Lab,
            Surface::MissedRunnerReview,
            RetentionTier::Warm,
            false,
        );
        assert!(!is_operational(&research));
    }

    #[test]
    fn hot_projection_assembles_entities() {
        let hp = build_hot_projection(
            vec!["opp1".into()],
            vec!["pos1".into(), "pos2".into()],
            vec!["int1".into()],
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(hp.active_opportunities.len(), 1);
        assert_eq!(hp.active_positions.len(), 2);
        assert_eq!(hp.active_intents.len(), 1);
        assert_eq!(hp.refreshed_at, "2026-01-01T00:00:00Z");
    }
}
