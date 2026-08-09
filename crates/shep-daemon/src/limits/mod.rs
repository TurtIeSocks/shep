//! Process-tree memory limits: what gets measured, and why (spec §4).
//!
//! A sheep's [`max_memory`](shep_core::config::AppConfig::max_memory) ceiling
//! is enforced against **the process tree** — the sheep's own pid plus every
//! lamb it has spawned — never against the sheep's own resident set alone.
//! [`sample::MemorySampler`] is the seam that reads the machine's process
//! table; [`sample::tree_rss`] is the pure function that sums one sheep's
//! tree out of a reading. The polling enforcer that watches those sums for a
//! breach is a later addition to this module.
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

pub mod sample;
