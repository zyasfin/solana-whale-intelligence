//! Retention maintenance scheduler (REV-046-A3).
//!
//! `prune_raw_events` and `prune_market_snapshots` existed since early phases, and
//! `RuntimeProfile::raw_retention_days()` existed to configure them — but nothing
//! ever called them. That is the shape the work order calls PARTIAL: a helper plus a
//! unit test, with no production caller, so disposable payloads grew without bound
//! and no operator could see it.
//!
//! What this module adds:
//!   * one maintenance task owned by `Command::Run`;
//!   * bounded batches, never an unbounded `DELETE` on a live ingest table;
//!   * only DISPOSABLE raw payloads are pruned — evidence, hashes, decision bundles,
//!     audit rows, tombstones and historical truth are never touched;
//!   * observable state (last success, last error, rows processed, oldest retained,
//!     next run) so `health` can report degraded instead of failing silently.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Rows deleted per statement. Bounded so one pass cannot hold a long lock on the
/// ingest hot path (REV-046-A3.5).
const BATCH_LIMIT: i64 = 5_000;

/// Safety ceiling on batches per cycle, so a huge backlog is drained over several
/// cycles instead of monopolising a connection.
const MAX_BATCHES_PER_CYCLE: u32 = 40;

/// Retention must be a positive number of days, and refusing an absurd value is
/// cheaper than discovering it deleted a year of data.
const MIN_RETENTION_DAYS: i64 = 1;
const MAX_RETENTION_DAYS: i64 = 3_650;

/// Observable maintenance state. Read by `health` (REV-046-A3.4/A3.6).
#[derive(Debug, Default)]
pub struct MaintenanceState {
    /// Unix seconds of the last successful cycle; 0 = never.
    last_success_unix: AtomicI64,
    /// Unix seconds of the last failed cycle; 0 = never.
    last_error_unix: AtomicI64,
    last_error: std::sync::Mutex<Option<String>>,
    rows_pruned_total: AtomicU64,
    /// Unix seconds of the oldest row still retained; 0 = unknown/empty.
    oldest_retained_unix: AtomicI64,
    /// Interval between cycles, so staleness can be judged against it.
    interval_secs: AtomicI64,
    started: AtomicBool,
    /// Unix seconds when the task started; the baseline for staleness before the
    /// first success (REV-046-A3.6).
    started_at_unix: AtomicI64,
}

impl MaintenanceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_success(&self) -> Option<DateTime<Utc>> {
        unix_to_dt(self.last_success_unix.load(Ordering::Relaxed))
    }

    pub fn last_error_at(&self) -> Option<DateTime<Utc>> {
        unix_to_dt(self.last_error_unix.load(Ordering::Relaxed))
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    pub fn rows_pruned_total(&self) -> u64 {
        self.rows_pruned_total.load(Ordering::Relaxed)
    }

    pub fn oldest_retained(&self) -> Option<DateTime<Utc>> {
        unix_to_dt(self.oldest_retained_unix.load(Ordering::Relaxed))
    }

    pub fn interval_secs(&self) -> i64 {
        self.interval_secs.load(Ordering::Relaxed)
    }

    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// Next expected run, derived from the last success and the interval.
    pub fn next_run(&self) -> Option<DateTime<Utc>> {
        let last = self.last_success()?;
        let iv = self.interval_secs();
        if iv <= 0 {
            return None;
        }
        Some(last + chrono::Duration::seconds(iv))
    }

    /// Whether maintenance is stale: no success for more than TWO intervals
    /// (REV-046-A3.6). A task that died silently is the failure mode this catches.
    ///
    /// Staleness is measured from the moment the task STARTED, not from "has it ever
    /// succeeded". The first version reported DEGRADED the instant `run` came up —
    /// the live smoke test showed `stale=true last_error=None last_success=None`
    /// seconds after startup. An alert that fires on every boot trains operators to
    /// ignore it, so it would have been worse than no alert at all.
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        if !self.is_started() {
            return false; // not enabled; not a fault
        }
        let iv = self.interval_secs();
        if iv <= 0 {
            return true;
        }
        match self.last_success() {
            // Never succeeded yet: only a fault once TWO intervals have elapsed since
            // start, which is the same tolerance applied to a running task.
            None => match self.started_at() {
                Some(started) => (now - started).num_seconds() > iv * 2,
                None => true,
            },
            Some(last) => (now - last).num_seconds() > iv * 2,
        }
    }

    /// When the task was started; `None` when it never was.
    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        unix_to_dt(self.started_at_unix.load(Ordering::Relaxed))
    }

    fn record_success(&self, rows: u64, oldest: Option<DateTime<Utc>>) {
        self.last_success_unix
            .store(Utc::now().timestamp(), Ordering::Relaxed);
        self.rows_pruned_total.fetch_add(rows, Ordering::Relaxed);
        self.oldest_retained_unix
            .store(oldest.map(|d| d.timestamp()).unwrap_or(0), Ordering::Relaxed);
        if let Ok(mut g) = self.last_error.lock() {
            *g = None;
        }
    }

    fn record_error(&self, e: &anyhow::Error) {
        self.last_error_unix
            .store(Utc::now().timestamp(), Ordering::Relaxed);
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(e.to_string());
        }
    }
}

