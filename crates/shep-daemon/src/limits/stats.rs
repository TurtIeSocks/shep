//! Live per-sheep resource readings: the CPU and memory figures every row of
//! `shep flock` carries.
//!
//! [`super::LimitEnforcer`] samples only sheep with `max_memory` set;
//! [`StatsState`] watches every sheep with a pid, off the same
//! [`TreeIndex`] per polling tick.
//!
//! Memory is a level and reads current on demand. CPU is a counter: a percentage needs the
//! last periodic baseline, subtracted without writing one, so its window is usually one
//! `MEMORY_POLL_INTERVAL` old, longer if a full breaches channel paused the poll loop. Both
//! sum the whole tree; see [`limits`](super) for the kill-unit divergence.

use core::fmt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use shep_core::protocol::Lamb;
use tokio::time::Instant;

use super::sample::{MemorySampler, ProcessIdentity, TreeIndex};

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
    // `std::sync::Mutex`: every critical section is a map operation, none
    // held across an `.await`. Never held together, and never across the
    // syscall walk, so there is no lock order to get wrong.
    /// Root pid per watched sheep id.
    watched: Mutex<HashMap<u32, u32>>,
    /// The last periodic reading, per watched root pid.
    baselines: Mutex<HashMap<u32, Baseline>>,
}

/// An indexed snapshot of the machine's process table: who each process is,
/// and which processes name it as their parent.
///
/// Built once per [`StatsState::lamb_index`] call and shared across
/// [`StatsState::lambs_of`] calls, so a caller walking several roots
/// (`shep describe all`) reuses one scan and one instant instead of
/// re-walking the table per root.
#[derive(Debug)]
pub(crate) struct LambIndex {
    /// Every process by pid, the name a lamb row carries.
    by_pid: HashMap<u32, ProcessIdentity>,
    /// Child pids per parent pid.
    children_of: HashMap<u32, Vec<u32>>,
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
    /// Re-watching an id replaces the previous pid: a respawn gives the same
    /// id a new one.
    pub(crate) fn watch(&self, id: u32, root_pid: u32) {
        self.watched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, root_pid);
        // A pid entering the watch set starts from no baseline, never one
        // recorded while a different process held that number: the OS
        // recycles pids, and inheriting a stale counter would charge a new
        // sheep with the old one's accumulated CPU.
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
    /// the daemon exits. [`Self::watch`] is what keeps a stale entry from
    /// ever being read.
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
    /// For a caller with no index already in hand. The polling tick always
    /// has one and uses [`Self::record_baseline`] directly, so this has no
    /// production caller; it lets a test set a baseline in one line.
    #[allow(dead_code, reason = "called only by this crate's tests")]
    pub(crate) fn record_baseline_now(&self, now: Instant) {
        let table = self.sampler.sample();
        self.record_baseline(&TreeIndex::build(&table), now);
    }

    /// One live reading per watched sheep, keyed by root pid.
    ///
    /// Blocking: it performs the syscall walk itself, which is what makes the
    /// memory figure current rather than up to `MEMORY_POLL_INTERVAL` stale.
    /// It writes no baseline: see this type's own doc for why.
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

    /// Indexes one fresh walk of the machine's process table.
    ///
    /// The table refresh lives here rather than in [`Self::lambs_of`], so a
    /// caller answering several sheep pays for it once. See [`LambIndex`].
    pub(crate) fn lamb_index(&self) -> LambIndex {
        let table = self.sampler.identify();
        let mut by_pid = HashMap::with_capacity(table.len());
        let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
        for entry in table {
            if let Some(parent) = entry.parent {
                children_of.entry(parent).or_default().push(entry.pid);
            }
            by_pid.insert(entry.pid, entry);
        }
        LambIndex {
            by_pid,
            children_of,
        }
    }

    /// Every process `index` reports as a descendant of `root_pid`, in pid
    /// order, excluding `root_pid` itself.
    ///
    /// Cycle-safe like [`TreeIndex::total_over`]: the kernel does not
    /// produce a cycle in the parent links, but a fixture can and a torn
    /// `/proc` read might.
    ///
    /// Not the set of processes a stop kills: the kill acts on the process
    /// group, which diverges from the ppid tree in both directions. Anything
    /// rendering this list owes the operator that caveat.
    pub(crate) fn lambs_of(&self, index: &LambIndex, root_pid: u32) -> Vec<Lamb> {
        // `visited` seeded with the root, which does two things at once: it
        // keeps the sheep out of its own lamb list, and it terminates a
        // cycle that leads back to it.
        let mut visited: HashSet<u32> = HashSet::from([root_pid]);
        let mut stack = vec![root_pid];
        let mut lambs = Vec::new();
        while let Some(pid) = stack.pop() {
            for child in index.children_of.get(&pid).into_iter().flatten() {
                if !visited.insert(*child) {
                    continue;
                }
                if let Some(entry) = index.by_pid.get(child) {
                    lambs.push(Lamb::new(entry.pid, entry.name.clone()));
                }
                stack.push(*child);
            }
        }
        lambs.sort_unstable_by_key(|lamb| lamb.pid);
        lambs
    }
}

impl fmt::Debug for StatsState {
    // The sampler is a trait object and is not Debug. The two maps are
    // behind locks this does not take, since a formatter called from inside
    // a locked section would deadlock the thread it was meant to debug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatsState")
            .field("sampler", &"<dyn MemorySampler>")
            .finish_non_exhaustive()
    }
}

