// One crate-root fixture module: every test module in this crate shares these
// helpers instead of hand-rolling its own.
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use shep_core::config::{AppConfig, ProbeConfig, ProbeKind, ProbeTarget, ResolvedApp, normalize};
use shep_core::paths::ShepPaths;
use shep_core::status::ProcStatus;
use shep_core::values::{MemSize, UpDuration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

use crate::assemble::assemble;
use crate::bus::SharedEvent;
use crate::cron::{Clock, DEFAULT_MAX_CRON_SLEEP};
use crate::entry::{ProcessEntry, ReloadState, RestartBudget};
use crate::extras::{Extras, ExtrasReports, LivenessReport};
use crate::fake::{FIRST_SCRIPTED_PID, ProcScript, ScriptedRunner};
use crate::limits::sample::{MemorySampler, ProcessIdentity, ProcessRss};
use crate::limits::stats::StatsState;
use crate::limits::{LimitBreach, LimitEnforcer};
use crate::privilege::SpawnIdentity;
use crate::probes::{ProbeFailure, Prober};
use crate::rpc::RpcContext;
use crate::runner::{ProcIo, ProcessRunner, RunnerError, SpawnSpec};
use crate::snapshot::FlockRegistry;
use crate::supervisor::SupervisorBuilder;

// A hand-rolled `MakeWriter` over one shared buffer, not
// `fmt::layer().with_test_writer()`: the test writer hands its output to
// libtest's capture, where the test itself cannot read it back.
#[derive(Debug, Clone, Default)]
pub(crate) struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// Everything rendered into this capture so far, as one string.
    fn rendered(&self) -> String {
        let buffer = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A second [`tracing::Dispatch`], kept alive for the life of the process, so
/// a callsite's cached `Interest` is a union over every registered dispatcher.
///
/// `tracing` caches each callsite's `Interest` process-wide on first reach.
/// While `Dispatchers::has_just_one` holds, that value is read off whichever
/// thread registered first, which in a test binary is routinely a sibling with
/// no subscriber: the callsite caches `Interest::never()` and [`capture_logs`]
/// comes back empty. A second live dispatcher makes the union
/// `Interest::sometimes()`, which routes every event through a per-thread
/// `enabled()`. [`tracing::Dispatch::none`] will not do: it never registers
/// itself, so it does not count towards the flag.
static SECOND_DISPATCH: LazyLock<tracing::Dispatch> =
    LazyLock::new(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));

/// Runs `f` with a subscriber scoped to THIS thread, returning everything the
/// records it wrote rendered to.
///
/// Scoped rather than global, since a global subscriber installs once per
/// process. `f` must be synchronous and stay on this thread: a record written
/// by a `tokio::spawn`ed task carries no thread-local dispatcher and is not
/// captured. Forcing [`SECOND_DISPATCH`] first is load-bearing; see its doc.
/// ANSI is off so assertions match text, and the level is `TRACE`.
pub(crate) fn capture_logs(f: impl FnOnce()) -> String {
    LazyLock::force(&SECOND_DISPATCH);
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    capture.rendered()
}

/// The one `warn!` [`a_sibling_thread_reaching_a_callsite_first_cannot_empty_the_capture`]
/// races over, in its own helper so exactly two threads share one callsite.
fn racing_warn() {
    tracing::warn!("a callsite two threads reach");
}

/// fails if [`SECOND_DISPATCH`] stops being forced.
///
/// The two channels put the sibling's registration inside the capture's scope
/// on every run: the sibling waits to be told the scope is open, and the
/// capture waits to be told registration is done. Under the full binary's
/// parallelism another live capture can make `has_just_one` false anyway, so
/// the negative control has to be run with `--exact`.
#[test]
fn a_sibling_thread_reaching_a_callsite_first_cannot_empty_the_capture() {
    let (scope_open, await_scope) = std::sync::mpsc::channel();
    let (registered, await_registration) = std::sync::mpsc::channel();

    let sibling = std::thread::spawn(move || {
        await_scope.recv().expect("the capture must open its scope");
        // No subscriber on this thread: this is the registration that decides
        // the callsite's cached `Interest` for the whole process.
        racing_warn();
        registered
            .send(())
            .expect("the capture must still be waiting");
    });

    let rendered = capture_logs(|| {
        scope_open
            .send(())
            .expect("the sibling must still be waiting");
        await_registration
            .recv()
            .expect("the sibling must register before this emit");
        racing_warn();
    });

    sibling.join().expect("the sibling thread must not panic");
    assert!(
        rendered.contains("a callsite two threads reach"),
        "a sibling thread registering first must not disable the callsite for \
         this capture: {rendered:?}"
    );
}

// The tempdir root is `$SHEP_HOME` with no extra nesting: `sun_path` caps a
// socket path at 104 bytes on macOS, and macOS temp paths are already long.
pub(crate) fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
    let home = dir.path().to_path_buf();
    ShepPaths::resolve(
        &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
        std::path::Path::new("/nonexistent"),
    )
}

