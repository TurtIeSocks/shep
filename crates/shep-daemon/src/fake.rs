//! Deterministic scripted [`ProcessRunner`](crate::runner::ProcessRunner) for engine tests
//!
// WHY: deterministic + instant under the paused tokio clock; real OS process
// behavior is covered only by `tests/real_runner.rs` (added when the real
// runner lands).

use core::fmt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, Instant, sleep_until};

use crate::channel::{ChildMessage, ShepherdMessage};
use crate::runner::{
    ExitOutcome, LogLine, ProcIo, ProcessRunner, RunnerError, RunningProcess, SpawnSpec, StopSignal,
};

/// Capacity of every channel the fake wires up — generous enough that no
/// test blocks on backpressure without meaning to.
const CHANNEL_CAPACITY: usize = 32;

/// Delay used by [`ProcScript::never_exits`] / [`ProcScript::ignores_signals`].
///
/// Large enough that no test's `tokio::time::advance` ever reaches it, small
/// enough (~30 days of milliseconds) to stay far under tokio's timer-wheel
/// range and never risk an `Instant + Duration` overflow.
const NEVER_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// How one scripted process behaves when spawned & waited by [`ScriptedRunner`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcScript {
    /// Milliseconds after spawn the process exits on its own
    pub delay_ms: u64,
    /// The outcome reported when the natural exit deadline is reached
    pub outcome: ExitOutcome,
    /// Whether the process honors `signal()`/`Shutdown` by exiting early
    pub obeys_signal: bool,
}

impl ProcScript {
    /// Exits immediately with `code`
    #[must_use]
    pub fn const_exit(code: i32) -> Self {
        Self {
            delay_ms: 0,
            outcome: ExitOutcome {
                code: Some(code),
                signal: None,
            },
            obeys_signal: true,
        }
    }

    /// Exits after `ms` milliseconds with `code`
    #[must_use]
    pub fn stable_then_exit(ms: u64, code: i32) -> Self {
        Self {
            delay_ms: ms,
            outcome: ExitOutcome {
                code: Some(code),
                signal: None,
            },
            obeys_signal: true,
        }
    }

    /// Never exits on its own; still obeys signals
    #[must_use]
    pub fn never_exits() -> Self {
        Self {
            delay_ms: NEVER_MS,
            outcome: ExitOutcome {
                code: None,
                signal: None,
            },
            obeys_signal: true,
        }
    }

    /// Never exits on its own AND ignores signals — only `kill_tree` ends it
    #[must_use]
    pub fn ignores_signals() -> Self {
        Self {
            obeys_signal: false,
            ..Self::never_exits()
        }
    }
}

/// The IO endpoints [`ScriptedRunner::io_handles`] hands back for a spawn:
/// the test-side counterparts to the [`ProcIo`] the same spawn returned.
#[derive(Debug)]
pub struct FakeIo {
    /// Injects stdout/stderr lines into the spawned [`ProcIo::logs`]
    pub logs_tx: mpsc::Sender<LogLine>,
    /// Injects child→daemon messages into the spawned [`ProcIo::from_child`]
    pub from_child_tx: mpsc::Sender<ChildMessage>,
    /// Observes every message the daemon sends on the spawned [`ProcIo::to_child`]
    pub to_child_rx: mpsc::Receiver<ShepherdMessage>,
}

/// Shared, thread-safe state for one scripted proc — lives in an `Arc` so
/// [`FakeProc`]'s clones (used to drive `wait()` and `signal()`/`kill_tree()`
/// from separate tasks in tests) and the `to_child` relay task all observe
/// the same signal/kill events.
struct ProcState {
    /// Spawn-relative exit instant — computed once at spawn (cancel-safety: a
    /// dropped-and-recreated `wait()` future must never restart this clock).
    exit_deadline: Instant,
    /// Outcome reported when `exit_deadline` is reached naturally
    outcome: ExitOutcome,
    /// Whether a signal/shutdown event resolves the wait early
    obeys_signal: bool,
    /// Notified on `signal()` or a `Shutdown` message; permit buffers if
    /// nobody is awaiting yet, so events firing before OR during a `wait()`
    /// both resolve it.
    signal_notify: Notify,
    /// Raw signal number recorded by the most recent explicit `signal()`
    /// call. A `Shutdown` message does NOT set this (see `record_shutdown`),
    /// so `wait()`'s fallback naturally reports `StopSignal::Term` for it.
    pending_signal: Mutex<Option<i32>>,
    /// Every raw signal number an explicit `signal()` call has recorded, in
    /// call order — read back via [`ScriptedRunner::signals`]. A `Shutdown`
    /// message does NOT append here, so tests can assert "no `signal()` call
    /// happened" even though the wait still resolved.
    signals: Mutex<Vec<i32>>,
    /// Notified on `kill_tree()`; same before-or-during buffering as above
    kill_notify: Notify,
    /// `kill_tree()` call count, read back via [`ScriptedRunner::kill_counts`]
    kill_count: AtomicU32,
    /// Latches the first resolved outcome so a repeated `wait()` re-reports
    /// it instead of racing the notify/sleep branches again — matches
    /// `tokio::process::Child::wait`'s documented repeat-call behavior.
    resolved: Mutex<Option<ExitOutcome>>,
}

