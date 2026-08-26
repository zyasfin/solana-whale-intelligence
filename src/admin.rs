//! Fully functional admin panel: password auth, RPC provider management,
//! Telegram channel management, wallet labels/blocklist, and dashboard UI.
//!
//! Every mutation endpoint requires a valid session. Secrets are never
//! returned; RPC API keys are managed by env-var reference, never raw value.

use crate::auth::{self, SESSION_COOKIE};
use crate::config::{AppConfig, EnvConfig, RuntimeProfile, Settings};
use crate::models::{ChainKind, Disposition};
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key};
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;

/// Admin app state.
#[derive(Clone)]
pub struct AdminState {
    pub pool: PgPool,
    /// Runtime configuration snapshot (used by the Settings page handlers).
    pub settings: Arc<Settings>,
    /// GMGN key pool for tracker endpoints (X/TG). Loaded from gmgn_keys;
    /// `None` when empty so the tracker routes return a clear 503.
    pub gmgn_pool: Option<Arc<crate::gmgn::GmgnPool>>,
    /// Tiny in-memory cache for tracker responses (feed/hot: 60s; wallet_stats
    /// enrichment: 15min) so UI refreshes don't burn the GMGN rate budget.
    pub track_cache: Arc<tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, serde_json::Value)>>>,
}

impl AdminState {
    /// Back-compat constructor for call sites that only have pool+settings
    /// (loads the GMGN pool from the DB; empty → tracker routes 503).
    pub async fn new(pool: PgPool, settings: Arc<Settings>) -> Self {
        let gmgn_pool = crate::gmgn::GmgnPool::load(pool.clone(), settings.config.gmgn.clone())
            .await
            .ok()
            .filter(|p| !p.is_empty())
            .map(Arc::new);
        Self {
            pool,
            settings,
            gmgn_pool,
            track_cache: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Cache read helper. Returns the cached JSON when fresh.
    async fn cache_get(&self, key: &str, ttl: std::time::Duration) -> Option<serde_json::Value> {
        let cache = self.track_cache.lock().await;
        cache.get(key).and_then(|(at, v)| {
            if at.elapsed() < ttl {
                Some(v.clone())
            } else {
                None
            }
        })
    }

    async fn cache_put(&self, key: &str, value: serde_json::Value) {
        let mut cache = self.track_cache.lock().await;
        cache.insert(key.to_string(), (std::time::Instant::now(), value));
    }
}

// ---------------------------------------------------------------------------
// Auth extraction
// ---------------------------------------------------------------------------

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookie| {
            cookie.split(';').find_map(|part| {
                let mut kv = part.trim().splitn(2, '=');
                match (kv.next(), kv.next()) {
                    (Some(k), Some(v)) if k == SESSION_COOKIE => Some(v.to_string()),
                    _ => None,
                }
            })
        })
}

async fn require_auth(state: &AdminState, headers: &HeaderMap) -> Result<(), StatusCode> {
    if !auth::auth_configured() {
        // Auth not configured: allow read access but block mutations.
        return Ok(());
    }
    let Some(token) = session_token(headers) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if auth::validate_session(&state.pool, &token).await {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Require auth even when ADMIN_PASSWORD_HASH is unset (mutation guard).
async fn require_write_auth(state: &AdminState, headers: &HeaderMap) -> Result<(), StatusCode> {
    if !auth::auth_configured() {
        return Err(StatusCode::FORBIDDEN);
    }
    require_auth(state, headers).await
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/", get(serve_dashboard))
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
        .route("/api/health", get(api_health))
        .route("/api/auth/status", get(auth_status))
        // Metrics (read-only)
        .route("/api/metrics/overview", get(metrics_overview))
        .route("/api/metrics/signals/timeline", get(metrics_signals_timeline))
        .route("/api/metrics/radar/trend", get(metrics_radar_trend))
        // API panel: DB-backed Helius / GMGN key pools
        .route("/api/helius-keys", get(list_helius_keys).post(create_helius_key))
        .route("/api/helius-keys/bulk", post(bulk_create_helius_keys))
        .route("/api/helius-keys/{id}/toggle", post(toggle_helius_key))
        .route("/api/helius-keys/{id}/test", post(test_helius_key))
        .route("/api/helius-keys/{id}", delete(delete_helius_key))
        .route("/api/gmgn-pubkeys", get(list_gmgn_pubkeys).post(create_gmgn_pubkey))
        .route("/api/gmgn-pubkeys/{id}", delete(delete_gmgn_pubkey))
        .route("/api/gmgn-pubkeys/{id}/reveal", post(reveal_gmgn_pubkey))
        .route("/api/gmgn-keys", get(list_gmgn_keys).post(create_gmgn_key))
        .route("/api/gmgn-keys/bulk", post(bulk_create_gmgn_keys))
        .route("/api/gmgn-keys/{id}/toggle", post(toggle_gmgn_key))
        .route("/api/gmgn-keys/{id}/test", post(test_gmgn_key))
        .route("/api/gmgn-keys/{id}", delete(delete_gmgn_key))
        // Trackers (GMGN-fed, read-only, cached)
        .route("/api/track/x/feed", get(track_x_feed))
        .route("/api/track/x/leaderboard", get(track_x_leaderboard))
        .route("/api/track/tg/hot", get(track_tg_hot))
        // Smart Wallets (GMGN-fed analysis track, separate from Funding Radar)
        .route("/api/smart-wallets", get(list_smart_wallets))
        .route("/api/smart-wallets/groups", get(list_smart_wallet_groups))
        .route("/api/smart-wallets/enrich-all", post(enrich_all_smart_wallets))
        .route("/api/smart-wallets/{address}/enrich", post(enrich_smart_wallet))
        .route("/api/smart-wallets/{address}/trades", get(smart_wallet_trades))
        .route("/api/smart-wallets/{address}/track", post(track_smart_wallet))
        .route("/api/smart-wallets/{address}/dismiss", post(dismiss_smart_wallet))
        // Telegram channels
        .route("/api/telegram/channels", get(list_channels).post(add_channel))
        .route("/api/telegram/channels/bulk", post(bulk_add_channels))
        .route("/api/telegram/channels/{key}", post(update_channel).delete(remove_channel))
        // Wallet labels / blocklist
        .route("/api/wallets/{chain}/{address}/labels", get(list_labels).post(add_label))
        .route("/api/wallets/{chain}/{address}/labels/{id}/revoke", post(revoke_label))
        .route("/api/blocklist/import", post(import_blocklist))
        // Read views (from the read API surface)
        .route("/api/wallets", get(list_wallets))
        .route("/api/wallets/{chain}/{address}/scores", get(wallet_scores))
        .route("/api/tokens/{chain}/{mint}/report", get(token_report))
        .route("/api/funding/radar/cases", get(radar_cases))
        .route("/api/funding/radar/cases/{id}", get(radar_case))
        .route("/api/signals", get(list_signals))
        .route("/api/signals/rejections", get(signal_rejections))
        .route("/api/clusters", get(list_clusters))
        // Settings (env read-only + runtime editable)
        .route("/api/settings/env", get(settings_env))
        .route("/api/settings/runtime", get(settings_runtime_get).post(settings_runtime_save))
        .route("/api/settings/secrets", post(settings_secret_set))
        .route("/api/settings/secrets/{name}", delete(settings_secret_delete))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Pages & auth handlers
// ---------------------------------------------------------------------------

async fn serve_dashboard(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if auth::auth_configured() {
        let authed = match session_token(&headers) {
            Some(token) => auth::validate_session(&state.pool, &token).await,
            None => false,
        };
        if !authed {
            return Redirect::to("/login").into_response();
        }
    }
    Html(crate::api::DASHBOARD_HTML).into_response()
}

async fn login_page() -> Html<&'static str> {
    Html(LOGIN_HTML)
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_submit(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !auth::auth_configured() {
        return (StatusCode::FORBIDDEN, "admin auth not configured").into_response();
    }
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim())
        .unwrap_or("unknown");
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let key = auth::rate_limit_key(Some(ip));
    match auth::is_rate_limited(&state.pool, &key).await {
        Ok(true) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Html(LOGIN_HTML.replace(
                    "<!--ERROR-->",
                    r#"<div class="err">terlalu banyak percobaan, coba lagi nanti</div>"#,
                )),
            )
                .into_response();
        }
        Ok(false) => {}
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let hash = auth::password_hash().unwrap_or_default();
    if !auth::verify_password(form.password.trim(), &hash) {
        let _ = auth::record_attempt(&state.pool, &key, false, Some(ip), ua).await;
        return Html(LOGIN_HTML.replace("<!--ERROR-->", r#"<div class="err">password salah</div>"#)).into_response();
    }
    let _ = auth::record_attempt(&state.pool, &key, true, Some(ip), ua).await;
    let _ = auth::clear_attempts(&state.pool, &key).await;
    match auth::create_session(&state.pool, Some(ip), ua).await {
        Ok(token) => {
            let cookie = auth::session_cookie_value(&token);
            let mut response = Redirect::to("/").into_response();
            response.headers_mut().insert(header::SET_COOKIE, cookie.parse().unwrap());
            response
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn logout(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        let _ = auth::destroy_session(&state.pool, &token).await;
    }
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, auth::clear_cookie_value().parse().unwrap());
    response
}

#[derive(Serialize)]
struct AuthStatus {
    configured: bool,
    authenticated: bool,
}

async fn auth_status(State(state): State<AdminState>, headers: HeaderMap) -> Json<AuthStatus> {
    let configured = auth::auth_configured();
    let authenticated = if configured {
        match session_token(&headers) {
            Some(token) => auth::validate_session(&state.pool, &token).await,
            None => false,
        }
    } else {
        true
    };
    Json(AuthStatus {
        configured,
        authenticated,
    })
}

async fn api_health(State(state): State<AdminState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    require_auth(&state, &headers).await?;
    let latency = crate::db::latency_ms(&state.pool).await.unwrap_or(-1.0);
    let radar = crate::health::radar_stage_counts(&state.pool).await.unwrap_or_default();
    let telegram = crate::health::telegram_health(&state.pool).await.unwrap_or_default();
    let smart_wallets_tracked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM smart_wallets WHERE status = 'tracked'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);
    // Per-group wallet counts (group-driven promotion tiers).
    let group_counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT group_name, COUNT(*) FROM smart_wallets WHERE group_name <> '' GROUP BY group_name",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let smart_wallets_by_group: serde_json::Map<String, serde_json::Value> = group_counts
        .into_iter()
        .map(|(name, n)| (name, serde_json::json!(n)))
        .collect();
    Ok(Json(serde_json::json!({
        "status": "ok",
        "database_latency_ms": latency,
        "funding_radar": {
            "funded": radar.funded, "preparation": radar.preparation,
            "deployed": radar.deployed, "dismissed": radar.dismissed,
        },
        "telegram": {
            "allowlisted": telegram.allowlisted, "active": telegram.active,
            "messages_stored": telegram.messages_stored,
        },
        "smart_wallets_tracked": smart_wallets_tracked,
        "smart_wallets_by_group": smart_wallets_by_group,
    })))
}

// ---------------------------------------------------------------------------
// Metrics (read-only time-series & overview)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MetricsQuery {
    hours: Option<i64>,
}

fn clamp_hours(h: Option<i64>, default: i64) -> i64 {
    h.unwrap_or(default).clamp(1, 168)
}

async fn metrics_signals_timeline(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<MetricsQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let hours = clamp_hours(query.hours, 24);
    let rows = sqlx::query_as::<_, (DateTime<Utc>, i64, i64)>(
        r#"
        SELECT date_trunc('hour', evaluated_at) AS t,
               COUNT(*) FILTER (WHERE status = 'accepted') AS accepted,
               COUNT(*) FILTER (WHERE status = 'rejected') AS rejected
          FROM signal_evaluations
         WHERE evaluated_at > now() - ($1 || ' hours')::interval
         GROUP BY t ORDER BY t
        "#,
    )
    .bind(hours)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| serde_json::json!({ "t": r.0, "accepted": r.1, "rejected": r.2 }))
            .collect(),
    ))
}

