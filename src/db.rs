//! Database connection pooling and migration support.

#![allow(dead_code)]  // helper API used by workers and tests

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use std::str::FromStr;
use std::time::Duration;

/// Connect to PostgreSQL using a `DATABASE_URL`-style connection string.
///
/// Accepts `postgres://` and `postgresql://` schemes. Pool size follows the
/// runtime profile: `low` keeps a small pool for 2 vCPU machines.
pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool> {
    let options = PgConnectOptions::from_str(database_url)
        .with_context(|| "invalid DATABASE_URL")?
        .ssl_mode(PgSslMode::Prefer);
    let pool = PgPoolOptions::new()
        .max_connections(max_connections.max(2))
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(600))
        .connect_with(options)
        .await
        .with_context(|| "failed to connect to PostgreSQL")?;
    Ok(pool)
}

/// Default pool size per runtime profile.
pub fn pool_size(scale_workers: usize) -> u32 {
    (scale_workers as u32 * 2).clamp(2, 16)
}

/// Apply migrations by executing SQL files under `./migrations` in order.
///
/// Each file runs inside a transaction; an applied file is recorded in the
/// `_migrations` table so re-runs are no-ops.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (name text PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())",
    )
    .execute(pool)
    .await?;
    let mut dir = std::path::PathBuf::from("migrations");
    if !dir.exists() {
        if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
            dir = std::path::Path::new(&manifest_dir).join("migrations");
        }
    }
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("failed to read migrations dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "sql").unwrap_or(false))
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid migration filename"))?
            .to_string();
        let applied: Option<(String,)> =
            sqlx::query_as("SELECT name FROM _migrations WHERE name = $1")
                .bind(&name)
                .fetch_optional(pool)
                .await?;
        if applied.is_some() {
            continue;
        }
        let sql = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(&sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO _migrations (name) VALUES ($1)")
            .bind(&name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(migration = %name, "applied");
    }
    Ok(())
}

/// Measure database latency in milliseconds (health check).
pub async fn latency_ms(pool: &PgPool) -> Result<f64> {
    let start = std::time::Instant::now();
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// Insert or ignore a raw provider payload BEFORE normalization.
///
/// Raw-first guarantee: returns true when the row was newly inserted, false
/// when a duplicate delivery was ignored (idempotency by signature).
pub async fn store_raw_event(
    pool: &PgPool,
    chain: &str,
    source: &str,
    signature: &str,
    payload: &serde_json::Value,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO raw_events (chain, source, signature, payload, observed_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (chain, source, signature) DO NOTHING
        "#,
    )
    .bind(chain)
    .bind(source)
    .bind(signature)
    .bind(payload)
    .bind(observed_at)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Upsert a wallet row (first_seen preserved on conflict).
pub async fn upsert_wallet(
    pool: &PgPool,
    chain: &str,
    address: &str,
    seen_at: chrono::DateTime<chrono::Utc>,
    source: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO wallets (chain, address, first_seen, last_seen, source)
        VALUES ($1, $2, $3, $3, $4)
        ON CONFLICT (chain, address) DO UPDATE
            SET last_seen = GREATEST(wallets.last_seen, EXCLUDED.last_seen),
                source = EXCLUDED.source
        "#,
    )
    .bind(chain)
    .bind(address)
    .bind(seen_at)
    .bind(source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Compute wallet age in seconds from first_seen; None when unknown.
pub async fn wallet_age_seconds(
    pool: &PgPool,
    chain: &str,
    address: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<i64>> {
    let row: Option<(chrono::DateTime<chrono::Utc>,)> =
        sqlx::query_as("SELECT first_seen FROM wallets WHERE chain = $1 AND address = $2")
            .bind(chain)
            .bind(address)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(first_seen,)| (now - first_seen).num_seconds().max(0)))
}

/// Record a wallet label. Manual labels carry `manual = true` and remain
/// authoritative over automatic dispositions.
pub async fn add_wallet_label(
    pool: &PgPool,
    chain: &str,
    address: &str,
    kind: &str,
    disposition: &str,
    reason: &str,
    source: &str,
    confidence: i32,
    manual: bool,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO wallet_labels
            (chain, address, kind, disposition, reason, source, confidence, manual, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(chain)
    .bind(address)
    .bind(kind)
    .bind(disposition)
    .bind(reason)
    .bind(source)
    .bind(confidence)
    .bind(manual)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revoke a wallet label (`wallet unblock`): history retained, never deleted.
pub async fn revoke_wallet_label(
    pool: &PgPool,
    chain: &str,
    address: &str,
    kind: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<u64> {
    let result = if let Some(kind) = kind {
        sqlx::query(
            r#"
            UPDATE wallet_labels
               SET revoked_at = $4
             WHERE chain = $1 AND address = $2 AND kind = $3 AND revoked_at IS NULL
            "#,
        )
        .bind(chain)
        .bind(address)
        .bind(kind)
        .bind(now)
        .execute(pool)
        .await?
    } else {
        sqlx::query(
            r#"
            UPDATE wallet_labels
               SET revoked_at = $3
             WHERE chain = $1 AND address = $2 AND revoked_at IS NULL
            "#,
        )
        .bind(chain)
        .bind(address)
        .bind(now)
        .execute(pool)
        .await?
    };
    Ok(result.rows_affected())
}

/// The authoritative active disposition for a wallet: manual labels win over
/// automatic ones; the most restrictive active label applies.
pub async fn active_disposition(
    pool: &PgPool,
    chain: &str,
    address: &str,
) -> Result<Option<String>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT disposition, manual
          FROM wallet_labels
         WHERE chain = $1 AND address = $2
           AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
         ORDER BY manual DESC, confidence DESC, created_at DESC
        "#,
    )
    .bind(chain)
    .bind(address)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().next().map(|(d, _)| d))
}

/// Persist a normalized transfer (idempotent).
#[allow(clippy::too_many_arguments)]
pub async fn store_transfer(
    pool: &PgPool,
    t: &crate::models::NormalizedTransfer,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO transfers
            (chain, signature, event_index, from_address, to_address, asset_kind, mint,
             raw_amount, amount, slot, block_time, observed_at, source)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (chain, signature, event_index, from_address, to_address) DO NOTHING
        "#,
    )
    .bind(t.chain.as_str())
    .bind(&t.signature)
    .bind(t.event_index)
    .bind(&t.from_address)
    .bind(&t.to_address)
    .bind(t.asset_kind.as_str())
    .bind(&t.mint)
    .bind(&t.raw_amount)
    .bind(t.amount)
    .bind(t.slot.map(|v| v as i64))
    .bind(t.block_time)
    .bind(t.observed_at)
    .bind(&t.source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist a normalized trade (idempotent).
pub async fn store_trade(pool: &PgPool, t: &crate::models::NormalizedTrade) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO trades
            (chain, signature, event_index, wallet, mint, side,
             raw_native_amount, raw_token_amount, native_amount, token_amount, usd_value,
             slot, block_time, observed_at, dex_id, source)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        ON CONFLICT (chain, signature, event_index, wallet) DO NOTHING
        "#,
    )
    .bind(t.chain.as_str())
    .bind(&t.signature)
    .bind(t.event_index)
    .bind(&t.wallet)
    .bind(&t.mint)
    .bind(t.side.as_str())
    .bind(&t.raw_native_amount)
    .bind(&t.raw_token_amount)
    .bind(t.native_amount)
    .bind(t.token_amount)
    .bind(t.usd_value)
    .bind(t.slot.map(|v| v as i64))
    .bind(t.block_time)
    .bind(t.observed_at)
    .bind(&t.dex_id)
    .bind(&t.source)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get or create a chain sync cursor row.
pub async fn get_sync_cursor(
    pool: &PgPool,
    chain: &str,
    stream_kind: &str,
) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT cursor FROM chain_sync_state WHERE chain = $1 AND stream_kind = $2",
    )
    .bind(chain)
    .bind(stream_kind)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(c,)| c))
}

