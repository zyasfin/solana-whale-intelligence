//! GMGN Agent API client: query-only enrichment against `https://openapi.gmgn.ai`.
//!
//! Auth: `X-APIKEY` header, fresh numeric timestamp, UUID client id, and a
//! stable serialization of query/body used for signing-safe dedupe. Trading
//! routes, private keys, swaps, orders, holdings, and follow-wallet endpoints
//! are NEVER used.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::config::GmgnConfig;
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Route weight classes for the shared bucket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteWeight {
    Light,
    Standard,
    Heavy,
    VeryHeavy,
}

impl RouteWeight {
    pub fn cost(self) -> u32 {
        match self {
            RouteWeight::Light => 1,
            RouteWeight::Standard => 2,
            RouteWeight::Heavy => 3,
            RouteWeight::VeryHeavy => 5,
        }
    }
}

/// GMGN query routes mapped to their official per-route weights.
///
/// Source: GMGN OpenAPI per-route weight tables in the gmgn-skills SKILL.md
/// files (github.com/GMGNAI/gmgn-skills, e.g. skills/gmgn-token/SKILL.md,
/// skills/gmgn-market/SKILL.md, skills/gmgn-portfolio/SKILL.md). The OpenAPI
/// uses a per-key leaky bucket (refill 20 tokens/s, capacity 20 burst) with
/// route weights {1,2,3,5}; there is no monthly quota (flat access).
///
/// Route names here match our `enabled_routes` config vocabulary.
pub fn route_weight(route: &str) -> RouteWeight {
    match route {
        // Weight 1 (Light): token info/security/pool_info, user info,
        // wallet_token_balance, kol, smartmoney, follow_token_groups,
        // trade query_order, gas_price, strategy/orders, market trending
        // (rank), market search, cooking stats.
        "info" | "security" | "pool" | "kol" | "smartmoney" | "trending" => RouteWeight::Light,
        // Weight 2 (Standard): market kline (token_kline), portfolio
        // created-tokens, order quote, order strategy cancel.
        "created_tokens" => RouteWeight::Standard,
        // Weight 3 (Heavy): market trenches, market signal, market
        // hot_searches, portfolio activity/stats/profits, track
        // follow-tokens, track follow-wallet.
        "trenches" | "signal" | "stats" | "profits" | "activity" => RouteWeight::Heavy,
        // Weight 5 (VeryHeavy): token holders, token traders, portfolio
        // holdings, swap, multi_swap, order strategy create, cooking create.
        "holders" | "traders" => RouteWeight::VeryHeavy,
        // Unknown routes default to Standard (2).
        _ => RouteWeight::Standard,
    }
}

/// Weighted token bucket with fixed capacity (default 20).
pub struct WeightedBucket {
    capacity: f64,
    refill_per_second: f64,
    tokens: f64,
    last_refill: Instant,
    /// Optional hard pause until (429 with X-RateLimit-Reset).
    pause_until: Option<Instant>,
}

impl WeightedBucket {
    pub fn new(capacity: u32, refill_per_second: u32) -> Self {
        Self {
            capacity: capacity as f64,
            refill_per_second: refill_per_second as f64,
            tokens: capacity as f64,
            last_refill: Instant::now(),
            pause_until: None,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        if let Some(pause) = self.pause_until {
            if now < pause {
                return;
            }
            self.pause_until = None;
        }
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
            self.last_refill = now;
        }
    }

    /// Try to acquire `cost` tokens; Some(wait) when unavailable.
    pub fn try_acquire(&mut self, cost: u32) -> Option<Duration> {
        self.refill();
        let cost = cost as f64;
        if self.tokens >= cost {
            self.tokens -= cost;
            None
        } else {
            let deficit = cost - self.tokens;
            let rate = self.refill_per_second.max(0.001);
            let mut wait = Duration::from_secs_f64(deficit / rate);
            if let Some(pause) = self.pause_until {
                let pause_wait = pause.saturating_duration_since(Instant::now());
                if pause_wait > wait {
                    wait = pause_wait;
                }
            }
            Some(wait)
        }
    }

    /// Apply a route-queue pause (e.g. `X-RateLimit-Reset` seconds).
    pub fn pause_for(&mut self, duration: Duration) {
        self.pause_until = Some(Instant::now() + duration);
    }

    pub fn current_tokens(&mut self) -> f64 {
        self.refill();
        self.tokens
    }
}

/// A request header pair (name, value).
#[derive(Clone, Debug)]
pub struct HeaderPair {
    pub name: String,
    pub value: String,
}

/// GMGN API client (query-only).
pub struct GmgnClient {
    config: GmgnConfig,
    api_key: Option<String>,
    bucket: Arc<Mutex<WeightedBucket>>,
    client: reqwest::Client,
}

/// GMGN OpenAPI auth headers for one API key (legacy/unsigned form):
/// `X-APIKEY`, `X-TIMESTAMP` (ms), `X-CLIENT-ID` (uuid), JSON content type.
/// Retained for the unsigned data routes; new code should use `GmgnAuth`.
pub fn auth_headers(api_key: &str) -> Vec<HeaderPair> {
    vec![
        HeaderPair { name: "X-APIKEY".to_string(), value: api_key.to_string() },
        HeaderPair { name: "X-TIMESTAMP".to_string(), value: Utc::now().timestamp_millis().to_string() },
        HeaderPair { name: "X-CLIENT-ID".to_string(), value: uuid::Uuid::new_v4().to_string() },
        HeaderPair { name: "Content-Type".to_string(), value: "application/json".to_string() },
    ]
}

