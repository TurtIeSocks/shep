//! The [`MemorySampler`] seam: one read of the machine's process table, and
//! the pure sum that turns it into one sheep's tree total.
//!
//! [`SysinfoSampler`] is the real implementation, backed by `sysinfo`. This
//! crate's own `#[cfg(test)]` fixture module carries a `ScriptedSampler` that
//! replays a scripted sequence of readings instead, for tests that need the
//! table to change between polls. [`tree_rss`] is the pure function both
//! feed — see the [`limits`](crate::limits) module doc for why the sum is
//! taken over the whole process tree rather than the sheep's own pid.
//!
//! Everything this module exports is public for consumers outside this crate's
//! `src` and for no other reason: the bench crate builds [`ProcessRss`] tables
//! and times [`tree_rss`] and [`SysinfoSampler`] against them, and
//! `tests/external_impls.rs` implements [`MemorySampler`] from outside.

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
    /// Cumulative since the process started, not a rate: a percentage is a
    /// delta between two readings divided by the wall time between them.
    /// Bigger than the process's wall-clock lifetime on a multi-core
    /// machine, which is why a percentage over 100 is honest rather than a
    /// bug.
    pub cpu_ms: u64,
}

/// One process's identity: who it is and whose child it is.
///
/// Separate from [`ProcessRss`] rather than a widening of it, and that is a
/// cost decision, not a taste one. `ProcessRss` is `Copy` and is the row type
/// of a whole-machine table the polling enforcer walks every
/// `MEMORY_POLL_INTERVAL` (15 seconds, private to this crate); putting a
/// `String` on it would take `Copy` away and allocate once per process on
/// the machine, per tick, for a field only `shep describe` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// The process's own pid.
    pub pid: u32,
    /// Its parent's pid, absent for the roots of the process table.
    pub parent: Option<u32>,
    /// The executable's name as the OS reports it — never its command line.
    /// See `shep_core::protocol::Lamb` for why the distinction is
    /// load-bearing.
    pub name: String,
}

/// Reads the machine's process table.
///
/// One implementation samples the real OS; the scripted one replays a fixture.
/// Sampling is synchronous on purpose: it is a bounded `/proc` walk that runs
/// on the enforcer's own task, and an `async fn` here would make the trait
/// dyn-incompatible for no gain.
pub trait MemorySampler: Send + Sync + 'static {
    /// Every process currently visible to this process's user.
    fn sample(&self) -> Vec<ProcessRss>;

    /// Every process currently visible, with the name the OS reports for it.
    ///
    /// Called on demand by `shep describe` and by nothing else — a lamb tree
    /// is an operator asking a question, not a poll. It performs its own
    /// table walk rather than sharing [`Self::sample`]'s: the two have
    /// different lifetimes (one is a 15-second tick, this is a request) and
    /// a shared table would have to be either stale for this or retained for
    /// that.
    ///
    /// For [`SysinfoSampler`] that separation is load-bearing rather than
    /// tidy, and its own `identify` says what goes wrong without it: a name
    /// read out of a retained table is the name that pid had when the table
    /// first saw it, forever.
    ///
    /// # Default implementation
    ///
    /// Returns nothing. Defaulted so that adding this to a `pub` trait did
    /// not break an out-of-tree implementor (the courtesy `#[non_exhaustive]`
    /// buys an enum — IR-20), and honest rather than convenient: a sampler
    /// that cannot report identities says so, and every consumer must read
    /// an empty answer as "unknown", never as "this sheep has no lambs".
    fn identify(&self) -> Vec<ProcessIdentity> {
        Vec::new()
    }
}