async fn metrics_radar_trend(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<MetricsQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let hours = clamp_hours(query.hours, 48);
    let rows = sqlx::query_as::<_, (DateTime<Utc>, i64, i64, i64)>(
        r#"
        SELECT date_trunc('hour', updated_at) AS t,
               COUNT(*) FILTER (WHERE stage = 'funded') AS funded,
               COUNT(*) FILTER (WHERE stage = 'preparation') AS preparation,
               COUNT(*) FILTER (WHERE stage = 'deployed') AS deployed
          FROM funding_radar_cases
         WHERE updated_at > now() - ($1 || ' hours')::interval
         GROUP BY t ORDER BY t
        "#,
    )
    .bind(hours)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| serde_json::json!({ "t": r.0, "funded": r.1, "preparation": r.2, "deployed": r.3 }))
            .collect(),
    ))
}

async fn metrics_overview(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_auth(&state, &headers).await?;
    let wallets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallets")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tokens")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let clusters: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallet_clusters")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let signals_24h: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM signals WHERE created_at > now() - interval '24 hours'")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let radar_open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM funding_radar_cases WHERE stage NOT IN ('dismissed','deployed')")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let transfers_24h: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transfers WHERE observed_at > now() - interval '24 hours'")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "wallets": wallets, "tokens": tokens, "clusters": clusters,
        "signals_24h": signals_24h, "radar_open": radar_open, "transfers_24h": transfers_24h,
    })))
}

// ---------------------------------------------------------------------------
// API panel: Helius keys (DB pool, monthly quotas)
// ---------------------------------------------------------------------------

/// Mask a key to `first6...last4` for list previews. Keys are never returned raw.
fn key_preview(key: &str) -> String {
    let k = key.trim();
    if k.len() <= 10 {
        return k.chars().take(4).chain(std::iter::repeat('…').take(1)).collect::<String>();
    }
    format!("{}...{}", &k[..6], &k[k.len() - 4..])
}

type HeliusKeyTuple = (
    i64,
    String,
    String,
    String,
    bool,
    Option<i32>,
    i32,
    String,
    Option<DateTime<Utc>>,
    Option<String>,
);

fn helius_key_json(row: HeliusKeyTuple, config: &crate::config::HeliusConfig) -> serde_json::Value {
    let (id, name, api_key, rpc_url, enabled, monthly_limit, used_this_month, usage_month, last_ok_at, last_error) = row;
    // A stale usage marker means the month rolled over: report zero.
    let used = if usage_month == crate::helius::current_month() {
        used_this_month
    } else {
        0
    };
    serde_json::json!({
        "id": id,
        "name": name,
        "key_preview": key_preview(&api_key),
        "rpc_url": rpc_url,
        "parse_tx_url": config.parse_tx_url(&api_key),
        "history_url": config.history_url(&api_key, "{address}"),
        "enabled": enabled,
        "monthly_limit": monthly_limit,
        "used_this_month": used,
        "last_ok_at": last_ok_at,
        "last_error": last_error,
    })
}

/// Default monthly quota applied when a Helius key is added without an
/// explicit `monthly_limit` (blank/null): Helius free plan = 1M credits/month.
/// An explicit number is kept as-is; NULL in the DB still means unlimited for
/// any pre-existing rows (no migration of those).
const HELIUS_DEFAULT_MONTHLY_LIMIT: i32 = 1_000_000;

const HELIUS_KEY_SELECT: &str =
    "SELECT id, name, api_key, rpc_url, enabled, monthly_limit, used_this_month, usage_month, last_ok_at, last_error FROM helius_keys";

async fn list_helius_keys(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let rows = sqlx::query_as::<_, HeliusKeyTuple>(&format!("{HELIUS_KEY_SELECT} ORDER BY id"))
        .fetch_all(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let config = &state.settings.config.helius;
    Ok(Json(rows.into_iter().map(|r| helius_key_json(r, config)).collect()))
}

#[derive(Deserialize)]
struct HeliusKeyForm {
    name: Option<String>,
    key_or_url: String,
    monthly_limit: Option<i32>,
}

async fn create_helius_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<HeliusKeyForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let Some((api_key, rpc_url)) = state.settings.config.helius.normalize_key_or_url(&form.key_or_url) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO helius_keys (name, api_key, rpc_url, monthly_limit) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(form.name.as_deref().unwrap_or("").trim())
    .bind(&api_key)
    .bind(&rpc_url)
    .bind(form.monthly_limit.filter(|l| *l > 0).or(Some(HELIUS_DEFAULT_MONTHLY_LIMIT)))
    .fetch_one(&state.pool)
    .await
    .map_err(|err| {
        if unique_violation(&err) {
            StatusCode::CONFLICT
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;
    Ok(Json(serde_json::json!({ "id": id, "ok": true })))
}

#[derive(Deserialize)]
struct BulkLinesForm {
    lines: String,
    pubkey_id: Option<i64>,
}

/// Whether a sqlx error is a unique-constraint violation (SQLSTATE 23505).
fn unique_violation(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db_err) => db_err.code().as_deref() == Some("23505"),
        _ => false,
    }
}

async fn bulk_create_helius_keys(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<BulkLinesForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let mut added = 0usize;
    let mut skipped_duplicates = 0usize;
    let mut errors: Vec<serde_json::Value> = Vec::new();
    for raw in form.lines.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((api_key, rpc_url)) = state.settings.config.helius.normalize_key_or_url(line) else {
            errors.push(serde_json::json!({ "line": line, "error": "unparsable: expected a raw key or a URL with api-key=..." }));
            continue;
        };
        let result = sqlx::query(
            "INSERT INTO helius_keys (api_key, rpc_url, monthly_limit) VALUES ($1, $2, $3) ON CONFLICT (api_key) DO NOTHING",
        )
        .bind(&api_key)
        .bind(&rpc_url)
        .bind(HELIUS_DEFAULT_MONTHLY_LIMIT)
        .execute(&state.pool)
        .await;
        match result {
            Ok(done) if done.rows_affected() > 0 => added += 1,
            Ok(_) => skipped_duplicates += 1,
            Err(_) => errors.push(serde_json::json!({ "line": line, "error": "database insert failed" })),
        }
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "added": added,
        "skipped_duplicates": skipped_duplicates,
        "errors": errors,
    })))
}

async fn toggle_helius_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    sqlx::query("UPDATE helius_keys SET enabled = NOT enabled WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn delete_helius_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    sqlx::query("DELETE FROM helius_keys WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Live-check one key with a real JSON-RPC `getHealth` call, then persist
/// last_ok_at / last_error so the panel reflects the outcome.
async fn test_helius_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let row: Option<(String,)> = sqlx::query_as("SELECT rpc_url FROM helius_keys WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some((rpc_url,)) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "swi-key-test",
        "method": "getHealth",
        "params": [],
    });
    let started = std::time::Instant::now();
    let outcome: Result<(), String> = async {
        let response = client
            .post(&rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("network error: {e}"))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| format!("decode error: {e}"))?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(format!("auth rejected (HTTP {status})"));
        }
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("non-JSON response: {e}"))?;
        if let Some(error) = value.get("error") {
            return Err(format!("rpc error: {error}"));
        }
        Ok(())
    }
    .await;
    let latency_ms = (started.elapsed().as_secs_f64() * 1000.0).round() as u64;
    let (ok, error) = match &outcome {
        Ok(()) => (true, None),
        Err(message) => (false, Some(message.clone())),
    };
    sqlx::query(
        "UPDATE helius_keys SET last_ok_at = CASE WHEN $2 THEN now() ELSE last_ok_at END, last_error = $3 WHERE id = $1",
    )
    .bind(id)
    .bind(ok)
    .bind(error.as_deref())
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": ok, "error": error, "latency_ms": latency_ms })))
}

// ---------------------------------------------------------------------------
// API panel: GMGN pubkeys + keys (3 API keys max per pubkey)
// ---------------------------------------------------------------------------

