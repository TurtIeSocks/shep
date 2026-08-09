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
//! > whole process tree, not the root pid alone. The process-group/Job-Object
//! > tree kill is what actually ends a sheep (spec §4: lambs are "killed with
//! > the sheep by the process-group/Job-Object tree kill"), so the tree is
//! > what has to be measured — a root-pid-only limit is trivially dodged by
//! > any app that forks a worker and keeps its own RSS under the ceiling
//! > while the group it owns holds a gigabyte. A sheep migrated from pm2
//! > with a forking app may see restarts pm2 never gave it.
//!
//! ## Reference
//!
//! - [`sample::MemorySampler`], [`sample::SysinfoSampler`],
//!   [`sample::ProcessRss`], [`sample::tree_rss`]

pub mod sample;