/// `MemorySampler` over sysinfo.
#[derive(Debug)]
pub struct SysinfoSampler {
    // `std::sync::Mutex`, not tokio's: the critical section is exactly one
    // synchronous syscall walk (`refresh_processes_specifics`), it is never
    // held across an `.await`, and a blocking mutex is cheaper than an async
    // one for that shape. `&self` rather than `&mut self` on `sample` is what
    // makes the mutex necessary in the first place — it lets this sampler
    // live behind an `Arc<dyn MemorySampler>` shared by the enforcer and,
    // later, by describe and the metrics dog.
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

// clippy's `new_without_default` is a `style` lint and fires under
// `-D warnings` the moment a `new()` takes no arguments — this impl exists to
// keep the gate quiet, not as decoration.
impl Default for SysinfoSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// The refresh kind [`SysinfoSampler::sample`] asks sysinfo for.
///
/// Named rather than written at the call site so a test can read back what
/// the sampler actually asks for. The behaviour the two `without`/`with`
/// calls buy is invisible on the host this is usually developed on, so an
/// assertion over the value is the only check that runs everywhere.
///
/// `.with_cpu()` is load-bearing and fails SILENTLY without it: sysinfo
/// populates `accumulated_cpu_time` only inside a `refresh_kind.cpu()`
/// branch, on every platform, so a memory-only refresh leaves the counter at
/// its `0` initial value and every percentage derived from it reads a
/// plausible, wrong `0.0`. It costs nothing extra — the macOS backend reads
/// both out of the same `proc_pidinfo` call, and the Linux one out of the
/// same `/proc/<pid>/stat` line.
///
/// `MINIMUM_CPU_UPDATE_INTERVAL` and the wait-then-refresh-twice dance
/// sysinfo documents do NOT apply: they govern `cpu_usage()`, which is a
/// rate sysinfo computes between two of its own refreshes.
/// `accumulated_cpu_time` is a counter and is correct on the first read; the
/// rate over it is `stats`' baseline subtraction, not sysinfo's.
///
/// `.without_tasks()` is load-bearing on Linux and a no-op everywhere else,
/// and `ProcessRefreshKind::nothing()` does NOT already mean it:
/// `nothing()` is `Default::default()`, and that default sets `tasks: true`
/// — the one field `nothing()` leaves on. With it on, sysinfo's Linux
/// backend walks `/proc/<pid>/task/` and inserts every THREAD into
/// `processes()` as a row of its own, parented to its process. Two things
/// then go wrong in [`tree_rss`], which cannot tell those rows from real
/// children:
///
/// - **Memory is multiplied by the thread count.** A thread's
///   `/proc/<pid>/task/<tid>/statm` reports the whole process's resident
///   set, because the threads share one address space. Summing the process
///   plus each of its threads reports `(1 + threads) × RSS`. Reported from a
///   live flock: ten instances of one app, each shown at ~610 MB for a
///   rollup of 5.9 GB, on a host whose whole memory use was 808 MB.
/// - **CPU is doubled.** A thread's `utime`/`stime` are its own, so the
///   threads sum to the process's total and the process row adds it a second
///   time.
///
/// It is also the cheaper walk: it drops one `readdir` and one `statm` read
/// per thread on the machine, every `MEMORY_POLL_INTERVAL`.
fn sample_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_memory()
        .with_cpu()
        .without_tasks()
}

/// The refresh kind [`SysinfoSampler::identify`] asks sysinfo for.
///
/// Neither `.with_memory()` nor `.with_cpu()`: this walk reads only `name()`
/// and `parent()`, and widening it would make an operator's `shep describe`
/// cost the same syscalls as the 15-second poll for figures nobody asked
/// this call for.
///
/// `.without_tasks()` for the reason [`sample_refresh_kind`] gives at
/// length, with a different symptom at this end: a thread row is parented to
/// its process, so `stats`' lamb index reads it as a child and
/// `shep describe` lists every one of a sheep's threads as a lamb.
fn identify_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().without_tasks()
}

