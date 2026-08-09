// IR-33: one crate-root fixture module; every test module in this crate
// shares this `test_paths` helper instead of hand-rolling its own.
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use shep_core::config::{ProbeConfig, ProbeKind, ProbeTarget};
use shep_core::paths::ShepPaths;
use shep_core::values::UpDuration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

use crate::cron::Clock;
use crate::fake::{ProcScript, ScriptedRunner};
use crate::limits::sample::{MemorySampler, ProcessRss};
use crate::probes::{ProbeFailure, Prober};
use crate::rpc::RpcContext;
use crate::snapshot::FlockRegistry;
use crate::supervisor::spawn_supervisor;

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
}

/// Builds one supervisor engine (a [`ScriptedRunner`] replaying `scripts`)
/// plus a fresh [`RpcContext`] wired to it.
pub(crate) fn harness(scripts: Vec<ProcScript>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let (events, events_rx) = broadcast::channel(256);
    let supervisor = spawn_supervisor(ScriptedRunner::new(scripts), paths.clone(), events.clone());
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
// one by default and Task 7's own dyn-compatibility line constructs one, so
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

/// Binds a loopback HTTP fake on `127.0.0.1:0` and serves one scripted reply
/// per accepted connection, in order.
///
/// Binds before spawning the accept loop and returns the already-bound
/// address, so a probe dialing the returned `SocketAddr` cannot race the
/// bind — restructuring this into "spawn a task that binds" reintroduces
/// that race (a fake torn down, or not yet listening, before the code under
/// test connects makes the connection fail for the wrong reason).
///
/// The returned `JoinHandle` is not detached: callers abort it once the test
/// is done with it.
pub(crate) async fn loopback_http(
    script: Vec<HttpReply>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback HTTP listener");
    let addr = listener.local_addr().expect("read bound loopback address");
    let handle = tokio::spawn(async move {
        for reply in script {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                return; // listener gone (test dropped it) — nothing left to serve
            };
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
    (addr, handle)
}