/// Authentication material for one GMGN API key: the key itself plus the
/// Ed25519 signing key parsed from the bound pubkey's PKCS#8 PEM (when the
/// key is bound to a server-held pubkey).
///
/// Official spec (GMGNAI/gmgn-skills src/client/signer.ts + OpenApiClient.ts):
/// - `timestamp` (Unix SECONDS, server window ±5s) and `client_id` (uuid,
///   replay window 7s) travel as **query params**, NOT headers.
/// - Data routes (token/market/track/quote, incl. the trenches/rank used
///   here) need only `X-APIKEY` — no signature.
/// - Critical routes (swap/order/holdings/follow-wallet) additionally send
///   `X-Signature` = base64( Ed25519_sign( message ) ), where
///   message = `{sub_path}:{sorted_query_string}:{body}:{timestamp}`.
///   sorted_query_string = all query params (incl. timestamp, client_id)
///   sorted by key, URL-encoded `k=v`, joined with `&`.
#[derive(Clone)]
pub struct GmgnAuth {
    pub api_key: String,
    /// Ed25519 signing key from gmgn_pubkeys.private_key_pem (None when the
    /// API key is not bound to a server-held pubkey → signing unavailable).
    signing_key: Option<ed25519_dalek::SigningKey>,
}

impl GmgnAuth {
    /// Build from an API key + optional PKCS#8 PEM private key. An unparsable
    /// PEM is treated as "no signing key" (data routes still work).
    pub fn new(api_key: &str, private_key_pem: Option<&str>) -> Self {
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        let signing_key = private_key_pem
            .and_then(|pem| ed25519_dalek::SigningKey::from_pkcs8_pem(pem.trim()).ok());
        Self {
            api_key: api_key.to_string(),
            signing_key,
        }
    }

    /// Whether a usable Ed25519 signing key is bound.
    pub fn can_sign(&self) -> bool {
        self.signing_key.is_some()
    }

    /// Build the canonical auth query params (seconds timestamp + uuid client_id).
    fn auth_query(&self) -> (i64, String) {
        (Utc::now().timestamp(), uuid::Uuid::new_v4().to_string())
    }

    /// Percent-encode one query component (application/x-www-form-urlencoded).
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// Sorted, URL-encoded query string over all params (incl. auth params).
    fn sorted_query(params: &BTreeMap<String, String>) -> String {
        params
            .iter()
            .map(|(k, v)| format!("{}={}", Self::enc(k), Self::enc(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// The signature message: `{sub_path}:{sorted_query_string}:{body}:{timestamp}`.
    pub fn build_message(sub_path: &str, sorted_query_string: &str, body: &str, timestamp: i64) -> String {
        format!("{sub_path}:{sorted_query_string}:{body}:{timestamp}")
    }

    /// Sign a message with the bound Ed25519 key; base64 signature.
    /// Returns None when no signing key is bound.
    pub fn sign_message(&self, message: &str) -> Option<String> {
        use ed25519_dalek::Signer;
        let key = self.signing_key.as_ref()?;
        let signature = key.sign(message.as_bytes());
        use base64::Engine;
        Some(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()))
    }

    /// Prepare a fully-authenticated request: returns (url, headers).
    ///
    /// `base_url` = GMGN host (no trailing slash), `sub_path` = `/v1/...`,
    /// `params` = route query params (auth params added here), `body` = the
    /// exact JSON body string ("" for GET/empty). When a signing key is bound
    /// the `X-Signature` header is included; otherwise only `X-APIKEY`.
    pub fn prepare_request(
        &self,
        base_url: &str,
        sub_path: &str,
        params: &BTreeMap<String, String>,
        body: &str,
    ) -> (String, Vec<HeaderPair>) {
        let (timestamp, client_id) = self.auth_query();
        let mut all = params.clone();
        all.insert("timestamp".to_string(), timestamp.to_string());
        all.insert("client_id".to_string(), client_id.clone());
        let qs = Self::sorted_query(&all);
        let url = format!("{}{}?{}", base_url.trim_end_matches('/'), sub_path, qs);

        let mut headers = vec![
            HeaderPair { name: "X-APIKEY".to_string(), value: self.api_key.clone() },
            HeaderPair { name: "Content-Type".to_string(), value: "application/json".to_string() },
        ];
        if let Some(signature) = self
            .sign_message(&Self::build_message(sub_path, &qs, body, timestamp))
        {
            headers.push(HeaderPair { name: "X-Signature".to_string(), value: signature });
        }
        let _ = client_id; // already embedded in the query string
        (url, headers)
    }
}

/// The GMGN envelope: `{code, data, message}`.
#[derive(Clone, Debug)]
pub struct GmgnEnvelope {
    pub code: i64,
    pub message: String,
    pub data: serde_json::Value,
}

impl GmgnEnvelope {
    /// Nonzero code is an error even when HTTP status is 200.
    pub fn is_error(&self) -> bool {
        self.code != 0
    }
}

impl GmgnClient {
    pub fn new(config: GmgnConfig, api_key: Option<String>) -> Self {
        let timeout = config.timeout_seconds;
        Self {
            bucket: Arc::new(Mutex::new(WeightedBucket::new(
                config.bucket_capacity,
                config.bucket_refill_per_second,
            ))),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout))
                .build()
                .unwrap_or_default(),
            config,
            api_key,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.api_key.as_deref().map(|k| !k.trim().is_empty()).unwrap_or(false)
    }

    /// Build headers exactly once per request: API key, timestamp, client id.
    pub fn build_headers(&self) -> Result<Vec<HeaderPair>> {
        let api_key = self
            .api_key
            .as_deref()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| anyhow!("gmgn api key missing"))?;
        Ok(auth_headers(api_key))
    }

    /// Stable serialization of query parameters (BTreeMap ordering).
    pub fn stable_query(params: &BTreeMap<String, String>) -> String {
        params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Perform a query route call with weighted rate limiting.
    pub async fn query(
        &self,
        route: &str,
        params: BTreeMap<String, String>,
    ) -> Result<GmgnEnvelope> {
        if !self.is_configured() {
            bail!("gmgn api key not configured");
        }
        if !self.config.enabled_routes.is_empty()
            && !self.config.enabled_routes.iter().any(|r| r == route)
        {
            bail!("gmgn route {route} is not enabled");
        }
        let weight = route_weight(route);
        loop {
            let wait = {
                let mut bucket = self.bucket.lock().await;
                bucket.try_acquire(weight.cost())
            };
            if let Some(wait) = wait {
                tokio::time::sleep(wait.min(Duration::from_secs(5))).await;
                continue;
            }

            let url = format!("{}/{}/api/v1", self.config.base_url.trim_end_matches('/'), route);
            let query = Self::stable_query(&params);
            let full = if query.is_empty() {
                url
            } else {
                format!("{url}?{query}")
            };
            let headers = self.build_headers()?;
            let mut request = self.client.get(&full);
            for header in headers {
                request = request.header(&header.name, header.value);
            }
            let response = request.send().await.map_err(|e| anyhow!("gmgn network error: {e}"))?;
            let status = response.status();
            let response_headers = response.headers().clone();
            let text = response
                .text()
                .await
                .map_err(|e| anyhow!("gmgn decode error: {e}"))?;

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let reset = response_headers
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let pause = reset
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_secs(1))
                    .min(Duration::from_secs(120));
                self.bucket.lock().await.pause_for(pause);
                // Continue loop: pause prevents tight-looping.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            if status.is_client_error() {
                bail!("gmgn client error {status}: {text}");
            }
            if !status.is_success() {
                bail!("gmgn server error {status}");
            }

            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| anyhow!("gmgn payload not json: {e}"))?;
            let code = value.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let message = value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            let data = value.get("data").cloned().unwrap_or(serde_json::Value::Null);
            let envelope = GmgnEnvelope { code, message, data };
            if envelope.is_error() {
                bail!("gmgn envelope error code {}: {}", envelope.code, envelope.message);
            }
            return Ok(envelope);
        }
    }

    /// Current bucket tokens (health).
    pub async fn bucket_tokens(&self) -> f64 {
        self.bucket.lock().await.current_tokens()
    }
}

