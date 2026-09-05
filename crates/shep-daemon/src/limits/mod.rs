//! Process-tree memory limits (spec §4).
//!
//! A sheep's [`max_memory`](shep_core::config::AppConfig::max_memory) is
//! enforced against the process tree, its own pid plus every lamb it spawned,
//! not its resident set alone. [`sample::MemorySampler`] reads the process
//! table, [`sample::tree_rss`] sums one tree, and [`LimitEnforcer`] watches
//! those sums; `stats::StatsState` rides the same tick for `shep flock`. The
//! ppid-based sum only approximates the killed process group: a forked
//! orphan can leave the tree but stay in the group, and a `setsid`
//! descendant can leave the group but stay in the tree. A breach is noticed
//! at the next `MEMORY_POLL_INTERVAL`, and its restart does not count
//! against `max_restarts`.

use core::time::Duration;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;

use shep_core::values::MemSize;

pub mod sample;
pub(crate) mod stats;

use sample::{MemorySampler, TreeIndex};
use stats::StatsState;

/// How often the polling enforcer samples the process table.
///
/// 15s per spec §14.2. Sampling is cheap enough that halving the worst-case
/// breach latency costs nothing; the numbers are in
/// `benches/benches/memory_sample.rs`.
pub(crate) const MEMORY_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// A sheep whose process tree exceeded its `max_memory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LimitBreach {
    /// The sheep's id.
    pub id: u32,
    /// The pid this enforcement was armed against.
    ///
    /// A report already queued when the sheep exits and respawns names a
    /// stale pid, so the consumer can tell it from one about the process
    /// running now.
    pub root_pid: u32,
    /// What the tree summed to.
    pub observed: MemSize,
    /// The limit it exceeded.
    pub limit: MemSize,
}

/// Watches each armed sheep's process tree for memory-limit breaches.
///
/// The mechanism is absent from this contract: the cgroup-v2 implementation
/// planned for v1.1 writes `memory.max` and reads `memory.events`, and must
/// replace the polling one without the engine noticing.
///
/// Public because `tests/external_impls.rs` implements it from outside this
/// crate.
pub trait LimitEnforcer: Send + Sync + 'static {
    /// Begins enforcing `limit` against the process tree rooted at `root_pid`.
    ///
    /// Arming an already-armed id replaces the previous arming; a respawn
    /// gives the same id a new pid.
    ///
    /// A breach disarms the id it reports, so the next sampling pass cannot
    /// re-report the same over-limit tree while the restart it caused is
    /// still in flight. Re-arming, once the sheep is back online, is the
    /// caller's job. Every implementation must honour this.
    fn arm(&self, id: u32, root_pid: u32, limit: MemSize);
    /// Stops enforcing against `id`. A no-op if it was never armed.
    fn disarm(&self, id: u32);
}

/// One sheep's armed enforcement: the pid a tree is summed from, and the
/// ceiling that sum must not cross.
#[derive(Debug, Clone, Copy)]
struct Armed {
    root_pid: u32,
    limit: MemSize,
}

/// `LimitEnforcer` by periodic sampling.
#[derive(Debug)]
pub(crate) struct PollingEnforcer {
    armed: Arc<Mutex<HashMap<u32, Armed>>>,
    // Aborted on `Drop` below, the only stop mechanism this type needs.
    task: tokio::task::JoinHandle<()>,
}