/// A [`ScriptedRunner`] a test can still read after the engine has taken
/// ownership of it. [`ProcessRunner::spawn`] takes `&self`, so sharing one
/// costs nothing but this forwarding impl.
///
/// The supervisor's tests and `boot`'s both hand a runner away and then assert
/// on its counters.
#[derive(Debug)]
pub(crate) struct SharedRunner(pub(crate) Arc<ScriptedRunner>);

impl ProcessRunner for SharedRunner {
    type Proc = crate::fake::FakeProc;

    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        self.0.spawn(spec)
    }
}

/// A [`ProcessRunner`] that announces each spawn on a unix datagram socket
/// before delegating, so a test can assert on the ORDER of two events rather
/// than on the presence of one.
///
/// For `boot`'s readiness-ordering case: the announcement and `boot`'s own
/// `READY=1` land on one socket, and AF_UNIX `SOCK_DGRAM` enqueues
/// synchronously, so the queue order read back is the program order. Unix
/// only.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct AnnouncingRunner<R> {
    inner: R,
    target: PathBuf,
}

#[cfg(unix)]
impl<R> AnnouncingRunner<R> {
    /// Wraps `inner`, announcing every spawn to the socket bound at `target`.
    pub(crate) fn new(inner: R, target: &Path) -> Self {
        Self {
            inner,
            target: target.to_path_buf(),
        }
    }
}

#[cfg(unix)]
impl<R: ProcessRunner> ProcessRunner for AnnouncingRunner<R> {
    type Proc = R::Proc;

    /// # Panics
    ///
    /// If the announcement cannot be sent, because the socket the test bound
    /// is gone or was never bound. Swallowing it would report a timeout
    /// instead of the real fault.
    #[track_caller]
    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        let socket = std::os::unix::net::UnixDatagram::unbound()
            .expect("a test must be able to open an unbound datagram socket");
        socket
            .send_to(b"SPAWNED\n", &self.target)
            .expect("the test's listening socket must be bound before the spawn");
        self.inner.spawn(spec)
    }
}

/// A proptest configuration running `local_cases` by default, and whatever
/// `PROPTEST_CASES` names when the environment sets it.
///
/// `Config::default()` already reads `PROPTEST_CASES`, but a struct-update
/// literal that then writes `cases:` overwrites what it read. Deferring to the
/// default whenever the variable is set keeps both: a source-tuned count
/// locally, an environment-set ceiling in CI.
pub(crate) fn proptest_config(local_cases: u32) -> proptest::test_runner::Config {
    let default = proptest::test_runner::Config::default();
    if std::env::var_os("PROPTEST_CASES").is_some() {
        default
    } else {
        proptest::test_runner::Config {
            cases: local_cases,
            ..default
        }
    }
}

