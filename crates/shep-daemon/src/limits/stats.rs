//! Live per-sheep resource readings: the CPU and memory figures every row of
//! `shep flock` carries.
//!
//! Sampling and enforcement are two jobs over one process-table reading, and
//! they cover different sheep. [`super::LimitEnforcer`] is armed only where an
//! app configured `max_memory`; [`StatsState`] watches **every** sheep with a
//! pid, because a listing reports resource use whether or not the app set a
//! ceiling — an app with no `max_memory` is the ordinary case, and a listing
//! that reported nothing for it would be reporting nothing for most of the
//! flock. Both are served by the same tick: the polling enforcer builds one
//! [`TreeIndex`] per pass and hands it here before it runs the enforcement
//! pass, so the syscall walk still happens once.
//!
//! CPU is a rate where memory is a level, and that is the whole of the design
//! here. Resident memory is whatever the current reading says, so an on-demand
//! read answers it outright. Accumulated CPU time is a counter, so a
//! percentage needs two readings and the wall time between them — which is
//! what a **baseline** is: the counter and the instant recorded for one root
//! pid at the last periodic tick. An on-demand read subtracts against that
//! baseline and never writes one, so two listings a moment apart cannot divide
//! a near-zero delta by a near-zero window; the window a percentage is
//! measured over is instead always at most one `MEMORY_POLL_INTERVAL` old.
//!
//! Both totals are summed over the sheep's whole process tree, exactly as the
//! memory limit is — see the [`limits`](super) module doc for what that tree
//! is and where it diverges from the kill unit.

use core::fmt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::time::Instant;

use super::sample::{MemorySampler, TreeIndex};

/// One sheep's live resource reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SheepStats {
    /// Tree CPU over the window since the last periodic baseline, as a
    /// percentage of one core.
    ///
    /// `None` when this pid has no baseline yet. A sheep spawned since the
    /// last tick has no honest figure, and one invented from a 50 ms window
    /// is worse than an empty cell.
    ///
    /// A value over 100 is a tree using more than one core, not a bug.
    pub cpu_percent: Option<f32>,
    /// Tree resident set size, current as of the reading that produced this.
    pub memory_bytes: u64,
}

/// One watched root's CPU counter, and when it was read.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    cpu_ms: u64,
    at: Instant,
}

/// Which sheep are worth sampling, and what their CPU counters read at the
/// last periodic tick.
///
/// [`Self::record_baseline`] is the ONLY writer of the baseline map. That is
/// the invariant the whole type rests on: [`Self::sample_now`] serves a
/// listing and must leave the baseline alone, or a second listing a
/// millisecond after the first would divide a near-zero CPU delta by a
/// near-zero window and report anything from 0% to thousands.
pub(crate) struct StatsState {
    sampler: Arc<dyn MemorySampler>,
    // Both maps take `std::sync::Mutex`, not tokio's, for the reason
    // `SysinfoSampler` gives for its own: every critical section below is a
    // map operation, none is held across an `.await`, and a poisoned lock
    // recovers with `PoisonError::into_inner` rather than ending a daemon
    // whose whole job is staying up. Neither is ever held while the other
    // is, and neither is ever held across the syscall walk — the walk's
    // input is a snapshot of the watch map, taken and released first — so
    // there is no lock order to get wrong.
    /// Root pid per watched sheep id.
    watched: Mutex<HashMap<u32, u32>>,
    /// The last periodic reading, per watched root pid.
    baselines: Mutex<HashMap<u32, Baseline>>,
}

impl StatsState {
    /// A sampler-backed state watching nothing yet.
    pub(crate) fn new(sampler: Arc<dyn MemorySampler>) -> Self {
        Self {
            sampler,
            watched: Mutex::new(HashMap::new()),
            baselines: Mutex::new(HashMap::new()),
        }
    }

