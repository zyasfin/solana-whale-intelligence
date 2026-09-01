//! Persistence for Token Recent + Deployer/Social Reuse (REV-020).
//!
//! sqlx-backed store over the `recent_events` / `social_identities` tables
//! (migration 1018). Append-only: inserts only; retraction is a NEW row. Mirrors
//! the existing `db.rs` sqlx style (`sqlx::query_as`, `PgPool`).

use anyhow::Result;
use sqlx::PgPool;

use super::recent::{CandidateRelation, RecentEvent};

/// Insert one recent event (append-only). Returns the new row id.
pub async fn insert_recent_event(pool: &PgPool, e: &RecentEvent) -> Result<i64> {
    let relation = e.relation.map(|r| r.as_str());
    let confidence_level = e.confidence_level.as_str();
    let coverage = coverage_str(e.coverage);
    let capability = capability_str(e.capability_status);
    let id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO recent_events (
            token_identity, event_type, anchor_identity, related_identities,
            chain_qualified_contract, occurred_at, observed_at, relation,
            truth_status, confidence, confidence_level, evidence_refs,
            dependency_group, freshness, coverage, capability_status, retraction
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
        RETURNING id
        "#,
    )
    .bind(&e.anchor_identity)
    .bind(&e.event_type)
    .bind(&e.anchor_identity)
    .bind(serde_json::to_value(&e.related_identities).unwrap_or_default())
    .bind(&e.chain_qualified_contract)
    .bind(&e.occurred_at)
    .bind(&e.observed_at)
    .bind(relation)
    .bind(truth_status_str(&e.truth_status))
    .bind(e.confidence)
    .bind(confidence_level)
    .bind(serde_json::to_value(&e.evidence_refs).unwrap_or_default())
    .bind(&e.dependency_group)
    .bind(e.freshness.as_ref().map(|f| serde_json::to_value(f).unwrap_or_default()))
    .bind(coverage)
    .bind(capability)
    .bind(e.retraction.as_ref().map(|r| serde_json::to_value(r).unwrap_or_default()))
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Row projection for a recent event (avoids sqlx's 16-tuple `FromRow` limit).
#[derive(sqlx::FromRow)]
struct RecentEventRow {
    event_type: String,
    anchor_identity: String,
    related_identities: serde_json::Value,
    chain_qualified_contract: String,
    occurred_at: String,
    observed_at: String,
    relation: Option<String>,
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
            event_type: self.event_type,
            anchor_identity: self.anchor_identity,
            related_identities: serde_json::from_value(self.related_identities).ok()?,
            chain_qualified_contract: self.chain_qualified_contract,
            occurred_at: self.occurred_at,
            observed_at: self.observed_at,
            relation: self.relation.as_deref().and_then(relation_from_str),
            truth_status: truth_status_from_str(&self.truth_status),
            confidence: self.confidence,
            confidence_level: confidence_from_str(&self.confidence_level),
            evidence_refs: serde_json::from_value(self.evidence_refs).ok()?,
            dependency_group: self.dependency_group,
            freshness: self
                .freshness
                .and_then(|f| serde_json::from_value(f).ok()),
            coverage: coverage_from_str(&self.coverage),
            capability_status: capability_from_str(&self.capability_status),
            retraction: self
                .retraction
                .and_then(|x| serde_json::from_value(x).ok()),
        })
    }
}

const RECENT_EVENT_COLUMNS: &str = r#"
    event_type, anchor_identity, related_identities,
    chain_qualified_contract, occurred_at::text, observed_at::text,
    relation::text, truth_status, confidence, confidence_level,
    evidence_refs, dependency_group, freshness, coverage,
    capability_status, retraction
"#;

/// Fetch a token's recent timeline within a window (`1h|24h|7d|30d|all`).
/// Sorted by `occurred_at` ascending.
pub async fn fetch_recent_timeline(
    pool: &PgPool,
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
             WHERE token_identity = $1 AND occurred_at >= now() - {interval} \
             ORDER BY occurred_at ASC"
        ))
        .bind(token_identity)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(&format!(
            "SELECT {RECENT_EVENT_COLUMNS} FROM recent_events \
             WHERE token_identity = $1 ORDER BY occurred_at ASC"
        ))
        .bind(token_identity)
        .fetch_all(pool)
        .await?
    };

    Ok(rows.into_iter().filter_map(|r| r.into_event()).collect())
}

/// Fetch corroborated relations for a token. A minimal read-only projection over
/// `recent_events` (relation + evidence). Returns distinct relations.
pub async fn fetch_relations(
    pool: &PgPool,
    token_identity: &str,
) -> Result<Vec<CandidateRelation>> {
    #[derive(sqlx::FromRow)]
    struct RelationRow {
        relation: String,
        related_identities: serde_json::Value,
        evidence_refs: serde_json::Value,
    }

    let rows: Vec<RelationRow> = sqlx::query_as(
        r#"
        SELECT DISTINCT ON (relation) relation::text AS relation,
               related_identities, evidence_refs
          FROM recent_events
         WHERE token_identity = $1
           AND relation IS NOT NULL
         ORDER BY relation, occurred_at DESC
        "#,
    )
    .bind(token_identity)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let relation = relation_from_str(&r.relation)?;
            let evidence_refs: Vec<String> =
                serde_json::from_value(r.evidence_refs).unwrap_or_default();
            let related_ids: Vec<super::recent::IdentityKey> =
                serde_json::from_value(r.related_identities).unwrap_or_default();
            let to_identity = related_ids.into_iter().next()?;
            Some(CandidateRelation {
                to_identity,
                relation,
                confidence: super::recent::RecentConfidence::Estimated,
                evidence_refs,
            })
        })
        .collect())
}

fn relation_from_str(s: &str) -> Option<super::recent::RecentRelation> {
    use super::recent::RecentRelation::*;
    Some(match s {
        "same_deployer" => SameDeployer,
        "same_authority" => SameAuthority,
        "same_fee_payer" => SameFeePayer,
        "same_funder" => SameFunder,
        "funded_by_known_deployer" => FundedByKnownDeployer,
        "same_social_account" => SameSocialAccount,
        "reused_social_link" => ReusedSocialLink,
        "official_ca_announcement" => OfficialCaAnnouncement,
        "cross_chain_deployment" => CrossChainDeployment,
        "derivative_of" => DerivativeOf,
        "suspected_copycat" => SuspectedCopycat,
        "liquidity_attention_rotated_to" => LiquidityAttentionRotatedTo,
        _ => return None,
    })
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