impl MemorySampler for SysinfoSampler {
    fn sample(&self) -> Vec<ProcessRss> {
        // A poisoned lock recovers instead of propagating the panic: the
        // table underneath is sysinfo's own state, a prior `sample()` call
        // panicking mid-walk leaves nothing about it this crate could repair
        // by unwinding further, and refusing every later poll over one bad
        // reading would be the worse failure for a supervisor whose whole
        // job is staying up.
        let mut system = self.system.lock().unwrap_or_else(PoisonError::into_inner);
        // `ProcessesToUpdate::All`, not the pids already known: refreshing
        // only what this sampler has seen before could never discover a lamb
        // the sheep forked since the last poll, which is precisely the
        // process the tree sum exists to catch.
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
        // A `System` of this call's own, built here and dropped at the end of
        // it, rather than the retained one `self.system` holds for `sample`.
        // This is the whole correctness of the method, not an allocation
        // preference: sysinfo fills a process's `name` when its table first
        // sees that pid and never revises it on a later refresh. Measured on
        // this machine (macOS 25.2, sysinfo 0.38.4) against
        // `/bin/sh -c 'sleep 0.6; exec sleep 5'` — one pid throughout: a
        // retained `System` refreshed before and after the `execve` reports
        // `sh` both times, while a freshly built one reports `sleep`.
        //
        // Sharing the enforcer's table would therefore not cost `describe` a
        // stale name for one 15-second tick. It would cost it that name for
        // the daemon's whole life, for every lamb the table happened to
        // catch between its `fork` and its `execve` — which is precisely the
        // shell-wrapper shape (`#!/bin/sh` plus a backgrounded binary) that
        // most lambs have. `shep describe` would name the wrapper instead of
        // the program, and nothing short of a restart would correct it.
        //
        // The extra table is affordable because this is an operator's
        // question, not a poll: `sample` runs every 15 seconds and keeps its
        // table; this runs when someone types `shep describe`. It costs the
        // same one process-table walk either way — what is not shared is the
        // retention, and the lock, which this no longer takes at all.
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
/// a child list per pid, built once and then walked as many times as needed.
///
/// [`tree_rss`] builds one of these and walks it exactly once — the right
/// shape for a one-off sum. Summing *several* roots out of the same table
/// (the polling enforcer's per-tick loop, one walk per armed id) is the
/// wrong shape for that: rebuilding the index per id would multiply the
/// per-tick cost by flock size on top of the syscall walk. Build one
/// `TreeIndex` per table and call [`TreeIndex::sum_from`] per root instead,
/// so the index — the expensive part, since it scans the whole table — is
/// shared across every root's walk.
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
        // Indexed once so `sum_from`'s cycle-safe walk never rescans `table`
        // per node — the whole-machine table this feeds from can run to
        // hundreds of entries, and a caller summing multiple roots (the
        // polling enforcer, one root per armed id) builds this once and
        // reuses it, rather than paying this scan again per root.
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
    /// as [`Self::sum_from`] sums resident memory — a clustered app's row in
    /// `shep flock` reports the whole tree it really costs the machine, not
    /// the bookkeeping its root pid does.
    pub(crate) fn cpu_from(&self, root: u32) -> u64 {
        self.total_over(root, &self.cpu_by_pid)
    }

    /// Sums `totals` over `root` and every descendant this index knows about.
    ///
    /// One walk shared by both per-pid quantities: the tree does not change
    /// between them, and two hand-written copies of a cycle-safe traversal
    /// are two places for a fix to land in only one of.
    fn total_over(&self, root: u32, totals: &HashMap<u32, u64>) -> u64 {
        // A pid is summed the first time it is popped, never again — this is
        // what turns a self-parenting pid or a parent-link cycle (neither of
        // which the kernel produces, but a fixture can) into a terminating
        // walk instead of an infinite one, without double-counting anything
        // on the way.
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
/// A pid that appears in no reading contributes nothing; a cycle in the parent
/// links (which the kernel does not produce but a fixture can) terminates
/// rather than recursing forever.
///
/// Builds a fresh `TreeIndex` every call, so it is the right tool for a
/// one-off sum (this bench, a single test assertion) but the wrong one for
/// summing several roots out of the same table — see `TreeIndex`'s own doc.
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

    // fails if tree_rss omits the root's own reading — e.g. an
    // implementation that sums only descendants
    #[test]
    fn lone_root_sums_its_own_bytes() {
        let table = [rss(1, None, 100)];
        assert_eq!(tree_rss(&table, 1), 100);
    }

    // fails if only one child is picked up, or children are dropped entirely
    #[test]
    fn root_with_two_children_sums_all_three() {
        let table = [rss(1, None, 100), rss(2, Some(1), 20), rss(3, Some(1), 30)];
        assert_eq!(tree_rss(&table, 1), 150);
    }

    // fails if the walk stops at depth 1 (direct children only) instead of
    // following the chain to its end
    #[test]
    fn three_deep_chain_sums_every_generation() {
        let table = [rss(1, None, 100), rss(2, Some(1), 20), rss(3, Some(2), 5)];
        assert_eq!(tree_rss(&table, 1), 125);
    }

    // the case that catches the most likely wrong implementation: summing
    // every process whose parent chain is non-empty (which would count pid 3
    // and pid 4 below but drop the two roots, giving 1029), or summing the
    // whole table (1329) — both differ from the correct answer, 230, which
    // excludes the unrelated sibling subtree rooted at pid 1 entirely
    #[test]
    fn sibling_subtree_is_excluded() {
        let table = [
            rss(1, None, 100),    // an unrelated root — not an ancestor of pid 2
            rss(2, None, 200),    // the root this call actually asks about
            rss(3, Some(2), 30),  // pid 2's own child — counted
            rss(4, Some(1), 999), // under the sibling — must not be counted
        ];
        assert_eq!(tree_rss(&table, 2), 230);
    }

    // fails if a missing root panics, or is treated as an error instead of
    // an empty tree
    #[test]
    fn root_absent_from_table_sums_to_zero() {
        let table = [rss(1, None, 100)];
        assert_eq!(tree_rss(&table, 999), 0);
    }

    // fails if `cpu_from` sums only descendants, or double-counts a pid
    // reachable by two paths — the same two mutations `tree_rss`'s own cases
    // pin for memory, which cannot see a CPU-side regression. The memory
    // assertion rides along so a `cpu_from` that summed the wrong map, or a
    // `build` that filled one map from the other's field, cannot pass.
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

    // fails if a naive walk with no visited-set follows a self-parenting pid
    // forever; the assertion is on the returned sum (counted exactly once),
    // not merely on the call completing at all
    #[test]
    fn self_parenting_pid_terminates_and_counts_once() {
        let table = [rss(1, Some(1), 100)];
        assert_eq!(tree_rss(&table, 1), 100);
    }

    // the two-node version of the same trap: pid 2's parent is pid 3 and
    // pid 3's parent is pid 2, and each must still be summed exactly once
    #[test]
    fn two_cycle_terminates_and_counts_each_once() {
        let table = [rss(2, Some(3), 40), rss(3, Some(2), 60)];
        assert_eq!(tree_rss(&table, 2), 100);
    }

    // IR-10 dyn-compatibility smoke test: fails to compile the moment
    // somebody adds a generic (non-dyn-safe) method to `MemorySampler`
    #[test]
    fn memory_sampler_is_dyn_compatible() {
        let _: &dyn MemorySampler = &SysinfoSampler::new();
    }

    const _: fn() = || {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SysinfoSampler>();
    };

    // IR-33: the only claim about the real OS that is both true everywhere
    // and worth making. Sampling the current test process's own pid and
    // asserting a non-zero RSS is a smoke test of this crate's wiring
    // (refresh flags, the Pid->u32 conversion, the field mapping into
    // ProcessRss); asserting anything more specific — an exact byte count, a
    // parent pid — would be a test of sysinfo or of the OS, not of us.
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

        // The one assertion that catches a refresh missing `.with_cpu()`,
        // which is the failure mode this whole field has: sysinfo populates
        // `accumulated_cpu_time` only under a CPU refresh and otherwise
        // leaves it at `0`, so every derived percentage reads a plausible,
        // wrong `0.0` rather than erroring. Whole-table rather than own-pid
        // alone: a freshly-started test binary can honestly round to zero
        // CPU-milliseconds, while a host with hundreds of processes on it
        // cannot have every one of them at zero.
        assert!(
            table.iter().any(|p| p.cpu_ms > 0),
            "no entry in the table carried any accumulated CPU time; \
             the refresh is missing `.with_cpu()`"
        );

        // Guards the parent-pid decision itself (see limits/mod.rs's
        // deviation note): a sampler that hardcoded `parent: None`, or that
        // refreshed only `ProcessesToUpdate::Some(&[own_pid])` instead of
        // `All`, would still pass the RSS assertion above while collapsing
        // the tree to the root pid. A real table has more than one entry,
        // at least one parent link, and this process's own parent resolves
        // in-table to a tree sum bigger than this process's RSS alone.
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

    // fails if the scripted sampler ignores the call index and always
    // returns the first (or last) reading, instead of advancing through the
    // script one entry per call and then holding on the final one — the
    // polling memory-limit enforcer needs exactly this sequencing to test a
    // tree that stays under its limit for a few polls and breaches on a
    // later one, rather than on the first or every poll
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

    // fails if `calls()` is not wired to every `sample()` invocation — the
    // polling enforcer's "no breach, and exactly N polls happened" assertions
    // depend on this counting every call, not just distinct readings or
    // every other call
    #[test]
    fn scripted_sampler_calls_counts_every_invocation() {
        let sampler = crate::testing::ScriptedSampler::new(vec![vec![rss(1, None, 1)]]);
        assert_eq!(sampler.calls(), 0);
        sampler.sample();
        sampler.sample();
        sampler.sample();
        assert_eq!(sampler.calls(), 3);
    }

    // fails if the sampling refresh kind stops asking for memory, stops
    // asking for CPU, or lets `tasks` back on.
    //
    // An assertion over the value rather than over behaviour, because two of
    // the three have no observable effect on macOS, which is where this is
    // developed: sysinfo's Apple backend ignores `tasks` entirely, so a
    // regression here is invisible to every local run and shows up only as
    // wrong numbers on an operator's Linux box. The Linux cases below cover
    // the behaviour where it exists; this one covers the intent everywhere.
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

    // fails if the identify refresh kind widens to memory or CPU (an
    // operator's `shep describe` paying the poll's syscalls for figures it
    // does not read) or lets `tasks` back on (every thread of a sheep listed
    // as a lamb).
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
    /// Linux-only because the phenomenon is: sysinfo's Apple and Windows
    /// backends have no notion of a task row at all, so there is nothing to
    /// assert about them. Nothing here is slow — a thread is not a process
    /// and none of it waits on a wall clock — but note that a green local
    /// `cargo test` on macOS has not run one line of it. CI's Linux legs are
    /// what execute these.
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
        /// parked, so `/proc/self/task/` is guaranteed to hold more than one
        /// entry while the sampler walks.
        ///
        /// Each thread reports in before `body` starts — a spawn that has
        /// not reached its first instruction has no task directory yet, and
        /// a case that walked `/proc` before then would pass for the wrong
        /// reason. They park on a `recv` that only returns when their sender
        /// drops, which is the end of this function.
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

        // fails if the sampler's refresh kind lets sysinfo list this
        // process's threads as processes of their own. `tree_rss` cannot
        // tell such a row from a real child — it is parented to the process
        // exactly as a child is — and a thread's
        // `/proc/<pid>/task/<tid>/statm` reports the WHOLE process's
        // resident set, so one leaked row inflates the sheep's reported
        // memory by a full copy of its own RSS.
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

        // fails if `identify`'s refresh kind lets threads into the table.
        // Its own symptom, separate from the sum's: `stats`' lamb index
        // reads a thread row as a child of the sheep, so `shep describe`
        // lists every thread a sheep happens to be running as a lamb.
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
    /// The inner loop skips this module with `--skip ::slow::`; the full
    /// suite still runs them because nothing here is `#[ignore]`d.
    mod slow {
        // All three are read only by the `cfg(unix)` cases below, which
        // drive a real `/bin/sh` child. There is no Windows twin of those
        // yet: `tests/real_runner_windows.rs` covers real-child behaviour on
        // that platform instead, at the runner tier rather than the
        // sampler's.
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

        // fails if `identify` reads its names out of a table the sampler
        // retains between calls. sysinfo fills a process's `name` when its
        // table first sees that pid and never revises it, so a lamb first
        // observed between its `fork` and its `execve` would keep the
        // wrapper shell's name for the daemon's whole life — and the
        // `#!/bin/sh` wrapper is the commonest lamb there is, which makes
        // this "`shep describe` names the wrong program, permanently"
        // rather than "for one tick".
        //
        // One pid throughout: `sh` execs into `sleep` in place. Real elapsed
        // time is the point of the case (there is no seam that could fake an
        // `execve`), which is why it lives in `slow`.
        //
        // `#[cfg(unix)]`: both halves of what this proves are unix-only.
        // `/bin/sh` does not exist on Windows to spawn, and the phenomenon
        // under test — one pid staying alive across a `fork` and then
        // changing name in place when it `exec`s — is `fork`+`execve`
        // itself, which Windows' process model (`CreateProcess`, a new pid
        // every time) has no equivalent of. There is no portable case to
        // write here; the daemon's Windows tier has no sampler calling
        // `identify` yet either (spec §11's functional tier is unbuilt).
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
