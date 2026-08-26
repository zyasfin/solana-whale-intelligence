//! Health reporting: chain adapters, provider limits, Telegram, radar stages,
//! queue lag, database latency, and retention. Secrets always redacted.

#![allow(dead_code)]

use crate::models::ChainKind;
use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use std::time::Instant;

/// Health snapshot for one chain adapter.
#[derive(Clone, Debug)]
pub struct ChainHealth {
    pub chain: ChainKind,
    pub enabled: bool,
    pub error: Option<String>,
    pub validated_chain_id: Option<String>,
}

/// Collect adapter health without leaking secrets.
pub fn chain_health(
    solana_enabled: bool,
    robinhood_configured: bool,
    robinhood_validated: bool,
    robinhood_error: Option<&str>,
    robinhood_chain_id: Option<&str>,
) -> Vec<ChainHealth> {
    vec![
        ChainHealth {
            chain: ChainKind::Solana,
            enabled: solana_enabled,
            error: None,
            validated_chain_id: None,
        },
        ChainHealth {
            chain: ChainKind::Robinhood,
            enabled: robinhood_configured && robinhood_validated,
            error: robinhood_error.map(str::to_string),
            validated_chain_id: robinhood_chain_id.filter(|_| robinhood_validated).map(str::to_string),
        },
    ]
}

/// Radar stage counts from the database.
#[derive(Clone, Debug, Default)]
pub struct RadarStageCounts {
    pub funded: u32,
    pub preparation: u32,
    pub deployed: u32,
    pub dismissed: u32,
}

/// Query radar stage counts.
pub async fn radar_stage_counts(db: &PgPool) -> Result<RadarStageCounts> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT stage, COUNT(*) FROM funding_radar_cases GROUP BY stage",
    )
    .fetch_all(db)
    .await?;
    let mut counts = RadarStageCounts::default();
    for (stage, count) in rows {
        match stage.as_str() {
            "funded" => counts.funded = count.max(0) as u32,
            "preparation" => counts.preparation = count.max(0) as u32,
            "deployed" => counts.deployed = count.max(0) as u32,
            "dismissed" => counts.dismissed = count.max(0) as u32,
            _ => {}
        }
    }
    Ok(counts)
}

/// Telegram channel status counts.
#[derive(Clone, Debug, Default)]
pub struct TelegramHealth {
    pub allowlisted: u32,
    pub active: u32,
    pub flood_wait: u32,
    pub disabled: u32,
    pub messages_stored: u64,
}

/// Query Telegram health.
pub async fn telegram_health(db: &PgPool) -> Result<TelegramHealth> {
    let statuses: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, COUNT(*) FROM telegram_channels GROUP BY status",
    )
    .fetch_all(db)
    .await?;
    let mut health = TelegramHealth::default();
    for (status, count) in statuses {
        match status.as_str() {
            "active" => health.active = count.max(0) as u32,
            "flood_wait" => health.flood_wait = count.max(0) as u32,
            "disabled" | "not_found" | "auth_failed" => health.disabled += count.max(0) as u32,
            _ => {}
        }
    }
    let allowlisted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM telegram_channels WHERE allowlisted")
            .fetch_one(db)
            .await?;
    health.allowlisted = allowlisted.max(0) as u32;
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM telegram_messages")
        .fetch_one(db)
        .await?;
    health.messages_stored = messages.max(0) as u64;
    Ok(health)
}

/// Full health report (secrets redacted).
pub async fn report(
    db: &PgPool,
    chains: Vec<ChainHealth>,
    helius_enabled: usize,
    helius_total: usize,
    gmgn_enabled: usize,
    gmgn_total: usize,
) -> Result<serde_json::Value> {
    let start = Instant::now();
    let db_latency = match crate::db::latency_ms(db).await {
        Ok(ms) => ms,
        Err(err) => {
            return Ok(json!({
                "status": "degraded",
                "database": { "error": err.to_string() },
            }))
        }
    };

    let radar = radar_stage_counts(db).await.unwrap_or_default();
    let telegram = telegram_health(db).await.unwrap_or_default();
    let raw_events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM raw_events")
        .fetch_one(db)
        .await
        .unwrap_or(0);
    let open_cases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM funding_radar_cases WHERE stage NOT IN ('dismissed')",
    )
    .fetch_one(db)
    .await
    .unwrap_or(0);

    let chains_json: Vec<serde_json::Value> = chains
        .iter()
        .map(|c| {
            json!({
                "chain": c.chain.as_str(),
                "enabled": c.enabled,
                "error": c.error,
                "validated_chain_id": c.validated_chain_id,
            })
        })
        .collect();

    let report = json!({
        "status": "ok",
        "generated_at": Utc::now().to_rfc3339(),
        "database": {
            "latency_ms": db_latency,
        },
        "chains": chains_json,
        "helius": {
            // DB-backed key pool counts (admin API panel); never the keys.
            "keys_enabled": helius_enabled,
            "keys_total": helius_total,
        },
        "gmgn": {
            "configured": gmgn_enabled > 0,
            "keys_enabled": gmgn_enabled,
            "keys_total": gmgn_total,
        },
        "telegram": {
            "allowlisted": telegram.allowlisted,
            "active": telegram.active,
            "flood_wait": telegram.flood_wait,
            "disabled": telegram.disabled,
            "messages_stored": telegram.messages_stored,
        },
        "funding_radar": {
            "stages": {
                "funded": radar.funded,
                "preparation": radar.preparation,
                "deployed": radar.deployed,
                "dismissed": radar.dismissed,
            },
            "open_cases": open_cases,
        },
        "storage": {
            "raw_events": raw_events,
        },
        "report_duration_ms": start.elapsed().as_secs_f64() * 1000.0,
    });
    Ok(report)
}

/// Redact a secret for logging: show only length and a short prefix.
pub fn redact(secret: &str) -> String {
    if secret.len() <= 4 {
        return "***".to_string();
    }
    format!("{}***({} chars)", &secret[..2], secret.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_health_reports_robinhood_disabled_with_error() {
        let health = chain_health(
            true,
            false,
            false,
            Some("missing HELIUS_KEY_1 or robinhood_chain_id"),
            None,
        );
        assert_eq!(health.len(), 2);
        assert!(health[0].enabled);
        assert!(!health[1].enabled);
        assert_eq!(health[1].error.as_deref(), Some("missing HELIUS_KEY_1 or robinhood_chain_id"));
    }

    #[test]
    fn chain_health_reports_validated_chain_id() {
        let health = chain_health(true, true, true, None, Some("1a2b3c"));
        assert!(health[1].enabled);
        assert_eq!(health[1].validated_chain_id.as_deref(), Some("1a2b3c"));
    }

    #[test]
    fn redact_never_leaks_secret() {
        assert_eq!(redact("ab"), "***");
        let redacted = redact("super-secret-key-material");
        assert!(redacted.starts_with("su***"));
        assert!(!redacted.contains("secret-key"));
        assert!(redacted.ends_with("chars)"));
    }

    #[test]
    fn redact_handles_empty() {
        assert_eq!(redact(""), "***");
        assert_eq!(redact("abcd"), "***");
    }
}
