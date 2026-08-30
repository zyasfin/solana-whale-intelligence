//! Runtime logic: source-health state machine (Phase 0, §8.12).
//!
//! Canonical source: PLAN SWI §8.12 "Source Health" (lines 599-610). Frozen
//! states: UP / SILENT / DEGRADED / DOWN / RECOVERING / DISABLED. Key rule:
//! **connected-but-silent is not healthy**. Health is driven by last
//! request/event/success, expected cadence, parser success, schema drift,
//! quota/backoff, latency, error rate, and coverage impact.
//!
//! This module implements the transition logic over the frozen
//! `core.rs::SourceHealthState` and `source.rs::ProviderHealth` types. It is
//! pure logic (no `sqlx`/Postgres), matching the other runtime modules.

use super::core::SourceHealthState;
use super::source::ProviderHealth;

/// A single health observation (one request/event cycle) used to drive a
/// transition. Timestamps are unix seconds; `None` means "no data".
#[derive(Clone, Debug)]
pub struct HealthSignal {
    /// Unix seconds of the most recent successful event/parse.
    pub last_success_secs: Option<i64>,
    /// Unix seconds of the most recent request (any, success or failure).
    pub last_request_secs: Option<i64>,
    /// Expected cadence in seconds (how often events should arrive).
    pub expected_cadence_secs: Option<i64>,
    /// Parser success rate in [0, 1]; `None` = no parser data yet.
    pub parser_success_rate: Option<f64>,
    /// Whether the provider's payload schema has drifted.
    pub schema_stale: bool,
    /// Consecutive failures since the last success.
    pub consecutive_failures: u32,
    /// Whether the provider is explicitly disabled by policy.
    pub disabled: bool,
}

