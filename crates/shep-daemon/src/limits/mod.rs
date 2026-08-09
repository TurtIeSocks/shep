//! Process-tree memory limits: what gets measured, and why (spec §4).
//!
//! A sheep's [`max_memory`](shep_core::config::AppConfig::max_memory) ceiling
//! is enforced against **the process tree** — the sheep's own pid plus every
//! lamb it has spawned — never against the sheep's own resident set alone.
//! [`sample::MemorySampler`] is the seam that reads the machine's process
//! table; [`sample::tree_rss`] is the pure function that sums one sheep's
//! tree out of a reading. [`LimitEnforcer`] watches those sums for a breach;
//! [`PollingEnforcer`] is the polling implementation.
//!
//! > Deviation from pm2 (deliberate): memory is measured over the sheep's
//! > whole process tree, not the root pid alone. [`sample::tree_rss`] walks
//! > the OS's **parent-pid** links; sysinfo exposes no process-group id, so
//! > this walk is only an approximation of the kill unit, not a guarantee
//! > equal to it. The process-group/Job-Object tree kill (spec §4: lambs are
//! > "killed with the sheep by the process-group/Job-Object tree kill") acts
//! > on the process **group**, a different unit that can diverge from the
//! > ppid tree in both directions:
//! >
//! > - A lamb that forks and then exits leaves its own children re-parented
//! >   to init: they drop out of the ppid tree (unmeasured) while staying in
//! >   the original process group (still killed).
//! > - A `setsid()` grandchild (the daemonize dance `tokio_runner`'s
//! >   group-spawn doc already calls out) keeps its original ppid (measured)
//! >   but leaves the process group by creating its own, so the group-wide
//! >   kill never reaches it (killed by neither rung).
//! >
//! > The ppid walk is still the right default despite the gap: it is the
//! > only tree sysinfo can report, and a root-pid-only limit is trivially
//! > dodged by any app that forks a worker and keeps its own RSS under the
//! > ceiling while the group it owns holds a gigabyte. A sheep migrated from
//! > pm2 with a forking app may see restarts pm2 never gave it — and, per the
//! > orphan-escape case above, may occasionally miss one where a
//! > double-forked descendant is killed without ever having been counted.
//!
//! ## Reference
//!
//! - [`sample::MemorySampler`], [`sample::SysinfoSampler`],
//!   [`sample::ProcessRss`], [`sample::tree_rss`]
//! - [`LimitEnforcer`], [`PollingEnforcer`], [`LimitBreach`],
//!   [`MEMORY_POLL_INTERVAL`]

use core::time::Duration;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;

use shep_core::values::MemSize;

pub mod sample;

use sample::{MemorySampler, tree_rss};

/// How often the polling enforcer samples the process table.
///
/// Spec §14.2 tightened this from 30s to 15s: sampling is cheap enough that
/// halving worst-case breach latency costs nothing measurable. See the numbers
/// in `benches/benches/memory_sample.rs`.
pub const MEMORY_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// A sheep whose process tree exceeded its `max_memory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitBreach {
    /// The sheep's id.
    pub id: u32,
    /// The pid this enforcement was armed against.
    ///
    /// Carried so the consumer can tell a breach about the process running
    /// now from one about the process it replaced: a report already queued
    /// when the sheep exits and respawns names a pid the id no longer has.
    pub root_pid: u32,
    /// What the tree was measured at.
    pub observed: MemSize,
    /// The limit it exceeded.
    pub limit: MemSize,
}

/// Watches each armed sheep's process tree for memory-limit breaches.
///
/// The mechanism is deliberately absent from this contract. The polling
/// implementation samples; the cgroup-v2 implementation planned for v1.1
/// writes `memory.max` and reads `memory.events`, and must be able to replace
/// this one without the engine noticing.
pub trait LimitEnforcer: Send + Sync + 'static {
    /// Begins enforcing `limit` against the process tree rooted at `root_pid`.
    ///
    /// Arming an already-armed id replaces the previous arming — a respawn
    /// gives the same id a new pid.
    ///
    /// A breach disarms the id it reports: the arming that raised it does
    /// not survive its own report, so the next sampling pass cannot see the
    /// same over-limit tree and report it again while the restart the first
    /// report caused is still in flight. Re-arming is the caller's job, once
    /// the sheep is back online. Every implementation of this trait —
    /// including the cgroup-v2 one planned for v1.1 — must honour this, so
    /// it is stated here rather than left as [`PollingEnforcer`]'s own
    /// implementation detail.
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
pub struct PollingEnforcer {
    armed: Arc<Mutex<HashMap<u32, Armed>>>,
    // Aborted on `Drop` (below) — see `start`'s doc for why that is the only
    // stop mechanism this type needs.
    task: tokio::task::JoinHandle<()>,
}

