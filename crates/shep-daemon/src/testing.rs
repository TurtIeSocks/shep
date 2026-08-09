// IR-33: one crate-root fixture module; every test module in this crate
// shares this `test_paths` helper instead of hand-rolling its own.
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use shep_core::config::{AppConfig, ProbeConfig, ProbeKind, ProbeTarget, ResolvedApp, normalize};
use shep_core::paths::ShepPaths;
use shep_core::status::ProcStatus;
use shep_core::values::{MemSize, UpDuration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};

use crate::assemble::assemble;
use crate::cron::{Clock, DEFAULT_MAX_CRON_SLEEP};
use crate::entry::{ProcessEntry, ReloadState, RestartBudget};
use crate::extras::{Extras, ExtrasReports};
use crate::fake::{ProcScript, ScriptedRunner};
use crate::limits::sample::{MemorySampler, ProcessRss};
use crate::limits::{LimitBreach, LimitEnforcer};
use crate::probes::{LivenessFailure, ProbeFailure, Prober};
use crate::rpc::RpcContext;
use crate::snapshot::FlockRegistry;
use crate::supervisor::SupervisorBuilder;

// `FD_REUSE_LOCK` lived here until 2026-08-08. It serialized the tests
// that close a real descriptor and then re-probe that same number, to
// stop them racing the kernel's lowest-available-fd allocation.
//
// It was removed because it could not work. A mutex only excludes the
// tests that TAKE it; every other test in the binary stayed free to open
// a file and be handed the just-closed number, after which `adopt_fd`'s
// `F_GETFD` probe legitimately succeeds and the adoption double-closes
// somebody else's descriptor. That is not hypothetical: it was
// reproduced WITH the lock in place, as `fatal runtime error: IO Safety
// violation: owned file descriptor already closed`, once in 25 saturated
// `--workspace --all-features` runs, taking the whole lib test binary
// down with SIGABRT.
//
// The fix is structural, not exclusive: `sys.rs`'s probe now parks on a
// high fd number (`F_DUPFD`), which the lowest-free allocation policy
// will not hand back while lower numbers remain free. See
// `a_closed_descriptor_is_refused_instead_of_adopted`.

// WHY a shallow home: later tasks bind a UDS under `run/`, and sun_path
// caps a socket path near 104 bytes. Using the tempdir root as
// $SHEP_HOME (no extra nesting) keeps every test in this crate under the
// limit on macOS, whose temp paths are already long.
pub(crate) fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
    let home = dir.path().to_path_buf();
    ShepPaths::resolve(
        &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
        std::path::Path::new("/nonexistent"),
    )
}

/// Creates `root.join(rel)`, making its parent directories first, and writes
/// one byte so the file actually exists on disk. Returns the absolute path
/// written.
///
/// One-byte writes are deliberate: watch tests care that a create/modify
/// event fires, never about the file's contents.
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

// IR-33: the dispatch tests and the connection-server's tests need the
// exact same fixture — one factory, not two.
pub(crate) struct Harness {
    pub(crate) ctx: RpcContext,
    // Kept alive only: dropping the tempdir would remove the paths `ctx`
    // still points at.
    _dir: tempfile::TempDir,
    // Kept alive only: dropping the sender's last receiver would turn
    // every future `events.send()` into a silent no-op.
    _events_rx: broadcast::Receiver<shep_core::protocol::BusEvent>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
    /// Breach reports the supervisor's extras produce, for tests that assert
    /// a memory limit fired.
    pub(crate) breaches: mpsc::Receiver<LimitBreach>,
    /// Liveness-failure reports, for tests that assert a probe threshold
    /// tripped.
    pub(crate) liveness: mpsc::Receiver<LivenessFailure>,
}

/// Capacity of the harness's two report channels — a test that needs more
/// than this many unread reports is asserting on something else.
const HARNESS_REPORT_CAPACITY: usize = 16;

/// Builds one supervisor engine (a [`ScriptedRunner`] replaying `scripts`)
/// plus a fresh [`RpcContext`] wired to it, with neutral lifecycle extras: a
/// harness nobody configured arms nothing and reports nothing.
pub(crate) fn harness(scripts: Vec<ProcScript>) -> Harness {
    harness_with_extras(scripts, |reports| Extras {
        clock: Arc::new(TestClock::starting_at(
            "2026-01-01T00:00:00Z"
                .parse()
                .expect("a valid RFC3339 timestamp"),
        )),
        // A machine with no visible processes: nothing an app arms against
        // can ever be found, so nothing breaches. NOT `ScriptedSampler::new
        // (vec![])`, which is a fixture bug the constructor asserts on — the
        // neutral value is one reading holding an empty table.
        enforcer: Arc::new(crate::limits::PollingEnforcer::start(
            Arc::new(ScriptedSampler::new(vec![vec![]])),
            reports.breaches.clone(),
        )),
        // A fixture nobody configured behaves like a daemon nobody
        // configured.
        max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
        reports,
    })
}

