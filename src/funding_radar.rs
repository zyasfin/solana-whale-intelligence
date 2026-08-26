//! Funding radar: detect large funding into fresh wallets and track possible
//! project preparation through funded → preparation → deployed stages.
//!
//! Two-stage alerts: `funding_watch` after the first qualifying funding
//! (processed commitment allowed), `preparation_alert` at confidence >= 70
//! with non-infrastructure evidence (confirmed required).

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::config::FundingRadarConfig;
use crate::models::{
    ChainKind, Commitment, FundingEvent, FundingRadarCase, FundingRadarDecision, RadarAlert,
    RadarStage, WalletLabelKind,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

/// Persist one funding observation (raw-first, idempotent).
pub async fn store_funding_observation(db: &PgPool, event: &FundingEvent) -> Result<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO funding_observations
            (chain, signature, slot, observed_at, from_address, to_address, asset_kind,
             mint, raw_amount, amount_usd, native_usd_price, commitment,
             recipient_age_seconds, source_disposition, raw)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        ON CONFLICT (chain, signature, from_address, to_address, asset_kind, mint)
        DO NOTHING
        "#,
    )
    .bind(event.chain.as_str())
    .bind(&event.signature)
    .bind(event.slot.map(|v| v as i64))
    .bind(event.observed_at)
    .bind(&event.from_address)
    .bind(&event.to_address)
    .bind(event.asset_kind.as_str())
    .bind(&event.mint)
    .bind(&event.raw_amount)
    .bind(event.amount_usd)
    .bind(event.native_usd_price)
    .bind(event.commitment.as_str())
    .bind(event.recipient_age_seconds)
    .bind(event.source_kind.map(|k| k.as_str()).unwrap_or("unknown"))
    .bind(&event.raw)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Whether an event qualifies as large funding.
///
/// Native asset: must exceed BOTH the SOL floor and the USD floor, OR exceed
/// the rolling percentile with the percentile floor. Token funding requires a
/// known event-time USD price and the USD threshold.
pub fn qualifies_as_large_funding(
    event: &FundingEvent,
    config: &FundingRadarConfig,
    percentile_threshold_sol: Option<Decimal>,
) -> bool {
    match event.asset_kind {
        crate::models::AssetKind::Native => {
            let sol_amount = event.native_amount;
            let min_sol = Decimal::from(config.large_funding_min_sol);
            let usd_ok = event
                .amount_usd
                .map(|usd| usd >= Decimal::from(config.large_funding_min_usd))
                .unwrap_or(false);
            let absolute_ok = sol_amount >= min_sol && usd_ok;
            let percentile_ok = percentile_threshold_sol
                .map(|threshold| {
                    sol_amount >= threshold && sol_amount >= Decimal::from(config.percentile_floor_sol)
                })
                .unwrap_or(false);
            absolute_ok || percentile_ok
        }
        crate::models::AssetKind::SplToken | crate::models::AssetKind::Erc20 => {
            // Token funding requires a known event-time USD price.
            event
                .amount_usd
                .map(|usd| usd >= Decimal::from(config.large_funding_min_usd))
                .unwrap_or(false)
        }
    }
}

/// Whether the recipient is fresh enough for radar tracking.
pub fn recipient_is_fresh(event: &FundingEvent, config: &FundingRadarConfig) -> bool {
    match event.recipient_age_seconds {
        Some(age) => age >= 0 && age <= (config.recipient_max_age_days as i64) * 86_400,
        None => false,
    }
}

