// IR-33: one crate-root fixture module; every test module in this crate
// shares this `test_paths` helper instead of hand-rolling its own.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use shep_core::paths::ShepPaths;
use tokio::sync::{broadcast, watch};

use crate::cron::Clock;
use crate::fake::{ProcScript, ScriptedRunner};
use crate::limits::sample::{MemorySampler, ProcessRss};
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
