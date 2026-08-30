//! Runtime workers: discovery, radar evaluation, signal evaluation, alert
//! dispatch, and chain funding streams. Each worker is a long-running loop
//! spawned by the `run` command; every worker pauses under queue backpressure.

#![allow(dead_code)]

use crate::db;
use crate::gmgn::GmgnClient;
use crate::models::{ChainKind, TradeSide};
use crate::queues::QueueState;
use crate::signals::{ExitGates, MarketGate, TokenGate};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::Mutex;

/// Shared application state for workers.
pub struct WorkerContext {
    pub pool: PgPool,
    pub queue_state: Mutex<QueueState>,
    pub funding_config: crate::config::FundingRadarConfig,
    pub helius_config: crate::config::HeliusConfig,
    pub signals_config: crate::config::SignalsConfig,
    pub scoring_config: crate::config::ScoringConfig,
    pub narrative_config: crate::config::NarrativeConfig,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub http: reqwest::Client,
}

impl WorkerContext {
    pub fn new(pool: PgPool, settings: &crate::config::Settings) -> Self {
        Self {
            pool,
            queue_state: Mutex::new(QueueState::new()),
            funding_config: settings.config.funding_radar.clone(),
            helius_config: settings.config.helius.clone(),
            signals_config: settings.config.signals.clone(),
            scoring_config: settings.config.scoring.clone(),
            narrative_config: settings.config.narrative.clone(),
            telegram_bot_token: settings.env.telegram_bot_token.clone(),
            telegram_chat_id: settings.env.telegram_chat_id.clone(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Whether a queue may run right now (paused queues wait).
    pub async fn queue_allowed(&self, queue: &str) -> bool {
        let state = self.queue_state.lock().await;
        !state.is_paused(queue)
    }
}

/// One GMGN discovery pass: fetch the newest tokens from `trenches`.
///
/// GMGN is enrichment: discovered tokens become `tokens` rows and GMGN
/// observations; they never become canonical scores by themselves.
pub async fn discover_tokens_once(
    pool: &PgPool,
    gmgn: &GmgnClient,
    chain: ChainKind,
) -> Result<u32> {
    let mut params = BTreeMap::new();
    params.insert("chain".to_string(), chain.as_str().to_string());
    params.insert("period".to_string(), "1h".to_string());
    let envelope = gmgn.query("trenches", params).await?;
    let observed_at = Utc::now();
    crate::ingest::store_gmgn_token_observation(
        pool,
        chain,
        "trenches_discovery",
        "trenches",
        &envelope.data,
        observed_at,
    )
    .await?;

    // Extract candidate mints from the payload.
    let mut count = 0u32;
    let items = envelope
        .data
        .get("pools")
        .or_else(|| envelope.data.get("items"))
        .or_else(|| envelope.data.get("tokens"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for item in items {
        let mint = item
            .get("mint")
            .or_else(|| item.get("address"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if mint.is_empty() {
            continue;
        }
        sqlx::query(
            r#"
            INSERT INTO tokens (chain, mint, first_seen_at, lifecycle_state)
            VALUES ($1, $2, $3, 'new_creation')
            ON CONFLICT (chain, mint) DO NOTHING
            "#,
        )
        .bind(chain.as_str())
        .bind(mint)
        .bind(observed_at)
        .execute(pool)
        .await?;
        count += 1;
    }
    Ok(count)
}

/// Compute and store a wallet score from trades in the database.
///
/// FIFO-matches the wallet's trades, aggregates skill/copyability inputs, and
/// persists the score row idempotently for `as_of`.
pub async fn score_wallet(
    pool: &PgPool,
    chain: ChainKind,
    address: &str,
    as_of: DateTime<Utc>,
    config: &crate::config::ScoringConfig,
) -> Result<crate::scoring::WalletScoreResult> {
    let rows: Vec<(String, String, Option<Decimal>, Option<Decimal>, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT mint, side, token_amount, usd_value, block_time
          FROM trades
         WHERE chain = $1 AND wallet = $2 AND block_time <= $3
         ORDER BY block_time ASC
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .bind(as_of)
    .fetch_all(pool)
    .await?;

    let scored: Vec<crate::scoring::ScoredTrade> = rows
        .into_iter()
        .filter_map(|(mint, side, token_amount, usd_value, block_time)| {
            let side = match side.as_str() {
                "buy" => TradeSide::Buy,
                "sell" => TradeSide::Sell,
                _ => return None,
            };
            Some(crate::scoring::ScoredTrade {
                mint,
                side,
                token_amount: token_amount.unwrap_or(Decimal::ZERO),
                usd_value,
                block_time,
            })
        })
        .collect();

    let matched = crate::scoring::fifo_match(&scored);
    let meaningful: Vec<_> = matched
        .iter()
        .filter(|m| m.realized_pnl_usd.map(|p| p.abs() >= Decimal::from(1)).unwrap_or(false))
        .collect();
    let realized_pnl: Decimal = matched
        .iter()
        .filter_map(|m| m.realized_pnl_usd)
        .sum();
    let wins = matched
        .iter()
        .filter(|m| m.realized_pnl_usd.map(|p| p > Decimal::ZERO).unwrap_or(false))
        .count();
    let meaningful_win_rate = if matched.is_empty() {
        Decimal::ZERO
    } else {
        Decimal::from(wins) / Decimal::from(matched.len())
    };
    let avg_hold = {
        let holds: Vec<i64> = matched.iter().filter_map(|m| m.hold_seconds).collect();
        if holds.is_empty() {
            None
        } else {
            Some(holds.iter().sum::<i64>() / holds.len().max(1) as i64)
        }
    };
    let tokens_traded = matched
        .iter()
        .map(|m| m.mint.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u32;

    let inputs = crate::scoring::ScoreInputs {
        meaningful_trades: meaningful.len() as u32,
        tokens_traded,
        realized_pnl_usd: realized_pnl,
        size_weighted_roi: Decimal::ZERO,
        meaningful_win_rate,
        early_entry_rate: Decimal::ZERO,
        history_completeness: Decimal::ONE,
        mev_likelihood: Decimal::ZERO,
        avg_hold_seconds: avg_hold,
        gmgn_agreement: None,
        liquidity_available: None,
    };
    let result = crate::scoring::compute_wallet_score(&inputs, config);
    crate::scoring::store_wallet_score(pool, chain, address, as_of, &inputs, &result).await?;
    Ok(result)
}

/// Evaluate one token's entry/exit signals from current evidence.
///
/// Reads token lifecycle, latest market snapshot, cluster buys, and wallet
/// eligibility; writes one `signal_evaluations` row per gate outcome.
pub async fn evaluate_token_signals(
    pool: &PgPool,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &crate::config::SignalsConfig,
) -> Result<Option<i64>> {
    // Token facts.
    let token_row: Option<(String, Option<DateTime<Utc>>, serde_json::Value)> = sqlx::query_as(
        "SELECT lifecycle_state, first_seen_at, risk_flags FROM tokens WHERE chain = $1 AND mint = $2",
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_optional(pool)
    .await?;
    let (lifecycle_str, first_seen, risk_flags) = token_row
        .ok_or_else(|| anyhow::anyhow!("token not tracked"))?;
    let lifecycle = crate::models::LifecycleState::parse(&lifecycle_str)
        .unwrap_or(crate::models::LifecycleState::Unsupported);
    let age_hours = first_seen.map(|t| (now - t).num_hours());
    let risk_flags: Vec<String> = serde_json::from_value(risk_flags).unwrap_or_default();

    // Market snapshot.
    let market_row: Option<(Option<Decimal>, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        SELECT liquidity_usd, observed_at FROM market_snapshots
         WHERE chain = $1 AND mint = $2 AND observed_at <= $3
         ORDER BY observed_at DESC LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    let market = MarketGate {
        liquidity_usd: market_row.as_ref().and_then(|(l, _)| *l),
        observed_at: market_row.as_ref().and_then(|(_, t)| *t),
    };

    // Cluster and wallet summaries (simplified: count active clusters with buys).
    let cluster_buys: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT m.cluster_id)
          FROM wallet_cluster_members m
          JOIN trades t ON t.chain = m.chain AND t.wallet = m.address
         WHERE m.chain = $1 AND t.mint = $2 AND t.side = 'buy' AND m.revoked_at IS NULL
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_one(pool)
    .await?;
    let meaningful_buys: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM trades
         WHERE chain = $1 AND mint = $2 AND side = 'buy' AND COALESCE(usd_value, 0) >= 1
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_one(pool)
    .await?;

    // Best wallet eligibility among recent buyers.
    let wallet_row: Option<(i32, i32, Decimal)> = sqlx::query_as(
        r#"
        SELECT skill_score, copyability_score, history_completeness
          FROM wallet_scores
         WHERE chain = $1 AND address IN (
            SELECT DISTINCT wallet FROM trades WHERE chain = $1 AND mint = $2 AND side = 'buy'
         )
         ORDER BY as_of DESC
         LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_optional(pool)
    .await?;
    let wallets = match wallet_row {
        Some((skill, copy, completeness)) => crate::signals::WalletEligibility {
            eligible_wallets: 1,
            max_skill: skill.max(0) as u32,
            max_copyability: copy.max(0) as u32,
            min_history_completeness: completeness,
        },
        None => crate::signals::WalletEligibility {
            eligible_wallets: 0,
            max_skill: 0,
            max_copyability: 0,
            min_history_completeness: Decimal::ZERO,
        },
    };

    let token = TokenGate {
        lifecycle,
        age_hours,
        risk_flags,
    };
    let clusters = crate::signals::ClusterSummary {
        eligible_clusters: cluster_buys.max(0) as u32,
        meaningful_buys: meaningful_buys.max(0) as u32,
    };
    crate::signals::evaluate_token(
        pool,
        chain,
        mint,
        now,
        config,
        &token,
        &market,
        &clusters,
        &wallets,
        None,
    )
    .await
}

/// Dispatch pending signals as Telegram alerts (outbound Bot API only).
pub async fn dispatch_alerts(ctx: &WorkerContext) -> Result<u32> {
    let (Some(bot_token), Some(chat_id)) = (ctx.telegram_bot_token.as_ref(), ctx.telegram_chat_id.as_ref()) else {
        return Ok(0);
    };
    let pending: Vec<(i64, String, String, i32, chrono::DateTime<Utc>)> = sqlx::query_as(
        r#"
        SELECT s.id, s.mint, s.signal_kind, s.score, s.created_at
          FROM signals s
         WHERE s.status = 'active'
           AND NOT EXISTS (SELECT 1 FROM alerts a WHERE a.signal_id = s.id)
         ORDER BY s.created_at ASC
         LIMIT 20
        "#,
    )
    .fetch_all(&ctx.pool)
    .await?;

    let mut sent = 0u32;
    for (signal_id, mint, kind, score, created_at) in pending {
        let chain = "solana";
        let message = crate::alerts::compose_signal_alert(chain, &mint, &kind, score.max(0) as u32, "see report");
        let dedup_key = crate::signals::alert_dedup_key(ChainKind::Solana, &mint, &kind, created_at);
        let result = crate::alerts::send_message(&ctx.http, bot_token, chat_id, &message.text).await;
        let error = result.as_ref().err().map(|e| e.to_string());
        crate::signals::record_alert(&ctx.pool, &dedup_key, signal_id, error.as_deref()).await?;
        if result.is_ok() {
            sent += 1;
        }
    }
    Ok(sent)
}

/// Evaluate open radar cases (periodic).
pub async fn evaluate_radar_cases(ctx: &WorkerContext) -> Result<u32> {
    let open: Vec<(i64, String, String)> = sqlx::query_as(
        r#"
        SELECT id, chain, recipient FROM funding_radar_cases
         WHERE stage NOT IN ('dismissed', 'deployed')
         ORDER BY updated_at ASC
         LIMIT 100
        "#,
    )
    .fetch_all(&ctx.pool)
    .await?;
    let now = Utc::now();
    let mut evaluated = 0u32;
    for (_id, chain_str, recipient) in open {
        let chain = ChainKind::parse(&chain_str).unwrap_or(ChainKind::Solana);
        let decision = crate::funding_radar::evaluate_radar_case(
            &ctx.pool,
            chain,
            &recipient,
            now,
            &ctx.funding_config,
        )
        .await?;
        if let Some(alert) = decision.alert {
            if let (Some(bot), Some(chat)) = (&ctx.telegram_bot_token, &ctx.telegram_chat_id) {
                let _ = crate::alerts::send_message(&ctx.http, bot, chat, &alert.message).await;
            }
        }
        evaluated += 1;
    }
    Ok(evaluated)
}

/// Poll Robinhood blocks via the EVM adapter and feed the funding radar.
pub async fn robinhood_funding_loop(
    ctx: std::sync::Arc<WorkerContext>,
    adapter: crate::chains::RobinhoodAdapter<crate::chains::HttpEvmRpc>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(15));
    loop {
        interval.tick().await;
        if !ctx.queue_allowed(crate::queues::QUEUE_FUNDING_RADAR).await {
            continue;
        }
        let current = match adapter.current_block().await {
            Ok(block) => block,
            Err(err) => {
                tracing::warn!(error = %err, "robinhood current_block failed");
                continue;
            }
        };
        let cursor = db::get_sync_cursor(&ctx.pool, "robinhood", "funding")
            .await
            .unwrap_or(None)
            .and_then(|c| c.parse::<u64>().ok())
            .unwrap_or(0);
        for block in (cursor + 1)..=current.min(cursor + 100) {
            let observed_at = Utc::now();
            match adapter.fetch_block_events(block, observed_at).await {
                Ok(events) => {
                    for transfer in &events.transfers {
                        let _ = crate::ingest::ingest_funding_transfer(
                            &ctx.pool,
                            transfer,
                            None,
                            &ctx.funding_config,
                            None,
                        )
                        .await;
                    }
                    let _ = db::set_sync_cursor(&ctx.pool, "robinhood", "funding", &block.to_string(), None).await;
                }
                Err(err) => {
                    tracing::warn!(block, error = %err, "robinhood block fetch failed");
                    let _ = db::set_sync_cursor(&ctx.pool, "robinhood", "funding", &block.to_string(), Some(&err.to_string())).await;
                    break;
                }
            }
        }
    }
}

/// Spawn the worker set for the `run` command.
pub async fn run_workers(ctx: std::sync::Arc<WorkerContext>, gmgn: Option<GmgnClient>) {
    let radar_ctx = ctx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if !radar_ctx.queue_allowed(crate::queues::QUEUE_FUNDING_RADAR).await {
                continue;
            }
            if let Err(err) = evaluate_radar_cases(&radar_ctx).await {
                tracing::warn!(error = %err, "radar evaluation failed");
            }
        }
    });

    let alert_ctx = ctx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        loop {
            interval.tick().await;
            if let Err(err) = dispatch_alerts(&alert_ctx).await {
                tracing::warn!(error = %err, "alert dispatch failed");
            }
        }
    });

    if let Some(gmgn) = gmgn {
        let discover_ctx = ctx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                if !discover_ctx.queue_allowed(crate::queues::QUEUE_SEED_SYNC).await {
                    continue;
                }
                if let Err(err) = discover_tokens_once(&discover_ctx.pool, &gmgn, ChainKind::Solana).await {
                    tracing::warn!(error = %err, "discovery failed");
                }
            }
        });
    }
}

/// Unused stub kept for API completeness; exit gate summary from a token.
/// Solana filtered funding stream over the Helius WebSocket.
///
/// Subscribes to `logsSubscribe` filtered by SOL-transfer program and feeds
/// qualifying inbound transfers into the funding radar. `processed`
/// commitment is accepted only for early `funding_watch`; promotion requires
/// `confirmed` handled by the radar evaluation layer.
pub async fn solana_funding_stream(
    ctx: std::sync::Arc<WorkerContext>,
    helius_key: String,
    watch_addresses: Vec<String>,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = ctx.helius_config.ws_url(&helius_key);
    let (mut socket, _) = match tokio_tungstenite::connect_async(&url).await {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!(error = %err, "solana websocket connect failed");
            return Err(anyhow::anyhow!("websocket connect failed: {err}"));
        }
    };
    tracing::info!("solana funding websocket connected");

    // Filtered logs subscription: watch the system program for SOL transfers
    // and any configured addresses.
    let mut mentions = vec!["11111111111111111111111111111111".to_string()];
    mentions.extend(watch_addresses);
    let subscribe = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "logsSubscribe",
        "params": [
            { "mentions": mentions },
            { "commitment": "confirmed" }
        ]
    });
    socket.send(Message::Text(subscribe.to_string().into())).await?;

    while let Some(message) = socket.next().await {
        let Ok(message) = message else { break };
        if !ctx.queue_allowed(crate::queues::QUEUE_FUNDING_RADAR).await {
            continue;
        }
        let text = match message {
            Message::Text(t) => t,
            Message::Ping(_) => {
                let _ = socket.send(Message::Pong(Vec::new().into())).await;
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let signature = value
            .pointer("/params/result/value/signature")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(signature) = signature {
            let observed_at = Utc::now();
            let _ = db::store_raw_event(
                &ctx.pool,
                "solana",
                "helius_ws",
                &signature,
                &value,
                observed_at,
            )
            .await;
        }
    }
    tracing::warn!("solana funding websocket closed");
    Ok(())
}
pub async fn summarize_exit_gates(
    pool: &PgPool,
    chain: ChainKind,
    mint: &str,
) -> Result<ExitGates> {
    let clusters_selling: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT m.cluster_id)
          FROM wallet_cluster_members m
          JOIN trades t ON t.chain = m.chain AND t.wallet = m.address
         WHERE m.chain = $1 AND t.mint = $2 AND t.side = 'sell' AND m.revoked_at IS NULL
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_one(pool)
    .await?;
    Ok(ExitGates {
        clusters_selling: clusters_selling.max(0) as u32,
        material_fresh_transfer: false,
        dev_distribution: false,
        liquidity_drop_ratio: None,
        tracked_selling: false,
    })
}