/// Ingest one funding event: store the raw observation, then create or update
/// a radar case when thresholds and freshness are satisfied.
///
/// Returns the radar case when one exists after ingestion.
pub async fn ingest_funding_event(
    db: &PgPool,
    event: FundingEvent,
    config: &FundingRadarConfig,
    percentile_threshold_sol: Option<Decimal>,
) -> Result<Option<FundingRadarCase>> {
    // Raw observation always stored, even when it does not qualify.
    store_funding_observation(db, &event).await?;

    // Only confirmed events open radar cases (processed may reorg).
    if event.commitment < Commitment::Confirmed {
        return fetch_case(db, event.chain, &event.to_address).await;
    }
    if !recipient_is_fresh(&event, config) {
        return fetch_case(db, event.chain, &event.to_address).await;
    }
    if !qualifies_as_large_funding(&event, config, percentile_threshold_sol) {
        return fetch_case(db, event.chain, &event.to_address).await;
    }

    // Ensure the recipient wallet row exists (FK).
    crate::db::upsert_wallet(db, event.chain.as_str(), &event.to_address, event.observed_at, "funding_radar").await?;
    crate::db::upsert_wallet(db, event.chain.as_str(), &event.from_address, event.observed_at, "funding_radar").await?;

    let existing = fetch_case(db, event.chain, &event.to_address).await?;
    let window_ends = event.observed_at + chrono::Duration::days(config.preparation_window_days as i64);

    match existing {
        Some(case) if case.stage != RadarStage::Dismissed => {
            // Second independent funding evidence for an existing case.
            record_case_event(
                db,
                case.id,
                "additional_funding",
                event.observed_at,
                &serde_json::json!({
                    "signature": event.signature,
                    "from_address": event.from_address,
                    "source_kind": event.source_kind.map(|k| k.as_str()),
                    "amount_usd": event.amount_usd,
                    "infrastructure": event.source_is_infrastructure(),
                }),
            )
            .await?;
            let fanout = count_fresh_children(db, event.chain, &event.to_address, config).await?;
            update_case_fanout(db, case.id, fanout).await?;
            fetch_case(db, event.chain, &event.to_address).await
        }
        _ => {
            // Create a new funded case.
            let infrastructure = event.source_is_infrastructure();
            let case_id: i64 = sqlx::query_scalar(
                r#"
                INSERT INTO funding_radar_cases
                    (chain, recipient, first_funded_at, first_funding_usd, first_funding_native,
                     source_address, source_kind, fanout_count, deploy_window_ends_at,
                     stage, confidence, evidence)
                VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, 'funded', $9, $10)
                RETURNING id
                "#,
            )
            .bind(event.chain.as_str())
            .bind(&event.to_address)
            .bind(event.observed_at)
            .bind(event.amount_usd)
            .bind(event.native_amount)
            .bind(&event.from_address)
            .bind(event.source_kind.map(|k| k.as_str()))
            .bind(window_ends)
            .bind(if infrastructure { 40 } else { 55 })
            .bind(serde_json::json!({
                "first_funding": {
                    "signature": event.signature,
                    "amount_usd": event.amount_usd,
                    "native_amount": event.native_amount,
                    "infrastructure_source": infrastructure,
                }
            }))
            .fetch_one(db)
            .await?;
            record_case_event(
                db,
                case_id,
                "first_funding",
                event.observed_at,
                &serde_json::json!({
                    "signature": event.signature,
                    "from_address": event.from_address,
                    "source_kind": event.source_kind.map(|k| k.as_str()),
                    "amount_usd": event.amount_usd,
                    "infrastructure": infrastructure,
                }),
            )
            .await?;
            fetch_case(db, event.chain, &event.to_address).await
        }
    }
}

async fn record_case_event(
    db: &PgPool,
    case_id: i64,
    event_kind: &str,
    observed_at: DateTime<Utc>,
    evidence: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO funding_radar_events (case_id, event_kind, observed_at, evidence)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(case_id)
    .bind(event_kind)
    .bind(observed_at)
    .bind(evidence)
    .execute(db)
    .await?;
    Ok(())
}

async fn update_case_fanout(db: &PgPool, case_id: i64, fanout: u32) -> Result<()> {
    sqlx::query("UPDATE funding_radar_cases SET fanout_count = $2, updated_at = now() WHERE id = $1")
        .bind(case_id)
        .bind(fanout as i32)
        .execute(db)
        .await?;
    Ok(())
}

/// Count fresh child wallets funded by this recipient (fan-out detection).
async fn count_fresh_children(
    db: &PgPool,
    chain: ChainKind,
    recipient: &str,
    config: &FundingRadarConfig,
) -> Result<u32> {
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT fo.to_address)
          FROM funding_observations fo
          JOIN wallets w ON w.chain = fo.chain AND w.address = fo.to_address
         WHERE fo.chain = $1
           AND fo.from_address = $2
           AND EXTRACT(EPOCH FROM (fo.observed_at - w.first_seen)) <= $3
        "#,
    )
    .bind(chain.as_str())
    .bind(recipient)
    .bind((config.recipient_max_age_days as i64) * 86_400)
    .fetch_one(db)
    .await?;
    Ok(count.max(0) as u32)
}

