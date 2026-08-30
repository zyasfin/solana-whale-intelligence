//! Solana Whale Intelligence — CLI entry point.
//!
//! Commands cover database migration, token discovery/reporting, Telegram
//! channel management, funding radar, wallet management, tracing, replay,
//! signal evaluation, webhook serving, and health.

mod admin;
mod alerts;
mod api;
mod auth;
mod chains;
mod config;
mod db;
mod filter;
mod funding_radar;
#[cfg(all(test, feature = "pg_tests"))]
mod funding_radar_pg_tests;
mod gmgn;
mod graph;
mod health;
mod helius;
mod ingest;
mod models;
mod narrative;
mod queues;
mod replay;
mod scoring;
mod signals;
mod telegram_client;
mod telegram_ingest;
mod telegram_parse;
mod workers;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use config::Settings;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "solana-whale-intelligence", version, about = "Solana-first multi-chain whale intelligence service")]
struct Cli {
    /// Path to config.toml
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Database operations
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Token discovery and reports
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Telegram channel management
    Telegram {
        #[command(subcommand)]
        action: TelegramAction,
    },
    /// Funding radar operations
    Funding {
        #[command(subcommand)]
        action: FundingAction,
    },
    /// Wallet management
    Wallet {
        #[command(subcommand)]
        action: WalletAction,
    },
    /// Trace wallet funding lineage
    Trace {
        /// Wallet address
        address: String,
        /// Chain: sol|solana|robinhood|rh
        #[arg(long, default_value = "solana")]
        chain: String,
        /// Maximum traversal depth
        #[arg(long, default_value = "3")]
        depth: u32,
    },
    /// Temporal replay
    Replay {
        /// Evaluation time (RFC3339); defaults to now
        #[arg(long)]
        at: Option<String>,
    },
    /// Signal evaluation
    Signal {
        #[command(subcommand)]
        action: SignalAction,
    },
    /// Webhook server
    Watch {
        #[command(subcommand)]
        action: WatchAction,
    },
    /// Run the worker set (radar, signals, alerts, streams)
    Run,
    /// Health report
    Health,
}

#[derive(Subcommand)]
enum DbAction {
    /// Apply migrations
    Migrate,
    /// Hash a password for ADMIN_PASSWORD_HASH
    HashPassword,
}

#[derive(Subcommand)]
enum TokenAction {
    /// Run one discovery pass (GMGN enrichment)
    Discover {
        #[arg(long)]
        once: bool,
    },
    /// Print a token report
    Report {
        mint: String,
        /// Chain: sol|solana|robinhood|rh
        #[arg(long, default_value = "solana")]
        chain: String,
    },
}

#[derive(Subcommand)]
enum TelegramAction {
    /// Interactive MTProto login (run once)
    Auth,
    /// Channel allowlist management
    Channels {
        #[command(subcommand)]
        action: ChannelsAction,
    },
    /// Backfill channel history
    Backfill {
        channel_key: String,
        /// Start time (RFC3339)
        #[arg(long)]
        from: String,
        /// End time (RFC3339), default now
        #[arg(long)]
        to: Option<String>,
    },
}

#[derive(Subcommand)]
enum ChannelsAction {
    /// List allowlisted channels
    List,
    /// Add a public channel to the allowlist by username or id
    Add {
        username_or_id: String,
    },
    /// Remove a channel from the allowlist (history retained)
    Remove {
        channel_key: String,
    },
}

#[derive(Subcommand)]
enum FundingAction {
    /// Radar case operations
    Radar {
        #[command(subcommand)]
        action: RadarAction,
    },
}

#[derive(Subcommand)]
enum RadarAction {
    /// List radar cases
    List {
        /// Filter by stage
        #[arg(long)]
        stage: Option<String>,
        /// Maximum rows
        #[arg(long, default_value = "20")]
        limit: i64,
    },
    /// Inspect one radar case
    Inspect {
        case_id: i64,
    },
    /// Evaluate a recipient's radar case
    Evaluate {
        recipient: String,
        /// Chain
        #[arg(long, default_value = "solana")]
        chain: String,
    },
}

