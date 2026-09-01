//! REST API (JSON) for the dashboard/frontend.
//!
//! Read-only endpoints. No secrets are ever returned; provider keys, session
//! material, and bot tokens are excluded from every payload.

use crate::models::ChainKind;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{Html, Json};
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;

/// Shared API state (database pool only).
#[derive(Clone)]
pub struct ApiState {
    pub pool: PgPool,
}

/// Authenticated workspace, derived from the session cookie (REV-025-F04).
/// Rejects with `401` when the request carries no valid session or the session
/// has no workspace binding — never a hardcoded literal.
pub struct Workspace(pub i64);

impl FromRequestParts<ApiState> for Workspace {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|cookie| {
                cookie.split(';').find_map(|part| {
                    let mut kv = part.trim().splitn(2, '=');
                    match (kv.next(), kv.next()) {
                        (Some(k), Some(v)) if k == crate::auth::SESSION_COOKIE => {
                            Some(v.to_string())
                        }
                        _ => None,
                    }
                })
            });
        let Some(token) = token else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        crate::auth::workspace_for_session(&state.pool, &token)
            .await
            .map(Workspace)
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

/// Build the API router.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/api/health", get(api_health))
        .route("/", get(serve_dashboard))
        .route("/api/wallets", get(api_wallets))
        .route("/api/wallets/{chain}/{address}/scores", get(api_wallet_scores))
        .route("/api/wallets/{chain}/{address}/labels", get(api_wallet_labels))
        .route("/api/funding/radar/cases", get(api_radar_cases))
        .route("/api/tokens/{chain}/{mint}/report", get(api_token_report))
        .route("/api/tokens/{chain}/{mint}/recent", get(api_token_recent))
        .route("/api/tokens/{chain}/{mint}/relations", get(api_token_relations))
        .route("/api/funding/radar/cases/{id}", get(api_radar_case))
        .route("/api/signals", get(api_signals))
        .route("/api/signals/rejections", get(api_signal_rejections))
        .route("/api/telegram/channels", get(api_telegram_channels))
        .route("/api/clusters", get(api_clusters))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WalletRow {
    chain: String,
    address: String,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    source: String,
    disposition: Option<String>,
}

#[derive(Serialize)]
struct ScoreRow {
    as_of: DateTime<Utc>,
    skill_score: i32,
    copyability_score: i32,
    conviction: i32,
    history_completeness: Option<Decimal>,
    provisional: bool,
}

#[derive(Serialize)]
struct LabelRow {
    kind: String,
    disposition: String,
    manual: bool,
    confidence: i32,
    active: bool,
}