/// One DB-backed GMGN API key with its own weighted bucket and quota state.
pub struct GmgnPoolKey {
    pub db_id: i64,
    pub api_key: String,
    /// Auth material (API key + optional Ed25519 signing key from the bound pubkey).
    pub auth: GmgnAuth,
    pub bucket: Mutex<WeightedBucket>,
    monthly_limit: Option<i64>,
    used_this_month: AtomicU64,
}

impl GmgnPoolKey {
    fn from_db_row(db_id: i64, api_key: String, private_key_pem: Option<String>, monthly_limit: Option<i32>, used_this_month: i32, config: &GmgnConfig) -> Self {
        let auth = GmgnAuth::new(&api_key, private_key_pem.as_deref());
        Self {
            db_id,
            api_key,
            auth,
            bucket: Mutex::new(WeightedBucket::new(config.bucket_capacity, config.bucket_refill_per_second)),
            monthly_limit: monthly_limit.map(|l| l.max(0) as i64),
            used_this_month: AtomicU64::new(used_this_month.max(0) as u64),
        }
    }

    /// Monthly quota exhausted (keys with a configured limit).
    pub fn quota_exhausted(&self) -> bool {
        match self.monthly_limit {
            Some(limit) => (self.used_this_month.load(Ordering::Relaxed) as i64) >= limit,
            None => false,
        }
    }
}

/// DB-backed GMGN key pool (admin "API panel").
///
/// Mirrors `GmgnClient` but round-robins across enabled `gmgn_keys` rows,
/// each with its own weighted token bucket and monthly quota. Usage persists
/// batched: per-request deltas accumulate in memory and a background task
/// flushes them every `USAGE_FLUSH_SECS` seconds (one UPDATE per dirty key,
/// atomic monthly reset + increment).
pub struct GmgnPool {
    config: GmgnConfig,
    keys: Vec<Arc<GmgnPoolKey>>,
    next: std::sync::atomic::AtomicUsize,
    client: reqwest::Client,
    db: PgPool,
    /// Pending per-key usage increments since the last flush (db id -> count).
    pending: std::sync::Mutex<std::collections::HashMap<i64, i64>>,
}

/// Flush cadence for batched usage persistence (seconds).
pub const USAGE_FLUSH_SECS: u64 = 15;

impl GmgnPool {
    /// Load enabled keys from `gmgn_keys`, resetting stale monthly counters.
    pub async fn load(db: PgPool, config: GmgnConfig) -> Result<Self> {
        sqlx::query("UPDATE gmgn_keys SET used_this_month = 0, usage_month = $1 WHERE usage_month <> $1")
            .bind(crate::helius::current_month())
            .execute(&db)
            .await?;
        // Join gmgn_pubkeys so each key carries its Ed25519 signing material
        // (private_key_pem stays server-side; only the parsed SigningKey is held).
        let rows: Vec<(i64, String, Option<String>, Option<i32>, i32)> = sqlx::query_as(
            r#"
            SELECT k.id, k.api_key, p.private_key_pem, k.monthly_limit, k.used_this_month
              FROM gmgn_keys k
              LEFT JOIN gmgn_pubkeys p ON p.id = k.pubkey_id
             WHERE k.enabled
             ORDER BY k.id
            "#,
        )
        .fetch_all(&db)
        .await?;
        Ok(Self::from_rows(rows, db, config))
    }

    /// Build from raw `(id, api_key, private_key_pem, monthly_limit, used_this_month)` rows.
    pub fn from_rows(rows: Vec<(i64, String, Option<String>, Option<i32>, i32)>, db: PgPool, config: GmgnConfig) -> Self {
        let timeout = config.timeout_seconds;
        let keys = rows
            .into_iter()
            .map(|(id, api_key, private_key_pem, monthly_limit, used)| {
                Arc::new(GmgnPoolKey::from_db_row(id, api_key, private_key_pem, monthly_limit, used, &config))
            })
            .collect();
        Self {
            config,
            keys,
            next: std::sync::atomic::AtomicUsize::new(0),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout))
                .build()
                .unwrap_or_default(),
            db,
            pending: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Keys usable right now (not quota-exhausted).
    pub fn available_count(&self) -> usize {
        self.keys.iter().filter(|k| !k.quota_exhausted()).count()
    }