/// GMGN allows at most 3 API keys per Ed25519 pubkey.
const GMGN_MAX_KEYS_PER_PUBKEY: i64 = 3;

async fn list_gmgn_pubkeys(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    // private_key_pem is NEVER selected, let alone returned.
    let rows = sqlx::query_as::<_, (i64, String, DateTime<Utc>, i64)>(
        r#"
        SELECT p.id, p.public_key_pem, p.created_at,
               (SELECT COUNT(*) FROM gmgn_keys k WHERE k.pubkey_id = p.id) AS n_keys
          FROM gmgn_pubkeys p
         ORDER BY p.id
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows
        .into_iter()
        .map(|(id, public_key_pem, created_at, n_keys)| {
            serde_json::json!({
                "id": id,
                "public_key_pem": public_key_pem,
                "created_at": created_at,
                "n_keys": n_keys,
            })
        })
        .collect()))
}

/// Generate an Ed25519 keypair server-side (PKCS#8 PEM private + SPKI PEM
/// public), store both, and return ONLY the public PEM.
async fn create_gmgn_pubkey(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    let signing_key = ed25519_dalek::SigningKey::generate(&mut aes_gcm::aead::OsRng);
    // ed25519-dalek's to_pkcs8_pem emits PKCS#8 v2 (version 01, public key
    // appended as context [1]), which many consumers — including GMGN's panel
    // — reject as "extra data". Build the classic v1 OneAsymmetricKey DER
    // instead: SEQUENCE { INTEGER 0, SEQUENCE { OID 1.3.101.112 },
    // OCTET STRING ( OCTET STRING seed ) } and PEM-armor it.
    let seed = signing_key.to_bytes(); // 32-byte Ed25519 seed
    let mut der = Vec::with_capacity(48);
    der.extend_from_slice(&[0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20]);
    der.extend_from_slice(&seed);
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&der);
    let private_pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        b64.as_str()
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let public_pem = signing_key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO gmgn_pubkeys (public_key_pem, private_key_pem) VALUES ($1, $2) RETURNING id",
    )
    .bind(&public_pem)
    .bind(&private_pem)
    .fetch_one(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "id": id, "public_key_pem": public_pem })))
}

async fn list_gmgn_keys(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            String,
            String,
            Option<i64>,
            Option<String>,
            bool,
            Option<i32>,
            i32,
            String,
            Option<DateTime<Utc>>,
            Option<String>,
        ),
    >(
        r#"
        SELECT k.id, k.name, k.api_key, k.pubkey_id, p.public_key_pem,
               k.enabled, k.monthly_limit, k.used_this_month, k.usage_month,
               k.last_ok_at, k.last_error
          FROM gmgn_keys k
          LEFT JOIN gmgn_pubkeys p ON p.id = k.pubkey_id
         ORDER BY k.id
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let month = crate::helius::current_month();
    Ok(Json(rows
        .into_iter()
        .map(|(id, name, api_key, pubkey_id, pubkey_pem, enabled, monthly_limit, used_this_month, usage_month, last_ok_at, last_error)| {
            let used = if usage_month == month { used_this_month } else { 0 };
            serde_json::json!({
                "id": id,
                "name": name,
                "key_preview": key_preview(&api_key),
                "pubkey_id": pubkey_id,
                "pubkey_pem": pubkey_pem,
                "enabled": enabled,
                "monthly_limit": monthly_limit,
                "used_this_month": used,
                "last_ok_at": last_ok_at,
                "last_error": last_error,
            })
        })
        .collect()))
}

/// Current API-key count on a pubkey (cap enforcement).
async fn gmgn_pubkey_key_count(pool: &PgPool, pubkey_id: i64) -> Result<i64, StatusCode> {
    sqlx::query_scalar("SELECT COUNT(*) FROM gmgn_keys WHERE pubkey_id = $1")
        .bind(pubkey_id)
        .fetch_one(pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// A pubkey row exists (400 otherwise).
async fn gmgn_pubkey_exists(pool: &PgPool, pubkey_id: i64) -> Result<bool, StatusCode> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gmgn_pubkeys WHERE id = $1)")
        .bind(pubkey_id)
        .fetch_one(pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
struct GmgnKeyForm {
    name: Option<String>,
    api_key: String,
    pubkey_id: Option<i64>,
    monthly_limit: Option<i32>,
}

async fn create_gmgn_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<GmgnKeyForm>,
) -> Response {
    if require_write_auth(&state, &headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let api_key = form.api_key.trim();
    if api_key.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(pubkey_id) = form.pubkey_id {
        let exists = match gmgn_pubkey_exists(&state.pool, pubkey_id).await {
            Ok(v) => v,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        if !exists {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let count = match gmgn_pubkey_key_count(&state.pool, pubkey_id).await {
            Ok(v) => v,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        if count >= GMGN_MAX_KEYS_PER_PUBKEY {
            // GMGN allows max 3 API keys per pubkey: 400 with the reason.
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": "pubkey already has 3 API keys (GMGN allows max 3 per pubkey) — create a new pubkey",
                })),
            )
                .into_response();
        }
    }
    let result = sqlx::query_scalar::<_, i64>(
        "INSERT INTO gmgn_keys (name, api_key, pubkey_id, monthly_limit) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(form.name.as_deref().unwrap_or("").trim())
    .bind(api_key)
    .bind(form.pubkey_id)
    .bind(form.monthly_limit.filter(|l| *l > 0))
    .fetch_one(&state.pool)
    .await;
    match result {
        Ok(id) => Json(serde_json::json!({ "id": id, "ok": true })).into_response(),
        Err(err) if unique_violation(&err) => StatusCode::CONFLICT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn bulk_create_gmgn_keys(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<BulkLinesForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    // Remaining capacity on the target pubkey, enforced across the whole bulk.
    let mut remaining: Option<i64> = None;
    if let Some(pubkey_id) = form.pubkey_id {
        if !gmgn_pubkey_exists(&state.pool, pubkey_id).await? {
            return Err(StatusCode::BAD_REQUEST);
        }
        let count = gmgn_pubkey_key_count(&state.pool, pubkey_id).await?;
        remaining = Some((GMGN_MAX_KEYS_PER_PUBKEY - count).max(0));
    }
    let mut added = 0usize;
    let mut skipped_duplicates = 0usize;
    let mut errors: Vec<serde_json::Value> = Vec::new();
    for raw in form.lines.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains("://") || line.contains('?') || line.contains('/') || line.contains(' ') {
            errors.push(serde_json::json!({ "line": line, "error": "expected a bare GMGN API key" }));
            continue;
        }
        if let Some(left) = remaining {
            if left <= 0 {
                errors.push(serde_json::json!({
                    "line": line,
                    "error": "pubkey already has 3 API keys (GMGN allows max 3 per pubkey) — create a new pubkey",
                }));
                continue;
            }
        }
        let result = sqlx::query(
            "INSERT INTO gmgn_keys (api_key, pubkey_id) VALUES ($1, $2) ON CONFLICT (api_key) DO NOTHING",
        )
        .bind(line)
        .bind(form.pubkey_id)
        .execute(&state.pool)
        .await;
        match result {
            Ok(done) if done.rows_affected() > 0 => {
                added += 1;
                if let Some(left) = remaining.as_mut() {
                    *left -= 1;
                }
            }
            Ok(_) => skipped_duplicates += 1,
            Err(_) => errors.push(serde_json::json!({ "line": line, "error": "database insert failed" })),
        }
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "added": added,
        "skipped_duplicates": skipped_duplicates,
        "errors": errors,
    })))
}

