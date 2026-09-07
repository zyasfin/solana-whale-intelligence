//! Wallet filtering and bot classification.
//!
//! Automatic dispositions are reversible annotations; manual blocks remain
//! authoritative. Only high-confidence MEV/Dex execution bots auto-skip after
//! sufficient evidence, and their direct funding edges are always retained.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use crate::models::{ChainKind, Disposition, WalletLabelKind};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Bot classification decision.
#[derive(Clone, Debug, PartialEq)]
pub struct BotClassification {
    pub kind: Option<WalletLabelKind>,
    pub likelihood: f64,
    pub swap_count: u32,
    pub auto_skip: bool,
    /// Labels that remain visible annotations but never auto-skip.
    pub annotations: Vec<WalletLabelKind>,
}

/// Minimum swaps before any automatic label.
pub const AUTO_LABEL_MIN_SWAPS: u32 = 100;
/// Minimum likelihood for an automatic skip.
pub const AUTO_SKIP_LIKELIHOOD: f64 = 0.90;

/// Classify a wallet's bot status from GMGN labels and local evidence.
///
/// Only `mev_bot`/high-confidence `dex_bot` can auto-skip. `bundler`,
/// `sniper`, `rat_trader`, `arbitrage`, `copy_trader`, and `market_maker`
/// remain annotations.
pub fn classify_bot(
    labels: &[WalletLabelKind],
    likelihood: f64,
    swap_count: u32,
) -> BotClassification {
    let has_mev = labels.contains(&WalletLabelKind::MevBot);
    let has_dex = labels.contains(&WalletLabelKind::DexBot);
    let annotations: Vec<WalletLabelKind> = labels
        .iter()
        .copied()
        .filter(|l| {
            matches!(
                l,
                WalletLabelKind::Sniper
                    | WalletLabelKind::Bundler
                    | WalletLabelKind::RatTrader
                    | WalletLabelKind::Arbitrage
                    | WalletLabelKind::CopyTrader
                    | WalletLabelKind::MarketMaker
            )
        })
        .collect();

    let kind = if has_mev {
        Some(WalletLabelKind::MevBot)
    } else if has_dex {
        Some(WalletLabelKind::DexBot)
    } else {
        None
    };

    // Auto-skip requires sufficient swaps AND high likelihood AND an
    // execution-bot label. Everything else is annotation only.
    let auto_skip = kind.is_some()
        && likelihood >= AUTO_SKIP_LIKELIHOOD
        && swap_count >= AUTO_LABEL_MIN_SWAPS;

    BotClassification {
        kind,
        likelihood,
        swap_count,
        auto_skip,
        annotations,
    }
}

/// Apply an automatic disposition when no manual label is authoritative.
///
/// Manual labels always win. Automatic labels are recorded with
/// `manual = false` so they can be revoked later.
///
/// REV-056-F01: labels are workspace-owned, so both the manual-authority lookup and
/// the automatic write are scoped. Reading another tenant's manual block here would let
/// their classification silently suppress this workspace's research, and writing
/// without an owner would recreate the unowned rows the finding is about.
pub async fn apply_automatic_disposition(
    db: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    address: &str,
    classification: &BotClassification,
    observed_at: DateTime<Utc>,
) -> Result<Disposition> {
    // Manual authority: any active manual label for this wallet IN THIS WORKSPACE
    // wins. REV-062-F06: this used to be its own `SELECT ... LIMIT 1` over manual
    // labels, which returned an ARBITRARY row when a wallet held several manual
    // labels (`watch` + `skip`). It now calls `manual_disposition`, the same
    // manual-first + restrictiveness-ranking helper every other policy read uses,
    // so a manual `skip` always beats an arbitrary `watch` and the result is
    // caller-independent.
    let manual = crate::db::manual_disposition(db, workspace_id, chain.as_str(), address).await?;
    if let Some(manual_disposition) = manual {
        if let Some(disposition) = Disposition::parse(&manual_disposition) {
            return Ok(disposition);
        }
    }

    if !classification.auto_skip {
        return Ok(Disposition::Score);
    }

    let kind = classification.kind.expect("auto_skip implies kind");
    crate::db::add_wallet_label(
        db,
        workspace_id,
        chain.as_str(),
        address,
        kind.as_str(),
        "skip",
        &format!(
            "automatic {} classification likelihood {:.2} after {} swaps",
            kind.as_str(),
            classification.likelihood,
            classification.swap_count
        ),
        "auto",
        (classification.likelihood * 100.0) as i32,
        false,
        None,
    )
    .await?;
    tracing::info!(
        chain = chain.as_str(),
        address = %crate::models::short_addr(address),
        kind = kind.as_str(),
        "automatic reversible skip applied; manual blocks remain authoritative"
    );
    let _ = observed_at;
    Ok(Disposition::Skip)
}

