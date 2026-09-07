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

/// Resolve the workspace a new session should be bound to.
///
/// Migration 1020 added `admin_sessions.workspace_id`, and the Recent API derives
/// the workspace from it (never a hardcoded literal). But `create_session` never
/// populated the column, so every normal login produced a session with a NULL
/// binding and the Recent endpoints returned 401 forever (REV-027-F04 /
/// REV-028-F09).
///
/// Binding rule (single-tenant admin panel), enforced in this order:
///   1. exactly one active workspace  -> bind to it;
///   2. several active workspaces, one slugged `default` -> bind to `default`;
///   3. otherwise (several actives, no `default`) -> `None`, session stays
///      unbound and workspace-scoped APIs reject it with 401.
///
/// Case 3 is the point: REV-028 documented this rule but implemented
/// `ORDER BY ... LIMIT 1`, which always picked the first active workspace and so
/// silently bound the session to an arbitrary tenant (REV-029/REV-025-F04). The
/// selection is now explicit — an ambiguous tenant is never guessed.
/// The selection rule as a PURE function, so it is testable without PostgreSQL
/// (the previous version hid the rule inside SQL, which is exactly why the
/// implementation could drift from its own doc comment unnoticed).
///
/// `default_id` — id of an active workspace slugged `default`, when present.
/// `active_ids` — ids of active workspaces, capped at 2 by the caller (only
/// "none / one / more than one" matters).
fn select_workspace(default_id: Option<i64>, active_ids: &[i64]) -> Option<i64> {
    if let Some(id) = default_id {
        return Some(id);
    }
    match active_ids {
        [only] => Some(*only),
        _ => None, // zero, or ambiguous with no `default` -> never guess
    }
}

async fn workspace_for_new_session(
    conn: &mut sqlx::PgConnection,
) -> Result<Option<i64>> {
    // REV-045-F02 (swept beyond the reported site): a swallowed error here was
    // fail-CLOSED in effect — the session ends up unbound, which denies rather than
    // grants. But it was also SILENT, so a permission or connectivity failure looked
    // identical to "no default workspace exists".
    //
    // REV-062-F07: the function used to return `Option<i64>` and fold a query error
    // into `None`, which misclassifies an operational failure as "no workspace" —
    // the session is unbound and the operator sees a 401 with no cause. It now
    // returns `Result` so `create_session` propagates a DB error as a real failure
    // (login returns 500) instead of silently binding an unbound session.
    //
    // REV-067-F07: takes a CONNECTION, not a pool, so the whole successful-login
    // sequence can run inside ONE transaction; a pool-only signature is what forced
    // three separately committed writes.
    let default_id: Option<i64> =
        sqlx::query_as::<_, (i64,)>(
            "SELECT id FROM workspaces WHERE status = 'active' AND slug = 'default'",
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|(id,)| id);

    // Fetch two rows so "more than one" is detectable.
    let active_ids: Vec<i64> = sqlx::query_as::<_, (i64,)>(
        "SELECT id FROM workspaces WHERE status = 'active' ORDER BY id ASC LIMIT 2",
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(id,)| id)
    .collect();

    let selected = select_workspace(default_id, &active_ids);
    if selected.is_none() && active_ids.len() > 1 {
        tracing::warn!(
            active_workspaces = active_ids.len(),
            "ambiguous workspace selection and no `default` workspace; session left unbound (fail-closed)"
        );
    }
    Ok(selected)
}

/// Create a session; returns the raw token (send to client once).
///
/// The session is bound to a workspace at creation time so downstream
/// workspace-scoped APIs can authorize without a literal (REV-028-F09).
pub async fn create_session(pool: &PgPool, ip: Option<&str>, user_agent: Option<&str>) -> Result<String> {
    let mut tx = pool.begin().await?;
    let token = create_session_in(&mut tx, ip, user_agent).await?;
    tx.commit().await?;
    Ok(token)
}