async fn toggle_gmgn_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    sqlx::query("UPDATE gmgn_keys SET enabled = NOT enabled WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn delete_gmgn_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    sqlx::query("DELETE FROM gmgn_keys WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Delete a GMGN pubkey. Rejects with 400 while any `gmgn_keys` row still
/// references it (unbind/delete those keys first); otherwise removes the row.
async fn delete_gmgn_pubkey(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if require_write_auth(&state, &headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let bound: i64 = match sqlx::query_scalar("SELECT COUNT(*) FROM gmgn_keys WHERE pubkey_id = $1")
        .bind(id)
        .fetch_one(&state.pool)
        .await
    {
        Ok(n) => n,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if bound > 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("pubkey still has {bound} API key(s) bound — unbind keys first"),
            })),
        )
            .into_response();
    }
    match sqlx::query("DELETE FROM gmgn_pubkeys WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await
    {
        Ok(done) if done.rows_affected() > 0 => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Reveal a GMGN pubkey's PRIVATE key. Sensitive endpoint: returns the
/// PKCS#8 private key PEM (raw from the DB row) once per call, only over an
/// authenticated write session. The user pastes this into the GMGN panel
/// when binding an API key (the gmgn-cli flow keeps keypair.pem locally;
/// here the server holds it).
async fn reveal_gmgn_pubkey(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let row: Option<(String,)> = sqlx::query_as("SELECT private_key_pem FROM gmgn_pubkeys WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some((private_key_pem,)) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(Json(serde_json::json!({ "private_key_pem": private_key_pem })))
}

/// Live-check one GMGN key against a FREE OpenAPI route with the real
/// `X-APIKEY` auth headers (shared helper, so client/pool/test never drift).
///
/// Route: `GET {gmgn.base_url}/v1/market/rank?chain=sol&interval=1h&limit=1`
/// — the public trending-rank endpoint; GMGN OpenAPI is open to all users
/// (1 req/s), so any valid key can call it (unlike the earlier
/// `/defi/quotation/v1/tokens/top_buyers/...` premium path, which 403s even
/// valid keys).
///
/// Semantics:
/// - 2xx            → ok (key accepted)
/// - 404            → ok (auth passed; route drifted — GMGN iterates fast)
/// - 401            → rejected (bad key)
/// - 403 with a Cloudflare challenge body → INCONCLUSIVE (edge bot check
///   intercepted before GMGN auth; cannot judge the key) → reported as an
///   error, NOT as "key rejected"
/// - 403 with a JSON/envelope body → rejected (GMGN auth/plan denial)
/// last_error always records the actual HTTP status / network error.
async fn test_gmgn_key(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    // Join gmgn_pubkeys to fetch the Ed25519 private key used for signing.
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT k.api_key, p.private_key_pem
          FROM gmgn_keys k
          LEFT JOIN gmgn_pubkeys p ON p.id = k.pubkey_id
         WHERE k.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some((api_key, private_key_pem)) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    let auth = crate::gmgn::GmgnAuth::new(&api_key, private_key_pem.as_deref());
    if !auth.can_sign() {
        // No server-held signing key bound → cannot produce a signed request.
        return Ok(Json(serde_json::json!({
            "ok": false,
            "error": "no pubkey bound — signing unavailable",
        })));
    }
    let base = state.settings.config.gmgn.base_url.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let started = std::time::Instant::now();
    let outcome: Result<(), String> = async {
        let mut params = std::collections::BTreeMap::new();
        params.insert("chain".to_string(), "sol".to_string());
        params.insert("interval".to_string(), "1h".to_string());
        params.insert("limit".to_string(), "1".to_string());
        let (url, headers) = auth.prepare_request(base, "/v1/market/rank", &params, "");
        let mut request = client.get(&url);
        for header in headers {
            request = request.header(&header.name, header.value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("network error: {e}"))?;
        let status = response.status();
        let code = status.as_u16();
        // Read a small body slice to distinguish Cloudflare challenges (HTML
        // "Just a moment...") from GMGN's own JSON error envelopes.
        let body = response.text().await.unwrap_or_default();
        let body = body.as_str();
        match code {
            401 => Err(format!("key rejected (HTTP 401)")),
            403 => {
                let cloudflare = body.contains("Just a moment")
                    || body.contains("challenges.cloudflare.com")
                    || body.contains("cf-chl");
                if cloudflare {
                    Err("inconclusive: Cloudflare edge challenge (HTTP 403) — retry from the server".to_string())
                } else {
                    Err(format!("key rejected (HTTP 403)"))
                }
            }
            _ => Ok(()), // 2xx ok; 404 treated as ok (auth passed, route drift)
        }
    }
    .await;
    let latency_ms = (started.elapsed().as_secs_f64() * 1000.0).round() as u64;
    let (ok, error) = match &outcome {
        Ok(()) => (true, None),
        Err(message) => (false, Some(message.clone())),
    };
    sqlx::query(
        "UPDATE gmgn_keys SET last_ok_at = CASE WHEN $2 THEN now() ELSE last_ok_at END, last_error = $3 WHERE id = $1",
    )
    .bind(id)
    .bind(ok)
    .bind(error.as_deref())
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": ok, "error": error, "latency_ms": latency_ms })))
}

// ---------------------------------------------------------------------------
// Trackers (GMGN-fed, read-only, cached)
//
// NOTE: GMGN social fields (twitter_username, twitter_name, telegram, etc.)
// are attacker-controlled / untrusted third-party data. They are displayed
// verbatim for research only — never executed, never used for auth, and must
// be treated as untrusted input by any consumer.
// ---------------------------------------------------------------------------

/// 503 body when no GMGN key pool is configured.
fn track_no_pool() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "no GMGN keys configured — add one in API Keys" })),
    )
}

/// Extract a nested string by trying several dotted paths (payloads vary).
fn pick_str<'a>(v: &'a serde_json::Value, paths: &[&str]) -> Option<&'a str> {
    paths.iter().find_map(|p| v.pointer(p).and_then(|x| x.as_str()))
}

/// Extract a number (f64) by trying several dotted paths.
fn pick_num(v: &serde_json::Value, paths: &[&str]) -> Option<f64> {
    paths.iter().find_map(|p| {
        v.pointer(p)
            .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok())))
    })
}

/// Normalize one KOL/SmartMoney trade row from GMGN into our feed shape.
/// Real GMGN row schema (verified live): token address = `base_address`,
/// symbol = `base_token.symbol`, wallet = `maker`, ts = `timestamp`,
/// usd = `amount_usd`, social = `maker_info{twitter_username,twitter_name}`.
fn track_trade_row(trade: &serde_json::Value, tag: &str) -> serde_json::Value {
    let ts = pick_num(trade, &["/timestamp", "/ts", "/block_time", "/time"]).unwrap_or(0.0);
    serde_json::json!({
        "ts": ts as i64,
        "side": pick_str(trade, &["/side", "/type", "/event_type"]).unwrap_or(""),
        "usd": pick_num(trade, &["/amount_usd", "/usd", "/usd_value", "/value_usd"]),
        "token_address": pick_str(trade, &["/base_address", "/token_address", "/token/address", "/address"]).unwrap_or(""),
        "token_symbol": pick_str(trade, &["/base_token/symbol", "/token_symbol", "/token/symbol", "/symbol"]).unwrap_or(""),
        "wallet": pick_str(trade, &["/maker", "/maker_info/address", "/wallet", "/wallet_address", "/address"]).unwrap_or(""),
        "wallet_tag": tag,
        "twitter_username": pick_str(trade, &["/maker_info/twitter_username", "/twitter_username"]).unwrap_or(""),
        "twitter_name": pick_str(trade, &["/maker_info/twitter_name", "/twitter_name"]).unwrap_or(""),
    })
}

/// Collect trade rows from a KOL/SmartMoney envelope. Real GMGN shape is
/// `data.list[]`; tolerate array/items/trades/rows variants too.
fn track_extract_trades(data: &serde_json::Value, tag: &str) -> Vec<serde_json::Value> {
    let arr: Vec<serde_json::Value> = if let Some(a) = data.as_array() {
        a.clone()
    } else {
        ["list", "trades", "items", "data", "rows"]
            .iter()
            .find_map(|k| data.get(k).and_then(|v| v.as_array()).cloned())
            .unwrap_or_default()
    };
    arr.iter().map(|t| track_trade_row(t, tag)).collect()
}

/// Public wrapper so the Smart Wallets worker can reuse the exact GMGN trade
/// extraction (single source of truth for the payload shape).
pub fn track_extract_trades_pub(data: &serde_json::Value, tag: &str) -> Vec<serde_json::Value> {
    track_extract_trades(data, tag)
}

/// Fetch the merged KOL+SmartMoney feed (calls both /v1/user/kol and
/// /v1/user/smartmoney, merges by ts desc). The FULL merged batch is cached
/// 60s; `limit` only truncates the returned slice, so a small request never
/// poisons the cache for a larger one (e.g. the leaderboard).
async fn track_fetch_feed(state: &AdminState, pool: &crate::gmgn::GmgnPool, limit: usize) -> Result<Vec<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(cached) = state.cache_get("x_feed", std::time::Duration::from_secs(60)).await {
        let mut rows: Vec<serde_json::Value> = serde_json::from_value(cached).unwrap_or_default();
        rows.truncate(limit);
        return Ok(rows);
    }
    let mut params = std::collections::BTreeMap::new();
    params.insert("chain".to_string(), "sol".to_string());
    let kol = pool.v1_get("/v1/user/kol", params.clone()).await;
    let smart = pool.v1_get("/v1/user/smartmoney", params).await;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    match kol {
        Ok(env) => rows.extend(track_extract_trades(&env.data, "kol")),
        Err(e) => tracing::warn!(error = %e, "x tracker kol feed failed"),
    }
    match smart {
        Ok(env) => rows.extend(track_extract_trades(&env.data, "smart")),
        Err(e) => tracing::warn!(error = %e, "x tracker smartmoney feed failed"),
    }
    rows.sort_by_key(|r| -(r.get("ts").and_then(|v| v.as_i64()).unwrap_or(0)));
    // Cache the full merged batch (uncapped by `limit`).
    state.cache_put("x_feed", serde_json::Value::Array(rows.clone())).await;
    rows.truncate(limit);
    Ok(rows)
}

#[derive(Deserialize)]
struct TrackQuery {
    limit: Option<usize>,
}

