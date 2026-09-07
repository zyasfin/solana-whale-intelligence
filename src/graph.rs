//! Soft wallet clustering graph.
//!
//! One transfer NEVER merges wallets. Cluster membership requires accumulated
//! confidence >= 0.70 across independent evidence kinds. Historical edges are
//! never mutated by later revocations.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::models::{ChainKind, NormalizedTransfer};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

/// Convert an f64 confidence into a Decimal via string parsing.
fn decimal_from_f64(value: f64) -> Decimal {
    Decimal::from_str_exact(&format!("{value}")).unwrap_or(Decimal::ZERO)
}


/// Edge confidence component weights.
pub const WEIGHT_FUNDING: f64 = 0.35;
pub const WEIGHT_TOKEN_ACCOUNT: f64 = 0.25;
pub const WEIGHT_REPEATED_FUNDING: f64 = 0.15;
pub const WEIGHT_CLOSE_TIME_ACTIVITY: f64 = 0.15;
pub const WEIGHT_TRADE_STYLE: f64 = 0.10;

/// Membership promotion threshold.
pub const MEMBERSHIP_THRESHOLD: f64 = 0.70;

/// Components of edge confidence between two wallets.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EdgeComponents {
    pub funding: bool,
    pub token_account_creation: bool,
    pub repeated_funding: bool,
    pub close_time_same_token: bool,
    pub trade_style_match: bool,
}

impl EdgeComponents {
    /// Compute the weighted confidence from observed components.
    pub fn confidence(&self) -> f64 {
        let mut score = 0.0;
        if self.funding {
            score += WEIGHT_FUNDING;
        }
        if self.token_account_creation {
            score += WEIGHT_TOKEN_ACCOUNT;
        }
        if self.repeated_funding {
            score += WEIGHT_REPEATED_FUNDING;
        }
        if self.close_time_same_token {
            score += WEIGHT_CLOSE_TIME_ACTIVITY;
        }
        if self.trade_style_match {
            score += WEIGHT_TRADE_STYLE;
        }
        score.min(1.0)
    }
}

/// Score edge confidence from observed evidence.
pub fn score_edge_confidence(components: &EdgeComponents) -> f64 {
    components.confidence()
}

/// Store one funding edge (never deleted; history preserved).
pub async fn update_funding_edges(
    db: &PgPool,
    transfer: &NormalizedTransfer,
) -> Result<()> {
    let components = EdgeComponents {
        funding: true,
        ..Default::default()
    };
    let confidence = score_edge_confidence(&components);
    sqlx::query(
        r#"
        INSERT INTO funding_edges
            (chain, from_address, to_address, signature, edge_kind, raw_amount,
             block_time, confidence, evidence, promoted)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (chain, from_address, to_address, signature, edge_kind) DO NOTHING
        "#,
    )
    .bind(transfer.chain.as_str())
    .bind(&transfer.from_address)
    .bind(&transfer.to_address)
    .bind(&transfer.signature)
    .bind(if transfer.asset_kind.as_str() == "native" {
        "funding"
    } else {
        "token_funding"
    })
    .bind(&transfer.raw_amount)
    .bind(transfer.block_time)
    .bind(decimal_from_f64(confidence))
    .bind(serde_json::json!({
        "components": {
            "funding": components.funding,
            "token_account_creation": components.token_account_creation,
            "repeated_funding": components.repeated_funding,
            "close_time_same_token": components.close_time_same_token,
            "trade_style_match": components.trade_style_match,
        },
        "asset_kind": transfer.asset_kind.as_str(),
        "mint": transfer.mint,
    }))
    .bind(confidence >= MEMBERSHIP_THRESHOLD)
    .execute(db)
    .await?;
    Ok(())
}

