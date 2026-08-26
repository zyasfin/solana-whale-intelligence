//! Runtime workers: discovery, radar evaluation, signal evaluation, alert
//! dispatch, and chain funding streams. Each worker is a long-running loop
//! spawned by the `run` command; every worker pauses under queue backpressure.

#![allow(dead_code)]

use crate::db;
use crate::gmgn::GmgnPool;
use crate::models::{ChainKind, TradeSide};
use crate::queues::QueueState;
use crate::signals::{ExitGates, MarketGate, TokenGate};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
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
    pub smart_wallet_config: crate::config::SmartWalletConfig,
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
            smart_wallet_config: settings.config.smart_wallet.clone(),
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
    gmgn: &GmgnPool,
    chain: ChainKind,
) -> Result<u32> {
    // Official trenches endpoint (POST /v1/trenches?chain=<chain>), shared
    // helper on GmgnPool so worker and admin TG-hot can't drift. GMGN expects
    // its short chain code ("sol"), not our internal ChainKind name ("solana").
    let gmgn_chain = match chain {
        ChainKind::Solana => "sol",
        _ => chain.as_str(),
    };
    let envelope = gmgn.trenches(gmgn_chain, 80).await?;
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

    // Extract candidate mints from the v2 payload
    // (data{new_creation:[..], near_completion:[..], completed:[..]}).
    // Each token row's mint is its `address` field.
    let mut count = 0u32;
    let items = crate::gmgn::flatten_trenches_tokens(&envelope.data);
    for item in items {
        let mint = item
            .get("address")
            .or_else(|| item.get("mint"))
            .or_else(|| item.get("token_address"))
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
pub async fn run_workers(ctx: std::sync::Arc<WorkerContext>, gmgn: Option<std::sync::Arc<GmgnPool>>) {
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
        let discover_gmgn = gmgn.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                if !discover_ctx.queue_allowed(crate::queues::QUEUE_SEED_SYNC).await {
                    continue;
                }
                if let Err(err) = discover_tokens_once(&discover_ctx.pool, &discover_gmgn, ChainKind::Solana).await {
                    tracing::warn!(error = %err, "discovery failed");
                }
            }
        });

        // Smart Wallets track (separate from Funding Radar): poll GMGN
        // smartmoney/kol trades, and periodically enrich with wallet_stats.
        let poll_ctx = ctx.clone();
        let poll_gmgn = gmgn.clone();
        tokio::spawn(async move {
            smart_wallet_poll_loop(poll_ctx, poll_gmgn).await;
        });
        let enrich_ctx = ctx.clone();
        let enrich_gmgn = gmgn.clone();
        tokio::spawn(async move {
            smart_wallet_enrich_loop(enrich_ctx, enrich_gmgn).await;
        });
    }
}

/// Smart Wallets poll worker: fetch real-time trades from GMGN
/// /v1/user/smartmoney + /v1/user/kol, upsert smart_wallets, insert
/// smart_wallet_trades, and store raw wallet observations. Interval from
/// config [smart_wallet].poll_interval_seconds.
pub async fn smart_wallet_poll_loop(ctx: std::sync::Arc<WorkerContext>, gmgn: std::sync::Arc<GmgnPool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(ctx.smart_wallet_config.poll_interval_seconds.max(5)));
    loop {
        interval.tick().await;
        if gmgn.is_empty() {
            continue; // no keys: sleep quietly
        }
        if !ctx.queue_allowed(crate::queues::QUEUE_SEED_SYNC).await {
            continue;
        }
        for (endpoint, source) in [("/v1/user/smartmoney", "smartmoney"), ("/v1/user/kol", "kol")] {
            let mut params = std::collections::BTreeMap::new();
            params.insert("chain".to_string(), "sol".to_string());
            match gmgn.v1_get(endpoint, params).await {
                Ok(env) => {
                    if let Err(err) = smart_wallet_ingest_trades(&ctx.pool, &env.data, source).await {
                        tracing::warn!(error = %err, source, "smart wallet ingest failed");
                    }
                }
                Err(err) => tracing::warn!(error = %err, source, "smart wallet poll failed"),
            }
        }
    }
}

