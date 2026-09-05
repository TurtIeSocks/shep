//! The [`MemorySampler`] seam: one read of the machine's process table, and
//! the pure sum that turns it into one sheep's tree total.
//!
//! [`SysinfoSampler`] is the real implementation. A `ScriptedSampler` test
//! fixture replays a scripted sequence of readings for tests that need the
//! table to change between polls. [`tree_rss`] is the pure function both
//! feed; see [`limits`](crate::limits) for why the sum spans the whole
//! process tree.
//!
//! Every item here is public so the bench crate and
//! `tests/external_impls.rs` can use [`ProcessRss`], [`tree_rss`],
//! [`SysinfoSampler`] and [`MemorySampler`] from outside this crate.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// One process's resident-set and CPU-time reading, with the parent link that
/// lets a caller rebuild the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRss {
    /// The process's own pid.
    pub pid: u32,
    /// Its parent's pid, absent for the roots of the process table.
    pub parent: Option<u32>,
    /// Resident set size in bytes.
    pub bytes: u64,
    /// Accumulated CPU time in CPU-milliseconds, as the OS reports it.
    ///
    /// Cumulative since process start, not a rate: a percentage is a delta
    /// between two readings over the wall time between them, and can exceed
    /// 100 on a multi-core machine.
    pub cpu_ms: u64,
}

/// One process's identity: who it is and whose child it is.
///
/// Separate from [`ProcessRss`] so that type stays `Copy`: adding this
/// `String` field would allocate once per process, every
/// `MEMORY_POLL_INTERVAL` tick, for a field only `shep describe` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// The process's own pid.
    pub pid: u32,
    /// Its parent's pid, absent for the roots of the process table.
    pub parent: Option<u32>,
    /// The executable's name as the OS reports it, not its command line.
    pub name: String,
}

/// Reads the machine's process table.
///
/// Synchronous: it is a bounded `/proc` walk on the enforcer's own task, and
/// an `async fn` here would make the trait dyn-incompatible for no gain.
pub trait MemorySampler: Send + Sync + 'static {
    /// Every process currently visible to this process's user.
    fn sample(&self) -> Vec<ProcessRss>;

    /// Every process currently visible, with the name the OS reports for it.
    ///
    /// Walks the table separately from [`Self::sample`] rather than sharing
    /// it: see [`SysinfoSampler`]'s own `identify` for why a shared table
    /// would give stale names forever.
    ///
    /// # Default implementation
    ///
    /// Empty: added to a `pub` trait without breaking an out-of-tree
    /// implementor. Callers must read an empty answer as "unknown", never
    /// as "no lambs".
    fn identify(&self) -> Vec<ProcessIdentity> {
        Vec::new()
    }
}

/// `MemorySampler` over sysinfo.
#[derive(Debug)]
pub struct SysinfoSampler {
    // std::sync::Mutex, not tokio's: the critical section is one synchronous
    // syscall walk, never held across an .await. Needed because `sample`
    // takes `&self`, so this can live behind a shared `Arc<dyn MemorySampler>`.
    system: Mutex<System>,
}

impl SysinfoSampler {
    /// A sampler holding an empty process table.
    ///
    /// Construction does no refresh: the first [`MemorySampler::sample`] call
    /// performs the first walk, so building one at boot cannot block.
    #[must_use]
    pub fn new() -> Self {
        Self {
            system: Mutex::new(System::new()),
        }
    }
}

// clippy's `new_without_default` fires under `-D warnings` for a `new()`
// that takes no arguments; this impl silences it.
impl Default for SysinfoSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// The refresh kind [`SysinfoSampler::sample`] asks sysinfo for, named so
/// a test can assert on the flags directly.
///
/// `.with_cpu()`: without it, sysinfo leaves `accumulated_cpu_time` at 0 on
/// every platform, so every derived percentage reads a plausible, wrong
/// `0.0`. It is a counter, correct on the first read, so sysinfo's
/// rate-only wait-then-refresh-twice advice does not apply here.
///
/// `.without_tasks()`: `ProcessRefreshKind::nothing()` defaults `tasks` to
/// on, and on Linux that lists each thread as a row [`tree_rss`] cannot
/// tell from a real child, multiplying memory by thread count and
/// doubling CPU.
fn sample_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_memory()
        .with_cpu()
        .without_tasks()
}

/// The refresh kind [`SysinfoSampler::identify`] asks sysinfo for.
///
/// Neither `.with_memory()` nor `.with_cpu()`: this walk reads only `name()`
/// and `parent()`, so `shep describe` does not pay the poll's syscalls for
/// figures it does not read.
///
/// `.without_tasks()`, for the reason [`sample_refresh_kind`] gives: without
/// it `stats`' lamb index would list every thread of a sheep as a lamb.
fn identify_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().without_tasks()
}