/// [`harness`], with the caller deciding the extras.
///
/// Takes a builder rather than a finished [`Extras`] — a deliberate departure
/// from the brief, which declared `harness_with_extras(scripts, extras)`. The
/// harness has to own both report RECEIVERS (that is the whole reason it can
/// hold them: no reporter is spawned, so a test asserts the report itself
/// rather than racing a restart it did not trigger), and a caller-built
/// `Extras` already carries senders whose receivers the harness could not
/// recover. Handing the caller the [`ExtrasReports`] the harness just made
/// keeps one owner for each half — and it is load-bearing rather than
/// cosmetic, since `PollingEnforcer` swallows its breach sender at
/// construction, so a harness that overwrote `reports` afterwards would send
/// breaches into a channel nobody reads.
pub(crate) fn harness_with_extras(
    scripts: Vec<ProcScript>,
    build_extras: impl FnOnce(ExtrasReports) -> Extras,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let (events, events_rx) = broadcast::channel(256);
    let (breach_tx, breaches) = mpsc::channel(HARNESS_REPORT_CAPACITY);
    let (live_tx, liveness) = mpsc::channel(HARNESS_REPORT_CAPACITY);
    let extras = build_extras(ExtrasReports {
        breaches: breach_tx,
        liveness: live_tx,
    });
    let supervisor =
        SupervisorBuilder::new(ScriptedRunner::new(scripts), paths.clone(), events.clone())
            .extras(extras)
            .spawn();
    let (shutdown, shutdown_rx) = watch::channel(false);
    Harness {
        ctx: RpcContext {
            supervisor,
            events,
            registry: FlockRegistry::new(),
            snapshot_path: paths.snapshot.clone(),
            daemon_version: "0.1.0".to_string(),
            pid: 4242,
            shutdown: Arc::new(shutdown),
        },
        _dir: dir,
        _events_rx: events_rx,
        shutdown_rx,
        breaches,
        liveness,
    }
}

/// `AppConfig::minimal(name, "./srv")` with `mutate` applied, normalized.
///
/// The one place a fixture app is built for the lifecycle-extra tests, so a
/// case that needs `cron_restart` or `watch` says only that (IR-33).
///
/// # Panics
///
/// Panics if the mutated config does not normalize — a fixture bug at the
/// call site that wrote it, not a condition under test.
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
        instance,
        status: ProcStatus::Online,
        pid: Some(pid),
        restarts: 0,
        started_at: None,
        budget: RestartBudget::default(),
        reload: ReloadState::None,
        credentials: None,
        out_file: spec.out_file,
        err_file: spec.err_file,
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

// WHY a recording fake rather than a `PollingEnforcer` over a scripted
// sampler: the registry tests assert on the ARGUMENTS an arming was made
// with — the pid above all, since "arms once and never updates" is the bug
// that shape exists to catch — and a real enforcer only ever reports the
// consequence of a reading, never what it was armed with.
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

// WHY a clock derived from tokio's Instant: `start_paused = true` freezes
// `tokio::time`, but `chrono::Utc::now()` keeps reading the real system clock.
// A cron test that used the real clock would have to wait real hours. Deriving
// wall time as `epoch + elapsed-since-construction` means `tokio::time::advance`
// moves both clocks by the same amount, and a whole day of schedule fits in a
// test that takes microseconds.
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
        // test clock cannot plausibly run long enough to overflow it; a
        // panicking fixture would just be a panicking constructor by another
        // name (IR-21), so this saturates instead.
        let elapsed =
            chrono::Duration::from_std(self.started.elapsed()).unwrap_or(chrono::Duration::MAX);
        self.epoch + elapsed
    }
}

// WHY a scripted sequence rather than one fixed table: the polling
// memory-limit enforcer's tests need the process-table reading to change
// between polls — e.g. a tree that stays under its limit for two ticks and
// crosses it on the third — and a `sample()` that always returns the same
// table cannot express that.
pub(crate) struct ScriptedSampler {
    readings: Vec<Vec<ProcessRss>>,
    calls: AtomicUsize,
}