/// Persist a cursor ONLY after durable storage of the events it covers.
pub async fn set_sync_cursor(
    pool: &PgPool,
    chain: &str,
    stream_kind: &str,
    cursor: &str,
    last_error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO chain_sync_state (chain, stream_kind, cursor, updated_at, last_error)
        VALUES ($1, $2, $3, now(), $4)
        ON CONFLICT (chain, stream_kind) DO UPDATE
            SET cursor = EXCLUDED.cursor,
                updated_at = now(),
                last_error = EXCLUDED.last_error
        "#,
    )
    .bind(chain)
    .bind(stream_kind)
    .bind(cursor)
    .bind(last_error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Run `SELECT 1` with the raw executor (helper for tests).
pub async fn ping(pool: &PgPool) -> bool {
    pool.acquire().await.is_ok()
}

/// Retention cleanup for raw events based on profile retention days.
pub async fn prune_raw_events(pool: &PgPool, retention_days: i64) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM raw_events WHERE observed_at < now() - ($1 || ' days')::interval",
    )
    .bind(retention_days.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Mark raw event pruning also applies to market snapshots and GMGN payloads.
pub async fn prune_market_snapshots(pool: &PgPool, retention_days: i64) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM market_snapshots WHERE observed_at < now() - ($1 || ' days')::interval",
    )
    .bind(retention_days.to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_size_clamps() {
        assert_eq!(pool_size(2), 4);
        assert_eq!(pool_size(0), 2);
        assert_eq!(pool_size(50), 16);
    }
}