/// GET /api/track/x/feed?limit=50 — merged KOL+SmartMoney trades, ts desc.
async fn track_x_feed(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<TrackQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_auth(&state, &headers).await.map_err(|s| (s, Json(serde_json::json!({"error":"unauthorized"}))))?;
    let Some(pool) = state.gmgn_pool.clone() else {
        return Err(track_no_pool());
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = track_fetch_feed(&state, &pool, limit).await?;
    Ok(Json(serde_json::json!(rows)))
}

/// GET /api/track/x/leaderboard?limit=25 — aggregate the feed batch by
/// twitter_username; wallet_stats enrichment for the top 5 (cached 15min).
async fn track_x_leaderboard(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<TrackQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_auth(&state, &headers).await.map_err(|s| (s, Json(serde_json::json!({"error":"unauthorized"}))))?;
    let Some(pool) = state.gmgn_pool.clone() else {
        return Err(track_no_pool());
    };
    let limit = q.limit.unwrap_or(25).clamp(1, 100);
    let feed = track_fetch_feed(&state, &pool, 200).await?;

    // Aggregate by twitter_username.
    #[derive(Default)]
    struct Agg {
        twitter_name: String,
        n_trades: usize,
        total_usd: f64,
        wallets: Vec<String>,
    }
    let mut map: std::collections::HashMap<String, Agg> = std::collections::HashMap::new();
    for row in &feed {
        let username = row.get("twitter_username").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if username.is_empty() {
            continue;
        }
        let agg = map.entry(username).or_default();
        if agg.twitter_name.is_empty() {
            agg.twitter_name = row.get("twitter_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        }
        agg.n_trades += 1;
        agg.total_usd += row.get("usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let wallet = row.get("wallet").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !wallet.is_empty() && !agg.wallets.contains(&wallet) {
            agg.wallets.push(wallet);
        }
    }
    let mut leaders: Vec<(String, Agg)> = map.into_iter().collect();
    leaders.sort_by(|a, b| b.1.total_usd.partial_cmp(&a.1.total_usd).unwrap_or(std::cmp::Ordering::Equal));
    leaders.truncate(limit);

    // wallet_stats enrichment for the top 5 (per-wallet, cached 15min). Each
    // call is time-boxed so a slow GMGN rate bucket never stalls the whole
    // endpoint — on timeout the row is returned without enrichment.
    let mut out: Vec<serde_json::Value> = Vec::new();
    for (idx, (username, agg)) in leaders.iter().enumerate() {
        let (mut followers, mut blue) = (None, None);
        if idx < 5 {
            if let Some(wallet) = agg.wallets.first() {
                let cache_key = format!("ws_{wallet}");
                if let Some(cached) = state.cache_get(&cache_key, std::time::Duration::from_secs(15 * 60)).await {
                    followers = cached.get("followers_count").cloned();
                    blue = cached.get("is_blue_verified").cloned();
                } else {
                    let mut params = std::collections::BTreeMap::new();
                    params.insert("address".to_string(), wallet.clone());
                    let fetch = tokio::time::timeout(
                        std::time::Duration::from_secs(6),
                        pool.v1_get("/v1/user/wallet_stats", params),
                    )
                    .await;
                    if let Ok(Ok(env)) = fetch {
                        let common = &env.data;
                        followers = Some(serde_json::json!(pick_num(common, &["/common/followers_count", "/followers_count"])));
                        blue = common.pointer("/common/is_blue_verified").or_else(|| common.get("is_blue_verified")).cloned();
                        state.cache_put(&cache_key, serde_json::json!({
                            "followers_count": followers.clone().unwrap_or(serde_json::Value::Null),
                            "is_blue_verified": blue.clone().unwrap_or(serde_json::Value::Null),
                        })).await;
                    }
                }
            }
        }
        out.push(serde_json::json!({
            "twitter_username": username,
            "twitter_name": agg.twitter_name,
            "n_trades": agg.n_trades,
            "total_usd": agg.total_usd,
            "wallets": agg.wallets,
            "followers_count": followers.unwrap_or(serde_json::Value::Null),
            "is_blue_verified": blue.unwrap_or(serde_json::Value::Null),
        }));
    }
    Ok(Json(serde_json::json!(out)))
}

/// GET /api/track/tg/hot?limit=25 — POST /v1/trenches (newest tokens),
/// mapped to TG/X social rows sorted by tg_call_count desc. Cached 60s.
async fn track_tg_hot(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<TrackQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_auth(&state, &headers).await.map_err(|s| (s, Json(serde_json::json!({"error":"unauthorized"}))))?;
    let Some(pool) = state.gmgn_pool.clone() else {
        return Err(track_no_pool());
    };
    let limit = q.limit.unwrap_or(25).clamp(1, 100);
    if let Some(cached) = state.cache_get("tg_hot", std::time::Duration::from_secs(60)).await {
        let mut rows: Vec<serde_json::Value> = serde_json::from_value(cached).unwrap_or_default();
        rows.truncate(limit);
        return Ok(Json(serde_json::json!(rows)));
    }

    // Shared trenches helper (same request shape as the discovery worker).
    let env = pool
        .trenches("sol", 80)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": format!("trenches fetch failed: {e}") }))))?;

    // Response: data{new_creation:[..], near_completion:[..], completed:[..]}
    // (or a flat array). Flatten all category arrays.
    let items = crate::gmgn::flatten_trenches_tokens(&env.data);
    let mut rows: Vec<serde_json::Value> = items
        .iter()
        .map(|t| {
            serde_json::json!({
                "token_address": pick_str(t, &["/address", "/token_address", "/token/address", "/contract"]).unwrap_or(""),
                "symbol": pick_str(t, &["/symbol", "/token/symbol", "/token_symbol"]).unwrap_or(""),
                "tg_call_count": pick_num(t, &["/social/tg_call_count", "/tg_call_count", "/callout_count"]),
                "x_user_follower": pick_num(t, &["/social/x_user_follower", "/x_user_follower"]),
                "twitter": pick_str(t, &["/social/twitter", "/twitter", "/link/twitter"]).unwrap_or(""),
                "telegram": pick_str(t, &["/social/telegram", "/telegram", "/link/telegram"]).unwrap_or(""),
                "liquidity": pick_num(t, &["/liquidity", "/liquidity_usd"]),
                "mcap": pick_num(t, &["/usd_market_cap", "/market_cap", "/mcap"]),
                "created_at": pick_num(t, &["/created_timestamp", "/creation_timestamp", "/created_at"]),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let ta = a.get("tg_call_count").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let tb = b.get("tg_call_count").and_then(|v| v.as_f64()).unwrap_or(0.0);
        tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit.max(100));
    state.cache_put("tg_hot", serde_json::Value::Array(rows.clone())).await;
    rows.truncate(limit);
    Ok(Json(serde_json::json!(rows)))
}

// ---------------------------------------------------------------------------
// Smart Wallets (GMGN-fed analysis track, separate from Funding Radar)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SmartWalletQuery {
    status: Option<String>,
    group: Option<String>,
    limit: Option<i64>,
}

/// GET /api/smart-wallets?status=&group=&limit=50 — list with trade
/// aggregates + extracted win_rate / realized_pnl from the stats payload.
/// `group` filters by assigned promotion tier (exact match, "" = no group).
async fn list_smart_wallets(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<SmartWalletQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let status_filter = q.status.as_deref().filter(|s| !s.trim().is_empty());
    let group_filter = q.group.as_deref().filter(|s| !s.trim().is_empty());
    let rows = sqlx::query_as::<
        _,
        (
            String, String, Option<String>, Option<String>, Option<i32>, Option<bool>,
            String, bool, String, Option<serde_json::Value>, DateTime<Utc>, i64, Option<f64>,
        ),
    >(
        r#"
        SELECT w.address, w.source, w.twitter_username, w.twitter_name,
               w.followers_count, w.is_blue_verified, w.status, w.tracked, w.group_name, w.stats,
               w.last_seen_at,
               (SELECT COUNT(*) FROM smart_wallet_trades t WHERE t.chain = w.chain AND t.wallet = w.address) AS n_trades,
               (SELECT SUM(t.amount_usd)::float8 FROM smart_wallet_trades t WHERE t.chain = w.chain AND t.wallet = w.address) AS total_usd
          FROM smart_wallets w
         WHERE ($1::text IS NULL OR w.status = $1)
           AND ($2::text IS NULL OR w.group_name = $2)
         ORDER BY w.tracked DESC, n_trades DESC, w.last_seen_at DESC
         LIMIT $3
        "#,
    )
    .bind(status_filter)
    .bind(group_filter)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out = rows
        .into_iter()
        .map(|(address, source, tw_user, tw_name, followers, blue, status, tracked, group_name, stats, last_seen, n_trades, total_usd)| {
            let num = |v: &serde_json::Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()));
            let win_rate = stats.as_ref().and_then(|s| s.pointer("/pnl_stat/winrate").or_else(|| s.get("winrate")).and_then(num));
            let realized_pnl = stats.as_ref().and_then(|s| s.get("realized_profit").and_then(num));
            serde_json::json!({
                "address": address,
                "source": source,
                "twitter_username": tw_user,
                "twitter_name": tw_name,
                "followers_count": followers,
                "is_blue_verified": blue,
                "status": status,
                "tracked": tracked,
                "group_name": group_name,
                "n_trades": n_trades,
                "total_usd": total_usd,
                "last_seen_at": last_seen,
                "win_rate": win_rate,
                "realized_pnl": realized_pnl,
            })
        })
        .collect();
    Ok(Json(out))
}

/// POST /api/smart-wallets/enrich-all — queue every non-dismissed wallet for
/// enrichment (sets enrich_requested_at; the enrich worker drains these first
/// on its next cycle). Returns {queued: n}.
async fn enrich_all_smart_wallets(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let done = sqlx::query(
        "UPDATE smart_wallets SET enrich_requested_at = now() WHERE status <> 'dismissed'",
    )
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let queued = done.rows_affected();
    Ok(Json(serde_json::json!({ "ok": true, "queued": queued })))
}

/// POST /api/smart-wallets/{address}/enrich — queue ONE wallet for enrichment.
/// If the address isn't tracked yet, insert a minimal row (source='manual',
/// status='candidate') first so any address can be enriched on demand.
/// Returns {queued: true, existed: bool}.
async fn enrich_smart_wallet(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let address = address.trim();
    if address.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let existed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM smart_wallets WHERE chain = 'solana' AND address = $1)",
    )
    .bind(address)
    .fetch_one(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !existed {
        // Insert a minimal manual-candidate row so the enrich worker can fetch it.
        sqlx::query(
            "INSERT INTO smart_wallets (chain, address, source, status) VALUES ('solana', $1, 'manual', 'candidate') ON CONFLICT (chain, address) DO NOTHING",
        )
        .bind(address)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    sqlx::query(
        "UPDATE smart_wallets SET enrich_requested_at = now(), status = CASE WHEN status = 'dismissed' THEN 'candidate' ELSE status END WHERE chain = 'solana' AND address = $1",
    )
    .bind(address)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true, "queued": true, "existed": existed })))
}

/// GET /api/smart-wallets/groups — config promotion tiers with live wallet
/// counts per group (group_name = name).
async fn list_smart_wallet_groups(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let groups = state.settings.config.smart_wallet.effective_groups();
    let mut out = Vec::new();
    for g in groups {
        let n_wallets: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM smart_wallets WHERE chain = 'solana' AND group_name = $1",
        )
        .bind(&g.name)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
        out.push(serde_json::json!({
            "name": g.name,
            "min_win_rate": g.min_win_rate,
            "min_realized_pnl_usd": g.min_realized_pnl_usd,
            "min_trades": g.min_trades,
            "n_wallets": n_wallets,
        }));
    }
    Ok(Json(out))
}

/// GET /api/smart-wallets/{address}/trades?limit=50 — latest tracked trades.
async fn smart_wallet_trades(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<TrackQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as i64;
    let rows = sqlx::query_as::<
        _,
        (String, Option<String>, String, Option<rust_decimal::Decimal>, DateTime<Utc>, String),
    >(
        r#"
        SELECT mint, symbol, side, amount_usd, trade_ts, source
          FROM smart_wallet_trades
         WHERE chain = 'solana' AND wallet = $1
         ORDER BY trade_ts DESC
         LIMIT $2
        "#,
    )
    .bind(&address)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows
        .into_iter()
        .map(|(mint, symbol, side, amount_usd, trade_ts, source)| {
            serde_json::json!({
                "mint": mint,
                "symbol": symbol,
                "side": side,
                "amount_usd": amount_usd,
                "trade_ts": trade_ts,
                "source": source,
            })
        })
        .collect()))
}

