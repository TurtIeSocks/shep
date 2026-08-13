//! Data the metrics dog exposes, and [`exposition::render`], the function
//! that turns it into Prometheus text.
//!
//! [`Reading`] is a snapshot, not a running total: nothing here accumulates
//! across scrapes, so there is no state to leak between requests and no
//! `Mutex` for a slow scraper to hold. The metrics dog (a later task) polls
//! `Request::ListFlock` and the daemon handshake fresh on every request and
//! builds one of these to hand to [`exposition::render`].

pub mod exposition;

use shep_core::protocol::ProcessInfo;

/// What the shepherd and the host looked like when the exposition was
/// rendered.
#[derive(Debug, Default)]
pub struct Reading {
    /// Every registered entry, sheep and dogs alike, as `ListFlock`
    /// answered.
    pub flock: Vec<ProcessInfo>,
    /// The shepherd's crate version, from the handshake rather than from a
    /// request: `HelloAck` already answered it, so asking again would be a
    /// round trip for something in hand (the same reasoning `shep ping`'s
    /// own module records).
    pub daemon_version: String,
    /// The shepherd's pid, from the same handshake.
    pub daemon_pid: u32,
    /// Host totals, `None` where the sampler could not read them.
    pub host: Option<HostReading>,
}

/// The machine the flock is running on.
///
/// Read through `sysinfo`, which is already a workspace dependency —
/// shep-daemon samples every sheep's tree with it — so naming it in
/// shep-cli's manifest adds **zero** crates to the tree when a later task
/// wires up the sampler that populates this. That wiring is not this
/// task's: `Reading`/`HostReading` are the shape [`exposition::render`]
/// consumes, and nothing here calls into `sysinfo` yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReading {
    /// Total physical memory in bytes.
    pub memory_total_bytes: u64,
    /// Memory in use, as the platform reports it.
    pub memory_used_bytes: u64,
    /// How many processes the host is running, the flock included. The
    /// number that explains a sampling walk getting slower.
    pub processes: usize,
    /// Seconds since the host booted.
    pub uptime_seconds: u64,
}
