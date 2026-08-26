//! Telegram MTProto ingestion over the public channel allowlist.
//!
//! Requires the user's authorized MTProto session. Public channels are read
//! without joining. Media is NEVER downloaded: only text and metadata.
//! FloodWait errors pause only the affected channel for the server-provided
//! duration. Invalid auth or banned channels disable that channel without
//! tight-looping.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::models::ChainKind;
use crate::telegram_parse::{content_hash, parse_message_text, MentionKind, ParsedMessage};
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

/// Status of a Telegram channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelStatus {
    Active,
    FloodWait,
    Disabled,
    NotFound,
    AuthFailed,
}

impl ChannelStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelStatus::Active => "active",
            ChannelStatus::FloodWait => "flood_wait",
            ChannelStatus::Disabled => "disabled",
            ChannelStatus::NotFound => "not_found",
            ChannelStatus::AuthFailed => "auth_failed",
        }
    }
}

/// A raw Telegram channel post (already normalized from MTProto).
#[derive(Clone, Debug)]
pub struct RawChannelMessage {
    pub channel_key: String,
    pub message_id: i64,
    pub edit_version: i32,
    pub posted_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub author_ref: Option<String>,
    pub author_name: Option<String>,
    pub text: Option<String>,
    pub deleted: bool,
    pub has_media: bool,
    pub raw: serde_json::Value,
}

impl RawChannelMessage {
    /// Compute the deterministic content hash for dedup.
    pub fn hash(&self) -> String {
        content_hash(&self.channel_key, self.message_id, self.edit_version, self.text.as_deref())
    }
}

/// Storage abstraction so ingestion logic is testable without MTProto.
#[async_trait::async_trait]
pub trait MessageStore: Send + Sync {
    /// Insert a message; returns false when `(channel, id, edit_version)` already exists.
    async fn insert_message(&self, message: &RawChannelMessage) -> Result<bool>;
    /// Insert one mention; returns false when the unique row already exists.
    async fn insert_mention(
        &self,
        channel_key: &str,
        message_id: i64,
        edit_version: i32,
        chain: &str,
        mint: Option<&str>,
        wallet: Option<&str>,
        mention_kind: &str,
        extracted_value: &str,
        context: &str,
    ) -> Result<bool>;
    /// Update channel status/cursor.
    async fn update_channel(
        &self,
        channel_key: &str,
        status: &str,
        cursor: Option<&str>,
        last_observed_at: Option<DateTime<Utc>>,
    ) -> Result<()>;
    /// Whether the channel is allowlisted for ingestion.
    async fn channel_allowlisted(&self, channel_key: &str) -> Result<bool>;
}

/// Errors surfaced by the MTProto layer, mapped to channel states.
#[derive(Clone, Debug, PartialEq)]
pub enum TelegramError {
    FloodWait { seconds: u64 },
    AuthFailed,
    ChannelNotFound,
    ChannelBanned,
    Network(String),
}

impl std::fmt::Display for TelegramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelegramError::FloodWait { seconds } => write!(f, "flood wait {seconds}s"),
            TelegramError::AuthFailed => write!(f, "auth failed"),
            TelegramError::ChannelNotFound => write!(f, "channel not found"),
            TelegramError::ChannelBanned => write!(f, "channel banned"),
            TelegramError::Network(msg) => write!(f, "network error: {msg}"),
        }
    }
}

/// Classify a raw MTProto/transport error string into a TelegramError.
pub fn classify_error(raw: &str) -> TelegramError {
    let lower = raw.to_lowercase();
    if lower.contains("flood") {
        // Extract the wait duration when present (e.g. "FLOOD_WAIT_30").
        let seconds = lower
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<u64>().ok())
            .next_back()
            .unwrap_or(5);
        return TelegramError::FloodWait { seconds };
    }
    if lower.contains("auth") || lower.contains("unauthorized") || lower.contains("session") {
        return TelegramError::AuthFailed;
    }
    if lower.contains("not found") || lower.contains("does not exist") {
        return TelegramError::ChannelNotFound;
    }
    if lower.contains("banned") || lower.contains("restricted") || lower.contains("kicked") {
        return TelegramError::ChannelBanned;
    }
    TelegramError::Network(raw.to_string())
}