#[derive(Serialize)]
struct RadarCaseRow {
    id: i64,
    chain: String,
    recipient: String,
    stage: String,
    confidence: i32,
    first_funding_native: Decimal,
    first_funding_usd: Option<Decimal>,
    source_address: String,
    fanout_count: i32,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct SignalRow {
    id: i64,
    chain: String,
    mint: String,
    signal_kind: String,
    score: i32,
    status: String,
    created_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct RejectionRow {
    chain: String,
    mint: String,
    signal_kind: String,
    rejection_code: String,
    evaluated_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct ChannelRow {
    channel_key: String,
    title: Option<String>,
    username: Option<String>,
    allowlisted: bool,
    status: String,
    last_observed_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct ClusterRow {
    cluster_id: i64,
    member_count: i64,
    created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn api_health(State(state): State<ApiState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let latency = crate::db::latency_ms(&state.pool).await.unwrap_or(-1.0);
    let radar = crate::health::radar_stage_counts(&state.pool).await.unwrap_or_default();
    let telegram = crate::health::telegram_health(&state.pool).await.unwrap_or_default();
    let raw_events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM raw_events")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "status": "ok",
        "database_latency_ms": latency,
        "funding_radar": {
            "funded": radar.funded,
            "preparation": radar.preparation,
            "deployed": radar.deployed,
            "dismissed": radar.dismissed,
        },
        "telegram": {
            "allowlisted": telegram.allowlisted,
            "active": telegram.active,
            "messages_stored": telegram.messages_stored,
        },
        "storage": { "raw_events": raw_events },
    })))
}

#[derive(serde::Deserialize, Default)]
struct WalletQuery {
    chain: Option<String>,
    limit: Option<i64>,
}

async fn api_wallets(
    State(state): State<ApiState>,
    Query(query): Query<WalletQuery>,
) -> Result<Json<Vec<WalletRow>>, StatusCode> {
    let chain = query
        .chain
        .as_deref()
        .and_then(ChainKind::parse)
        .map(|c| c.as_str())
        .unwrap_or("solana")
        .to_string();
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let rows = sqlx::query_as::<_, (String, String, DateTime<Utc>, DateTime<Utc>, String)>(
        "SELECT chain, address, first_seen, last_seen, source FROM wallets WHERE chain = $1 ORDER BY last_seen DESC LIMIT $2",
    )
    .bind(&chain)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut out = Vec::with_capacity(rows.len());
    for (chain, address, first_seen, last_seen, source) in rows {
        let disposition = crate::db::active_disposition(&state.pool, &chain, &address)
            .await
            .ok()
            .flatten();
        out.push(WalletRow {
            chain,
            address,
            first_seen,
            last_seen,
            source,
            disposition,
        });
    }
    Ok(Json(out))
}

async fn api_wallet_scores(
    State(state): State<ApiState>,
    Path((chain, address)): Path<(String, String)>,
) -> Result<Json<Vec<ScoreRow>>, StatusCode> {
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let rows = sqlx::query_as::<
        _,
        (DateTime<Utc>, i32, i32, i32, Option<Decimal>, bool),
    >(
        r#"
        SELECT as_of, skill_score, copyability_score, conviction, history_completeness, provisional
          FROM wallet_scores
         WHERE chain = $1 AND address = $2
         ORDER BY as_of DESC
         LIMIT 100
        "#,
    )
    .bind(chain.as_str())
    .bind(&address)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(as_of, skill_score, copyability_score, conviction, history_completeness, provisional)| ScoreRow {
            as_of,
            skill_score,
            copyability_score,
            conviction,
            history_completeness,
            provisional,
        })
        .collect();
    Ok(Json(out))
}

async fn api_wallet_labels(
    State(state): State<ApiState>,
    Path((chain, address)): Path<(String, String)>,
) -> Result<Json<Vec<LabelRow>>, StatusCode> {
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let rows = sqlx::query_as::<_, (String, String, bool, i32, bool)>(
        r#"
        SELECT kind, disposition, manual, confidence, revoked_at IS NULL AS active
          FROM wallet_labels
         WHERE chain = $1 AND address = $2
         ORDER BY created_at DESC
         LIMIT 200
        "#,
    )
    .bind(chain.as_str())
    .bind(&address)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(kind, disposition, manual, confidence, active)| LabelRow {
            kind,
            disposition,
            manual,
            confidence,
            active,
        })
        .collect();
    Ok(Json(out))
}

