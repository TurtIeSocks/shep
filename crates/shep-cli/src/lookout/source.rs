//! What the link task reads the shepherd through, and the real implementation
//! over `shep-client`.
//!
//! Three traits rather than one concrete type, so [`super::link::run_link`] is
//! drivable with no socket at all: the tests that matter here are about a bus
//! that genuinely drops frames and a shepherd that genuinely will not answer,
//! and neither is reachable through a real connection on demand.
//!
//! **Why two source traits and not one object.** Reading the bus needs `&mut`
//! and issuing a `ListFlock` needs a shared reference, and `tokio::select!`
//! cannot hold both against one value. `crate::dog::bark` split its own pair
//! for exactly this reason and this follows it.
//!
//! **Why they are declared here and not shared with bark.** The shapes look
//! alike; the meanings differ. Bark's `EventSource::next` yields
//! `Result<BusEvent, u64>` because a dog only needs the count; this one yields
//! `Result<BusEvent, `[`Lagged`]`>` because the status bar prints the notice
//! and has to distinguish it from the shepherd's own `BusEvent::Dropped`. A
//! shared home for two six-line traits, one of which would then be generic
//! over its error type to serve both callers, is a worse trade than the
//! duplication — the repetition here is of shape, not of meaning.
//!
//! Every public item below is wired together by `super::mod`'s `lookout` — the
//! opening dial — and by `super::link::run_link`, which drives both traits
//! through the rest of a connection's life.

use core::fmt;
use core::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use shep_client::{Client, ConnectError, EventStream, Lagged, RequestError};
use shep_core::protocol::{BusEvent, ProcessInfo, Request, Response};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};

use crate::exit::ExitCode;

/// The topics lookout subscribes to.
///
/// `process.*` is what the flock table is made of; `daemon.*` carries
/// `BusEvent::Dropped` and `BusEvent::DaemonShutdown`, both of which this
/// dashboard reports rather than ignores.
///
/// **Not `log.*`, and that is now a shipped decision rather than a deferral.**
/// The bleats feed reads the selected sheep's log files from disk
/// ([`super::tail`]) rather than subscribing. Subscribing would make lookout
/// the highest-volume subscriber on the bus, and `super::link::run_connected`
/// answers a lag or a drop with an immediate `ListFlock` — so log traffic
/// would convert into request load on the shepherd at exactly the moment the
/// shepherd is busiest. The phase plan for 12b has the full accounting,
/// including what reading files costs instead.
pub const TOPICS: &[&str] = &["process.*", "daemon.*"];

/// Reading the flock. `&self`, so [`super::link::run_connected`] can hold it
/// across the same `select!` that holds an [`EventSource`] mutably.
pub trait FlockSource: Send + Sync {
    /// The flock as it stands.
    ///
    /// # Errors
    /// Whatever the underlying source could not answer with — for the real
    /// implementation, whatever `Request::ListFlock` failed with.
    fn flock(&self) -> impl Future<Output = Result<Vec<ProcessInfo>, RequestError>> + Send;
}

/// One source of bus frames.
pub trait EventSource: Send {
    /// The next frame; `Err(`[`Lagged`]`)` when this client's own receiver fell
    /// behind and discarded frames; `None` when the subscription ends, which
    /// is how a dead connection announces itself.
    fn next_event(&mut self) -> impl Future<Output = Option<Result<BusEvent, Lagged>>> + Send;
}

/// Opens a connection and hands back both halves of it together.
///
/// One factory rather than two independently-refreshable parameters: a
/// reconnect rebuilds the request path and the subscription at the same
/// moment, and a signature that let a caller replace one without the other
/// would admit a state the real connection cannot be in.
pub trait Shepherd: Send {
    /// This connection's request half.
    type Flock: FlockSource;
    /// This connection's subscription half.
    type Events: EventSource;

    /// Connects and subscribes.
    ///
    /// # Errors
    /// [`LinkError::Unreachable`] when the socket would not answer or the
    /// handshake failed, [`LinkError::Refused`] when it answered and then
    /// refused the subscription.
    fn link(
        &mut self,
    ) -> impl Future<Output = Result<(Self::Flock, Self::Events), LinkError>> + Send;
}

/// Why opening a connection failed.
///
/// No `#[non_exhaustive]`, and that is a decision rather than an oversight:
/// IR-20's obligation is on `pub` error enums in LIBRARY crates, and shep-cli
/// is `[[bin]]`-only — there is no downstream to break, and every match on this
/// type is in this crate. Stated here rather than left silent, which is the
/// half of IR-20 that applies either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// Nothing answered at the socket, or the handshake did not complete.
    Unreachable(String),
    /// The shepherd answered and speaks a different wire version.
    ///
    /// Held apart from [`Self::Unreachable`] for one reason: it is the single
    /// connect failure with its own exit code, and `main.rs`'s
    /// `connect_client` — the path every other client verb takes — already
    /// makes that distinction. A lookout that reported a version skew as
    /// "the shepherd did not answer" would send the operator to check whether
    /// the daemon is running, which it is.
    Protocol(String),
    /// The shepherd answered but refused the subscription.
    Refused(String),
}

