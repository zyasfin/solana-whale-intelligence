//! PostgreSQL integration tests for the funding radar lifecycle.
//!
//! Requires a disposable database; skipped unless `TEST_DATABASE_URL` or
//! `DATABASE_URL` is set. Verifies: funded case creation, funding_watch,
//! preparation promotion with preparation_alert, one-transfer no-promotion,
//! deployment linking, expiry dismissal, and processed-only no-promotion.

#![cfg(feature = "pg_tests")]
#![cfg(test)]

use chrono::{Duration, Utc};
use rust_decimal::Decimal;

use crate::config::FundingRadarConfig;
use crate::funding_radar::{
    evaluate_radar_case, ingest_funding_event, link_deployment, record_case_evidence,
};
use crate::models::{AssetKind, ChainKind, Commitment, FundingEvent, RadarStage, WalletLabelKind};

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

fn funding_event(
    signature: &str,
    recipient: &str,
    native: Decimal,
    usd: Option<Decimal>,
    age_seconds: i64,
    commitment: Commitment,
    source_kind: Option<WalletLabelKind>,
) -> FundingEvent {
    FundingEvent {
        chain: ChainKind::Solana,
        signature: signature.to_string(),
        slot: Some(1),
        observed_at: Utc::now(),
        commitment,
        from_address: "FunderWalletAddressForIntegrationTests".to_string(),
        to_address: recipient.to_string(),
        asset_kind: AssetKind::Native,
        mint: String::new(),
        raw_amount: (native * Decimal::from(1_000_000_000)).to_string(),
        native_amount: native,
        amount_usd: usd,
        native_usd_price: None,
        recipient_age_seconds: Some(age_seconds),
        source_kind,
        raw: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn funded_case_and_watch_alert() {
    let (_scratch, pool) = crate::pg_test_support::migrated_scratch("frfunded_case_and_wa").await;
    let recipient = format!("FreshRecipient{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let event = funding_event(
        "sig-pg-1",
        &recipient,
        Decimal::from(150),
        Some(Decimal::from(30_000)),
        86_400,
        Commitment::Confirmed,
        None,
    );
    let case = ingest_funding_event(&pool, 1, event, &config(), None).await.unwrap();
    let case = case.expect("confirmed large funding creates a case");
    assert_eq!(case.stage, RadarStage::Funded);
    assert_eq!(case.chain, ChainKind::Solana);
    assert_eq!(case.recipient, recipient);
    assert_eq!(case.confidence, 55);

    // funding_watch evidence recorded.
    let decision = evaluate_radar_case(&pool, 1, ChainKind::Solana, &recipient, Utc::now(), &config())
        .await
        .unwrap();
    assert_eq!(decision.case_id, case.id);
    assert_eq!(decision.stage, RadarStage::Funded);
    assert_eq!(decision.evidence_count, 1, "one evidence item only");
    assert!(decision.alert.is_none(), "no preparation alert from one transfer");
}

#[tokio::test]
async fn one_transfer_never_promotes() {
    let (_scratch, pool) = crate::pg_test_support::migrated_scratch("frone_transfer_never").await;
    let recipient = format!("SingleTransfer{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let event = funding_event(
        "sig-pg-2",
        &recipient,
        Decimal::from(200),
        Some(Decimal::from(40_000)),
        3_600,
        Commitment::Confirmed,
        None,
    );
    ingest_funding_event(&pool, 1, event, &config(), None).await.unwrap();
    let decision = evaluate_radar_case(&pool, 1, ChainKind::Solana, &recipient, Utc::now(), &config())
        .await
        .unwrap();
    assert_eq!(decision.stage, RadarStage::Funded, "one transfer alone never promotes");
}

#[tokio::test]
async fn second_evidence_promotes_preparation_with_alert() {
    let (_scratch, pool) = crate::pg_test_support::migrated_scratch("frsecond_evidence_pr").await;
    let recipient = format!("PreparationWallet{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let first = funding_event(
        "sig-pg-3a",
        &recipient,
        Decimal::from(150),
        Some(Decimal::from(30_000)),
        86_400,
        Commitment::Confirmed,
        None,
    );
    let case = ingest_funding_event(&pool, 1, first, &config(), None)
        .await
        .unwrap()
        .expect("case created");

    // Second independent evidence: fan-out to three fresh children.
    record_case_evidence(
        &pool,
        case.id,
        "fanout",
        Utc::now(),
        serde_json::json!({ "children": 3, "infrastructure": false }),
    )
    .await
    .unwrap();

    let decision = evaluate_radar_case(&pool, 1, ChainKind::Solana, &recipient, Utc::now(), &config())
        .await
        .unwrap();
    assert_eq!(decision.stage, RadarStage::Preparation);
    assert!(decision.confidence >= 70);
    let alert = decision.alert.expect("preparation alert emitted");
    assert!(alert.message.contains("possible project preparation"));
}

#[tokio::test]
async fn deployment_link_promotes_to_deployed() {
    let (_scratch, pool) = crate::pg_test_support::migrated_scratch("frdeployment_link_pr").await;
    let recipient = format!("DeployWallet{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let first = funding_event(
        "sig-pg-4",
        &recipient,
        Decimal::from(150),
        Some(Decimal::from(30_000)),
        86_400,
        Commitment::Confirmed,
        None,
    );
    let case = ingest_funding_event(&pool, 1, first, &config(), None)
        .await
        .unwrap()
        .expect("case created");

    link_deployment(&pool, case.id, "deploy-sig-1", "MintAddressDeployed", Utc::now())
        .await
        .unwrap();
    let decision = evaluate_radar_case(&pool, 1, ChainKind::Solana, &recipient, Utc::now(), &config())
        .await
        .unwrap();
    assert_eq!(decision.stage, RadarStage::Deployed);
}

#[tokio::test]
async fn expired_window_dismisses_with_history_retained() {
    let (_scratch, pool) = crate::pg_test_support::migrated_scratch("frexpired_window_dis").await;
    let recipient = format!("ExpiryWallet{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let mut event = funding_event(
        "sig-pg-5",
        &recipient,
        Decimal::from(150),
        Some(Decimal::from(30_000)),
        86_400,
        Commitment::Confirmed,
        None,
    );
    // Backdate the first funding beyond the 7-day window.
    event.observed_at = Utc::now() - Duration::days(10);
    let case = ingest_funding_event(&pool, 1, event, &config(), None)
        .await
        .unwrap()
        .expect("case created");

    let decision = evaluate_radar_case(&pool, 1, ChainKind::Solana, &recipient, Utc::now(), &config())
        .await
        .unwrap();
    assert_eq!(decision.stage, RadarStage::Dismissed);
    assert_eq!(decision.reason, "expired window without preparation evidence");

    // History retained.
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM funding_radar_events WHERE case_id = $1")
        .bind(case.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(events >= 1, "radar event history retained after dismissal");
}

#[tokio::test]
async fn processed_only_event_never_promotes() {
    let (_scratch, pool) = crate::pg_test_support::migrated_scratch("frprocessed_only_eve").await;
    let recipient = format!("ProcessedWallet{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let event = funding_event(
        "sig-pg-6",
        &recipient,
        Decimal::from(150),
        Some(Decimal::from(30_000)),
        86_400,
        Commitment::Processed,
        None,
    );
    let case = ingest_funding_event(&pool, 1, event, &config(), None).await.unwrap();
    assert!(case.is_none(), "processed commitment cannot open a case");

    // Raw observation still stored for later confirmation.
    let stored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM funding_observations WHERE signature = 'sig-pg-6'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, 1, "raw funding observation retained");
}

#[tokio::test]
async fn infrastructure_source_retained_but_no_alpha() {
    let (_scratch, pool) = crate::pg_test_support::migrated_scratch("frinfrastructure_sou").await;
    let recipient = format!("InfraWallet{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let event = funding_event(
        "sig-pg-7",
        &recipient,
        Decimal::from(150),
        Some(Decimal::from(30_000)),
        86_400,
        Commitment::Confirmed,
        Some(WalletLabelKind::Bridge),
    );
    let case = ingest_funding_event(&pool, 1, event, &config(), None)
        .await
        .unwrap()
        .expect("case created for infrastructure source");
    // Lower initial confidence for infrastructure funding.
    assert_eq!(case.confidence, 40);
    assert_eq!(case.source_kind, Some(WalletLabelKind::Bridge));
}
