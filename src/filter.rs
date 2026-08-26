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
pub async fn apply_automatic_disposition(
    db: &PgPool,
    chain: ChainKind,
    address: &str,
    classification: &BotClassification,
    observed_at: DateTime<Utc>,
) -> Result<Disposition> {
    // Manual authority: any active manual label for this wallet wins.
    let manual: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT disposition FROM wallet_labels
         WHERE chain = $1 AND address = $2 AND manual = true
           AND revoked_at IS NULL
           AND (expires_at IS NULL OR expires_at > now())
         LIMIT 1
        "#,
    )
    .bind(chain.as_str())
    .bind(address)
    .fetch_optional(db)
    .await?;
    if let Some((manual_disposition,)) = manual {
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

/// The effective disposition for scoring inclusion.
///
/// `skip` wallets never contribute alpha; their funding edges are retained by
/// the graph module regardless of this decision.
pub fn scoring_disposition(active: Option<&str>) -> Disposition {
    match active {
        Some(value) => Disposition::parse(value).unwrap_or(Disposition::Watch),
        None => Disposition::Watch,
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

    #[test]
    fn unknown_wallet_defaults_to_watch() {
        assert_eq!(scoring_disposition(None), Disposition::Watch);
        assert_eq!(scoring_disposition(Some("skip")), Disposition::Skip);
        assert_eq!(scoring_disposition(Some("flow_only")), Disposition::FlowOnly);
        assert_eq!(scoring_disposition(Some("score")), Disposition::Score);
        assert_eq!(scoring_disposition(Some("garbage")), Disposition::Watch);
    }
}