    /// Spawn the batched usage flusher (every USAGE_FLUSH_SECS seconds).
    pub fn spawn_usage_flusher(self: &Arc<Self>) {
        let pool = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(USAGE_FLUSH_SECS));
            loop {
                interval.tick().await;
                let Some(pool) = pool.upgrade() else { break };
                pool.flush_usage().await;
            }
        });
    }

    /// Persist pending usage increments (atomic monthly-reset + increment).
    pub async fn flush_usage(&self) {
        let deltas: Vec<(i64, i64)> = match self.pending.lock() {
            Ok(mut pending) => pending.drain().collect(),
            Err(_) => return,
        };
        for (id, count) in deltas {
            let result = sqlx::query(
                r#"
                UPDATE gmgn_keys
                   SET used_this_month = CASE WHEN usage_month <> $2 THEN $3::int
                                              ELSE used_this_month + $3::int END,
                       usage_month = $2,
                       last_ok_at = now(),
                       last_error = NULL
                 WHERE id = $1
                "#,
            )
            .bind(id)
            .bind(crate::helius::current_month())
            .bind(count as i32)
            .execute(&self.db)
            .await;
            if let Err(err) = result {
                tracing::warn!(error = %err, key = id, "gmgn usage flush failed");
            }
        }
    }

    /// Persist a failure message for one key (best-effort, one UPDATE).
    pub async fn persist_error(&self, db_id: i64, message: &str) {
        let trimmed: String = message.chars().take(500).collect();
        if let Err(err) = sqlx::query("UPDATE gmgn_keys SET last_error = $2 WHERE id = $1")
            .bind(db_id)
            .bind(trimmed)
            .execute(&self.db)
            .await
        {
            tracing::warn!(error = %err, key = db_id, "gmgn error persist failed");
        }
    }

    /// Record one successful request against `key`: bump the in-memory
    /// counter (monthly quota gate) and queue a batched DB increment.
    fn record_success_usage(&self, key: &Arc<GmgnPoolKey>) {
        key.used_this_month.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut pending) = self.pending.lock() {
            *pending.entry(key.db_id).or_insert(0) += 1;
        }
    }

    /// Pick the next quota-available key (fair round-robin).
    fn pick_key(&self) -> Result<Arc<GmgnPoolKey>> {
        if self.keys.is_empty() {
            bail!("no gmgn keys configured");
        }
        let count = self.keys.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for offset in 0..count {
            let key = &self.keys[(start + offset) % count];
            if !key.quota_exhausted() {
                return Ok(key.clone());
            }
        }
        bail!("all gmgn keys exhausted their monthly quota");
    }

    /// Auth headers for one key: API key, fresh timestamp, random client id.
    /// NOTE: the pool no longer builds ad-hoc headers — requests go through
    /// `GmgnAuth::prepare_request`, which puts timestamp/client_id in the query
    /// string and adds X-Signature when a signing key is bound (official spec).

    /// Perform a query route call with per-key weighted rate limiting,
    /// round-robin key selection, and monthly quota enforcement.
    pub async fn query(
        &self,
        route: &str,
        params: BTreeMap<String, String>,
    ) -> Result<GmgnEnvelope> {
        if !self.config.enabled_routes.is_empty()
            && !self.config.enabled_routes.iter().any(|r| r == route)
        {
            bail!("gmgn route {route} is not enabled");
        }
        let sub_path = format!("/{}/api/v1", route);
        self.execute(route_weight(route), "GET", &sub_path, &params, "").await
    }

    /// Spec-layout GET against the official OpenAPI (`{base}/v1/...`).
    ///
    /// Round-robins across keys, applies the per-key weighted bucket, honors
    /// 429 `X-RateLimit-Reset`, and signs via `GmgnAuth::prepare_request`
    /// (timestamp/client_id in the query string, `X-APIKEY`, `X-Signature`
    /// when a key is bound). `sub_path` includes the leading `/v1/...`.
    pub async fn v1_get(
        &self,
        sub_path: &str,
        params: BTreeMap<String, String>,
    ) -> Result<GmgnEnvelope> {
        self.check_route_allowed(sub_path)?;
        self.execute(route_weight_for_v1(sub_path), "GET", sub_path, &params, "").await
    }

    /// Spec-layout POST against the official OpenAPI (`{base}/v1/...`).
    ///
    /// `params` are the routing query params (e.g. `chain`); the JSON body
    /// string is signed per spec:
    /// message = `{sub_path}:{sorted_query}:{body}:{timestamp}`.
    pub async fn v1_post(
        &self,
        sub_path: &str,
        params: BTreeMap<String, String>,
        json_body: serde_json::Value,
    ) -> Result<GmgnEnvelope> {
        self.check_route_allowed(sub_path)?;
        let body = serde_json::to_string(&json_body).unwrap_or_default();
        self.execute(route_weight_for_v1(sub_path), "POST", sub_path, &params, &body).await
    }

    /// Enforce the `enabled_routes` allowlist (same semantics as `query`):
    /// empty = all allowed; otherwise the sub-path's route segment
    /// (`/v1/trenches` -> `trenches`) must be listed.
    fn check_route_allowed(&self, sub_path: &str) -> Result<()> {
        if self.config.enabled_routes.is_empty() {
            return Ok(());
        }
        // The route vocabulary in `enabled_routes` uses the resource name
        // (trenches, smartmoney, kol, stats, ...). Map a /v1/... sub-path to
        // its LAST segment: /v1/user/smartmoney -> smartmoney,
        // /v1/user/wallet_stats -> wallet_stats (also accept the bare `stats`
        // alias used in config), /v1/trenches -> trenches.
        let full = sub_path.trim_start_matches("/v1/").trim_start_matches('/');
        let last = full.rsplit('/').next().unwrap_or(full);
        let allowed = self.config.enabled_routes.iter().any(|r| {
            r == full
                || r == last
                // config aliases: stats -> wallet_stats, info -> token/info, etc.
                || (r == "stats" && last == "wallet_stats")
                || (r == "info" && full == "token/info")
                || (r == "security" && full == "token/security")
                || (r == "pool" && full == "token/pool_info")
        });
        if allowed {
            Ok(())
        } else {
            bail!("gmgn route {full} is not enabled")
        }
    }

    /// Fetch the newest tokens via the official trenches endpoint
    /// (`POST /v1/trenches?chain=<chain>`), used by BOTH the discovery worker
    /// and the admin TG-hot endpoint so the request shape can't drift.
    ///
    /// Body mirrors gmgn-cli's buildTrenchesBody: `version: v2` with one
    /// section per token category (filters/limit). Returns the parsed
    /// envelope whose `data` holds `{new_creation:[..], near_completion:[..],
    /// completed:[..]}`.
    pub async fn trenches(&self, chain: &str, limit: u32) -> Result<GmgnEnvelope> {
        let section = serde_json::json!({
            "filters": ["offchain", "onchain"],
            "launchpad_platform_v2": true,
            "limit": limit,
        });
        let body = serde_json::json!({
            "version": "v2",
            "new_creation": section,
            "near_completion": section,
            "completed": section,
        });
        let mut params = BTreeMap::new();
        params.insert("chain".to_string(), chain.to_string());
        self.v1_post("/v1/trenches", params, body).await
    }

    /// Shared request executor: weighted bucket + round-robin + 429 handling +
    /// envelope parsing. `method` GET sends no body; POST sends `body`.
    async fn execute(
        &self,
        weight: RouteWeight,
        method: &str,
        sub_path: &str,
        params: &BTreeMap<String, String>,
        body: &str,
    ) -> Result<GmgnEnvelope> {
        if self.keys.is_empty() {
            bail!("gmgn api key not configured");
        }
        let mut attempts = 0usize;
        loop {
            let key = self.pick_key()?;
            let wait = {
                let mut bucket = key.bucket.lock().await;
                bucket.try_acquire(weight.cost())
            };
            if let Some(wait) = wait {
                // This key is momentarily saturated: try the next key a few
                // times before sleeping on this one.
                attempts += 1;
                if attempts <= self.keys.len() {
                    continue;
                }
                tokio::time::sleep(wait.min(Duration::from_secs(5))).await;
                continue;
            }

            let (full, headers) = key
                .auth
                .prepare_request(&self.config.base_url, sub_path, params, body);
            let mut request = if method == "POST" {
                self.client.post(&full).body(body.to_string())
            } else {
                self.client.get(&full)
            };
            for header in headers {
                request = request.header(&header.name, header.value);
            }
            let response = request.send().await.map_err(|e| anyhow!("gmgn network error: {e}"))?;
            let status = response.status();
            let response_headers = response.headers().clone();
            let text = response
                .text()
                .await
                .map_err(|e| anyhow!("gmgn decode error: {e}"))?;

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let reset = response_headers
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let pause = reset
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_secs(1))
                    .min(Duration::from_secs(120));
                key.bucket.lock().await.pause_for(pause);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            if status.is_client_error() {
                self.persist_error(key.db_id, &format!("gmgn client error {status}")).await;
                bail!("gmgn client error {status}: {text}");
            }
            if !status.is_success() {
                self.persist_error(key.db_id, &format!("gmgn server error {status}")).await;
                bail!("gmgn server error {status}");
            }

            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| anyhow!("gmgn payload not json: {e}"))?;
            let code = value.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let message = value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            let data = value.get("data").cloned().unwrap_or(serde_json::Value::Null);
            let envelope = GmgnEnvelope { code, message, data };
            if envelope.is_error() {
                self.persist_error(key.db_id, &format!("envelope {}: {}", envelope.code, envelope.message)).await;
                bail!("gmgn envelope error code {}: {}", envelope.code, envelope.message);
            }
            self.record_success_usage(&key);
            return Ok(envelope);
        }
    }
}