impl ProcState {
    /// Records an explicit `signal()` call: appends to the `signals` ledger,
    /// arms `pending_signal` with the raw number `wait()` should report, and
    /// wakes a pending (or buffers for a future) wait.
    fn record_signal(&self, raw: i32) {
        self.signals.lock().unwrap().push(raw);
        *self.pending_signal.lock().unwrap() = Some(raw);
        self.signal_notify.notify_one();
    }

    /// A `Shutdown` message resolves an obeys_signal wait exactly like
    /// `signal()` would (falling back to `StopSignal::Term` since
    /// `pending_signal` is left untouched), but is NOT itself an explicit
    /// `signal()` call: it never appears in `signals`.
    fn record_shutdown(&self) {
        self.signal_notify.notify_one();
    }

    fn record_kill(&self) {
        self.kill_count.fetch_add(1, Ordering::SeqCst);
        self.kill_notify.notify_one();
    }
}

impl fmt::Debug for ProcState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcState")
            .field("exit_deadline", &self.exit_deadline)
            .field("outcome", &self.outcome)
            .field("obeys_signal", &self.obeys_signal)
            .field("kill_count", &self.kill_count.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// A single scripted live child produced by [`ScriptedRunner::spawn`]
///
/// `Clone`s share the same underlying state (private `ProcState`, held in an
/// `Arc`), which lets one handle drive `wait()` on a spawned task while
/// another delivers `signal()`/`kill_tree()` concurrently — the pattern the
/// daemon's kill ladder uses against the real runner too. A control event
/// (signal, shutdown, kill) resolves exactly ONE waiting `wait()` call, not
/// every clone independently — `tokio::sync::Notify::notify_one` semantics.
#[derive(Debug, Clone)]
pub struct FakeProc {
    pid: u32,
    state: Arc<ProcState>,
}

impl RunningProcess for FakeProc {
    fn pid(&self) -> u32 {
        self.pid
    }

    async fn wait(&mut self) -> ExitOutcome {
        if let Some(outcome) = *self.state.resolved.lock().unwrap() {
            return outcome;
        }

        // Every branch resolves the wait, so this select! never re-loops: a signal
        // this wait doesn't obey simply isn't a candidate branch (the `if` guard),
        // it doesn't fall through to a retry.
        let outcome = tokio::select! {
            () = sleep_until(self.state.exit_deadline) => self.state.outcome,
            () = self.state.signal_notify.notified(), if self.state.obeys_signal => {
                let raw = self.state.pending_signal.lock().unwrap().take();
                ExitOutcome {
                    code: None,
                    signal: Some(raw.unwrap_or_else(|| StopSignal::Term.as_raw())),
                }
            }
            () = self.state.kill_notify.notified() => {
                ExitOutcome { code: None, signal: Some(StopSignal::Kill.as_raw()) }
            }
        };
        *self.state.resolved.lock().unwrap() = Some(outcome);
        outcome
    }

    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError> {
        self.state.record_signal(sig.as_raw());
        Ok(())
    }

    fn kill_tree(&mut self) -> Result<(), RunnerError> {
        self.state.record_kill();
        Ok(())
    }
}

/// One spawn's shared state plus its still-unclaimed [`FakeIo`] test handles
struct SpawnedProc {
    state: Arc<ProcState>,
    io: Option<FakeIo>,
}

/// Deterministic fake [`ProcessRunner`] driven by a pre-scripted [`ProcScript`] per spawn.
pub struct ScriptedRunner {
    scripts: Mutex<VecDeque<ProcScript>>,
    /// State + IO for every spawn, indexed by spawn order, behind ONE lock.
    /// Two separate `Mutex`es here (one for state, one for IO) would let
    /// concurrent spawns interleave their critical sections and desync a
    /// proc's state from its `FakeIo` at the same index; one lock makes
    /// "assign the next index, push both pieces" atomic.
    spawned: Mutex<Vec<SpawnedProc>>,
}

impl fmt::Debug for ScriptedRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptedRunner").finish_non_exhaustive()
    }
}