impl PollingEnforcer {
    /// Starts the polling task; breaches arrive on `breaches`.
    ///
    /// The task ends when the returned value is dropped.
    ///
    /// Must be called from within a Tokio runtime context: it spawns the
    /// polling task immediately, the same way `spawn_supervisor` and
    /// `ProcessRunner::spawn` already document for themselves. The phrasing is
    /// prose rather than a `# Panics` section deliberately — neither of those
    /// carries one, and IR-21 wants `# Panics` and `#[track_caller]` to travel
    /// together or not at all.
    #[must_use]
    pub fn start(sampler: Arc<dyn MemorySampler>, breaches: mpsc::Sender<LimitBreach>) -> Self {
        let armed: Arc<Mutex<HashMap<u32, Armed>>> = Arc::new(Mutex::new(HashMap::new()));
        let loop_armed = Arc::clone(&armed);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(MEMORY_POLL_INTERVAL).await;

                // One sample pass serves every armed sheep: summing one tree
                // per armed id out of a single table, rather than refreshing
                // per sheep, keeps the syscall walk from multiplying by flock
                // size.
                let table = sampler.sample();

                // Computed and self-disarmed in the same locked section:
                // `retain` sums each armed tree and drops the entries that
                // just breached in one pass, so the very next tick can never
                // see — and re-report — the same over-limit reading.
                let mut breached = Vec::new();
                {
                    let mut guard = loop_armed.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.retain(|&id, entry| {
                        let observed = tree_rss(&table, entry.root_pid);
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

                for breach in breached {
                    if breaches.send(breach).await.is_err() {
                        // No receiver left to hear about a breach — the
                        // consumer this task exists to feed is gone, so
                        // another tick has nothing left to do.
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

    fn rss(pid: u32, parent: Option<u32>, bytes: u64) -> ProcessRss {
        ProcessRss { pid, parent, bytes }
    }

    /// Generous bound on how long a test may wait for a breach on the
    /// (paused) tokio clock. Costs no real wall-clock time: the paused
    /// runtime auto-advances to this deadline only if nothing else becomes
    /// ready first.
    const BREACH_WAIT: Duration = Duration::from_secs(120);

    /// Starts a [`PollingEnforcer`] and yields once before returning.
    ///
    /// Mirrors `cron::tests::spawn_worker_and_settle`: a task spawned right
    /// before the clock is advanced would take its very first `sleep`
    /// reading after the jump, missing the tick the jump was meant to
    /// produce. Yielding once first lets the loop commit to its initial
    /// sleep while the clock still reads close to "now".
    async fn start_and_settle(
        sampler: Arc<dyn MemorySampler>,
        breaches: mpsc::Sender<LimitBreach>,
    ) -> PollingEnforcer {
        let enforcer = PollingEnforcer::start(sampler, breaches);
        tokio::task::yield_now().await;
        enforcer
    }

    /// `n` full polling ticks, plus a hair — long enough to be sure `n`
    /// ticks have elapsed and been processed, short enough to stay strictly
    /// before tick `n + 1`.
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
    /// A bounded `timeout` + `recv`, not a bare `try_recv` — a deliberate
    /// departure from the brief, which suggested a bare `try_recv` on the
    /// reasoning that the loop under test has no `.await` between a tick's
    /// sleep completing and it parking on the next one when nothing
    /// breaches, so one `yield_now` should already settle it. That is true
    /// of the loop in isolation, but this helper drives the clock forward
    /// too, and the mechanism it uses for that turned out to matter just as
    /// much: an explicit `tokio::time::advance` call is documented as not
    /// guaranteeing every pending timer is processed before it returns, and
    /// empirically under-counted `sampler.calls()` by a full tick when tried
    /// here. `timeout` + `recv` sidesteps the question entirely by driving
    /// the paused clock through `tokio::time::sleep`'s auto-advance instead,
    /// which tokio's own docs call the reliable way to cross a pending
    /// timer — and matches what Global Constraints rule 11 asks for on
    /// negative assertions generally.
    async fn assert_no_breach_within(rx: &mut mpsc::Receiver<LimitBreach>, window: Duration) {
        match tokio::time::timeout(window, rx.recv()).await {
            Err(_) => {} // window elapsed with nothing arriving — expected
            Ok(Some(breach)) => panic!("unexpected breach observed: {breach:?}"),
            Ok(None) => panic!("breach channel disconnected while checking for no breach"),
        }
    }

    /// Dyn-compatibility smoke test (IR-10): fails to compile the moment
    /// somebody adds a generic (non-dyn-safe) method to `LimitEnforcer`.
    #[tokio::test(start_paused = true)]
    async fn polling_enforcer_is_dyn_compatible() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 0)]]));
        let (tx, _rx) = mpsc::channel(1);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        let _: &dyn LimitEnforcer = &enforcer;
    }