/// Creates `root.join(rel)`, making its parent directories first, and writes
/// one byte so the file actually exists on disk. Returns the absolute path
/// written.
///
/// One byte, because watch tests care that a create or modify event fires and
/// never about the contents.
///
/// # Errors
///
/// Whatever `std::fs::create_dir_all` or `std::fs::write` returns.
pub(crate) fn touch(root: &Path, rel: &str) -> std::io::Result<PathBuf> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, b"x")?;
    Ok(path)
}

// The dispatch tests and the connection server's need the same fixture.
pub(crate) struct Harness {
    pub(crate) ctx: RpcContext,
    // Kept alive only: dropping the tempdir would remove the paths `ctx`
    // still points at.
    _dir: tempfile::TempDir,
    // Kept alive only: dropping the sender's last receiver would turn
    // every future `events.send()` into a silent no-op.
    _events_rx: broadcast::Receiver<SharedEvent>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    /// Breach reports the supervisor's extras produce, for tests that assert
    /// a memory limit fired.
    pub(crate) breaches: mpsc::Receiver<LimitBreach>,
    /// Liveness-failure reports, for tests that assert a probe threshold
    /// tripped.
    pub(crate) liveness: mpsc::Receiver<LivenessReport>,
    /// The same [`StatsState`] the extras and [`Self::ctx`] share, so a test
    /// can plant the periodic CPU baseline an on-demand reading measures
    /// against instead of waiting a poll interval for the enforcer's own
    /// tick to plant one.
    pub(crate) stats: Arc<StatsState>,
}

/// Capacity of the harness's two report channels. A test needing more unread
/// reports than this is asserting on something else.
const HARNESS_REPORT_CAPACITY: usize = 16;

/// Builds one supervisor engine (a [`ScriptedRunner`] replaying `scripts`)
/// plus a fresh [`RpcContext`] wired to it, with neutral lifecycle extras: a
/// harness nobody configured arms nothing and reports nothing.
pub(crate) fn harness(scripts: Vec<ProcScript>) -> Harness {
    // A machine with no visible processes, so nothing breaches. Not
    // `ScriptedSampler::new(vec![])`, which the constructor asserts on: the
    // neutral value is one reading holding an empty table.
    harness_sampling(scripts, vec![vec![]])
}

/// [`harness`], over a process table that really holds the first sheep's
/// tree.
///
/// The pid it describes is the one [`ScriptedRunner`] hands the first spawn,
/// so a sheep started through this harness joins against a real reading. Every
/// reading reports the same 4 KiB tree and a CPU counter that advances 1500 ms
/// between the first two, enough to baseline off one and measure the next.
pub(crate) fn harness_with_stats(scripts: Vec<ProcScript>) -> Harness {
    let tree = |cpu_ms| {
        vec![ProcessRss {
            pid: FIRST_SCRIPTED_PID,
            parent: None,
            bytes: SCRIPTED_TREE_BYTES,
            cpu_ms,
        }]
    };
    harness_sampling(scripts, vec![tree(1_000), tree(2_500), tree(2_501)])
}

/// Resident size every reading [`harness_with_stats`] scripts reports, small
/// and distinctive so a test asserting on it cannot be reading a default.
pub(crate) const SCRIPTED_TREE_BYTES: u64 = 4096;

/// [`harness`], with every spawn reporting `pid` rather than a number
/// derived from spawn order.
///
/// For a test whose scripted proc has to be the same process as a real
/// socket's peer; see [`ScriptedRunner::spawning_at`].
pub(crate) fn harness_at_pid(scripts: Vec<ProcScript>, pid: u32) -> Harness {
    harness_sampling_with(ScriptedRunner::new(scripts).spawning_at(pid), vec![vec![]])
}

/// [`harness`] over a scripted process table, the body both spellings share.
fn harness_sampling(scripts: Vec<ProcScript>, readings: Vec<Vec<ProcessRss>>) -> Harness {
    harness_sampling_with(ScriptedRunner::new(scripts), readings)
}