    /// Starts sampling `root_pid` for `id`.
    ///
    /// Re-watching an id replaces the previous pid — a respawn gives the same
    /// id a new one.
    pub(crate) fn watch(&self, id: u32, root_pid: u32) {
        self.watched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, root_pid);
        // A pid entering the watch set starts from no baseline, never from
        // one recorded while a DIFFERENT process held that number — the OS
        // really does recycle pids, and inheriting a stale counter would
        // charge a new sheep with whatever CPU the old one had accumulated.
        // This one line is the whole of that guarantee, and it is enough
        // because `watch` is the only door into the watch set: a baseline
        // left behind by an id that stopped being watched is unreadable
        // (`sample_now` reports only watched pids) until its pid is watched
        // again, and that path runs this.
        self.baselines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&root_pid);
    }

    /// Stops sampling `id`. A no-op for an id never watched.
    ///
    /// Leaves the baseline map alone: see [`Self::watch`] for why nothing can
    /// read a stale entry, and [`Self::record_baseline`] for what clears it.
    pub(crate) fn unwatch(&self, id: u32) {
        self.watched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// Records every watched root's CPU counter from one periodic reading.
    ///
    /// The whole map is replaced rather than updated in place, so an id that
    /// stopped being watched does not leave a baseline sitting in it until
    /// the daemon exits. That is housekeeping, not the correctness rule —
    /// [`Self::watch`] is what keeps a stale entry from ever being read.
    pub(crate) fn record_baseline(&self, index: &TreeIndex, now: Instant) {
        let fresh: HashMap<u32, Baseline> = self
            .watched_pids()
            .into_iter()
            .map(|root_pid| {
                let baseline = Baseline {
                    cpu_ms: index.cpu_from(root_pid),
                    at: now,
                };
                (root_pid, baseline)
            })
            .collect();
        *self
            .baselines
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = fresh;
    }

    /// [`Self::record_baseline`] over a reading this call takes itself.
    ///
    /// For a caller that has not already built an index. The polling tick uses
    /// the other one: it has an index in hand and must not walk the process
    /// table twice per tick, which leaves this one with no production caller
    /// — it exists so a test can put a baseline in place in one line instead
    /// of reaching through to the sampler for an index of its own.
    #[allow(dead_code, reason = "called only by this crate's tests")]
    pub(crate) fn record_baseline_now(&self, now: Instant) {
        let table = self.sampler.sample();
        self.record_baseline(&TreeIndex::build(&table), now);
    }

    /// One live reading per watched sheep, keyed by root pid.
    ///
    /// Blocking: it performs the syscall walk itself, which is what makes the
    /// memory figure current rather than up to `MEMORY_POLL_INTERVAL` stale.
    /// It writes no baseline — see this type's own doc for why that matters.
    pub(crate) fn sample_now(&self) -> HashMap<u32, SheepStats> {
        let roots = self.watched_pids();
        let table = self.sampler.sample();
        let index = TreeIndex::build(&table);
        let now = Instant::now();
        // Cloned rather than read under the lock: the walks below are the
        // expensive part of this call, and holding the baseline map across
        // them would block the periodic tick's store behind a listing.
        let baselines = self
            .baselines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        roots
            .into_iter()
            .map(|root_pid| {
                let observed_cpu_ms = index.cpu_from(root_pid);
                let stats = SheepStats {
                    cpu_percent: baselines
                        .get(&root_pid)
                        .and_then(|baseline| cpu_percent(*baseline, observed_cpu_ms, now)),
                    memory_bytes: index.sum_from(root_pid),
                };
                (root_pid, stats)
            })
            .collect()
    }

    /// Every watched root pid, with the lock released before the caller does
    /// anything with them.
    fn watched_pids(&self) -> Vec<u32> {
        self.watched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .copied()
            .collect()
    }

    /// Every `(id, root_pid)` currently watched, sorted by id.
    ///
    /// Lets a test outside this module assert on the watch set without a
    /// second fake standing in for the sampler.
    #[cfg(test)]
    pub(crate) fn watched_for_test(&self) -> Vec<(u32, u32)> {
        let mut watched: Vec<(u32, u32)> = self
            .watched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(&id, &root_pid)| (id, root_pid))
            .collect();
        watched.sort_unstable();
        watched
    }
}

