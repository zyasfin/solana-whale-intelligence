//! Password-only admin authentication with HttpOnly session cookies.
//!
//! The password hash is read from `ADMIN_PASSWORD_HASH` (bcrypt). Sessions are
//! random tokens stored hashed (SHA-256) in `admin_sessions` with expiry.

#![allow(dead_code)]  // hash_password used by a setup helper; prune_sessions by maintenance

use anyhow::{anyhow, Result};
use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

pub const SESSION_COOKIE: &str = "swi_session";
pub const SESSION_TTL_HOURS: i64 = 24;
/// Max failed login attempts per key within the window before limiting.
pub const MAX_FAILURES: u32 = 5;
/// Sliding window for counting failures, in minutes.
pub const WINDOW_MINUTES: i64 = 15;

/// Hash a password with bcrypt (cost 12).
pub fn hash_password(password: &str) -> Result<String> {
    bcrypt::hash(password, 12).map_err(|e| anyhow!("bcrypt hash failed: {e}"))
}

/// Verify a candidate password against a bcrypt hash.
pub fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// Generate a random session token (hex-encoded 32 bytes).
pub fn new_session_token() -> String {
    let mut bytes = [0u8; 32];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// SHA-256 hash of the session token (stored, never the raw token).
fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Create a session; returns the raw token (send to client once).
pub async fn create_session(pool: &PgPool, ip: Option<&str>, user_agent: Option<&str>) -> Result<String> {
    let token = new_session_token();
    let hash = token_hash(&token);
    let expires_at = Utc::now() + Duration::hours(SESSION_TTL_HOURS);
    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, expires_at, ip, user_agent) VALUES ($1, $2, $3, $4)",
    )
    .bind(&hash)
    .bind(expires_at)
    .bind(ip)
    .bind(user_agent)
    .execute(pool)
    .await?;
    Ok(token)
}

/// Validate a session token; refreshes last_seen. Returns true when valid.
pub async fn validate_session(pool: &PgPool, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let hash = token_hash(token);
    let result = sqlx::query(
        "UPDATE admin_sessions SET last_seen_at = now() WHERE token_hash = $1 AND expires_at > now()",
    )
    .bind(&hash)
    .execute(pool)
    .await;
    match result {
        Ok(r) => r.rows_affected() > 0,
        Err(_) => false,
    }
}

/// Destroy a session (logout).
pub async fn destroy_session(pool: &PgPool, token: &str) -> Result<()> {
    let hash = token_hash(token);
    sqlx::query("DELETE FROM admin_sessions WHERE token_hash = $1")
        .bind(&hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove expired sessions (maintenance).
pub async fn prune_sessions(pool: &PgPool) -> Result<u64> {
    let r = sqlx::query("DELETE FROM admin_sessions WHERE expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}
/// Rate-limit key for a client IP ("ip:<ip>", or "global" when unknown).
pub fn rate_limit_key(ip: Option<&str>) -> String {
    match ip {
        Some(ip) if !ip.trim().is_empty() => format!("ip:{}", ip.trim()),
        _ => "global".to_string(),
    }
}

/// Pure threshold check (kept separate for unit testing).
fn threshold_reached(failures: u32) -> bool {
    failures >= MAX_FAILURES
}

/// True when `key` has >= MAX_FAILURES failed attempts within the last
/// WINDOW_MINUTES minutes.
pub async fn is_rate_limited(pool: &PgPool, key: &str) -> Result<bool> {
    let row: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM login_attempts \
         WHERE key = $1 AND NOT success AND attempted_at > now() - ($2 || ' minutes')::interval",
    )
    .bind(key)
    .bind(WINDOW_MINUTES)
    .fetch_one(pool)
    .await?;
    Ok(threshold_reached(row.0 as u32))
}

/// Record a login attempt for `key`.
pub async fn record_attempt(
    pool: &PgPool,
    key: &str,
    success: bool,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO login_attempts (key, success, ip, user_agent) VALUES ($1, $2, $3, $4)",
    )
    .bind(key)
    .bind(success)
    .bind(ip)
    .bind(user_agent)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete failed attempts for `key` (call on successful login).
pub async fn clear_attempts(pool: &PgPool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM login_attempts WHERE key = $1 AND NOT success")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete attempt rows older than 24h (maintenance).
pub async fn prune_attempts(pool: &PgPool) -> Result<u64> {
    let r = sqlx::query("DELETE FROM login_attempts WHERE attempted_at < now() - interval '24 hours'")
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// Read the bcrypt password hash.
///
/// Prefer `ADMIN_PASSWORD_HASH_B64` (base64) because dotenvy performs `$VAR`
/// substitution and bcrypt hashes contain `$` characters that would be mangled.
/// Falls back to `ADMIN_PASSWORD_HASH` when set in the real process env.
pub fn password_hash() -> Option<String> {
    if let Ok(b64) = std::env::var("ADMIN_PASSWORD_HASH_B64") {
        if !b64.trim().is_empty() {
            use base64::Engine;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                if let Ok(hash) = String::from_utf8(bytes) {
                    return Some(hash);
                }
            }
        }
    }
    std::env::var("ADMIN_PASSWORD_HASH")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Whether admin auth is configured (password hash available).
pub fn auth_configured() -> bool {
    password_hash().is_some()
}

/// Build the Set-Cookie header value for the session.
pub fn session_cookie_value(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        SESSION_TTL_HOURS * 3600
    )
}

/// Clear-cookie value for logout.
pub fn clear_cookie_value() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong", &hash));
        // Bcrypt hashes are salted: same password gives different hashes.
        let hash2 = hash_password("correct horse").unwrap();
        assert_ne!(hash, hash2);
    }

    #[test]
    fn session_token_is_random_and_long() {
        let a = new_session_token();
        let b = new_session_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn token_hash_is_sha256_hex() {
        let h = token_hash("abc");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cookie_values() {
        assert!(session_cookie_value("tok").contains("swi_session=tok"));
        assert!(session_cookie_value("tok").contains("HttpOnly"));
        assert!(clear_cookie_value().contains("Max-Age=0"));
    }
    #[test]
    fn rate_limit_key_formats() {
        assert_eq!(rate_limit_key(Some("1.2.3.4")), "ip:1.2.3.4");
        assert_eq!(rate_limit_key(Some("  10.0.0.1 ")), "ip:10.0.0.1");
        assert_eq!(rate_limit_key(None), "global");
        assert_eq!(rate_limit_key(Some("")), "global");
        assert_eq!(rate_limit_key(Some("   ")), "global");
    }

    #[test]
    fn rate_limit_thresholds_are_sensible() {
        assert!(MAX_FAILURES >= 3 && MAX_FAILURES <= 10);
        assert!(WINDOW_MINUTES >= 5 && WINDOW_MINUTES <= 60);
        assert!(!threshold_reached(0));
        assert!(!threshold_reached(MAX_FAILURES - 1));
        assert!(threshold_reached(MAX_FAILURES));
        assert!(threshold_reached(MAX_FAILURES + 10));
    }
}
