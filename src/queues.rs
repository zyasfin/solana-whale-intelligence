//! Queue backpressure: fixed precedence with bottom-up pausing.
//!
//! Precedence (highest first): `live_watch`, `funding_radar`,
//! `telegram_ingest`, `seed_sync`, `historical_backfill`. Under resource
//! pressure queues pause from the bottom upward; live confirmed monitoring is
//! preserved.

#![allow(dead_code)]  // planned API surface; runtime wiring lands with the workers

use std::collections::BTreeMap;

/// Queue names in fixed precedence order.
pub const QUEUE_LIVE_WATCH: &str = "live_watch";
pub const QUEUE_FUNDING_RADAR: &str = "funding_radar";
pub const QUEUE_TELEGRAM_INGEST: &str = "telegram_ingest";
pub const QUEUE_SEED_SYNC: &str = "seed_sync";
pub const QUEUE_HISTORICAL_BACKFILL: &str = "historical_backfill";

/// All queues in precedence order (index 0 = highest).
pub const ALL_QUEUES: [&str; 5] = [
    QUEUE_LIVE_WATCH,
    QUEUE_FUNDING_RADAR,
    QUEUE_TELEGRAM_INGEST,
    QUEUE_SEED_SYNC,
    QUEUE_HISTORICAL_BACKFILL,
];

/// Runtime pause state per queue.
#[derive(Clone, Debug, Default)]
pub struct QueueState {
    pub paused: BTreeMap<String, bool>,
    /// Oldest pending age per queue (seconds), used for lag decisions.
    pub lag_seconds: BTreeMap<String, u64>,
}

impl QueueState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_paused(&self, queue: &str) -> bool {
        self.paused.get(queue).copied().unwrap_or(false)
    }

    pub fn set_paused(&mut self, queue: &str, paused: bool) {
        self.paused.insert(queue.to_string(), paused);
    }

    pub fn set_lag(&mut self, queue: &str, seconds: u64) {
        self.lag_seconds.insert(queue.to_string(), seconds);
    }

    /// Pause queues from the bottom upward until `target_paused` are paused.
    ///
    /// `live_watch` is never paused by this routine.
    pub fn pause_bottom_up(&mut self, target_paused: usize) {
        let mut paused_count = self.paused.values().filter(|p| **p).count();
        for queue in ALL_QUEUES.iter().rev() {
            if paused_count >= target_paused {
                break;
            }
            if *queue == QUEUE_LIVE_WATCH {
                continue;
            }
            if !self.is_paused(queue) {
                self.set_paused(queue, true);
                paused_count += 1;
            }
        }
    }

    /// Resume all queues.
    pub fn resume_all(&mut self) {
        for queue in ALL_QUEUES {
            self.set_paused(queue, false);
        }
    }

    /// Whether historical backfill should run given its lag threshold.
    pub fn backfill_allowed(&self, pause_lag_seconds: u64) -> bool {
        let lag = self.lag_seconds.get(QUEUE_HISTORICAL_BACKFILL).copied().unwrap_or(0);
        !self.is_paused(QUEUE_HISTORICAL_BACKFILL) && lag < pause_lag_seconds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_order_is_fixed() {
        assert_eq!(ALL_QUEUES[0], QUEUE_LIVE_WATCH);
        assert_eq!(ALL_QUEUES[1], QUEUE_FUNDING_RADAR);
        assert_eq!(ALL_QUEUES[2], QUEUE_TELEGRAM_INGEST);
        assert_eq!(ALL_QUEUES[3], QUEUE_SEED_SYNC);
        assert_eq!(ALL_QUEUES[4], QUEUE_HISTORICAL_BACKFILL);
    }

    #[test]
    fn pause_bottom_up_never_pauses_live_watch() {
        let mut state = QueueState::new();
        state.pause_bottom_up(5);
        assert!(!state.is_paused(QUEUE_LIVE_WATCH));
        assert!(state.is_paused(QUEUE_HISTORICAL_BACKFILL));
        assert!(state.is_paused(QUEUE_SEED_SYNC));
        assert!(state.is_paused(QUEUE_TELEGRAM_INGEST));
        assert!(state.is_paused(QUEUE_FUNDING_RADAR));
    }

    #[test]
    fn pausing_one_pauses_only_lowest() {
        let mut state = QueueState::new();
        state.pause_bottom_up(1);
        assert!(state.is_paused(QUEUE_HISTORICAL_BACKFILL));
        assert!(!state.is_paused(QUEUE_SEED_SYNC));
        assert!(!state.is_paused(QUEUE_TELEGRAM_INGEST));
        assert!(!state.is_paused(QUEUE_FUNDING_RADAR));
    }

    #[test]
    fn backfill_gates_on_lag() {
        let mut state = QueueState::new();
        state.set_lag(QUEUE_HISTORICAL_BACKFILL, 30);
        assert!(state.backfill_allowed(60));

        state.set_lag(QUEUE_HISTORICAL_BACKFILL, 90);
        assert!(!state.backfill_allowed(60), "lag 90s exceeds 60s threshold");

        state.set_lag(QUEUE_HISTORICAL_BACKFILL, 10);
        state.pause_bottom_up(1);
        assert!(!state.backfill_allowed(60), "paused backfill never runs");
    }

    #[test]
    fn resume_all_clears_pauses() {
        let mut state = QueueState::new();
        state.pause_bottom_up(5);
        state.resume_all();
        for queue in ALL_QUEUES {
            assert!(!state.is_paused(queue));
        }
    }

    #[test]
    fn low_profile_pauses_backfill_at_sixty_seconds() {
        let mut state = QueueState::new();
        state.set_lag(QUEUE_HISTORICAL_BACKFILL, 60);
        assert!(!state.backfill_allowed(60));
        state.set_lag(QUEUE_HISTORICAL_BACKFILL, 59);
        assert!(state.backfill_allowed(60));
    }
}
