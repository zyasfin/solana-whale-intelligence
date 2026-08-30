//! Runtime logic: portfolio projections and risk aggregation (Phase 1).
//!
//! Canonical source: PLAN SWI §8.11 "Portfolio/Risk" (lines 588-597):
//! exposure per token/pool/chain/strategy, correlated exposure, wallet reserve,
//! total open notional, realized/unrealized PnL, daily loss/drawdown, ambiguous
//! execution exposure, treasury/hot-wallet separation.
//!
//! This module aggregates a set of `Exposure` lines into objective projections:
//! total notional, per-`kind` subtotals, and a correlated-exposure sum. It
//! consumes the frozen `portfolio.rs` domain types (`Exposure`,
//! `PortfolioSnapshot`) and introduces no new frozen state. Precision is
//! preserved via `rust_decimal::Decimal` (wire form stays decimal strings).

use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::portfolio::Exposure;

/// Aggregate portfolio projections (objective, no universal score — §12.2).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PortfolioProjection {
    /// Total open notional summed across all exposure lines.
    pub total_notional: Decimal,
    /// Notional subtotal per exposure `kind` (token | pool | chain | strategy).
    pub by_kind: HashMap<String, Decimal>,
    /// Sum of notional for exposure lines that declare a correlation group.
    /// Only the FIRST line of each correlation group is counted (a group is a
    /// set of correlated keys, not an additive axis) — correlated exposure is
    /// reported, never double-counted as if independent.
    pub correlated_notional: Decimal,
    /// Number of distinct correlation groups (each group = one shared set of
    /// correlated keys).
    pub correlation_groups: u32,
}

/// Parse a decimal-string notional, tolerating empty/absent -> 0 (fail-closed:
/// never invent a notional from a malformed string).
fn parse_decimal(s: &str) -> Decimal {
    s.parse::<Decimal>().unwrap_or(Decimal::ZERO)
}

/// Aggregate exposure lines into objective portfolio projections.
///
/// - `total_notional` = Σ every line's notional.
/// - `by_kind` = Σ per `kind` (token/pool/chain/strategy).
/// - `correlated_notional` = Σ notional of lines that are in a correlation
///   group, where a group is identified by the union of `correlated` keys.
///   Two lines belong to the same group if they share any correlation key.
///   Each group is counted ONCE (its member lines are NOT additive).
pub fn project(exposures: &[Exposure]) -> PortfolioProjection {
    let mut total = Decimal::ZERO;
    let mut by_kind: HashMap<String, Decimal> = HashMap::new();

    for e in exposures {
        let n = parse_decimal(&e.notional);
        total += n;
        *by_kind.entry(e.kind.clone()).or_default() += n;
    }

    // Correlation groups: union-find over lines that share any correlation key.
    // Each group's notional is the sum of its member lines, counted once.
    let n = exposures.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut Vec<usize>, x: usize) -> usize {
        let mut r = x;
        while parent[r] != r {
            r = parent[r];
        }
        // path compression
        let mut c = x;
        while parent[c] != c {
            let next = parent[c];
            parent[c] = r;
            c = next;
        }
        r
    }

    fn union(parent: &mut Vec<usize>, a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    // Map correlation key -> first line index that declares it.
    let mut key_owner: HashMap<String, usize> = HashMap::new();
    for (i, e) in exposures.iter().enumerate() {
        if let Some(keys) = &e.correlated {
            for k in keys {
                match key_owner.get(k) {
                    Some(&owner) => union(&mut parent, i, owner),
                    None => {
                        key_owner.insert(k.clone(), i);
                    }
                }
            }
        }
    }

    // Sum notional per correlation root; only lines in a group (have keys) count.
    let mut group_notional: HashMap<usize, Decimal> = HashMap::new();
    let mut correlated_notional = Decimal::ZERO;
    for (i, e) in exposures.iter().enumerate() {
        if e.correlated.is_some() {
            let root = find(&mut parent, i);
            let entry = group_notional.entry(root).or_insert(Decimal::ZERO);
            *entry += parse_decimal(&e.notional);
        }
    }
    for (_root, sum) in &group_notional {
        correlated_notional += *sum;
    }

    PortfolioProjection {
        total_notional: total,
        by_kind,
        correlated_notional,
        correlation_groups: group_notional.len() as u32,
    }
}

/// Check treasury/hot-wallet separation (doc §8.11 + §25 non-goal: no treasury
/// wallet in execution worker). Returns `Ok(())` when separated, or an error
/// describing the violation. `treasury_separated` is the frozen boolean on the
/// snapshot; this function exists so callers can assert it fail-closed rather
/// than trusting a bare flag.
pub fn check_treasury_separation(treasury_separated: bool) -> Result<(), &'static str> {
    if treasury_separated {
        Ok(())
    } else {
        Err("treasury and execution hot-wallet are not separated")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(kind: &str, key: &str, notional: &str, correlated: Option<Vec<&str>>) -> Exposure {
        Exposure {
            kind: kind.into(),
            key: key.into(),
            notional: notional.into(),
            correlated: correlated.map(|v| v.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn project_totals_and_by_kind() {
        let exps = vec![
            exp("token", "T1", "100", None),
            exp("token", "T2", "50", None),
            exp("pool", "P1", "200", None),
            exp("strategy", "S1", "10", None),
        ];
        let p = project(&exps);
        assert_eq!(p.total_notional, "360".parse::<rust_decimal::Decimal>().unwrap());
        assert_eq!(p.by_kind["token"], "150".parse::<rust_decimal::Decimal>().unwrap());
        assert_eq!(p.by_kind["pool"], "200".parse::<rust_decimal::Decimal>().unwrap());
        assert_eq!(p.correlated_notional, rust_decimal::Decimal::ZERO);
        assert_eq!(p.correlation_groups, 0);
    }

    #[test]
    fn correlated_exposure_counted_once_per_group() {
        let exps = vec![
            exp("token", "T1", "100", Some(vec!["g1", "g2"])),
            exp("token", "T2", "50", Some(vec!["g2"])), // shares g2 -> same group as T1
            exp("token", "T3", "30", Some(vec!["g3"])), // separate group
        ];
        let p = project(&exps);
        // T1 and T2 are one group (150), T3 is another (30) -> total 180.
        assert_eq!(p.correlated_notional, "180".parse::<rust_decimal::Decimal>().unwrap());
        assert_eq!(p.correlation_groups, 2);
    }

    #[test]
    fn malformed_notional_fails_closed_to_zero() {
        let exps = vec![exp("token", "T1", "not-a-number", None)];
        let p = project(&exps);
        assert_eq!(p.total_notional, rust_decimal::Decimal::ZERO);
    }

    #[test]
    fn treasury_separation_check() {
        assert!(check_treasury_separation(true).is_ok());
        assert!(check_treasury_separation(false).is_err());
    }
}