/// Flatten a v2 trenches `data` payload into a single token list:
/// `data{new_creation:[..], near_completion:[..], completed:[..]}` (or a flat
/// array / items variant). Shared by the discovery worker and admin endpoint.
pub fn flatten_trenches_tokens(data: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut items: Vec<serde_json::Value> = Vec::new();
    if let Some(a) = data.as_array() {
        items.extend(a.clone());
        return items;
    }
    for key in ["new_creation", "near_completion", "completed", "items", "list", "data"] {
        if let Some(a) = data.get(key).and_then(|v| v.as_array()) {
            items.extend(a.clone());
        }
    }
    items
}

/// Map an official `/v1/...` sub-path to its per-route weight.
/// Mirrors route_weight() for the spec-layout sub-paths used by v1_get/v1_post.
fn route_weight_for_v1(sub_path: &str) -> RouteWeight {
    match sub_path {
        "/v1/user/kol" | "/v1/user/smartmoney" | "/v1/user/info"
        | "/v1/token/info" | "/v1/token/security" | "/v1/token/pool_info"
        | "/v1/user/wallet_token_balance" | "/v1/user/follow_token_groups"
        | "/v1/trade/query_order" | "/v1/trade/gas_price" | "/v1/trade/strategy/orders"
        | "/v1/market/rank" | "/v1/cooking/statistics" => RouteWeight::Light,
        "/v1/market/token_kline" | "/v1/user/created_tokens"
        | "/v1/trade/quote" | "/v1/trade/strategy/cancel" => RouteWeight::Standard,
        "/v1/trenches" | "/v1/market/token_signal" | "/v1/market/hot_searches"
        | "/v1/user/wallet_activity" | "/v1/user/wallet_stats" | "/v1/user/wallet_profits"
        | "/v1/user/follow_tokens" | "/v1/trade/follow_wallet" => RouteWeight::Heavy,
        "/v1/market/token_top_holders" | "/v1/market/token_top_traders"
        | "/v1/user/wallet_holdings" | "/v1/trade/swap" | "/v1/trade/multi_swap"
        | "/v1/trade/strategy/create" | "/v1/cooking/create_token" => RouteWeight::VeryHeavy,
        _ => RouteWeight::Standard,
    }
}