impl LinkError {
    /// The exit code this reports when it happens on the FIRST dial, before
    /// the dashboard exists.
    ///
    /// Only the first dial reaches this: once a link has been established, a
    /// failure is a rung on [`super::link::run_link`]'s ladder and never an
    /// exit. Derived from `ExitCode::from(&ConnectError)` at conversion time
    /// rather than re-decided here, so this and every other verb's mapping
    /// cannot drift apart.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Protocol(_) => ExitCode::ProtocolMismatch,
            Self::Unreachable(_) | Self::Refused(_) => ExitCode::DaemonUnreachable,
        }
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "the shepherd did not answer: {why}"),
            Self::Protocol(why) => write!(f, "{why}"),
            Self::Refused(why) => write!(f, "the shepherd refused the subscription: {why}"),
        }
    }
}

impl core::error::Error for LinkError {}

impl From<ConnectError> for LinkError {
    fn from(err: ConnectError) -> Self {
        // `ExitCode::from(&ConnectError)` is the existing taxonomy — reused
        // rather than re-derived, so the two cannot skew.
        if ExitCode::from(&err) == ExitCode::ProtocolMismatch {
            return Self::Protocol(err.to_string());
        }
        Self::Unreachable(err.to_string())
    }
}

/// The request half of a live connection.
#[derive(Debug)]
pub struct ClientFlock(Client);

impl FlockSource for ClientFlock {
    async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
        match self.0.request(Request::ListFlock).await? {
            Response::Flock(flock) => Ok(flock),
            // `Response` is `#[non_exhaustive]`; a reply this binary does not
            // recognise is not a reason to tear the dashboard down, and the
            // next poll asks again.
            _unrecognised => Ok(Vec::new()),
        }
    }
}

impl EventSource for EventStream {
    async fn next_event(&mut self) -> Option<Result<BusEvent, Lagged>> {
        self.next().await
    }
}

/// The real thing: a socket path that can be dialled again.
#[derive(Debug)]
pub struct UnixShepherd {
    socket: PathBuf,
}

impl UnixShepherd {
    /// Watches the shepherd listening at `socket`.
    #[must_use]
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }
}

impl Shepherd for UnixShepherd {
    type Flock = ClientFlock;
    type Events = EventStream;

    async fn link(&mut self) -> Result<(Self::Flock, Self::Events), LinkError> {
        // `Client::connect`, never `connect_or_spawn`: opening a dashboard
        // must not start a shepherd, and a RECONNECT starting one would be
        // worse still — it would resurrect a supervisor the operator may have
        // just killed on purpose, from a process whose whole job is to watch.
        // `main.rs`'s own dispatch draws the same line for every verb but
        // `start` and `muster`.
        let client = Client::connect(&self.socket).await?;
        let topics = TOPICS.iter().map(|topic| (*topic).to_string()).collect();
        let stream = client
            .subscribe(topics)
            .await
            .map_err(|err| LinkError::Refused(err.to_string()))?;
        Ok((ClientFlock(client), stream))
    }
}

/// One reading of the machine this lookout is running on.
///
/// Deliberately NOT `dog::metrics::HostReading`, whose shape this overlaps.
/// That one carries a host **process count** — which costs a process-table
/// walk — and no load average; this one is the other way round, because it is
/// read on a one-second heartbeat rather than once per Prometheus scrape.
/// `source`'s own doc already records making this call for `EventSource`, in
/// these words: the repetition here is of shape, not of meaning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostSample {
    /// One-, five- and fifteen-minute load averages.
    pub load: (f64, f64, f64),
    /// How many cores that load is spread across, from
    /// `std::thread::available_parallelism`. `None` when the platform would
    /// not say — a load average with no denominator is a number nobody can
    /// read, so the strip drops the whole segment rather than guessing 1.
    pub cores: Option<usize>,
    /// Total physical memory in bytes.
    pub memory_total_bytes: u64,
    /// Memory in use, as the platform reports it.
    pub memory_used_bytes: u64,
    /// Seconds since the host booted.
    pub uptime_seconds: u64,
}

/// Everything lookout reads that does not come off the socket.
///
/// One trait rather than two, because both methods answer the same question —
/// what can this process see without asking the shepherd — and because
/// `super::run_ui` already carries two generic parameters.
///
/// `&mut self` rather than `&self`: the tail reader remembers each file's
/// length at the previous read, which is what makes the gap notice exact, and
/// `run_ui` owns this outright. That is the opposite call from
/// [`FlockSource::flock`], which is `&self` precisely because
/// [`super::link::run_connected`] holds it across a `select!` with an
/// [`EventSource`] borrowed mutably.
pub trait Local {
    /// This machine's load, memory and uptime, or `None` on a platform
    /// `sysinfo` does not support.
    fn host(&mut self) -> Option<HostSample>;

    /// The tail of one sheep's two log files. See [`super::tail`].
    fn tail(&mut self, out: Option<&Path>, err: Option<&Path>) -> super::tail::Tail;
}