/// [`harness_sampling`], over a runner the caller built.
fn harness_sampling_with(runner: ScriptedRunner, readings: Vec<Vec<ProcessRss>>) -> Harness {
    harness_with_runner(runner, |reports| {
        let sampler: Arc<dyn MemorySampler> = Arc::new(ScriptedSampler::new(readings));
        let stats = Arc::new(StatsState::new(Arc::clone(&sampler)));
        Extras {
            clock: Arc::new(TestClock::starting_at(
                "2026-01-01T00:00:00Z"
                    .parse()
                    .expect("a valid RFC3339 timestamp"),
            )),
            enforcer: Arc::new(crate::limits::PollingEnforcer::start(
                sampler,
                reports.breaches.clone(),
                Arc::clone(&stats),
            )),
            // A fixture nobody configured behaves like a daemon nobody
            // configured.
            max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
            reports,
            stats,
        }
    })
}

/// [`harness`], over a process table that reports process identities.
///
/// The one fixture that can answer a lamb walk. [`harness_sampling`]'s sampler
/// takes the trait's default `identify`, which returns nothing, so a
/// `describe` through [`harness`] finds no lambs however the walk is written.
pub(crate) fn harness_identifying(
    scripts: Vec<ProcScript>,
    identities: Vec<ProcessIdentity>,
) -> Harness {
    harness_with_extras(scripts, |reports| {
        let sampler: Arc<dyn MemorySampler> =
            Arc::new(ScriptedSampler::identifying(vec![identities]));
        let stats = Arc::new(StatsState::new(Arc::clone(&sampler)));
        Extras {
            clock: Arc::new(TestClock::starting_at(
                "2026-01-01T00:00:00Z"
                    .parse()
                    .expect("a valid RFC3339 timestamp"),
            )),
            enforcer: Arc::new(crate::limits::PollingEnforcer::start(
                sampler,
                reports.breaches.clone(),
                Arc::clone(&stats),
            )),
            max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
            reports,
            stats,
        }
    })
}

/// One row of a scripted identity table, shared by [`harness_identifying`]
/// and `stats.rs`'s and `rpc.rs`'s test modules.
pub(crate) fn identity(pid: u32, parent: Option<u32>, name: &str) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        parent,
        name: name.to_string(),
    }
}

/// A [`StatsState`] over a machine with no visible processes.
///
/// The neutral value for a fixture that has to hand [`Extras`] a `stats` but
/// asserts nothing about resource readings. Watching still works against it,
/// since the watch map is the fixture's own bookkeeping.
pub(crate) fn idle_stats() -> Arc<StatsState> {
    Arc::new(StatsState::new(Arc::new(ScriptedSampler::new(vec![
        vec![],
    ]))))
}

/// [`harness`], with the caller deciding the extras.
///
/// Takes a builder rather than a finished [`Extras`], because the harness has
/// to own both report receivers: no reporter is spawned, so a test asserts the
/// report itself rather than racing a restart. A caller-built `Extras` already
/// carries senders whose receivers the harness could not recover, and
/// overwriting `reports` afterwards does not help, since `PollingEnforcer`
/// swallows its breach sender at construction.
pub(crate) fn harness_with_extras(
    scripts: Vec<ProcScript>,
    build_extras: impl FnOnce(ExtrasReports) -> Extras,
) -> Harness {
    harness_with_runner(ScriptedRunner::new(scripts), build_extras)
}