async fn api_token_report(
    State(state): State<ApiState>,
    Path((chain, mint)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let token = sqlx::query_as::<_, (Option<String>, Option<String>, String, Option<DateTime<Utc>>, serde_json::Value)>(
        "SELECT symbol, name, lifecycle_state, first_seen_at, risk_flags FROM tokens WHERE chain = $1 AND mint = $2",
    )
    .bind(chain.as_str())
    .bind(&mint)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;

    let (symbol, name, lifecycle, first_seen, risk_flags) = token;
    let narrative = crate::narrative::explain_narrative(&state.pool, chain, &mint, Utc::now())
        .await
        .ok()
        .flatten();
    let market = sqlx::query_as::<_, (Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<DateTime<Utc>>)>(
        r#"
        SELECT price_usd, market_cap_usd, liquidity_usd, observed_at
          FROM market_snapshots
         WHERE chain = $1 AND mint = $2
         ORDER BY observed_at DESC LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(&mint)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();

    Ok(Json(serde_json::json!({
        "chain": chain.as_str(),
        "mint": mint,
        "symbol": symbol,
        "name": name,
        "lifecycle_state": lifecycle,
        "first_seen_at": first_seen,
        "risk_flags": risk_flags,
        "narrative": narrative.map(|n| serde_json::json!({
            "narrative": n.narrative,
            "confidence": n.confidence,
            "why_now": n.why_now,
            "counter_evidence": n.counter_evidence,
            "sources": n.sources,
        })),
        "market": market.map(|(price, mcap, liq, at)| serde_json::json!({
            "price_usd": price,
            "market_cap_usd": mcap,
            "liquidity_usd": liq,
            "observed_at": at,
        })),
    })))
}

#[derive(serde::Deserialize, Default)]
struct RecentQuery {
    window: Option<String>,
}

async fn api_token_recent(
    State(state): State<ApiState>,
    Workspace(workspace_id): Workspace,
    Path((chain, mint)): Path<(String, String)>,
    Query(query): Query<RecentQuery>,
) -> Result<Json<Vec<solana_whale_intelligence::sf::recent::RecentEvent>>, StatusCode> {
    // Canonical chain parsing: an invalid chain returns 400 (REV-023 §1).
    if crate::models::ChainKind::parse(&chain).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let window = query.window.as_deref().unwrap_or("24h");
    if !matches!(window, "1h" | "24h" | "7d" | "30d" | "all") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let token_identity = format!("{chain}:{mint}");
    let events = solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &state.pool,
        workspace_id,
        &token_identity,
        window,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(events))
}

async fn api_token_relations(
    State(state): State<ApiState>,
    Workspace(workspace_id): Workspace,
    Path((chain, mint)): Path<(String, String)>,
) -> Result<Json<Vec<solana_whale_intelligence::sf::recent::CandidateRelation>>, StatusCode> {
    if crate::models::ChainKind::parse(&chain).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let token_identity = format!("{chain}:{mint}");
    let relations = solana_whale_intelligence::sf::recent_store::fetch_relations(
        &state.pool,
        workspace_id,
        &token_identity,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(relations))
}

async fn api_radar_cases(
    State(state): State<ApiState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<RadarCaseRow>>, StatusCode> {
    let stage = query.get("stage").cloned();
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    let rows = sqlx::query_as::<
        _,
        (i64, String, String, String, i32, Decimal, Option<Decimal>, String, i32, DateTime<Utc>),
    >(
        r#"
        SELECT id, chain, recipient, stage, confidence, first_funding_native,
               first_funding_usd, source_address, fanout_count, updated_at
          FROM funding_radar_cases
         WHERE ($1::text IS NULL OR stage = $1)
         ORDER BY updated_at DESC
         LIMIT $2
        "#,
    )
    .bind(stage)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(id, chain, recipient, stage, confidence, first_funding_native, first_funding_usd, source_address, fanout_count, updated_at)| RadarCaseRow {
            id,
            chain,
            recipient,
            stage,
            confidence,
            first_funding_native,
            first_funding_usd,
            source_address,
            fanout_count,
            updated_at,
        })
        .collect();
    Ok(Json(out))
}

async fn api_radar_case(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let case = sqlx::query_as::<
        _,
        (i64, String, String, String, i32, serde_json::Value, DateTime<Utc>),
    >(
        r#"
        SELECT id, chain, recipient, stage, confidence, evidence, updated_at
          FROM funding_radar_cases WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;

    let events = sqlx::query_as::<_, (String, DateTime<Utc>, serde_json::Value)>(
        "SELECT event_kind, observed_at, evidence FROM funding_radar_events WHERE case_id = $1 ORDER BY observed_at",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let (id, chain, recipient, stage, confidence, evidence, updated_at) = case;
    Ok(Json(serde_json::json!({
        "id": id,
        "chain": chain,
        "recipient": recipient,
        "stage": stage,
        "confidence": confidence,
        "evidence": evidence,
        "updated_at": updated_at,
        "events": events
            .into_iter()
            .map(|(kind, at, ev)| serde_json::json!({ "kind": kind, "observed_at": at, "evidence": ev }))
            .collect::<Vec<_>>(),
    })))
}

async fn api_signals(
    State(state): State<ApiState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<SignalRow>>, StatusCode> {
    let kind = query.get("kind").cloned();
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    let rows = sqlx::query_as::<_, (i64, String, String, String, i32, String, DateTime<Utc>)>(
        r#"
        SELECT id, chain, mint, signal_kind, score, status, created_at
          FROM signals
         WHERE ($1::text IS NULL OR signal_kind = $1)
         ORDER BY created_at DESC
         LIMIT $2
        "#,
    )
    .bind(kind)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(id, chain, mint, signal_kind, score, status, created_at)| SignalRow {
            id,
            chain,
            mint,
            signal_kind,
            score,
            status,
            created_at,
        })
        .collect();
    Ok(Json(out))
}

async fn api_signal_rejections(
    State(state): State<ApiState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<RejectionRow>>, StatusCode> {
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    let rows = sqlx::query_as::<_, (String, String, String, String, DateTime<Utc>)>(
        r#"
        SELECT chain, mint, signal_kind, rejection_code, evaluated_at
          FROM signal_evaluations
         WHERE status = 'rejected' AND rejection_code IS NOT NULL
         ORDER BY evaluated_at DESC
         LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(chain, mint, signal_kind, rejection_code, evaluated_at)| RejectionRow {
            chain,
            mint,
            signal_kind,
            rejection_code,
            evaluated_at,
        })
        .collect();
    Ok(Json(out))
}

async fn api_telegram_channels(
    State(state): State<ApiState>,
) -> Result<Json<Vec<ChannelRow>>, StatusCode> {
    let rows = sqlx::query_as::<
        _,
        (String, Option<String>, Option<String>, bool, String, Option<DateTime<Utc>>),
    >(
        r#"
        SELECT channel_key, title, username, allowlisted, status, last_observed_at
          FROM telegram_channels
         ORDER BY channel_key
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(channel_key, title, username, allowlisted, status, last_observed_at)| ChannelRow {
            channel_key,
            title,
            username,
            allowlisted,
            status,
            last_observed_at,
        })
        .collect();
    Ok(Json(out))
}

async fn api_clusters(
    State(state): State<ApiState>,
) -> Result<Json<Vec<ClusterRow>>, StatusCode> {
    let rows = sqlx::query_as::<_, (i64, i64, DateTime<Utc>)>(
        r#"
        SELECT c.cluster_id,
               COUNT(m.id) FILTER (WHERE m.revoked_at IS NULL) AS member_count,
               c.created_at
          FROM wallet_clusters c
          LEFT JOIN wallet_cluster_members m ON m.cluster_id = c.cluster_id
         GROUP BY c.cluster_id, c.created_at
         ORDER BY member_count DESC
         LIMIT 200
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(cluster_id, member_count, created_at)| ClusterRow {
            cluster_id,
            member_count,
            created_at,
        })
        .collect();
    Ok(Json(out))
}
/// Serve the single-page dashboard (vanilla JS fetch from /api/*).
async fn serve_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

pub const DASHBOARD_HTML: &str = include_str!("../static/index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_parse_for_api() {
        assert_eq!(ChainKind::parse("sol"), Some(ChainKind::Solana));
        assert_eq!(ChainKind::parse("robinhood"), Some(ChainKind::Robinhood));
        assert_eq!(ChainKind::parse("bogus"), None);
    }
}