/// The real one: a `sysinfo` handle and the tail reader's memory of each
/// file's length.
#[derive(Debug)]
pub struct LocalReader {
    cores: Option<usize>,
    seen: std::collections::BTreeMap<PathBuf, u64>,
}

impl LocalReader {
    /// Reads the core count once — it does not change for the life of a
    /// process — and starts with no memory of any log file.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cores: std::thread::available_parallelism()
                .ok()
                .map(NonZeroUsize::get),
            seen: std::collections::BTreeMap::new(),
        }
    }
}

/// Same as [`LocalReader::new`].
///
/// `clippy::new_without_default` is on by default and the gate denies
/// warnings: an argument-less `new` with no `Default` fails it.
/// [`super::term::RestoreGuard`] carries this impl and this sentence for the
/// same reason — the repetition is the lint's, not this module's.
impl Default for LocalReader {
    fn default() -> Self {
        Self::new()
    }
}

impl Local for LocalReader {
    fn host(&mut self) -> Option<HostSample> {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return None;
        }
        // Memory only. `.with_processes(..)` is what makes `dog::metrics`'
        // own sampler a process-table walk, and this runs every second.
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
        );
        let load = System::load_average();
        Some(HostSample {
            load: (load.one, load.five, load.fifteen),
            cores: self.cores,
            memory_total_bytes: system.total_memory(),
            memory_used_bytes: system.used_memory(),
            uptime_seconds: System::uptime(),
        })
    }

    fn tail(&mut self, out: Option<&Path>, err: Option<&Path>) -> super::tail::Tail {
        super::tail::read(&mut self.seen, out, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the host sampler starts walking the process table. That walk
    /// is what makes `dog::metrics`' own `sample_host` expensive enough that
    /// shep-daemon's memory sampler runs at fifteen seconds; this one runs on
    /// a one-second heartbeat and must stay a memory read and a load average.
    ///
    /// Asserted through the wall clock rather than by inspecting the
    /// `RefreshKind`, because the `RefreshKind` is exactly what a regression
    /// would change and asserting on it would be asserting the code says what
    /// it says.
    ///
    /// **Bound lowered from the plan's 50 ms to 2 ms** (Step 3.5's report):
    /// on this machine a memory-only sample runs ~8 µs and the mutated
    /// process-table walk ~8 ms, so 50 ms sits below neither and the
    /// mutation could not redden it. 2 ms clears the real reading by two
    /// orders of magnitude and stays four times under the walk.
    #[test]
    fn a_host_sample_is_cheap_enough_for_a_one_second_heartbeat() {
        let mut local = LocalReader::new();
        // One warm sample first: the first `System` construction pays for
        // whatever the platform caches, and this test is about the steady
        // state the heartbeat actually runs in.
        let _ = local.host();

        let started = std::time::Instant::now();
        for _ in 0..10 {
            let _ = local.host();
        }
        let each = started.elapsed() / 10;
        assert!(
            each < std::time::Duration::from_millis(2),
            "one host sample took {each:?}; the heartbeat fires every second"
        );
    }

    /// fails if the sampler starts inventing numbers on a platform sysinfo
    /// does not support. `None` is a real, expected case — `dog::metrics`'
    /// `Reading::host` says so in its own doc — and a strip rendering an
    /// unsupported platform as `0.00 load, 0 bytes` would be a lie the
    /// operator has no way to detect.
    ///
    /// Branched on `IS_SUPPORTED_SYSTEM` rather than asserting it.
    /// `sysinfo::IS_SUPPORTED_SYSTEM` is a `const bool`, and both
    /// `assert!(IS_SUPPORTED_SYSTEM)` and its negation trip
    /// `clippy::assertions_on_constants`, which is on by default and denied by
    /// the gate's `-D warnings`. The workspace carries no `allow` for it.
    #[test]
    fn an_unsupported_platform_reports_nothing_rather_than_zero() {
        let mut local = LocalReader::new();
        if sysinfo::IS_SUPPORTED_SYSTEM {
            let sample = local.host().expect("a supported platform samples");
            assert!(sample.memory_total_bytes > 0, "a supported host has memory");
            // `<`, not `<=`: see Step 3.6 — the weaker form survives a
            // mutation that reports used == total, which is the whole point of
            // running the mutation.
            assert!(sample.memory_used_bytes < sample.memory_total_bytes);
            assert!(sample.cores.is_some_and(|cores| cores >= 1));
        } else {
            assert!(
                local.host().is_none(),
                "no numbers where there is nothing to read"
            );
        }
    }

    /// fails if the load average's denominator stops coming from std.
    ///
    /// `sysinfo` can also report a CPU count, from its own cpu list, and it is
    /// the obvious thing to reach for while writing a `sysinfo` sampler — but
    /// the two can disagree (an affinity mask, a cgroup quota), and the number
    /// this strip needs is the one the load average is actually spread across.
    #[test]
    fn the_core_count_comes_from_std_and_not_from_sysinfo() {
        let mut local = LocalReader::new();
        assert_eq!(
            local.host().and_then(|sample| sample.cores),
            std::thread::available_parallelism()
                .ok()
                .map(NonZeroUsize::get),
        );
    }
}
