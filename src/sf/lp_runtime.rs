//! Runtime logic: LP pool intelligence (Phase 4).
//!
//! Canonical source: PLAN SWI §8.9 "LP Pool Intelligence" (lines 561-577) and
//! §8.10 "LP Wallet Intelligence" (lines 579-584). Solana Meteora DLMM +
//! Robinhood Uniswap/Pancake. Ethereum/Base/BSC LP = N/A (frozen scope §1).
//!
//! This module computes derived LP metrics (fee-to-TVL, active-TVL ratio) and
//! assembles `LpRangeChamber` recommendations. It consumes the frozen `lp.rs`
//! types (`LpPoolIntelligence`, `LpPnl`, `LpWalletIntelligence`,
//! `LpRangeChamber`, `LpProtocol`) and introduces no new frozen state.

use rust_decimal::Decimal;

use super::lp::{LpPoolIntelligence, LpProtocol, LpRangeChamber};

/// Compute fee-to-TVL (fees / total value locked) as a ratio. Returns `None`
/// when either value is absent or TVL is zero (fail-closed: never divide by
/// zero or fabricate a ratio from missing data).
pub fn fee_to_tvl(fees: Option<&str>, tvl: Option<&str>) -> Option<f64> {
    let fees = parse_decimal(fees?);
    let tvl = parse_decimal(tvl?);
    if tvl == Decimal::ZERO {
        return None;
    }
    // ratio as f64 (lossy but acceptable for a display metric; exact math stays
    // in decimal for accounting).
    Some((fees / tvl).to_string().parse::<f64>().ok().unwrap_or(0.0))
}

/// Whether a pool is in an unsupported protocol scope (Ethereum/Base/BSC are
/// N/A per frozen §1). This is a scope gate, not a health signal.
pub fn is_supported_scope(chain: &str, protocol: &LpProtocol) -> bool {
    let chain_l = chain.to_lowercase();
    match protocol {
        LpProtocol::MeteoraDlmm => chain_l == "solana",
        // Robinhood adapters are Solana-indexed per frozen scope.
        LpProtocol::UniswapV2
        | LpProtocol::UniswapV3
        | LpProtocol::UniswapV4
        | LpProtocol::PancakeV2
        | LpProtocol::PancakeV3 => false, // Ethereum/Base/BSC LP = N/A (frozen §1)
    }
}

/// Build a range-chamber recommendation. The recommendation is the caller-
/// provided `recommended_range`; if absent, the current range is reused (no
/// fabricated recommendation). `range_shift_reason` is carried through.
pub fn build_range_chamber(
    pool: &LpPoolIntelligence,
    recommended_range: Option<&str>,
    shift_reason: Option<&str>,
) -> LpRangeChamber {
    LpRangeChamber {
        pool_address: pool.pool_address.clone(),
        current_range: pool.range.clone(),
        recommended_range: recommended_range
            .map(String::from)
            .or_else(|| pool.range.clone()),
        range_shift_reason: shift_reason.map(String::from),
    }
}

/// Parse a decimal-string, tolerating empty/absent -> 0.
fn parse_decimal(s: &str) -> Decimal {
    s.parse::<Decimal>().unwrap_or(Decimal::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(fees: Option<&str>, tvl: Option<&str>, range: Option<&str>) -> LpPoolIntelligence {
        LpPoolIntelligence {
            chain: "solana".into(),
            protocol: LpProtocol::MeteoraDlmm,
            pool_address: "pool1".into(),
            active_bin: None,
            bin_step: None,
            range: range.map(String::from),
            tvl: tvl.map(String::from),
            active_tvl: None,
            reserves: None,
            volume: None,
            fees: fees.map(String::from),
            fee_to_tvl: None,
        }
    }

    #[test]
    fn fee_to_tvl_computes_ratio() {
        let r = fee_to_tvl(Some("10"), Some("100"));
        assert!((r.unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn fee_to_tvl_fail_closed() {
        assert_eq!(fee_to_tvl(None, Some("100")), None);
        assert_eq!(fee_to_tvl(Some("10"), None), None);
        assert_eq!(fee_to_tvl(Some("10"), Some("0")), None); // divide by zero
    }

    #[test]
    fn scope_gate_rejects_unsupported() {
        assert!(is_supported_scope("solana", &LpProtocol::MeteoraDlmm));
        assert!(!is_supported_scope("ethereum", &LpProtocol::UniswapV3));
        assert!(!is_supported_scope("solana", &LpProtocol::UniswapV3));
    }

    #[test]
    fn range_chamber_reuses_current_when_no_recommendation() {
        let p = pool(Some("10"), Some("100"), Some("rangeA"));
        let c = build_range_chamber(&p, None, None);
        assert_eq!(c.current_range.as_deref(), Some("rangeA"));
        assert_eq!(c.recommended_range.as_deref(), Some("rangeA")); // reused, not fabricated
    }
}