/// Ingest one smartmoney/kol trades payload: upsert wallets + insert trades.
/// Real GMGN shape: data.list[] with maker, base_address, base_token.symbol,
/// side, amount_usd, timestamp, maker_info{twitter_username,twitter_name}.
async fn smart_wallet_ingest_trades(pool: &PgPool, data: &serde_json::Value, source: &str) -> Result<u32> {
    let rows = crate::admin::track_extract_trades_pub(data, source);
    let mut n = 0u32;
    for row in &rows {
        let wallet = row.get("wallet").and_then(|v| v.as_str()).unwrap_or("");
        if wallet.is_empty() {
            continue;
        }
        let twitter_username = row.get("twitter_username").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let twitter_name = row.get("twitter_name").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        // Upsert the wallet (fill identity when present, bump last_seen_at).
        sqlx::query(
            r#"
            INSERT INTO smart_wallets (chain, address, source, twitter_username, twitter_name)
            VALUES ('solana', $1, $2, $3, $4)
            ON CONFLICT (chain, address) DO UPDATE
              SET last_seen_at = now(),
                  twitter_username = COALESCE(EXCLUDED.twitter_username, smart_wallets.twitter_username),
                  twitter_name = COALESCE(EXCLUDED.twitter_name, smart_wallets.twitter_name)
            "#,
        )
        .bind(wallet)
        .bind(source)
        .bind(twitter_username)
        .bind(twitter_name)
        .execute(pool)
        .await?;

        // Insert the trade (idempotent).
        let ts = row.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        let trade_ts = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_else(Utc::now);
        let mint = row.get("token_address").and_then(|v| v.as_str()).unwrap_or("");
        if mint.is_empty() {
            continue;
        }
        let side = row.get("side").and_then(|v| v.as_str()).unwrap_or("");
        let amount_usd = row.get("usd").and_then(|v| v.as_f64());
        let symbol = row.get("token_symbol").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        sqlx::query(
            r#"
            INSERT INTO smart_wallet_trades (chain, wallet, mint, symbol, side, amount_usd, trade_ts, source, raw)
            VALUES ('solana', $1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (chain, wallet, mint, side, trade_ts) DO NOTHING
            "#,
        )
        .bind(wallet)
        .bind(mint)
        .bind(symbol)
        .bind(side)
        .bind(amount_usd)
        .bind(trade_ts)
        .bind(source)
        .bind(row)
        .execute(pool)
        .await?;

        // Raw-first observation.
        let _ = crate::ingest::store_gmgn_wallet_observation(
            pool,
            ChainKind::Solana,
            wallet,
            source,
            "realtime",
            row,
            Utc::now(),
        )
        .await;
        n += 1;
    }
    Ok(n)
}

/// Smart Wallets enrich worker (every 15 min): pull wallet_stats for up to 10
/// candidates (most active first, stale stats), store stats + identity, and
/// promote/dismiss based on the configured thresholds.
pub async fn smart_wallet_enrich_loop(ctx: std::sync::Arc<WorkerContext>, gmgn: std::sync::Arc<GmgnPool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(15 * 60));
    loop {
        interval.tick().await;
        if gmgn.is_empty() {
            continue;
        }
        if !ctx.queue_allowed(crate::queues::QUEUE_SEED_SYNC).await {
            continue;
        }
        if let Err(err) = smart_wallet_enrich_once(&ctx, &gmgn).await {
            tracing::warn!(error = %err, "smart wallet enrich failed");
        }
    }
}

/// Pure status-decision for one wallet after enrichment (testable).
///
/// `group_name` = the assigned tier ("" = no group met). Returns
/// (new_status, new_consecutive_fails):
/// - in a group, or manually pinned → ("tracked", 0). Pinned wallets are never
///   demoted to candidate or dismissed.
/// - no group → fails+1; at `dismiss_after_fails` → ("dismissed", fails),
///   else ("candidate", fails).
fn smart_wallet_status_decision(
    group_name: &str,
    manually_pinned: bool,
    prev_fails: i32,
    dismiss_after_fails: u32,
) -> (&'static str, i32) {
    let meets = !group_name.is_empty();
    let fails = if meets { 0 } else { prev_fails + 1 };
    let status = if meets || manually_pinned {
        "tracked"
    } else if fails >= dismiss_after_fails as i32 {
        "dismissed"
    } else {
        "candidate"
    };
    (status, fails)
}

/// Extract win rate, realized pnl (USD), and trade count from a wallet_stats
/// payload (defensive path probing; values may be strings or numbers).
fn smart_wallet_stats_metrics(stats: &serde_json::Value) -> (Option<f64>, Option<f64>, Option<i64>) {
    let num = |v: &serde_json::Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()));
    let get = |paths: &[&str]| paths.iter().find_map(|p| stats.pointer(p).and_then(num));
    let win_rate = get(&["/pnl_stat/winrate", "/winrate", "/win_rate"]);
    let realized_pnl = get(&["/realized_profit", "/realized_profit_usd", "/realizedPnl"]);
    let buy = get(&["/buy", "/buy_count"]).unwrap_or(0.0);
    let sell = get(&["/sell", "/sell_count"]).unwrap_or(0.0);
    let trades = get(&["/total_trades", "/trade_count"]).map(|t| t as i64).or(Some((buy + sell) as i64));
    (win_rate, realized_pnl, trades)
}

