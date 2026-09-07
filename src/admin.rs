//! Fully functional admin panel: password auth, API-key pools, Telegram channel
//! management, wallet labels/blocklist, and dashboard UI.
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
    match auth::validate_session(&state.pool, &token).await {
        Ok(true) => Ok(()),
        // `Ok(false)` is a real invalid/expired session -> 401.
        Ok(false) => Err(StatusCode::UNAUTHORIZED),
        // A DB failure is not "bad session": surface it as 500 (REV-060-F06).
        Err(e) => {
            tracing::error!(
                error = %e,
                "session validation failed; refusing to report a session as invalid"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
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
        // RPC providers: REMOVED. Migration 0006_drop_rpc_providers.sql dropped the
        // `rpc_providers` table (superseded by the API-keys panel: helius_keys /
        // gmgn_keys). The routes are removed with the table (REV-028-F01).
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
        // Recent intelligence: the admin router serves the Token Recent dashboard
        // (`serve_dashboard` returns the same HTML as the read API), but did not
        // expose the endpoints that page fetches, so both panels always rendered
        // an error (REV-027-F09). Registered here against the same handlers the
        // read API uses, so workspace-from-session authorization is identical.
        .route("/api/tokens/{chain}/{mint}/recent", get(token_recent))
        .route("/api/tokens/{chain}/{mint}/relations", get(token_relations))
        .route("/api/funding/radar/cases", get(radar_cases))
        .route("/api/funding/radar/cases/{id}", get(radar_case))
        .route("/api/signals", get(list_signals))
        .route("/api/signals/rejections", get(signal_rejections))
        .route("/api/clusters", get(list_clusters))
        // Queue backpressure control (REV-076-F02: the durable authority workers read)
        .route("/api/queues", get(list_queue_state))
        .route("/api/queues/{name}/pause", post(set_queue_pause))
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
        // REV-062-F07: `validate_session(...).unwrap_or(false)` treated a DB error
        // as an invalid session — an outage or a missing table became a redirect to
        // /login instead of a 500. `Ok(false)` is a genuine bad/expired session
        // (client-side -> 401); `Err` is a server-side failure and must surface as
        // 500 so an operator sees the real cause, not a flood of 401s.
        let ok = match session_token(&headers) {
            Some(token) => match auth::validate_session(&state.pool, &token).await {
                Ok(true) => true,
                Ok(false) => false,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "session validation failed on the dashboard; refusing to call the session invalid"
                    );
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            },
            None => false,
        };
        if !ok {
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
        // REV-064-F07: a failed attempt that is not RECORDED is a rate limiter that
        // does not exist — the audit row is what `is_rate_limited` counts, so
        // swallowing this error turns brute-force protection off silently. Fail closed.
        if auth::record_attempt(&state.pool, &key, false, Some(ip), ua).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        return Html(LOGIN_HTML.replace("<!--ERROR-->", r#"<div class="err">password salah</div>"#)).into_response();
    }
    // REV-064-F07: a store failure on the success path must not hand out a session.
    //
    // REV-067-F07: the three successful-login writes (audit the success, clear the
    // failure counter, insert the session) are ONE transaction. As three separate
    // statements a failure in the second or third returned 500 with no cookie —
    // correct as a response — while the earlier writes stayed committed, leaving an
    // audit trail and counter describing a login the client never got. All or none.
    match auth::establish_session(&state.pool, &key, Some(ip), ua).await {
        Ok(token) => {
            let cookie = auth::session_cookie_value(&token);
            let mut response = Redirect::to("/").into_response();
            response.headers_mut().insert(header::SET_COOKIE, cookie.parse().unwrap());
            response
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// REV-064-F07: logout must not answer "logged out" when the server-side session
/// still exists. It used to discard the `destroy_session` error, clear the cookie
/// and redirect — the browser looked logged out while the token stayed valid for
/// anyone holding it. A store failure is now a 500 and the cookie is left alone,
/// so the client's state never claims more than the server did.
async fn logout(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        if auth::destroy_session(&state.pool, &token).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
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

async fn auth_status(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<AuthStatus>, StatusCode> {
    let configured = auth::auth_configured();
    let authenticated = if configured {
        match session_token(&headers) {
            Some(token) => match auth::validate_session(&state.pool, &token).await {
                // REV-062-F07: a DB error during validation is a server-side failure,
                // never "authenticated: false" — which would read as a logged-out
                // session and hide the actual outage. Surface it as 500.
                Ok(true) => true,
                Ok(false) => false,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "session validation failed in auth_status; refusing to report not-authenticated"
                    );
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
            },
            None => false,
        }
    } else {
        true
    };
    Ok(Json(AuthStatus {
        configured,
        authenticated,
    }))
}
async fn api_health(State(state): State<AdminState>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    require_auth(&state, &headers).await?;
    let latency = crate::db::latency_ms(&state.pool).await.unwrap_or(-1.0);
    let radar = crate::health::radar_stage_counts(&state.pool).await.unwrap_or_default();
    let telegram = crate::health::telegram_health(&state.pool).await.unwrap_or_default();
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

/// REV-072-F06 (HIGH): accepted/rejected counts are workspace-owned policy outcomes.
async fn metrics_signals_timeline(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<MetricsQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let workspace_id = require_workspace(&state, &headers).await?;
    let hours = clamp_hours(query.hours, 24);
    let rows = sqlx::query_as::<_, (DateTime<Utc>, i64, i64)>(
        r#"
        SELECT date_trunc('hour', evaluated_at) AS t,
               COUNT(*) FILTER (WHERE status = 'accepted') AS accepted,
               COUNT(*) FILTER (WHERE status = 'rejected') AS rejected
          FROM signal_evaluations
         WHERE workspace_id = $1
           AND evaluated_at > now() - ($2 || ' hours')::interval
         GROUP BY t ORDER BY t
        "#,
    )
    .bind(workspace_id)
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

/// REV-072-F06: `signals_24h` is a workspace-owned count; the rest of this overview
/// is infrastructure-wide and stays unscoped because those tables carry no tenancy.
async fn metrics_overview(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let workspace_id = require_workspace(&state, &headers).await?;
    let wallets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallets")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tokens")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let clusters: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallet_clusters")
        .fetch_one(&state.pool).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let signals_24h: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM signals \
          WHERE workspace_id = $1 AND created_at > now() - interval '24 hours'",
    )
        .bind(workspace_id)
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
// RPC providers CRUD — REMOVED (REV-028-F01).
//
// Migration 0006_drop_rpc_providers.sql dropped the `rpc_providers` table and
// its comment states "its UI and routes are removed in the same change" — but
// the handlers were left behind, so every one of them (plus `api_health`, which
// counted enabled providers) raised `relation "rpc_providers" does not exist`
// on any migrated database. The feature is superseded by the API-keys panel
// (`helius_keys` / `gmgn_keys`, migration 0005). Do not reintroduce these
// routes without a forward migration recreating the table.
// ---------------------------------------------------------------------------

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
    // REV-056-F01 (HIGH): the workspace comes from the authenticated session, and the
    // query filters on it. Without this filter the reviewer read workspace A's private
    // label from a workspace B session over real HTTP.
    let workspace_id = require_workspace(&state, &headers).await?;
    // REV-053-F03: `wallet_labels.chain` stores the canonical spelling, so binding the
    // raw path segment made `/sol/<address>/labels` return an empty list for a wallet
    // that has labels under `solana`.
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let rows = sqlx::query_as::<_, (i64, String, String, bool, i32, bool)>(
        r#"
        SELECT id, kind, disposition, manual, confidence,
               -- REV-058-F02 (same class): an EXPIRED label is not active. Reporting
               -- `revoked_at IS NULL` showed it as active while the policy queries had
               -- already stopped honouring it.
               (revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())) AS active
          FROM wallet_labels
         WHERE workspace_id = $1 AND chain = $2 AND address = $3
         ORDER BY created_at DESC
        "#,
    )
    .bind(workspace_id)
    .bind(chain.as_str())
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
    // REV-056-F01: the label is OWNED by the session's workspace.
    let workspace_id = require_workspace(&state, &headers).await?;
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
        workspace_id,
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
    // REV-056-F01 (HIGH): a revoke is a DESTRUCTIVE cross-tenant action. The reviewer
    // revoked workspace A's label from a workspace B session and the DB confirmed it.
    // The workspace now participates in the `WHERE`, so another tenant's row is simply
    // not addressable.
    let workspace_id = require_workspace(&state, &headers).await?;
    // REV-053-F03: this is a MUTATION whose `WHERE` bound the raw path segment, so
    // `/sol/<address>/labels/<id>/revoke` matched no row and still returned
    // `{"ok": true}` — an operator was told the label was revoked while it stayed
    // active.
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    // REV-056-F03: `AND revoked_at IS NULL` restricts this to ACTIVE rows.
    //
    // Without it a replay matched the already-revoked row, overwrote `revoked_at` with
    // a fresh `now()`, and reported `revoked=1` a second time — destroying the original
    // revocation timestamp (audit evidence) and claiming work that did not happen.
    let result = sqlx::query(
        "UPDATE wallet_labels SET revoked_at = now() \
          WHERE id = $1 AND workspace_id = $2 AND chain = $3 AND address = $4 \
            AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(chain.as_str())
    .bind(&address)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if result.rows_affected() == 0 {
        // Distinguish "already revoked" from "no such label": the first is an
        // idempotent no-op the caller can safely ignore, the second is a real 404. A
        // single answer for both is what made the replay look like fresh work.
        let already: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM wallet_labels \
              WHERE id = $1 AND workspace_id = $2 AND chain = $3 AND address = $4 \
                AND revoked_at IS NOT NULL)",
        )
        .bind(id)
        .bind(workspace_id)
        .bind(chain.as_str())
        .bind(&address)
        .fetch_one(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if already {
            return Ok(Json(serde_json::json!({
                "ok": true, "revoked": 0, "already_revoked": true
            })));
        }
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(serde_json::json!({
        "ok": true, "revoked": result.rows_affected(), "already_revoked": false
    })))
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
    // REV-056-F01: imported blocks belong to the importing workspace.
    let workspace_id = require_workspace(&state, &headers).await?;
    let Some(chain) = ChainKind::parse(&form.chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };

    // REV-056-F05 / REV-058-F03: ONE transactional implementation, shared with the CLI
    // (`db::import_blocklist_tx`). A blocklist that is half-applied is not a blocklist,
    // and having two copies of that rule is how the CLI stayed non-transactional after
    // this handler was fixed.
    let imported = crate::db::import_blocklist_tx(
        &state.pool,
        workspace_id,
        chain.as_str(),
        &form.addresses,
        "admin blocklist import",
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "blocklist import failed; nothing was committed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(serde_json::json!({ "ok": true, "imported": imported })))
}

// ---------------------------------------------------------------------------
// Read views (delegate to the read-only surface for list endpoints)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Queue backpressure control (REV-076-F02)
// ---------------------------------------------------------------------------

/// REV-076-F02: pause/lag was process-local memory with no writer — the gate
/// worked but nothing operational could ever pause anything, so "backpressure"
/// was a claim, not a mechanism. `queue_state` is the durable authority: workers
/// read it in `queue_allowed`, and these endpoints write it under write-auth with
/// an audit row. Unknown queue names are rejected against the frozen vocabulary
/// rather than silently creating a row no worker will ever read.
async fn list_queue_state(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let rows = sqlx::query_as::<_, (String, bool, i64, DateTime<Utc>, String)>(
        "SELECT queue, paused, lag_seconds, updated_at, updated_by FROM queue_state ORDER BY queue",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({
        "queue": r.0, "paused": r.1, "lag_seconds": r.2, "updated_at": r.3, "updated_by": r.4,
    })).collect()))
}

#[derive(Deserialize)]
struct QueuePauseBody {
    paused: bool,
}

async fn set_queue_pause(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<QueuePauseBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_write_auth(&state, &headers).await?;
    if !crate::queues::ALL_QUEUES.contains(&name.as_str()) {
        return Err(StatusCode::NOT_FOUND);
    }
    sqlx::query(
        "INSERT INTO queue_state (queue, paused, updated_at, updated_by) \
         VALUES ($1, $2, now(), 'admin') \
         ON CONFLICT (queue) DO UPDATE SET paused = $2, updated_at = now(), updated_by = 'admin'",
    )
    .bind(&name)
    .bind(body.paused)
    .execute(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "queue": name, "paused": body.paused })))
}

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
    // REV-067-F07: `is_err() -> 401` collapsed every auth outcome into "bad
    // session", so a validation-store failure (which `require_auth` deliberately
    // maps to 500) was reported as unauthorized. The operator then sees 401s
    // instead of the outage, and the taxonomy this codebase already enforces
    // elsewhere is broken at exactly one route. Propagate the exact status.
    if let Err(status) = require_write_auth(&state, &headers).await {
        return status.into_response();
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
    // HELIUS_KEY_1..N: env-only (managed in the Helius dashboard, not editable here).
    for (i, key) in env.helius_keys.iter().enumerate() {
        rows.push(env_row(&format!("HELIUS_KEY_{}", i + 1), Some(key), true, Some("env")));
    }
    // GMGN key: env-only (managed in the GMGN panel, not editable here).
    rows.push(env_row("GMGN_API_KEY", env.gmgn_api_key.as_deref(), true, if env.gmgn_api_key.is_some() { Some("env") } else { None }));
    rows.push(secret_env_row(pool, "TG_API_ID", env.tg_api_id.map(|v| v.to_string())).await);
    rows.push(secret_env_row(pool, "TG_API_HASH", env.tg_api_hash.clone()).await);
    rows.push(secret_env_row(pool, "TG_SESSION_PATH", env.tg_session_path.clone()).await);
    rows.push(secret_env_row(pool, "TELEGRAM_BOT_TOKEN", env.telegram_bot_token.clone()).await);
    rows.push(secret_env_row(pool, "TELEGRAM_CHAT_ID", env.telegram_chat_id.clone()).await);
    // Not in the DB allowlist: env-only.
    let admin_hash = std::env::var("ADMIN_PASSWORD_HASH_B64").ok();
    rows.push(env_row("ADMIN_PASSWORD_HASH_B64", admin_hash.as_deref(), true, if admin_hash.is_some() { Some("env") } else { None }));
    // Robinhood RPC URL: env-only (not editable here). Read straight from the
    // environment — EnvConfig no longer carries it (runtime derives the
    // Robinhood endpoint from the Helius EVM URL + chain config), but the var
    // may still be present in .env and should stay visible here.
    let rh_url = std::env::var("ROBINHOOD_RPC_URL").ok().filter(|v| !v.trim().is_empty());
    rows.push(env_row("ROBINHOOD_RPC_URL", rh_url.as_deref(), true, if rh_url.is_some() { Some("env") } else { None }));
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
    let workspace_id = require_workspace(&state, &headers).await?;
    // REV-056-F04: this bound the raw query value with NO parsing at all, so
    // `?chain=sol` was a false-empty page and `?chain=anything` silently returned
    // nothing rather than 400.
    let chain = match query.chain.as_deref() {
        None => ChainKind::Solana,
        Some(raw) => ChainKind::parse(raw).ok_or(StatusCode::BAD_REQUEST)?,
    };
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
    .bind(chain.as_str())
    .bind(&search)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut out = Vec::new();
    for (chain, address, last_seen, source) in rows {
        // REV-056-F01: workspace-owned labels only.
        //
        // REV-058-F07: a failed lookup must not be reported as "no disposition". A
        // safety classification that disappears because of a DB error is worse than an
        // error page, because nothing tells the operator it disappeared.
        let disposition =
            crate::db::active_disposition(&state.pool, workspace_id, &chain, &address)
                .await
                .map_err(|e| {
                    tracing::error!(
                        address = %crate::models::short_addr(&address),
                        error = %e,
                        "active-disposition lookup failed; refusing to report the wallet \
                         as unclassified"
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
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
    // REV-056-F04: the raw path segment was bound with no parsing, so
    // `/api/wallets/sol/<address>/scores` returned an empty history for a wallet that
    // has scores under `solana`, and an invalid chain returned 200 instead of 400.
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let rows = sqlx::query_as::<_, (DateTime<Utc>, i32, i32, i32, bool)>(
        r#"
        SELECT as_of, skill_score, copyability_score, conviction, provisional
          FROM wallet_scores WHERE chain = $1 AND address = $2
         ORDER BY as_of DESC LIMIT 100
        "#,
    )
    .bind(chain.as_str())
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

/// Resolve the workspace bound to the caller's session (REV-027-F09).
///
/// Workspace-scoped reads must derive the workspace from the authenticated
/// session, never a literal. A session without a binding fails closed with 401,
/// exactly like the read API's `Workspace` extractor.
async fn require_workspace(state: &AdminState, headers: &HeaderMap) -> Result<i64, StatusCode> {
    let Some(token) = session_token(headers) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    match crate::auth::workspace_for_session(&state.pool, &token).await {
        Ok(Some(id)) => Ok(id),
        // `Ok(None)` is a real unbound/invalid session -> 401.
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        // A DB failure is a server-side problem, not a bad session (REV-060-F06).
        Err(e) => {
            tracing::error!(
                error = %e,
                "workspace lookup failed; refusing to report a session as invalid"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
#[derive(Deserialize)]
struct RecentQuery {
    window: Option<String>,
}

async fn token_recent(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((chain, mint)): Path<(String, String)>,
    Query(query): Query<RecentQuery>,
) -> Result<Json<Vec<solana_whale_intelligence::sf::recent::RecentEvent>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let workspace_id = require_workspace(&state, &headers).await?;
    // REV-053-F03: the parsed chain BUILDS the key. Validating the raw segment and
    // then interpolating it meant `/sol/<mint>` queried a key that is never written,
    // returning 200 with `[]` for a token that has rows under `solana:<mint>`.
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let window = query.window.as_deref().unwrap_or("24h");
    if !matches!(window, "1h" | "24h" | "7d" | "30d" | "all") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let token_identity = format!("{}:{}", chain.as_str(), mint);
    solana_whale_intelligence::sf::recent_store::fetch_recent_timeline(
        &state.pool,
        workspace_id,
        &token_identity,
        window,
    )
    .await
    .map(Json)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn token_relations(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((chain, mint)): Path<(String, String)>,
) -> Result<Json<Vec<solana_whale_intelligence::sf::recent::CandidateRelation>>, StatusCode> {
    require_auth(&state, &headers).await?;
    let workspace_id = require_workspace(&state, &headers).await?;
    // REV-053-F03: canonical key, same reason as `token_recent`.
    let Some(chain) = ChainKind::parse(&chain) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let token_identity = format!("{}:{}", chain.as_str(), mint);
    solana_whale_intelligence::sf::recent_store::fetch_relations(
        &state.pool,
        workspace_id,
        &token_identity,
    )
    .await
    .map(Json)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
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

/// REV-072-F06 (HIGH): scoped to the session's workspace, like every other
/// policy-dependent read.
async fn list_signals(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let workspace_id = require_workspace(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let rows = sqlx::query_as::<_, (i64, String, String, String, i32, DateTime<Utc>)>(
        "SELECT id, chain, mint, signal_kind, score, created_at FROM signals \
          WHERE workspace_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(|r| serde_json::json!({
        "id": r.0, "chain": r.1, "mint": r.2, "signal_kind": r.3, "score": r.4, "created_at": r.5,
    })).collect()))
}

/// REV-072-F06: rejections are the tenant's own policy answers.
async fn signal_rejections(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let workspace_id = require_workspace(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let rows = sqlx::query_as::<_, (String, String, String, String, DateTime<Utc>)>(
        "SELECT chain, mint, signal_kind, rejection_code, evaluated_at FROM signal_evaluations \
          WHERE workspace_id = $1 AND status = 'rejected' ORDER BY evaluated_at DESC LIMIT $2",
    )
    .bind(workspace_id)
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
    fn mask_formats_and_handles_short() {
        assert_eq!(mask("abcdefgh").as_deref(), Some("ab…gh"));
        assert_eq!(mask("GMGNKEY1234").as_deref(), Some("GM…34"));
        assert_eq!(mask("ab"), None);
        assert_eq!(mask(""), None);
        assert_eq!(mask("   "), None);
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