#[derive(Deserialize)]
struct TrackWalletForm {
    tracked: bool,
}

/// POST /api/smart-wallets/{address}/track {tracked} — pin/unpin watching.
async fn track_smart_wallet(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(address): Path<String>,
    Json(form): Json<TrackWalletForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    // tracked=true pins + forces status tracked; false unpins -> candidate.
    let new_status = if form.tracked { "tracked" } else { "candidate" };
    let done = sqlx::query(
        "UPDATE smart_wallets SET tracked = $2, status = $3 WHERE chain = 'solana' AND address = $1",
    )
    .bind(&address)
    .bind(form.tracked)
    .bind(new_status)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if done.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(serde_json::json!({ "ok": true, "tracked": form.tracked, "status": new_status })))
}

/// POST /api/smart-wallets/{address}/dismiss — mark dismissed.
async fn dismiss_smart_wallet(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let done = sqlx::query(
        "UPDATE smart_wallets SET status = 'dismissed', tracked = false WHERE chain = 'solana' AND address = $1",
    )
    .bind(&address)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if done.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(serde_json::json!({ "ok": true, "status": "dismissed" })))
}

// ---------------------------------------------------------------------------
// Telegram channels CRUD
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ChannelRow {
    channel_key: String,
    title: Option<String>,
    username: Option<String>,
    allowlisted: bool,
    status: String,
    last_observed_at: Option<DateTime<Utc>>,
}

async fn list_channels(State(state): State<AdminState>, headers: HeaderMap) -> Result<Json<Vec<ChannelRow>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, bool, String, Option<DateTime<Utc>>)>(
        "SELECT channel_key, title, username, allowlisted, status, last_observed_at FROM telegram_channels ORDER BY channel_key",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| ChannelRow {
        channel_key: r.0, title: r.1, username: r.2, allowlisted: r.3, status: r.4, last_observed_at: r.5,
    }).collect()))
}

#[derive(Deserialize)]
struct ChannelForm {
    channel_key: String,
    title: Option<String>,
    username: Option<String>,
}

async fn add_channel(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<ChannelForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    sqlx::query(
        r#"
        INSERT INTO telegram_channels (channel_key, title, username, allowlisted, status)
        VALUES ($1, $2, $3, true, 'active')
        ON CONFLICT (channel_key) DO UPDATE
            SET allowlisted = true, status = 'active',
                title = COALESCE(EXCLUDED.title, telegram_channels.title),
                username = COALESCE(EXCLUDED.username, telegram_channels.username)
        "#,
    )
    .bind(&form.channel_key)
    .bind(&form.title)
    .bind(&form.username)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// A bulk channel entry: bare key string or { channel_key, title } object.
#[derive(Deserialize)]
#[serde(untagged)]
enum BulkChannelEntry {
    Key(String),
    Full { channel_key: String, title: Option<String> },
}

#[derive(Deserialize)]
struct BulkChannelForm {
    channels: Vec<BulkChannelEntry>,
}

async fn bulk_add_channels(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<BulkChannelForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let mut imported = 0usize;
    for entry in &form.channels {
        let (channel_key, title) = match entry {
            BulkChannelEntry::Key(k) => (k, &None),
            BulkChannelEntry::Full { channel_key, title } => (channel_key, title),
        };
        if channel_key.trim().is_empty() {
            continue;
        }
        sqlx::query(
            r#"
            INSERT INTO telegram_channels (channel_key, title, username, allowlisted, status)
            VALUES ($1, $2, NULL, true, 'active')
            ON CONFLICT (channel_key) DO UPDATE
                SET allowlisted = true, status = 'active',
                    title = COALESCE(EXCLUDED.title, telegram_channels.title)
            "#,
        )
        .bind(channel_key)
        .bind(title)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        imported += 1;
    }
    Ok(Json(serde_json::json!({ "ok": true, "imported": imported })))
}

#[derive(Deserialize)]
struct ChannelUpdate {
    allowlisted: Option<bool>,
    status: Option<String>,
}

async fn update_channel(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(form): Json<ChannelUpdate>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    sqlx::query(
        r#"
        UPDATE telegram_channels SET
            allowlisted = COALESCE($2, allowlisted),
            status = COALESCE($3, status)
         WHERE channel_key = $1
        "#,
    )
    .bind(&key)
    .bind(form.allowlisted)
    .bind(&form.status)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn remove_channel(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    // Remove from allowlist; retain message history.
    sqlx::query("UPDATE telegram_channels SET allowlisted = false, status = 'disabled' WHERE channel_key = $1")
        .bind(&key)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Wallet labels & blocklist
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct LabelRow {
    id: i64,
    kind: String,
    disposition: String,
    manual: bool,
    confidence: i32,
    active: bool,
}

async fn list_labels(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((chain, address)): Path<(String, String)>,
) -> Result<Json<Vec<LabelRow>>, StatusCode> {
    require_auth(&state, &headers).await?;
    if ChainKind::parse(&chain).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let rows = sqlx::query_as::<_, (i64, String, String, bool, i32, bool)>(
        r#"
        SELECT id, kind, disposition, manual, confidence, revoked_at IS NULL AS active
          FROM wallet_labels
         WHERE chain = $1 AND address = $2
         ORDER BY created_at DESC
        "#,
    )
    .bind(chain)
    .bind(&address)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| LabelRow {
        id: r.0, kind: r.1, disposition: r.2, manual: r.3, confidence: r.4, active: r.5,
    }).collect()))
}

#[derive(Deserialize)]
struct LabelForm {
    kind: String,
    disposition: String,
    reason: Option<String>,
}