async fn smart_wallet_enrich_once(ctx: &std::sync::Arc<WorkerContext>, gmgn: &std::sync::Arc<GmgnPool>) -> Result<u32> {
    let batch = ctx.smart_wallet_config.enrich_batch.max(1) as i64;
    let mut enriched = 0u32;

    // 1) Drain manually requested wallets FIRST (enrich_requested_at set via
    //    the admin endpoints), clearing the flag as each is processed.
    let requested: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT w.address, w.tracked
          FROM smart_wallets w
         WHERE w.enrich_requested_at IS NOT NULL
           AND w.status <> 'dismissed'
         ORDER BY w.enrich_requested_at ASC
         LIMIT $1
        "#,
    )
    .bind(batch)
    .fetch_all(&ctx.pool)
    .await?;
    if !requested.is_empty() {
        tracing::info!(count = requested.len(), "smart wallet manual enrich drain");
    }
    for (address, pinned) in requested {
        match smart_wallet_enrich_one(ctx, gmgn, &address, pinned).await {
            Ok(()) => enriched += 1,
            Err(err) => tracing::warn!(error = %err, address, "manual enrich failed"),
        }
        sqlx::query("UPDATE smart_wallets SET enrich_requested_at = NULL WHERE chain='solana' AND address=$1")
            .bind(&address)
            .execute(&ctx.pool)
            .await?;
    }

    // 2) Normal stale batch (stats missing or >6h old), excluding wallets just
    //    drained above and any still-flagged manual requests.
    let candidates: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT w.address, w.tracked
          FROM smart_wallets w
         WHERE w.status IN ('candidate', 'tracked')
           AND w.enrich_requested_at IS NULL
           AND (w.last_stats_at IS NULL OR w.last_stats_at < now() - interval '6 hours')
         ORDER BY (SELECT COUNT(*) FROM smart_wallet_trades t WHERE t.wallet = w.address) DESC,
                  w.last_seen_at DESC
         LIMIT $1
        "#,
    )
    .bind(batch)
    .fetch_all(&ctx.pool)
    .await?;

    for (address, manually_pinned) in candidates {
        match smart_wallet_enrich_one(ctx, gmgn, &address, manually_pinned).await {
            Ok(()) => enriched += 1,
            Err(err) => tracing::warn!(error = %err, address, "enrich failed"),
        }
    }
    Ok(enriched)
}