/// Record additional evidence for an existing wallet pair (token account
/// creation, repeated funding, close-time activity, trade-style match).
pub async fn record_edge_evidence(
    db: &PgPool,
    chain: ChainKind,
    from_address: &str,
    to_address: &str,
    components: &EdgeComponents,
    evidence: serde_json::Value,
) -> Result<()> {
    let confidence = score_edge_confidence(components);
    sqlx::query(
        r#"
        INSERT INTO funding_edges
            (chain, from_address, to_address, signature, edge_kind, raw_amount,
             block_time, confidence, evidence, promoted)
        VALUES ($1, $2, $3, $4, 'evidence', '0', NULL, $5, $6, $7)
        ON CONFLICT (chain, from_address, to_address, signature, edge_kind) DO NOTHING
        "#,
    )
    .bind(chain.as_str())
    .bind(from_address)
    .bind(to_address)
    .bind(format!("evidence-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0)))
    .bind(decimal_from_f64(confidence))
    .bind(evidence)
    .bind(confidence >= MEMBERSHIP_THRESHOLD)
    .execute(db)
    .await?;
    Ok(())
}

/// Rebuild cluster membership for one wallet.
///
/// Promotion requires pair confidence >= 0.70. Existing memberships are
/// revoked (not deleted) when their confidence falls below the threshold
/// through missing evidence; historical edges remain untouched.
///
/// REV-072-F06 (HIGH): the component is MERGED onto one canonical cluster.
///
/// This used to take the first existing cluster containing any member (`LIMIT 1`)
/// and write the whole component into it, leaving every OTHER cluster those members
/// belonged to active. The schema permitted it — uniqueness is only
/// `(cluster_id, chain, address)` — so one wallet could hold several active
/// memberships at once. `evaluate_token_signals` counts `COUNT(DISTINCT cluster_id)`
/// as a hard entry gate documented as "two INDEPENDENT eligible clusters", so a
/// single wallet in two active clusters passed that gate by itself and a rejected
/// token became an accepted signal.
///
/// If a member is active in clusters A and B then A and B are one component, so the
/// correct state is one cluster, not "keep A and forget B". Every overlapping
/// cluster is therefore folded into the smallest cluster id and its superseded
/// memberships are revoked — the same retirement every other path in this schema
/// uses. Migration 1032 performs the identical merge for existing data and then
/// installs a partial unique index on `(chain, address) WHERE revoked_at IS NULL`,
/// so the invariant survives a caller that forgets it.
///
/// The whole rebuild runs in ONE transaction: a merge that committed halfway would
/// leave a wallet in two clusters, which is exactly the state being eliminated.
pub async fn rebuild_cluster_for(
    db: &PgPool,
    chain: ChainKind,
    address: &str,
    _now: DateTime<Utc>,
) -> Result<Option<i64>> {
    // Strongest incoming/outgoing edges for this wallet.
    let rows: Vec<(String, String, rust_decimal::Decimal)> = sqlx::query_as(
        r#"
        SELECT from_address, to_address, confidence
          FROM funding_edges
         WHERE chain = $1 AND (from_address = $2 OR to_address = $2)
           AND confidence >= $3
         ORDER BY confidence DESC
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .bind(decimal_from_f64(MEMBERSHIP_THRESHOLD))
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    // Collect the connected component above threshold.
    let mut cluster_members: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    cluster_members.insert(address.to_string());
    for (from, to, _) in &rows {
        cluster_members.insert(from.clone());
        cluster_members.insert(to.clone());
    }
    let members: Vec<String> = cluster_members.iter().cloned().collect();

    let mut tx = db.begin().await?;

    // EVERY cluster any member is currently active in — not just the first one.
    // Taking one and ignoring the rest is what left a wallet counted twice.
    let existing: Vec<i64> = sqlx::query_scalar(
        r#"
        SELECT DISTINCT cluster_id FROM wallet_cluster_members
         WHERE chain = $1 AND address = ANY($2) AND revoked_at IS NULL
         ORDER BY cluster_id
        "#,
    )
    .bind(chain.as_str())
    .bind(&members)
    .fetch_all(&mut *tx)
    .await?;

    // The smallest existing id is canonical, so repeated rebuilds converge on one
    // cluster instead of ping-ponging between equally valid choices.
    let cluster_id: i64 = match existing.first() {
        Some(id) => *id,
        None => {
            sqlx::query_scalar::<_, i64>("INSERT INTO wallet_clusters DEFAULT VALUES RETURNING cluster_id")
                .fetch_one(&mut *tx)
                .await?
        }
    };

    // Fold the superseded clusters in: their members join the canonical cluster and
    // their old memberships are revoked, so the component is one cluster afterwards.
    // Members of a superseded cluster that are NOT in this component still move —
    // they were transitively connected through the shared wallet, which is what made
    // the clusters overlap in the first place.
    for superseded in existing.iter().copied().filter(|id| *id != cluster_id) {
        let moved: Vec<String> = sqlx::query_scalar(
            "SELECT address FROM wallet_cluster_members \
              WHERE cluster_id = $1 AND chain = $2 AND revoked_at IS NULL",
        )
        .bind(superseded)
        .bind(chain.as_str())
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE wallet_cluster_members SET revoked_at = now() \
              WHERE cluster_id = $1 AND chain = $2 AND revoked_at IS NULL",
        )
        .bind(superseded)
        .bind(chain.as_str())
        .execute(&mut *tx)
        .await?;
        for member in moved {
            upsert_membership(&mut tx, cluster_id, chain, &member).await?;
        }
    }

    for member in &members {
        upsert_membership(&mut tx, cluster_id, chain, member).await?;
    }

    tx.commit().await?;
    Ok(Some(cluster_id))
}

/// Activate one membership in the canonical cluster.
///
/// Revoking the superseded row first and re-inserting here is what keeps at most one
/// ACTIVE membership per `(chain, address)` — the invariant migration 1032 enforces
/// with a partial unique index, so a violation is a constraint error rather than a
/// silently double-counted cluster.
async fn upsert_membership(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: i64,
    chain: ChainKind,
    address: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO wallet_cluster_members (cluster_id, chain, address, membership_kind, confidence)
        VALUES ($1, $2, $3, 'soft', $4)
        ON CONFLICT (cluster_id, chain, address) DO UPDATE
            SET confidence = EXCLUDED.confidence,
                revoked_at = NULL
        "#,
    )
    .bind(cluster_id)
    .bind(chain.as_str())
    .bind(address)
    .bind(decimal_from_f64(MEMBERSHIP_THRESHOLD))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Trace wallet lineage: funding sources and destinations, stopping at
/// flow-only exchange/bridge endpoints.
pub struct TraceStep {
    pub address: String,
    pub direction: &'static str,
    pub via_signature: String,
    pub block_time: Option<DateTime<Utc>>,
    pub endpoint: bool,
}

/// Trace a wallet's funding lineage, stopping at flow-only endpoints.
///
/// REV-058-F02 (HIGH): the flow-only policy lookup is workspace-scoped AND respects
/// expiry.
///
/// Two defects in one query. It accepted no workspace, so a `flow_only` label created
/// by another tenant silently truncated THIS tenant's traversal — a cross-workspace
/// policy leak with no visible cause. And it ignored `expires_at`, so a label that had
/// already expired kept suppressing traversal forever; every other label consumer in
/// this crate checks `(expires_at IS NULL OR expires_at > now())`, and this one was the
/// outlier.
///
/// `workspace_id` is required rather than optional: an unscoped trace is precisely the
/// bug, and a defaulted one would hide it again.
///
/// REV-064-F06: scoping and expiry were right, the QUESTION was wrong. Asking
/// "does any active row say `flow_only`?" bypasses the disposition authority:
/// a manual `watch` beside an automatic `flow_only` resolves to `watch`, yet the
/// existence check still truncated the trace. Traversal now reads the effective
/// disposition (`db::active_disposition`) and applies the shared
/// `db::disposition_stops_traversal` rule, so no consumer re-implements policy and
/// an unknown disposition surfaces as an error instead of being ranked as "walk".
pub async fn trace_wallet(
    db: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    address: &str,
    max_depth: u32,
) -> Result<Vec<TraceStep>> {
    let is_flow_endpoint =
        match crate::db::active_disposition(db, workspace_id, chain.as_str(), address).await? {
            Some(disposition) => crate::db::disposition_stops_traversal(&disposition)?,
            None => false,
        };

    let mut steps = Vec::new();
    if is_flow_endpoint {
        // Still record immediate edges (provenance) but stop traversal.
        let rows: Vec<(String, String, String, Option<DateTime<Utc>>)> = sqlx::query_as(
            r#"
            SELECT from_address, to_address, signature, block_time
              FROM funding_edges
             WHERE chain = $1 AND (from_address = $2 OR to_address = $2)
             ORDER BY block_time NULLS LAST
             LIMIT 50
            "#,
        )
        .bind(chain.as_str())
        .bind(address)
        .fetch_all(db)
        .await?;
        for (from, to, signature, block_time) in rows {
            let direction = if from == address { "outgoing" } else { "incoming" };
            steps.push(TraceStep {
                address: if from == address { to } else { from },
                direction,
                via_signature: signature,
                block_time,
                endpoint: true,
            });
        }
        return Ok(steps);
    }

    // Normal traversal bounded by depth.
    let rows: Vec<(String, String, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT from_address, to_address, signature, block_time
          FROM funding_edges
         WHERE chain = $1 AND (from_address = $2 OR to_address = $2)
         ORDER BY block_time NULLS LAST
         LIMIT $3
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .bind((max_depth as i64) * 10)
    .fetch_all(db)
    .await?;
    for (from, to, signature, block_time) in rows {
        let direction = if from == address { "outgoing" } else { "incoming" };
        steps.push(TraceStep {
            address: if from == address { to } else { from },
            direction,
            via_signature: signature,
            block_time,
            endpoint: false,
        });
    }
    Ok(steps)
}

/// Detect near-equal transfer cycles within 24 hours as wash candidates.
pub async fn detect_wash_candidates(
    db: &PgPool,
    chain: ChainKind,
    now: DateTime<Utc>,
) -> Result<Vec<(String, String, String)>> {
    let rows: Vec<(String, String, String, String, DateTime<Utc>)> = sqlx::query_as(
        r#"
        SELECT a.from_address, a.to_address, a.signature, b.signature, a.block_time
          FROM transfers a
          JOIN transfers b
            ON b.chain = a.chain
           AND b.from_address = a.to_address
           AND b.to_address = a.from_address
           AND b.asset_kind = a.asset_kind
           AND b.raw_amount = a.raw_amount
           AND b.block_time BETWEEN a.block_time AND a.block_time + interval '24 hours'
         WHERE a.chain = $1
           AND a.block_time BETWEEN $2 - interval '24 hours' AND $2
           AND a.signature <> b.signature
         LIMIT 100
        "#,
    )
    .bind(chain.as_str())
    .bind(now)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(from, to, sig_a, sig_b, _)| (from, to, format!("{sig_a}+{sig_b}")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_transfer_does_not_reach_membership() {
        let components = EdgeComponents {
            funding: true,
            ..Default::default()
        };
        assert!(components.confidence() < MEMBERSHIP_THRESHOLD);
    }

    #[test]
    fn token_account_plus_funding_promotes() {
        // Plan: token-account creation plus repeated funding reaches >= 0.70.
        let components = EdgeComponents {
            funding: true,
            token_account_creation: true,
            repeated_funding: true,
            ..Default::default()
        };
        assert!(components.confidence() >= MEMBERSHIP_THRESHOLD);

        // Two components alone stay below membership.
        let partial = EdgeComponents {
            funding: true,
            token_account_creation: true,
            ..Default::default()
        };
        assert!(partial.confidence() < MEMBERSHIP_THRESHOLD);
    }

    #[test]
    fn all_components_reach_one() {
        let components = EdgeComponents {
            funding: true,
            token_account_creation: true,
            repeated_funding: true,
            close_time_same_token: true,
            trade_style_match: true,
        };
        assert_eq!(components.confidence(), 1.0);
    }

    #[test]
    fn edge_confidence_matches_weights() {
        let funding_only = EdgeComponents {
            funding: true,
            ..Default::default()
        };
        assert!((funding_only.confidence() - 0.35).abs() < 1e-9);

        let token_only = EdgeComponents {
            token_account_creation: true,
            ..Default::default()
        };
        assert!((token_only.confidence() - 0.25).abs() < 1e-9);

        let style_only = EdgeComponents {
            trade_style_match: true,
            ..Default::default()
        };
        assert!((style_only.confidence() - 0.10).abs() < 1e-9);
    }
}