impl MemorySampler for SysinfoSampler {
    fn sample(&self) -> Vec<ProcessRss> {
        // Recovers from a poisoned lock rather than propagating the panic:
        // refusing every later poll over one bad reading is worse for a
        // supervisor that must stay up.
        let mut system = self.system.lock().unwrap_or_else(PoisonError::into_inner);
        // `ProcessesToUpdate::All`: refreshing only already-known pids would
        // miss a lamb the sheep forked since the last poll.
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, sample_refresh_kind());
        system
            .processes()
            .values()
            .map(|process| ProcessRss {
                pid: process.pid().as_u32(),
                parent: process.parent().map(sysinfo::Pid::as_u32),
                bytes: process.memory(),
                cpu_ms: process.accumulated_cpu_time(),
            })
            .collect()
    }

    fn identify(&self) -> Vec<ProcessIdentity> {
        // Fresh `System`, not `self.system`: sysinfo fixes a process's name
        // on first sight of its pid, so the enforcer's retained table would
        // give a lamb its pre-exec wrapper-shell name forever.
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, identify_refresh_kind());
        system
            .processes()
            .values()
            .map(|process| ProcessIdentity {
                pid: process.pid().as_u32(),
                parent: process.parent().map(sysinfo::Pid::as_u32),
                name: process.name().to_string_lossy().into_owned(),
            })
            .collect()
    }
}

/// Shared index over one sampled `table`: a byte total, a CPU-time total and
/// a child list per pid, built once and walked as many times as needed.
///
/// Build one per table and call [`TreeIndex::sum_from`] per root when
/// summing several roots (the polling enforcer's per-tick loop): rebuilding
/// per root would multiply the per-tick cost by flock size. [`tree_rss`]
/// builds one and walks it once, the right shape for a single sum.
#[derive(Debug)]
pub(crate) struct TreeIndex {
    bytes_by_pid: HashMap<u32, u64>,
    cpu_by_pid: HashMap<u32, u64>,
    children_of: HashMap<u32, Vec<u32>>,
}

impl TreeIndex {
    /// Indexes `table`: a byte total, a CPU-time total and a child list per
    /// pid.
    pub(crate) fn build(table: &[ProcessRss]) -> Self {
        // Indexed once so a caller summing multiple roots (the polling
        // enforcer, one root per armed id) reuses this instead of rescanning
        // the whole-machine table per root.
        let mut bytes_by_pid: HashMap<u32, u64> = HashMap::with_capacity(table.len());
        let mut cpu_by_pid: HashMap<u32, u64> = HashMap::with_capacity(table.len());
        let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
        for entry in table {
            bytes_by_pid.insert(entry.pid, entry.bytes);
            cpu_by_pid.insert(entry.pid, entry.cpu_ms);
            if let Some(parent) = entry.parent {
                children_of.entry(parent).or_default().push(entry.pid);
            }
        }
        Self {
            bytes_by_pid,
            cpu_by_pid,
            children_of,
        }
    }

    /// Sums resident memory over `root` and every descendant this index
    /// knows about.
    ///
    /// A pid absent from the index contributes nothing; a cycle in the
    /// parent links (which the kernel does not produce but a fixture can)
    /// terminates rather than recursing forever.
    pub(crate) fn sum_from(&self, root: u32) -> u64 {
        self.total_over(root, &self.bytes_by_pid)
    }

    /// Sums accumulated CPU time over `root` and every descendant, exactly
    /// as [`Self::sum_from`] sums resident memory.
    pub(crate) fn cpu_from(&self, root: u32) -> u64 {
        self.total_over(root, &self.cpu_by_pid)
    }

    /// Sums `totals` over `root` and every descendant this index knows about.
    ///
    /// Shared by both per-pid quantities so the cycle-safe walk exists once.
    fn total_over(&self, root: u32, totals: &HashMap<u32, u64>) -> u64 {
        // Summed once, on first pop: a self-parenting pid or a parent-link
        // cycle (a fixture can produce one) terminates instead of looping.
        let mut visited = HashSet::new();
        let mut stack = vec![root];
        let mut sum = 0u64;
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            sum = sum.saturating_add(totals.get(&pid).copied().unwrap_or(0));
            if let Some(children) = self.children_of.get(&pid) {
                stack.extend(children.iter().copied());
            }
        }
        sum
    }
}