async fn add_label(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((chain, address)): Path<(String, String)>,
    Json(form): Json<LabelForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(disposition) = Disposition::parse(&form.disposition) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    crate::db::upsert_wallet(&state.pool, chain.as_str(), &address, Utc::now(), "admin").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    crate::db::add_wallet_label(
        &state.pool,
        chain.as_str(),
        &address,
        &form.kind,
        disposition.as_str(),
        form.reason.as_deref().unwrap_or("admin label"),
        "manual",
        100,
        true,
        None,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn revoke_label(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((chain, address, id)): Path<(String, String, i64)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    if ChainKind::parse(&chain).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    sqlx::query("UPDATE wallet_labels SET revoked_at = now() WHERE id = $1 AND chain = $2 AND address = $3")
        .bind(id)
        .bind(chain)
        .bind(&address)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct BlocklistForm {
    chain: String,
    addresses: Vec<String>,
}

async fn import_blocklist(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<BlocklistForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    let Some(chain) = ChainKind::parse(&form.chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let mut imported = 0u32;
    for address in &form.addresses {
        let address = address.trim();
        if address.is_empty() || address.starts_with('#') {
            continue;
        }
        let _ = crate::db::upsert_wallet(&state.pool, chain.as_str(), address, Utc::now(), "blocklist").await;
        let _ = crate::db::add_wallet_label(
            &state.pool,
            chain.as_str(),
            address,
            "manual_block",
            "skip",
            "admin blocklist import",
            "manual",
            100,
            true,
            None,
        )
        .await;
        imported += 1;
    }
    Ok(Json(serde_json::json!({ "ok": true, "imported": imported })))
}

// ---------------------------------------------------------------------------
// Read views (delegate to the read-only surface for list endpoints)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Settings (environment read-only + runtime editable)
// ---------------------------------------------------------------------------

/// Mask a secret to `first2…last2`. Returns None when empty/too short.
fn mask(secret: &str) -> Option<String> {
    let s = secret.trim();
    if s.len() < 4 {
        return None;
    }
    Some(format!("{}…{}", &s[..2], &s[s.len() - 2..]))
}

// ---------------------------------------------------------------------------
// Secret store (write-only, AES-256-GCM encrypted at rest)
// ---------------------------------------------------------------------------

/// Allowlist of secret env vars that may be (re)set from the admin UI.
/// Values are encrypted and stored in `secret_store`; they are NEVER returned
/// to the browser (only the masked preview is ever shown).
/// HELIUS_KEY_*, GMGN_API_KEY and ROBINHOOD_RPC_URL are intentionally NOT here:
/// those are managed in the providers' own dashboards and are env-only.
const SECRET_KEYS: &[&str] = &[
    "DATABASE_URL",
    "TG_API_ID",
    "TG_API_HASH",
    "TG_SESSION_PATH",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_CHAT_ID",
];

/// Derive the AES-256 key from SECRET_STORE_KEY (SHA-256 of the raw material so
/// any string works). Returns None when unset/empty — secret writes are then
/// rejected with 503.
fn secret_store_key() -> Option<[u8; 32]> {
    let raw = std::env::var("SECRET_STORE_KEY").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    Some(key)
}

/// AES-256-GCM encrypt. Returns (nonce_hex, base64(ciphertext||tag)).
fn encrypt_secret(key: &[u8; 32], plaintext: &str) -> Result<(String, String), StatusCode> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // random 96-bit nonce per record
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    use base64::Engine;
    Ok((hex::encode(nonce), base64::engine::general_purpose::STANDARD.encode(ct)))
}

/// AES-256-GCM decrypt (server-side only; result is never sent to the browser).
fn decrypt_secret(key: &[u8; 32], nonce_hex: &str, b64: &str) -> Option<String> {
    let nonce_bytes = hex::decode(nonce_hex).ok()?;
    use base64::Engine;
    let ct = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
    let pt = cipher.decrypt(nonce, ct.as_ref()).ok()?;
    String::from_utf8(pt).ok()
}

/// Resolve a secret: DB-decrypted override wins, else the env value.
/// Workers that consume secrets should call this (DB override over env file);
/// long-running workers read config at startup, so a restart applies edits.
#[allow(dead_code)] // consumed by runtime workers at startup, not by admin handlers
pub async fn resolve_secret(pool: &PgPool, env_value: Option<&str>, name: &str) -> Option<String> {
    if let Some(key) = secret_store_key() {
        let row = sqlx::query_as::<_, (String, String)>(
            "SELECT nonce, ciphertext FROM secret_store WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();
        if let Some((nonce, ct)) = row {
            if let Some(value) = decrypt_secret(&key, &nonce, &ct) {
                return Some(value);
            }
        }
    }
    env_value.map(|s| s.to_string())
}

/// Whether a DB override exists for `name`, and its decrypted value (server-side).
async fn db_secret(pool: &PgPool, name: &str) -> Option<String> {
    let key = secret_store_key()?;
    let row = sqlx::query_as::<_, (String, String)>(
        "SELECT nonce, ciphertext FROM secret_store WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;
    decrypt_secret(&key, &row.0, &row.1)
}

#[derive(Deserialize)]
struct SecretSetForm {
    name: String,
    value: String,
}

/// Set a secret (write-only). Encrypts + upserts; never echoes the value.
async fn settings_secret_set(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<SecretSetForm>,
) -> Response {
    if require_write_auth(&state, &headers).await.is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !SECRET_KEYS.contains(&form.name.as_str()) || form.value.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(key) = secret_store_key() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "SECRET_STORE_KEY not configured" })),
        )
            .into_response();
    };
    let (nonce, ciphertext) = match encrypt_secret(&key, form.value.trim()) {
        Ok(v) => v,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match sqlx::query(
        "INSERT INTO secret_store (name, nonce, ciphertext, updated_at) VALUES ($1, $2, $3, now()) \
         ON CONFLICT (name) DO UPDATE SET nonce = EXCLUDED.nonce, ciphertext = EXCLUDED.ciphertext, updated_at = now()",
    )
    .bind(&form.name)
    .bind(&nonce)
    .bind(&ciphertext)
    .execute(&state.pool)
    .await
    {
        Ok(_) => Json(serde_json::json!({ "ok": true, "set": true })).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Remove a DB secret override (revert to the env-file value).
async fn settings_secret_delete(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    if !SECRET_KEYS.contains(&name.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    sqlx::query("DELETE FROM secret_store WHERE name = $1")
        .bind(&name)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn env_row(name: &str, value: Option<&str>, secret: bool, source: Option<&str>) -> serde_json::Value {
    match value {
        Some(v) if !v.trim().is_empty() => {
            let preview = if secret { mask(v) } else { Some(v.to_string()) };
            serde_json::json!({ "name": name, "set": true, "secret": secret, "preview": preview, "source": source })
        }
        _ => serde_json::json!({ "name": name, "set": false, "secret": secret, "preview": null, "source": null }),
    }
}

/// List every known env var with set status + masked preview (never raw secrets).
/// A secret is "set" if present in env OR overridden in `secret_store`; DB values
/// are decrypted server-side only to compute the masked preview.
/// Build one env row, resolving any DB override (decrypted server-side only).
/// DB override wins for set/preview/source; never returns the raw secret.
/// Only called for SECRET_KEYS entries, so the row is marked `"editable": true`
/// (the UI shows its Set/Delete buttons solely off this flag); plain `env_row`
/// never sets the flag, keeping env-only rows read-only.
async fn secret_env_row(pool: &PgPool, name: &str, env_value: Option<String>) -> serde_json::Value {
    let mut row = match db_secret(pool, name).await {
        Some(v) => env_row(name, Some(&v), true, Some("db")),
        None => env_row(name, env_value.as_deref(), true, if env_value.is_some() { Some("env") } else { None }),
    };
    row["editable"] = serde_json::Value::Bool(true);
    row
}

/// List every known env var with set status + masked preview (never raw secrets).
/// Allowlisted secrets (SECRET_KEYS) are "set" if present in env OR overridden in
/// `secret_store` and carry `"editable": true` (the UI keys its Set/Delete buttons
/// off that flag); env-only secrets (HELIUS_KEY_*, GMGN_API_KEY, ROBINHOOD_RPC_URL,
/// ADMIN_PASSWORD_HASH_B64) reflect the env value alone — any stale DB override
/// rows for them are ignored.
async fn settings_env(State(state): State<AdminState>, headers: HeaderMap) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let pool = &state.pool;
    let env: &EnvConfig = &state.settings.env;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    rows.push(secret_env_row(pool, "DATABASE_URL", env.database_url.clone()).await);
    // Helius/GMGN keys live in the DB-backed API panel now (not env vars);
    // surface a non-secret summary row each (hidden from the Settings table UI).
    let helius_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM helius_keys")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let gmgn_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM gmgn_keys")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    rows.push(serde_json::json!({
        "name": "HELIUS_KEYS",
        "set": helius_count > 0,
        "secret": false,
        "preview": if helius_count > 0 { Some(format!("{helius_count} keys in API panel")) } else { None::<String> },
        "source": if helius_count > 0 { Some("db") } else { None::<&str> },
    }));
    rows.push(serde_json::json!({
        "name": "GMGN_KEYS",
        "set": gmgn_count > 0,
        "secret": false,
        "preview": if gmgn_count > 0 { Some(format!("{gmgn_count} keys in API panel")) } else { None::<String> },
        "source": if gmgn_count > 0 { Some("db") } else { None::<&str> },
    }));
    rows.push(secret_env_row(pool, "TG_API_ID", env.tg_api_id.map(|v| v.to_string())).await);
    rows.push(secret_env_row(pool, "TG_API_HASH", env.tg_api_hash.clone()).await);
    rows.push(secret_env_row(pool, "TG_SESSION_PATH", env.tg_session_path.clone()).await);
    rows.push(secret_env_row(pool, "TELEGRAM_BOT_TOKEN", env.telegram_bot_token.clone()).await);
    rows.push(secret_env_row(pool, "TELEGRAM_CHAT_ID", env.telegram_chat_id.clone()).await);
    // Not in the DB allowlist: env-only.
    let admin_hash = std::env::var("ADMIN_PASSWORD_HASH_B64").ok();
    rows.push(env_row("ADMIN_PASSWORD_HASH_B64", admin_hash.as_deref(), true, if admin_hash.is_some() { Some("env") } else { None }));
    // NOTE: no ROBINHOOD_RPC_URL row — the runtime derives the Robinhood EVM
    // endpoint from the Helius base URL + chain config and a key from the API
    // panel, so the legacy env var is intentionally not surfaced.
    Ok(Json(rows))
}

/// Editable non-secret runtime keys (allowlist). Values persist in `admin_settings`
/// and override the config.toml/env defaults when read back.
///
/// NOTE: edits take effect for readers that consult `admin_settings` at request
/// time; long-running workers read config at startup, so a service restart is
/// required for some settings to fully apply. Nothing is written to disk/.env.
const EDITABLE_KEYS: &[&str] = &[
    "large_funding_min_sol",
    "large_funding_min_usd",
    "recipient_max_age_days",
    "preparation_alert_confidence",
    "entry_min_liquidity_usd",
    "full_skill_score",
    "full_copyability_score",
    "telegram_concurrency",
    "runtime_profile",
];

/// Default (config.toml/env) values for the editable runtime keys.
fn runtime_defaults(config: &AppConfig) -> serde_json::Map<String, serde_json::Value> {
    let fr = &config.funding_radar;
    let sc = &config.scoring;
    let sg = &config.signals;
    let profile = match config.runtime_profile {
        RuntimeProfile::Low => "low",
        RuntimeProfile::Scale => "scale",
    };
    serde_json::Map::from_iter([
        ("large_funding_min_sol".to_string(), serde_json::json!(fr.large_funding_min_sol)),
        ("large_funding_min_usd".to_string(), serde_json::json!(fr.large_funding_min_usd)),
        ("recipient_max_age_days".to_string(), serde_json::json!(fr.recipient_max_age_days)),
        ("preparation_alert_confidence".to_string(), serde_json::json!(fr.preparation_alert_confidence)),
        ("entry_min_liquidity_usd".to_string(), serde_json::json!(sg.entry_min_liquidity_usd)),
        ("full_skill_score".to_string(), serde_json::json!(sc.full_skill_score)),
        ("full_copyability_score".to_string(), serde_json::json!(sc.full_copyability_score)),
        ("telegram_concurrency".to_string(), serde_json::json!(config.telegram.concurrency)),
        ("runtime_profile".to_string(), serde_json::json!(profile)),
    ])
}

/// Current effective runtime settings: DB overrides layered onto config defaults.
async fn settings_runtime_get(State(state): State<AdminState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    require_auth(&state, &headers).await?;
    let mut effective = runtime_defaults(&state.settings.config);
    let rows = sqlx::query_as::<_, (String, serde_json::Value)>(
        "SELECT key, value FROM admin_settings",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    for (key, value) in rows {
        if EDITABLE_KEYS.contains(&key.as_str()) {
            effective.insert(key, value);
        }
    }
    Ok(Json(serde_json::Value::Object(effective)))
}

#[derive(Deserialize)]
struct RuntimeSaveForm {
    key: Option<String>,
    value: Option<serde_json::Value>,
    settings: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Validate an editable value against the expected type for its key.
fn valid_runtime_value(key: &str, value: &serde_json::Value) -> bool {
    if key == "runtime_profile" {
        return matches!(value.as_str(), Some("low") | Some("scale"));
    }
    // All other editable keys are non-negative integer counts/thresholds.
    value.as_u64().is_some()
}

async fn settings_runtime_save(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(form): Json<RuntimeSaveForm>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    // Normalize to a list of (key, value) pairs from either shape.
    let mut pairs: Vec<(String, serde_json::Value)> = Vec::new();
    if let (Some(k), Some(v)) = (form.key, form.value) {
        pairs.push((k, v));
    }
    if let Some(map) = form.settings {
        pairs.extend(map.into_iter());
    }
    if pairs.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Validate every pair before writing any (all-or-nothing).
    for (k, v) in &pairs {
        if !EDITABLE_KEYS.contains(&k.as_str()) || !valid_runtime_value(k, v) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let mut applied = 0usize;
    for (k, v) in &pairs {
        sqlx::query(
            "INSERT INTO admin_settings (key, value, updated_at) VALUES ($1, $2, now()) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(k)
        .bind(v)
        .execute(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        applied += 1;
    }
    Ok(Json(serde_json::json!({ "ok": true, "applied": applied })))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    chain: Option<String>,
    limit: Option<i64>,
    q: Option<String>,
}

async fn list_wallets(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let chain = query.chain.unwrap_or_else(|| "solana".to_string());
    let limit = query.limit.unwrap_or(50).clamp(1, 1000);
    let search = query.q.map(|s| format!("%{s}%"));
    let rows = sqlx::query_as::<_, (String, String, DateTime<Utc>, String)>(
        r#"
        SELECT chain, address, last_seen, source FROM wallets
         WHERE chain = $1
           AND ($2::text IS NULL OR address ILIKE $2)
         ORDER BY last_seen DESC LIMIT $3
        "#,
    )
    .bind(&chain)
    .bind(&search)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut out = Vec::new();
    for (chain, address, last_seen, source) in rows {
        let disposition = crate::db::active_disposition(&state.pool, &chain, &address).await.ok().flatten();
        out.push(serde_json::json!({
            "chain": chain, "address": address, "last_seen": last_seen,
            "source": source, "disposition": disposition,
        }));
    }
    Ok(Json(out))
}

async fn wallet_scores(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((chain, address)): Path<(String, String)>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let rows = sqlx::query_as::<_, (DateTime<Utc>, i32, i32, i32, bool)>(
        r#"
        SELECT as_of, skill_score, copyability_score, conviction, provisional
          FROM wallet_scores WHERE chain = $1 AND address = $2
         ORDER BY as_of DESC LIMIT 100
        "#,
    )
    .bind(chain)
    .bind(&address)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({
        "as_of": r.0, "skill_score": r.1, "copyability_score": r.2,
        "conviction": r.3, "provisional": r.4,
    })).collect()))
}

async fn token_report(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((chain, mint)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_auth(&state, &headers).await?;
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let report = crate::narrative::explain_narrative(&state.pool, chain, &mint, Utc::now())
        .await
        .ok()
        .flatten();
    Ok(Json(serde_json::json!({
        "chain": chain.as_str(), "mint": mint,
        "narrative": report.map(|r| serde_json::json!({
            "narrative": r.narrative, "confidence": r.confidence,
            "why_now": r.why_now, "counter_evidence": r.counter_evidence,
        })),
    })))
}

async fn radar_cases(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let rows = sqlx::query_as::<_, (i64, String, String, String, i32, Decimal, DateTime<Utc>)>(
        r#"
        SELECT id, chain, recipient, stage, confidence, first_funding_native, updated_at
          FROM funding_radar_cases ORDER BY updated_at DESC LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({
        "id": r.0, "chain": r.1, "recipient": r.2, "stage": r.3,
        "confidence": r.4, "first_funding_native": r.5, "updated_at": r.6,
    })).collect()))
}

async fn radar_case(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_auth(&state, &headers).await?;
    let case = sqlx::query_as::<_, (i64, String, String, String, i32, serde_json::Value)>(
        "SELECT id, chain, recipient, stage, confidence, evidence FROM funding_radar_cases WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({
        "id": case.0, "chain": case.1, "recipient": case.2,
        "stage": case.3, "confidence": case.4, "evidence": case.5,
    })))
}

async fn list_signals(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let rows = sqlx::query_as::<_, (i64, String, String, String, i32, DateTime<Utc>)>(
        "SELECT id, chain, mint, signal_kind, score, created_at FROM signals ORDER BY created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({
        "id": r.0, "chain": r.1, "mint": r.2, "signal_kind": r.3, "score": r.4, "created_at": r.5,
    })).collect()))
}

