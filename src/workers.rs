//! Runtime workers: discovery, radar evaluation, signal evaluation, alert
//! dispatch, and chain funding streams. Each worker is a long-running loop
//! spawned by the `run` command; every worker pauses under queue backpressure.

#![allow(dead_code)]

use crate::db;
use crate::gmgn::GmgnClient;
use crate::models::{ChainKind, TradeSide};
use crate::queues::QueueState;
use crate::signals::{ExitGates, MarketGate, TokenGate};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::Mutex;

/// Shared application state for workers.
pub struct WorkerContext {
    pub pool: PgPool,
    /// Workspace this worker process operates in (REV-046-A4).
    ///
    /// Bound once at startup from configuration — the job context — so no provider
    /// payload or request body can steer which tenant a resolution lands in.
    pub workspace: solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope,
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
    /// Build the worker context.
    ///
    /// `workspace` is resolved from the job context by the caller and validated by
    /// `WorkspaceScope`, so an invalid tenant fails startup rather than producing
    /// rows in the wrong workspace (REV-046-A4).
    pub fn new(
        pool: PgPool,
        settings: &crate::config::Settings,
        workspace: solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope,
    ) -> Self {
        Self {
            pool,
            workspace,
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

    /// Whether a queue may run (REV-076-F02).
    ///
    /// The DURABLE `queue_state` table is the authority — the admin API writes it
    /// (audit-logged), and that is what makes "backpressure" an operational
    /// mechanism rather than a claim. The in-memory map remains as a local
    /// override for tests and for the bottom-up pressure computer; a pause in
    /// EITHER stops the queue.
    /// REV-078-F05: the durable read FAILS CLOSED. The REV-077 shape logged the
    /// error and returned `true`, so a permission outage or schema error silently
    /// bypassed the operational stop control — the exact moment backpressure is
    /// needed most. An unreadable authority means "do not run", and the error is
    /// surfaced for the caller to log, not swallowed here.
    pub async fn queue_allowed(&self, queue: &str) -> bool {
        match self.queue_allowed_checked(queue).await {
            Ok(allowed) => allowed,
            Err(e) => {
                tracing::error!(error = %e, queue = %queue, "queue_state unreadable; failing CLOSED");
                false
            }
        }
    }

    /// The checked form: Ok = authority answered, Err = authority unreadable.
    pub async fn queue_allowed_checked(&self, queue: &str) -> Result<bool> {
        if self.queue_state.lock().await.is_paused(queue) {
            return Ok(false);
        }
        let paused: Option<bool> = sqlx::query_scalar(
            "SELECT paused FROM queue_state WHERE queue = $1",
        )
        .bind(queue)
        .fetch_optional(&self.pool)
        .await?;
        Ok(!paused.unwrap_or(false))
    }
}

/// One GMGN discovery pass: fetch the newest tokens from `trenches`.
///
/// GMGN is enrichment: discovered tokens become `tokens` rows and GMGN
/// observations; they never become canonical scores by themselves.
/// Discover tokens for one chain, and resolve Recent intelligence for each new one.
///
/// `workspace` is threaded in from the worker's job context (REV-046-A4): it must not
/// be derived from provider payload, so the caller supplies it and the type prevents
/// a bare integer from being passed by accident.
pub async fn discover_tokens_once(
    pool: &PgPool,
    gmgn: &GmgnClient,
    chain: ChainKind,
    workspace: solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope,
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

        // REV-046-A4: the production caller for Recent intelligence.
        //
        // `resolve_candidates_from_store` and `insert_recent_event` existed with no
        // production caller, so resolved relations were never persisted and the API
        // had nothing to serve. First liquidity / first observation of a token IS the
        // activation trigger (REV-020), so this is where the pipeline belongs.
        //
        // A failure here must not abort discovery — losing an ingest pass is worse
        // than missing one resolution — but it is logged, never swallowed silently.
        if let Err(e) = resolve_recent_for_token(pool, workspace, chain, mint, observed_at).await {
            tracing::warn!(
                chain = chain.as_str(),
                mint,
                error = %e,
                "recent-intelligence resolution failed for a newly discovered token"
            );
        }
    }
    Ok(count)
}

/// One promoted-or-not funding edge owned by a token (REV-053-F01).
pub(crate) struct TokenFundingEdge {
    pub from_address: String,
    pub to_address: String,
    pub edge_kind: String,
    pub block_time: DateTime<Utc>,
    /// Cluster-membership promotion (`confidence >= MEMBERSHIP_THRESHOLD`).
    ///
    /// Carried rather than filtered on, because promotion answers "are these two
    /// wallets one actor?", not "is this edge usable evidence?". It decides the
    /// TRUTH STATUS handed to the resolver, which is what gates `Exact`.
    pub promoted: bool,
}

/// Funding edges OWNED BY one token, newest first (REV-050-F01, REV-053-F01).
///
/// A separate function so the ownership predicate is testable against a live
/// database: the original bug was a missing predicate, and a missing predicate is
/// only provable by seeding two tokens and asserting one cannot see the other's edge.
///
/// REV-053-F01: `promoted = true` is NO LONGER a filter here, and removing it is the
/// fix rather than a loosening.
///
/// `promoted` is cluster-membership semantics: `graph.rs` states "One transfer NEVER
/// merges wallets" and requires accumulated confidence >= 0.70 across INDEPENDENT
/// evidence kinds. The only exercised production writer,
/// `graph::update_funding_edges()`, records a single funding component worth 0.35 —
/// so by construction it can never emit a promoted row. Filtering on promotion
/// therefore made the production path unreachable: the reviewer's real webhook probe
/// wrote `confidence=0.3500 promoted=false evidence.mint=<mint>`, the reader returned
/// zero rows, and `token discover --once` produced `Recent before=0 / after=0`.
///
/// Raising the writer's score to clear 0.70 would mean inventing evidence components
/// that were never observed, which is the opposite of the fail-closed rule. So the
/// edge is READ, and its weakness is carried in `promoted` — an unpromoted edge is
/// handed to the resolver as non-authoritative and can never produce an `Exact`
/// relation (see `resolve_recent_for_token`).
///
/// Still excluded, and still fail-closed:
///   * an edge with no `evidence->>'mint'` belongs to NO token (native funding, or a
///     bare evidence row), so it is never attributed to one;
///   * `block_time IS NULL` cannot satisfy the resolver's validity-window check, and
///     decoding NULL into `DateTime<Utc>` would fail the whole pass.
pub(crate) async fn token_owned_funding_edges(
    pool: &PgPool,
    chain: ChainKind,
    mint: &str,
) -> Result<Vec<TokenFundingEdge>> {
    let rows: Vec<(String, String, String, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT from_address, to_address, edge_kind, block_time, promoted \
           FROM funding_edges \
          WHERE chain = $1 \
            AND evidence ->> 'mint' = $2 \
            AND block_time IS NOT NULL \
          ORDER BY block_time DESC \
          LIMIT 200",
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_all(pool)
    .await
    .context("failed to read funding edges for recent resolution")?;

    Ok(rows
        .into_iter()
        .map(
            |(from_address, to_address, edge_kind, block_time, promoted)| TokenFundingEdge {
                from_address,
                to_address,
                edge_kind,
                block_time,
                promoted,
            },
        )
        .collect())
}

/// The `from_address` of the token's EARLIEST funding edge, or `None`.
///
/// REV-062-F04: the initial funder used to be derived in Rust from the graph
/// evidence page, which is the NEWEST 200 edges (`ORDER BY block_time DESC LIMIT
/// 200`). For a token with 201+ edges the earliest funder is outside that page, so
/// the worker named the 2nd-earliest funder as the initial one — a false claim
/// about an authoritative actor. It is queried directly instead.
///
/// REV-064-F04: `ORDER BY block_time, signature` was NOT a total order. The legacy
/// `funding_edges` primary key is
/// `(chain, from_address, to_address, signature, edge_kind)`, so two rows can share
/// `block_time` AND `signature` while differing in the remaining PK columns; the
/// winner then depended on physical/insertion order, and the "authoritative" actor
/// changed between two fixtures holding identical data. The ORDER BY now covers
/// every PK column not already pinned by the WHERE, which IS a total order — one
/// input can only ever yield one answer.
///
/// It is a named function, not an inline query, so the regression can assert on the
/// production statement instead of on a copy of it.
pub(crate) async fn earliest_token_funder(
    pool: &PgPool,
    chain: ChainKind,
    mint: &str,
) -> Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT from_address FROM funding_edges \
          WHERE chain = $1 \
            AND evidence ->> 'mint' = $2 \
            AND block_time IS NOT NULL \
            AND (edge_kind = 'funding' OR edge_kind = 'token_funding') \
          ORDER BY block_time ASC, signature ASC, from_address ASC, to_address ASC, \
                   edge_kind ASC \
          LIMIT 1",
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_optional(pool)
    .await
    .context("failed to read the token's earliest funding edge")
}

/// Resolve and persist Recent-intelligence relations for one token (REV-046-A4).
///
/// The graph input comes from the authoritative store (`funding_edges`), never from
/// the provider payload: a provider could otherwise assert a relation directly. The
/// workspace is bound from the job context by the caller.
///
/// REV-050-F01: the funding evidence is bound to THE TOKEN BEING RESOLVED, by
/// `token_owned_funding_edges` — see that function for the ownership argument.
///
/// REV-053-F02: the pipeline runs on EVERY triggered token, including one with no
/// usable evidence at all.
///
/// The previous version returned early on an empty edge set, which made the
/// partial-coverage disclosure unreachable exactly when it matters most: a token with
/// no deployer/authority/funder evidence appended nothing, the API returned an empty
/// array, and the dashboard rendered "No recent events" — readable as "no reuse".
/// REV-048 forbids precisely that reading, so an empty evidence set is now a
/// DISCLOSED state rather than a silent return.
///
/// Nothing is fabricated by running the pipeline on an empty graph: with no edges the
/// resolver emits no candidates, so the only row appended is the coverage disclosure,
/// which asserts `relation: NULL` and names the inputs that were missing.
pub(crate) async fn resolve_recent_for_token(
    pool: &PgPool,
    workspace: solana_whale_intelligence::sf::recent_pipeline::WorkspaceScope,
    chain: ChainKind,
    mint: &str,
    observed_at: DateTime<Utc>,
) -> Result<()> {
    use solana_whale_intelligence::sf::graph::{EntityEdge, EntityNode, NodeType};
    use solana_whale_intelligence::sf::recent::{ActivationTrigger, ActorExtraction};

    let token_key = format!("{}:{}", chain.as_str(), mint);

    // Token-owned funding evidence from our own graph. May legitimately be empty.
    let rows = token_owned_funding_edges(pool, chain, mint).await?;

    let mut nodes: Vec<EntityNode> = vec![EntityNode {
        entity_key: token_key.clone(),
        node_type: NodeType::Token,
    }];
    let mut edges: Vec<EntityEdge> = Vec::new();
    for row in &rows {
        let from_key = format!("{}:{}", chain.as_str(), row.from_address);
        let to_key = format!("{}:{}", chain.as_str(), row.to_address);
        nodes.push(EntityNode {
            entity_key: to_key.clone(),
            node_type: NodeType::Wallet,
        });
        edges.push(EntityEdge {
            from_entity_key: from_key,
            to_entity_key: to_key,
            edge_type: match row.edge_kind.as_str() {
                "deploy" => solana_whale_intelligence::sf::graph::EdgeType::DeployedBy,
                _ => solana_whale_intelligence::sf::graph::EdgeType::FundedBy,
            },
            occurred_at: row.block_time.to_rfc3339(),
            source_id: None,
            // REV-053-F01: promotion decides TRUTH STATUS, not visibility.
            //
            // `Confirmed` was previously hardcoded for every row, which would now be a
            // real defect: since promotion is no longer a filter, a single 0.35 funding
            // observation would arrive labelled as confirmed truth and could reach
            // `Exact` through `edge_is_authoritative`. An unpromoted edge is evidence
            // that two wallets interacted, NOT evidence that they are one actor, so it
            // is handed over as `Unknown` and the resolver's authority test rejects it.
            truth_status: if row.promoted {
                solana_whale_intelligence::sf::core::TruthStatus::Confirmed
            } else {
                solana_whale_intelligence::sf::core::TruthStatus::Unknown
            },
            confidence: None,
            valid_from: row.block_time.to_rfc3339(),
            valid_until: None,
            supersedes: None,
            // Evidence reference: the graph edge itself, with its promotion state, so a
            // reader can tell corroborated membership from a single observation.
            evidence_refs: vec![format!(
                "funding_edge:{}:{}:{}",
                chain.as_str(),
                row.edge_kind,
                if row.promoted { "promoted" } else { "unpromoted" }
            )],
        });
    }

    // REV-060-F03: the worker previously hardcoded every actor input to `None`, so
    // degraded coverage was the only reachable state from the real producer — the
    // degraded->full transition only ever passed in a synthetic test. At least one
    // authoritative actor is derivable from OUR OWN store, not from provider
    // payload: the initial funder is the `from_address` of the token's earliest
    // funding edge (`funding_edges` is keyed on `evidence->>'mint'`, so it is bound
    // to THE TOKEN being resolved). Wiring it means the worker's coverage can move
    // from "all three unknown" to "only deployer + authority unknown" — a genuinely
    // disclosed partial state, not a fabricated one.
    //
    // `deployer` and `authority` remain `None`: the producer has no authoritative
    // source for them, and the resolver is fail-closed on `None` (it produces no
    // relation rather than a guessed one). Full coverage stays honestly unreachable,
    // which is the correct disclosure — we do not invent an actor to make the number
    // go green.
    let initial_funder = earliest_token_funder(pool, chain, mint)
        .await?
        .map(|from| format!("{}:{}", chain.as_str(), from));

    let anchor = ActorExtraction {
        token: token_key,
        deployer: None,
        authority: None,
        fee_payer: None,
        factory: None,
        initial_funder,
        authority_changes: vec![],
        social_identities: vec![],
    };

    let outcome = solana_whale_intelligence::sf::recent_pipeline::run_for_anchor(
        pool,
        workspace,
        solana_whale_intelligence::sf::token::TokenLifecycle::Active,
        Some(ActivationTrigger::FirstLiquidity),
        &anchor,
        &nodes,
        &edges,
        observed_at,
    )
    .await?;

    // Logged whenever the pipeline did anything observable, INCLUDING a disclosure:
    // "we published the limits of what we know" is exactly the event an operator needs
    // to see, and it was previously invisible.
    if outcome.appended > 0 || outcome.duplicates > 0 || outcome.disclosed_partial_coverage {
        tracing::info!(
            resolved = outcome.resolved,
            appended = outcome.appended,
            duplicates = outcome.duplicates,
            disclosed_partial_coverage = outcome.disclosed_partial_coverage,
            evidence_edges = rows.len(),
            "recent-intelligence pass persisted"
        );
    }
    Ok(())
}

/// Compute and store a wallet score from trades in the database.
///
/// FIFO-matches the wallet's trades, aggregates skill/copyability inputs, and
/// persists the score row idempotently for `as_of`.
///
/// REV-067-F06: `workspace_id` is REQUIRED and the disposition decides eligibility.
/// `models::Disposition` declares that only `score` contributes alpha — `watch` is
/// "no scoring contribution yet", `flow_only` is "excluded from alpha", `skip` is
/// excluded outright — and this boundary enforced none of it: every wallet was
/// scored and its row persisted regardless of policy. `Ok(None)` means "policy
/// forbids scoring", which a caller can distinguish from a computed score; an
/// unreadable or unknown policy is an ERROR, never permission.
pub async fn score_wallet(
    pool: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    address: &str,
    as_of: DateTime<Utc>,
    config: &crate::config::ScoringConfig,
) -> Result<Option<crate::scoring::WalletScoreResult>> {
    if !crate::filter::effective_disposition(pool, workspace_id, chain, address)
        .await?
        .allows_scoring()
    {
        return Ok(None);
    }
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
    Ok(Some(result))
}

/// Evaluate one token's entry/exit signals from current evidence.
///
/// Reads token lifecycle, latest market snapshot, cluster buys, and wallet
/// eligibility; writes one `signal_evaluations` row per gate outcome.
///
/// REV-067-F06: `workspace_id` is REQUIRED because wallet ELIGIBILITY is a policy
/// question. The eligibility query took the best `wallet_scores` row among the
/// token's buyers with no disposition filter at all, so a `skip`, `watch`, or
/// `flow_only` wallet still decided whether a signal fired — the contract says only
/// `score` contributes alpha. The filter now runs in SQL against the same
/// workspace-scoped, expiry-aware label predicate every policy read uses, so an
/// excluded wallet cannot reach the gate.
/// REV-084-F06: crate-internal only — production callers must use the fenced path.
pub(crate) async fn evaluate_token_signals(
    pool: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &crate::config::SignalsConfig,
) -> Result<Option<i64>> {
    evaluate_token_signals_impl(pool, workspace_id, chain, mint, now, config, None).await
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_token_signals_impl(
    pool: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &crate::config::SignalsConfig,
    claim_token: Option<&str>,
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

    // REV-069-F06: EVERY alpha input is computed over the SAME policy-eligible buyer
    // set, resolved once through the shared disposition authority.
    //
    // REV-068 filtered only `WalletEligibility`. The cluster count and the
    // meaningful-buy count were still computed over ALL buyers, and both are hard
    // entry gates — so two `skip` wallets in two clusters flipped a rejection into an
    // accepted signal while the contract said only `score` contributes alpha.
    // Filtering after the count is not filtering: the set must be decided first.
    let eligible = eligible_signal_buyers(pool, workspace_id, chain, mint).await?;

    // REV-072-F06 (HIGH): the gate asks for two INDEPENDENT clusters, so it counts
    // distinct clusters over DISTINCT WALLETS' canonical memberships.
    //
    // `COUNT(DISTINCT m.cluster_id)` over raw memberships let ONE wallet supply two
    // clusters: the schema permitted several active memberships per
    // `(chain, address)` and `rebuild_cluster_for` never merged the overlaps, so a
    // single eligible buyer in two active clusters passed a two-cluster gate alone
    // and a rejected token became an accepted signal.
    //
    // Two defences, deliberately both: migration 1032 merges the overlaps and
    // installs a partial unique index so the state cannot recur, and this query
    // takes one canonical membership per wallet (`DISTINCT ON`, smallest cluster id)
    // so a pre-upgrade database — or a future writer that reintroduces the
    // duplication — still cannot inflate the count.
    let cluster_buys: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT canonical.cluster_id)
          FROM (
            SELECT DISTINCT ON (m.address) m.address, m.cluster_id
              FROM wallet_cluster_members m
              JOIN trades t ON t.chain = m.chain AND t.wallet = m.address
             WHERE m.chain = $1 AND t.mint = $2 AND t.side = 'buy' AND m.revoked_at IS NULL
               AND m.address = ANY($3)
             ORDER BY m.address, m.cluster_id
          ) canonical
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(&eligible)
    .fetch_one(pool)
    .await?;
    let meaningful_buys: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM trades
         WHERE chain = $1 AND mint = $2 AND side = 'buy' AND COALESCE(usd_value, 0) >= 1
           AND wallet = ANY($3)
        "#,
    )
    .bind(chain.as_str())
    .bind(mint)
    .bind(&eligible)
    .fetch_one(pool)
    .await?;

