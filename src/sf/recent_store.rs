//! Persistence for Token Recent + Deployer/Social Reuse (REV-020 / REV-023).
//!
//! sqlx-backed store over the `recent_events` / `social_identities` tables
//! (migrations 1018 + 1019). Append-only: inserts only; retraction is a NEW row.
//!
//! REV-023 corrections:
//! - typed `DateTime<Utc>` bound directly to PostgreSQL `timestamptz` (never a
//!   Rust `String`);
//! - typed `RecentRelation` bound to the PostgreSQL `recent_relation` enum
//!   (never a Rust `String`);
//! - every read/write carries the authenticated `workspace_id` (workspace
//!   isolation);
//! - relation projection returns one row per relation + target, preserving the
//!   stored confidence/truth/evidence (no `DISTINCT ON (relation)` truncation,
//!   no hard-coded `Estimated`).

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::recent::{CandidateRelation, RecentEvent, RecentRelation};

/// The canonical columns selected for a `recent_events` read. `occurred_at` and
/// `observed_at` are typed `timestamptz` (returned as `DateTime<Utc>`), and
/// `relation` is the typed `recent_relation` enum (returned as
/// `Option<RecentRelation>`).
const RECENT_EVENT_COLUMNS: &str = r#"
    event_id, event_type, anchor_identity, related_identities,
    chain_qualified_contract, occurred_at, observed_at,
    relation, truth_status, confidence, confidence_level,
    evidence_refs, dependency_group, freshness, coverage,
    capability_status, retraction
"#;

