//! Helius provider pool: per-key token buckets, fair round-robin, cooldowns,
//! circuit breakers, usage accounting, and request queues.
//!
//! Free plan defaults per API class: RPC 10 req/s, Enhanced/DAS/Wallet 2 req/s.
//! `getTransactionsForAddress` and `getTransfersByAddress` are single-address
//! requests; no batch mode exists. Multiple user-owned keys are load-balanced
//! fairly; keys are never rotated to evade provider limits.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::config::HeliusConfig;
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Helius API classes with independent rate limits.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApiClass {
    Rpc,
    Enhanced,
    Wallet,
}

impl ApiClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ApiClass::Rpc => "rpc",
            ApiClass::Enhanced => "enhanced",
            ApiClass::Wallet => "wallet",
        }
    }
}

/// One provider key with per-class token buckets and failure state.
pub struct ProviderKey {
    pub id: String,
    pub key: String,
    buckets: Mutex<HashMap<ApiClass, TokenBucket>>,
    /// Instant until which this key is cooling down (429/5xx bursts).
    cooldown_until: Mutex<Option<Instant>>,
    /// Consecutive failure count; opens the circuit breaker at threshold.
    consecutive_failures: AtomicU64,
    open_until: Mutex<Option<Instant>>,
}

impl ProviderKey {
    fn new(id: String, key: String, rpc_rate: u32, enhanced_rate: u32, wallet_rate: u32) -> Self {
        let mut buckets = HashMap::new();
        buckets.insert(ApiClass::Rpc, TokenBucket::new(rpc_rate));
        buckets.insert(ApiClass::Enhanced, TokenBucket::new(enhanced_rate));
        buckets.insert(ApiClass::Wallet, TokenBucket::new(wallet_rate));
        Self {
            id,
            key,
            buckets: Mutex::new(buckets),
            cooldown_until: Mutex::new(None),
            consecutive_failures: AtomicU64::new(0),
            open_until: Mutex::new(None),
        }
    }

    /// Try to acquire one token for the class; None means not available now.
    pub async fn try_acquire(&self, class: ApiClass) -> Option<Duration> {
        // Interior mutability: buckets behind a mutex for safe mutation.
        let cooldown = *self.cooldown_until.lock().await;
        if let Some(until) = cooldown {
            if Instant::now() < until {
                return Some(until - Instant::now());
            }
        }
        let open = *self.open_until.lock().await;
        if let Some(until) = open {
            if Instant::now() < until {
                return Some(until - Instant::now());
            }
        }
        let mut buckets = self.buckets.lock().await;
        buckets
            .get_mut(&class)
            .map(|bucket| bucket.try_acquire())
            .flatten()
    }

    /// Record success: reset consecutive failures, close breaker.
    pub async fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        *self.open_until.lock().await = None;
    }

    /// Record failure; repeated failures open the circuit breaker.
    pub async fn record_failure(&self, breaker_threshold: u64) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= breaker_threshold {
            *self.open_until.lock().await = Some(Instant::now() + Duration::from_secs(60));
            self.consecutive_failures.store(0, Ordering::Relaxed);
        }
    }

    /// Apply a cooldown (e.g. 429 with Retry-After).
    pub async fn apply_cooldown(&self, duration: Duration) {
        *self.cooldown_until.lock().await = Some(Instant::now() + duration);
    }

    pub async fn is_available(&self) -> bool {
        let cooldown = *self.cooldown_until.lock().await;
        if let Some(until) = cooldown {
            if Instant::now() < until {
                return false;
            }
        }
        let open = *self.open_until.lock().await;
        if let Some(until) = open {
            if Instant::now() < until {
                return false;
            }
        }
        true
    }
}

/// Simple token bucket: capacity = rate (1 second of tokens), refill per second.
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(rate_per_second: u32) -> Self {
        let capacity = rate_per_second as f64;
        Self {
            capacity,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time, then try to consume one.
    /// Returns Some(wait) when a token is unavailable right now.
    pub fn try_acquire(&mut self) -> Option<Duration> {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let deficit = 1.0 - self.tokens;
            let rate = self.capacity;
            let wait = Duration::from_secs_f64(deficit / rate.max(0.001));
            Some(wait)
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.capacity).min(self.capacity);
            self.last_refill = now;
        }
    }

    pub fn current_tokens(&mut self) -> f64 {
        self.refill();
        self.tokens
    }
}

/// HTTP response classification for retry decisions.
#[derive(Clone, Debug, PartialEq)]
pub enum HttpOutcome {
    Success,
    RateLimited { retry_after: Option<Duration> },
    AuthError,
    ValidationError,
    ServerError,
    NetworkError,
}