    let wallet_row = best_score_among(pool, chain, &eligible).await?;
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
    match claim_token {
        Some(ct) => {
            crate::signals::evaluate_token_fenced(
                pool,
                workspace_id,
                chain,
                mint,
                now,
                config,
                &token,
                &market,
                &clusters,
                &wallets,
                None,
                ct,
            )
            .await
        }
        None => {
            crate::signals::evaluate_token(
                pool,
                workspace_id,
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
    }
}

/// Fenced entry evaluation for the scheduled worker loop (REV-080-F04).
///
/// Identical inputs to [`evaluate_token_signals`], plus the caller's per-claim
/// token. The compute path is shared; only the WRITE path differs —
/// [`crate::signals::evaluate_token_fenced`] verifies the claim inside the write
/// transaction, so a stale worker whose lease expired after a reclaim writes
/// ZERO rows instead of duplicate logical signals.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate_token_signals_fenced(
    pool: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
    now: DateTime<Utc>,
    config: &crate::config::SignalsConfig,
    claim_token: &str,
) -> Result<Option<i64>> {
    evaluate_token_signals_impl(
        pool, workspace_id, chain, mint, now, config, Some(claim_token),
    )
    .await
}

/// The token's buyers that POLICY allows to contribute alpha.
///
/// One set, resolved once, used for every alpha input (cluster count, meaningful-buy
/// count, wallet score). REV-069-F06: filtering only the score lookup while counting
/// clusters and buys over ALL buyers meant excluded wallets still supplied two hard
/// entry gates — two `skip` wallets in two clusters flipped a rejection into an
/// accepted signal. The set has to be decided BEFORE anything is counted.
///
/// Each buyer is resolved through `db::active_disposition`, the same authority every
/// other policy read uses (workspace-scoped, unexpired, unrevoked, manual-first,
/// most-restrictive-wins). Only `Disposition::Score` contributes, per `models.rs`.
///
/// Fail-closed: an unknown disposition or a store failure is an `Err`, never an
/// empty set. An empty set would be written as a normal rejection row — a policy
/// answer the evaluation is not entitled to give when it could not read the policy.
pub async fn eligible_signal_buyers(
    pool: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    mint: &str,
) -> Result<Vec<String>> {
    let buyers: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT wallet FROM trades \
          WHERE chain = $1 AND mint = $2 AND side = 'buy'",
    )
    .bind(chain.as_str())
    .bind(mint)
    .fetch_all(pool)
    .await?;

