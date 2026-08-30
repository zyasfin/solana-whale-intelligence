//! Runtime logic: entity graph traversal and false-confluence detection (Phase 0).
//!
//! Canonical source: PLAN SWI §9 "Evidence and graph model" (lines 614-622).
//! Entity nodes + edges carry time, source, truth status, confidence, evidence
//! refs, valid window, and supersession. False confluence (two independent
//! wallets/events that merely LOOK correlated) must be flagged, not silently
//! merged (doc §8.7).
//!
//! This module provides pure helpers over the frozen `graph.rs` types
//! (`EntityNode`, `EntityEdge`) and introduces no new frozen state.

use super::core::TruthStatus;
use super::graph::{EdgeType, EntityEdge, EntityNode};

/// Whether an edge is currently valid at a reference timestamp `now_secs`
/// (within its `valid_from`..`valid_until` window). A missing `valid_until`
/// means "open-ended" (still valid). A malformed timestamp fails closed to
/// `false` (never treat an unparseable edge as currently valid).
pub fn is_edge_valid(edge: &EntityEdge, now_secs: i64) -> bool {
    let Some(from) = parse_secs(&edge.valid_from) else {
        return false; // unparseable -> invalid (fail-closed)
    };
    if now_secs < from {
        return false;
    }
    match &edge.valid_until {
        Some(until) => parse_secs(until).map(|u| now_secs <= u).unwrap_or(false),
        None => true,
    }
}

/// Detect false confluence between two edges that share the same `(from,to,edge_type)`
/// but whose evidence refs are disjoint and whose truth status is not `Confirmed`.
///
/// False confluence (doc §8.7): two wallets/events that merely look correlated.
/// Two edges claiming the same relationship with disjoint evidence and low
/// confidence are flagged rather than merged.
pub fn is_false_confluence(a: &EntityEdge, b: &EntityEdge) -> bool {
    if a.from_entity_key != b.from_entity_key
        || a.to_entity_key != b.to_entity_key
        || a.edge_type != b.edge_type
    {
        return false; // different relationship -> not confluence
    }
    // Disjoint evidence refs -> independent claims of the same edge.
    let disjoint = a
        .evidence_refs
        .iter()
        .all(|r| !b.evidence_refs.contains(r))
        && !a.evidence_refs.is_empty()
        && !b.evidence_refs.is_empty();
    // Low confidence ONLY when BOTH confidences are present AND below 0.5.
    // Missing confidence is NOT low confidence (missing != zero, principle #3).
    let low_confidence = match (a.confidence, b.confidence) {
        (Some(ca), Some(cb)) => ca < 0.5 && cb < 0.5,
        _ => false,
    };
    let non_confirmed = a.truth_status != TruthStatus::Confirmed && b.truth_status != TruthStatus::Confirmed;
    disjoint && low_confidence && non_confirmed
}

/// Count distinct neighbor entity keys reachable by a given edge type (a cheap
/// degree measure for a node, useful for graph-index projections).
pub fn neighbor_count(node: &EntityNode, edges: &[EntityEdge], edge_type: EdgeType) -> usize {
    edges
        .iter()
        .filter(|e| e.edge_type == edge_type && (e.from_entity_key == node.entity_key || e.to_entity_key == node.entity_key))
        .flat_map(|e| {
            let mut v = vec![e.from_entity_key.clone(), e.to_entity_key.clone()];
            v.retain(|k| k != &node.entity_key);
            v
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Parse an RFC3339 timestamp into unix seconds (None when unparseable).
fn parse_secs(s: &str) -> Option<i64> {
    s.parse::<chrono::DateTime<chrono::Utc>>()
        .ok()
        .map(|dt| dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: &str, to: &str, ty: EdgeType, refs: Vec<&str>, conf: f64, ts: TruthStatus) -> EntityEdge {
        EntityEdge {
            from_entity_key: from.into(),
            to_entity_key: to.into(),
            edge_type: ty,
            occurred_at: "2026-01-01T00:00:00Z".into(),
            source_id: None,
            truth_status: ts,
            confidence: Some(conf),
            valid_from: "2026-01-01T00:00:00Z".into(),
            valid_until: None,
            supersedes: None,
            evidence_refs: refs.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn edge_validity_window() {
        let e = edge("A", "B", EdgeType::FundedBy, vec![], 1.0, TruthStatus::Confirmed);
        // valid_from = 2026-01-01 = 1767225600
        assert!(is_edge_valid(&e, 1767225600));
        assert!(!is_edge_valid(&e, 1767225599)); // before valid_from
    }

    #[test]
    fn false_confluence_detected() {
        let a = edge("A", "B", EdgeType::FundedBy, vec!["ev1"], 0.3, TruthStatus::Disputed);
        let b = edge("A", "B", EdgeType::FundedBy, vec!["ev2"], 0.4, TruthStatus::Unknown);
        assert!(is_false_confluence(&a, &b));
    }

    #[test]
    fn confirmed_evidence_is_not_false_confluence() {
        let a = edge("A", "B", EdgeType::FundedBy, vec!["ev1"], 1.0, TruthStatus::Confirmed);
        let b = edge("A", "B", EdgeType::FundedBy, vec!["ev2"], 0.4, TruthStatus::Unknown);
        assert!(!is_false_confluence(&a, &b));
    }

    // REV-003-F02: malformed valid_from must always fail closed (false), even
    // when now_secs == i64::MAX.
    #[test]
    fn malformed_valid_from_fails_closed() {
        let mut e = edge("A", "B", EdgeType::FundedBy, vec![], 1.0, TruthStatus::Confirmed);
        e.valid_from = "not-a-date".into();
        assert!(!is_edge_valid(&e, i64::MAX));
    }

    // REV-003-F03: missing confidence is NOT low confidence.
    #[test]
    fn missing_confidence_is_not_false_confluence() {
        let mut a = edge("A", "B", EdgeType::FundedBy, vec!["ev1"], 0.3, TruthStatus::Disputed);
        let mut b = edge("A", "B", EdgeType::FundedBy, vec!["ev2"], 0.4, TruthStatus::Unknown);
        a.confidence = None;
        b.confidence = None;
        assert!(!is_false_confluence(&a, &b));
    }

    #[test]
    fn neighbor_count_is_distinct() {
        let node = EntityNode { entity_key: "A".into(), node_type: super::super::graph::NodeType::Wallet };
        let edges = vec![
            edge("A", "B", EdgeType::FundedBy, vec![], 1.0, TruthStatus::Confirmed),
            edge("A", "C", EdgeType::FundedBy, vec![], 1.0, TruthStatus::Confirmed),
            edge("A", "B", EdgeType::FundedBy, vec![], 1.0, TruthStatus::Confirmed), // duplicate neighbor
            edge("A", "D", EdgeType::CalledBy, vec![], 1.0, TruthStatus::Confirmed), // different type
        ];
        assert_eq!(neighbor_count(&node, &edges, EdgeType::FundedBy), 2); // B, C distinct
    }
}