/// Parse a Decimal from GMGN payloads tolerating strings and numbers.
pub fn parse_decimal(value: &serde_json::Value) -> Option<Decimal> {
    match value {
        serde_json::Value::Number(n) => Decimal::from_str_exact(&n.to_string()).ok(),
        serde_json::Value::String(s) => Decimal::from_str_exact(s.trim()).ok(),
        _ => None,
    }
}

/// Token triage extraction from a GMGN token/security payload.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TokenTriage {
    pub mint: String,
    pub symbol: Option<String>,
    pub liquidity_usd: Option<Decimal>,
    pub market_cap_usd: Option<Decimal>,
    pub smart_degen_count: Option<u32>,
    pub top_rat_trader_percentage: Option<Decimal>,
    pub top_bundler_trader_percentage: Option<Decimal>,
    pub fresh_wallet_rate: Option<Decimal>,
    pub private_vault_hold_rate: Option<Decimal>,
    pub is_wash_trading: Option<bool>,
    pub dex_bot: Option<bool>,
    pub sniper: Option<bool>,
    pub bundler: Option<bool>,
    pub rat_trader: Option<bool>,
}

impl TokenTriage {
    /// Extract triage fields from a GMGN `info`/`security`-style payload.
    pub fn from_payload(payload: &serde_json::Value) -> Self {
        let mint = payload
            .get("address")
            .or_else(|| payload.get("mint"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Self {
            mint,
            symbol: payload.get("symbol").and_then(|v| v.as_str()).map(str::to_string),
            liquidity_usd: parse_decimal(payload.get("liquidity").unwrap_or(&serde_json::Value::Null)),
            market_cap_usd: parse_decimal(payload.get("market_cap").unwrap_or(&serde_json::Value::Null)),
            smart_degen_count: payload
                .get("smart_degen_count")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            top_rat_trader_percentage: parse_decimal(
                payload.get("top_rat_trader_percentage").unwrap_or(&serde_json::Value::Null),
            ),
            top_bundler_trader_percentage: parse_decimal(
                payload.get("top_bundler_trader_percentage").unwrap_or(&serde_json::Value::Null),
            ),
            fresh_wallet_rate: parse_decimal(payload.get("fresh_wallet_rate").unwrap_or(&serde_json::Value::Null)),
            private_vault_hold_rate: parse_decimal(
                payload.get("private_vault_hold_rate").unwrap_or(&serde_json::Value::Null),
            ),
            is_wash_trading: payload.get("is_wash_trading").and_then(|v| v.as_bool()),
            dex_bot: payload.get("dex_bot").and_then(|v| v.as_bool()),
            sniper: payload.get("sniper").and_then(|v| v.as_bool()),
            bundler: payload.get("bundler").and_then(|v| v.as_bool()),
            rat_trader: payload.get("rat_trader").and_then(|v| v.as_bool()),
        }
    }

    /// Risk flags derived from triage fields.
    pub fn risk_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        if self.is_wash_trading == Some(true) {
            flags.push("wash_trading".to_string());
        }
        if self.dex_bot == Some(true) {
            flags.push("dex_bot_activity".to_string());
        }
        if self
            .top_rat_trader_percentage
            .map(|p| p > Decimal::from(30))
            .unwrap_or(false)
        {
            flags.push("rat_trader_heavy".to_string());
        }
        if self
            .fresh_wallet_rate
            .map(|r| r > Decimal::from(50))
            .unwrap_or(false)
        {
            flags.push("fresh_wallet_heavy".to_string());
        }
        flags
    }

    /// Whether this triage warrants an automatic skip candidate.
    pub fn skip_candidate(&self) -> bool {
        self.dex_bot == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(routes: Vec<&str>) -> GmgnConfig {
        GmgnConfig {
            base_url: "https://openapi.gmgn.ai".into(),
            bucket_capacity: 20,
            bucket_refill_per_second: 20,
            timeout_seconds: 5,
            enabled_routes: routes.into_iter().map(str::to_string).collect(),
        }
    }

    #[test]
    fn route_weights_within_bucket_capacity() {
        let mut bucket = WeightedBucket::new(20, 0);
        // Consume weights 1+2+3+5 = 11; capacity 20 allows them all.
        assert!(bucket.try_acquire(RouteWeight::Light.cost()).is_none());
        assert!(bucket.try_acquire(RouteWeight::Standard.cost()).is_none());
        assert!(bucket.try_acquire(RouteWeight::Heavy.cost()).is_none());
        assert!(bucket.try_acquire(RouteWeight::VeryHeavy.cost()).is_none());
        // 20 - 11 = 9 tokens left; a very heavy route (5) fits, then heavy (3) fits, then heavy again needs 3 > 1.
        assert!(bucket.try_acquire(RouteWeight::VeryHeavy.cost()).is_none());
        assert!(bucket.try_acquire(RouteWeight::Heavy.cost()).is_none());
        assert!(bucket.try_acquire(RouteWeight::Heavy.cost()).is_some());
    }

    #[test]
    fn route_weight_mapping() {
        // Weight 1 (Light).
        assert_eq!(route_weight("info"), RouteWeight::Light);
        assert_eq!(route_weight("security"), RouteWeight::Light);
        assert_eq!(route_weight("pool"), RouteWeight::Light);
        assert_eq!(route_weight("kol"), RouteWeight::Light);
        assert_eq!(route_weight("smartmoney"), RouteWeight::Light);
        assert_eq!(route_weight("trending"), RouteWeight::Light);
        // Weight 2 (Standard).
        assert_eq!(route_weight("created_tokens"), RouteWeight::Standard);
        // Weight 3 (Heavy).
        assert_eq!(route_weight("trenches"), RouteWeight::Heavy);
        assert_eq!(route_weight("signal"), RouteWeight::Heavy);
        assert_eq!(route_weight("stats"), RouteWeight::Heavy);
        assert_eq!(route_weight("profits"), RouteWeight::Heavy);
        assert_eq!(route_weight("activity"), RouteWeight::Heavy);
        // Weight 5 (VeryHeavy).
        assert_eq!(route_weight("holders"), RouteWeight::VeryHeavy);
        assert_eq!(route_weight("traders"), RouteWeight::VeryHeavy);
        // Unknown routes default to Standard (2).
        assert_eq!(route_weight("unknown_route"), RouteWeight::Standard);
    }

    #[test]
    fn stable_query_orders_parameters() {
        let mut params = BTreeMap::new();
        params.insert("zeta".to_string(), "1".to_string());
        params.insert("alpha".to_string(), "2".to_string());
        params.insert("mid".to_string(), "3".to_string());
        assert_eq!(GmgnClient::stable_query(&params), "alpha=2&mid=3&zeta=1");
    }

    #[test]
    fn headers_require_api_key() {
        let client = GmgnClient::new(config(vec!["info"]), None);
        assert!(!client.is_configured());
        assert!(client.build_headers().is_err());

        let client = GmgnClient::new(config(vec!["info"]), Some("key-123".into()));
        assert!(client.is_configured());
        let headers = client.build_headers().unwrap();
        assert!(headers.iter().any(|h| h.name == "X-APIKEY" && h.value == "key-123"));
        assert!(headers
            .iter()
            .any(|h| h.name == "X-TIMESTAMP" && h.value.chars().all(|c| c.is_ascii_digit())));
        assert!(headers
            .iter()
            .any(|h| h.name == "X-CLIENT-ID" && uuid::Uuid::parse_str(&h.value).is_ok()));
    }

    #[test]
    fn envelope_nonzero_code_is_error() {
        let ok = GmgnEnvelope {
            code: 0,
            message: String::new(),
            data: serde_json::Value::Null,
        };
        let bad = GmgnEnvelope {
            code: 1001,
            message: "quota exceeded".into(),
            data: serde_json::Value::Null,
        };
        assert!(!ok.is_error());
        assert!(bad.is_error());
    }

    #[test]
    fn token_triage_extracts_gmgn_fields() {
        let payload = serde_json::json!({
            "address": "MintAddress111111111111111111111111111111111",
            "symbol": "TEST",
            "liquidity": 25000,
            "market_cap": "1200000.5",
            "smart_degen_count": 3,
            "top_rat_trader_percentage": 12.5,
            "top_bundler_trader_percentage": 4.2,
            "fresh_wallet_rate": 33,
            "private_vault_hold_rate": 10,
            "is_wash_trading": false,
            "dex_bot": false,
            "sniper": true,
            "bundler": false,
            "rat_trader": true
        });
        let triage = TokenTriage::from_payload(&payload);
        assert_eq!(triage.symbol.as_deref(), Some("TEST"));
        assert_eq!(triage.liquidity_usd, Some(Decimal::from(25_000)));
        assert_eq!(
            triage.market_cap_usd,
            Some(Decimal::from_str_exact("1200000.5").unwrap())
        );
        assert_eq!(triage.smart_degen_count, Some(3));
        assert_eq!(triage.sniper, Some(true));
        assert_eq!(triage.rat_trader, Some(true));
        assert!(!triage.skip_candidate());
    }

    #[test]
    fn token_triage_dex_bot_is_skip_candidate() {
        let payload = serde_json::json!({ "address": "m", "dex_bot": true });
        let triage = TokenTriage::from_payload(&payload);
        assert!(triage.skip_candidate());
        assert!(triage.risk_flags().contains(&"dex_bot_activity".to_string()));
    }

    #[test]
    fn token_triage_high_rat_traders_flagged() {
        let payload = serde_json::json!({
            "address": "m",
            "top_rat_trader_percentage": 55,
            "fresh_wallet_rate": 80
        });
        let triage = TokenTriage::from_payload(&payload);
        let flags = triage.risk_flags();
        assert!(flags.contains(&"rat_trader_heavy".to_string()));
        assert!(flags.contains(&"fresh_wallet_heavy".to_string()));
    }

    #[test]
    fn parse_decimal_tolerates_strings_and_numbers() {
        assert_eq!(parse_decimal(&serde_json::json!(1234)), Some(Decimal::from(1234)));
        assert_eq!(
            parse_decimal(&serde_json::json!("12.34")),
            Some(Decimal::from_str_exact("12.34").unwrap())
        );
        assert_eq!(parse_decimal(&serde_json::json!(null)), None);
        assert_eq!(parse_decimal(&serde_json::json!(true)), None);
    }

    #[tokio::test]
    async fn disabled_route_rejected_without_request() {
        let client = GmgnClient::new(config(vec!["info"]), Some("key".into()));
        let err = client
            .query("trenches", BTreeMap::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not enabled"));
    }

    #[tokio::test]
    async fn unconfigured_key_rejected() {
        let client = GmgnClient::new(config(vec!["info"]), None);
        let err = client.query("info", BTreeMap::new()).await.unwrap_err();
        assert!(err.to_string().contains("api key not configured"));
    }
    #[test]
    fn pool_key_quota_gate() {
        let cfg = config(vec!["info"]);
        let limited = GmgnPoolKey::from_db_row(1, "k1".into(), None, Some(10), 10, &cfg);
        let room = GmgnPoolKey::from_db_row(2, "k2".into(), None, Some(10), 3, &cfg);
        let unlimited = GmgnPoolKey::from_db_row(3, "k3".into(), None, None, 12345, &cfg);
        assert!(limited.quota_exhausted());
        assert!(!room.quota_exhausted());
        assert!(!unlimited.quota_exhausted());
    }

    /// A fixed Ed25519 PKCS#8 v1 PEM (deterministic test key, never used live).
    const TEST_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MC4CAQAwBQYDK2VwBCIEIAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g\n\
-----END PRIVATE KEY-----\n";

    #[test]
    fn sign_message_is_deterministic_and_verifies() {
        let auth = GmgnAuth::new("test-api-key", Some(TEST_PRIV_PEM));
        assert!(auth.can_sign());
        let message = GmgnAuth::build_message("/v1/market/rank", "chain=sol&interval=1h&limit=1&timestamp=1700000000", "", 1700000000);
        let sig1 = auth.sign_message(&message).expect("signs");
        let sig2 = auth.sign_message(&message).expect("signs");
        assert_eq!(sig1, sig2, "Ed25519 signing is deterministic");
        // Verify the signature against the public key derived from the same key.
        use base64::Engine;
        use ed25519_dalek::pkcs8::DecodePrivateKey;
        use ed25519_dalek::Verifier;
        let signing = ed25519_dalek::SigningKey::from_pkcs8_pem(TEST_PRIV_PEM).unwrap();
        let sig_bytes = base64::engine::general_purpose::STANDARD.decode(&sig1).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        assert!(signing
            .verifying_key()
            .verify(message.as_bytes(), &sig)
            .is_ok());
    }

    #[test]
    fn build_message_matches_official_format() {
        // Format: {sub_path}:{sorted_query_string}:{body}:{timestamp}
        let msg = GmgnAuth::build_message("/v1/trade/swap", "chain=sol&timestamp=1700000000", "{\"a\":1}", 1700000000);
        assert_eq!(msg, "/v1/trade/swap:chain=sol&timestamp=1700000000:{\"a\":1}:1700000000");
    }
    #[test]
    fn flatten_trenches_tokens_reads_v2_categories() {
        let data = serde_json::json!({
            "new_creation": [{"address": "mintA", "symbol": "A"}],
            "near_completion": [{"address": "mintB", "symbol": "B"}],
            "completed": [{"address": "mintC", "symbol": "C"}]
        });
        let items = flatten_trenches_tokens(&data);
        assert_eq!(items.len(), 3);
        let mints: Vec<&str> = items.iter().filter_map(|t| t.get("address").and_then(|a| a.as_str())).collect();
        assert!(mints.contains(&"mintA") && mints.contains(&"mintB") && mints.contains(&"mintC"));
        // Flat-array variant also works.
        let flat = serde_json::json!([{"address": "mintZ"}]);
        assert_eq!(flatten_trenches_tokens(&flat).len(), 1);
    }
    /// Golden vector: signature produced by the OFFICIAL gmgn-cli
    /// (node dist/client/signer.js) for the same key + message. Our
    /// implementation must match it byte-for-byte.
    #[test]
    fn signature_matches_official_gmgn_cli() {
        let auth = GmgnAuth::new("k", Some(TEST_PRIV_PEM));
        // Same query the node reference used (already sorted):
        let qs = "chain=sol&client_id=11111111-2222-3333-4444-555555555555&interval=1h&limit=1&timestamp=1700000000";
        let msg = GmgnAuth::build_message("/v1/market/rank", qs, "", 1700000000);
        assert_eq!(
            msg,
            "/v1/market/rank:chain=sol&client_id=11111111-2222-3333-4444-555555555555&interval=1h&limit=1&timestamp=1700000000::1700000000"
        );
        let sig = auth.sign_message(&msg).unwrap();
        assert_eq!(
            sig,
            "8w/B+iY9SSX4pTkOE1npbLDUvQ5Ie0FH1RPu3XKiZCkENf/EPBtn/LTOR0429RY/Lx+HfL5SrB56/jRx3a0HDQ==",
            "must equal the official gmgn-cli Ed25519 signature"
        );
    }

    #[test]
    fn prepare_request_puts_auth_in_query_and_signs() {
        let auth = GmgnAuth::new("mykey", Some(TEST_PRIV_PEM));
        let mut params = BTreeMap::new();
        params.insert("chain".to_string(), "sol".to_string());
        params.insert("interval".to_string(), "1h".to_string());
        let (url, headers) = auth.prepare_request("https://openapi.gmgn.ai", "/v1/market/rank", &params, "");
        // timestamp + client_id present in the query string, sorted by key.
        assert!(url.starts_with("https://openapi.gmgn.ai/v1/market/rank?"));
        let qs = url.splitn(2, '?').nth(1).unwrap();
        let chain_pos = qs.find("chain=sol").unwrap();
        let client_pos = qs.find("client_id=").unwrap();
        let interval_pos = qs.find("interval=1h").unwrap();
        let ts_pos = qs.find("timestamp=").unwrap();
        assert!(chain_pos < client_pos && client_pos < interval_pos && interval_pos < ts_pos,
            "query params sorted alphabetically: {qs}");
        // Headers: X-APIKEY + X-Signature (signed because a key is bound).
        assert!(headers.iter().any(|h| h.name == "X-APIKEY" && h.value == "mykey"));
        assert!(headers.iter().any(|h| h.name == "X-Signature" && !h.value.is_empty()));
    }

    #[test]
    fn prepare_request_without_privkey_omits_signature() {
        let auth = GmgnAuth::new("mykey", None);
        assert!(!auth.can_sign());
        let params = BTreeMap::new();
        let (_url, headers) = auth.prepare_request("https://openapi.gmgn.ai", "/v1/market/rank", &params, "");
        assert!(headers.iter().any(|h| h.name == "X-APIKEY"));
        assert!(!headers.iter().any(|h| h.name == "X-Signature"));
    }

    #[test]
    fn prepare_request_bad_pem_falls_back_to_unsigned() {
        let auth = GmgnAuth::new("mykey", Some("not a pem"));
        assert!(!auth.can_sign());
    }
}