/// [`harness`], over a [`ScriptedRunner`] the caller built.
///
/// A `Vec<ProcScript>` cannot say anything about the runner itself: which pid
/// its spawns report, which sheep it refuses.
pub(crate) fn harness_with_runner(
    runner: ScriptedRunner,
    build_extras: impl FnOnce(ExtrasReports) -> Extras,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let (events, events_rx) = crate::bus::test_bus(256);
    let (breach_tx, breaches) = mpsc::channel(HARNESS_REPORT_CAPACITY);
    let (live_tx, liveness) = mpsc::channel(HARNESS_REPORT_CAPACITY);
    let extras = build_extras(ExtrasReports {
        breaches: breach_tx,
        liveness: live_tx,
    });
    // Taken before `extras` is moved, as `boot` takes it: the RPC layer and
    // the extras must share one state, or a listing would read a watch set
    // nothing ever wrote to.
    let stats = Arc::clone(&extras.stats);
    let supervisor = SupervisorBuilder::new(runner, paths.clone(), events.clone())
        .extras(extras)
        .spawn();
    let (shutdown, shutdown_rx) = watch::channel(false);
    Harness {
        ctx: RpcContext {
            supervisor,
            events,
            registry: FlockRegistry::new(),
            snapshot_path: paths.snapshot.clone(),
            dogs_config: paths.dogs_config.clone(),
            // The two built-in dogs, which is what a `$SHEP_HOME` with
            // nothing adopted hands a real boot. A test wanting an adopted
            // name assigns its own set over this one.
            known_dogs: crate::rpc::KnownDogs::new(
                ["metrics".to_string(), "bark".to_string()]
                    .into_iter()
                    .collect(),
            ),
            paths: paths.clone(),
            daemon_version: "0.1.0".to_string(),
            dog_refusals: crate::dogs::DogRefusals::new(),
            peer_contacts: crate::dogs::PeerContacts::new(),
            pid: 4242,
            shutdown: Arc::new(shutdown),
            stats: Arc::clone(&stats),
        },
        _dir: dir,
        _events_rx: events_rx,
        shutdown_rx,
        breaches,
        liveness,
        stats,
    }
}

/// `AppConfig::minimal(name, "./srv")` with `mutate` applied, normalized.
///
/// The one place a fixture app is built for the lifecycle-extra tests, so a
/// case that needs `cron_restart` or `watch` says only that.
///
/// # Panics
///
/// Panics if the mutated config does not normalize, which is a fixture bug at
/// the call site rather than a condition under test.
#[track_caller]
pub(crate) fn app_with(name: &str, mutate: impl FnOnce(&mut AppConfig)) -> ResolvedApp {
    let mut app = AppConfig::minimal(name, "./srv");
    mutate(&mut app);
    normalize(app).expect("the fixture app must normalize")
}

/// An `Online` [`ProcessEntry`] shaped like one the actor really registered.
///
/// Its two log paths come from [`assemble`] rather than being invented, so a
/// registry-tier test's entry cannot drift from what a spawn produces.
pub(crate) fn armed_entry(
    id: u32,
    instance: u32,
    pid: u32,
    app: ResolvedApp,
    paths: &ShepPaths,
) -> ProcessEntry {
    let spec = assemble(&app, instance, paths, None);
    ProcessEntry {
        id,
        spec: app,
        pending: None,
        pending_reidentifies: false,
        overridden: Vec::new(),
        instance,
        status: ProcStatus::Online,
        pid: Some(pid),
        restarts: 0,
        started_at: None,
        budget: RestartBudget::default(),
        reload: ReloadState::None,
        // Online, so its identity is settled: this entry stands in for one
        // the actor really registered through a `Start`.
        credentials: SpawnIdentity::Resolved(None),
        out_file: spec.out_file,
        err_file: spec.err_file,
        dog: None,
        last_exit: None,
    }
}

/// One [`LimitEnforcer::arm`] call, exactly as the registry made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArmCall {
    /// The sheep's id.
    pub(crate) id: u32,
    /// The pid the arming was made against.
    pub(crate) root_pid: u32,
    /// The ceiling it was armed with.
    pub(crate) limit: MemSize,
}

// A recording fake rather than a `PollingEnforcer` over a scripted sampler:
// the registry tests assert on the arguments an arming was made with, the pid
// above all, and a real enforcer only reports the consequence of a reading.
#[derive(Debug, Default)]
pub(crate) struct RecordingEnforcer {
    arms: Mutex<Vec<ArmCall>>,
    disarms: Mutex<Vec<u32>>,
}

