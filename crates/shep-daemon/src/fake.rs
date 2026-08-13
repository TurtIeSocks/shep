//! Deterministic scripted [`ProcessRunner`](crate::runner::ProcessRunner) for engine tests
//!
// WHY: deterministic + instant under the paused tokio clock; real OS process
// behavior is covered only by `tests/real_runner.rs` (added when the real
// runner lands).

use core::fmt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Duration, Instant, sleep_until};

use crate::channel::{ChildMessage, ShepherdMessage};
use crate::runner::{
    ExitOutcome, LogCtl, LogLine, ProcIo, ProcessRunner, RunnerError, RunningProcess, SpawnSpec,
    StopSignal,
};

/// Capacity of every channel the fake wires up — generous enough that no
/// test blocks on backpressure without meaning to.
const CHANNEL_CAPACITY: usize = 32;

/// Delay used by [`ProcScript::never_exits`], [`ProcScript::ignores_signals`]
/// and [`ProcScript::never_reports_its_exit`].
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
    /// Whether `kill_tree()` resolves its `wait()`. `true` for every ordinary
    /// process — `SIGKILL` cannot be caught — and `false` only for
    /// [`ProcScript::never_reports_its_exit`], which models the one child a
    /// kill ladder cannot end.
    pub obeys_kill: bool,
    /// Whether a forked lamb keeps the child's stdout and stderr open past
    /// the child's own exit, so neither stream ever reaches EOF. See
    /// [`ProcScript::with_a_lamb_holding_the_pipe`].
    pub lamb_holds_the_pipe: bool,
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
            obeys_kill: true,
            lamb_holds_the_pipe: false,
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
            obeys_kill: true,
            lamb_holds_the_pipe: false,
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
            obeys_kill: true,
            lamb_holds_the_pipe: false,
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

    /// Never resolves its `wait()` at all: not on a signal, and not on
    /// `kill_tree` either.
    ///
    /// Models the one child a kill ladder cannot end — one wedged in
    /// uninterruptible sleep, where `SIGKILL` is delivered and accepted by the
    /// kernel and the `wait(2)` behind it simply never returns. `kill_process`
    /// waits on that unbounded, so the sheep task never reports a
    /// `Msg::Exited`, and this is the only way to put a test on what the
    /// supervisor does when a message it is waiting on never comes.
    ///
    /// The kill is still delivered and still counted
    /// ([`ScriptedRunner::kill_counts`]) — what this withholds is the exit,
    /// not the signal.
    #[must_use]
    pub fn never_reports_its_exit() -> Self {
        Self {
            obeys_kill: false,
            ..Self::ignores_signals()
        }
    }

    /// This script, with a forked lamb holding the child's stdout and stderr
    /// open past the child's own exit.
    ///
    /// A scripted proc's log-control task otherwise ends with the proc,
    /// which models the ordinary sheep: both streams reach EOF when the
    /// child does, and that retires the real runner's pump. A lamb that
    /// inherited those pipes retires nothing, and the pump is then left to
    /// end on one of its other conditions — its `logs` receiver going away,
    /// or the last control sender dropping. This is the only way to put a
    /// test on that path from the fake tier.
    #[must_use]
    pub fn with_a_lamb_holding_the_pipe(self) -> Self {
        Self {
            lamb_holds_the_pipe: true,
            ..self
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
    /// Whether a `kill_tree()` event resolves the wait; see
    /// [`ProcScript::obeys_kill`]
    obeys_kill: bool,
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
    /// Flipped to `true` when `wait()` latches an outcome, and watched by
    /// this proc's log-control task so that task ends with the proc — unless
    /// [`ProcScript::lamb_holds_the_pipe`] says the streams outlive it.
    ///
    /// Without it the fake would answer a reopen aimed at a proc that exited
    /// long ago, while the real runner's pump — and with it the receiving
    /// end of [`ProcIo::log_ctl`] — is gone once the child's streams reach
    /// EOF. A caller's "the pump is already gone" branch would then be
    /// unreachable from this tier, and a test for it would pass whether or
    /// not that branch existed.
    exited: watch::Sender<bool>,
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

        // Every branch resolves the wait, so this select! never re-loops: an
        // event this wait doesn't obey simply isn't a candidate branch (the
        // `if` guard), it doesn't fall through to a retry. With both guards
        // off, `exit_deadline` is the only branch left — which for
        // `never_reports_its_exit` is `NEVER_MS` away, so nothing this side
        // of a month of virtual time resolves it.
        let outcome = tokio::select! {
            () = sleep_until(self.state.exit_deadline) => self.state.outcome,
            () = self.state.signal_notify.notified(), if self.state.obeys_signal => {
                let raw = self.state.pending_signal.lock().unwrap().take();
                ExitOutcome {
                    code: None,
                    signal: Some(raw.unwrap_or_else(|| StopSignal::Term.as_raw())),
                }
            }
            () = self.state.kill_notify.notified(), if self.state.obeys_kill => {
                ExitOutcome { code: None, signal: Some(StopSignal::Kill.as_raw()) }
            }
        };
        *self.state.resolved.lock().unwrap() = Some(outcome);
        // Ends this proc's log-control task; see `ProcState::exited`.
        // `send_replace` rather than `send` because the task may already be
        // gone, and a proc having exited is not news the fake can fail on.
        self.state.exited.send_replace(true);
        outcome
    }

    // A scripted proc models exactly ONE process with no descendants, so
    // `signal`'s group-wide delivery contract and a leader-only delivery are
    // indistinguishable here: there is nobody else in the group to reach.
    // Both methods therefore record the call and resolve the wait, and
    // neither is evidence that a real sheep's forked lambs are signalled —
    // `tests/real_runner.rs` proves that against a real forking wrapper,
    // which is the only tier that can.
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
    /// `false` once this spawn's log-control task has ended — read back via
    /// [`ScriptedRunner::log_ctl_live`].
    log_ctl_live: Arc<AtomicBool>,
    /// How many [`LogCtl::Reopen`] requests this spawn's log-control task has
    /// answered — read back via [`ScriptedRunner::reopens`].
    reopens: Arc<AtomicU32>,
    /// How many [`LogCtl::Flush`] requests it has answered — read back via
    /// [`ScriptedRunner::flushes`].
    flushes: Arc<AtomicU32>,
}