/// The authoritative effective disposition for a wallet, fail-closed.
///
/// REV-067-F06: `scoring_disposition(Option<&str>)` was a pure mapper that turned an
/// UNKNOWN value into `Watch`. Every other policy reader (`db::active_disposition`,
/// `db::restrictiveness_rank`, `graph::trace_wallet`) treats an unrecognised
/// disposition as an ERROR, because a label that stopped meaning the vocabulary is a
/// schema bug and silently downgrading it to `Watch` is a policy decision made by a
/// typo. One contract, one behaviour.
///
/// A store failure is likewise an error, never `Watch`: "we could not read the
/// policy" and "the policy is watch" are different facts and only one of them
/// permits scoring.
///
/// `None` (no active label) legitimately means `Watch`: nothing has classified this
/// wallet, so it is monitored but contributes no alpha.
pub async fn effective_disposition(
    db: &PgPool,
    workspace_id: i64,
    chain: ChainKind,
    address: &str,
) -> Result<Disposition> {
    match crate::db::active_disposition(db, workspace_id, chain.as_str(), address).await? {
        Some(value) => Disposition::parse(&value).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown wallet disposition '{value}'; the label vocabulary is \
                 [skip, flow_only, watch, score]"
            )
        }),
        None => Ok(Disposition::Watch),
    }
}

impl Disposition {
    /// May this wallet's history be deep-synced?
    ///
    /// `models::Disposition` declares `skip` = "excluded from deep-sync/scoring".
    /// The contract was documented and never enforced (REV-067-F06).
    pub fn allows_deep_sync(self) -> bool {
        !matches!(self, Disposition::Skip)
    }

    /// May this wallet be scored / contribute alpha?
    ///
    /// Only `score` does. `watch` is explicitly "no scoring contribution yet",
    /// `flow_only` is "graph edges only, excluded from alpha", `skip` is excluded
    /// outright.
    pub fn allows_scoring(self) -> bool {
        matches!(self, Disposition::Score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mev_bot_auto_skips_with_evidence() {
        let labels = vec![WalletLabelKind::MevBot];
        let class = classify_bot(&labels, 0.95, 150);
        assert!(class.auto_skip);
        assert_eq!(class.kind, Some(WalletLabelKind::MevBot));
    }

    #[test]
    fn dex_bot_auto_skips_with_evidence() {
        let labels = vec![WalletLabelKind::DexBot];
        let class = classify_bot(&labels, 0.92, 120);
        assert!(class.auto_skip);
    }

    #[test]
    fn insufficient_swaps_prevents_auto_skip() {
        let labels = vec![WalletLabelKind::MevBot];
        let class = classify_bot(&labels, 0.99, 50);
        assert!(!class.auto_skip, "fewer than 100 swaps cannot auto-skip");
    }

    #[test]
    fn low_likelihood_prevents_auto_skip() {
        let labels = vec![WalletLabelKind::DexBot];
        let class = classify_bot(&labels, 0.80, 500);
        assert!(!class.auto_skip, "likelihood below 0.90 cannot auto-skip");
    }

    #[test]
    fn annotations_never_auto_skip() {
        for label in [
            WalletLabelKind::Bundler,
            WalletLabelKind::Sniper,
            WalletLabelKind::RatTrader,
            WalletLabelKind::Arbitrage,
            WalletLabelKind::CopyTrader,
            WalletLabelKind::MarketMaker,
        ] {
            let class = classify_bot(&[label], 0.99, 500);
            assert!(!class.auto_skip, "{:?} must not auto-skip from label alone", label);
            assert!(class.annotations.contains(&label));
        }
    }

    // REV-067-F06: the contract in `models.rs` names four dispositions and what each
    // one permits. It was documentation only until the boundaries enforced it, so it
    // is pinned here: `score` is the only one that may contribute alpha, and only
    // `skip` is excluded from deep-sync.
    #[test]
    fn the_disposition_contract_decides_deep_sync_and_scoring() {
        assert!(Disposition::Score.allows_deep_sync() && Disposition::Score.allows_scoring());
        assert!(Disposition::Watch.allows_deep_sync() && !Disposition::Watch.allows_scoring());
        assert!(Disposition::FlowOnly.allows_deep_sync() && !Disposition::FlowOnly.allows_scoring());
        assert!(!Disposition::Skip.allows_deep_sync() && !Disposition::Skip.allows_scoring());
    }
}