impl PollingEnforcer {
    /// Starts the polling task; breaches arrive on `breaches`.
    ///
    /// The task ends when the returned value is dropped. Must be called from
    /// within a Tokio runtime context: it spawns the polling task at once.
    ///
    /// `stats` rides the same tick: this loop is the only scheduled walk of
    /// the process table, and a second loop would double a 5.77 ms walk to
    /// buy nothing.
    #[must_use]
    pub(crate) fn start(
        sampler: Arc<dyn MemorySampler>,
        breaches: mpsc::Sender<LimitBreach>,
        stats: Arc<StatsState>,
    ) -> Self {
        let armed: Arc<Mutex<HashMap<u32, Armed>>> = Arc::new(Mutex::new(HashMap::new()));
        let loop_armed = Arc::clone(&armed);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(MEMORY_POLL_INTERVAL).await;

                // One sample pass and one index build serve every armed
                // sheep. Without the hoist a tick costs O(flock × table
                // size), each id building its own index over the whole host
                // table.
                let table = sampler.sample();
                let index = TreeIndex::build(&table);

                // The sampling half, before the enforcement pass: the only
                // writer of the CPU baseline an on-demand listing measures
                // its window against.
                stats.record_baseline(&index, tokio::time::Instant::now());

                // Summed and self-disarmed in one locked section, so the
                // next tick cannot re-report the same over-limit reading.
                // The lock is held for every armed id's walk, which is what
                // `arm`/`disarm` block on.
                let mut breached = Vec::new();
                {
                    let mut guard = loop_armed.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.retain(|&id, entry| {
                        let observed = index.sum_from(entry.root_pid);
                        let over_limit = observed > entry.limit.bytes();
                        if over_limit {
                            breached.push(LimitBreach {
                                id,
                                root_pid: entry.root_pid,
                                observed: MemSize::from_bytes(observed),
                                limit: entry.limit,
                            });
                        }
                        !over_limit
                    });
                }

                // Sent one at a time, awaiting each, so a full `breaches`
                // channel backpressures every armed id's next sample pass,
                // not just the stuck one's.
                for breach in breached {
                    if breaches.send(breach).await.is_err() {
                        // No receiver left, so another tick has nothing
                        // left to do.
                        return;
                    }
                }
            }
        });
        Self { armed, task }
    }
}

impl LimitEnforcer for PollingEnforcer {
    fn arm(&self, id: u32, root_pid: u32, limit: MemSize) {
        let mut guard = self.armed.lock().unwrap_or_else(PoisonError::into_inner);
        guard.insert(id, Armed { root_pid, limit });
    }

    fn disarm(&self, id: u32) {
        let mut guard = self.armed.lock().unwrap_or_else(PoisonError::into_inner);
        guard.remove(&id);
    }
}