/// Fetch the current radar case for a recipient.
pub async fn fetch_case(db: &PgPool, chain: ChainKind, recipient: &str) -> Result<Option<FundingRadarCase>> {
    let row = sqlx::query_as::<
        _,
        (
            i64,
            String,
            String,
            DateTime<Utc>,
            Option<Decimal>,
            Decimal,
            String,
            Option<String>,
            i32,
            DateTime<Utc>,
            String,
            i32,
            serde_json::Value,
            DateTime<Utc>,
        ),
    >(
        r#"
        SELECT id, chain, recipient, first_funded_at, first_funding_usd, first_funding_native,
               source_address, source_kind, fanout_count, deploy_window_ends_at, stage,
               confidence, evidence, updated_at
          FROM funding_radar_cases
         WHERE chain = $1 AND recipient = $2
         ORDER BY id DESC
         LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(recipient)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| FundingRadarCase {
        id: row.0,
        chain: ChainKind::parse(&row.1).unwrap_or(chain),
        recipient: row.2,
        first_funded_at: row.3,
        first_funding_usd: row.4,
        first_funding_native: row.5,
        source_address: row.6,
        source_kind: row.7.as_deref().and_then(WalletLabelKind::parse),
        fanout_count: row.8.max(0) as u32,
        deploy_window_ends_at: row.9,
        stage: RadarStage::parse(&row.10).unwrap_or(RadarStage::Funded),
        confidence: row.11.max(0) as u32,
        evidence: row.12,
        updated_at: row.13,
    }))
}

/// Evaluate a radar case: promotion, dismissal, and alert emission.
///
/// - `preparation` requires >= 2 independent evidence items within the window
///   AND non-infrastructure evidence AND confirmed commitment.
/// - `deployed` requires a confirmed mint/pool/deployment signature.
/// - Expired windows dismiss with history retained.
/// - A `processed`-only event never promotes a case.
pub async fn evaluate_radar_case(
    db: &PgPool,
    chain: ChainKind,
    recipient: &str,
    now: DateTime<Utc>,
    config: &FundingRadarConfig,
) -> Result<FundingRadarDecision> {
    let Some(case) = fetch_case(db, chain, recipient).await? else {
        return Ok(FundingRadarDecision {
            case_id: 0,
            chain,
            recipient: recipient.to_string(),
            stage: RadarStage::Funded,
            previous_stage: None,
            confidence: 0,
            alert: None,
            reason: "no case".to_string(),
            evidence_count: 0,
        });
    };
    let previous_stage = Some(case.stage);

    // Dismissed and deployed cases are terminal for evaluation.
    if case.stage == RadarStage::Dismissed || case.stage == RadarStage::Deployed {
        return Ok(FundingRadarDecision {
            case_id: case.id,
            chain,
            recipient: recipient.to_string(),
            stage: case.stage,
            previous_stage,
            confidence: case.confidence,
            alert: None,
            reason: format!("case already {}", case.stage.as_str()),
            evidence_count: count_case_events(db, case.id).await?,
        });
    }

    // Expired window: dismiss with reason; history retained.
    if now > case.deploy_window_ends_at && case.stage == RadarStage::Funded {
        let reason = "expired window without preparation evidence";
        dismiss_case(db, case.id, reason).await?;
        return Ok(FundingRadarDecision {
            case_id: case.id,
            chain,
            recipient: recipient.to_string(),
            stage: RadarStage::Dismissed,
            previous_stage,
            confidence: case.confidence,
            alert: None,
            reason: reason.to_string(),
            evidence_count: count_case_events(db, case.id).await?,
        });
    }

    // Count independent evidence kinds within the window.
    let evidence_rows: Vec<(String, DateTime<Utc>, serde_json::Value)> = sqlx::query_as(
        r#"
        SELECT event_kind, observed_at, evidence
          FROM funding_radar_events
         WHERE case_id = $1 AND observed_at <= $2
        "#,
    )
    .bind(case.id)
    .bind(now)
    .fetch_all(db)
    .await?;

    let mut distinct_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut non_infrastructure = false;
    for (kind, _at, evidence) in &evidence_rows {
        distinct_kinds.insert(kind.clone());
        let infra = evidence
            .get("infrastructure")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !infra {
            non_infrastructure = true;
        }
    }

    // Deployment evidence promotes to deployed.
    if distinct_kinds.contains("deployment") {
        update_stage(db, case.id, RadarStage::Deployed, 85, None).await?;
        return Ok(FundingRadarDecision {
            case_id: case.id,
            chain,
            recipient: recipient.to_string(),
            stage: RadarStage::Deployed,
            previous_stage,
            confidence: 85,
            alert: None,
            reason: "confirmed deployment linked".to_string(),
            evidence_count: distinct_kinds.len() as u32,
        });
    }

    // Preparation promotion: >= 2 independent evidence kinds, non-infrastructure.
    let required = config.preparation_evidence_required as usize;
    if distinct_kinds.len() >= required && non_infrastructure && case.stage == RadarStage::Funded {
        let confidence = preparation_confidence(&case, config, &distinct_kinds);
        update_stage(db, case.id, RadarStage::Preparation, confidence, None).await?;
        let updated = fetch_case(db, chain, recipient).await?.unwrap_or(case);
        let alert = if confidence >= config.preparation_alert_confidence {
            Some(RadarAlert::preparation(
                &updated,
                serde_json::json!({ "evidence_kinds": distinct_kinds.iter().collect::<Vec<_>>() }),
            ))
        } else {
            None
        };
        return Ok(FundingRadarDecision {
            case_id: updated.id,
            chain,
            recipient: recipient.to_string(),
            stage: RadarStage::Preparation,
            previous_stage,
            confidence,
            alert,
            reason: "possible project preparation".to_string(),
            evidence_count: distinct_kinds.len() as u32,
        });
    }

    // Still funded: report current state without promotion.
    let _fanout_ok = case.fanout_count >= config.fanout_threshold;
    Ok(FundingRadarDecision {
        case_id: case.id,
        chain,
        recipient: recipient.to_string(),
        stage: case.stage,
        previous_stage,
        confidence: case.confidence,
        alert: None,
        reason: format!(
            "awaiting more evidence ({}/{} kinds, fanout {}/{})",
            distinct_kinds.len(),
            required,
            case.fanout_count,
            config.fanout_threshold
        ),
        evidence_count: distinct_kinds.len() as u32,
    })
}