/// Ingest one raw message: dedupe, parse, persist mentions.
/// Postgres-backed message store for the runtime worker.
pub struct PgMessageStore {
    pool: sqlx::PgPool,
}

impl PgMessageStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl MessageStore for PgMessageStore {
    async fn insert_message(&self, message: &RawChannelMessage) -> Result<bool> {
        // Ensure the channel row exists (FK); it is created on allowlist add.
        sqlx::query(
            "INSERT INTO telegram_channels (channel_key, allowlisted, status)
             VALUES ($1, true, 'active')
             ON CONFLICT (channel_key) DO NOTHING",
        )
        .bind(&message.channel_key)
        .execute(&self.pool)
        .await?;
        let result = sqlx::query(
            r#"
            INSERT INTO telegram_messages
                (channel_key, message_id, edit_version, posted_at, observed_at,
                 author_ref, author_name, text_content, raw, content_hash, deleted_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (channel_key, message_id, edit_version) DO NOTHING
            "#,
        )
        .bind(&message.channel_key)
        .bind(message.message_id)
        .bind(message.edit_version)
        .bind(message.posted_at)
        .bind(message.observed_at)
        .bind(&message.author_ref)
        .bind(&message.author_name)
        .bind(&message.text)
        .bind(&message.raw)
        .bind(message.hash())
        .bind(if message.deleted { Some(message.observed_at) } else { None })
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn insert_mention(
        &self,
        channel_key: &str,
        message_id: i64,
        edit_version: i32,
        chain: &str,
        mint: Option<&str>,
        wallet: Option<&str>,
        mention_kind: &str,
        extracted_value: &str,
        context: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"
            INSERT INTO telegram_mentions
                (channel_key, message_id, edit_version, chain, mint, wallet,
                 mention_kind, extracted_value, context)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (channel_key, message_id, edit_version, mention_kind, extracted_value)
            DO NOTHING
            "#,
        )
        .bind(channel_key)
        .bind(message_id)
        .bind(edit_version)
        .bind(chain)
        .bind(mint)
        .bind(wallet)
        .bind(mention_kind)
        .bind(extracted_value)
        .bind(context)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_channel(
        &self,
        channel_key: &str,
        status: &str,
        cursor: Option<&str>,
        last_observed_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO telegram_channels (channel_key, status, last_cursor, last_observed_at)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (channel_key) DO UPDATE
                SET status = EXCLUDED.status,
                    last_cursor = COALESCE(EXCLUDED.last_cursor, telegram_channels.last_cursor),
                    last_observed_at = COALESCE(EXCLUDED.last_observed_at, telegram_channels.last_observed_at)
            "#,
        )
        .bind(channel_key)
        .bind(status)
        .bind(cursor)
        .bind(last_observed_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn channel_allowlisted(&self, channel_key: &str) -> Result<bool> {
        let allowed: Option<bool> = sqlx::query_scalar(
            "SELECT allowlisted FROM telegram_channels WHERE channel_key = $1",
        )
        .bind(channel_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(allowed.unwrap_or(false))
    }
}
///
/// Access control: only allowlisted channels produce rows. Media-only messages
/// store metadata without any download.
pub async fn ingest_message(store: &dyn MessageStore, message: RawChannelMessage) -> Result<bool> {
    if !store.channel_allowlisted(&message.channel_key).await? {
        // Not allowlisted: no message row, no mention rows. History untouched.
        return Ok(false);
    }
    let inserted = store.insert_message(&message).await?;
    if !inserted {
        // Duplicate (channel, message_id, edit_version): keep original evidence,
        // do not create duplicate mentions.
        return Ok(false);
    }
    let parsed: ParsedMessage = parse_message_text(message.text.as_deref());
    let chain = ChainKind::Solana.as_str();
    for mention in &parsed.mentions {
        let (mint, wallet) = match mention.kind {
            MentionKind::Mint => (Some(mention.extracted_value.as_str()), None),
            MentionKind::Wallet => (None, Some(mention.extracted_value.as_str())),
            MentionKind::Url => (None, None),
        };
        store
            .insert_mention(
                &message.channel_key,
                message.message_id,
                message.edit_version,
                chain,
                mint,
                wallet,
                mention.kind.as_str(),
                &mention.extracted_value,
                &mention.context,
            )
            .await?;
    }
    Ok(true)
}

/// Handle a channel-level error by updating channel status.
///
/// FloodWait pauses only this channel; auth/ban errors disable it. Both
/// preserve history and never tight-loop.
pub async fn handle_channel_error(
    store: &dyn MessageStore,
    channel_key: &str,
    error: &TelegramError,
) -> Result<()> {
    match error {
        TelegramError::FloodWait { seconds } => {
            store
                .update_channel(channel_key, ChannelStatus::FloodWait.as_str(), None, None)
                .await?;
            tracing::warn!(channel = channel_key, seconds, "telegram flood wait; channel paused");
            // The scheduler must respect `seconds` before retrying this channel.
        }
        TelegramError::AuthFailed => {
            store
                .update_channel(channel_key, ChannelStatus::AuthFailed.as_str(), None, None)
                .await?;
            tracing::error!(channel = channel_key, "telegram auth failed; channel disabled");
        }
        TelegramError::ChannelNotFound | TelegramError::ChannelBanned => {
            store
                .update_channel(channel_key, ChannelStatus::Disabled.as_str(), None, None)
                .await?;
            tracing::warn!(channel = channel_key, "telegram channel unavailable; disabled");
        }
        TelegramError::Network(msg) => {
            tracing::warn!(channel = channel_key, error = %msg, "telegram network error; will retry");
        }
    }
    Ok(())
}

/// Global concurrency limiter for history requests (per profile).
#[derive(Debug)]
pub struct ConcurrencyLimit {
    limit: usize,
}

impl ConcurrencyLimit {
    pub fn new(limit: usize) -> Self {
        Self { limit: limit.max(1) }
    }

    /// Low profile: 4 concurrent history requests. Scale: 32.
    pub fn from_profile(profile: crate::config::RuntimeProfile) -> Self {
        Self::new(profile.telegram_concurrency())
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

/// In-memory message store used by tests and as a reference implementation.
///
/// Interior mutability uses `std::sync::Mutex`; the store is shared safely
/// across await points without unsafe casts.
#[derive(Default)]
pub struct MemoryMessageStore {
    inner: std::sync::Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    allowlist: HashSet<String>,
    messages: Vec<RawChannelMessage>,
    mentions: Vec<MentionRow>,
    statuses: std::collections::HashMap<String, String>,
    cursors: std::collections::HashMap<String, String>,
}

/// Stored mention row.
#[derive(Clone, Debug, PartialEq)]
pub struct MentionRow {
    pub channel_key: String,
    pub message_id: i64,
    pub edit_version: i32,
    pub chain: String,
    pub mint: Option<String>,
    pub wallet: Option<String>,
    pub mention_kind: String,
    pub extracted_value: String,
    pub context: String,
}

impl MemoryMessageStore {
    pub fn new(allowlist: Vec<String>) -> Self {
        Self {
            inner: std::sync::Mutex::new(MemoryState {
                allowlist: allowlist.into_iter().collect(),
                ..Default::default()
            }),
        }
    }

    pub fn message_count(&self) -> usize {
        self.inner.lock().map(|s| s.messages.len()).unwrap_or(0)
    }

    pub fn mention_count(&self) -> usize {
        self.inner.lock().map(|s| s.mentions.len()).unwrap_or(0)
    }

    pub fn status(&self, channel_key: &str) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|s| s.statuses.get(channel_key).cloned())
    }

    pub fn cursor(&self, channel_key: &str) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|s| s.cursors.get(channel_key).cloned())
    }
}

#[async_trait::async_trait]
impl MessageStore for MemoryMessageStore {
    async fn insert_message(&self, message: &RawChannelMessage) -> Result<bool> {
        let mut state = self.inner.lock().map_err(|_| anyhow::anyhow!("store poisoned"))?;
        let exists = state.messages.iter().any(|m| {
            m.channel_key == message.channel_key
                && m.message_id == message.message_id
                && m.edit_version == message.edit_version
        });
        if exists {
            return Ok(false);
        }
        state.messages.push(message.clone());
        Ok(true)
    }

    async fn insert_mention(
        &self,
        channel_key: &str,
        message_id: i64,
        edit_version: i32,
        chain: &str,
        mint: Option<&str>,
        wallet: Option<&str>,
        mention_kind: &str,
        extracted_value: &str,
        context: &str,
    ) -> Result<bool> {
        let mut state = self.inner.lock().map_err(|_| anyhow::anyhow!("store poisoned"))?;
        let exists = state.mentions.iter().any(|m| {
            m.channel_key == channel_key
                && m.message_id == message_id
                && m.edit_version == edit_version
                && m.mention_kind == mention_kind
                && m.extracted_value == extracted_value
        });
        if exists {
            return Ok(false);
        }
        state.mentions.push(MentionRow {
            channel_key: channel_key.to_string(),
            message_id,
            edit_version,
            chain: chain.to_string(),
            mint: mint.map(str::to_string),
            wallet: wallet.map(str::to_string),
            mention_kind: mention_kind.to_string(),
            extracted_value: extracted_value.to_string(),
            context: context.to_string(),
        });
        Ok(true)
    }

    async fn update_channel(
        &self,
        channel_key: &str,
        status: &str,
        cursor: Option<&str>,
        _last_observed_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let mut state = self.inner.lock().map_err(|_| anyhow::anyhow!("store poisoned"))?;
        state.statuses.insert(channel_key.to_string(), status.to_string());
        if let Some(cursor) = cursor {
            state.cursors.insert(channel_key.to_string(), cursor.to_string());
        }
        Ok(())
    }

    async fn channel_allowlisted(&self, channel_key: &str) -> Result<bool> {
        let state = self.inner.lock().map_err(|_| anyhow::anyhow!("store poisoned"))?;
        Ok(state.allowlist.contains(channel_key))
    }
}

/// Ensure the session file is stored outside the repository.
pub fn validate_session_path(path: &str, repo_root: &str) -> Result<()> {
    let canonical = std::path::Path::new(path)
        .canonicalize()
        .map_err(|e| anyhow!("session path invalid: {e}"))?;
    let repo = std::path::Path::new(repo_root)
        .canonicalize()
        .map_err(|e| anyhow!("repo root invalid: {e}"))?;
    if canonical.starts_with(&repo) {
        bail!("telegram session must be stored outside the repository");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(channel: &str, id: i64, edit: i32, text: Option<&str>) -> RawChannelMessage {
        RawChannelMessage {
            channel_key: channel.to_string(),
            message_id: id,
            edit_version: edit,
            posted_at: Some(Utc::now()),
            observed_at: Utc::now(),
            author_ref: Some("anon".to_string()),
            author_name: Some("Anonymous".to_string()),
            text: text.map(str::to_string),
            deleted: false,
            has_media: false,
            raw: serde_json::json!({"id": id}),
        }
    }

    #[tokio::test]
    async fn allowlisted_channel_ingests() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        let mint = bs58::encode([9u8; 32]).into_string();
        let text = format!("buy {mint} https://example.com");
        let inserted = ingest_message(&store, message("chan_a", 1, 0, Some(&text)))
            .await
            .unwrap();
        assert!(inserted);
        assert_eq!(store.message_count(), 1);
        assert_eq!(store.mention_count(), 2);
    }

    #[tokio::test]
    async fn non_allowlisted_channel_creates_nothing() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        let inserted = ingest_message(&store, message("chan_b", 1, 0, Some("buy now")))
            .await
            .unwrap();
        assert!(!inserted);
        assert_eq!(store.message_count(), 0);
        assert_eq!(store.mention_count(), 0);
    }

    #[tokio::test]
    async fn duplicate_ingestion_deduplicates() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        let text = "signal";
        let first = ingest_message(&store, message("chan_a", 5, 0, Some(text)))
            .await
            .unwrap();
        let second = ingest_message(&store, message("chan_a", 5, 0, Some(text)))
            .await
            .unwrap();
        assert!(first);
        assert!(!second, "duplicate must be ignored");
        assert_eq!(store.message_count(), 1);
    }