impl RecordingEnforcer {
    /// Every [`LimitEnforcer::arm`] call so far, in order.
    pub(crate) fn arms(&self) -> Vec<ArmCall> {
        self.arms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every id [`LimitEnforcer::disarm`] was called with, in order.
    pub(crate) fn disarms(&self) -> Vec<u32> {
        self.disarms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl LimitEnforcer for RecordingEnforcer {
    fn arm(&self, id: u32, root_pid: u32, limit: MemSize) {
        self.arms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ArmCall {
                id,
                root_pid,
                limit,
            });
    }

    fn disarm(&self, id: u32) {
        self.disarms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(id);
    }
}

// `start_paused = true` freezes `tokio::time`, but `chrono::Utc::now()` keeps
// reading the real system clock. Deriving wall time as `epoch + elapsed` makes
// `tokio::time::advance` move both by the same amount, so a whole day of
// schedule fits in a test that takes microseconds.
pub(crate) struct TestClock {
    epoch: DateTime<Utc>,
    started: tokio::time::Instant,
    // Counts `now_utc` calls. The only observable difference between two
    // `max_sleep` values is how often the loop wakes, and on a paused clock a
    // wakeup leaves no other trace.
    reads: AtomicUsize,
}

impl TestClock {
    /// A clock that reads `epoch` at construction and advances in lockstep
    /// with `tokio::time` from there.
    pub(crate) fn starting_at(epoch: DateTime<Utc>) -> Self {
        Self {
            epoch,
            started: tokio::time::Instant::now(),
            reads: AtomicUsize::new(0),
        }
    }

    /// How many times [`Clock::now_utc`] has been called.
    pub(crate) fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

impl Clock for TestClock {
    fn now_utc(&self) -> DateTime<Utc> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        // `chrono::Duration::from_std` is fallible over its full range, but a
        // test clock cannot run long enough to overflow it, so this saturates
        // rather than panicking.
        let elapsed =
            chrono::Duration::from_std(self.started.elapsed()).unwrap_or(chrono::Duration::MAX);
        self.epoch + elapsed
    }
}

// A scripted sequence rather than one fixed table: the polling memory-limit
// enforcer's tests need the reading to change between polls, such as a tree
// that crosses its limit only on the third tick.
pub(crate) struct ScriptedSampler {
    readings: Vec<Vec<ProcessRss>>,
    calls: AtomicUsize,
    // Empty for every sampler built through `new`, which is what makes those
    // samplers behave like the trait's default `identify`: an empty table is
    // returned on every call rather than replayed against `identities`.
    identities: Vec<Vec<ProcessIdentity>>,
    identify_calls: AtomicUsize,
}

impl ScriptedSampler {
    /// A sampler that replays `readings` in order, one per [`MemorySampler::sample`]
    /// call, repeating the last reading once the script is exhausted.
    ///
    /// `identify` is left unscripted; see [`Self::identifying`] for a sampler
    /// that answers it too.
    pub(crate) fn new(readings: Vec<Vec<ProcessRss>>) -> Self {
        // A script with nothing to replay is a fixture bug: failing here beats
        // an index-out-of-bounds panic three frames away inside `sample`.
        assert!(
            !readings.is_empty(),
            "ScriptedSampler needs at least one reading to replay"
        );
        Self {
            readings,
            calls: AtomicUsize::new(0),
            identities: Vec::new(),
            identify_calls: AtomicUsize::new(0),
        }
    }

    /// A sampler that answers `identify` from `tables`, one per call, and
    /// `sample` from an empty machine.
    ///
    /// A separate constructor because the two halves are scripted
    /// independently and every caller of `new` wants the default `identify`.
    pub(crate) fn identifying(tables: Vec<Vec<ProcessIdentity>>) -> Self {
        assert!(
            !tables.is_empty(),
            "ScriptedSampler::identifying needs at least one table to replay"
        );
        Self {
            readings: vec![vec![]],
            calls: AtomicUsize::new(0),
            identities: tables,
            identify_calls: AtomicUsize::new(0),
        }
    }

    /// How many times [`MemorySampler::sample`] has been called.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl MemorySampler for ScriptedSampler {
    fn sample(&self) -> Vec<ProcessRss> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let index = call.min(self.readings.len() - 1);
        self.readings[index].clone()
    }

    fn identify(&self) -> Vec<ProcessIdentity> {
        // A sampler built through `new` has nothing scripted here, and reads
        // exactly as the trait's own default would.
        if self.identities.is_empty() {
            return Vec::new();
        }
        let call = self.identify_calls.fetch_add(1, Ordering::Relaxed);
        let index = call.min(self.identities.len() - 1);
        self.identities[index].clone()
    }
}

// A scripted sequence rather than one fixed outcome: the liveness loop's tests
// need pass/fail to change between polls. Unlike `ScriptedSampler`, an empty
// script is not a fixture bug here; `new(vec![])` means never fails, the
// neutral value for a prober nobody scripted.
pub(crate) struct ScriptedProber {
    script: Vec<Result<(), ProbeFailure>>,
    calls: AtomicUsize,
    delay: Duration,
    // The `timeout` argument of the most recent `probe()` call, in
    // milliseconds. `probe()` ignores the parameter itself, so recording it is
    // the only way a caller wiring `interval` where `timeout` belongs fails.
    last_timeout_ms: AtomicU64,
}

impl ScriptedProber {
    /// A prober that replays `script` in order, one outcome per
    /// [`Prober::probe`] call, repeating the last outcome once the script is
    /// exhausted. `script: vec![]` returns `Ok(())` forever.
    pub(crate) fn new(script: Vec<Result<(), ProbeFailure>>) -> Self {
        Self {
            script,
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            last_timeout_ms: AtomicU64::new(0),
        }
    }