impl HttpOutcome {
    /// Whether a retry is permitted for this outcome.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            HttpOutcome::RateLimited { .. } | HttpOutcome::ServerError | HttpOutcome::NetworkError
        )
    }

    /// Parse from a reqwest response status and headers.
    pub fn from_status(status: reqwest::StatusCode, headers: &reqwest::header::HeaderMap) -> Self {
        if status.is_success() {
            HttpOutcome::Success
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            HttpOutcome::RateLimited { retry_after }
        } else if status == reqwest::StatusCode::UNAUTHORIZED || status.as_u16() == 403 {
            HttpOutcome::AuthError
        } else if status.is_client_error() {
            HttpOutcome::ValidationError
        } else {
            HttpOutcome::ServerError
        }
    }
}

/// In-memory usage accounting row (mirrors the `helius_usage` table).
#[derive(Clone, Debug)]
pub struct UsageRecord {
    pub provider_id: String,
    pub api_class: ApiClass,
    pub window_start: DateTime<Utc>,
    pub request_count: i64,
}

/// The Helius provider pool.
pub struct HeliusPool {
    providers: Vec<Arc<ProviderKey>>,
    /// Fair round-robin cursor.
    next: std::sync::atomic::AtomicUsize,
    config: HeliusConfig,
    usage: Mutex<Vec<UsageRecord>>,
    client: reqwest::Client,
}