/// The pid [`ScriptedRunner`] gives the first proc it spawns; each later
/// spawn gets the next number up.
///
/// Named so a fixture that has to describe that proc's process table — a
/// scripted memory sampler, say — can say which pid it means instead of
/// repeating a literal this file is free to change.
pub const FIRST_SCRIPTED_PID: u32 = 1000;

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

    /// Whether the log-control task for the proc spawned at `spawn_index` is
    /// still running.
    ///
    /// It runs while something still holds a [`ProcIo::log_ctl`] sender AND
    /// something still holds the [`ProcIo::logs`] receiver AND the proc has
    /// not exited (that last one only while no lamb holds the pipe —
    /// [`ProcScript::with_a_lamb_holding_the_pipe`]). Those are the three
    /// facts that keep a real runner's log pump alive, and this is how an
    /// engine test pins them: the fake writes no files, so a pump left
    /// running by mistake costs its tests nothing otherwise.
    ///
    /// # Panics
    ///
    /// If `spawn_index` is out of range.
    #[must_use]
    pub fn log_ctl_live(&self, spawn_index: usize) -> bool {
        self.spawned.lock().unwrap()[spawn_index]
            .log_ctl_live
            .load(Ordering::SeqCst)
    }

    /// How many [`LogCtl::Reopen`] requests the proc spawned at `spawn_index`
    /// has been sent.
    ///
    /// The fake writes no files, so this is the only thing an engine-tier
    /// test can assert about a reopen: that the request reached this spawn's
    /// end of [`ProcIo::log_ctl`] at all. Whether a reopened handle really
    /// lands on a recreated inode is `tests/real_runner.rs`'s question.
    ///
    /// # Panics
    ///
    /// If `spawn_index` is out of range.
    #[must_use]
    pub fn reopens(&self, spawn_index: usize) -> u32 {
        self.spawned.lock().unwrap()[spawn_index]
            .reopens
            .load(Ordering::SeqCst)
    }

    /// How many [`LogCtl::Flush`] requests the proc spawned at `spawn_index`
    /// has been sent.
    ///
    /// A separate counter from [`Self::reopens`] rather than one shared
    /// "control requests" total, because the two verbs differ only in which
    /// variant they push: a `flush` wired to send `LogCtl::Reopen` would
    /// reopen the flock's handles and truncate nothing, and a single counter
    /// would read the same either way.
    ///
    /// Note what this can and cannot show. The fake writes no files, so it
    /// proves only that the request reached this spawn's end of
    /// [`ProcIo::log_ctl`]. That the truncate happens AFTER the answer is
    /// `supervisor.rs`'s own `LateWritingPumpRunner`, and that a real
    /// handle's pending bytes land where they should is
    /// `tests/real_runner.rs`.
    ///
    /// # Panics
    ///
    /// If `spawn_index` is out of range.
    #[must_use]
    pub fn flushes(&self, spawn_index: usize) -> u32 {
        self.spawned.lock().unwrap()[spawn_index]
            .flushes
            .load(Ordering::SeqCst)
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

    /// Every field of `spec` besides `spec.channel` is still read by nothing
    /// here: the fake writes no files (`spec.out_file`/`err_file`) and runs
    /// no program (`spec.program`/`args`/`env`/`cwd`/`credentials`), which is
    /// what keeps it deterministic and instant under the paused clock — see
    /// this module's own top-level `WHY`. `spec.channel` is the one exception,
    /// because `begin_action` (`supervisor.rs`) now treats "does this sheep
    /// have a channel" as load-bearing, and a fake that answered that
    /// question wrong for every spawn would be worse than one that could not
    /// answer it at all. What that one flag changes is below, gated on it.
    ///
    /// Real fd-3 delivery, refusal, and timeout — the facts this flag alone
    /// cannot reach, since nothing here is a real socketpair to a real child
    /// — are proven against [`crate::tokio_runner::TokioRunner`] instead, in
    /// `tests/daemon_e2e.rs`.
    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RunnerError::SpawnFailed("script exhausted".to_string()))?;

        let (exited_tx, mut exited_rx) = watch::channel(false);
        let state = Arc::new(ProcState {
            exit_deadline: Instant::now() + Duration::from_millis(script.delay_ms),
            outcome: script.outcome,
            obeys_signal: script.obeys_signal,
            obeys_kill: script.obeys_kill,
            signal_notify: Notify::new(),
            pending_signal: Mutex::new(None),
            signals: Mutex::new(Vec::new()),
            kill_notify: Notify::new(),
            kill_count: AtomicU32::new(0),
            resolved: Mutex::new(None),
            exited: exited_tx,
        });

        let (logs_tx, logs_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (from_child_tx, from_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (to_child_tx, raw_to_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (relay_tx, relay_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (log_ctl_tx, mut log_ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);

        // The fake ignores `spec.out_file`/`err_file` and writes no files, so
        // there is no handle here to swap and nothing about a reopen this
        // tier can prove — `tests/real_runner.rs` is where a reopened handle
        // meets a real inode. What the fake must not do is leave the
        // acknowledgement unanswered: a caller awaiting one would hang
        // against every scripted proc, so this answers each request as a
        // live pump would.
        //
        // What it must ALSO not do is answer forever. A real pump ends when
        // the child's streams reach EOF, so a reopen aimed at a sheep that
        // has stopped fails to send; a fake that kept answering past its
        // proc's exit would make that branch unreachable from this tier and
        // every test of it vacuous. So this task ends on each of the three
        // conditions that end a real pump, and on nothing else.
        let log_ctl_live = Arc::new(AtomicBool::new(true));
        let ctl_live = Arc::clone(&log_ctl_live);
        let reopens = Arc::new(AtomicU32::new(0));
        let reopen_count = Arc::clone(&reopens);
        let flushes = Arc::new(AtomicU32::new(0));
        let flush_count = Arc::clone(&flushes);
        let lamb_holds_the_pipe = script.lamb_holds_the_pipe;
        let logs_for_pump = logs_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    ctl = log_ctl_rx.recv() => match ctl {
                        // Both arms count BEFORE they answer, so a test that
                        // observes the acknowledgement can read the count
                        // without racing this task.
                        Some(LogCtl::Reopen { done }) => {
                            reopen_count.fetch_add(1, Ordering::SeqCst);
                            // Always `Ok`: the fake writes no files, so it
                            // has no open that could fail. A pump that
                            // cannot reopen a path is `tokio_runner`'s
                            // tier, and a caller's handling of that answer
                            // is tested against a runner written for it
                            // (`supervisor`'s `FailingPumpRunner`).
                            let _ = done.send(Ok(()));
                        }
                        Some(LogCtl::Flush { done }) => {
                            flush_count.fetch_add(1, Ordering::SeqCst);
                            // Always `Ok`, for the same reason: with no
                            // handle there is nothing queued that could
                            // fail to land.
                            let _ = done.send(Ok(()));
                        }
                        None => break, // nothing holds ProcIo::log_ctl
                    },
                    // The owner dropped ProcIo::logs. A real pump ends on
                    // this whether or not a line is flowing, and it is the
                    // only thing that ends one whose streams a lamb is
                    // holding open.
                    () = logs_for_pump.closed() => break,
                    // Resolves when `wait()` latches an outcome, and errors
                    // once nothing holds this proc's state any more. Either
                    // way the proc is over — which reaches the pump as both
                    // streams hitting EOF, unless a lamb inherited them.
                    _ = exited_rx.changed(), if !lamb_holds_the_pipe => break,
                }
            }
            // Stored before this task returns, so the flag is already false
            // by the time `log_ctl_rx` drops and closes the channel: a test
            // that waits on the close never then reads a stale `true`.
            ctl_live.store(false, Ordering::SeqCst);
        });

        // Relay task: the fake watches its own to_child stream so a `Shutdown`
        // message resolves an obeys_signal wait (falling back to Term) exactly
        // like `signal()` would — but via `record_shutdown`, NOT `record_signal`,
        // so it never shows up in `ScriptedRunner::signals`. Every message is
        // also forwarded onward so tests can independently observe daemon→child
        // traffic via `FakeIo::to_child_rx`.
        //
        // Gated on `spec.channel` — mirroring `tokio_runner.rs`'s own `else`
        // branch — so this fake agrees with the real runner about the one
        // fact `begin_action` now treats as load-bearing: whether a sheep has
        // a channel at all. `spec.channel == false` drops both real ends
        // (`from_child_tx`, `raw_to_child_rx`) right here, exactly as the real
        // runner does at spawn: `ProcIo::from_child` reads closed at once and
        // `ProcIo::to_child.send()` fails immediately, rather than being
        // accepted by a relay that will never run. `FakeIo`'s own test-facing
        // handles get their own already-closed stand-ins in that branch —
        // there is no reader or writer task on this spawn for a test to
        // observe traffic through.
        let (fake_from_child_tx, fake_to_child_rx) = if spec.channel {
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
            (from_child_tx, relay_rx)
        } else {
            drop(from_child_tx);
            drop(raw_to_child_rx);
            drop(relay_tx);
            let (stand_in_tx, stand_in_rx) = mpsc::channel::<ChildMessage>(1);
            drop(stand_in_rx);
            let (dead_tx, dead_rx) = mpsc::channel::<ShepherdMessage>(1);
            drop(dead_tx);
            (stand_in_tx, dead_rx)
        };

        let mut spawned = self.spawned.lock().unwrap();
        let index = spawned.len();
        spawned.push(SpawnedProc {
            state: Arc::clone(&state),
            io: Some(FakeIo {
                logs_tx,
                from_child_tx: fake_from_child_tx,
                to_child_rx: fake_to_child_rx,
            }),
            log_ctl_live,
            reopens,
            flushes,
        });
        drop(spawned);

        let proc_io = ProcIo {
            logs: logs_rx,
            from_child: from_child_rx,
            to_child: to_child_tx,
            log_ctl: log_ctl_tx,
        };
        // Arbitrary but deterministic — real pids come from the OS in the real runner.
        let pid = FIRST_SCRIPTED_PID + u32::try_from(index).unwrap_or(u32::MAX);
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
            params: None,
            id: 0,
        };
        io.to_child.send(sent.clone()).await.unwrap();
        let observed = fake_io.to_child_rx.recv().await.unwrap();
        assert_eq!(observed, sent);
    }

    /// Fails if the fake drops its control receiver instead of answering:
    /// the send fails, or the acknowledgement resolves `Err` because the
    /// `oneshot` sender was dropped unanswered. Either way a caller that
    /// awaits a reopen would hang against every scripted proc.
    ///
    /// This tier proves the acknowledgement and nothing else — the fake
    /// ignores `spec.out_file`/`err_file` and writes no files, so it has no
    /// handle to swap. What a reopened handle does to a real inode is
    /// `tokio_runner`'s own tests.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_is_acknowledged_by_the_scripted_runner() {
        // One script for one spawn. A second spawn would answer
        // `SpawnFailed("script exhausted")`, which is why the count is
        // stated rather than left generous.
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (_proc, io) = runner.spawn(&spec()).unwrap();

        let (done, ack) = tokio::sync::oneshot::channel();
        io.log_ctl.send(LogCtl::Reopen { done }).await.unwrap();

        // Bounded rather than a bare await: an unanswered reopen must fail
        // this test, not hang it. Under the paused clock the deadline fires
        // as soon as the runtime is idle, so a passing run costs nothing.
        let outcome = tokio::time::timeout(Duration::from_secs(5), ack)
            .await
            .expect("a reopen must be acknowledged")
            .expect("the fake must answer rather than drop the acknowledgement");
        assert_eq!(
            outcome,
            Ok(()),
            "a fake with no files to open has nothing that can fail"
        );
    }

    /// Fails if the fake's control task outlives its proc: `log_ctl.send`
    /// would keep succeeding against a proc that exited long ago, and the
    /// "the pump is already gone" branch every caller of
    /// [`ProcIo::log_ctl`] is told to take would be unreachable from this
    /// tier. A test for reopening a stopped sheep would then pass whether or
    /// not the code handled one.
    #[tokio::test(start_paused = true)]
    async fn a_proc_that_has_exited_closes_its_control_channel() {
        // One script for one spawn. A second spawn would answer
        // `SpawnFailed("script exhausted")` and never reach the channel at
        // all, which is why the count is stated rather than left generous.
        let runner = ScriptedRunner::new(vec![ProcScript::const_exit(0)]);
        let (mut proc, io) = runner.spawn(&spec()).unwrap();
        assert!(runner.log_ctl_live(0), "sanity: live before the proc exits");

        proc.wait().await;

        // Bounded rather than an immediate assertion: the control task ends
        // on its own schedule, so this waits for the close instead of
        // requiring it to have already happened.
        tokio::time::timeout(Duration::from_secs(5), io.log_ctl.closed())
            .await
            .expect("a proc that has exited must close its control channel");
        assert!(!runner.log_ctl_live(0));

        let (done, ack) = tokio::sync::oneshot::channel();
        assert!(
            io.log_ctl.send(LogCtl::Reopen { done }).await.is_err(),
            "a reopen aimed at an exited proc must fail to send"
        );
        // And a request that never reaches a pump resolves the caller's
        // acknowledgement as an error rather than leaving it pending — the
        // same no-op, observed a moment later.
        assert!(ack.await.is_err());
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