/// Insert one recent event (append-only), scoped to a workspace. Returns the new
/// row id. The anchor invariant (`anchor_identity == chain_qualified_contract`)
/// is enforced before append (REV-023 §1).
pub async fn insert_recent_event(
    pool: &PgPool,
    workspace_id: i64,
    e: &RecentEvent,
) -> Result<i64> {
    if e.anchor_identity != e.chain_qualified_contract {
        anyhow::bail!("anchor_identity != chain_qualified_contract");
    }
    let id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO recent_events (
            workspace_id, event_id, token_identity, event_type, anchor_identity,
            related_identities, chain_qualified_contract, occurred_at, observed_at,
            relation, truth_status, confidence, confidence_level, evidence_refs,
            dependency_group, freshness, coverage, capability_status, retraction
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(&e.event_id)
    .bind(&e.anchor_identity)
    .bind(&e.event_type)
    .bind(&e.anchor_identity)
    .bind(serde_json::to_value(&e.related_identities).unwrap_or_default())
    .bind(&e.chain_qualified_contract)
    .bind(e.occurred_at)
    .bind(e.observed_at)
    .bind(e.relation)
    .bind(truth_status_str(&e.truth_status))
    .bind(e.confidence)
    .bind(e.confidence_level.as_str())
    .bind(serde_json::to_value(&e.evidence_refs).unwrap_or_default())
    .bind(&e.dependency_group)
    .bind(e.freshness.as_ref().map(|f| serde_json::to_value(f).unwrap_or_default()))
    .bind(coverage_str(e.coverage))
    .bind(capability_str(e.capability_status))
    .bind(e.retraction.as_ref().map(|r| serde_json::to_value(r).unwrap_or_default()))
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Row projection for a recent event (avoids sqlx's 16-tuple `FromRow` limit).
#[derive(sqlx::FromRow)]
struct RecentEventRow {
    event_id: String,
    event_type: String,
    anchor_identity: String,
    related_identities: serde_json::Value,
    chain_qualified_contract: String,
    occurred_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    relation: Option<RecentRelation>,
    truth_status: String,
    confidence: Option<f64>,
    confidence_level: String,
    evidence_refs: serde_json::Value,
    dependency_group: Option<String>,
    freshness: Option<serde_json::Value>,
    coverage: String,
    capability_status: String,
    retraction: Option<serde_json::Value>,
}

impl RecentEventRow {
    fn into_event(self) -> Option<RecentEvent> {
        Some(RecentEvent {
            event_id: self.event_id,
            event_type: self.event_type,
            anchor_identity: self.anchor_identity,
            related_identities: serde_json::from_value(self.related_identities).ok()?,
            chain_qualified_contract: self.chain_qualified_contract,
            occurred_at: self.occurred_at,
            observed_at: self.observed_at,
            relation: self.relation,
            truth_status: truth_status_from_str(&self.truth_status),
            confidence: self.confidence,
            confidence_level: confidence_from_str(&self.confidence_level),
            evidence_refs: serde_json::from_value(self.evidence_refs).ok()?,
            dependency_group: self.dependency_group,
            freshness: self.freshness.and_then(|f| serde_json::from_value(f).ok()),
            coverage: coverage_from_str(&self.coverage),
            capability_status: capability_from_str(&self.capability_status),
            retraction: self.retraction.and_then(|x| serde_json::from_value(x).ok()),
        })
    }
}

/// Fetch a token's recent timeline within a window (`1h|24h|7d|30d|all`),
/// scoped to a workspace. Sorted by `occurred_at` ascending.
pub async fn fetch_recent_timeline(
    pool: &PgPool,
    workspace_id: i64,
    token_identity: &str,
    window: &str,
) -> Result<Vec<RecentEvent>> {
    let interval = match window {
        "1h" => Some("interval '1 hour'"),
        "24h" => Some("interval '24 hours'"),
        "7d" => Some("interval '7 days'"),
        "30d" => Some("interval '30 days'"),
        _ => None, // "all" or unknown -> no time bound
    };

    let rows: Vec<RecentEventRow> = if let Some(interval) = interval {
        sqlx::query_as(&format!(
            "SELECT {RECENT_EVENT_COLUMNS} FROM recent_events \
             WHERE workspace_id = $1 AND token_identity = $2 AND occurred_at >= now() - {interval} \
             ORDER BY occurred_at ASC"
        ))
        .bind(workspace_id)
        .bind(token_identity)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(&format!(
            "SELECT {RECENT_EVENT_COLUMNS} FROM recent_events \
             WHERE workspace_id = $1 AND token_identity = $2 ORDER BY occurred_at ASC"
        ))
        .bind(workspace_id)
        .bind(token_identity)
        .fetch_all(pool)
        .await?
    };

    Ok(rows.into_iter().filter_map(|r| r.into_event()).collect())
}

/// Fetch corroborated relations for a token, scoped to a workspace. Projects one
/// row per relation + target, preserving stored confidence, truth status, and
/// evidence (REV-023 §1). Superseded/erroneous rows are excluded from the
/// current view.
pub async fn fetch_relations(
    pool: &PgPool,
    workspace_id: i64,
    token_identity: &str,
) -> Result<Vec<CandidateRelation>> {
    #[derive(sqlx::FromRow)]
    struct RelationRow {
        relation: RecentRelation,
        related_identities: serde_json::Value,
        evidence_refs: serde_json::Value,
        confidence_level: String,
    }

    let rows: Vec<RelationRow> = sqlx::query_as(
        r#"
        SELECT relation, related_identities, evidence_refs, confidence_level
          FROM recent_events
         WHERE workspace_id = $1
           AND token_identity = $2
           AND relation IS NOT NULL
           AND truth_status NOT IN ('superseded', 'erroneous')
         ORDER BY occurred_at DESC
        "#,
    )
    .bind(workspace_id)
    .bind(token_identity)
    .fetch_all(pool)
    .await?;

    // One candidate per (relation, target), preserving confidence/truth.
    let mut out: Vec<CandidateRelation> = Vec::new();
    let mut seen: std::collections::HashSet<(RecentRelation, String)> =
        std::collections::HashSet::new();
    for r in rows {
        let evidence_refs: Vec<String> =
            serde_json::from_value(r.evidence_refs).unwrap_or_default();
        let related_ids: Vec<super::recent::IdentityKey> =
            serde_json::from_value(r.related_identities).unwrap_or_default();
        for to_identity in related_ids {
            let key = (r.relation, to_identity.value.clone());
            if !seen.insert(key) {
                continue;
            }
            out.push(CandidateRelation {
                to_identity,
                relation: r.relation,
                confidence: confidence_from_str(&r.confidence_level),
                evidence_refs: evidence_refs.clone(),
            });
        }
    }
    Ok(out)
}

fn truth_status_str(s: &super::core::TruthStatus) -> &'static str {
    use super::core::TruthStatus::*;
    match s {
        Unknown => "unknown",
        Confirmed => "confirmed",
        Disputed => "disputed",
        Superseded => "superseded",
        Erroneous => "erroneous",
    }
}

fn truth_status_from_str(s: &str) -> super::core::TruthStatus {
    use super::core::TruthStatus::*;
    match s {
        "confirmed" => Confirmed,
        "disputed" => Disputed,
        "superseded" => Superseded,
        "erroneous" => Erroneous,
        _ => Unknown,
    }
}

fn confidence_from_str(s: &str) -> super::recent::RecentConfidence {
    use super::recent::RecentConfidence::*;
    match s {
        "exact" => Exact,
        "reconstructed" => Reconstructed,
        "estimated" => Estimated,
        _ => Insufficient,
    }
}

fn coverage_str(c: super::recent::Coverage) -> &'static str {
    use super::recent::Coverage::*;
    match c {
        Full => "full",
        Degraded => "degraded",
        OnDemand => "on_demand",
        Unavailable => "unavailable",
    }
}

fn coverage_from_str(s: &str) -> super::recent::Coverage {
    use super::recent::Coverage::*;
    match s {
        "full" => Full,
        "degraded" => Degraded,
        "on_demand" => OnDemand,
        _ => Unavailable,
    }
}

fn capability_str(c: super::recent::CapabilityStatus) -> &'static str {
    use super::recent::CapabilityStatus::*;
    match c {
        Available => "available",
        Insufficient => "insufficient",
        Unavailable => "unavailable",
    }
}

fn capability_from_str(s: &str) -> super::recent::CapabilityStatus {
    use super::recent::CapabilityStatus::*;
    match s {
        "available" => Available,
        "insufficient" => Insufficient,
        _ => Unavailable,
    }
}