impl ScriptedRunner {
    /// Builds a runner that hands out `scripts` to spawns in order
    #[must_use]
    pub fn new(scripts: Vec<ProcScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            spawned: Mutex::new(Vec::new()),
        }
    }

    /// `kill_tree()` call count per proc, indexed by spawn order — the only
    /// kill-assertion accessor.
    #[must_use]
    pub fn kill_counts(&self) -> Vec<u32> {
        self.spawned
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.state.kill_count.load(Ordering::SeqCst))
            .collect()
    }

    /// Every raw signal number an explicit `signal()` call has recorded for
    /// the proc spawned at `spawn_index`, in call order. A `Shutdown`
    /// message never appears here — only real `RunningProcess::signal`
    /// calls do, so this and a resolved `ExitOutcome` together distinguish
    /// "signalled" from "shut down over the channel".
    ///
    /// # Panics
    ///
    /// If `spawn_index` is out of range.
    #[must_use]
    pub fn signals(&self, spawn_index: usize) -> Vec<i32> {
        self.spawned.lock().unwrap()[spawn_index]
            .state
            .signals
            .lock()
            .unwrap()
            .clone()
    }

    /// Takes the [`FakeIo`] test-side handles for the proc spawned at `spawn_index`
    ///
    /// # Panics
    ///
    /// If `spawn_index` is out of range or its `FakeIo` was already taken.
    #[must_use]
    pub fn io_handles(&self, spawn_index: usize) -> FakeIo {
        self.spawned
            .lock()
            .unwrap()
            .get_mut(spawn_index)
            .and_then(|p| p.io.take())
            .expect("io_handles: no unclaimed IO bundle at this spawn index")
    }
}

impl ProcessRunner for ScriptedRunner {
    type Proc = FakeProc;

    fn spawn(&self, _spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RunnerError::SpawnFailed("script exhausted".to_string()))?;

        let state = Arc::new(ProcState {
            exit_deadline: Instant::now() + Duration::from_millis(script.delay_ms),
            outcome: script.outcome,
            obeys_signal: script.obeys_signal,
            signal_notify: Notify::new(),
            pending_signal: Mutex::new(None),
            signals: Mutex::new(Vec::new()),
            kill_notify: Notify::new(),
            kill_count: AtomicU32::new(0),
            resolved: Mutex::new(None),
        });