/// `baseline`'s counter against a later reading of the same tree, as a
/// percentage of one core.
///
/// `None` when no wall time has passed since the baseline: dividing by a
/// zero window would produce a nonsense figure.
fn cpu_percent(baseline: Baseline, cpu_ms: u64, now: Instant) -> Option<f32> {
    let window = now.saturating_duration_since(baseline.at);
    if window.is_zero() {
        return None;
    }
    // Saturating: a counter that went backwards means the tree under this
    // pid is not the one the baseline was taken from (a lamb exited, or the
    // pid was recycled), and zero is the honest reading for that window.
    let elapsed_cpu_ms = cpu_ms.saturating_sub(baseline.cpu_ms);
    // CPU-milliseconds over wall-seconds is per-mille of one core. Computed
    // in f64 and narrowed once at the end, since f32 would lose milliseconds
    // off a counter that has run for a month.
    let percent = elapsed_cpu_ms as f64 / window.as_secs_f64() / 10.0;
    Some(percent as f32)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::super::MEMORY_POLL_INTERVAL;
    use super::super::sample::ProcessRss;
    use super::*;
    use crate::testing::{ScriptedSampler, identity};

    fn rss_cpu(pid: u32, parent: Option<u32>, bytes: u64, cpu_ms: u64) -> ProcessRss {
        ProcessRss {
            pid,
            parent,
            bytes,
            cpu_ms,
        }
    }

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

    /// 1500 CPU-ms over a 15 s window is 10%; the process's whole
    /// accumulated time instead would read 16.7%.
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

    /// An on-demand read must not write the baseline, or a second call a
    /// moment later would divide a near-zero delta by a near-zero window.
    #[tokio::test(start_paused = true)]
    async fn a_second_read_a_moment_later_still_measures_from_the_periodic_baseline() {
        // Three readings: baseline, then two on-demand reads a millisecond
        // apart, where the CPU counter barely moves. A baseline-writing
        // implementation would divide ~1 CPU-ms by ~1 ms here.
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

    // Pins the hand-rolled Debug output so a later edit cannot start
    // printing the maps or the sampler.
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

    // extras.rs's own case asserts on this helper; a helper that lied would
    // take that case with it too.
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

    #[test]
    fn the_lamb_walk_excludes_the_sheeps_own_pid() {
        let table = vec![
            identity(100, None, "srv"),
            identity(101, Some(100), "node"),
            identity(102, Some(101), "sh"),
        ];
        let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));

        let lambs = stats.lambs_of(&stats.lamb_index(), 100);

        assert_eq!(
            lambs,
            vec![Lamb::new(101, "node"), Lamb::new(102, "sh")],
            "the root pid must not appear among its own lambs"
        );
    }

    /// A `sh` wrapper execing a runtime that forks workers is three deep,
    /// the ordinary case.
    #[test]
    fn the_lamb_walk_reaches_every_generation() {
        let table = vec![
            identity(100, None, "sh"),
            identity(101, Some(100), "node"),
            identity(102, Some(101), "node"),
            identity(103, Some(102), "node"),
        ];
        let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));
        assert_eq!(stats.lambs_of(&stats.lamb_index(), 100).len(), 3);
    }

    /// Both roots are walked off one index, as `shep describe all` does:
    /// index built once, `lambs_of` called per row.
    #[test]
    fn a_sibling_subtree_is_not_this_sheeps() {
        let table = vec![
            identity(100, None, "srv"),
            identity(101, Some(100), "mine"),
            identity(200, None, "srv"),
            identity(201, Some(200), "theirs"),
        ];
        let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));
        let index = stats.lamb_index();
        assert_eq!(stats.lambs_of(&index, 100), vec![Lamb::new(101, "mine")]);
        assert_eq!(stats.lambs_of(&index, 200), vec![Lamb::new(201, "theirs")]);
    }

    /// `lambs_of` is a synchronous `fn`, so a `tokio::time::timeout` wrapper
    /// around it never gets a chance to fire: the call runs to completion or
    /// hangs on its first poll either way. The assertion below is what
    /// actually catches a non-terminating walk.
    #[test]
    fn a_parent_link_cycle_terminates_and_reports_each_pid_once() {
        let table = vec![identity(100, Some(101), "a"), identity(101, Some(100), "b")];
        let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));

        let lambs = stats.lambs_of(&stats.lamb_index(), 100);

        assert_eq!(
            lambs,
            vec![Lamb::new(101, "b")],
            "the root is its own descendant through the cycle and must not be listed"
        );
    }

    #[test]
    fn lambs_come_back_in_pid_order() {
        let table = vec![
            identity(100, None, "srv"),
            identity(103, Some(100), "c"),
            identity(101, Some(100), "a"),
            identity(102, Some(100), "b"),
        ];
        let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));
        assert_eq!(
            stats
                .lambs_of(&stats.lamb_index(), 100)
                .iter()
                .map(|l| l.pid)
                .collect::<Vec<_>>(),
            vec![101, 102, 103]
        );
    }

    /// The default `identify` returns nothing; consumers must read that as
    /// "unknown", never "no lambs".
    #[test]
    fn a_sampler_that_cannot_identify_reports_no_lambs() {
        // `ScriptedSampler::new(..)` implements only `sample`, taking the
        // default `identify`.
        let stats = StatsState::new(Arc::new(ScriptedSampler::new(vec![vec![]])));
        assert!(stats.lambs_of(&stats.lamb_index(), 100).is_empty());
    }
}