    /// Every subsequent `probe()` call sleeps `delay` on the (paused) tokio
    /// clock before returning its scripted outcome.
    ///
    /// The delay is honoured even when it exceeds a `probe()` call's own
    /// `timeout`, which this fake ignores: a case reaching for `with_delay`
    /// wants a probe that passes or fails slowly, not one that times out.
    pub(crate) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// How many times [`Prober::probe`] has been called.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    /// The `timeout` argument passed to the most recently started
    /// [`Prober::probe`] call. `Duration::ZERO` before the first call.
    pub(crate) fn last_timeout(&self) -> Duration {
        Duration::from_millis(self.last_timeout_ms.load(Ordering::Relaxed))
    }
}

impl Prober for ScriptedProber {
    fn probe<'a>(
        &'a self,
        _target: &'a ProbeTarget,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProbeFailure>> + Send + 'a>> {
        Box::pin(async move {
            // Counted at call start, not completion: a count advancing only
            // after `with_delay`'s sleep would make "no further calls" and
            // "one call in flight" indistinguishable.
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            // Test timeouts are single-digit seconds, so this cast cannot
            // truncate, and a wrong value here only breaks an assertion.
            self.last_timeout_ms
                .store(timeout.as_millis() as u64, Ordering::Relaxed);
            tokio::time::sleep(self.delay).await;
            if self.script.is_empty() {
                return Ok(());
            }
            let index = call.min(self.script.len() - 1);
            self.script[index].clone()
        })
    }
}

/// Builds a `ProbeConfig` with fixture-friendly `interval`/`timeout`
/// (production defaults: 10s/5s) and `failure_threshold` at its production
/// default of 3.
pub(crate) fn probe_config(kind: ProbeKind, target: &str) -> ProbeConfig {
    ProbeConfig {
        kind,
        target: target.to_string(),
        interval: UpDuration::from_millis(10_000),
        timeout: UpDuration::from_millis(5_000),
        failure_threshold: 3,
    }
}

/// One scripted reply [`loopback_http`] serves for one accepted connection.
pub(crate) enum HttpReply {
    /// Writes a minimal `HTTP/1.1 {code} OK\r\n\r\n` status line, then closes.
    Status(u16),
    /// Writes `raw` verbatim, then closes, for a response that is not a
    /// well-formed HTTP status line.
    Raw(String),
    /// Accepts the connection and never writes a byte, the only way to reach
    /// `OsProber`'s read-side timeout: any reply resolves the read.
    Hang,
}

/// Longest request head [`loopback_http`] will read off a connection before
/// replying anyway, so a client that never sends the blank line ending its
/// headers cannot grow this fixture's buffer without bound.
const REQUEST_HEAD_CAP: usize = 8 * 1024;

/// How long [`LoopbackHttp::next_request`] waits for a request to arrive.
/// Failing there beats hanging a test binary that has no per-test deadline.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// A bound loopback HTTP fake, plus the requests it has received.
///
/// Aborts its own accept loop on drop, so a test owns the fake for exactly
/// its own scope and never has to remember the teardown.
pub(crate) struct LoopbackHttp {
    /// Where it is listening. Already bound by the time this struct exists.
    pub(crate) addr: SocketAddr,
    requests: mpsc::UnboundedReceiver<String>,
    accept_loop: tokio::task::JoinHandle<()>,
}

impl LoopbackHttp {
    /// The request head of the next connection this fake accepted, verbatim.
    ///
    /// Requests are queued as they arrive, so this may return immediately;
    /// what it never does is wait forever.
    pub(crate) async fn next_request(&mut self) -> String {
        tokio::time::timeout(REQUEST_DEADLINE, self.requests.recv())
            .await
            .expect("the fake received no request within the deadline")
            .expect("the fake's accept loop ended without sending a request")
    }
}

impl Drop for LoopbackHttp {
    fn drop(&mut self) {
        self.accept_loop.abort();
    }
}

/// Binds a loopback HTTP fake on `127.0.0.1:0`; see [`loopback_http_on`].
pub(crate) async fn loopback_http(script: Vec<HttpReply>) -> LoopbackHttp {
    loopback_http_on("127.0.0.1:0", script).await
}

/// Binds a loopback HTTP fake on `bind` and serves one scripted reply per
/// accepted connection, in order, recording each request it read.
///
/// Binds before spawning the accept loop and returns the already-bound
/// address, so a probe dialing it cannot race the bind. Restructuring this
/// into a task that binds brings that race back.
///
/// Every connection is read before it is replied to, [`HttpReply::Hang`]
/// included: the reply does not depend on the request, so recording it is the
/// only way a prober that dropped the `Host:` header would fail.
pub(crate) async fn loopback_http_on(bind: &str, script: Vec<HttpReply>) -> LoopbackHttp {
    let listener = TcpListener::bind(bind)
        .await
        .unwrap_or_else(|err| panic!("bind loopback HTTP fake on {bind}: {err}"));
    let addr = listener.local_addr().expect("read bound loopback address");
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let accept_loop = tokio::spawn(async move {
        for reply in script {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                return; // listener gone: nothing left to serve
            };
            if requests_tx
                .send(read_request_head(&mut stream).await)
                .is_err()
            {
                return; // the owning test dropped its LoopbackHttp
            }
            match reply {
                HttpReply::Status(code) => {
                    let response = format!("HTTP/1.1 {code} OK\r\n\r\n");
                    let _ = stream.write_all(response.as_bytes()).await;
                }
                HttpReply::Raw(raw) => {
                    let _ = stream.write_all(raw.as_bytes()).await;
                }
                HttpReply::Hang => {
                    // Never write, never drop `stream` early: the connection
                    // stays open until the caller's own timeout fires or this
                    // task is aborted.
                    std::future::pending::<()>().await;
                }
            }
        }
    });
    LoopbackHttp {
        addr,
        requests,
        accept_loop,
    }
}

/// Reads one request head, everything through the blank line that ends the
/// headers, giving up at EOF, a read error, or [`REQUEST_HEAD_CAP`].
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut chunk = [0_u8; 256];
    while !head.ends_with(b"\r\n\r\n") && head.len() < REQUEST_HEAD_CAP {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    // Lossy, not `expect`: what a prober writes is exactly what a test needs
    // to see, including bytes that are not UTF-8 at all.
    String::from_utf8_lossy(&head).into_owned()
}