#[derive(Subcommand)]
enum WalletAction {
    /// Sync wallet history (Helius canonical)
    Sync {
        address: String,
        #[arg(long, default_value = "solana")]
        chain: String,
        #[arg(long, default_value = "5")]
        max_pages: u32,
    },
    /// Recompute wallet scores
    Score {
        address: String,
        #[arg(long, default_value = "solana")]
        chain: String,
    },
    /// Top wallets by score
    Leaderboard {
        #[arg(long, default_value = "solana")]
        chain: String,
        #[arg(long, default_value = "20")]
        limit: i64,
    },
    /// Manually block a wallet (authoritative, reversible)
    Block {
        address: String,
        #[arg(long, default_value = "solana")]
        chain: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Mark a wallet flow-only (edges retained, no alpha)
    FlowOnly {
        address: String,
        #[arg(long, default_value = "solana")]
        chain: String,
    },
    /// Watch a wallet lightly
    Watch {
        address: String,
        #[arg(long, default_value = "solana")]
        chain: String,
    },
    /// Revoke manual labels (unblock)
    Unblock {
        address: String,
        #[arg(long, default_value = "solana")]
        chain: String,
        #[arg(long)]
        kind: Option<String>,
    },
    /// List labels for a wallet
    Labels {
        address: String,
        #[arg(long, default_value = "solana")]
        chain: String,
    },
    /// Import a blocklist (one address per line)
    ImportBlocklist {
        path: String,
        #[arg(long, default_value = "solana")]
        chain: String,
    },
}

#[derive(Subcommand)]
enum SignalAction {
    /// List recent evaluations
    Rejected {
        #[arg(long, default_value = "20")]
        limit: i64,
    },
    /// Evaluate signals for one token now
    Evaluate {
        mint: String,
        /// Chain: sol|solana|robinhood|rh
        #[arg(long, default_value = "solana")]
        chain: String,
    },
}

#[derive(Subcommand)]
enum WatchAction {
    /// Serve the authenticated webhook endpoint
    ServeWebhook {
        /// Optional explicit bind address override
        #[arg(long)]
        bind: Option<String>,
    },
    /// Serve the read-only REST API (JSON) for the dashboard
    ServeApi {
        /// Optional explicit bind address override
        #[arg(long)]
        bind: Option<String>,
    },
    /// Serve the full admin panel (auth + management UI)
    ServeAdmin {
        /// Optional explicit bind address override
        #[arg(long)]
        bind: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let settings = Settings::load(cli.config.clone())?;

    match cli.command {
        Command::Db { action } => match action {
            DbAction::Migrate => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                db::migrate(&pool).await?;
                println!("migrations applied");
            }
            DbAction::HashPassword => {
                use std::io::{self, Write};
                print!("password: ");
                io::stdout().flush()?;
                let mut pw = String::new();
                io::stdin().read_line(&mut pw)?;
                let hash = auth::hash_password(pw.trim())?;
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(hash.as_bytes());
                println!("add to .env:");
                println!("ADMIN_PASSWORD_HASH_B64={b64}");
            }
        },
        Command::Token { action } => match action {
            TokenAction::Discover { once: _ } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, db::pool_size(settings.config.runtime_profile.provider_workers())).await?;
                db::migrate(&pool).await?;
                narrative::seed_narratives(&pool).await?;
                let gmgn = gmgn::GmgnClient::new(
                    settings.config.gmgn.clone(),
                    settings.env.gmgn_api_key.clone(),
                );
                if !gmgn.is_configured() {
                    bail!("GMGN_API_KEY missing; discovery disabled");
                }
                let discovered = workers::discover_tokens_once(&pool, &gmgn, models::ChainKind::Solana).await?;
                println!("discovered {} token(s) from GMGN", discovered);
            }
            TokenAction::Report { mint, chain } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, db::pool_size(settings.config.runtime_profile.provider_workers())).await?;
                let report = narrative::explain_narrative(&pool, chain, &mint, Utc::now()).await?;
                match report {
                    Some(report) => {
                        println!("chain: {}", report.chain);
                        println!("mint: {}", report.mint);
                        println!("narrative: {}", report.narrative);
                        println!("confidence: {}", report.confidence);
                        println!("why_now: {}", report.why_now);
                        for counter in &report.counter_evidence {
                            println!("counter_evidence: {counter}");
                        }
                    }
                    None => println!("no narrative evidence for {mint}"),
                }
            }
        },
        Command::Telegram { action } => match action {
            TelegramAction::Auth => {
                let (api_id, api_hash, session_path) = (
                    settings.env.tg_api_id,
                    settings.env.tg_api_hash.clone(),
                    settings.env.tg_session_path.clone(),
                );
                let (Some(api_id), Some(api_hash), Some(session_path)) = (api_id, api_hash, session_path) else {
                    bail!("set TG_API_ID, TG_API_HASH, and TG_SESSION_PATH (session outside the repository)");
                };
                telegram_client::ensure_session_outside_repo(&session_path)?;
                let tg = telegram_client::connect(api_id as i32, &session_path).await?;
                telegram_client::interactive_auth(&tg, &api_hash).await?;
            }
            TelegramAction::Channels { action } => match action {
                ChannelsAction::List => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let rows: Vec<(String, String)> = sqlx::query_as(
                    "SELECT channel_key, status FROM telegram_channels ORDER BY channel_key",
                )
                .fetch_all(&pool)
                .await?;
                if rows.is_empty() {
                    println!("no channels configured");
                }
                for (key, status) in rows {
                    println!("{key}: {status}");
                }
            }
                ChannelsAction::Add { username_or_id } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                sqlx::query(
                    r#"
                    INSERT INTO telegram_channels (channel_key, username, allowlisted, status)
                    VALUES ($1, $2, true, 'active')
                    ON CONFLICT (channel_key) DO UPDATE
                        SET allowlisted = true, status = 'active'
                    "#,
                )
                .bind(&username_or_id)
                .bind(&username_or_id)
                .execute(&pool)
                .await?;
                println!("channel {username_or_id} allowlisted (no auto-join)");
            }
                ChannelsAction::Remove { channel_key } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                sqlx::query(
                    "UPDATE telegram_channels SET allowlisted = false, status = 'disabled' WHERE channel_key = $1",
                )
                .bind(&channel_key)
                .execute(&pool)
                .await?;
                println!("channel {channel_key} removed from allowlist (history retained)");
                }
            }
            TelegramAction::Backfill { channel_key, from, to } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let from_time = chrono::DateTime::parse_from_rfc3339(&from)
                    .with_context(|| "invalid --from RFC3339")?
                    .with_timezone(&Utc);
                let to_time = match to {
                    Some(to) => chrono::DateTime::parse_from_rfc3339(&to)
                        .with_context(|| "invalid --to RFC3339")?
                        .with_timezone(&Utc),
                    None => Utc::now(),
                };
                println!(
                    "backfill {channel_key} from {} to {} requires an active MTProto session",
                    from_time.to_rfc3339(),
                    to_time.to_rfc3339()
                );
                let _ = &pool;
            }
        },
        Command::Funding { action } => match action {
            FundingAction::Radar { action } => match action {
                RadarAction::List { stage, limit } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let rows: Vec<(i64, String, String, String, i32)> = sqlx::query_as(
                    r#"
                    SELECT id, chain, recipient, stage, confidence
                      FROM funding_radar_cases
                     WHERE ($1::text IS NULL OR stage = $1)
                     ORDER BY updated_at DESC
                     LIMIT $2
                    "#,
                )
                .bind(stage)
                .bind(limit)
                .fetch_all(&pool)
                .await?;
                for (id, chain, recipient, stage, confidence) in rows {
                    println!("{id}: {chain} {} stage={stage} confidence={confidence}", models::short_addr(&recipient));
                }
            }
                RadarAction::Inspect { case_id } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let events: Vec<(String, chrono::DateTime<Utc>, serde_json::Value)> = sqlx::query_as(
                    "SELECT event_kind, observed_at, evidence FROM funding_radar_events WHERE case_id = $1 ORDER BY observed_at",
                )
                .bind(case_id)
                .fetch_all(&pool)
                .await?;
                println!("case {case_id}: {} events", events.len());
                for (kind, at, evidence) in events {
                    println!("  {kind} at {}: {}", at.to_rfc3339(), serde_json::to_string(&evidence).unwrap_or_default());
                }
            }
                RadarAction::Evaluate { recipient, chain } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let decision = funding_radar::evaluate_radar_case(
                    &pool,
                    chain,
                    &recipient,
                    Utc::now(),
                    &settings.config.funding_radar,
                )
                .await?;
                println!(
                    "case {} stage={:?} confidence={} reason={}",
                    decision.case_id, decision.stage, decision.confidence, decision.reason
                );
                if let Some(alert) = decision.alert {
                    println!("alert {}: {}", alert.kind.as_str(), alert.message);
                }
                }
            }
        },
        Command::Wallet { action } => match action {
            WalletAction::Sync { address, chain, max_pages } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, db::pool_size(settings.config.runtime_profile.provider_workers())).await?;
                db::migrate(&pool).await?;
                let adapter = chains::SolanaAdapter::new();
                let helius = helius::HeliusPool::new(settings.env.helius_keys.clone(), settings.config.helius.clone());
                if helius.provider_count() == 0 {
                    bail!("no HELIUS_KEY_1 configured; set user-owned keys");
                }
                let outcome = ingest::sync_wallet(&pool, &helius, chain, &address, &adapter, max_pages).await?;
                println!(
                    "pages={} transfers_new={} trades_new={} completed={}",
                    outcome.pages_fetched, outcome.transfers_new, outcome.trades_new, outcome.completed
                );
            }
            WalletAction::Score { address, chain } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                // Recompute from current trade history (idempotent per as_of minute).
                let as_of = chrono::DateTime::from_timestamp(
                    Utc::now().timestamp() - (Utc::now().timestamp() % 60),
                    0,
                )
                .unwrap_or_else(Utc::now);
                let result = workers::score_wallet(
                    &pool,
                    chain,
                    &address,
                    as_of,
                    &settings.config.scoring,
                )
                .await?;
                println!(
                    "skill={} copyability={} conviction={} provisional={}",
                    result.skill, result.copyability, result.conviction, result.provisional
                );
            }
            WalletAction::Leaderboard { chain, limit } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let rows: Vec<(String, i32, i32)> = sqlx::query_as(
                    r#"
                    SELECT DISTINCT ON (address) address, skill_score, conviction
                      FROM wallet_scores
                     WHERE chain = $1
                     ORDER BY address, as_of DESC
                    "#,
                )
                .bind(chain.as_str())
                .fetch_all(&pool)
                .await?;
                let mut sorted = rows;
                sorted.sort_by_key(|(_, _, conviction)| -conviction);
                for (address, skill, conviction) in sorted.into_iter().take(limit as usize) {
                    println!("{} skill={skill} conviction={conviction}", models::short_addr(&address));
                }
            }
            WalletAction::Block { address, chain, reason } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                db::upsert_wallet(&pool, chain.as_str(), &address, Utc::now(), "manual").await?;
                db::add_wallet_label(
                    &pool,
                    chain.as_str(),
                    &address,
                    "manual_block",
                    "skip",
                    reason.as_deref().unwrap_or("manual block"),
                    "manual",
                    100,
                    true,
                    None,
                )
                .await?;
                println!("blocked {} (manual labels are authoritative and reversible)", models::short_addr(&address));
            }
            WalletAction::FlowOnly { address, chain } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                db::upsert_wallet(&pool, chain.as_str(), &address, Utc::now(), "manual").await?;
                db::add_wallet_label(
                    &pool,
                    chain.as_str(),
                    &address,
                    "manual_flow_only",
                    "flow_only",
                    "manual flow-only",
                    "manual",
                    100,
                    true,
                    None,
                )
                .await?;
                println!("flow-only {} (edges retained, excluded from alpha)", models::short_addr(&address));
            }
            WalletAction::Watch { address, chain } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                db::upsert_wallet(&pool, chain.as_str(), &address, Utc::now(), "manual").await?;
                db::add_wallet_label(
                    &pool,
                    chain.as_str(),
                    &address,
                    "manual_watch",
                    "watch",
                    "manual watch",
                    "manual",
                    100,
                    true,
                    None,
                )
                .await?;
                println!("watching {} (light monitoring)", models::short_addr(&address));
            }
            WalletAction::Unblock { address, chain, kind } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let revoked = db::revoke_wallet_label(&pool, chain.as_str(), &address, kind.as_deref(), Utc::now()).await?;
                println!("revoked {revoked} labels for {}", models::short_addr(&address));
            }
            WalletAction::Labels { address, chain } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let rows: Vec<(String, String, bool, Option<chrono::DateTime<Utc>>)> = sqlx::query_as(
                    r#"
                    SELECT kind, disposition, manual, revoked_at
                      FROM wallet_labels
                     WHERE chain = $1 AND address = $2
                     ORDER BY created_at DESC
                    "#,
                )
                .bind(chain.as_str())
                .bind(&address)
                .fetch_all(&pool)
                .await?;
                for (kind, disposition, manual, revoked) in rows {
                    let status = revoked.map(|_| "revoked").unwrap_or("active");
                    let source = if manual { "manual" } else { "auto" };
                    println!("{kind} disposition={disposition} source={source} status={status}");
                }
            }
            WalletAction::ImportBlocklist { path, chain } => {
                let chain = parse_chain(&chain)?;
                let content = std::fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let mut imported = 0u32;
                for line in content.lines() {
                    let address = line.trim();
                    if address.is_empty() || address.starts_with('#') {
                        continue;
                    }
                    db::upsert_wallet(&pool, chain.as_str(), address, Utc::now(), "blocklist").await?;
                    db::add_wallet_label(
                        &pool,
                        chain.as_str(),
                        address,
                        "manual_block",
                        "skip",
                        "imported blocklist",
                        "manual",
                        100,
                        true,
                        None,
                    )
                    .await?;
                    imported += 1;
                }
                println!("imported {imported} blocked wallets");
            }
        },
        Command::Trace { address, chain, depth } => {
            let chain = parse_chain(&chain)?;
            let database_url = require_database_url(&settings)?;
            let pool = db::connect(&database_url, 2).await?;
            let steps = graph::trace_wallet(&pool, chain, &address, depth).await?;
            if steps.is_empty() {
                println!("no funding edges for {}", models::short_addr(&address));
            }
            for step in steps {
                println!(
                    "{} {} via {} endpoint={}",
                    step.direction,
                    models::short_addr(&step.address),
                    &step.via_signature.get(..8).unwrap_or(&step.via_signature),
                    step.endpoint
                );
            }
        }
        Command::Replay { at } => {
            let evaluation_time = match at {
                Some(at) => chrono::DateTime::parse_from_rfc3339(&at)
                    .with_context(|| "invalid --at RFC3339")?
                    .with_timezone(&Utc),
                None => Utc::now(),
            };
            let database_url = require_database_url(&settings)?;
            let pool = db::connect(&database_url, 2).await?;
            let accepted: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM signal_evaluations WHERE status = 'accepted'",
            )
            .fetch_one(&pool)
            .await?;
            let rejected: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM signal_evaluations WHERE status = 'rejected'",
            )
            .fetch_one(&pool)
            .await?;
            println!("replay at {}", evaluation_time.to_rfc3339());
            println!("accepted={accepted} rejected={rejected}");
        }
        Command::Signal { action } => match action {
            SignalAction::Rejected { limit } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let rows: Vec<(String, String, Option<String>, chrono::DateTime<Utc>)> = sqlx::query_as(
                    r#"
                    SELECT chain, mint, rejection_code, evaluated_at
                      FROM signal_evaluations
                     WHERE status = 'rejected'
                     ORDER BY evaluated_at DESC
                     LIMIT $1
                    "#,
                )
                .bind(limit)
                .fetch_all(&pool)
                .await?;
                for (chain, mint, code, at) in rows {
                    println!("{chain} {mint} rejected={:?} at {}", code, at.to_rfc3339());
                }
            }
            SignalAction::Evaluate { mint, chain } => {
                let chain = parse_chain(&chain)?;
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let result = workers::evaluate_token_signals(
                    &pool,
                    chain,
                    &mint,
                    Utc::now(),
                    &settings.config.signals,
                )
                .await?;
                match result {
                    Some(id) => println!("signal id {id} created for {mint}"),
                    None => println!("no signal for {mint} (rejection recorded)"),
                }
            }
        },
        Command::Watch { action } => match action {
            WatchAction::ServeWebhook { bind } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, db::pool_size(settings.config.runtime_profile.provider_workers())).await?;
                db::migrate(&pool).await?;
                let bind = bind
                    .or_else(|| settings.config.server.webhook_bind.clone())
                    .unwrap_or_else(|| "127.0.0.1:8787".to_string());
                let auth_token = settings
                    .config
                    .server
                    .webhook_auth_token
                    .clone()
                    .filter(|t| !t.trim().is_empty());
                if auth_token.is_none() {
                    bail!("webhook auth token missing; set server.webhook_auth_token in config.toml");
                }
                serve_webhook(pool, bind, auth_token.unwrap()).await?;
            }
            WatchAction::ServeApi { bind } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                let bind = bind
                    .or_else(|| settings.config.server.webhook_bind.clone())
                    .unwrap_or_else(|| "127.0.0.1:8788".to_string());
                let listener = tokio::net::TcpListener::bind(&bind)
                    .await
                    .with_context(|| format!("failed to bind api at {bind}"))?;
                tracing::info!(%bind, "REST API listening (read-only)");
                axum::serve(listener, api::router(api::ApiState { pool })).await?;
            }
            WatchAction::ServeAdmin { bind } => {
                let database_url = require_database_url(&settings)?;
                let pool = db::connect(&database_url, 2).await?;
                db::migrate(&pool).await?;
                let bind = bind
                    .or_else(|| settings.config.server.webhook_bind.clone())
                    .unwrap_or_else(|| "127.0.0.1:8789".to_string());
                if !auth::auth_configured() {
                    tracing::warn!("ADMIN_PASSWORD_HASH not set; admin panel runs in open mode (mutations blocked)");
                }
                {
                    let prune_pool = pool.clone();
                    tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(30 * 60));
                        loop {
                            interval.tick().await;
                            if let Err(err) = auth::prune_attempts(&prune_pool).await {
                                tracing::error!(error = %err, "login_attempts prune failed");
                            }
                        }
                    });
                }
                let listener = tokio::net::TcpListener::bind(&bind)
                    .await
                    .with_context(|| format!("failed to bind admin at {bind}"))?;
                tracing::info!(%bind, "admin panel listening");
                axum::serve(listener, admin::router(admin::AdminState { pool, settings: std::sync::Arc::new(settings.clone()) })).await?;
            }
        },
        Command::Run => {
            let database_url = require_database_url(&settings)?;
            let pool = db::connect(&database_url, db::pool_size(settings.config.runtime_profile.provider_workers())).await?;
            db::migrate(&pool).await?;
            narrative::seed_narratives(&pool).await?;

            let gmgn = if settings.env.gmgn_api_key.is_some() {
                Some(gmgn::GmgnClient::new(settings.config.gmgn.clone(), settings.env.gmgn_api_key.clone()))
            } else {
                tracing::warn!("GMGN_API_KEY missing; discovery worker disabled");
                None
            };

            let ctx = std::sync::Arc::new(workers::WorkerContext::new(pool, &settings));

            // Optional Robinhood funding poller over the Helius multi-chain
            // EVM endpoint (requires HELIUS_KEY_1; disabled without it).
            let robinhood_enabled = !settings.env.helius_keys.is_empty()
                && !settings.config.chains.robinhood.chain_id.is_empty();
            if robinhood_enabled {
                let robinhood_rpc_url = settings.config.helius.evm_http_url(
                    &settings.config.chains.robinhood.helius_slug,
                    &settings.env.helius_keys[0],
                );
                let rpc = chains::HttpEvmRpc::new(robinhood_rpc_url, 30)?;
                let mut adapter = chains::RobinhoodAdapter::new(
                    rpc,
                    settings.config.chains.robinhood.chain_id.clone(),
                    settings.config.chains.robinhood.native_decimals,
                    settings.config.chains.robinhood.start_block,
                );
                match adapter.validate_chain_id().await {
                    Ok(()) => {
                        let rh_ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = workers::robinhood_funding_loop(rh_ctx, adapter).await {
                                tracing::error!(error = %err, "robinhood funding loop stopped");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "robinhood adapter disabled at startup");
                    }
                }
            }

            tracing::info!("workers started: radar, alerts, discovery");
            // Solana filtered funding stream (requires HELIUS_KEY_1).
            if let Some(helius_key) = settings.env.helius_keys.first().cloned() {
                let ws_ctx = ctx.clone();
                tokio::spawn(async move {
                    let mut backoff = Duration::from_secs(2);
                    loop {
                        match workers::solana_funding_stream(ws_ctx.clone(), helius_key.clone(), Vec::new()).await {
                            Ok(()) => {
                                tracing::warn!("solana funding stream ended; reconnecting");
                                backoff = Duration::from_secs(2);
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, wait = backoff.as_secs(), "solana funding stream error; backing off");
                            }
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                    }
                });
            } else {
                tracing::warn!("no HELIUS_KEY_1; solana funding stream disabled");
            }

            // Telegram MTProto polling worker (requires session + credentials).
            let telegram_configured = settings.config.telegram.enabled
                && settings.env.tg_api_id.is_some()
                && settings.env.tg_session_path.is_some();
            if telegram_configured {
                let session_path = settings.env.tg_session_path.clone().unwrap();
                let api_id = settings.env.tg_api_id.unwrap() as i32;
                let tg_store = std::sync::Arc::new(telegram_ingest::PgMessageStore::new(ctx.pool.clone()));
                let tg_db = ctx.pool.clone();
                let tg_limit = settings.config.telegram.backfill_limit_per_channel as usize;
                match telegram_client::connect(api_id, &session_path).await {
                    Ok(tg) => {
                        match tg.client.is_authorized().await {
                            Ok(true) => {
                                tracing::info!("telegram polling worker started");
                                tokio::spawn(async move {
                                    telegram_client::telegram_poll_loop(tg, tg_store, tg_db, 30, tg_limit).await;
                                });
                            }
                            Ok(false) => {
                                tracing::warn!("telegram session not authorized; run `telegram auth` first");
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "telegram authorization check failed");
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "telegram client connect failed");
                    }
                }
            } else {
                tracing::warn!("telegram not configured; polling worker disabled");
            }

            workers::run_workers(ctx, gmgn).await;
            // Block forever; workers run until killed.
            std::future::pending::<()>().await;
        },
        Command::Health => {
            let database_url = require_database_url(&settings)?;
            let pool = db::connect(&database_url, 2).await?;
            let robinhood_configured = !settings.env.helius_keys.is_empty()
                && !settings.config.chains.robinhood.chain_id.is_empty();
            let chain_health = health::chain_health(
                settings.config.chains.solana.enabled,
                robinhood_configured,
                false,
                if robinhood_configured {
                    None
                } else {
                    Some("missing HELIUS_KEY_1 or robinhood_chain_id")
                },
                None,
            );
            let helius = helius::HeliusPool::new(settings.env.helius_keys.clone(), settings.config.helius.clone());
            let gmgn = gmgn::GmgnClient::new(settings.config.gmgn.clone(), settings.env.gmgn_api_key.clone());
            let bucket_tokens = if gmgn.is_configured() {
                Some(gmgn.bucket_tokens().await)
            } else {
                None
            };
            let report = health::report(
                &pool,
                chain_health,
                helius.provider_count(),
                gmgn.is_configured(),
                bucket_tokens,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

fn require_database_url(settings: &Settings) -> Result<String> {
    settings
        .env
        .database_url
        .clone()
        .filter(|url| !url.trim().is_empty())
        .context("DATABASE_URL not set; copy .env.example to .env and configure it")
}

fn parse_chain(value: &str) -> Result<models::ChainKind> {
    models::ChainKind::parse(value).with_context(|| format!("unknown chain '{value}'"))
}

/// Serve the authenticated Helius webhook endpoint.
///
/// Auth: `Authorization: Bearer <token>`. Duplicate deliveries deduplicate by
/// signature through raw-first storage.
async fn serve_webhook(pool: sqlx::PgPool, bind: String, auth_token: String) -> Result<()> {
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::Router;

    #[derive(Clone)]
    struct AppState {
        pool: sqlx::PgPool,
        auth_token: String,
    }

    async fn handle_webhook(
        State(state): State<AppState>,
        headers: HeaderMap,
        body: String,
    ) -> StatusCode {
        let authorized = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|token| token == state.auth_token)
            .unwrap_or(false);
        if !authorized {
            return StatusCode::UNAUTHORIZED;
        }
        let payload: serde_json::Value = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(_) => return StatusCode::BAD_REQUEST,
        };
        let transactions: Vec<serde_json::Value> = payload
            .as_array()
            .cloned()
            .or_else(|| Some(vec![payload]))
            .unwrap_or_default();
        let adapter = chains::SolanaAdapter::new();
        match ingest::ingest_webhook(&state.pool, &transactions, "helius_webhook", &adapter).await {
            Ok(_) => StatusCode::OK,
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    let state = AppState { pool, auth_token };
    let app = Router::new().route("/webhook", post(handle_webhook)).with_state(state);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    tracing::info!(%bind, "webhook server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::RuntimeProfile;

    #[test]
    fn parse_chain_accepts_aliases() {
        assert_eq!(parse_chain("sol").unwrap(), models::ChainKind::Solana);
        assert_eq!(parse_chain("solana").unwrap(), models::ChainKind::Solana);
        assert_eq!(parse_chain("robinhood").unwrap(), models::ChainKind::Robinhood);
        assert_eq!(parse_chain("rh").unwrap(), models::ChainKind::Robinhood);
        assert!(parse_chain("bitcoin").is_err());
    }

    #[test]
    fn runtime_profiles_available() {
        assert_eq!(RuntimeProfile::Low.telegram_concurrency(), 4);
        assert_eq!(RuntimeProfile::Scale.telegram_concurrency(), 32);
    }
}