fn preparation_confidence(
    case: &FundingRadarCase,
    config: &FundingRadarConfig,
    kinds: &std::collections::HashSet<String>,
) -> u32 {
    let mut confidence = 55u32;
    if kinds.contains("fanout") {
        confidence += 15;
    }
    if kinds.contains("token_creation") || kinds.contains("account_creation") {
        confidence += 10;
    }
    if kinds.contains("launchpad_interaction") || kinds.contains("dex_interaction") {
        confidence += 10;
    }
    if kinds.contains("telegram_mention") || kinds.contains("gmgn_mention") {
        confidence += 5;
    }
    if case.fanout_count >= config.fanout_threshold {
        confidence += 5;
    }
    confidence.min(95)
}

async fn count_case_events(db: &PgPool, case_id: i64) -> Result<u32> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM funding_radar_events WHERE case_id = $1")
        .bind(case_id)
        .fetch_one(db)
        .await?;
    Ok(count.max(0) as u32)
}

async fn update_stage(
    db: &PgPool,
    case_id: i64,
    stage: RadarStage,
    confidence: u32,
    dismissed_reason: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE funding_radar_cases
           SET stage = $2, confidence = $3, updated_at = now(),
               dismissed_reason = COALESCE($4, dismissed_reason)
         WHERE id = $1
        "#,
    )
    .bind(case_id)
    .bind(stage.as_str())
    .bind(confidence as i32)
    .bind(dismissed_reason)
    .execute(db)
    .await?;
    Ok(())
}

async fn dismiss_case(db: &PgPool, case_id: i64, reason: &str) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE funding_radar_cases
           SET stage = 'dismissed', dismissed_reason = $2, updated_at = now()
         WHERE id = $1
        "#,
    )
    .bind(case_id)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// Record an external evidence item for a case (token creation, launchpad
/// interaction, telegram/GMGN mention, fanout, deployment).
pub async fn record_case_evidence(
    db: &PgPool,
    case_id: i64,
    event_kind: &str,
    observed_at: DateTime<Utc>,
    evidence: serde_json::Value,
) -> Result<()> {
    record_case_event(db, case_id, event_kind, observed_at, &evidence).await
}

/// Link a confirmed deployment signature to a radar case.
pub async fn link_deployment(
    db: &PgPool,
    case_id: i64,
    signature: &str,
    mint: &str,
    observed_at: DateTime<Utc>,
) -> Result<()> {
    record_case_event(
        db,
        case_id,
        "deployment",
        observed_at,
        &serde_json::json!({ "signature": signature, "mint": mint, "confirmed": true }),
    )
    .await
}

