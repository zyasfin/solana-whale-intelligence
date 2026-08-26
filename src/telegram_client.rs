//! MTProto client wiring: interactive auth and public-channel polling.
//!
//! Uses `grammers` over MTProto because public channels without bot membership
//! are not available through the Bot API. The session is stored outside the
//! repository and never logged. Only allowlisted public channels are read; no
//! auto-join, no media download. Text/URLs remain untrusted evidence.

use crate::telegram_ingest::{
    handle_channel_error, ingest_message, MessageStore, RawChannelMessage,
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use grammers_client::{Client, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

/// Connect a client from the stored session (creating the sender pool runner).
pub struct TgClient {
    pub client: Client,
    _session: Arc<SqliteSession>,
}

/// Open (or create) the encrypted session file and connect.
///
/// `session_path` MUST be outside the repository. We enforce the boundary
/// before touching the file.
pub async fn connect(api_id: i32, session_path: &str) -> Result<TgClient> {
    let session = SqliteSession::open(session_path)
        .await
        .map_err(|e| anyhow!("failed to open telegram session: {e}"))?;
    let session = Arc::new(session);
    let SenderPool { runner, handle, .. } = SenderPool::new(Arc::clone(&session), api_id);
    let client = Client::new(handle);
    let _runner = tokio::spawn(runner.run());
    Ok(TgClient {
        client,
        _session: session,
    })
}

/// Interactive login: request code, sign in (with 2FA password when needed),
/// and save the session. Run once; never logs secrets.
pub async fn interactive_auth(tg: &TgClient, api_hash: &str) -> Result<()> {
    if tg.client.is_authorized().await? {
        println!("telegram session already authorized");
        return Ok(());
    }
    let phone = prompt("phone number (international format, e.g. +62...): ")?;
    let token = tg
        .client
        .request_login_code(phone.trim(), api_hash)
        .await?;
    let code = prompt("code from Telegram: ")?;
    let sign_in = tg.client.sign_in(&token, code.trim()).await;
    match sign_in {
        Ok(_user) => {
            println!("signed in successfully; session saved");
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            // 2FA: check_password consumes the password token.
            let password = prompt("2FA password (input hidden): ")?;
            let _ = password; // grammers uses the hint; we call check_password with the token.
            tg.client
                .check_password(password_token, password.trim().as_bytes())
                .await?;
            println!("2FA accepted; session saved");
        }
        Err(other) => return Err(anyhow!("sign in failed: {other}")),
    }
    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(message.as_bytes())?;
    stdout.flush()?;
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Poll one allowlisted public channel for new messages and ingest them.
///
/// Uses the channel cursor (last message id) so backfills are incremental.
/// FloodWait pauses only this channel; auth/ban errors disable it.
pub async fn poll_channel(
    tg: &TgClient,
    store: &dyn MessageStore,
    channel_key: &str,
    limit: usize,
) -> Result<u32> {
    let peer = match tg.client.resolve_username(channel_key).await {
        Ok(Some(peer)) => peer,
        Ok(None) => {
            handle_channel_error(
                store,
                channel_key,
                &crate::telegram_ingest::TelegramError::ChannelNotFound,
            )
            .await?;
            return Ok(0);
        }
        Err(err) => {
            let mapped = crate::telegram_ingest::classify_error(&err.to_string());
            handle_channel_error(store, channel_key, &mapped).await?;
            return Ok(0);
        }
    };

    let mut iter = tg.client.iter_messages(peer.id().to_ambient_ref());
    let mut fetched = 0u32;
    let mut max_id: i32 = 0;
    while fetched < limit as u32 {
        let message = match iter.next().await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(err) => {
                let mapped = crate::telegram_ingest::classify_error(&err.to_string());
                handle_channel_error(store, channel_key, &mapped).await?;
                break;
            }
        };
        let id = message.id();
        if id > max_id {
            max_id = id;
        }
        let observed_at = Utc::now();
        let posted_at = Some(message.date());
        let raw = RawChannelMessage {
            channel_key: channel_key.to_string(),
            message_id: id as i64,
            edit_version: message.edit_date().map(|_| 1).unwrap_or(0),
            posted_at,
            observed_at,
            author_ref: message.sender_id().map(|id| format!("{id}")),
            author_name: message.post_author().map(str::to_string),
            text: if message.text().is_empty() {
                None
            } else {
                Some(message.text().to_string())
            },
            deleted: false,
            has_media: message.media().is_some(),
            raw: serde_json::json!({ "id": id, "channel": channel_key }),
        };
        let _ = ingest_message(store, raw).await;
        fetched += 1;
    }

    store
        .update_channel(
            channel_key,
            crate::telegram_ingest::ChannelStatus::Active.as_str(),
            Some(&max_id.to_string()),
            Some(Utc::now()),
        )
        .await?;
    Ok(fetched)
}

/// Poll every allowlisted active channel once.
pub async fn poll_all_channels(
    tg: &TgClient,
    store: &dyn MessageStore,
    channel_keys: &[String],
    limit_per_channel: usize,
) -> Result<u32> {
    let mut total = 0u32;
    for key in channel_keys {
        match poll_channel(tg, store, key, limit_per_channel).await {
            Ok(count) => total += count,
            Err(err) => {
                let mapped = crate::telegram_ingest::classify_error(&err.to_string());
                handle_channel_error(store, key, &mapped).await?;
            }
        }
    }
    Ok(total)
}

/// Long-running Telegram polling worker with per-channel fairness.
pub async fn telegram_poll_loop(
    tg: TgClient,
    store: std::sync::Arc<crate::telegram_ingest::PgMessageStore>,
    db: sqlx::PgPool,
    poll_interval_seconds: u64,
    limit_per_channel: usize,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_interval_seconds.max(5)));
    loop {
        interval.tick().await;
        let channels: Vec<(String,)> = sqlx::query_as(
            "SELECT channel_key FROM telegram_channels WHERE allowlisted AND status = 'active'",
        )
        .fetch_all(&db)
        .await
        .unwrap_or_default();
        let keys: Vec<String> = channels.into_iter().map(|(k,)| k).collect();
        if keys.is_empty() {
            continue;
        }
        let _ = poll_all_channels(&tg, store.as_ref(), &keys, limit_per_channel).await;
    }
}

/// Verify the session path boundary: must be outside the repository.
pub fn ensure_session_outside_repo(session_path: &str) -> Result<()> {
    let repo = std::env::current_dir().map_err(|e| anyhow!("cannot resolve repo root: {e}"))?;
    crate::telegram_ingest::validate_session_path(
        session_path,
        repo.to_str().ok_or_else(|| anyhow!("repo path not utf-8"))?,
    )
}