/// Enrich one wallet: fetch wallet_stats (one payload per distinct group
/// stats_period), evaluate groups against their period's payload, assign the
/// strictest matching group, and persist stats + identity + group/status.
async fn smart_wallet_enrich_one(
    ctx: &std::sync::Arc<WorkerContext>,
    gmgn: &std::sync::Arc<GmgnPool>,
    address: &str,
    manually_pinned: bool,
) -> Result<()> {
    let cfg = &ctx.smart_wallet_config;
    // Fetch one wallet_stats payload per distinct stats_period (usually 1).
    let mut payloads: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();
    let mut primary: Option<serde_json::Value> = None;
    for period in cfg.distinct_stats_periods() {
        let mut params = std::collections::BTreeMap::new();
        params.insert("chain".to_string(), "sol".to_string());
        params.insert("wallet_address".to_string(), address.to_string());
        params.insert("period".to_string(), period.clone());
        match gmgn.v1_get("/v1/user/wallet_stats", params).await {
            Ok(env) => {
                if primary.is_none() {
                    primary = Some(env.data.clone());
                }
                payloads.insert(period, env.data);
            }
            Err(err) => {
                tracing::warn!(error = %err, address, period, "wallet_stats fetch failed");
            }
        }
    }
    let Some(stats) = primary else {
        bail!("no wallet_stats payload fetched for {address}");
    };

    // Evaluate groups against their own period's payload.
    let group_name = cfg.assign_group_stats(&payloads);

    // Identity + metrics from the primary payload.
    let (win_rate, realized_pnl, trades) = smart_wallet_stats_metrics(&stats);
    let followers = stats.pointer("/common/followers_count").and_then(|v| v.as_i64()).map(|v| v as i32);
    let blue = stats.pointer("/common/is_blue_verified").and_then(|v| v.as_bool());
    let twitter_username = stats.pointer("/common/twitter_username").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let twitter_name = stats.pointer("/common/twitter_name").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

    let prev_fails: i32 = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT COALESCE((stats->>'_consecutive_fails')::int, 0) FROM smart_wallets WHERE chain='solana' AND address=$1",
    )
    .bind(address)
    .fetch_optional(&ctx.pool)
    .await
    .ok()
    .flatten()
    .flatten()
    .unwrap_or(0);
    let (new_status, fails) = smart_wallet_status_decision(
        &group_name,
        manually_pinned,
        prev_fails,
        cfg.dismiss_after_fails,
    );
    // Merge the fail counter + metrics into the stored stats payload.
    let mut stats_val = stats.clone();
    if let Some(obj) = stats_val.as_object_mut() {
        obj.insert("_consecutive_fails".to_string(), serde_json::json!(fails));
        obj.insert("_win_rate".to_string(), serde_json::json!(win_rate));
        obj.insert("_realized_pnl".to_string(), serde_json::json!(realized_pnl));
        obj.insert("_trades".to_string(), serde_json::json!(trades));
    }
    sqlx::query(
        r#"
        UPDATE smart_wallets
           SET stats = $2,
               last_stats_at = now(),
               status = $3,
               group_name = $4,
               followers_count = COALESCE($5, followers_count),
               is_blue_verified = COALESCE($6, is_blue_verified),
               twitter_username = COALESCE($7, twitter_username),
               twitter_name = COALESCE($8, twitter_name)
         WHERE chain = 'solana' AND address = $1
        "#,
    )
    .bind(address)
    .bind(&stats_val)
    .bind(new_status)
    .bind(&group_name)
    .bind(followers)
    .bind(blue)
    .bind(twitter_username)
    .bind(twitter_name)
    .execute(&ctx.pool)
    .await?;

    // Raw observation of the stats payload.
    let _ = crate::ingest::store_gmgn_wallet_observation(
        &ctx.pool,
        ChainKind::Solana,
        address,
        "wallet_stats",
        "30d",
        &stats_val,
        Utc::now(),
    )
    .await;
    if !group_name.is_empty() {
        tracing::info!(address, group = %group_name, win_rate, realized_pnl, trades, "smart wallet assigned group");
    }
    Ok(())
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_wallet_stats_metrics_reads_real_payload() {
        // Mirrors the verified live /v1/user/wallet_stats shape.
        let stats = serde_json::json!({
            "wallet_address": "4uCT",
            "realized_profit": "15234.5",
            "buy": 19312,
            "sell": 5102,
            "pnl_stat": { "winrate": 0.62 },
            "common": { "followers_count": 78, "is_blue_verified": false }
        });
        let (win_rate, pnl, trades) = smart_wallet_stats_metrics(&stats);
        assert_eq!(win_rate, Some(0.62));
        assert_eq!(pnl, Some(15234.5));
        assert_eq!(trades, Some(19312 + 5102));
    }

    #[test]
    fn smart_wallet_stats_metrics_handles_missing_fields() {
        let stats = serde_json::json!({ "buy": 3, "sell": 2 });
        let (win_rate, pnl, trades) = smart_wallet_stats_metrics(&stats);
        assert_eq!(win_rate, None);
        assert_eq!(pnl, None);
        assert_eq!(trades, Some(5));
    }
    #[test]
    fn status_decision_group_assigns_tracked_and_resets_fails() {
        let (status, fails) = smart_wallet_status_decision("solid", false, 2, 3);
        assert_eq!(status, "tracked");
        assert_eq!(fails, 0);
    }

    #[test]
    fn status_decision_dismisses_only_after_n_fails() {
        // No group met: candidate at 1 and 2 fails, dismissed at 3.
        assert_eq!(smart_wallet_status_decision("", false, 0, 3), ("candidate", 1));
        assert_eq!(smart_wallet_status_decision("", false, 1, 3), ("candidate", 2));
        assert_eq!(smart_wallet_status_decision("", false, 2, 3), ("dismissed", 3));
    }

    #[test]
    fn status_decision_pinned_never_dismissed_or_demoted() {
        // Manually pinned: stays tracked even with no group and many fails.
        let (status, fails) = smart_wallet_status_decision("", true, 5, 3);
        assert_eq!(status, "tracked");
        assert_eq!(fails, 6, "fail counter still increments for visibility");
        // Pinned + in a group also tracked.
        assert_eq!(smart_wallet_status_decision("elite", true, 0, 3).0, "tracked");
    }
}