fn unix_to_dt(secs: i64) -> Option<DateTime<Utc>> {
    if secs <= 0 {
        return None;
    }
    DateTime::from_timestamp(secs, 0)
}

/// Effective retention in days: profile default, overridden by config when present,
/// then validated (REV-046-A3.2).
///
/// An invalid override is REJECTED rather than clamped: silently substituting a
/// different retention than the operator configured is how data disappears
/// unexpectedly.
pub fn effective_retention_days(
    profile_days: i64,
    override_days: Option<i64>,
) -> Result<i64> {
    let days = override_days.unwrap_or(profile_days);
    if days < MIN_RETENTION_DAYS {
        anyhow::bail!(
            "retention must be at least {MIN_RETENTION_DAYS} day(s); configured {days}"
        );
    }
    if days > MAX_RETENTION_DAYS {
        anyhow::bail!(
            "retention of {days} days exceeds the {MAX_RETENTION_DAYS}-day maximum; \
             refusing a value this far outside the intended range"
        );
    }
    Ok(days)
}

/// Run ONE maintenance cycle. Returns rows pruned.
///
/// Only disposable payload tables are touched. `raw_events` is the provider payload
/// cache and `market_snapshots` is resampled telemetry; both are re-derivable. No
/// evidence, hash, decision, audit, or tombstone row is in scope — those are
/// append-only truth, and the runtime role no longer even has DELETE on them
/// (migration 1028).
pub async fn run_cycle(pool: &PgPool, retention_days: i64) -> Result<u64> {
    let mut total: u64 = 0;

    // Bounded batches until a pass finds nothing, capped per cycle.
    for _ in 0..MAX_BATCHES_PER_CYCLE {
        let n = crate::db::prune_raw_events_batch(pool, retention_days, BATCH_LIMIT).await?;
        total += n;
        if n == 0 {
            break;
        }
    }

    // Market snapshots are far smaller; one statement is acceptable.
    total += crate::db::prune_market_snapshots(pool, retention_days).await?;
    Ok(total)
}

/// Run one cycle AND record the outcome in `state` (REV-050-F03).
///
/// The loop used to inline this, so the only way to observe a real cycle was to wait
/// out the six-hour interval: the 30-second runtime smoke proved the task STARTED and
/// nothing else. A reviewer cannot verify "old row deleted, boundary row retained,
/// last_success and rows_pruned_total updated, forced failure degrades health" from a
/// startup log line.
///
/// Extracted rather than duplicated on purpose: a separate one-shot path would be a
/// second implementation, and then the thing under test would not be the thing that
/// runs in production. `db retention run-once` and the scheduler execute THIS
/// function.
pub async fn run_cycle_recording(
    pool: &PgPool,
    retention_days: i64,
    state: &MaintenanceState,
) -> Result<u64> {
    match run_cycle(pool, retention_days).await {
        Ok(rows) => {
            let oldest = crate::db::oldest_retained_raw_event(pool).await.unwrap_or(None);
            state.record_success(rows, oldest);
            Ok(rows)
        }
        Err(e) => {
            // Recorded BEFORE returning: a caller that propagates the error must not
            // also have to remember to degrade health.
            state.record_error(&e);
            Err(e)
        }
    }
}