        let (logs_tx, logs_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (from_child_tx, from_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (to_child_tx, raw_to_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (relay_tx, relay_rx) = mpsc::channel(CHANNEL_CAPACITY);

        // Relay task: the fake watches its own to_child stream so a `Shutdown`
        // message resolves an obeys_signal wait (falling back to Term) exactly
        // like `signal()` would — but via `record_shutdown`, NOT `record_signal`,
        // so it never shows up in `ScriptedRunner::signals`. Every message is
        // also forwarded onward so tests can independently observe daemon→child
        // traffic via `FakeIo::to_child_rx`.
        let relay_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut raw_rx = raw_to_child_rx;
            while let Some(msg) = raw_rx.recv().await {
                if msg == ShepherdMessage::Shutdown {
                    relay_state.record_shutdown();
                }
                if relay_tx.send(msg).await.is_err() {
                    break; // observer dropped FakeIo::to_child_rx; stop relaying
                }
            }
        });

        let mut spawned = self.spawned.lock().unwrap();
        let index = spawned.len();
        spawned.push(SpawnedProc {
            state: Arc::clone(&state),
            io: Some(FakeIo {
                logs_tx,
                from_child_tx,
                to_child_rx: relay_rx,
            }),
        });
        drop(spawned);

        let proc_io = ProcIo {
            logs: logs_rx,
            from_child: from_child_rx,
            to_child: to_child_tx,
        };
        // Arbitrary but deterministic — real pids come from the OS in the real runner.
        let pid = 1000 + u32::try_from(index).unwrap_or(u32::MAX);
        Ok((FakeProc { pid, state }, proc_io))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::channel::{ChildMessage, ShepherdMessage};
    use crate::runner::{
        ExitOutcome, ProcessRunner, RunnerError, RunningProcess, SpawnSpec, StopSignal,
    };

    fn spec() -> SpawnSpec {
        SpawnSpec {
            name: "web".to_string(),
            program: "/bin/true".to_string(),
            args: vec![],
            cwd: None,
            env: BTreeMap::new(),
            out_file: PathBuf::from("/tmp/shep-test-out.log"),
            err_file: PathBuf::from("/tmp/shep-test-err.log"),
            channel: true,
            credentials: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn const_exit_resolves_immediately_with_code() {
        let runner = ScriptedRunner::new(vec![ProcScript::const_exit(0)]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        let outcome = proc.wait().await;
        assert_eq!(
            outcome,
            ExitOutcome {
                code: Some(0),
                signal: None
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stable_then_exit_resolves_after_advancing_its_delay() {
        let runner = ScriptedRunner::new(vec![ProcScript::stable_then_exit(5_000, 0)]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        let outcome = proc.wait().await;
        assert_eq!(
            outcome,
            ExitOutcome {
                code: Some(0),
                signal: None
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn signal_before_wait_resolves_with_raw_signal_number() {
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        proc.signal(StopSignal::Int).unwrap();
        let outcome = proc.wait().await;
        assert_eq!(
            outcome,
            ExitOutcome {
                code: None,
                signal: Some(StopSignal::Int.as_raw())
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn signal_during_pending_wait_resolves() {
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (proc, _io) = runner.spawn(&spec()).unwrap();
        let mut waiter = proc.clone();
        let mut signaler = proc;
        let handle = tokio::spawn(async move { waiter.wait().await });
        tokio::time::advance(Duration::from_millis(1)).await;
        signaler.signal(StopSignal::Term).unwrap();
        let outcome = handle.await.unwrap();
        assert_eq!(
            outcome,
            ExitOutcome {
                code: None,
                signal: Some(StopSignal::Term.as_raw())
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn kill_tree_resolves_pending_wait_and_counts() {
        let runner = ScriptedRunner::new(vec![ProcScript::ignores_signals()]);
        let (proc, _io) = runner.spawn(&spec()).unwrap();
        let mut waiter = proc.clone();
        let mut killer = proc;
        let handle = tokio::spawn(async move { waiter.wait().await });
        tokio::time::advance(Duration::from_millis(1)).await;
        killer.kill_tree().unwrap();
        let outcome = handle.await.unwrap();
        assert_eq!(
            outcome,
            ExitOutcome {
                code: None,
                signal: Some(9)
            }
        );
        assert_eq!(runner.kill_counts(), vec![1]);
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_script_errors_on_spawn() {
        let runner = ScriptedRunner::new(vec![]);
        let err = runner.spawn(&spec()).unwrap_err();
        assert_eq!(
            err,
            RunnerError::SpawnFailed("script exhausted".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_safety_deadline_survives_drop_and_reawait() {
        let runner = ScriptedRunner::new(vec![ProcScript::stable_then_exit(5_000, 7)]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();

        // Drive `wait()` on a spawned task so we can abort (drop) it mid-flight — the
        // same "sheep task owns the proc" shape `RunningProcess::wait` is documented
        // for, and the shape the N1 regression actually happened under.
        let mut first_wait = proc.clone();
        let handle = tokio::spawn(async move { first_wait.wait().await });
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            !handle.is_finished(),
            "wait() must still be pending 1s into a 5s delay"
        );
        handle.abort(); // drop the wait() future without ever resolving it

        // N1 regression guard: if the deadline were recomputed from *this* re-await
        // instead of the original spawn-relative one, 4 more seconds would not be enough.
        tokio::time::advance(Duration::from_secs(4)).await;
        let outcome = proc.wait().await;
        assert_eq!(
            outcome,
            ExitOutcome {
                code: Some(7),
                signal: None
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_message_resolves_wait_and_is_observable() {
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (mut proc, io) = runner.spawn(&spec()).unwrap();
        let mut fake_io = runner.io_handles(0);

        io.to_child.send(ShepherdMessage::Shutdown).await.unwrap();

        let outcome = proc.wait().await;
        assert_eq!(
            outcome,
            ExitOutcome {
                code: None,
                signal: Some(StopSignal::Term.as_raw())
            }
        );
        // A Shutdown message is not an explicit signal() call (IMPORTANT-3): it
        // resolves the wait via record_shutdown, which never touches `signals`.
        assert!(runner.signals(0).is_empty());

        let observed = fake_io.to_child_rx.recv().await.unwrap();
        assert_eq!(observed, ShepherdMessage::Shutdown);
    }

    #[tokio::test(start_paused = true)]
    async fn io_handles_relay_both_directions() {
        let runner = ScriptedRunner::new(vec![ProcScript::const_exit(0)]);
        let (_proc, mut io) = runner.spawn(&spec()).unwrap();
        let mut fake_io = runner.io_handles(0);

        fake_io
            .from_child_tx
            .send(ChildMessage::Ready)
            .await
            .unwrap();
        let received = io.from_child.recv().await.unwrap();
        assert_eq!(received, ChildMessage::Ready);

        let sent = ShepherdMessage::Action {
            name: "gc".to_string(),
        };
        io.to_child.send(sent.clone()).await.unwrap();
        let observed = fake_io.to_child_rx.recv().await.unwrap();
        assert_eq!(observed, sent);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_wait_returns_the_same_cached_outcome() {
        // MINOR-8 regression guard: a second wait() must re-report the latched
        // outcome instead of racing the (already-fired) select! branches again.
        let runner = ScriptedRunner::new(vec![ProcScript::const_exit(3)]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        let first = proc.wait().await;
        let second = proc.wait().await;
        assert_eq!(first, second);
        assert_eq!(
            first,
            ExitOutcome {
                code: Some(3),
                signal: None
            }
        );
    }
}