/// Everything a SUCCESSFUL login writes, in ONE transaction.
///
/// REV-067-F07: `record_attempt(true)`, `clear_attempts`, and `create_session` used
/// to be three separately committed statements. A failure in the second or third
/// returned 500 with no cookie — correct as a response — while the writes that had
/// already committed stayed. The observable result is a store whose audit trail and
/// failure counter describe a login that, as far as the client is concerned, never
/// happened. Either all three land or none do.
///
/// The rate-limit READ stays outside: it decides whether to attempt the login at
/// all, and holding a transaction open across it would serialize every login attempt
/// on one row for no correctness gain.
pub async fn establish_session(
    pool: &PgPool,
    key: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<String> {
    let mut tx = pool.begin().await?;
    record_attempt_in(&mut tx, key, true, ip, user_agent).await?;
    clear_attempts_in(&mut tx, key).await?;
    let token = create_session_in(&mut tx, ip, user_agent).await?;
    tx.commit().await?;
    Ok(token)
}

async fn create_session_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<String> {
    let token = new_session_token();
    let hash = token_hash(&token);
    let expires_at = Utc::now() + Duration::hours(SESSION_TTL_HOURS);
    // REV-062-F07: a workspace-lookup DB error now propagates (the login fails with
    // 500) rather than being folded into an unbound session, which misclassified an
    // operational failure as "no workspace". An ambiguous/absent workspace is still
    // NOT fatal — the legacy panel is workspace-agnostic — but a store failure is.
    let workspace_id = workspace_for_new_session(&mut **tx).await?;
    if workspace_id.is_none() {
        // Not fatal for the legacy admin panel (which is workspace-agnostic), but
        // workspace-scoped endpoints will fail closed with 401 until a workspace
        // exists. Surface it instead of failing silently.
        tracing::warn!(
            "no active workspace to bind the session to; workspace-scoped APIs will reject this session"
        );
    }
    sqlx::query(
        "INSERT INTO admin_sessions (token_hash, expires_at, ip, user_agent, workspace_id) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&hash)
    .bind(expires_at)
    .bind(ip)
    .bind(user_agent)
    .bind(workspace_id)
    .execute(&mut **tx)
    .await?;
    Ok(token)
}

/// Validate a session token; refreshes last_seen. Returns `Ok(true)` when valid,
/// `Ok(false)` when the session is invalid/expired (a normal, expected answer),
/// and `Err(_)` when the database itself failed — a query error must NOT read as
/// "this session is bad".
///
/// REV-060-F06: this used to return `bool` and turn any query error into `false`,
/// so an outage, a missing table, or a permission denial before the policy lookup
/// became a 401 as if the session were invalid. That masks a real failure as an
/// authentication one, and an operator sees hundreds of 401s instead of a 500 that
/// says the policy store is down. `None`/`false` is a client-side answer; `Err` is
/// a server-side one, and only the former may be a 401.
pub async fn validate_session(pool: &PgPool, token: &str) -> Result<bool> {
    if token.is_empty() {
        return Ok(false);
    }
    let hash = token_hash(token);
    let result = sqlx::query(
        "UPDATE admin_sessions SET last_seen_at = now() WHERE token_hash = $1 AND expires_at > now()",
    )
    .bind(&hash)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}
/// Resolve the workspace bound to a valid session. Returns `Ok(Some(id))` when the
/// session is valid and bound, `Ok(None)` when it is invalid, expired, or carries no
/// workspace binding (REV-025-F04) — a normal, expected answer — and `Err(_)` when
/// the database itself failed.
///
/// REV-060-F06: this used to return `Option<i64>` and `.ok().flatten()` a query error
/// into `None`, so a DB failure before the policy lookup became a 401 exactly as if the
/// session were invalid. `None` is a client-side answer (bad or unbound session); `Err`
/// is a server-side one (the store is unreachable), and only the former may be a 401.
/// A workspace is derived from the authenticated session, never a hardcoded literal.
pub async fn workspace_for_session(pool: &PgPool, token: &str) -> Result<Option<i64>> {
    if token.is_empty() {
        return Ok(None);
    }
    let hash = token_hash(token);
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT workspace_id FROM admin_sessions \
         WHERE token_hash = $1 AND expires_at > now() AND workspace_id IS NOT NULL",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(ws,)| ws))
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
    let mut tx = pool.begin().await?;
    record_attempt_in(&mut tx, key, success, ip, user_agent).await?;
    tx.commit().await?;
    Ok(())
}

/// `record_attempt` inside an existing transaction (REV-067-F07).
async fn record_attempt_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Delete failed attempts for `key` (call on successful login).
pub async fn clear_attempts(pool: &PgPool, key: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    clear_attempts_in(&mut tx, key).await?;
    tx.commit().await?;
    Ok(())
}

/// `clear_attempts` inside an existing transaction (REV-067-F07).
async fn clear_attempts_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM login_attempts WHERE key = $1 AND NOT success")
        .bind(key)
        .execute(&mut **tx)
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
    // REV-029/REV-025-F04: an ambiguous tenant must NEVER be guessed. REV-028
    // documented this rule but implemented `ORDER BY ... LIMIT 1`, which always
    // picked the first active workspace.
    #[test]
    fn ambiguous_workspace_selection_fails_closed() {
        // Exactly one active workspace -> unambiguous, bind it.
        assert_eq!(select_workspace(None, &[7]), Some(7));
        // A `default` workspace always wins, even with several actives.
        assert_eq!(select_workspace(Some(3), &[1, 2]), Some(3));
        // Several actives and NO `default` -> unbound, never the first one.
        assert_eq!(select_workspace(None, &[1, 2]), None);
        // No workspace at all -> unbound.
        assert_eq!(select_workspace(None, &[]), None);
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
