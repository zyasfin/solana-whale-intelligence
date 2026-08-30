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
use std::collections::BTreeMap;
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

/// GMGN query routes mapped to weights.
pub fn route_weight(route: &str) -> RouteWeight {
    match route {
        "trenches" | "trending" => RouteWeight::Heavy,
        "signal" | "smartmoney" | "kol" => RouteWeight::VeryHeavy,
        "info" | "security" => RouteWeight::Light,
        "pool" | "holders" | "traders" => RouteWeight::Standard,
        "stats" | "profits" => RouteWeight::Heavy,
        "activity" | "created_tokens" => RouteWeight::Standard,
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
        let timestamp = Utc::now().timestamp_millis().to_string();
        let client_id = uuid::Uuid::new_v4().to_string();
        Ok(vec![
            HeaderPair { name: "X-APIKEY".to_string(), value: api_key.to_string() },
            HeaderPair { name: "X-TIMESTAMP".to_string(), value: timestamp },
            HeaderPair { name: "X-CLIENT-ID".to_string(), value: client_id },
            HeaderPair { name: "Content-Type".to_string(), value: "application/json".to_string() },
        ])
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
        assert_eq!(route_weight("trenches"), RouteWeight::Heavy);
        assert_eq!(route_weight("signal"), RouteWeight::VeryHeavy);
        assert_eq!(route_weight("info"), RouteWeight::Light);
        assert_eq!(route_weight("pool"), RouteWeight::Standard);
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
}