impl fmt::Debug for StatsState {
    // The sampler is a trait object and is not Debug; the two maps are behind
    // locks this impl deliberately does not take, since a formatter reached
    // from inside a locked section would deadlock the thread it was meant to
    // help debug. Role only, and `finish_non_exhaustive` because the two maps
    // really are omitted rather than absent.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatsState")
            .field("sampler", &"<dyn MemorySampler>")
            .finish_non_exhaustive()
    }
}

/// `baseline`'s counter against a later reading of the same tree, as a
/// percentage of one core.
///
/// `None` when no wall time has passed since the baseline — dividing by that
/// window is what produces the nonsense figure the whole baseline scheme
/// exists to avoid.
fn cpu_percent(baseline: Baseline, cpu_ms: u64, now: Instant) -> Option<f32> {
    let window = now.saturating_duration_since(baseline.at);
    if window.is_zero() {
        return None;
    }
    // Saturating: a counter that went BACKWARDS means the tree under this pid
    // is not the tree the baseline was taken from — a lamb exited, or the pid
    // was recycled — and zero is the honest reading for a window this
    // baseline cannot describe.
    let elapsed_cpu_ms = cpu_ms.saturating_sub(baseline.cpu_ms);
    // CPU-milliseconds over wall-seconds is per-mille of one core, so a tenth
    // of that is the percentage. Computed in f64 and narrowed once at the end:
    // the subtraction above is exact in u64, and f32 alone would start losing
    // milliseconds off a counter that has been running for a month.
    let percent = elapsed_cpu_ms as f64 / window.as_secs_f64() / 10.0;
    Some(percent as f32)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::super::MEMORY_POLL_INTERVAL;
    use super::super::sample::ProcessRss;
    use super::*;
    use crate::testing::ScriptedSampler;

    fn rss_cpu(pid: u32, parent: Option<u32>, bytes: u64, cpu_ms: u64) -> ProcessRss {
        ProcessRss {
            pid,
            parent,
            bytes,
            cpu_ms,
        }
    }

    /// fails if a sheep with no baseline reports a number. A process spawned
    /// since the last tick has no honest CPU figure, and one invented from a
    /// 50 ms window is worse than an empty cell.
    #[tokio::test(start_paused = true)]
    async fn a_sheep_with_no_baseline_reports_no_cpu_but_still_reports_memory() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss_cpu(
            100, None, 4096, 1000,
        )]]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);

        let now = stats.sample_now();
        assert_eq!(now[&100].cpu_percent, None);
        assert_eq!(now[&100].memory_bytes, 4096, "memory is always current");
    }

    /// fails if the delta is computed against the wrong pair of readings.
    /// 1500 CPU-ms over a 15 s window is 10% of one core; a percentage
    /// computed against the process's whole accumulated time instead would
    /// read 16.7%, and against the wrong elapsed window, anything at all.
    #[tokio::test(start_paused = true)]
    async fn cpu_is_the_delta_since_the_periodic_baseline() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 2_500)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);

        stats.record_baseline_now(Instant::now());
        tokio::time::advance(MEMORY_POLL_INTERVAL).await;

        let now = stats.sample_now();
        let cpu = now[&100].cpu_percent.expect("a baseline exists");
        assert!((cpu - 10.0).abs() < 0.01, "expected ~10%, got {cpu}");
    }

    /// fails if an on-demand read writes the baseline. Two `flock` calls a
    /// moment apart would then divide a near-zero CPU delta by a near-zero
    /// window — the second call reporting anything from 0% to thousands,
    /// depending on rounding.
    #[tokio::test(start_paused = true)]
    async fn a_second_read_a_moment_later_still_measures_from_the_periodic_baseline() {
        // Three readings: the baseline, then two on-demand ones a millisecond
        // apart. The CPU counter barely moves between the last two, which is
        // exactly the shape that makes a baseline-writing implementation
        // divide ~1 CPU-ms by ~1 ms and report ~100%.
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 2_500)],
            vec![rss_cpu(100, None, 4096, 2_501)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);
        stats.record_baseline_now(Instant::now());
        tokio::time::advance(MEMORY_POLL_INTERVAL).await;

        let first = stats.sample_now()[&100].cpu_percent.unwrap();
        tokio::time::advance(Duration::from_millis(1)).await;
        let second = stats.sample_now()[&100].cpu_percent.unwrap();
        assert!(
            (first - 10.0).abs() < 0.01,
            "1500 CPU-ms over 15 s is 10%, got {first}"
        );
        assert!(
            (second - 10.0).abs() < 0.02,
            "the second read divided by the gap between the two READS rather \
             than by the window since the tick: {first} then {second}"
        );
    }

    /// fails if `unwatch` stops removing the id from the watch set. A sheep
    /// that is gone would otherwise be sampled forever, and every listing
    /// would carry a row for a pid the OS has already handed to somebody
    /// else.
    #[tokio::test(start_paused = true)]
    async fn an_unwatched_sheep_is_no_longer_sampled() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 2_500)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);
        stats.record_baseline_now(Instant::now());
        tokio::time::advance(MEMORY_POLL_INTERVAL).await;

        stats.unwatch(1);
        assert!(
            stats.sample_now().is_empty(),
            "an unwatched sheep must not still be sampled"
        );
    }

    /// fails if `watch` stops clearing the incoming pid's baseline. The OS
    /// really does recycle pids, and no tick need run in between: the
    /// baseline map is written only by the periodic tick, so between two of
    /// them `watch` is the ONLY thing that can drop the counter the dead
    /// process left behind on that number.
    #[tokio::test(start_paused = true)]
    async fn a_recycled_pid_starts_from_no_baseline_rather_than_the_dead_process_counter() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 9_000)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);
        stats.record_baseline_now(Instant::now());
        tokio::time::advance(MEMORY_POLL_INTERVAL).await;

        stats.unwatch(1);
        stats.watch(2, 100);
        assert_eq!(
            stats.sample_now()[&100].cpu_percent,
            None,
            "the new process on pid 100 must not be measured against the dead one's counter"
        );
    }

    /// fails if the zero-length window is not guarded. A listing taken in the
    /// same instant as a tick divides by zero, and f64 answers that with
    /// `inf` or `NaN` rather than an error — `Some(inf)` would reach the
    /// column and render as a number.
    #[tokio::test(start_paused = true)]
    async fn a_read_in_the_same_instant_as_the_baseline_reports_no_cpu() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(100, None, 4096, 1_000)],
            vec![rss_cpu(100, None, 4096, 9_000)],
        ]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);
        stats.record_baseline_now(Instant::now());

        // No `advance`: the paused clock has not moved since the baseline.
        assert_eq!(stats.sample_now()[&100].cpu_percent, None);
    }

    // IR-41: `StatsState`'s Debug is hand-rolled — the sampler is a trait
    // object, and the two maps are behind locks a formatter must not take.
    // Pinned exactly so a later edit cannot quietly start printing either.
    #[tokio::test(start_paused = true)]
    async fn stats_state_debug_names_the_sampler_by_role_and_prints_no_map() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss_cpu(
            100, None, 4096, 1000,
        )]]));
        let stats = StatsState::new(sampler);
        stats.watch(1, 100);
        assert_eq!(
            format!("{stats:?}"),
            r#"StatsState { sampler: "<dyn MemorySampler>", .. }"#
        );
    }

    // fails if `watched_for_test` stops reporting the pid an id is watched
    // against, or stops replacing a re-watched id's pid rather than adding a
    // second entry for it. `extras.rs`'s own case asserts on this helper, so
    // a helper that lied would take that case with it.
    #[tokio::test(start_paused = true)]
    async fn watching_reports_each_id_once_against_its_current_pid() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss_cpu(
            100, None, 4096, 1000,
        )]]));
        let stats = StatsState::new(sampler);
        stats.watch(7, 4242);
        stats.watch(2, 100);
        assert_eq!(stats.watched_for_test(), vec![(2, 100), (7, 4242)]);

        stats.watch(7, 5353);
        assert_eq!(
            stats.watched_for_test(),
            vec![(2, 100), (7, 5353)],
            "a respawn replaces the id's pid rather than adding a second one"
        );

        stats.unwatch(2);
        stats.unwatch(99);
        assert_eq!(
            stats.watched_for_test(),
            vec![(7, 5353)],
            "unwatching an id never watched must leave the rest alone"
        );
    }
}