/// Spawn the maintenance loop. Returns the shared state for `health` to read.
///
/// The caller keeps ONE handle per process. `Command::Run` owns it; no other command
/// starts it, so a restart cannot leave two loops pruning concurrently
/// (REV-046-A3 acceptance).
pub fn spawn(
    pool: PgPool,
    retention_days: i64,
    interval: Duration,
) -> Arc<MaintenanceState> {
    let state = Arc::new(MaintenanceState::new());
    state
        .interval_secs
        .store(interval.as_secs() as i64, Ordering::Relaxed);
    state.started.store(true, Ordering::Relaxed);
    state
        .started_at_unix
        .store(Utc::now().timestamp(), Ordering::Relaxed);

    let task_state = state.clone();
    tokio::spawn(async move {
        tracing::info!(
            retention_days,
            interval_secs = interval.as_secs(),
            "retention maintenance task started"
        );
        loop {
            match run_cycle_recording(&pool, retention_days, &task_state).await {
                Ok(rows) if rows > 0 => {
                    tracing::info!(rows, "retention maintenance pruned disposable rows");
                }
                Ok(_) => {}
                Err(e) => {
                    // `run_cycle_recording` already recorded the error, so health is
                    // degraded; logging here keeps the loop alive rather than ending
                    // maintenance forever on one transient failure.
                    tracing::error!(error = %e, "retention maintenance cycle failed");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });

    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_override_is_validated_not_clamped() {
        // Profile default when no override.
        assert_eq!(effective_retention_days(14, None).unwrap(), 14);
        // Override wins.
        assert_eq!(effective_retention_days(14, Some(30)).unwrap(), 30);
        // Zero and negative are rejected: a `0` would delete everything.
        assert!(effective_retention_days(14, Some(0)).is_err());
        assert!(effective_retention_days(14, Some(-1)).is_err());
        // Absurdly large is rejected rather than silently accepted.
        assert!(effective_retention_days(14, Some(100_000)).is_err());
        // A profile default that is itself invalid must not slip through.
        assert!(effective_retention_days(0, None).is_err());
    }

    #[test]
    fn staleness_needs_two_missed_intervals() {
        let s = MaintenanceState::new();
        // Not started: not a fault.
        assert!(!s.is_stale(Utc::now()));

        let start = Utc::now();
        s.started.store(true, Ordering::Relaxed);
        s.started_at_unix.store(start.timestamp(), Ordering::Relaxed);
        s.interval_secs.store(60, Ordering::Relaxed);

        // Just started and no success yet: NOT stale. The live smoke test showed the
        // first version alerting DEGRADED seconds after boot, and an alert that fires
        // on every start gets ignored.
        assert!(!s.is_stale(start));
        assert!(!s.is_stale(start + chrono::Duration::seconds(90)));
        // But a task that has never succeeded after two intervals IS stale — that is
        // what a silently dead pruner looks like.
        assert!(s.is_stale(start + chrono::Duration::seconds(121)));

        let now = Utc::now();
        s.last_success_unix.store(now.timestamp(), Ordering::Relaxed);
        assert!(!s.is_stale(now));
        assert!(!s.is_stale(now + chrono::Duration::seconds(90)));
        assert!(s.is_stale(now + chrono::Duration::seconds(121)));
    }

    #[test]
    fn next_run_derives_from_last_success_and_interval() {
        let s = MaintenanceState::new();
        assert!(s.next_run().is_none(), "no success yet means no prediction");

        // Compare against the SECOND-truncated instant that was stored, not against
        // `Utc::now()`: the state keeps unix seconds, so the sub-second remainder of
        // `now` would make this assertion off by one and flaky. (My first version
        // was, and a flaky test gets deleted rather than fixed.)
        let now = Utc::now();
        let stored = DateTime::from_timestamp(now.timestamp(), 0).expect("valid ts");
        s.interval_secs.store(3600, Ordering::Relaxed);
        s.last_success_unix.store(now.timestamp(), Ordering::Relaxed);
        let next = s.next_run().expect("predicted");
        assert_eq!((next - stored).num_seconds(), 3600);
    }

    #[test]
    fn error_is_recorded_and_cleared_by_the_next_success() {
        let s = MaintenanceState::new();
        s.record_error(&anyhow::anyhow!("permission denied for table raw_events"));
        assert!(s.last_error().unwrap().contains("permission denied"));
        assert!(s.last_error_at().is_some());

        s.record_success(10, Some(Utc::now()));
        assert!(
            s.last_error().is_none(),
            "a later success must clear the stale error text"
        );
        assert_eq!(s.rows_pruned_total(), 10);
        assert!(s.oldest_retained().is_some());
    }
}
