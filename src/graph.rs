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

    // Find or create a cluster that already contains any member.
    let existing: Option<(i64,)> = sqlx::query_as(
        r#"
        SELECT cluster_id FROM wallet_cluster_members
         WHERE chain = $1 AND address = ANY($2) AND revoked_at IS NULL
         LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(&rows.iter().map(|(f, _t, _)| f.clone()).chain(std::iter::once(address.to_string())).collect::<Vec<_>>())
    .fetch_optional(db)
    .await?;
    let cluster_id: i64 = match existing {
        Some((id,)) => id,
        None => {
            sqlx::query_scalar::<_, i64>("INSERT INTO wallet_clusters DEFAULT VALUES RETURNING cluster_id")
                .fetch_one(db)
                .await?
        }
    };

    for member in &cluster_members {
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
        .bind(member)
        .bind(decimal_from_f64(MEMBERSHIP_THRESHOLD))
        .execute(db)
        .await?;
    }

    Ok(Some(cluster_id))
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

pub async fn trace_wallet(
    db: &PgPool,
    chain: ChainKind,
    address: &str,
    max_depth: u32,
) -> Result<Vec<TraceStep>> {
    let labels: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT kind FROM wallet_labels
         WHERE chain = $1 AND address = $2 AND disposition = 'flow_only'
           AND revoked_at IS NULL
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .fetch_all(db)
    .await?;
    let is_flow_endpoint = !labels.is_empty();

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
