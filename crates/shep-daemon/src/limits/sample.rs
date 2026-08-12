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

/// Reads the machine's process table.
///
/// One implementation samples the real OS; the scripted one replays a fixture.
/// Sampling is synchronous on purpose: it is a bounded `/proc` walk that runs
/// on the enforcer's own task, and an `async fn` here would make the trait
/// dyn-incompatible for no gain.
pub trait MemorySampler: Send + Sync + 'static {
    /// Every process currently visible to this process's user.
    fn sample(&self) -> Vec<ProcessRss>;
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
        //
        // `.with_cpu()` is load-bearing and fails SILENTLY without it:
        // sysinfo populates `accumulated_cpu_time` only inside a
        // `refresh_kind.cpu()` branch, on every platform, so a
        // memory-only refresh leaves the counter at its `0` initial value
        // and every percentage derived from it reads a plausible, wrong
        // `0.0`. It costs nothing extra here — the macOS backend reads both
        // out of the same `proc_pidinfo` call, and the Linux one out of the
        // same `/proc/<pid>/stat` line.
        //
        // `MINIMUM_CPU_UPDATE_INTERVAL` and the wait-then-refresh-twice
        // dance sysinfo documents do NOT apply: they govern `cpu_usage()`,
        // which is a rate sysinfo computes between two of its own refreshes.
        // `accumulated_cpu_time` is a counter and is correct on the first
        // read; the rate over it is `stats`' baseline subtraction, not
        // sysinfo's.
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
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

    /// A reading with no CPU time on it, for the memory cases: every one of
    /// them predates `cpu_ms` and stays byte-for-byte what it was, so a
    /// regression in one of them still means what it used to mean.
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
}