/// Sums resident memory over `root` and every descendant in `table`.
///
/// A pid absent from `table` contributes nothing; a cycle in the parent
/// links terminates rather than recursing forever.
///
/// Builds a fresh `TreeIndex` every call: use `TreeIndex` directly to
/// sum several roots out of the same table.
#[must_use]
pub fn tree_rss(table: &[ProcessRss], root: u32) -> u64 {
    TreeIndex::build(table).sum_from(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading with no CPU time on it, for the memory cases.
    fn rss(pid: u32, parent: Option<u32>, bytes: u64) -> ProcessRss {
        rss_cpu(pid, parent, bytes, 0)
    }

    /// A reading carrying both quantities, for the CPU cases.
    fn rss_cpu(pid: u32, parent: Option<u32>, bytes: u64, cpu_ms: u64) -> ProcessRss {
        ProcessRss {
            pid,
            parent,
            bytes,
            cpu_ms,
        }
    }

    #[test]
    fn lone_root_sums_its_own_bytes() {
        let table = [rss(1, None, 100)];
        assert_eq!(tree_rss(&table, 1), 100);
    }

    #[test]
    fn root_with_two_children_sums_all_three() {
        let table = [rss(1, None, 100), rss(2, Some(1), 20), rss(3, Some(1), 30)];
        assert_eq!(tree_rss(&table, 1), 150);
    }

    #[test]
    fn three_deep_chain_sums_every_generation() {
        let table = [rss(1, None, 100), rss(2, Some(1), 20), rss(3, Some(2), 5)];
        assert_eq!(tree_rss(&table, 1), 125);
    }

    // Distinguishes the two likely wrong sums (1029, 1329) from the correct 230.
    #[test]
    fn sibling_subtree_is_excluded() {
        let table = [
            rss(1, None, 100),    // an unrelated root, not an ancestor of pid 2
            rss(2, None, 200),    // the root this call actually asks about
            rss(3, Some(2), 30),  // pid 2's own child, counted
            rss(4, Some(1), 999), // under the sibling, must not be counted
        ];
        assert_eq!(tree_rss(&table, 2), 230);
    }

    #[test]
    fn root_absent_from_table_sums_to_zero() {
        let table = [rss(1, None, 100)];
        assert_eq!(tree_rss(&table, 999), 0);
    }

    #[test]
    fn cpu_sums_the_whole_tree_including_the_root() {
        let table = [
            rss_cpu(100, None, 1024, 500),
            rss_cpu(101, Some(100), 2048, 250),
            rss_cpu(102, Some(101), 4096, 125),
        ];
        let index = TreeIndex::build(&table);
        assert_eq!(index.cpu_from(100), 875);
        assert_eq!(index.cpu_from(101), 375);
        assert_eq!(index.sum_from(100), 7168, "memory must be unaffected");
    }

    #[test]
    fn self_parenting_pid_terminates_and_counts_once() {
        let table = [rss(1, Some(1), 100)];
        assert_eq!(tree_rss(&table, 1), 100);
    }

    #[test]
    fn two_cycle_terminates_and_counts_each_once() {
        let table = [rss(2, Some(3), 40), rss(3, Some(2), 60)];
        assert_eq!(tree_rss(&table, 2), 100);
    }

    #[test]
    fn memory_sampler_is_dyn_compatible() {
        let _: &dyn MemorySampler = &SysinfoSampler::new();
    }

    const _: fn() = || {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SysinfoSampler>();
    };

    // Asserts only wiring (refresh flags, Pid->u32, field mapping): an exact
    // byte count or parent pid would test sysinfo, not us.
    #[test]
    fn sysinfo_sampler_finds_this_process_with_nonzero_rss() {
        let sampler = SysinfoSampler::new();
        let pid = std::process::id();
        let table = sampler.sample();
        let reading = table
            .iter()
            .copied()
            .find(|p| p.pid == pid)
            .unwrap_or_else(|| panic!("own pid {pid} not found in sampled process table"));
        assert!(reading.bytes > 0, "own RSS reading was zero");

        // Whole-table, not own-pid: a fresh test binary can honestly read
        // zero CPU-milliseconds, so this checks the table has any nonzero
        // entry instead.
        assert!(
            table.iter().any(|p| p.cpu_ms > 0),
            "no entry in the table carried any accumulated CPU time; \
             the refresh is missing `.with_cpu()`"
        );

        // Guards parent-pid wiring: a sampler that hardcoded `parent: None`
        // or refreshed only the own pid would still pass the RSS assertion
        // above while collapsing the tree to the root.
        assert!(table.len() > 1, "table had only one process in it");
        assert!(
            table.iter().any(|p| p.parent.is_some()),
            "no entry in the table carried a parent pid"
        );
        let parent = reading
            .parent
            .unwrap_or_else(|| panic!("own reading for pid {pid} carried no parent"));
        assert!(
            tree_rss(&table, parent) > reading.bytes,
            "tree sum rooted at own parent ({parent}) was not larger than this process's own RSS"
        );
    }

    #[test]
    fn scripted_sampler_replays_in_order_then_repeats_last() {
        let under = vec![rss(1, None, 100)];
        let over = vec![rss(1, None, 900)];
        let sampler = crate::testing::ScriptedSampler::new(vec![under.clone(), over.clone()]);
        assert_eq!(
            sampler.sample(),
            under,
            "call 1 should be the first reading"
        );
        assert_eq!(
            sampler.sample(),
            over,
            "call 2 should be the second reading"
        );
        assert_eq!(
            sampler.sample(),
            over,
            "call 3, past the end of the script, should repeat the last reading"
        );
    }

    #[test]
    fn scripted_sampler_calls_counts_every_invocation() {
        let sampler = crate::testing::ScriptedSampler::new(vec![vec![rss(1, None, 1)]]);
        assert_eq!(sampler.calls(), 0);
        sampler.sample();
        sampler.sample();
        sampler.sample();
        assert_eq!(sampler.calls(), 3);
    }

    // Asserts the flags directly rather than behavior: macOS ignores
    // `tasks` entirely, so a regression here would be invisible to a local
    // run and show up only on an operator's Linux box.
    #[test]
    fn the_sampling_refresh_kind_asks_for_memory_and_cpu_but_never_tasks() {
        let kind = sample_refresh_kind();
        assert!(kind.memory(), "the tree sum is a memory sum");
        assert!(
            kind.cpu(),
            "without this `accumulated_cpu_time` stays at 0 and every \
             percentage reads a plausible, wrong 0.0"
        );
        assert!(
            !kind.tasks(),
            "`tasks` makes sysinfo's Linux backend list every thread as a \
             process parented to its own process, which multiplies a sheep's \
             reported memory by its thread count"
        );
    }

    #[test]
    fn the_identify_refresh_kind_asks_for_neither_memory_nor_cpu_nor_tasks() {
        let kind = identify_refresh_kind();
        assert!(!kind.memory(), "identify reads only `name` and `parent`");
        assert!(!kind.cpu(), "identify reads only `name` and `parent`");
        assert!(
            !kind.tasks(),
            "`tasks` would make `shep describe` report every thread of a \
             sheep as one of its lambs"
        );
    }

    /// Cases that need `/proc` and a real thread of this process.
    ///
    /// Linux-only: sysinfo's Apple and Windows backends have no notion of a
    /// task row. Not slow, but a macOS `cargo test` never runs these; CI's
    /// Linux legs do.
    #[cfg(target_os = "linux")]
    mod linux {
        use std::sync::mpsc;
        use std::thread;

        use super::*;

        /// Every thread id this process currently has, straight out of
        /// `/proc/self/task/`, this thread's own included.
        fn own_thread_ids() -> Vec<u32> {
            std::fs::read_dir("/proc/self/task")
                .expect("/proc/self/task exists on every Linux this daemon runs on")
                .filter_map(|entry| {
                    entry
                        .ok()?
                        .file_name()
                        .to_str()
                        .and_then(|name| name.parse::<u32>().ok())
                })
                .collect()
        }

        /// Runs `body` with `count` extra threads of this process alive and
        /// parked, so `/proc/self/task/` holds more than one entry while the
        /// sampler walks.
        ///
        /// Each thread reports in before `body` starts, since a spawn that
        /// has not reached its first instruction has no task directory yet.
        /// They park on a `recv` that returns only when their sender drops.
        fn with_extra_threads<T>(count: usize, body: impl FnOnce() -> T) -> T {
            let (report_started, started) = mpsc::channel::<()>();
            let mut releases = Vec::with_capacity(count);
            let mut handles = Vec::with_capacity(count);
            for _ in 0..count {
                let (release, parked) = mpsc::channel::<()>();
                let report_started = report_started.clone();
                releases.push(release);
                handles.push(thread::spawn(move || {
                    report_started
                        .send(())
                        .expect("the test thread outlives every thread it parks");
                    // Returns `Err` when `releases` drops below; either way
                    // this thread is done.
                    let _ = parked.recv();
                }));
            }
            drop(report_started);
            for _ in 0..count {
                started
                    .recv()
                    .expect("every parked thread reports in before it parks");
            }

            let out = body();

            drop(releases);
            for handle in handles {
                handle.join().expect("a parked thread cannot panic");
            }
            out
        }

        // A thread row would be parented like a real child, and its
        // /proc/<pid>/task/<tid>/statm reports the whole process's RSS, so
        // a leaked row multiplies the sheep's reported memory.
        #[test]
        fn the_sample_table_holds_no_thread_of_this_process() {
            let own_pid = std::process::id();
            with_extra_threads(4, || {
                let threads: Vec<u32> = own_thread_ids()
                    .into_iter()
                    .filter(|tid| *tid != own_pid)
                    .collect();
                assert!(
                    !threads.is_empty(),
                    "the case says nothing unless this process really has \
                     threads other than its main one; it did not"
                );

                let table = SysinfoSampler::new().sample();
                let sampled: HashSet<u32> = table.iter().map(|entry| entry.pid).collect();
                let leaked: Vec<u32> = threads
                    .into_iter()
                    .filter(|tid| sampled.contains(tid))
                    .collect();

                assert!(
                    sampled.contains(&own_pid),
                    "the walk has to see this process at all, or a clean \
                     result below means only that the walk failed"
                );
                assert!(
                    leaked.is_empty(),
                    "the table must hold processes, not threads; these \
                     thread ids of pid {own_pid} were sampled as processes \
                     in their own right: {leaked:?}"
                );
            });
        }

        // A leaked thread row here would make `stats`' lamb index list it
        // as a lamb in `shep describe`.
        #[test]
        fn identify_reports_no_thread_of_this_process() {
            let own_pid = std::process::id();
            with_extra_threads(4, || {
                let threads: Vec<u32> = own_thread_ids()
                    .into_iter()
                    .filter(|tid| *tid != own_pid)
                    .collect();
                assert!(
                    !threads.is_empty(),
                    "the case says nothing unless this process really has \
                     threads other than its main one; it did not"
                );

                let identified: HashSet<u32> = SysinfoSampler::new()
                    .identify()
                    .into_iter()
                    .map(|identity| identity.pid)
                    .collect();
                let leaked: Vec<u32> = threads
                    .into_iter()
                    .filter(|tid| identified.contains(tid))
                    .collect();

                assert!(
                    identified.contains(&own_pid),
                    "the walk has to see this process at all, or a clean \
                     result below means only that the walk failed"
                );
                assert!(
                    leaked.is_empty(),
                    "a lamb is a child process, never a thread; these thread \
                     ids of pid {own_pid} were identified as processes: \
                     {leaked:?}"
                );
            });
        }
    }

    /// Tests that spawn a real process and wait on real elapsed time.
    ///
    /// Skipped by `--skip ::slow::`; nothing here is `#[ignore]`d.
    mod slow {
        // Read only by the cfg(unix) cases below. No Windows twin here;
        // tests/real_runner_windows.rs covers real-child behavior at the
        // runner tier instead.
        #[cfg(unix)]
        use std::process::Command;
        #[cfg(unix)]
        use std::time::Duration;

        #[cfg(unix)]
        use super::*;

        /// The name `sampler` reports for `pid`, or `None` if the walk did
        /// not see it.
        ///
        /// `cfg(unix)` alongside its only caller, below.
        #[cfg(unix)]
        fn name_of(sampler: &SysinfoSampler, pid: u32) -> Option<String> {
            sampler
                .identify()
                .into_iter()
                .find(|identity| identity.pid == pid)
                .map(|identity| identity.name)
        }

        // Pins `identify` reading the post-exec name, not a stale one from
        // a retained table. unix only: Windows has no fork-then-exec
        // equivalent, and real elapsed time is why this lives in `slow`.
        #[cfg(unix)]
        #[test]
        fn identify_reports_the_name_a_lamb_execed_into_not_the_one_it_forked_with() {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("sleep 0.6; exec sleep 30")
                .spawn()
                .expect("/bin/sh is present on every platform this daemon supports");
            let pid = child.id();
            let sampler = SysinfoSampler::new();

            let forked_as = name_of(&sampler, pid);
            std::thread::sleep(Duration::from_millis(1_500));
            let execed_into = name_of(&sampler, pid);

            let _ = child.kill();
            let _ = child.wait();

            assert_eq!(
                forked_as.as_deref(),
                Some("sh"),
                "the first walk has to land before the `execve` or the case \
                 says nothing; it did not"
            );
            assert_eq!(
                execed_into.as_deref(),
                Some("sleep"),
                "the second walk must report what pid {pid} is NOW, not what \
                 the first walk recorded for it"
            );
        }
    }
}