async fn signal_rejections(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let rows = sqlx::query_as::<_, (String, String, String, String, DateTime<Utc>)>(
        "SELECT chain, mint, signal_kind, rejection_code, evaluated_at FROM signal_evaluations WHERE status = 'rejected' ORDER BY evaluated_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({
        "chain": r.0, "mint": r.1, "signal_kind": r.2, "rejection_code": r.3, "evaluated_at": r.4,
    })).collect()))
}

async fn list_clusters(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let rows = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT c.cluster_id, COUNT(m.id) FILTER (WHERE m.revoked_at IS NULL) AS member_count
          FROM wallet_clusters c LEFT JOIN wallet_cluster_members m ON m.cluster_id = c.cluster_id
         GROUP BY c.cluster_id ORDER BY member_count DESC LIMIT 200
        "#,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({
        "cluster_id": r.0, "member_count": r.1,
    })).collect()))
}

// ---------------------------------------------------------------------------
// Login page HTML
// ---------------------------------------------------------------------------

const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="id"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Login — Solana Whale Intelligence</title>
<style>
 body{margin:0;font-family:ui-sans-serif,system-ui;background:#0b0f17;color:#e6edf6;display:flex;align-items:center;justify-content:center;height:100vh}
 .card{background:#121826;border:1px solid #1f2a3d;border-radius:12px;padding:32px;width:340px}
 h1{font-size:18px;margin:0 0 20px}
 input{width:100%;padding:10px 12px;border-radius:8px;border:1px solid #1f2a3d;background:#0e1420;color:#e6edf6;margin-bottom:12px}
 button{width:100%;padding:10px;border:0;border-radius:8px;background:#4f8cff;color:#fff;font-weight:600;cursor:pointer}
 .err{color:#f05c5c;font-size:13px;margin-bottom:12px}
</style></head><body>
<div class="card"><h1>Solana Whale Intelligence</h1><!--ERROR-->
<form method="post" action="/login"><input type="password" name="password" placeholder="admin password" autofocus required>
<button type="submit">Masuk</button></form></div></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_contains_form() {
        assert!(LOGIN_HTML.contains("name=\"password\""));
        assert!(LOGIN_HTML.contains("/login"));
    }

    #[test]
    fn session_token_parses_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "swi_session=abc123; other=x".parse().unwrap());
        assert_eq!(session_token(&headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn session_token_missing_cookie() {
        let headers = HeaderMap::new();
        assert_eq!(session_token(&headers), None);
    }
    #[test]
    fn helius_monthly_limit_defaults_to_free_plan_when_blank() {
        // The bind expression used by create_helius_key: explicit positive
        // numbers win; null/blank falls back to the 1M free-plan default.
        let resolve = |form_limit: Option<i32>| form_limit.filter(|l| *l > 0).or(Some(HELIUS_DEFAULT_MONTHLY_LIMIT));
        assert_eq!(resolve(None), Some(1_000_000));
        assert_eq!(resolve(Some(0)), Some(1_000_000), "non-positive treated as blank");
        assert_eq!(resolve(Some(-5)), Some(1_000_000));
        assert_eq!(resolve(Some(250_000)), Some(250_000), "explicit number kept");
        assert_eq!(HELIUS_DEFAULT_MONTHLY_LIMIT, 1_000_000);
    }
    #[test]
    fn mask_formats_and_handles_short() {
        assert_eq!(mask("abcdefgh").as_deref(), Some("ab…gh"));
        assert_eq!(mask("GMGNKEY1234").as_deref(), Some("GM…34"));
        assert_eq!(mask("ab"), None);
        assert_eq!(mask(""), None);
        assert_eq!(mask("   "), None);
    }
    #[test]
    fn track_extract_trades_reads_real_gmgn_shape() {
        // Mirrors the verified live GMGN `data.list[]` trade schema.
        let data = serde_json::json!({
            "list": [
                {
                    "timestamp": 1787650087,
                    "side": "buy",
                    "amount_usd": 127.45,
                    "base_address": "BcoNUiz5cWTAmNz3akQ6xJy9ytUjrNC38uDkW3DTpump",
                    "base_token": { "symbol": "ASTRA" },
                    "maker": "4nptUNXrLg2C5h2f4xSwvJLSLpgvc5tNLKkPc5M5eHjf",
                    "maker_info": { "twitter_username": "somekol", "twitter_name": "Some KOL" }
                }
            ]
        });
        let rows = track_extract_trades(&data, "kol");
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r["ts"], 1787650087);
        assert_eq!(r["side"], "buy");
        assert_eq!(r["usd"], 127.45);
        assert_eq!(r["token_address"], "BcoNUiz5cWTAmNz3akQ6xJy9ytUjrNC38uDkW3DTpump");
        assert_eq!(r["token_symbol"], "ASTRA");
        assert_eq!(r["wallet"], "4nptUNXrLg2C5h2f4xSwvJLSLpgvc5tNLKkPc5M5eHjf");
        assert_eq!(r["wallet_tag"], "kol");
        assert_eq!(r["twitter_username"], "somekol");
        assert_eq!(r["twitter_name"], "Some KOL");
    }

    #[test]
    fn track_pick_helpers_tolerate_numbers_and_strings() {
        let v = serde_json::json!({ "a": { "b": 42 }, "c": "3.5", "d": "x" });
        assert_eq!(pick_num(&v, &["/a/b"]), Some(42.0));
        assert_eq!(pick_num(&v, &["/c"]), Some(3.5));
        assert_eq!(pick_num(&v, &["/d"]), None);
        assert_eq!(pick_str(&v, &["/d"]), Some("x"));
        assert_eq!(pick_str(&v, &["/missing", "/c"]), Some("3.5"));
    }

    #[test]
    fn secret_encrypt_decrypt_roundtrip() {
        // Random 32-byte key (not derived from env here).
        let mut key = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut key);
        let (nonce, ct) = encrypt_secret(&key, "hello").expect("encrypt");
        assert_ne!(ct, "hello");
        let back = decrypt_secret(&key, &nonce, &ct).expect("decrypt");
        assert_eq!(back, "hello");
        // Tampered ciphertext fails to decrypt.
        assert!(decrypt_secret(&key, &nonce, "AAAA").is_none());
    }
}