/// Compute the rolling 99th percentile threshold from stored observations.
pub async fn percentile_threshold(
    db: &PgPool,
    chain: ChainKind,
    percentile: f64,
) -> Result<Option<Decimal>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        r#"
        SELECT percentile_disc($2) WITHIN GROUP (ORDER BY amount)
          FROM (
            SELECT (raw_amount::numeric / 1000000000) AS amount
              FROM funding_observations
             WHERE chain = $1 AND asset_kind = 'native'
          ) sub
        "#,
    )
    .bind(chain.as_str())
    .bind(percentile)
    .fetch_optional(db)
    .await?;
    Ok(row
        .and_then(|(v,)| v)
        .and_then(|v| Decimal::from_str_exact(&v).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AssetKind;

    fn config() -> FundingRadarConfig {
        FundingRadarConfig {
            large_funding_min_sol: 100,
            large_funding_min_usd: 10_000,
            percentile_floor_sol: 10,
            percentile: 0.99,
            recipient_max_age_days: 7,
            preparation_window_days: 7,
            fanout_threshold: 3,
            preparation_evidence_required: 2,
            preparation_alert_confidence: 70,
        }
    }

    fn event(native: Decimal, usd: Option<Decimal>, age: Option<i64>, commitment: Commitment) -> FundingEvent {
        FundingEvent {
            chain: ChainKind::Solana,
            signature: format!("sig-{native}-{commitment:?}"),
            slot: Some(1),
            observed_at: Utc::now(),
            commitment,
            from_address: "FromWalletAddressForFundingTests".into(),
            to_address: "ToWalletAddressForFundingTests".into(),
            asset_kind: AssetKind::Native,
            mint: String::new(),
            raw_amount: (native * Decimal::from(1_000_000_000)).to_string(),
            native_amount: native,
            amount_usd: usd,
            native_usd_price: None,
            recipient_age_seconds: age,
            source_kind: None,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn large_funding_requires_both_sol_and_usd() {
        let cfg = config();
        // 150 SOL + $30k qualifies.
        assert!(qualifies_as_large_funding(
            &event(Decimal::from(150), Some(Decimal::from(30_000)), Some(86_400), Commitment::Confirmed),
            &cfg,
            None
        ));
        // 150 SOL but missing USD price: cannot satisfy the absolute rule...
        // ...unless percentile applies; with None percentile it does not qualify.
        assert!(!qualifies_as_large_funding(
            &event(Decimal::from(150), None, Some(86_400), Commitment::Confirmed),
            &cfg,
            None
        ));
        // $30k but only 50 SOL: below SOL floor.
        assert!(!qualifies_as_large_funding(
            &event(Decimal::from(50), Some(Decimal::from(30_000)), Some(86_400), Commitment::Confirmed),
            &cfg,
            None
        ));
    }

    #[test]
    fn percentile_path_qualifies_with_floor() {
        let cfg = config();
        // 20 SOL with a 99th percentile threshold of 15 SOL qualifies (>= floor 10).
        assert!(qualifies_as_large_funding(
            &event(Decimal::from(20), None, Some(86_400), Commitment::Confirmed),
            &cfg,
            Some(Decimal::from(15))
        ));
        // 8 SOL below the percentile floor of 10 SOL never qualifies.
        assert!(!qualifies_as_large_funding(
            &event(Decimal::from(8), Some(Decimal::from(50_000)), Some(86_400), Commitment::Confirmed),
            &cfg,
            Some(Decimal::from(5))
        ));
    }

    #[test]
    fn token_funding_requires_usd_price() {
        let cfg = config();
        let mut ev = event(Decimal::ZERO, None, Some(86_400), Commitment::Confirmed);
        ev.asset_kind = AssetKind::SplToken;
        ev.mint = "Mint".into();
        // No USD price: no qualification.
        assert!(!qualifies_as_large_funding(&ev, &cfg, None));
        ev.amount_usd = Some(Decimal::from(25_000));
        assert!(qualifies_as_large_funding(&ev, &cfg, None));
    }

    #[test]
    fn freshness_enforced() {
        let cfg = config();
        // Fresh recipient (1 day old).
        assert!(recipient_is_fresh(
            &event(Decimal::from(150), Some(Decimal::from(30_000)), Some(86_400), Commitment::Confirmed),
            &cfg
        ));
        // 30 days old: too old.
        assert!(!recipient_is_fresh(
            &event(Decimal::from(150), Some(Decimal::from(30_000)), Some(30 * 86_400), Commitment::Confirmed),
            &cfg
        ));
        // Unknown age: not fresh.
        assert!(!recipient_is_fresh(
            &event(Decimal::from(150), Some(Decimal::from(30_000)), None, Commitment::Confirmed),
            &cfg
        ));
    }

    #[test]
    fn infrastructure_sources_excluded_from_confidence() {
        let mut ev = event(Decimal::from(150), Some(Decimal::from(30_000)), Some(86_400), Commitment::Confirmed);
        assert!(!ev.source_is_infrastructure());
        ev.source_kind = Some(WalletLabelKind::Bridge);
        assert!(ev.source_is_infrastructure());
        // Case confidence for infrastructure-sourced first funding is lower (40 vs 55).
        let confidence = if ev.source_is_infrastructure() { 40 } else { 55 };
        assert_eq!(confidence, 40);
    }
}