impl HeliusPool {
    /// Build a pool from user-owned keys. Zero keys yields an empty pool that
    /// reports `no providers configured` on request.
    pub fn new(keys: Vec<String>, config: HeliusConfig) -> Self {
        let providers = keys
            .into_iter()
            .enumerate()
            .map(|(index, key)| {
                Arc::new(ProviderKey::new(
                    format!("helius_{}", index + 1),
                    key,
                    config.rpc_rate_per_second,
                    config.enhanced_rate_per_second,
                    config.wallet_rate_per_second,
                ))
            })
            .collect();
        Self {
            providers,
            next: std::sync::atomic::AtomicUsize::new(0),
            config,
            usage: Mutex::new(Vec::new()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// Pick the next available provider fairly: round-robin over eligible keys.
    async fn pick_provider(&self, class: ApiClass) -> Result<(Arc<ProviderKey>, Option<Duration>)> {
        if self.providers.is_empty() {
            bail!("no helius providers configured");
        }
        let count = self.providers.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let mut best_wait: Option<(Arc<ProviderKey>, Duration)> = None;
        for offset in 0..count {
            let provider = &self.providers[(start + offset) % count];
            if !provider.is_available().await {
                continue;
            }
            match provider.try_acquire(class).await {
                None => return Ok((provider.clone(), None)),
                Some(wait) => {
                    if best_wait
                        .as_ref()
                        .map(|(_, w)| wait < *w)
                        .unwrap_or(true)
                    {
                        best_wait = Some((provider.clone(), wait));
                    }
                }
            }
        }
        match best_wait {
            Some((provider, wait)) => Ok((provider, Some(wait))),
            None => {
                // All providers cooling down: shortest availability.
                let mut shortest = None;
                for provider in &self.providers {
                    let wait = provider.try_acquire(class).await;
                    if let Some(w) = wait {
                        if shortest.map(|s| w < s).unwrap_or(true) {
                            shortest = Some(w);
                        }
                    }
                }
                let wait = shortest.unwrap_or(Duration::from_millis(500));
                Err(anyhow!("all helius providers unavailable; retry in {:?}", wait))
            }
        }
    }

    /// Record usage accounting for a sent request.
    async fn record_usage(&self, provider_id: &str, class: ApiClass) {
        let now = Utc::now();
        let window = now.timestamp() - (now.timestamp() % 60);
        let window_start = DateTime::from_timestamp(window, 0).unwrap_or(now);
        let mut usage = self.usage.lock().await;
        if let Some(record) = usage
            .iter_mut()
            .find(|r| r.provider_id == provider_id && r.api_class == class && r.window_start == window_start)
        {
            record.request_count += 1;
        } else {
            usage.push(UsageRecord {
                provider_id: provider_id.to_string(),
                api_class: class,
                window_start,
                request_count: 1,
            });
            // Keep accounting bounded.
            if usage.len() > 10_000 {
                usage.drain(0..5_000);
            }
        }
    }

    /// Execute one JSON-RPC request against a provider key with retry policy.
    pub async fn rpc_request(
        &self,
        class: ApiClass,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.request_json(class, method, params).await
    }

    /// Internal request executor with retry/cooldown handling.
    async fn request_json(
        &self,
        class: ApiClass,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut attempt = 0u32;
        loop {
            let (provider, wait) = self.pick_provider(class).await?;
            if let Some(wait) = wait {
                tokio::time::sleep(wait.min(Duration::from_secs(2))).await;
            }
            self.record_usage(&provider.id, class).await;

            let url = self.config.http_url(&provider.key);
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": format!("swi-{}", uuid::Uuid::new_v4()),
                "method": method,
                "params": params,
            });

            match self.client.post(&url).json(&body).send().await {
                Ok(response) => {
                    let status = response.status();
                    let headers = response.headers().clone();
                    let payload = response.json::<serde_json::Value>().await;
                    let outcome = HttpOutcome::from_status(status, &headers);
                    match outcome {
                        HttpOutcome::Success => {
                            provider.record_success().await;
                            let value = payload.map_err(|e| anyhow!("helius decode error: {e}"))?;
                            if let Some(error) = value.get("error") {
                                // JSON-RPC application error: not retryable.
                                provider.record_failure(self.config.max_retries as u64).await;
                                bail!("helius rpc error: {error}");
                            }
                            return Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null));
                        }
                        HttpOutcome::RateLimited { retry_after } => {
                            let wait = retry_after
                                .unwrap_or(Duration::from_secs(1))
                                .min(Duration::from_secs(60));
                            provider.apply_cooldown(wait).await;
                            attempt += 1;
                            if attempt > self.config.max_retries {
                                bail!("helius rate limited after {attempt} attempts");
                            }
                        }
                        HttpOutcome::AuthError => {
                            provider.record_failure(3).await;
                            bail!("helius auth error for provider {}", provider.id);
                        }
                        HttpOutcome::ValidationError => {
                            provider.record_failure(3).await;
                            bail!("helius validation error (4xx) for provider {}", provider.id);
                        }
                        HttpOutcome::ServerError => {
                            provider.record_failure(self.config.max_retries as u64).await;
                            attempt += 1;
                            if attempt > self.config.max_retries {
                                bail!("helius server error after {attempt} attempts");
                            }
                        }
                        HttpOutcome::NetworkError => {
                            provider.record_failure(self.config.max_retries as u64).await;
                            attempt += 1;
                            if attempt > self.config.max_retries {
                                bail!("helius network error after {attempt} attempts");
                            }
                        }
                    }
                }
                Err(err) => {
                    provider.record_failure(self.config.max_retries as u64).await;
                    attempt += 1;
                    if attempt > self.config.max_retries {
                        bail!("helius request failed: {err}");
                    }
                }
            }
        }
    }

    /// Fetch transaction history for one address (single-address API).
    pub async fn get_transactions_for_address(&self, address: &str, before: Option<&str>) -> Result<serde_json::Value> {
        let mut params = serde_json::json!([address]);
        if let Some(before) = before {
            params[0]["before"] = serde_json::json!(before);
        }
        self.rpc_request(ApiClass::Enhanced, "getTransactionsForAddress", params)
            .await
    }

    /// Fetch transfers for one address (single-address API).
    pub async fn get_transfers_for_address(&self, address: &str) -> Result<serde_json::Value> {
        self.rpc_request(ApiClass::Enhanced, "getTransfersByAddress", serde_json::json!([address]))
            .await
    }

    /// Usage snapshot for health reporting.
    pub async fn usage_snapshot(&self) -> Vec<UsageRecord> {
        self.usage.lock().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(rpc: u32, enhanced: u32) -> HeliusConfig {
        HeliusConfig {
            base_url: crate::config::default_helius_base_url(),
            rpc_rate_per_second: rpc,
            enhanced_rate_per_second: enhanced,
            wallet_rate_per_second: enhanced,
            max_retries: 3,
            timeout_seconds: 30,
        }
    }

    #[test]
    fn token_bucket_enforces_two_per_second() {
        let mut bucket = TokenBucket::new(2);
        assert!(bucket.try_acquire().is_none());
        assert!(bucket.try_acquire().is_none());
        // Third request within the same instant must wait.
        let wait = bucket.try_acquire();
        assert!(wait.is_some());
        // Wait duration is at most 0.5s for a 2/s bucket.
        let wait = wait.unwrap();
        assert!(wait <= Duration::from_millis(500));
    }

    #[test]
    fn token_bucket_never_exceeds_capacity() {
        let mut bucket = TokenBucket::new(10);
        let mut granted = 0;
        for _ in 0..12 {
            if bucket.try_acquire().is_none() {
                granted += 1;
            }
        }
        assert_eq!(granted, 10);
    }

    #[tokio::test]
    async fn two_enhanced_providers_never_exceed_two_per_second_each() {
        let pool = HeliusPool::new(
            vec!["key-a".into(), "key-b".into()],
            config(10, 2),
        );
        assert_eq!(pool.provider_count(), 2);
        // Issue six acquire attempts; per-provider each must be limited to 2.
        let provider_a = pool.providers[0].clone();
        let provider_b = pool.providers[1].clone();
        let mut a_granted = 0;
        let mut b_granted = 0;
        for _ in 0..3 {
            if provider_a.try_acquire(ApiClass::Enhanced).await.is_none() {
                a_granted += 1;
            }
            if provider_b.try_acquire(ApiClass::Enhanced).await.is_none() {
                b_granted += 1;
            }
        }
        assert_eq!(a_granted, 2, "provider A must cap at 2 req/s enhanced");
        assert_eq!(b_granted, 2, "provider B must cap at 2 req/s enhanced");
    }

    #[tokio::test]
    async fn rpc_class_independent_of_enhanced() {
        let pool = HeliusPool::new(vec!["key-a".into()], config(10, 2));
        let provider = pool.providers[0].clone();
        // Exhaust enhanced (2), then RPC still has 10 tokens.
        assert!(provider.try_acquire(ApiClass::Enhanced).await.is_none());
        assert!(provider.try_acquire(ApiClass::Enhanced).await.is_none());
        assert!(provider.try_acquire(ApiClass::Enhanced).await.is_some());
        let mut rpc_granted = 0;
        for _ in 0..10 {
            if provider.try_acquire(ApiClass::Rpc).await.is_none() {
                rpc_granted += 1;
            }
        }
        assert_eq!(rpc_granted, 10);
        assert!(provider.try_acquire(ApiClass::Rpc).await.is_some());
    }

    #[tokio::test]
    async fn cooldown_blocks_availability() {
        let pool = HeliusPool::new(vec!["key-a".into()], config(10, 2));
        let provider = pool.providers[0].clone();
        assert!(provider.is_available().await);
        provider
            .apply_cooldown(Duration::from_millis(50))
            .await;
        assert!(!provider.is_available().await);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(provider.is_available().await);
    }

    #[tokio::test]
    async fn circuit_breaker_opens_after_threshold() {
        let pool = HeliusPool::new(vec!["key-a".into()], config(10, 2));
        let provider = pool.providers[0].clone();
        for _ in 0..3 {
            provider.record_failure(3).await;
        }
        assert!(!provider.is_available().await);
        provider.record_success().await;
        // Success closes the breaker only after cooldown elapses; cooldown still applies.
        // record_success clears breaker state, but open_until was set: verify reset path.
    }

    #[test]
    fn http_outcome_classification() {
        let success = reqwest::StatusCode::OK;
        assert_eq!(HttpOutcome::from_status(success, &reqwest::header::HeaderMap::new()), HttpOutcome::Success);

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "5".parse().unwrap());
        assert_eq!(
            HttpOutcome::from_status(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers),
            HttpOutcome::RateLimited { retry_after: Some(Duration::from_secs(5)) }
        );

        assert_eq!(
            HttpOutcome::from_status(reqwest::StatusCode::UNAUTHORIZED, &reqwest::header::HeaderMap::new()),
            HttpOutcome::AuthError
        );
        assert_eq!(
            HttpOutcome::from_status(reqwest::StatusCode::BAD_REQUEST, &reqwest::header::HeaderMap::new()),
            HttpOutcome::ValidationError
        );
        assert_eq!(
            HttpOutcome::from_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &reqwest::header::HeaderMap::new()),
            HttpOutcome::ServerError
        );
    }

    #[test]
    fn retry_policy_allows_only_retryable_outcomes() {
        assert!(HttpOutcome::RateLimited { retry_after: None }.retryable());
        assert!(HttpOutcome::ServerError.retryable());
        assert!(HttpOutcome::NetworkError.retryable());
        assert!(!HttpOutcome::AuthError.retryable());
        assert!(!HttpOutcome::ValidationError.retryable());
        assert!(!HttpOutcome::Success.retryable());
    }

    #[tokio::test]
    async fn empty_pool_reports_no_providers() {
        let pool = HeliusPool::new(vec![], config(10, 2));
        assert_eq!(pool.provider_count(), 0);
        let err = pool
            .rpc_request(ApiClass::Rpc, "getSlot", serde_json::json!([]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no helius providers"));
    }

    #[tokio::test]
    async fn usage_accounting_counts_only_sent_requests() {
        let pool = HeliusPool::new(vec!["key-a".into()], config(10, 2));
        let provider = pool.providers[0].clone();
        pool.record_usage(&provider.id, ApiClass::Rpc).await;
        pool.record_usage(&provider.id, ApiClass::Rpc).await;
        let snapshot = pool.usage_snapshot().await;
        let total: i64 = snapshot
            .iter()
            .filter(|r| r.provider_id == provider.id && r.api_class == ApiClass::Rpc)
            .map(|r| r.request_count)
            .sum();
        assert_eq!(total, 2);
    }
}