/// The next health state, computed from the current state + a fresh signal.
///
/// Frozen §8.12 semantics:
/// - `disabled` -> DISABLED (terminal, policy override wins).
/// - connected-but-silent (no recent success beyond expected cadence) -> SILENT,
///   even if requests keep flowing — this is the core "not healthy" rule.
/// - schema drift or repeated failures -> DEGRADED (or DOWN past a threshold).
/// - a success after degradation -> RECOVERING (one good sample is not yet UP).
/// - a steady stream of on-time successes -> UP.
pub fn transition(current: &ProviderHealth, signal: &HealthSignal) -> SourceHealthState {
    if signal.disabled {
        return SourceHealthState::Disabled;
    }

    // Hard failures dominate: 3+ consecutive failures -> DOWN, regardless of
    // any historical success timestamp still on the record (REV-001-F02).
    if signal.consecutive_failures >= 3 {
        return SourceHealthState::Down;
    }

    // Success-based rules: a recent success drives the "up" path.
    if let Some(success) = signal.last_success_secs {
        let cadence_ok = signal
            .expected_cadence_secs
            .map(|c| {
                signal
                    .last_request_secs
                    .map(|req| req - success <= c)
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        if cadence_ok && !signal.schema_stale {
            // Steady on-time success. If we were degraded, one good sample is
            // only RECOVERING; otherwise UP.
            match current.state {
                SourceHealthState::Degraded
                | SourceHealthState::Down
                | SourceHealthState::Silent => SourceHealthState::Recovering,
                _ => SourceHealthState::Up,
            }
        } else if signal.schema_stale {
            SourceHealthState::Degraded
        } else {
            // Had a success but cadence is broken -> still degraded/silent.
            SourceHealthState::Silent
        }
    } else if signal.schema_stale {
        SourceHealthState::Degraded
    } else {
        // No success at all -> connected-but-silent.
        SourceHealthState::Silent
    }
}

/// Apply a health signal to a `ProviderHealth` in place, returning the new
pub fn apply(health: &mut ProviderHealth, signal: &HealthSignal) -> SourceHealthState {
    if let Some(s) = signal.last_success_secs {
        health.last_success_at = Some(s.to_string());
    }
    // `consecutive_failures` is the caller-provided truth; a fresh success
    // yields 0 there, but a historical success on the record must NOT reset
    // the counter (REV-001-F02).
    health.consecutive_failures = signal.consecutive_failures;
    if let Some(r) = signal.last_request_secs {
        health.last_request_at = Some(r.to_string());
    }
    if let Some(rate) = signal.parser_success_rate {
        health.parser_success_rate = Some(rate);
    }
    health.schema_stale = signal.schema_stale;

    let next = transition(health, signal);
    health.state = next;
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ProviderHealth {
        ProviderHealth::default()
    }

    #[test]
    fn connected_but_silent_is_not_healthy() {
        // Requests keep flowing (last_request_secs recent) but no success and
        // cadence broken -> SILENT, never UP.
        let sig = HealthSignal {
            last_success_secs: None,
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: None,
            schema_stale: false,
            consecutive_failures: 0,
            disabled: false,
        };
        assert_eq!(transition(&base(), &sig), SourceHealthState::Silent);
    }

    #[test]
    fn on_time_success_is_up() {
        let sig = HealthSignal {
            last_success_secs: Some(1000),
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: Some(1.0),
            schema_stale: false,
            consecutive_failures: 0,
            disabled: false,
        };
        assert_eq!(transition(&base(), &sig), SourceHealthState::Up);
    }

    #[test]
    fn schema_stale_is_degraded() {
        let sig = HealthSignal {
            last_success_secs: Some(1000),
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: Some(1.0),
            schema_stale: true,
            consecutive_failures: 0,
            disabled: false,
        };
        assert_eq!(transition(&base(), &sig), SourceHealthState::Degraded);
    }

    #[test]
    fn hard_failures_are_down() {
        let sig = HealthSignal {
            last_success_secs: None,
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: None,
            schema_stale: false,
            consecutive_failures: 5,
            disabled: false,
        };
        assert_eq!(transition(&base(), &sig), SourceHealthState::Down);
    }

    #[test]
    fn disabled_overrides_everything() {
        let sig = HealthSignal {
            last_success_secs: Some(1000),
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: Some(1.0),
            schema_stale: false,
            consecutive_failures: 0,
            disabled: true,
        };
        assert_eq!(transition(&base(), &sig), SourceHealthState::Disabled);
    }

    #[test]
    fn degraded_recovers_not_instantly_up() {
        let mut h = base();
        h.state = SourceHealthState::Degraded;
        let sig = HealthSignal {
            last_success_secs: Some(1000),
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: Some(1.0),
            schema_stale: false,
            consecutive_failures: 0,
            disabled: false,
        };
        // One good sample after degraded -> RECOVERING, not UP.
        assert_eq!(transition(&h, &sig), SourceHealthState::Recovering);
    }

    #[test]
    fn apply_mutates_health_record() {
        let mut h = base();
        let sig = HealthSignal {
            last_success_secs: Some(1000),
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: Some(1.0),
            schema_stale: false,
            consecutive_failures: 0,
            disabled: false,
        };
        let st = apply(&mut h, &sig);
        assert_eq!(st, SourceHealthState::Up);
        assert_eq!(h.state, SourceHealthState::Up);
        assert_eq!(h.last_success_at.as_deref(), Some("1000"));
        assert_eq!(h.consecutive_failures, 0);
    }

    // Regression REV-001-F02: 3+ consecutive failures must be DOWN even when a
    // historical success timestamp is still on the record.
    #[test]
    fn failures_dominate_historical_success() {
        let mut h = base();
        h.last_success_at = Some("900".to_string()); // historical success
        let sig = HealthSignal {
            last_success_secs: Some(900), // still Some (historical), but...
            last_request_secs: Some(1000),
            expected_cadence_secs: Some(60),
            parser_success_rate: Some(1.0),
            schema_stale: false,
            consecutive_failures: 3, // 3 recent failures
            disabled: false,
        };
        assert_eq!(transition(&h, &sig), SourceHealthState::Down);

        // apply must also produce DOWN and NOT reset the counter to 0.
        let st = apply(&mut h, &sig);
        assert_eq!(st, SourceHealthState::Down);
        assert_eq!(h.consecutive_failures, 3);
    }
}