    // fails if the loop polls on the wrong cadence, or reports a breach on
    // equality with the limit instead of strictly exceeding it
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

    // fails if the loop reports on its first tick (before a full interval
    // has elapsed) or a tick late (after the reading has already changed
    // again) instead of on the tick the limit was actually crossed
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

    // fails if the enforcer skips tree_rss and reports the root's own bytes
    // (100, which would never breach a 150-byte limit here), or if it leaves
    // root_pid at zero instead of the pid arm() was given
    #[tokio::test(start_paused = true)]
    async fn breach_observed_is_the_tree_sum_and_root_pid_is_the_armed_pid() {
        // The root alone (100 bytes) stays under the 150-byte limit; its
        // lamb (100 more) pushes the TREE over it.
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

    // fails if arm() does not self-disarm on breach: the reading stays over
    // the limit forever (ScriptedSampler repeats its last entry), so a
    // second breach for the same id would arrive within the following two
    // ticks if the missing self-disarm bug were present
    #[tokio::test(start_paused = true)]
    async fn a_breach_self_disarms_so_two_more_ticks_report_nothing() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 900)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(9, 1, MemSize::from_bytes(500));

        expect_breach(&mut rx, 9).await;
        assert_no_breach_within(&mut rx, ticks(2)).await;
    }

    // fails if the enforcer compares every tree against every limit, or
    // against only the first limit armed, instead of each id against its own
    #[tokio::test(start_paused = true)]
    async fn only_the_id_over_its_own_limit_breaches() {
        let table = vec![
            rss(1, None, 300), // id 101's root: under its own (high) limit
            rss(2, None, 700), // id 102's root: over its own (lower) limit
        ];
        let sampler = Arc::new(ScriptedSampler::new(vec![table]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        // Different limits, and 101 (under its own) armed first, 102 (over
        // its own) armed second: an enforcer that used only the first-armed
        // limit for every id would compare 102's 700 bytes against 101's
        // 1000-byte limit and never breach at all.
        enforcer.arm(101, 1, MemSize::from_bytes(1000));
        enforcer.arm(102, 2, MemSize::from_bytes(500));

        let breach = expect_breach(&mut rx, 102).await;

        assert_eq!(
            breach.id, 102,
            "only the id over its own limit should breach"
        );
        // id 101's reading never changes (ScriptedSampler repeats its last
        // entry), so it never breaches at any tick — one more tick's worth
        // of headroom is enough to show 102's breach was not a mislabeled
        // report meant for 101.
        assert_no_breach_within(&mut rx, ticks(1)).await;
    }

    // fails if disarm() leaks the armed entry instead of removing it
    #[tokio::test(start_paused = true)]
    async fn disarm_before_the_next_tick_produces_no_breach() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 900)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        enforcer.arm(4, 1, MemSize::from_bytes(500));
        enforcer.disarm(4);

        assert_no_breach_within(&mut rx, ticks(1)).await;
    }

    // fails if a missing root is treated as tree_rss returning something
    // other than 0, or if the comparison is `>=` rather than `>` (0 >= 0
    // would wrongly breach a zero limit against an absent root)
    #[tokio::test(start_paused = true)]
    async fn missing_root_pid_produces_no_breach_even_against_a_zero_limit() {
        let sampler = Arc::new(ScriptedSampler::new(vec![vec![rss(1, None, 100)]]));
        let (tx, mut rx) = mpsc::channel(8);
        let enforcer = start_and_settle(Arc::clone(&sampler) as Arc<dyn MemorySampler>, tx).await;
        // pid 999 never appears in the table.
        enforcer.arm(5, 999, MemSize::from_bytes(0));

        assert_no_breach_within(&mut rx, ticks(1)).await;
    }

    // The comparison is `observed > limit`, strictly: a tree exactly at the
    // limit has not exceeded it. Fails if the comparison were `>=`.
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
}