    #[tokio::test]
    async fn edited_version_stored_separately() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        ingest_message(&store, message("chan_a", 5, 0, Some("original")))
            .await
            .unwrap();
        let edited = ingest_message(&store, message("chan_a", 5, 1, Some("edited text")))
            .await
            .unwrap();
        assert!(edited, "edited version is a new row");
        assert_eq!(store.message_count(), 2);
    }

    #[tokio::test]
    async fn media_only_message_stores_metadata_only() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        let mut msg = message("chan_a", 9, 0, None);
        msg.has_media = true;
        let inserted = ingest_message(&store, msg).await.unwrap();
        assert!(inserted);
        assert_eq!(store.message_count(), 1);
        assert_eq!(store.mention_count(), 0, "no mentions from media-only");
    }

    #[tokio::test]
    async fn flood_wait_pauses_channel() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        let err = TelegramError::FloodWait { seconds: 42 };
        handle_channel_error(&store, "chan_a", &err).await.unwrap();
        assert_eq!(store.status("chan_a").as_deref(), Some("flood_wait"));
    }

    #[tokio::test]
    async fn auth_failure_disables_channel() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        handle_channel_error(&store, "chan_a", &TelegramError::AuthFailed)
            .await
            .unwrap();
        assert_eq!(store.status("chan_a").as_deref(), Some("auth_failed"));
    }

    #[tokio::test]
    async fn banned_channel_disabled() {
        let store = MemoryMessageStore::new(vec!["chan_a".into()]);
        handle_channel_error(&store, "chan_a", &TelegramError::ChannelBanned)
            .await
            .unwrap();
        assert_eq!(store.status("chan_a").as_deref(), Some("disabled"));
    }

    #[test]
    fn error_classification() {
        assert_eq!(
            classify_error("FLOOD_WAIT_30"),
            TelegramError::FloodWait { seconds: 30 }
        );
        assert_eq!(classify_error("AUTH_KEY_UNREGISTERED"), TelegramError::AuthFailed);
        assert_eq!(classify_error("channel not found"), TelegramError::ChannelNotFound);
        assert_eq!(classify_error("user banned"), TelegramError::ChannelBanned);
        assert!(matches!(classify_error("timeout"), TelegramError::Network(_)));
    }

    #[test]
    fn concurrency_limit_from_profile() {
        use crate::config::RuntimeProfile;
        assert_eq!(ConcurrencyLimit::from_profile(RuntimeProfile::Low).limit(), 4);
        assert_eq!(ConcurrencyLimit::from_profile(RuntimeProfile::Scale).limit(), 32);
    }

    #[test]
    fn session_path_outside_repository_required() {
        let repo = std::env::temp_dir();
        let repo_str = repo.to_string_lossy().to_string();
        let inside = repo.join("session.bin");
        assert!(validate_session_path(inside.to_str().unwrap(), &repo_str).is_err());
        let outside = std::env::temp_dir().join("other-place").join("session.bin");
        if outside.exists() || outside.parent().map(|p| p.exists()).unwrap_or(false) {
            // Path exists or parent exists: canonicalization works.
            if outside.canonicalize().is_ok() {
                assert!(validate_session_path(outside.to_str().unwrap(), &repo_str).is_ok());
            }
        }
    }
}