    let mut eligible = Vec::new();
    for buyer in buyers {
        // `effective_disposition` propagates both an unreadable store and a value
        // outside the frozen vocabulary; neither may be silently treated as "not
        // eligible", because that is indistinguishable from a real policy answer.
        if crate::filter::effective_disposition(pool, workspace_id, chain, &buyer)
            .await?
            .allows_scoring()
        {
            eligible.push(buyer);
        }
    }
    Ok(eligible)
}

/// The highest-`as_of` stored score among an already policy-filtered wallet set.
///
/// Takes the set rather than re-deriving it, so the score lookup cannot disagree
/// with the counts about who is eligible (REV-069-F06).
async fn best_score_among(
    pool: &PgPool,
    chain: ChainKind,
    eligible: &[String],
) -> Result<Option<(i32, i32, Decimal)>> {
    if eligible.is_empty() {
        return Ok(None);
    }
    let row = sqlx::query_as(
        "SELECT skill_score, copyability_score, history_completeness \
           FROM wallet_scores \
          WHERE chain = $1 AND address = ANY($2) \
          ORDER BY as_of DESC \
          LIMIT 1",
    )
    .bind(chain.as_str())
    .bind(eligible)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Signals awaiting an alert, for ONE workspace.
///
/// Selection over the outbox: a signal is due when NO alert row exists for its
/// (signal, destination) identity yet, or its row is `pending`, due, and not
/// under an unexpired claim. `sent`/`dead` rows never reselect.
pub async fn pending_signal_alerts(
    pool: &PgPool,
    workspace_id: i64,
    destination: &str,
) -> Result<Vec<(i64, String, String, String, i32, chrono::DateTime<Utc>)>> {
    // REV-076-F04: the exclusion is per (signal, DESTINATION). Filtering on
    // signal_id alone made a `sent` row for one chat suppress delivery to every
    // other destination — the same swallow, one level down.
    Ok(sqlx::query_as(
        r#"
        SELECT s.id, s.chain, s.mint, s.signal_kind, s.score, s.created_at
          FROM signals s
         WHERE s.workspace_id = $1
           AND s.status = 'active'
           AND NOT EXISTS (
               SELECT 1 FROM alerts a
                WHERE a.signal_id = s.id
                  AND a.destination = $2
                  AND (a.state IN ('sent', 'dead')
                       OR (a.state = 'pending' AND a.next_attempt_at > now())
                       OR (a.state = 'pending' AND a.claim_expires_at > now()))
           )
         ORDER BY s.created_at ASC
         LIMIT 20
        "#,
    )
    .bind(workspace_id)
    .bind(destination)
    .fetch_all(pool)
    .await?)
}

/// Funding-radar alerts awaiting (re)delivery, for ONE workspace/destination.
///
/// REV-078-F03: the REV-077 funding path claimed against the production producer
/// but had NO drain: a funding alert is emitted only during the Funded→Preparation
/// transition, so a transient failure meant the case never offered the alert
/// again, and the signal-only selection never saw it. The drain is driven by the
/// outbox row itself, not by producer state transitions.
/// REV-082-F04 (HIGH): the recovery threshold is the CONFIGURED policy, not a
/// hardcoded 70. `min_confidence` comes from
/// `FundingRadarConfig.preparation_alert_confidence` — a config below 70 used to
/// lose intended alerts; a config above 70 sent alerts the producer suppressed.
pub async fn pending_funding_alerts(
    pool: &PgPool,
    workspace_id: i64,
    destination: &str,
    min_confidence: u32,
) -> Result<Vec<(i64, String)>> {
    // REV-080-F02 (CRITICAL): the selection is workspace-scoped on the row's OWN
    // ownership column. The REV-079 drain filtered by destination only, then
    // claimed with the CURRENT worker's workspace — because the dedup key embeds
    // the workspace, worker A created a second row under A for workspace B's case
    // and left B's row pending: a cross-tenant duplicate delivery. The claim must
    // name the row's owner, not the selector's context.
    Ok(sqlx::query_as(
        r#"
        SELECT x.funding_case_id, x.text
          FROM (
            -- Outbox-due rows (retry of a failed delivery).
            SELECT a.funding_case_id,
                   c.chain || ' wallet ' || c.recipient ||
                   ' confidence ' || c.confidence ||
                   ': possible project preparation' AS text,
                   a.dedup_key AS ord
              FROM alerts a
              JOIN funding_radar_cases c ON c.id = a.funding_case_id AND c.workspace_id = a.workspace_id
             WHERE a.subject_kind = 'funding'
               AND a.workspace_id = $1
               AND a.destination = $2
               AND a.state = 'pending'
               AND a.next_attempt_at <= now()
               AND (a.claim_expires_at IS NULL OR a.claim_expires_at <= now())
            UNION ALL
            -- REV-080-F05 (HIGH): derive the intent from DURABLE CASE STATE. The
            -- radar transition to `preparation` persists inside evaluate_radar_case
            -- BEFORE the producer claims the outbox row; a failed claim used to
            -- lose the alert forever (the transition never re-emits). A
            -- preparation-stage case with alert confidence and NO outbox row for
            -- this destination IS the due intent, recovered independently of the
            -- producer path that lost it.
            SELECT c.id, c.chain || ' wallet ' || c.recipient ||
                   ' confidence ' || c.confidence ||
                   ': possible project preparation',
                   'derived:' || c.id::text
              FROM funding_radar_cases c
             WHERE c.workspace_id = $1
               AND c.stage = 'preparation'
               AND c.confidence >= $3
               AND NOT EXISTS (
                   SELECT 1 FROM alerts a
                    WHERE a.subject_kind = 'funding'
                      AND a.funding_case_id = c.id
                      AND a.workspace_id = $1
                      AND a.destination = $2
               )
          ) x
         ORDER BY x.ord ASC
         LIMIT 20
        "#,
    )
    .bind(workspace_id)
    .bind(destination)
    .bind(min_confidence as i32)
    .fetch_all(pool)
    .await?)
}

/// Dispatch pending signals as Telegram alerts (outbound Bot API only).
///
/// REV-072-F06: only signals OWNED by this worker's workspace are dispatched.
/// REV-074-F03/F04: outbox delivery, real chain, immutable identity.
/// REV-076-F03/F04: the destination (chat id) is part of the dedup identity and
/// stored on the row; the claim is one atomic statement with a fencing token and
/// a lease; completion is fenced by that token and must update exactly one row;
/// Telegram 2xx still requires `ok == true` in the body.
pub async fn dispatch_alerts(ctx: &WorkerContext) -> Result<u32> {
    dispatch_alerts_via(ctx, crate::alerts::TELEGRAM_BOT_API_BASE).await
}

/// The dispatcher with the Bot API base explicit (production: Telegram; tests:
/// a local mock — a retry that cannot be forced to fail-then-succeed cannot be
/// proven).
pub async fn dispatch_alerts_via(ctx: &WorkerContext, bot_api_base: &str) -> Result<u32> {
    let (Some(bot_token), Some(chat_id)) = (ctx.telegram_bot_token.as_ref(), ctx.telegram_chat_id.as_ref()) else {
        return Ok(0);
    };
    let workspace_id = ctx.workspace.id();
    let pending = pending_signal_alerts(&ctx.pool, workspace_id, chat_id).await?;

    // REV-078-F03: drain funding-radar retries first — they are pure outbox work
    // (the producer already emitted once; these are failures coming back).
    for (case_id, text) in pending_funding_alerts(
        &ctx.pool, workspace_id, chat_id, ctx.funding_config.preparation_alert_confidence,
    ).await? {
        let Some(claim) = crate::signals::claim_alert(
            &ctx.pool, "funding", workspace_id, case_id, chat_id,
        )
        .await? else {
            continue;
        };
        match crate::alerts::send_message(&ctx.http, bot_api_base, bot_token, chat_id, &text).await {
            Ok(()) => {
                // REV-084-F02: a false completion means our lease was lost
                // mid-send — the row belongs to a newer claimant. Make it
                // observable instead of silently treating it as delivered.
                if !crate::signals::mark_alert_sent(
                    &ctx.pool, "funding", workspace_id, case_id, chat_id, &claim.token,
                )
                .await?
                {
                    tracing::warn!(case_id, "funding alert completion matched no row (stale claim)");
                }
            }
            Err(failure) => {
                let (permanent, retry_after) = match failure {
                    crate::alerts::SendFailure::Permanent => (true, None),
                    crate::alerts::SendFailure::Transient => (false, None),
                    crate::alerts::SendFailure::RateLimited(secs) => (false, Some(secs)),
                };
                if !crate::signals::mark_alert_failed(
                    &ctx.pool, "funding", workspace_id, case_id, chat_id,
                    &claim.token, claim.attempt,
                    &format!("{failure:?}"), permanent, retry_after,
                )
                .await?
                {
                    tracing::warn!(case_id, "funding alert failure update matched no row (stale claim)");
                }
            }
        }
    }

    let mut sent = 0u32;
    for (signal_id, chain, mint, kind, score, created_at) in pending {
        let _ = created_at;
        // Atomic claim with fencing token + lease; a concurrent dispatcher that
        // loses skips the row entirely (REV-076-F03 fresh-claim race).
        let Some(claim) = crate::signals::claim_alert(
            &ctx.pool, "signal", workspace_id, signal_id, chat_id,
        )
        .await? else {
            continue;
        };
        let outcome: Result<(), crate::alerts::SendFailure> = (async {
            let chain_kind = crate::models::ChainKind::parse(&chain)
                .ok_or(crate::alerts::SendFailure::Permanent)?;
            let message = crate::alerts::compose_signal_alert(
                chain_kind.as_str(), &mint, &kind, score.max(0) as u32, "see report",
            );
            crate::alerts::send_message(&ctx.http, bot_api_base, bot_token, chat_id, &message.text).await
        })
        .await;
        match outcome {
            Ok(()) => {
                // Fenced completion; if our lease was lost mid-HTTP this records
                // nothing and returns false — the newer claimant owns the row.
                if crate::signals::mark_alert_sent(
                    &ctx.pool, "signal", workspace_id, signal_id, chat_id, &claim.token,
                )
                .await?
                {
                    sent += 1;
                }
            }
            Err(failure) => {
                let (permanent, retry_after) = match failure {
                    crate::alerts::SendFailure::Permanent => (true, None),
                    crate::alerts::SendFailure::Transient => (false, None),
                    crate::alerts::SendFailure::RateLimited(secs) => (false, Some(secs)),
                };
                // REV-087-F02 (HIGH): this was the ONLY one of nine alert-outbox
                // completion sites that discarded the boolean. `?` propagated SQL
                // errors but a `false` — our lease was lost mid-send and the row
                // belongs to a newer claimant — was silent. Same shape as the
                // funding drain above and the radar producer below.
                if !crate::signals::mark_alert_failed(
                    &ctx.pool, "signal", workspace_id, signal_id, chat_id,
                    &claim.token, claim.attempt,
                    &format!("{failure:?}"), permanent, retry_after,
                )
                .await?
                {
                    tracing::warn!(signal_id, "signal alert failure update matched no row (stale claim)");
                }
            }
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
           AND workspace_id = $1
         ORDER BY updated_at ASC
         LIMIT 100
        "#,
    )
    .bind(ctx.workspace.id())
    .fetch_all(&ctx.pool)
    .await?;
    let now = Utc::now();
    let mut evaluated = 0u32;
    for (_id, chain_str, recipient) in open {
        let chain = ChainKind::parse(&chain_str).unwrap_or(ChainKind::Solana);
        let decision = crate::funding_radar::evaluate_radar_case(
            &ctx.pool,
            ctx.workspace.id(),
            chain,
            &recipient,
            now,
            &ctx.funding_config,
        )
        .await?;
        if let Some(alert) = decision.alert {
            if let (Some(bot), Some(chat)) = (&ctx.telegram_bot_token, &ctx.telegram_chat_id) {
                // REV-076-F03: funding-radar alerts go through the SAME outbox as
                // signal alerts. The old direct send lost every transient failure
                // forever — the exact defect class the outbox exists to close.
                // REV-078-F03: claim errors propagate — the REV-077 path swallowed
                // them with `if let Ok(...)`, so the FK violation it shipped was
                // invisible. A failed claim must not look like a delivered alert.
                if let Some(claim) = crate::signals::claim_alert(
                    &ctx.pool, "funding", ctx.workspace.id(), alert.case_id, chat,
                )
                .await?
                {
                    let result = crate::alerts::send_message(
                        &ctx.http,
                        crate::alerts::TELEGRAM_BOT_API_BASE,
                        bot,
                        chat,
                        &alert.message,
                    )
                    .await;
                    match result {
                        Ok(()) => {
                            // REV-084-F02: completion errors propagate and a
                            // stale claim (false) is observable — the REV-083
                            // `let _ =` hid both SQL failures and lost claims.
                            if !crate::signals::mark_alert_sent(
                                &ctx.pool, "funding", ctx.workspace.id(), alert.case_id, chat, &claim.token,
                            )
                            .await?
                            {
                                tracing::warn!(case_id = alert.case_id, "funding alert completion matched no row (stale claim)");
                            }
                        }
                        Err(failure) => {
                            let (permanent, retry_after) = match failure {
                                crate::alerts::SendFailure::Permanent => (true, None),
                                crate::alerts::SendFailure::Transient => (false, None),
                                crate::alerts::SendFailure::RateLimited(secs) => (false, Some(secs)),
                            };
                            if !crate::signals::mark_alert_failed(
                                &ctx.pool, "funding", ctx.workspace.id(), alert.case_id, chat,
                                &claim.token, claim.attempt,
                                &format!("{failure:?}"), permanent, retry_after,
                            )
                            .await?
                            {
                                tracing::warn!(case_id = alert.case_id, "funding alert failure update matched no row (stale claim)");
                            }
                        }
                    }
                }
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
                            ctx.workspace.id(),
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

/// How often the signal-evaluation loop scans for due tokens (REV-074-F02).
pub const SIGNAL_EVAL_INTERVAL_SECONDS: u64 = 60;

/// Minimum seconds between evaluations of the SAME token.
///
/// A re-evaluation with unchanged evidence must not pile up duplicate rows: the
/// gates are deterministic over the same facts, so a fresh row is only meaningful
/// after new evidence could have arrived. The cursor is derived from the token's
/// LATEST evaluation, not stored separately — a cursor table would be a second
/// place truth lives, and they could disagree.
pub const SIGNAL_EVAL_REEVAL_SECONDS: i64 = 300;

/// One periodic pass of signal evaluation over due tokens (REV-074-F02).
///
/// Before this, NOTHING in `run` ever called `evaluate_token_signals`: a token
/// could accumulate evidence forever with no evaluation row, and the alert loop
/// had nothing to deliver unless an operator ran the CLI by hand.
///
/// Batch selection is bounded and deterministic: tokens whose latest evaluation is
/// older than the re-evaluation cadence (or that were never evaluated), oldest
/// first, per chain. Per-token failures are isolated — one bad token logs and the
/// batch continues, because a single poisoned mint must not stop every later
/// token. The workspace comes from the job context, never from a row, so another
/// tenant's policy is never used.
pub async fn evaluate_due_signals(ctx: &WorkerContext) -> Result<u32> {
    let workspace_id = ctx.workspace.id();
    let now = Utc::now();
    // Only tokens the entry gates COULD accept are worth a pass: anything older
    // than the entry window is a guaranteed rejection row, and on a busy database
    // those pile up forever and starve the batch (the 20 oldest stale tokens would
    // be re-rejected on every pass while a fresh token never gets evaluated).
    // Restricting to entry-supported, in-window tokens is also what keeps "due"
    // stable across workspaces: a token evaluated recently by THIS workspace is
    // excluded, independently of what other tenants did.
    let due: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT t.chain, t.mint
          FROM tokens t
         WHERE t.chain IN ('solana', 'robinhood')
           AND t.lifecycle_state IN ('new_creation', 'bonding_curve', 'near_graduation',
                                     'graduated', 'post_migration')
           AND t.first_seen_at > now() - make_interval(secs => $3)
           AND NOT EXISTS (
               SELECT 1 FROM signal_evaluations e
                WHERE e.workspace_id = $1
                  AND e.chain = t.chain
                  AND e.mint = t.mint
                  AND e.evaluated_at > now() - make_interval(secs => $2)
           )
         ORDER BY t.first_seen_at ASC NULLS LAST, t.mint ASC
         LIMIT 20
        "#,
    )
    .bind(workspace_id)
    .bind(SIGNAL_EVAL_REEVAL_SECONDS as f64)
    .bind((ctx.signals_config.entry_max_token_age_hours as f64) * 3600.0)
    .fetch_all(&ctx.pool)
    .await?;

    let mut evaluated = 0u32;
    for (chain_str, mint) in due {
        let Some(chain) = crate::models::ChainKind::parse(&chain_str) else {
            tracing::warn!(chain = %chain_str, mint = %mint, "skipping token with unparseable chain");
            continue;
        };
        // REV-076-F02: DURABLE claim before evaluating. Due selection alone was
        // read-only — two worker processes on one workspace could pick the same
        // token and evaluate with different timestamps, defeating the timestamp
        // based signal identity and writing duplicate logical signals. The claim
        // row's PRIMARY KEY is the arbiter: INSERT wins, the loser skips. The
        // claim is released on completion and expires on its own, so a crashed
        // worker wedges a token for at most the claim lease.
        // REV-080-F04 (HIGH): a UNIQUE token per claim acquisition, and the
        // evaluator's writes must present it. REV-078 fenced only the release;
        // a stale owner whose lease expired could still write signal/evaluation
        // rows after another worker reclaimed. The token is minted here and
        // threaded through so the write path can verify ownership+lease in the
        // same transaction as the rows it writes.
        let claimant = format!("worker-{}-{}", std::process::id(), uuid::Uuid::new_v4());
        let claimed: Option<i64> = sqlx::query_scalar(
            r#"
            INSERT INTO signal_eval_claims (workspace_id, chain, mint, claimed_by, expires_at)
            VALUES ($1, $2, $3, $4, now() + interval '10 minutes')
            ON CONFLICT (workspace_id, chain, mint) DO UPDATE
                SET claimed_by = $4, claimed_at = now(), expires_at = now() + interval '10 minutes'
                WHERE signal_eval_claims.expires_at <= now()
            RETURNING workspace_id
            "#,
        )
        .bind(workspace_id)
        .bind(&chain_str)
        .bind(&mint)
        .bind(&claimant)
        .fetch_optional(&ctx.pool)
        .await?;
        if claimed.is_none() {
            continue; // another live worker holds this token
        }
        let result = evaluate_token_signals_fenced(
            &ctx.pool,
            workspace_id,
            chain,
            &mint,
            now,
            &ctx.signals_config,
            &claimant,
        )
        .await;
        // REV-082-F07 (MEDIUM): the outer UNCONDITIONAL release was removed because
        // the fenced evaluator already releases the claim in the same transaction as
        // its writes. That is still true on the SUCCESS path.
        //
        // REV-087-F05 (MEDIUM): but a FAILED evaluation rolls that release back with
        // the rest of its transaction, so the token stayed wedged for the full
        // 10-minute lease, and `evaluated` counted the failure as work done. Release
        // on the error path only, FENCED by this claimant so a worker that already
        // reclaimed the token is never disturbed — this is not a reinstatement of the
        // unfenced release REV-082-F07 deleted. Per-token isolation is preserved: the
        // batch continues.
        match result {
            Ok(_) => evaluated += 1,
            Err(err) => {
                tracing::warn!(error = %err, chain = %chain_str, mint = %mint, "signal evaluation failed for one token");
                if let Err(release_err) = sqlx::query(
                    "DELETE FROM signal_eval_claims \
                      WHERE workspace_id = $1 AND chain = $2 AND mint = $3 AND claimed_by = $4",
                )
                .bind(workspace_id)
                .bind(&chain_str)
                .bind(&mint)
                .bind(&claimant)
                .execute(&ctx.pool)
                .await
                {
                    tracing::warn!(
                        error = %release_err, chain = %chain_str, mint = %mint,
                        "failed to release the evaluation claim after a failed evaluation"
                    );
                }
            }
        }
    }
    Ok(evaluated)
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

    // REV-074-F02: signal evaluation runs on a schedule like every other worker.
    // The queue gate respects backpressure exactly as radar/discovery do — a paused
    // `signal_eval` queue suppresses execution rather than letting evaluation pile
    // work onto a pressured system.
    let signal_ctx = ctx.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(SIGNAL_EVAL_INTERVAL_SECONDS));
        loop {
            interval.tick().await;
            if !signal_ctx.queue_allowed(crate::queues::QUEUE_SIGNAL_EVAL).await {
                continue;
            }
            if let Err(err) = evaluate_due_signals(&signal_ctx).await {
                tracing::warn!(error = %err, "signal evaluation pass failed");
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
                // Workspace comes from the worker context, i.e. the job context.
            if let Err(err) = discover_tokens_once(
                &discover_ctx.pool,
                &gmgn,
                ChainKind::Solana,
                discover_ctx.workspace,
            )
            .await
            {
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
    // REV-072-F06: same defect class as the entry gate — `clusters_selling >= 2` is a
    // trigger, so one wallet with two active memberships could fire an exit signal by
    // itself. One canonical membership per wallet.
    let clusters_selling: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(DISTINCT canonical.cluster_id)
          FROM (
            SELECT DISTINCT ON (m.address) m.address, m.cluster_id
              FROM wallet_cluster_members m
              JOIN trades t ON t.chain = m.chain AND t.wallet = m.address
             WHERE m.chain = $1 AND t.mint = $2 AND t.side = 'sell' AND m.revoked_at IS NULL
             ORDER BY m.address, m.cluster_id
          ) canonical
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