impl Drop for PollingEnforcer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::limits::sample::ProcessRss;
    use crate::testing::ScriptedSampler;

    /// Every case here is about the memory ceiling, so none carries CPU
    /// time.
    fn rss(pid: u32, parent: Option<u32>, bytes: u64) -> ProcessRss {
        ProcessRss {
            pid,
            parent,
            bytes,
            cpu_ms: 0,
        }
    }

    /// Generous bound on how long a test may wait for a breach on the paused
    /// tokio clock. Costs no wall-clock time: the runtime auto-advances to
    /// this deadline only if nothing else becomes ready first.
    const BREACH_WAIT: Duration = Duration::from_secs(120);

    /// Starts a [`PollingEnforcer`] and yields once before returning.
    ///
    /// A task spawned right before the clock is advanced would take its
    /// first `sleep` reading after the jump, missing the tick the jump meant
    /// to produce. Yielding lets the loop commit to that sleep first.
    async fn start_and_settle(
        sampler: Arc<dyn MemorySampler>,
        breaches: mpsc::Sender<LimitBreach>,
    ) -> PollingEnforcer {
        // A `StatsState` over the same sampler, watching nothing: the
        // sampling half runs, so a panic in it takes every case here, and it
        // costs no extra `sample()` call.
        let stats = Arc::new(StatsState::new(Arc::clone(&sampler)));
        let enforcer = PollingEnforcer::start(sampler, breaches, stats);
        tokio::task::yield_now().await;
        enforcer
    }

    /// `n` full polling ticks plus a hair: past tick `n` and everything it
    /// processed, strictly before tick `n + 1`.
    fn ticks(n: u32) -> Duration {
        MEMORY_POLL_INTERVAL * n + Duration::from_millis(1)
    }

    async fn expect_breach(rx: &mut mpsc::Receiver<LimitBreach>, id: u32) -> LimitBreach {
        match tokio::time::timeout(BREACH_WAIT, rx.recv()).await {
            Ok(Some(breach)) => breach,
            Ok(None) => panic!("breach channel closed before a breach for id {id} arrived"),
            Err(_) => panic!("timed out waiting for a breach for id {id}"),
        }
    }

    /// Waits up to `window` for a breach, panicking if one arrives.
    ///
    /// A bounded `timeout` + `recv`, not `advance` + `try_recv`:
    /// `tokio::time::advance` does not guarantee every pending timer is
    /// processed before it returns, and it under-counted `sampler.calls()` by
    /// a full tick here. `sleep`'s auto-advance is the reliable crossing.
    async fn assert_no_breach_within(rx: &mut mpsc::Receiver<LimitBreach>, window: Duration) {
        match tokio::time::timeout(window, rx.recv()).await {
            Err(_) => {} // the window elapsed with nothing arriving
            Ok(Some(breach)) => panic!("unexpected breach observed: {breach:?}"),
            Ok(None) => panic!("breach channel disconnected while checking for no breach"),
        }
    }

    /// A reading carrying CPU time, for the one case here that is about the
    /// sampling half rather than the ceiling.
    fn rss_cpu(pid: u32, parent: Option<u32>, bytes: u64, cpu_ms: u64) -> ProcessRss {
        ProcessRss {
            pid,
            parent,
            bytes,
            cpu_ms,
        }
    }

    // Nothing is armed on purpose: this is the half that has to run for a
    // sheep the enforcer was never told about. 1500 CPU-ms over the 7.5 s
    // between the tick and the read is 20%; a baseline recorded at the read
    // instead gives the same delta over 0 s.
    #[tokio::test(start_paused = true)]
    async fn one_tick_records_the_sampling_baseline_for_a_sheep_nothing_is_armed_against() {
        let sampler: Arc<dyn MemorySampler> = Arc::new(ScriptedSampler::new(vec![
            vec![rss_cpu(1, None, 100, 1_000)],
            vec![rss_cpu(1, None, 100, 2_500)],
        ]));
        let stats = Arc::new(StatsState::new(Arc::clone(&sampler)));
        stats.watch(9, 1);
        let (tx, mut rx) = mpsc::channel(1);
        let enforcer = PollingEnforcer::start(Arc::clone(&sampler), tx, Arc::clone(&stats));
        tokio::task::yield_now().await;

        // The first call lands just past tick 1; the second carries the
        // clock to 7.5 s past it without reaching tick 2.
        assert_no_breach_within(&mut rx, ticks(1)).await;
        assert_no_breach_within(&mut rx, Duration::from_millis(7_499)).await;

        let cpu = stats.sample_now()[&1]
            .cpu_percent
            .expect("the tick must have recorded a baseline");
        assert!(
            (cpu - 20.0).abs() < 0.01,
            "1500 CPU-ms over the 7.5 s since the tick is 20%, got {cpu}"
        );
        drop(enforcer);
    }

    /// Fails to compile the moment somebody adds a generic method to
    /// `LimitEnforcer`.
    #[tokio::test(start_paused = true)]
    async fn polling_enforcer_is_dyn_compatible() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 0)]]));
        let (tx, _rx) = mpsc::channel(1);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        let _: &dyn LimitEnforcer = &enforcer;
    }

    #[tokio::test(start_paused = true)]
    async fn three_ticks_under_limit_produce_no_breach_and_three_samples() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 100)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(1, 1, MemSize::from_bytes(200));

        assert_no_breach_within(&mut rx, ticks(3)).await;
        assert_eq!(
            sampler.calls(),
            3,
            "expected exactly one sample() call per elapsed tick"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn breach_arrives_on_exactly_the_third_tick() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss(1, None, 100)], // tick 1: under
            vec![rss(1, None, 100)], // tick 2: under
            vec![rss(1, None, 900)], // tick 3: over
        ]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(7, 1, MemSize::from_bytes(500));

        let start = tokio::time::Instant::now();
        assert_no_breach_within(&mut rx, ticks(2)).await; // stays strictly before tick 3
        let breach = expect_breach(&mut rx, 7).await;

        assert_eq!(breach.id, 7);
        assert_eq!(
            tokio::time::Instant::now() - start,
            MEMORY_POLL_INTERVAL * 3,
            "breach should arrive at exactly the third tick, not before or after"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn breach_observed_is_the_tree_sum_and_root_pid_is_the_armed_pid() {
        // The root alone (100 bytes) stays under the 150-byte limit; its
        // lamb (100 more) pushes the tree over it.
        let table = vec![rss(10, None, 100), rss(11, Some(10), 100)];
        let sampler = Arc::new(ScriptedSampler::new(vec![table]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(3, 10, MemSize::from_bytes(150));

        let breach = expect_breach(&mut rx, 3).await;

        assert_eq!(
            breach.root_pid, 10,
            "root_pid must be the pid arm() was given"
        );
        assert_eq!(
            breach.observed.bytes(),
            200,
            "observed must be the tree sum (100 + 100), not the root's own 100 bytes"
        );
        assert_eq!(breach.limit.bytes(), 150);
    }

    // `ScriptedSampler` repeats its last entry, so the reading stays over
    // the limit and a missing self-disarm reports again within two ticks.
    #[tokio::test(start_paused = true)]
    async fn a_breach_self_disarms_so_two_more_ticks_report_nothing() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 900)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(9, 1, MemSize::from_bytes(500));

        expect_breach(&mut rx, 9).await;
        assert_no_breach_within(&mut rx, ticks(2)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_id_over_its_own_limit_breaches() {
        let table = vec![
            rss(1, None, 300), // id 101's root: under its own (high) limit
            rss(2, None, 700), // id 102's root: over its own (lower) limit
        ];
        let sampler = Arc::new(ScriptedSampler::new(vec![table]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        // 101 (under its own limit) is armed first: an enforcer using only
        // the first-armed limit would compare 102's 700 bytes against 101's
        // 1000 and never breach.
        enforcer.arm(101, 1, MemSize::from_bytes(1000));
        enforcer.arm(102, 2, MemSize::from_bytes(500));

        let breach = expect_breach(&mut rx, 102).await;

        assert_eq!(
            breach.id, 102,
            "only the id over its own limit should breach"
        );
        // 101's reading never changes, so one more tick shows 102's breach
        // was not a mislabeled report meant for 101.
        assert_no_breach_within(&mut rx, ticks(1)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn disarm_before_the_next_tick_produces_no_breach() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 900)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(4, 1, MemSize::from_bytes(500));
        enforcer.disarm(4);

        assert_no_breach_within(&mut rx, ticks(1)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn missing_root_pid_produces_no_breach_even_against_a_zero_limit() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 100)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        // pid 999 never appears in the table.
        enforcer.arm(5, 999, MemSize::from_bytes(0));

        assert_no_breach_within(&mut rx, ticks(1)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn exactly_at_the_limit_does_not_breach_but_one_byte_over_does() {
        let sampler = Arc::new(ScriptedSampler::new(vec![
            vec![rss(1, None, 500)], // tick 1: exactly at the limit
            vec![rss(1, None, 501)], // tick 2: one byte over
        ]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(6, 1, MemSize::from_bytes(500));

        assert_no_breach_within(&mut rx, ticks(1)).await; // stays strictly before tick 2

        let breach = expect_breach(&mut rx, 6).await;
        assert_eq!(breach.observed.bytes(), 501);
    }

    // The instant is pinned because the reading repeats: an enforcer needing
    // two readings, or a first tick that only primes a baseline, still
    // delivers the breach, just a tick late.
    #[tokio::test(start_paused = true)]
    async fn a_limit_below_any_plausible_reading_breaches_on_the_very_first_tick() {
        // One page: below any reading this sampler could describe.
        let limit = MemSize::from_bytes(1);
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 4096)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(11, 1, limit);

        let start = tokio::time::Instant::now();
        let breach = expect_breach(&mut rx, 11).await;

        assert_eq!(
            tokio::time::Instant::now() - start,
            MEMORY_POLL_INTERVAL,
            "a tree already over its ceiling must breach on the first tick, not the second"
        );
        assert_eq!(breach.observed.bytes(), 4096);
        assert_eq!(breach.limit, limit);
        assert_eq!(sampler.calls(), 1, "one tick, one sample");
    }

    // The equality is reached through `tree_rss`, a root plus its lamb,
    // rather than off one reading. The sample count is the control that keeps
    // the negative from passing vacuously.
    #[tokio::test(start_paused = true)]
    async fn a_tree_summing_to_exactly_the_limit_does_not_breach() {
        // The root alone is well under; root + lamb lands exactly on it.
        let table = vec![rss(10, None, 300), rss(11, Some(10), 200)];
        let sampler = Arc::new(ScriptedSampler::new(vec![table]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(12, 10, MemSize::from_bytes(500));

        assert_no_breach_within(&mut rx, ticks(3)).await;
        assert_eq!(
            sampler.calls(),
            3,
            "expected exactly one sample() call per elapsed tick"
        );
    }

    // `ticks()` and the `Instant` assertions are all expressed as
    // `MEMORY_POLL_INTERVAL * n`, so only a literal pins the constant.
    #[test]
    fn memory_poll_interval_is_fifteen_seconds() {
        assert_eq!(MEMORY_POLL_INTERVAL, Duration::from_secs(15));
    }

    // fails if the send loop keeps only the first breach of a tick.
    #[tokio::test(start_paused = true)]
    async fn two_ids_breaching_the_same_tick_both_deliver() {
        let table = vec![rss(1, None, 900), rss(2, None, 900)];
        let sampler = Arc::new(ScriptedSampler::new(vec![table]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(21, 1, MemSize::from_bytes(500));
        enforcer.arm(22, 2, MemSize::from_bytes(500));

        let first = expect_breach(&mut rx, 21).await;
        let second = expect_breach(&mut rx, 22).await;

        let mut ids = [first.id, second.id];
        ids.sort_unstable();
        assert_eq!(
            ids,
            [21, 22],
            "both ids over their own limit on the same tick must be delivered, in either order"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rearming_an_already_armed_id_replaces_its_root_pid() {
        // pid 10 never appears in the table, so an arming that kept it sums
        // to 0 forever; pid 20 is over the limit from the first tick.
        let table = vec![rss(20, None, 900)];
        let sampler = Arc::new(ScriptedSampler::new(vec![table]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(30, 10, MemSize::from_bytes(500));
        enforcer.arm(30, 20, MemSize::from_bytes(500));

        let breach = expect_breach(&mut rx, 30).await;

        assert_eq!(
            breach.root_pid, 20,
            "re-arming the same id must replace the previous root_pid, not keep it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_enforcer_stops_further_sampling() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 100)]]));
        let (tx, _rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(40, 1, MemSize::from_bytes(1000)); // never breaches

        tokio::time::sleep(ticks(1)).await;
        let calls_before_drop = sampler.calls();
        assert!(
            calls_before_drop > 0,
            "expected at least one sample before drop"
        );

        drop(enforcer);

        tokio::time::sleep(ticks(3)).await;
        assert_eq!(
            sampler.calls(),
            calls_before_drop,
            "sample() must not be called again once the enforcer is dropped"
        );
    }
}