impl ScriptedSampler {
    /// A sampler that replays `readings` in order, one per [`MemorySampler::sample`]
    /// call, repeating the last reading once the script is exhausted.
    pub(crate) fn new(readings: Vec<Vec<ProcessRss>>) -> Self {
        // A script with nothing to replay is a fixture bug: failing loudly
        // here, at the call site that misconfigured it, beats an
        // index-out-of-bounds panic three frames away inside `sample`.
        assert!(
            !readings.is_empty(),
            "ScriptedSampler needs at least one reading to replay"
        );
        Self {
            readings,
            calls: AtomicUsize::new(0),
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
}

// WHY a scripted sequence rather than one fixed outcome: the liveness loop's
// tests need pass/fail outcomes to change between polls — e.g. two failures
// followed by a pass that resets the consecutive-failure counter — and a
// `probe()` that always returns the same value cannot express that. Unlike
// `ScriptedSampler`, an empty script is not a fixture bug: `harness` wires
// one by default and the `Prober` dyn-compatibility test constructs one, so
// `new(vec![])` has to mean something rather than panic. It means "never
// fails" — the neutral value for a prober nobody scripted, exactly as an
// empty `ScriptedSampler` table means "a machine with no visible processes."
pub(crate) struct ScriptedProber {
    script: Vec<Result<(), ProbeFailure>>,
    calls: AtomicUsize,
    delay: Duration,
    // The `timeout` argument of the most recent `probe()` call, in
    // milliseconds. Nothing else in this fake reads it — `probe()` ignores
    // its own `timeout` parameter exactly like it ignores `target` — but a
    // caller that wires the wrong value in (e.g. `interval` where `timeout`
    // belongs) has nothing else in this fixture to fail against, since
    // every other assertion here is keyed on pass/fail outcomes and call
    // counts alone.
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
    /// Builder-style rather than a second constructor, so call sites built
    /// around `new`'s signature — the four threshold cases and the
    /// dyn-compatibility smoke test — stay untouched. The delay is honoured
    /// even when it exceeds a `probe()` call's own `timeout` argument,
    /// because this fake ignores that argument like every other one: the
    /// point of a case that reaches for `with_delay` is a probe that passes
    /// (or fails) *slowly*, not one that actually times out.
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
            // Counted at call start, not at completion: a liveness loop that
            // has ended (reported and returned) must never issue another
            // call, and a count that only advanced once `with_delay`'s sleep
            // finished would make "no further calls after N intervals"
            // indistinguishable from "one more call currently in flight."
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            // Test timeouts are always small (single-digit seconds), so this
            // cast cannot truncate in practice — and a wrong recorded value
            // here only breaks a test's own assertion, never production
            // behavior.
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
/// default of 3. A call site that needs a different threshold overwrites the
/// field directly via struct-update syntax.
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
    /// Writes `raw` verbatim, then closes — for a response that is not a
    /// well-formed HTTP status line at all.
    Raw(String),
    /// Accepts the connection and then never writes a byte — the only way to
    /// exercise `OsProber`'s read-side timeout honestly, since a scripted
    /// reply that writes something (even garbage) always resolves the read.
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

/// Binds a loopback HTTP fake on `127.0.0.1:0` — see [`loopback_http_on`].
pub(crate) async fn loopback_http(script: Vec<HttpReply>) -> LoopbackHttp {
    loopback_http_on("127.0.0.1:0", script).await
}

/// Binds a loopback HTTP fake on `bind` and serves one scripted reply per
/// accepted connection, in order, recording each request it read.
///
/// Binds before spawning the accept loop and returns the already-bound
/// address, so a probe dialing the returned `SocketAddr` cannot race the
/// bind — restructuring this into "spawn a task that binds" reintroduces
/// that race (a fake torn down, or not yet listening, before the code under
/// test connects makes the connection fail for the wrong reason).
///
/// Every connection is READ before it is replied to, including
/// [`HttpReply::Hang`]'s. Without that, a prober that ignored the target's
/// path, dropped the `Host:` header or never wrote a request at all would
/// pass every test here — the reply does not depend on the request, so only
/// recording it can tell those apart.
pub(crate) async fn loopback_http_on(bind: &str, script: Vec<HttpReply>) -> LoopbackHttp {
    let listener = TcpListener::bind(bind)
        .await
        .unwrap_or_else(|err| panic!("bind loopback HTTP fake on {bind}: {err}"));
    let addr = listener.local_addr().expect("read bound loopback address");
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let accept_loop = tokio::spawn(async move {
        for reply in script {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                return; // listener gone (test dropped it) — nothing left to serve
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

/// Reads one request head — everything through the blank line that ends the
/// headers — giving up at EOF, at a read error, or at [`REQUEST_HEAD_CAP`].
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
