//! The supervisor actor: owns the flock's lifecycle state machine.
//!
//! [`spawn_supervisor`] starts the actor task and hands back a
//! [`SupervisorHandle`]. Every registered instance ("sheep") additionally
//! gets its own per-sheep task, spawned by the actor, that owns the live
//! `(proc, ProcIo)` pair for that instance's whole lifetime and forwards its
//! logs/shepherd-channel traffic — the actor itself never touches a live
//! process directly, only [`RunningProcess`] handles held by those tasks and
//! two senders per sheep: a fire-and-forget control sender, and a [`LogCtl`]
//! sender to that sheep's log pump, whose every message carries an
//! acknowledgement back.
//!
//! # One exit path
//!
//! Every sheep, however it ends — a natural exit or a kill request — reaches
//! the actor as exactly one `Msg::Exited`. The actor's map never holds a
//! `proc`; it holds one lifecycle entry plus those two senders per id, so the
//! actor loop never awaits process I/O.
//!
//! Nor does it ever await an acknowledgement, and with a sender that has one
//! to give, that is a rule the code keeps rather than a shape that enforces
//! itself. Awaiting one from inside the loop would stop the mailbox draining,
//! and the pump that owes the answer is downstream of exactly that mailbox —
//! so every handler that sends a [`LogCtl`] is synchronous and hands the
//! awaiting to a task it spawns. `Actor::handle_reopen` makes the argument in
//! full.
//!
//! # Deferred, aggregated replies
//!
//! `SupervisorHandle::stop`/`restart`/`restart_automatic`/`delete` and
//! [`shutdown`](SupervisorHandle::shutdown) resolve their selector into a
//! set of matched ids up front, then wait until every matched sheep is
//! terminal before answering the caller. The crash loop's own restarts go
//! through the same exit path without ever registering a deferred reply.

use core::fmt;
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use shep_core::config::ResolvedApp;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
use shep_core::selector::ProcessSelector;
use shep_core::status::ProcStatus;

use crate::assemble::{assemble, instance_slots};
use crate::brain::{Decision, decide_on_exit};
use crate::channel::ChildMessage;
use crate::entry::{ProcessEntry, ReloadState, RestartBudget};
use crate::extras::{Extras, ExtrasRegistry};
use crate::kill::kill_process;
use crate::privilege::{self, Credentials};
use crate::probes::Prober;
use crate::probes::os::OsProber;
use crate::probes::ready::{Readiness, ReadinessSource, await_ready};
use crate::runner::{
    ExitOutcome, FlushError, LogCtl, ProcIo, ProcessRunner, ReopenError, RunningProcess, SpawnSpec,
    check_log_ancestry, open_log_path,
};

/// Capacity of the actor's own mailbox (commands + internal events).
const MAILBOX_CAPACITY: usize = 256;

/// Capacity of one sheep task's control mailbox — at most one live `Kill` is
/// ever in flight, so this stays small on purpose.
const SHEEP_CTL_CAPACITY: usize = 4;

// ---------------------------------------------------------------------
// Public command / handle surface
// ---------------------------------------------------------------------

/// Commands the supervisor actor accepts (wrapped in [`Msg::Command`]).
///
/// `pub(crate)` like [`Msg`], and for the same reason: [`SupervisorHandle`] is
/// the only door into the actor, nothing outside this crate names this type,
/// and a public non-`#[non_exhaustive]` enum would make every new subsystem's
/// command a semver break for a surface nobody uses.
#[derive(Debug)]
pub(crate) enum Command {
    /// Registers + spawns each app's instances.
    Start {
        /// Already-validated app specs to expand into instances.
        apps: Vec<ResolvedApp>,
        /// Answers with every spawned instance, or the first spawn failure.
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Stops every sheep matching `selector` (stays registered).
    Stop {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers once every matched sheep is terminal.
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Restarts every sheep matching `selector`.
    Restart {
        /// Which sheep.
        selector: ProcessSelector,
        /// Who asked: an operator off the control socket, or the daemon's own
        /// cron or watch worker. Governs exactly two things — whether this
        /// restart can be displaced mid-kill-ladder (see
        /// `Actor::claim_manual`), and the `manually` flag on the bus events
        /// it produces. It never changes what the restart DOES, including its
        /// budget reset.
        origin: CommandOrigin,
        /// Answers once every matched sheep is back online (or errored).
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Restarts one sheep on behalf of a memory breach or a liveness failure,
    /// if the process that produced the report is still the one running now.
    ///
    /// The only command with no `reply`: dropping a stale report is the
    /// intended outcome, not an error its reporter could act on.
    ExtraRestart {
        /// The sheep's id.
        id: u32,
        /// The pid the report was raised against, used as a generation token.
        pid: u32,
    },
    /// Replaces every matched sheep with a fresh instance, one instance of
    /// each app at a time.
    ///
    /// `allow` rather than `expect` because the supervisor's own tests drive
    /// this command while no production caller does yet: the expectation
    /// would be fulfilled in the test build and unfulfilled in the lib build,
    /// which is an error either way round. Both attributes go when the
    /// control socket grows the verb that sends it.
    #[allow(dead_code, reason = "no production sender until the wire verb lands")]
    Reload {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers the moment the reload is ACCEPTED, not when it finishes —
        /// see [`Actor::handle_reload`] for the arithmetic that rules out
        /// holding the caller.
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Stops + deregisters every sheep matching `selector`.
    Delete {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers with the deleted ids once every matched sheep is terminal.
        reply: oneshot::Sender<Result<Vec<u32>, SupervisorError>>,
    },
    /// Full flock listing, id-sorted.
    List {
        /// Answers with the current snapshot.
        reply: oneshot::Sender<Vec<ProcessInfo>>,
    },
    /// Reopens the log files of every sheep matching `selector`.
    Reopen {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers once every matched sheep's log pump has acknowledged —
        /// off a task of its own, never the actor loop (see
        /// [`Actor::handle_reopen`]).
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Empties the log files of every sheep matching `selector`: flushes
    /// every pump writing to one of those paths, then truncates them.
    Flush {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers once every path has been truncated — off a task of its
        /// own, never the actor loop (see [`Actor::handle_flush`]).
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Graceful engine shutdown: kill ladder on every online sheep, then stop.
    Shutdown {
        /// Answers once every online sheep is terminal, right before the
        /// actor returns.
        reply: oneshot::Sender<()>,
    },
}

/// The actor's mailbox message: [`Command`]s plus events the actor
/// generates for itself (sheep-task exits, restart timers, drained
/// readiness signals).
#[derive(Debug)]
pub(crate) enum Msg {
    /// A caller-issued command.
    Command(Command),
    /// A sheep task's proc resolved — natural exit or a completed kill.
    Exited {
        /// The sheep's id.
        id: u32,
        /// How it ended.
        outcome: ExitOutcome,
    },
    /// A scheduled restart's backoff has elapsed.
    RestartDue {
        /// The sheep's id.
        id: u32,
        /// The sheep's `SheepSlot::epoch` at scheduling time. Ignored if it
        /// no longer matches the slot's current epoch — a stale timer left
        /// behind by a respawn that happened in the meantime (IMPORTANT-3).
        epoch: u64,
    },
    /// The sheep's shepherd channel reported readiness.
    ///
    /// Forwarded to the waiting readiness task's `oneshot::Sender`, if one is
    /// waiting (`SheepSlot::ready_tx`) — dropped silently otherwise, so an
    /// app is free to write `{"kind":"ready"}` whenever it likes, including
    /// twice.
    Ready {
        /// The sheep's id.
        id: u32,
    },
    /// A readiness wait resolved.
    ReadyResult {
        /// The sheep's id.
        id: u32,
        /// The slot's epoch when the wait began; a stale result is dropped.
        epoch: u64,
        /// The `manually` flag the spawn this wait belongs to would have put
        /// on its own `Online` had it not been gated. Rides along with
        /// `epoch` (rather than being stored on the slot) because the two
        /// answer the same question — which spawn is this? — and so cannot
        /// drift apart.
        manually: bool,
        /// Whether the signal arrived or the deadline elapsed.
        readiness: Readiness,
    },
}

/// Error type returned from supervisor commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    /// The selector matched no registered sheep.
    NotFound,
    /// Spawn failed (carries the runner's message).
    SpawnFailed(String),
    /// The selector reached an app that is already being reloaded; carries
    /// that app's name.
    ///
    /// Refused rather than queued or interleaved: a second reload of the same
    /// app would put a third entry in an instance slot two are already
    /// sharing, and neither of the two would then know which one it is meant
    /// to outlive. The whole command is refused, not the overlapping part of
    /// it — a partly-accepted selector leaves the caller unable to tell which
    /// half was taken.
    ReloadInFlight(String),
    /// At least one matched sheep's log pump could not open a log path
    /// again, so that stream has no file to write to. Carries one
    /// `"<name> (id <id>): <paths and reasons>"` entry per such sheep,
    /// joined by `"; "`. Every other matched sheep was reopened.
    ///
    /// One sheep's own two paths arrive joined by `", "` — see
    /// [`ReopenError::message`](crate::runner::ReopenError::message) — so
    /// the two levels stay tellable apart in one flat string.
    ReopenFailed(String),
    /// At least one matched log file could not be flushed or truncated: a
    /// pump could not land what it owed that file, or the path itself could
    /// not be truncated. Carries one `"<path>: <reason>"` entry per such
    /// file, joined by `"; "` — where a single pump that failed on both of
    /// its streams contributes one entry with the two paths joined by `", "`,
    /// so the nesting stays readable.
    /// Every other matched path was emptied.
    ///
    /// Says nothing about what those files hold afterwards, because the two
    /// halves differ there and neither is knowable from here. A truncate that
    /// failed leaves its file as it was; a flush that failed does not stop
    /// the truncate — the bytes it reports are bytes that errored, not bytes
    /// still in flight — so that file is empty, and what the operator is
    /// being told is that the lines it held are gone unwritten.
    ///
    /// Keyed by path where [`Self::ReopenFailed`] is keyed by sheep, and
    /// deliberately: a reopen's unit of work is one sheep's pump, while a
    /// flush's is one file — and one file can belong to several sheep at
    /// once (see [`FlushError::message`](crate::runner::FlushError::message)).
    FlushFailed(String),
    /// The actor has shut down; its mailbox is closed.
    EngineStopped,
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("selector matched no registered sheep"),
            Self::SpawnFailed(msg) => write!(f, "spawn failed: {msg}"),
            Self::ReloadInFlight(name) => write!(f, "{name} is already being reloaded"),
            Self::ReopenFailed(msg) => write!(f, "log reopen failed: {msg}"),
            Self::FlushFailed(msg) => write!(f, "log flush failed: {msg}"),
            Self::EngineStopped => f.write_str("supervisor engine has shut down"),
        }
    }
}

impl core::error::Error for SupervisorError {}

/// Handle to a running supervisor actor.
///
/// Cloning shares the same actor; every clone's commands are serialized
/// through its single mailbox.
///
/// Public, with [`spawn_supervisor`] and [`SupervisorError`], because the
/// crate-root doc example drives one — and rustdoc compiles a doc example as
/// its own crate, so `start`, `list` and `shutdown` have to be reachable from
/// outside. The rest of the handle's methods do not, and are crate-private.
#[derive(Debug, Clone)]
pub struct SupervisorHandle {
    tx: mpsc::Sender<Msg>,
}

impl SupervisorHandle {
    /// Registers + spawns each app's instances.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::SpawnFailed`] — the first instance that failed
    ///   to spawn (already-registered instances persist regardless).
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub async fn start(&self, apps: Vec<ResolvedApp>) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Start { apps, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Stops every sheep matching `selector`.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn stop(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Stop { selector, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Restarts every sheep matching `selector`, resetting its restart
    /// budget (spec §4: a manual action resets budget).
    ///
    /// Declares `CommandOrigin::Operator`: a person typed this, so it is owed
    /// an answer, nothing may take the sheep off it mid-kill-ladder, and the
    /// events it emits carry `manually: true`.
    ///
    /// A restart the daemon raised on its own goes through one of two
    /// siblings instead, and which one depends on what raised it:
    /// [`Self::restart_automatic`] for a cron occurrence or a change under a
    /// watched tree, and [`Self::extra_restart`] for a memory breach or a
    /// liveness failure. All three reset the budget; only this one reports
    /// itself as a user action.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn restart(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        self.restart_with_origin(selector, CommandOrigin::Operator)
            .await
    }

    /// Restarts every sheep matching `selector` on the daemon's own
    /// initiative — a cron occurrence, or a change under a watched tree —
    /// resetting its restart budget exactly as [`Self::restart`] does.
    ///
    /// Declares `CommandOrigin::Automatic`: nobody typed this, so an
    /// operator's `stop` or `delete` landing while it is still mid-kill-ladder
    /// takes the sheep back off it instead of being silently converted into
    /// the restart it raced, and the events it emits carry `manually: false`.
    /// The caller is still handed the same answer [`Self::restart`] returns —
    /// a cron or watch worker reads it to log a failed spawn and to notice the
    /// engine going away.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn restart_automatic(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        self.restart_with_origin(selector, CommandOrigin::Automatic)
            .await
    }

    /// The body both restart methods share; they differ only in the origin
    /// they declare.
    async fn restart_with_origin(
        &self,
        selector: ProcessSelector,
        origin: CommandOrigin,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Restart {
                selector,
                origin,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Restarts `id` on behalf of a memory breach or a liveness failure, if the
    /// process that produced the report is still the process running now.
    ///
    /// Silently does nothing when the report is stale. There is no reply: a
    /// dropped report is the intended outcome, not an error the reporter can act
    /// on.
    ///
    /// The third of the three restart doors, and the only one that guards on
    /// the reporting process still being current: [`Self::restart`] is an
    /// operator's, [`Self::restart_automatic`] is a cron occurrence's or a
    /// watched tree's. Like both of them it resets the restart budget, and
    /// like [`Self::restart_automatic`] it goes in as
    /// `CommandOrigin::Automatic` — displaceable by an operator's command,
    /// and reported on the bus with `manually: false`.
    pub(crate) async fn extra_restart(&self, id: u32, pid: u32) {
        let _ = self
            .tx
            .send(Msg::Command(Command::ExtraRestart { id, pid }))
            .await;
    }

    /// Replaces every sheep matching `selector` with a fresh instance of the
    /// same app, one instance of each app at a time, so the app can stay
    /// reachable across the swap.
    ///
    /// This is an overlap, not zero downtime. Mid-swap the old instance and
    /// its replacement are both alive and both bound to the same instance
    /// slot, which is the window an application needs in order to hand over
    /// without dropping work — but the old listener's accept backlog is reset
    /// when it closes, so whatever was queued and not yet accepted is lost
    /// unless the app itself stops accepting and drains inside
    /// `graceful_timeout`. An app that ignores its stop signal until shep's
    /// `SIGKILL` drops that backlog on every reload.
    ///
    /// Answers as soon as the reload is accepted, with the matched sheep as
    /// they stood at that moment; the swaps themselves are reported on the
    /// bus. A matched sheep that is not `Online` has nothing to replace and
    /// is a no-op success in that answer. Nothing here re-reads config: the
    /// replacement is spawned from the stored `ResolvedApp` and the
    /// credentials resolved at the first `Start`.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::ReloadInFlight`] — the selector reached an app
    ///   whose reload has not finished; carries that app's name.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone, or a
    ///   graceful shutdown has begun (a reload spawns, and a shutdown forbids
    ///   any new spawn).
    #[allow(dead_code, reason = "no production caller until the wire verb lands")]
    pub(crate) async fn reload(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Reload { selector, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Stops + deregisters every sheep matching `selector`.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn delete(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<u32>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Delete { selector, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Reopens the log files of every sheep matching `selector`, for an
    /// external rotator that has renamed them.
    ///
    /// Answers only once every matched sheep's log pump has swapped both
    /// handles, which is the contract a logrotate `postrotate` stanza needs:
    /// when this returns, no live pump is still holding a renamed inode. A
    /// matched sheep that is not running has no pump and nothing to reopen,
    /// and is reported as a success alongside the rest — see
    /// [`Actor::handle_reopen`] for both ways that shows up.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::ReopenFailed`] — every matched pump answered,
    ///   but at least one could not open a log path again. The old handles
    ///   are closed either way, so the rename is safe to act on; what
    ///   failed is the sheep getting a file back.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn reopen(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Reopen { selector, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Empties the log files of every sheep matching `selector`: flushes
    /// every pump writing to one of the paths those sheep were registered
    /// with, then truncates those paths.
    ///
    /// Answers once every one of those paths has been truncated — or, on the
    /// error below, once every one has been attempted. A matched sheep that
    /// is not running has no pump to flush, and its files are truncated all
    /// the same — the operation addresses paths, and a stopped sheep's logs
    /// are readable (`shep bleats --no-follow`) and so worth being able to
    /// empty.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::FlushFailed`] — at least one matched file could
    ///   not be flushed or truncated: a pump could not land what it still
    ///   owed, or a path could not be truncated. Every other matched path was
    ///   emptied. The variant's own doc says why this claims nothing about
    ///   what the named files hold afterwards.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn flush(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Flush { selector, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Full flock listing, id-sorted.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn list_checked(&self) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::List { reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)
    }

    /// Full flock listing, id-sorted.
    ///
    /// Convenience over `Self::list_checked` for callers that don't need
    /// to distinguish "actor gone" from "empty flock" — mainly tests.
    ///
    /// # Panics
    ///
    /// Panics if the actor has shut down.
    #[must_use]
    pub async fn list(&self) -> Vec<ProcessInfo> {
        self.list_checked()
            .await
            .expect("supervisor actor is no longer running")
    }

    /// Graceful engine shutdown: kill ladder on every online sheep, then
    /// stop the actor. A no-op if the actor is already gone.
    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Msg::Command(Command::Shutdown { reply }))
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }
}

/// Builds a supervisor actor.
///
/// Two subsystems beyond the four lifecycle extras (dogs, metrics) are already
/// on the roadmap, so the optional wiring goes on a builder rather than growing
/// [`spawn_supervisor`] a positional parameter each time.
#[derive(Debug)]
pub(crate) struct SupervisorBuilder<R: ProcessRunner> {
    runner: R,
    paths: ShepPaths,
    events: broadcast::Sender<BusEvent>,
    extras: Option<Extras>,
}

impl<R: ProcessRunner> SupervisorBuilder<R> {
    /// A builder with no lifecycle extras: the engine spawns, restarts and
    /// kills, and nothing watches, schedules or probes.
    ///
    /// `events` receives [`BusEvent::Process`] (+ `LogOut`/`LogErr` forwarded
    /// from each sheep's `ProcIo::logs`).
    pub(crate) fn new(runner: R, paths: ShepPaths, events: broadcast::Sender<BusEvent>) -> Self {
        Self {
            runner,
            paths,
            events,
            extras: None,
        }
    }

    /// Wires in the lifecycle extras.
    #[must_use]
    pub(crate) fn extras(mut self, extras: Extras) -> Self {
        self.extras = Some(extras);
        self
    }

    /// Spawns the actor.
    ///
    /// Must be called from within a Tokio runtime context.
    pub(crate) fn spawn(self) -> SupervisorHandle {
        let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
        let actor = Actor {
            runner: self.runner,
            paths: self.paths,
            events: self.events,
            tx: tx.clone(),
            sheep: HashMap::new(),
            next_id: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: self.extras,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
        };
        tokio::spawn(actor.run(rx));
        SupervisorHandle { tx }
    }
}

/// Spawns the actor with no lifecycle extras — shorthand for
/// `SupervisorBuilder::new(runner, paths, events).spawn()` (IR-28).
///
/// Must be called from within a Tokio runtime context: it spawns the actor
/// task immediately.
pub fn spawn_supervisor<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    events: broadcast::Sender<BusEvent>,
) -> SupervisorHandle {
    SupervisorBuilder::new(runner, paths, events).spawn()
}

// ---------------------------------------------------------------------
// Internal actor state
// ---------------------------------------------------------------------

/// Fire-and-forget control message to one sheep task (see `run_sheep`).
///
/// No `done` acknowledgement: a sheep task's own natural `Msg::Exited` is
/// the only completion signal the actor ever waits on — the one-exit-path
/// invariant that keeps the actor loop from ever parking on a live process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SheepCtl {
    /// Run the kill ladder and report the resulting exit.
    Kill {
        /// How long the ladder's polite rung gets before it escalates to
        /// `SIGKILL`. Carried on the message rather than read off the app
        /// inside [`kill_process`] because an app configures two such caps —
        /// `kill_timeout` for an ordinary stop, `graceful_timeout` for the
        /// one stop that asks the instance to finish work in hand first —
        /// and only the sender knows which of the two it is asking for.
        grace: Duration,
    },
}

/// Which of an app's two ladder caps a stop runs under.
///
/// The stop ladder waits one timeout on its polite rung before escalating to
/// `SIGKILL`, and an app configures two of them because two different asks
/// reach that rung. Naming the ask rather than passing a bare `Duration`
/// keeps one site that turns an ask into a number, so no caller can reach
/// for the wrong field of the same config by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LadderCap {
    /// `kill_timeout` — an operator's `stop`, `restart` or `delete`, the
    /// daemon's own automatic restarts, and the engine-wide shutdown.
    Stop,
    /// `graceful_timeout` — a reload's drain, the one stop that asks the
    /// instance to finish the work already in hand before it goes, and so
    /// the one given longer to do it.
    Drain,
}

impl LadderCap {
    /// This cap's value for `app`.
    fn of(self, app: &shep_core::config::AppConfig) -> Duration {
        match self {
            Self::Stop => app.kill_timeout,
            Self::Drain => app.graceful_timeout,
        }
        .as_duration()
    }
}

/// One app's in-flight reload.
///
/// Keyed by app name in [`Actor::reloads`], which is what makes a second
/// reload of the same app refusable ([`SupervisorError::ReloadInFlight`])
/// and what every handler uses to ask "is this id part of a live reload".
/// An entry existing here means the app is mid-reload; the entry going away
/// means the reload is over, finished or abandoned.
#[derive(Debug)]
struct ReloadJob {
    /// Instances not yet taken, in slot order. Popped one at a time: a
    /// reload replaces one instance of an app at a time, so the app is only
    /// ever one instance short of its configured count.
    queue: VecDeque<u32>,
    /// The pair mid-swap right now. Exactly one per job — that is what
    /// "one at a time" means.
    swap: ReloadSwap,
}

/// The drainee/replacement pair a reload is working on right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReloadSwap {
    /// The instance being replaced. Carries `ProcStatus::Stopping` and
    /// [`ReloadState::SpawningReplacement`] from the moment the replacement
    /// is spawned.
    old_id: u32,
    /// Its replacement, in the same instance slot under a new id. Carries
    /// [`ReloadState::Draining`] until the swap finishes.
    new_id: u32,
    /// How far along `SpawnNew → AwaitReady → DrainOld → ReapOld` this pair
    /// is.
    phase: ReloadPhase,
}

/// Where a [`ReloadSwap`] is in the spec's per-instance state machine.
///
/// Two variants for four states, because two of the four are instants rather
/// than intervals: `SpawnNew` is one synchronous step of the actor loop and
/// `ReapOld` is the drainee's `Msg::Exited` arriving. What a handler actually
/// has to ask is the question these two answer — is the old instance still
/// there to go back to?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadPhase {
    /// The replacement is registered and starting; nothing has been killed.
    /// The reload is still abandonable with the drainee left serving.
    AwaitReady,
    /// Committed. Either the replacement went online and the drainee's
    /// ladder is running, or the drainee is already gone — there is no old
    /// instance left to return to either way.
    DrainOld,
}

/// Which manual command is pending against a sheep, cleared the moment its
/// `Msg::Exited` is processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualKind {
    /// A `Stop` command targeted this sheep.
    Stop,
    /// A `Restart` command targeted this sheep.
    Restart,
    /// A `Delete` command targeted this sheep.
    Delete,
}

/// Who asked for a pending manual command. Read in exactly two places:
/// [`Actor::claim_manual`], to decide which of two racing commands owns a
/// sheep's next exit, and the two sites that carry the command out
/// ([`Actor::handle_exited`] and [`Actor::apply_immediate`]), which report it
/// as the `manually` flag on every bus event the restart emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandOrigin {
    /// A person asked for it: a `Stop`, `Restart` or `Delete` off the control
    /// socket, or the daemon-wide `Shutdown`. An operator is waiting on the
    /// answer.
    Operator,
    /// The daemon raised it itself: a memory breach or a liveness failure
    /// (through [`SupervisorHandle::extra_restart`]), or a cron occurrence or
    /// watched-file change firing its name-group's restart (through
    /// [`SupervisorHandle::restart_automatic`]).
    ///
    /// Having a reply is not what separates these from an operator's command
    /// — a cron or watch worker does read the `Result`, to log a failed spawn
    /// and to notice the engine going away. What separates them is that
    /// nobody is owed the answer, so an operator's `stop` may take the sheep
    /// off one mid-ladder rather than be converted into it.
    Automatic,
}

/// The manual command that owns a sheep's next exit, and who asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingManual {
    /// What that exit will be turned into.
    kind: ManualKind,
    /// Who asked. What the command DOES once the sheep is down is decided
    /// entirely by `kind`; `origin` survives into `handle_exited` for one
    /// purpose, the `manually` flag on the events that exit produces —
    /// otherwise a cron, watch, memory-breach or liveness restart would be
    /// broadcast as a user action.
    origin: CommandOrigin,
}

/// One registered instance: its lifecycle state plus a live control sender
/// (`None` once its sheep task has ended).
#[derive(Debug)]
struct SheepSlot {
    /// Lifecycle state (spec, status, restart budget, ...).
    entry: ProcessEntry,
    /// Sender for this sheep's control mailbox; `None` when not running.
    ctl: Option<mpsc::Sender<SheepCtl>>,
    /// A clone of the [`ProcIo::log_ctl`] the most recent successful spawn
    /// handed out, which is how a `Reopen` or a `Flush` reaches this sheep's
    /// log pump. `None` only for a slot whose spawn never succeeded at all.
    ///
    /// Written on every successful spawn and never cleared, unlike `ctl`.
    /// Whether a pump is still there is a fact the channel already answers —
    /// a send fails the moment the pump ends — and clearing this field
    /// alongside `ctl` would be a second copy of that fact, free to disagree
    /// with the first.
    ///
    /// Holding it costs the pump no extra life. A pump ends when its `logs`
    /// receiver goes as readily as when its last control sender does
    /// (`spawn_log_pump`'s own `select!` has a branch for each), and the
    /// sheep task holds that receiver for exactly as long as it holds the
    /// original sender — so this clone can delay nothing. Without that
    /// branch it could: a lamb that inherited the child's pipe holds both
    /// streams open past the child's exit, and a clone kept here would then
    /// be the only thing left deciding when the pump — and its two files and
    /// two pipe read ends — got to end.
    log_ctl: Option<mpsc::Sender<LogCtl>>,
    /// Which manual command (if any) is waiting on this sheep's next exit,
    /// and who asked for it. Claimed through [`Actor::claim_manual`].
    manual: Option<PendingManual>,
    /// Set whenever a `Delete` targets this id, even if an earlier command
    /// already owns `manual` (adversarial finding #2 — the fix for
    /// Delete-racing-Shutdown). `manual` records who owns the next Kill;
    /// `pending_delete` records intent that must survive regardless of who
    /// won that race, so a Delete can never be silently downgraded to a
    /// Stop (or, worse, a Restart — a Delete racing a Restart the same way
    /// used to respawn a brand-new live process and still tell the Delete
    /// caller it succeeded) just because another command's marker got there
    /// first. `handle_exited` checks this flag on every path that would
    /// otherwise skip `decide_on_exit` (today, just the manual-Restart
    /// early return) as well as on the `CleanStop` branch itself.
    pending_delete: bool,
    /// Bumped on every successful respawn (IMPORTANT-3). A `RestartDue`
    /// timer carries the epoch it was scheduled under; `handle_restart_due`
    /// drops one whose epoch no longer matches — a stale timer left behind
    /// by a respawn (manual or automatic) that happened in the meantime.
    epoch: u64,
    /// The readiness task's signal sender for the CURRENT epoch.
    /// `Msg::Ready`'s handler takes it to wake the task.
    ///
    /// `None` means one of two things, and deliberately not a third: no
    /// readiness task was ever armed (an app with neither `wait_ready` nor
    /// `readiness_probe` set), or a channel `Ready` already took the sender
    /// to wake one. A wait that resolved some OTHER way — a probe that
    /// passed, a deadline that elapsed — leaves its sender sitting here,
    /// because `handle_ready_result` has no business reaching into a slot a
    /// respawn may already have re-armed. Nothing goes wrong: a `Msg::Ready`
    /// arriving late takes a sender whose receiver is gone, and the send
    /// simply fails, which is the same silent drop an unarmed slot gets.
    ready_tx: Option<oneshot::Sender<()>>,
}

/// Where a deferred `Stop`/`Restart`/`Delete`/`Shutdown` reply eventually
/// goes — the three commands differ only in their reply's payload shape.
#[derive(Debug)]
enum ReplyKind {
    /// `Stop`/`Restart`: reply with the matched sheep's terminal snapshots.
    Info(oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>),
    /// `Delete`: reply with the matched (and now deregistered) ids.
    Ids(oneshot::Sender<Result<Vec<u32>, SupervisorError>>),
    /// `Shutdown`: reply once every online sheep is terminal.
    Shutdown(oneshot::Sender<()>),
}

/// One command's aggregation state: which ids are still outstanding, the
/// terminal snapshots collected so far, and where to reply once `remaining`
/// is empty.
#[derive(Debug)]
struct PendingReply {
    /// Ids not yet observed terminal.
    remaining: HashSet<u32>,
    /// Terminal snapshots collected so far, in arrival order.
    results: Vec<ProcessInfo>,
    /// Where the answer goes once `remaining` is empty.
    reply: ReplyKind,
}

/// The supervisor actor. Holds every registered sheep's lifecycle state and
/// control handle — never a live `proc` — plus any deferred command replies
/// still waiting on matched sheep to go terminal.
struct Actor<R: ProcessRunner> {
    /// Spawn seam (real OS processes or, in tests, the scripted fake).
    runner: R,
    /// `$SHEP_HOME` layout, for assembling spawn specs.
    paths: ShepPaths,
    /// Bus: process lifecycle events + forwarded logs.
    events: broadcast::Sender<BusEvent>,
    /// Clone handed to sheep tasks and restart timers so they can report
    /// back into this same actor's mailbox.
    tx: mpsc::Sender<Msg>,
    /// Every registered instance, keyed by id.
    sheep: HashMap<u32, SheepSlot>,
    /// Monotonic id counter — ids are never reused.
    next_id: u32,
    /// Deferred command replies still waiting on matched sheep.
    pending: Vec<PendingReply>,
    /// Set once a `Shutdown` command starts (CRITICAL-1). While `true`:
    /// `Start`/`Restart` are rejected outright and `RestartDue` respawns
    /// nothing — nothing is allowed to spawn a child the shutdown
    /// aggregation (fixed at the moment it ran) doesn't know to kill.
    shutting_down: bool,
    /// The lifecycle extras' seams and report wiring, or `None` for an engine
    /// built without them (`spawn_supervisor`, and every test that exercises
    /// no schedule, limit, probe or watch).
    extras: Option<Extras>,
    /// What is armed right now, per sheep and per name. Stays empty while
    /// `extras` is `None`: there are no seams to arm anything on.
    registry: ExtrasRegistry,
    /// Every app currently mid-reload, keyed by app name. Empty is the
    /// ordinary state; an entry is what makes a second reload of the same
    /// app refusable and what tells a sheep's exit whether it is a swap's
    /// business or its own.
    reloads: HashMap<String, ReloadJob>,
}

impl<R: ProcessRunner> Actor<R> {
    /// Runs the actor to completion: processes every mailbox message until
    /// a `Shutdown` fully resolves, then returns (dropping `rx` closes the
    /// mailbox, so subsequent [`SupervisorHandle`] calls see
    /// [`SupervisorError::EngineStopped`]).
    async fn run(mut self, mut rx: mpsc::Receiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            let should_break = match msg {
                // Sync now (CRITICAL-2): nothing left in the command path
                // ever awaits — `try_send` replaced the one blocking
                // `.await` that could park the actor on a busy sheep task.
                Msg::Command(cmd) => self.handle_command(cmd),
                Msg::Exited { id, outcome } => self.handle_exited(id, outcome),
                Msg::RestartDue { id, epoch } => {
                    self.handle_restart_due(id, epoch);
                    false
                }
                Msg::Ready { id } => {
                    self.handle_ready_signal(id);
                    false
                }
                Msg::ReadyResult {
                    id,
                    epoch,
                    manually,
                    readiness,
                } => {
                    self.handle_ready_result(id, epoch, manually, readiness);
                    false
                }
            };
            if should_break {
                break;
            }
        }
    }

    fn handle_command(&mut self, cmd: Command) -> bool {
        match cmd {
            // CRITICAL-1: Start is rejected outright once shutdown has
            // begun — it would register + spawn a child the shutdown
            // aggregation (computed from `online` ids at the moment it ran)
            // can never know to kill, orphaning it after the actor exits.
            Command::Start { apps, reply } => {
                let result = if self.shutting_down {
                    Err(SupervisorError::EngineStopped)
                } else {
                    self.do_start(apps)
                };
                let _ = reply.send(result);
                false
            }
            Command::List { reply } => {
                let _ = reply.send(self.snapshot_all());
                false
            }
            // Neither is rejected while `shutting_down`, unlike Start and
            // Restart: a reopen and a flush each register nothing and spawn
            // nothing, so there is no child either could leave outside the
            // shutdown aggregation.
            Command::Reopen { selector, reply } => {
                self.handle_reopen(&selector, reply);
                false
            }
            Command::Flush { selector, reply } => {
                self.handle_flush(&selector, reply);
                false
            }
            Command::Stop { selector, reply } => {
                self.begin_manual(
                    selector,
                    ManualKind::Stop,
                    CommandOrigin::Operator,
                    ReplyKind::Info(reply),
                );
                false
            }
            // CRITICAL-1: Restart is rejected outright once shutdown has
            // begun, for the same reason as Start — its forced respawn
            // (handle_exited's manual-Restart branch, or apply_immediate's)
            // would spawn a child outside the shutdown aggregation.
            Command::Restart {
                selector,
                origin,
                reply,
            } => {
                if self.shutting_down {
                    send_reply(ReplyKind::Info(reply), Err(SupervisorError::EngineStopped));
                } else {
                    self.begin_manual(
                        selector,
                        ManualKind::Restart,
                        origin,
                        ReplyKind::Info(reply),
                    );
                }
                false
            }
            Command::ExtraRestart { id, pid } => {
                self.handle_extra_restart(id, pid);
                false
            }
            // Rejected while `shutting_down` for CRITICAL-1's reason, the
            // same one that rejects Start and Restart: a reload spawns, and
            // the replacement would be a child outside the shutdown
            // aggregation's `online` snapshot.
            Command::Reload { selector, reply } => {
                if self.shutting_down {
                    let _ = reply.send(Err(SupervisorError::EngineStopped));
                } else {
                    self.handle_reload(&selector, reply);
                }
                false
            }
            Command::Delete { selector, reply } => {
                self.begin_manual(
                    selector,
                    ManualKind::Delete,
                    CommandOrigin::Operator,
                    ReplyKind::Ids(reply),
                );
                false
            }
            Command::Shutdown { reply } => self.begin_shutdown(reply),
        }
    }

    /// Expands each app through `instance_slots` + `assemble`, spawning one
    /// instance per slot. Already-registered entries persist even when a
    /// later spawn in the batch fails.
    fn do_start(&mut self, apps: Vec<ResolvedApp>) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let mut results = Vec::new();
        for app in apps {
            let name = app.config().name.clone();
            // Resolved once per app in this Start batch, not once per
            // instance: every instance of the same app shares one identity,
            // and respawn() reuses this same value from ProcessEntry for
            // every future restart instead of re-touching the passwd
            // database (crate::privilege's module doc).
            let credentials = privilege::resolve(app.config())
                .map_err(|err| SupervisorError::SpawnFailed(err.to_string()))?;
            let mut existing: Vec<u32> = self
                .sheep
                .values()
                .filter(|slot| slot.entry.spec.config().name == name)
                .map(|slot| slot.entry.instance)
                .collect();
            existing.sort_unstable();
            let slots = instance_slots(&existing, app.config().instances);

            for instance in slots {
                match self.spawn_fresh(&app, instance, credentials) {
                    Ok(info) => results.push(info),
                    Err(message) => return Err(SupervisorError::SpawnFailed(message)),
                }
            }
        }
        Ok(results)
    }

    /// Registers + spawns one brand-new instance (a fresh id, `restarts: 0`).
    ///
    /// Always inserts a [`SheepSlot`] before returning, so the entry
    /// persists regardless of the outcome: on success, `Starting` with a
    /// readiness task armed when the app configures `wait_ready` or
    /// `readiness_probe`, `Online` immediately otherwise; `Errored` with no
    /// task on failure.
    fn spawn_fresh(
        &mut self,
        app: &ResolvedApp,
        instance: u32,
        credentials: Option<Credentials>,
    ) -> Result<ProcessInfo, String> {
        let spec = assemble(app, instance, &self.paths, credentials);
        let id = self.next_id;
        self.next_id += 1;

        // Kept off the assembled spec rather than recomputed: the assembler
        // is the only place that knows whether the app set an explicit
        // out_file/err_file or takes the merge_logs-dependent default, and
        // these are the exact paths the child is about to write to. Both
        // arms below register an entry, so both need them.
        let out_file = spec.out_file.clone();
        let err_file = spec.err_file.clone();

        // `app` came through `normalize` (it is a `ResolvedApp`), which
        // already runs `ProbeTarget::parse` over `readiness_probe` — an
        // `Err` here would mean the daemon adopted an app that skipped that
        // step.
        let source = ReadinessSource::of(app.config())
            .expect("ResolvedApp already passed ProbeTarget::parse in normalize");
        let gated = !matches!(source, ReadinessSource::Heuristic);

        match self.runner.spawn(&spec) {
            Ok((proc, io)) => {
                let pid = proc.pid();
                let status = if gated {
                    ProcStatus::Starting
                } else {
                    ProcStatus::Online
                };
                let entry = ProcessEntry {
                    id,
                    spec: app.clone(),
                    instance,
                    status,
                    pid: Some(pid),
                    restarts: 0,
                    started_at: Some(tokio::time::Instant::now()),
                    budget: RestartBudget::default(),
                    reload: ReloadState::None,
                    credentials,
                    out_file,
                    err_file,
                };
                let info = to_info(&entry);
                let log_ctl = io.log_ctl.clone();
                let ctl = spawn_sheep_task::<R::Proc>(
                    id,
                    proc,
                    io,
                    app.clone(),
                    self.events.clone(),
                    self.tx.clone(),
                );
                let ready_tx = if gated {
                    Some(spawn_readiness_task(
                        id,
                        0,
                        // A `Start` is always a caller's own doing, gated or
                        // not, so this matches the `manually: true` the
                        // ungated arm below emits.
                        true,
                        source,
                        app.config().listen_timeout.as_duration(),
                        spec_prober(&spec),
                        self.tx.clone(),
                    ))
                } else {
                    None
                };
                self.sheep.insert(
                    id,
                    SheepSlot {
                        entry,
                        ctl: Some(ctl),
                        log_ctl: Some(log_ctl),
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
                        ready_tx,
                    },
                );
                self.emit(ProcessEventKind::Start, info.clone(), true);
                // For a gated app, `Online` fires later — from
                // `handle_ready_result`, once the readiness task above
                // resolves — not here. `Start` above is still the bus's
                // first word on this sheep, so a subscriber is never left
                // silent for the whole readiness window.
                if !gated {
                    self.went_online(id, info.clone(), true);
                }
                Ok(info)
            }
            Err(error) => {
                let entry = ProcessEntry {
                    id,
                    spec: app.clone(),
                    instance,
                    status: ProcStatus::Errored,
                    pid: None,
                    restarts: 0,
                    started_at: None,
                    budget: RestartBudget::default(),
                    reload: ReloadState::None,
                    credentials,
                    out_file,
                    err_file,
                };
                let info = to_info(&entry);
                self.sheep.insert(
                    id,
                    SheepSlot {
                        entry,
                        ctl: None,
                        log_ctl: None,
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
                        ready_tx: None,
                    },
                );
                self.emit(ProcessEventKind::Errored, info, true);
                Err(error.to_string())
            }
        }
    }

    /// Respawns an already-registered id in place: reassembles from its
    /// stored spec + instance, bumps `restarts` and resets timing on
    /// success, or marks the entry `Errored` on failure. Used for both the
    /// crash loop's own respawn (`RestartDue`) and the forced one a
    /// `Restart` command produces.
    ///
    /// `manually` is what the `Restart`, `Online` and `Errored` events this
    /// respawn emits report about who caused it, and it is narrower than
    /// "forced": `true` only for an operator's `Restart`, `false` for the
    /// crash loop AND for every restart the daemon raised itself — a cron
    /// occurrence, a change under a watched tree, a memory breach, a liveness
    /// failure. Callers pass `origin == CommandOrigin::Operator`, never a
    /// literal, wherever a command is what got them here.
    fn respawn(&mut self, id: u32, manually: bool) -> ProcessInfo {
        let slot = self.sheep.get(&id).expect("respawn: unknown id");
        let app = slot.entry.spec.clone();
        let instance = slot.entry.instance;
        // Reused as-is from the initial Start (never re-resolved): a
        // restart must never re-touch the passwd database, and must never
        // silently change identity out from under an already-running app.
        let credentials = slot.entry.credentials;
        // Computed ahead of the mutable borrow below (IMPORTANT-3): this
        // respawn's new epoch, one past the slot's current one.
        let next_epoch = slot.epoch + 1;
        let spec = assemble(&app, instance, &self.paths, credentials);
        let source = ReadinessSource::of(app.config())
            .expect("ResolvedApp already passed ProbeTarget::parse in normalize");
        let gated = !matches!(source, ReadinessSource::Heuristic);

        match self.runner.spawn(&spec) {
            Ok((proc, io)) => {
                let pid = proc.pid();
                let log_ctl = io.log_ctl.clone();
                let ctl = spawn_sheep_task::<R::Proc>(
                    id,
                    proc,
                    io,
                    app.clone(),
                    self.events.clone(),
                    self.tx.clone(),
                );
                let ready_tx = if gated {
                    Some(spawn_readiness_task(
                        id,
                        next_epoch,
                        // Carried, not defaulted: whether this respawn was a
                        // caller's `Restart` or the crash loop's own doing is
                        // a fact about the respawn, and a gated app must
                        // report it the same way the ungated arm below does.
                        manually,
                        source,
                        app.config().listen_timeout.as_duration(),
                        spec_prober(&spec),
                        self.tx.clone(),
                    ))
                } else {
                    None
                };
                let slot = self
                    .sheep
                    .get_mut(&id)
                    .expect("respawn: entry vanished mid-respawn");
                slot.entry.status = if gated {
                    ProcStatus::Starting
                } else {
                    ProcStatus::Online
                };
                slot.entry.pid = Some(pid);
                slot.entry.started_at = Some(tokio::time::Instant::now());
                slot.entry.restarts += 1;
                slot.ctl = Some(ctl);
                slot.log_ctl = Some(log_ctl);
                // IMPORTANT-3: a new process now exists for this id — any
                // RestartDue timer, or readiness task, scheduled before this
                // point (targeting the process this replaced) is stale the
                // moment it fires. Dropping the old `ready_tx` here (the
                // assignment below) also lets a still-pending OLD readiness
                // task discover its sender is gone; it rides out its own
                // deadline instead of resolving early (`await_ready`'s
                // `Channel` arm), and its eventual `ReadyResult` is dropped
                // by `handle_ready_result`'s epoch check.
                slot.epoch += 1;
                debug_assert_eq!(slot.epoch, next_epoch);
                slot.ready_tx = ready_tx;
                let info = to_info(&slot.entry);
                self.emit(ProcessEventKind::Restart, info.clone(), manually);
                // Same gap as `spawn_fresh`'s Ok arm (see its own comment):
                // for a gated app `Online` fires later, from
                // `handle_ready_result`, not here.
                if !gated {
                    self.went_online(id, info.clone(), manually);
                }
                info
            }
            Err(_error) => {
                // The deferred aggregation reply has no per-id error slot
                // (locked model): a failed respawn simply lands the entry in
                // Errored, same as a budget-exhausted crash loop.
                let slot = self
                    .sheep
                    .get_mut(&id)
                    .expect("respawn: entry vanished mid-respawn");
                slot.entry.status = ProcStatus::Errored;
                slot.entry.pid = None;
                slot.entry.started_at = None;
                slot.ctl = None;
                slot.ready_tx = None;
                let info = to_info(&slot.entry);
                self.emit(ProcessEventKind::Errored, info.clone(), manually);
                // The same terminal status `Decision::Errored` reaches, and it
                // needs the same disarm: a respawn that fails to spawn (the
                // binary replaced mid-deploy, `EAGAIN`, a cwd that is gone)
                // would otherwise leave the name-group's cron worker and watch
                // live, and the enforcer armed against a pid that no longer
                // exists. Re-disarming an already-disarmed id is a no-op by
                // construction, so this is safe from every caller.
                self.disarm_extras(id, &info.name);
                info
            }
        }
    }

    /// Offers `manual` the `manual` marker on a RUNNING sheep, starting its
    /// kill ladder if nothing else already has.
    ///
    /// IMPORTANT-4: chosen semantics — the FIRST manual command to reach a
    /// running sheep owns its `manual` marker and its one live Kill; a later
    /// command racing in against the SAME in-flight kill does not overwrite
    /// that marker or send a second Kill. It just rides the same eventual
    /// `Msg::Exited` to whatever terminal state the FIRST command's intent
    /// produces, so both callers get the SAME honest terminal snapshot instead
    /// of one of them being lied to (the old last-writer-wins bug handed a
    /// `stop()` caller back an `Online` `ProcessInfo`). A `stop()` that lands
    /// first still wins over a racing `restart()`, and vice versa.
    ///
    /// With ONE carve-out, and it is not a fairness question: an operator's
    /// command takes the marker off an in-flight AUTOMATIC restart. First
    /// command wins is fair between two operators who are each owed an
    /// answer, and a memory breach, a liveness failure, a cron occurrence or
    /// a watched file changing is not one — nobody is behind any of them,
    /// while the operator's `stop` is the only party waiting. Without the
    /// carve-out that `stop` came back `Online`, having been silently
    /// converted into the restart it raced, which is precisely the lie the
    /// rule above exists to prevent.
    ///
    /// Automatic-versus-automatic and operator-versus-operator both keep the
    /// plain first-command-wins dedupe, and an automatic restart never takes
    /// the marker off anything.
    ///
    /// CRITICAL-2: taking the marker over does NOT send a second Kill — the
    /// first command's ladder is already running, and the sheep stopped
    /// draining its ctl mailbox the moment it started. Only a sheep that had
    /// no marker at all gets one.
    ///
    /// `cap` decides how long the ladder this may start waits before
    /// `SIGKILL`, and is consulted only on the arm that actually sends a
    /// `Kill`. A command that rides an already-running ladder inherits the
    /// cap that ladder started under: the timeout is already ticking inside
    /// the sheep task, and nothing here can revise it.
    fn claim_manual(&mut self, id: u32, manual: PendingManual, cap: LadderCap) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        match slot.manual.map(|in_flight| in_flight.origin) {
            // Nothing has claimed this sheep's next exit yet, so this command
            // owns it — and starts the one kill ladder that will produce it.
            None => {
                slot.manual = Some(manual);
                // CRITICAL-2: try_send, never `.await`. The sheep task stops
                // draining its ctl mailbox the moment it starts the kill
                // ladder, so a blocking send here could park the actor for up
                // to `kill_timeout` — or, with the mailbox-full tail (a flood
                // of commands after this one), deadlock forever (actor parked
                // in `ctl.send()`, sheep parked in `actor_tx.send()`, neither
                // drains the other). `Full`/`Closed` are both fine to ignore:
                // a Kill already queued means the ladder is already running (a
                // second would be redundant); `Closed` means the sheep already
                // exited and its own `Msg::Exited` is already in flight (or
                // about to be).
                if let Some(ctl) = &slot.ctl {
                    let grace = cap.of(slot.entry.spec.config());
                    let _ = ctl.try_send(SheepCtl::Kill { grace });
                }
            }
            // The carve-out: take the marker, and leave the ladder the
            // automatic restart already started running.
            Some(CommandOrigin::Automatic) if manual.origin == CommandOrigin::Operator => {
                slot.manual = Some(manual);
            }
            // Already claimed by an operator, or by an automatic restart this
            // command has no standing to displace: ride that one's outcome.
            // Both variants are named rather than wildcarded so that a third
            // origin has to be ruled on here instead of inheriting this one.
            Some(CommandOrigin::Operator | CommandOrigin::Automatic) => {}
        }
    }

    /// Resolves `selector`, then either defers to each matched sheep's next
    /// exit (if running) or applies the command immediately (if not).
    ///
    /// Mixed selectors (some matched ids already terminal, some still
    /// running) are handled uniformly: immediate results are collected into
    /// `results` up front and folded into the SAME `PendingReply` as the
    /// deferred ids, so the reply carries every match and only fires once
    /// the last running one goes terminal too (confirmed by probe C's
    /// equivalent scenario — no code change needed there, this comment is
    /// the "why it already works").
    fn begin_manual(
        &mut self,
        selector: ProcessSelector,
        kind: ManualKind,
        origin: CommandOrigin,
        reply: ReplyKind,
    ) {
        let matched: Vec<u32> = self
            .sheep
            .iter()
            .filter_map(|(id, slot)| {
                let config = slot.entry.spec.config();
                selector
                    .matches(&config.name, *id, config.fold.as_deref())
                    .then_some(*id)
            })
            .collect();

        if matched.is_empty() {
            send_reply(reply, Err(SupervisorError::NotFound));
            return;
        }

        let mut remaining = HashSet::new();
        let mut results = Vec::new();

        for id in matched {
            // An automatic restart is held off BOTH halves of an in-flight
            // swap. A reload's whole point is the overlap, and a cron
            // occurrence or a watched file landing inside it destroys that
            // from either side: killing the drainee abandons the reload, and
            // killing the replacement abandons it just as surely — the deploy
            // becomes the ordinary hard restart the feature exists to avoid.
            // For a `watch` app, the archetypal reload-often one, any save
            // inside the readiness window did it.
            //
            // Dropping the trigger costs nobody an answer, which is
            // `claim_manual`'s own carve-out argument applied one step
            // earlier: an operator's command is the only one with a party
            // waiting behind it. And the replacement is a process spawned
            // moments ago, so it already carries whatever the trigger wanted
            // picked up. Instances of the app the reload has not reached yet
            // are not half of any swap and restart as usual.
            //
            // The other two automatic triggers — a memory breach and a
            // liveness failure — need nothing here: they arrive through
            // `Msg::ExtraRestart`, whose guard rejects anything that is not
            // `Online`, and neither half of a swap is.
            let held_off_by_a_swap = origin == CommandOrigin::Automatic
                && self
                    .sheep
                    .get(&id)
                    .is_some_and(|slot| slot.entry.reload != ReloadState::None);
            if held_off_by_a_swap {
                continue;
            }
            let is_running = self.sheep.get(&id).is_some_and(|slot| slot.ctl.is_some());
            if is_running {
                // Whoever ends up owning the marker (see `claim_manual`), this
                // id joins `remaining` below and this command is answered off
                // the same eventual Msg::Exited.
                self.claim_manual(id, PendingManual { kind, origin }, LadderCap::Stop);
                if kind == ManualKind::Delete {
                    // Regardless of which command's `manual` marker won,
                    // this id must still be deregistered once it goes
                    // terminal — see the SheepSlot::pending_delete doc.
                    if let Some(slot) = self.sheep.get_mut(&id) {
                        slot.pending_delete = true;
                    }
                }
                remaining.insert(id);
            } else if let Some(info) = self.apply_immediate(id, kind, origin) {
                results.push(info);
            }
        }

        if remaining.is_empty() {
            results.sort_unstable_by_key(|info| info.id);
            send_reply(reply, Ok(results));
            return;
        }

        self.pending.push(PendingReply {
            remaining,
            results,
            reply,
        });
    }

    /// Applies a manual command synchronously to a matched sheep that has no
    /// live task right now (already `Stopped`/`Errored`/`WaitingRestart`).
    ///
    /// Two of these three arms are terminal transitions, and both disarm.
    /// `handle_exited`'s branches cannot cover them: a sheep waiting out its
    /// restart backoff still holds every extra its last `Online` armed (that
    /// is deliberate — the respawn re-arms them, and the pid guard drops
    /// anything reported in between), and its exit already happened, so
    /// stopping or deleting it HERE is the moment it goes terminal. Without
    /// this, `shep stop web` issued during a backoff leaves the group's
    /// watcher and cron worker armed, and the next file save or occurrence
    /// brings back a sheep the user stopped.
    ///
    /// `origin` is carried for one reason, the same one it is carried into
    /// `handle_exited` for: the `manually` flag on the events below. A cron
    /// occurrence or a watched file landing on a name whose instances are
    /// mid-backoff restarts them from HERE, not from `handle_exited`, so
    /// hardcoding `true` here would leave exactly that case lying about who
    /// caused it.
    fn apply_immediate(
        &mut self,
        id: u32,
        kind: ManualKind,
        origin: CommandOrigin,
    ) -> Option<ProcessInfo> {
        let manually = origin == CommandOrigin::Operator;
        match kind {
            ManualKind::Stop => {
                let slot = self.sheep.get_mut(&id)?;
                match slot.entry.status {
                    // WaitingRestart: cancels the pending restart —
                    // `handle_restart_due` only respawns an id still in
                    // WaitingRestart (and, since IMPORTANT-3, only a
                    // still-current epoch).
                    //
                    // Errored (MINOR-5, pm2 parity): `stop` on an
                    // already-errored sheep still lands it in `Stopped`
                    // rather than being a silent no-op — matches pm2's
                    // `stop` clearing the errored flag instead of leaving
                    // the sheep in limbo.
                    ProcStatus::WaitingRestart | ProcStatus::Errored => {
                        slot.entry.status = ProcStatus::Stopped;
                        let info = to_info(&slot.entry);
                        self.emit(ProcessEventKind::Stop, info.clone(), manually);
                        self.disarm_extras(id, &info.name);
                        Some(info)
                    }
                    _ => Some(to_info(&slot.entry)),
                }
            }
            ManualKind::Delete => {
                let slot = self.sheep.remove(&id)?;
                let info = to_info(&slot.entry);
                self.emit(ProcessEventKind::Delete, info.clone(), manually);
                self.disarm_extras(id, &info.name);
                Some(info)
            }
            ManualKind::Restart => {
                self.sheep.get_mut(&id)?.entry.budget.reset();
                Some(self.respawn(id, manually))
            }
        }
    }

    /// Accepts a reload: answers the caller at once, then starts one swap per
    /// matched app.
    ///
    /// # Why the answer is an acceptance and not a result
    ///
    /// One instance costs `listen_timeout` + `graceful_timeout` ≈ 11 s in the
    /// worst case, and `crate::rpc`'s ceiling on a request budget is 60 s, so
    /// six instances cannot be covered by any reply the caller is allowed to
    /// wait for. Expiring a budget bounds the REPLY and not the actor's work,
    /// so a synchronous reload would routinely time out while still running —
    /// the worst of both. The answer here is therefore the matched sheep as
    /// they stood at the moment the reload was accepted, sent before the first
    /// replacement is spawned; the swaps report themselves on the bus.
    ///
    /// # What gets replaced
    ///
    /// Only an `Online` instance. Everything else the selector matched is a
    /// no-op success, listed in the answer alongside the rest: a reload's
    /// contract is "replace a serving instance so the app stays reachable",
    /// and an instance that is not serving has nothing to keep reachable —
    /// starting one would also surprise an operator who deliberately stopped
    /// it, and `shep start` is the verb for that. This is wider than "not
    /// running": a `Starting` sheep is excluded too, because putting a second
    /// live process in its slot buys the overlap nobody can use yet.
    ///
    /// Nothing here re-reads configuration. The replacement is assembled from
    /// the drainee's stored [`ResolvedApp`] and the credentials resolved at
    /// the first `Start`, which is what [`ProcessEntry::credentials`]' own
    /// once-only rule requires; a config-rereading verb would be a different
    /// feature with a different argument shape.
    fn handle_reload(
        &mut self,
        selector: &ProcessSelector,
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    ) {
        let mut matched: Vec<u32> = self
            .sheep
            .iter()
            .filter_map(|(id, slot)| {
                let config = slot.entry.spec.config();
                selector
                    .matches(&config.name, *id, config.fold.as_deref())
                    .then_some(*id)
            })
            .collect();
        matched.sort_unstable();

        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        // Refused whole, before anything is spawned: see
        // `SupervisorError::ReloadInFlight` for why a partly-accepted
        // selector is worse than a refused one.
        let in_flight = matched.iter().find_map(|id| {
            let name = &self.sheep[id].entry.spec.config().name;
            self.reloads.contains_key(name).then(|| name.clone())
        });
        if let Some(name) = in_flight {
            let _ = reply.send(Err(SupervisorError::ReloadInFlight(name)));
            return;
        }

        let accepted: Vec<ProcessInfo> = matched
            .iter()
            .map(|id| to_info(&self.sheep[id].entry))
            .collect();

        // Grouped by app because a reload runs one instance of an app at a
        // time, and ordered by instance slot because that is the order an
        // operator reads a clustered app in — an id order would be the same
        // thing until the first respawn and then quietly stop being.
        let mut queues: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
        for id in matched {
            let entry = &self.sheep[&id].entry;
            if entry.status != ProcStatus::Online {
                continue;
            }
            queues
                .entry(entry.spec.config().name.clone())
                .or_default()
                .push((entry.instance, id));
        }

        let _ = reply.send(Ok(accepted));

        for (name, mut instances) in queues {
            instances.sort_unstable();
            let queue = instances.into_iter().map(|(_, id)| id).collect();
            self.advance_reload(&name, queue);
        }
    }

    /// Starts the next swap of `name`'s reload, or ends the reload when
    /// `queue` runs out.
    ///
    /// The one door into `SpawnNew`, used both to begin a reload and to carry
    /// it on after each drainee is reaped, so "one instance at a time" is a
    /// property of this function rather than a rule spread over its callers.
    ///
    /// An instance that stopped being `Online` between acceptance and its turn
    /// is skipped and the reload carries on — an operator stopping one
    /// instance is not a reason to abandon the others. A replacement that
    /// cannot be SPAWNED is the opposite and ends the reload, per spec §4:
    /// failure of a new instance aborts the rest and leaves the old instances
    /// running.
    fn advance_reload(&mut self, name: &str, mut queue: VecDeque<u32>) {
        // CRITICAL-1, defence in depth: a shutdown clears every job before
        // anything can reach here, so this only fires if that ever stops
        // being true — and a replacement spawned now would be a child outside
        // the shutdown aggregation.
        if self.shutting_down {
            self.reloads.remove(name);
            return;
        }
        while let Some(old_id) = queue.pop_front() {
            let replaceable = self
                .sheep
                .get(&old_id)
                .is_some_and(|slot| slot.entry.status == ProcStatus::Online);
            if !replaceable {
                continue;
            }
            match self.spawn_replacement(old_id) {
                Ok(new_id) => {
                    self.reloads.insert(
                        name.to_string(),
                        ReloadJob {
                            queue,
                            swap: ReloadSwap {
                                old_id,
                                new_id,
                                phase: ReloadPhase::AwaitReady,
                            },
                        },
                    );
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        name,
                        old_id,
                        error,
                        "reload abandoned: the replacement could not be spawned"
                    );
                    self.reloads.remove(name);
                    return;
                }
            }
        }
        self.reloads.remove(name);
    }

    /// `SpawnNew`: registers and spawns one replacement for the instance
    /// `old_id` occupies, and hands back its id.
    ///
    /// Three things separate this from [`Self::spawn_fresh`], and each is
    /// load-bearing rather than incidental:
    ///
    /// - **The same instance slot.** [`assemble`] writes the slot number into
    ///   the child's environment (`SHEP_INSTANCE`, or the app's own
    ///   `increment_var`), and an app that derives its port from it would bind
    ///   a DIFFERENT port under a different slot — no overlap, no handover,
    ///   nothing for the feature to be about. The slot also fixes the log
    ///   paths and the prober's environment, so all three follow the drainee.
    /// - **A new id.** Two live processes for one id is the invariant the
    ///   supervisor's property test asserts over the event stream, and a
    ///   same-id replacement breaks it outright. Same slot, new id is the only
    ///   combination that satisfies both requirements, and reload is the first
    ///   operation for which the two diverge.
    /// - **Readiness is always gated.** [`Self::spawn_fresh`] and
    ///   [`Self::respawn`] arm a readiness task only for a `Channel` or
    ///   `Probe` app, so a `Heuristic` one has none at all. A replacement
    ///   without a readiness wait would have nothing to hold `DrainOld` back,
    ///   and the drainee would be killed the instant the new process existed
    ///   — an ordinary restart with extra steps. `await_ready`'s `Heuristic`
    ///   arm exists for exactly this caller.
    ///
    /// `restarts` carries over from the drainee: a reload is not a crash, but
    /// the count is an operator's view of that instance's history and losing
    /// it at every deploy would make the number useless. The restart BUDGET
    /// does not carry over — a fresh instance starts with a clean one, which
    /// is the reset spec §4 grants a manual reload.
    ///
    /// # Why the drainee is marked before the spawn
    ///
    /// `Stopping` goes on the drainee before `runner.spawn` is called, and
    /// therefore before the replacement's `Start` reaches the bus. Two
    /// separate things depend on that order:
    ///
    /// - **The muster roll.** `snapshot.rs`'s `is_running` counts `Online |
    ///   Starting | WaitingRestart`, and its writer arms a debounce on the
    ///   first lifecycle event of a burst — the replacement's `Start` is one.
    ///   With both entries passing `is_running` the roll would record a count
    ///   the flock does not have. `Stopping` is not in that set.
    /// - **The liveness window.** A drainee keeps its liveness probe armed
    ///   for the whole overlap, and `claim_manual` only drops an automatic
    ///   report once something holds the sheep's `manual` marker — which
    ///   nothing does until `DrainOld`. That leaves the whole `AwaitReady`
    ///   window, up to `listen_timeout`, in which a failing probe would
    ///   RESTART the instance shep is in the middle of replacing. Claiming
    ///   the marker early is not the fix: `claim_manual` sends the `Kill`
    ///   along with the marker, which would end the drainee before its
    ///   replacement is ready. The status is, for the two automatic triggers
    ///   that arrive through `Msg::ExtraRestart` — a memory breach and a
    ///   liveness failure — because `handle_extra_restart`'s guard rejects
    ///   anything that is not `Online`. So the transition the roll needs
    ///   closes those two in the same stroke. It closes neither of the other
    ///   two: a cron occurrence and a watched file reach `begin_manual`,
    ///   which reads no status at all, so they are held off by that
    ///   function's own carve-out on the `reload` marker instead.
    ///
    /// The mark is undone if the spawn fails, and nothing can observe it in
    /// between — the actor is synchronous here and emits no event.
    fn spawn_replacement(&mut self, old_id: u32) -> Result<u32, String> {
        let drainee = &self.sheep[&old_id].entry;
        let app = drainee.spec.clone();
        let instance = drainee.instance;
        // `Credentials` is `Copy`; reused, never re-resolved.
        let credentials = drainee.credentials;
        let restarts = drainee.restarts;
        let old_pid = drainee
            .pid
            .expect("spawn_replacement: an Online drainee has a pid");

        let new_id = self.next_id;
        self.next_id += 1;

        let spec = assemble(&app, instance, &self.paths, credentials);
        let out_file = spec.out_file.clone();
        let err_file = spec.err_file.clone();
        let source = ReadinessSource::of(app.config())
            .expect("ResolvedApp already passed ProbeTarget::parse in normalize");

        let drainee = self
            .sheep
            .get_mut(&old_id)
            .expect("spawn_replacement: the drainee was read a moment ago");
        drainee.entry.status = ProcStatus::Stopping;
        drainee.entry.reload = ReloadState::SpawningReplacement { new_id };

        match self.runner.spawn(&spec) {
            Ok((proc, io)) => {
                let pid = proc.pid();
                let entry = ProcessEntry {
                    id: new_id,
                    spec: app.clone(),
                    instance,
                    status: ProcStatus::Starting,
                    pid: Some(pid),
                    restarts,
                    started_at: Some(tokio::time::Instant::now()),
                    budget: RestartBudget::default(),
                    reload: ReloadState::Draining { old_pid },
                    credentials,
                    out_file,
                    err_file,
                };
                let info = to_info(&entry);
                let log_ctl = io.log_ctl.clone();
                let ctl = spawn_sheep_task::<R::Proc>(
                    new_id,
                    proc,
                    io,
                    app.clone(),
                    self.events.clone(),
                    self.tx.clone(),
                );
                let ready_tx = spawn_readiness_task(
                    new_id,
                    0,
                    // A reload is an operator's doing, so the `Online` this
                    // wait defers reports itself as one.
                    true,
                    source,
                    app.config().listen_timeout.as_duration(),
                    spec_prober(&spec),
                    self.tx.clone(),
                );
                self.sheep.insert(
                    new_id,
                    SheepSlot {
                        entry,
                        ctl: Some(ctl),
                        log_ctl: Some(log_ctl),
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
                        ready_tx: Some(ready_tx),
                    },
                );
                self.emit(ProcessEventKind::Start, info, true);
                Ok(new_id)
            }
            Err(error) => {
                // Nothing is registered for a replacement that never existed:
                // the drainee still owns this instance slot, and a permanent
                // `Errored` row beside it would double every name-keyed verb
                // (`stop`, `flush`) and every count taken off the slot for as
                // long as the flock lives. The id is spent all the same — ids
                // are never reused.
                let drainee = self
                    .sheep
                    .get_mut(&old_id)
                    .expect("spawn_replacement: the drainee was marked a moment ago");
                drainee.entry.status = ProcStatus::Online;
                drainee.entry.reload = ReloadState::None;
                Err(error.to_string())
            }
        }
    }

    /// `AwaitReady` resolved for a replacement: commit the swap, or abandon
    /// the reload.
    ///
    /// # Why one mapping covers all three readiness sources
    ///
    /// A reload is the one caller for which a readiness deadline elapsing is
    /// a FAILURE — an ordinary start goes online anyway rather than turning a
    /// slow app into a restart loop, but a replacement that cannot answer has
    /// not proved it can take over, and killing the instance that can would
    /// be the outage the feature exists to avoid.
    ///
    /// That failure is keyed on the [`Readiness`] verdict and never on the
    /// deadline itself, which is what makes it correct for all three sources
    /// at once. `await_ready`'s `Channel` and `Probe` arms report `TimedOut`
    /// when their deadline beats the signal; its `Heuristic` arm reports
    /// `Ready`, because for a heuristic the elapse IS the signal — there is
    /// nothing else it could be waiting for. A handler that asked "did the
    /// deadline elapse" instead of "what did the wait say" would abandon
    /// every reload of every app that configures neither `wait_ready` nor
    /// `readiness_probe`, which is most of them.
    fn reload_ready_result(&mut self, new_id: u32, manually: bool, readiness: Readiness) {
        let Some(name) = self.reload_of(new_id) else {
            // Defensive: nothing leaves a `Draining` marker behind without a
            // job naming it. Take the ordinary transition rather than strand
            // the sheep at `Starting` for the rest of its life.
            tracing::warn!(
                id = new_id,
                "a replacement resolved with no reload to belong to"
            );
            self.clear_reload(new_id);
            let info = self.set_status(new_id, ProcStatus::Online);
            self.went_online(new_id, info, manually);
            return;
        };

        if readiness == Readiness::TimedOut {
            // Abandoning protects the instance that can still serve, so it
            // is only worth doing while there IS one. A drainee that went on
            // its own while this replacement was starting leaves nothing to
            // fall back to, and killing the replacement as well would empty
            // the instance slot outright — no entry, no restart, no report
            // beyond a log line. With nothing to protect, this takes the
            // ordinary readiness rule instead: online anyway, rather than
            // turn a slow start into a missing instance.
            let old_id = self.reloads[&name].swap.old_id;
            if self.sheep.contains_key(&old_id) {
                self.abort_reload(&name, "the replacement was not ready inside listen_timeout");
                return;
            }
            tracing::warn!(
                id = new_id,
                "a replacement's readiness deadline elapsed with no old instance left to keep; \
                 marking online anyway"
            );
        }

        let info = self.set_status(new_id, ProcStatus::Online);
        self.went_online(new_id, info, manually);
        self.begin_drain(&name);
    }

    /// `DrainOld`: the replacement is serving, so ask the instance it
    /// replaced to go.
    ///
    /// The ladder runs under `graceful_timeout` rather than `kill_timeout`
    /// (see [`LadderCap`]): this is the stop that expects the instance to
    /// stop accepting, finish what it already has, and exit — the only part
    /// of the handover shep cannot do on the app's behalf.
    ///
    /// Marks the swap committed FIRST. From here there is no old instance to
    /// return to, so an abandonment from this point on leaves the replacement
    /// where it is instead of trying to undo a kill that is already in
    /// flight.
    fn begin_drain(&mut self, name: &str) {
        let Some(job) = self.reloads.get_mut(name) else {
            return;
        };
        job.swap.phase = ReloadPhase::DrainOld;
        let old_id = job.swap.old_id;

        if !self.sheep.contains_key(&old_id) {
            // The drainee went on its own while the replacement was still
            // starting, so `ReapOld` already happened and there is nothing
            // left to drain.
            self.finish_swap(name);
            return;
        }
        self.claim_manual(
            old_id,
            PendingManual {
                kind: ManualKind::Stop,
                origin: CommandOrigin::Operator,
            },
            LadderCap::Drain,
        );
    }

    /// One instance replaced: the replacement stops being half of a pair, and
    /// the reload moves on to the next instance (or ends).
    fn finish_swap(&mut self, name: &str) {
        let Some(job) = self.reloads.remove(name) else {
            return;
        };
        self.clear_reload(job.swap.new_id);
        self.advance_reload(name, job.queue);
    }

    /// Abandons `name`'s reload: the instance it was replacing goes back to
    /// serving, the instances it had not reached yet are left alone, and the
    /// replacement is killed and deregistered.
    ///
    /// Spec §4: failure of a new instance aborts the rest and keeps the old
    /// instances running. The replacement goes through the kill ladder rather
    /// than being dropped, because a process that got far enough to be spawned
    /// may already have forked lambs — the ladder's `SIGKILL` rung is what
    /// sweeps the group. Its entry is then deregistered rather than left as an
    /// `Errored` row: the instance slot still belongs to the drainee, and a
    /// second permanent row in it would double every name-keyed verb for as
    /// long as the flock lives.
    ///
    /// Only reachable while the swap is still `AwaitReady`; see
    /// [`Self::begin_drain`] for why a committed swap has nothing to undo.
    fn abort_reload(&mut self, name: &str, reason: &str) {
        let Some(job) = self.reloads.remove(name) else {
            return;
        };
        tracing::warn!(
            name,
            old_id = job.swap.old_id,
            new_id = job.swap.new_id,
            reason,
            "reload abandoned"
        );

        if let Some(drainee) = self.sheep.get_mut(&job.swap.old_id) {
            drainee.entry.status = ProcStatus::Online;
            drainee.entry.reload = ReloadState::None;
        }

        let new_id = job.swap.new_id;
        let Some(replacement) = self.sheep.get_mut(&new_id) else {
            return;
        };
        replacement.entry.reload = ReloadState::None;
        if replacement.ctl.is_none() {
            // Already terminal — this abandonment IS its exit being handled,
            // and `handle_exited` deregisters it on the way out. Sending a
            // `Kill` to a task that has ended would claim a marker no exit
            // will ever come to clear.
            return;
        }
        replacement.pending_delete = true;
        self.claim_manual(
            new_id,
            PendingManual {
                kind: ManualKind::Delete,
                origin: CommandOrigin::Operator,
            },
            // A failed start, not a graceful handover: nothing is being
            // drained, so there is no work in hand to wait on.
            LadderCap::Stop,
        );
    }

    /// `ReapOld`: the drainee has exited, so its registration goes with it.
    ///
    /// Nothing else would ever remove it. A drainee is not deleted and does
    /// not respawn, so without this its `SheepSlot` outlives the process
    /// forever — one dead row per instance per reload, each carrying the live
    /// instance's name and both of its log paths.
    ///
    /// Returns what [`Self::resolve_pending`] returned, so an operator's
    /// `stop`/`delete` that was waiting on this exit is still answered.
    fn reap_drainee(&mut self, old_id: u32) -> bool {
        let terminal = self.deregister_on_exit(old_id);
        let Some(name) = self.reload_of(old_id) else {
            return terminal;
        };
        match self.reloads[&name].swap.phase {
            ReloadPhase::DrainOld => self.finish_swap(&name),
            // The drainee died on its own before its replacement was ready.
            // The swap has nothing left to abandon back to, so it is
            // committed here rather than carried on: the replacement finishes
            // its wait, finds no drainee, and the reload moves along.
            ReloadPhase::AwaitReady => {
                self.reloads
                    .get_mut(&name)
                    .expect("reap_drainee: the phase was read a moment ago")
                    .swap
                    .phase = ReloadPhase::DrainOld;
            }
        }
        terminal
    }

    /// Deregisters an id whose `Msg::Exited` is being handled, announcing it
    /// the way every other deregistration is announced.
    fn deregister_on_exit(&mut self, id: u32) -> bool {
        let mut removed = self
            .sheep
            .remove(&id)
            .expect("deregister_on_exit: unknown id");
        removed.entry.status = ProcStatus::Stopped;
        removed.entry.reload = ReloadState::None;
        let info = to_info(&removed.entry);
        self.emit(ProcessEventKind::Delete, info.clone(), true);
        self.disarm_extras(id, &info.name);
        self.resolve_pending(id, info)
    }

    /// The app whose in-flight reload names `id`, in either role.
    fn reload_of(&self, id: u32) -> Option<String> {
        self.reloads
            .iter()
            .find(|(_, job)| job.swap.old_id == id || job.swap.new_id == id)
            .map(|(name, _)| name.clone())
    }

    /// Takes `id` out of any reload it is half of, leaving an ordinary entry.
    fn clear_reload(&mut self, id: u32) {
        if let Some(slot) = self.sheep.get_mut(&id) {
            slot.entry.reload = ReloadState::None;
        }
    }

    /// Resolves `selector` and hands every match to a task that reopens its
    /// log files and then answers the caller.
    ///
    /// # Why the acknowledgements are never awaited here
    ///
    /// Awaiting one from inside the actor loop closes a permanent cycle
    /// (CRITICAL-2): the actor stops draining its mailbox, so a sheep task
    /// blocks in `actor_tx.send`, so nothing drains that sheep's `logs`, so
    /// its pump — the party that owes the acknowledgement — never gets to
    /// answer. This handler is therefore synchronous and does nothing but
    /// collect senders; `spawn_reopen_task` owns every await.
    ///
    /// That task reports to the caller directly rather than back through the
    /// mailbox as a `Msg`, which is where this departs from
    /// [`spawn_readiness_task`]'s shape. A readiness result decides a status
    /// transition, so it has to re-enter the actor to be applied, and has to
    /// be checked against the slot's epoch in case a respawn beat it home. A
    /// reopen changes no actor state at all, and the only party waiting is
    /// the caller — so a return trip through the mailbox would buy nothing
    /// but a second hop and an epoch check with nothing to guard.
    ///
    /// `&self` is that argument made structural: a handler that took `&mut
    /// self` could grow a state change without anything noticing, and the
    /// epoch check waved off above would quietly start being needed. The
    /// compiler re-checks the claim on every build.
    ///
    /// Staleness needs no guard for the same reason. A respawn between this
    /// handler and the send leaves the task holding the previous run's
    /// sender: that pump is already ending, so the send or the acknowledgement
    /// fails and the sheep is reported as the no-op success below — which is
    /// honest, because the replacement process opened its log files after the
    /// rotation and is not holding a renamed inode either way.
    ///
    /// # What a matched sheep with no pump means
    ///
    /// Nothing to reopen, reported as a success rather than an error —
    /// nobody rotating logs wants `reopen all` to fail because one sheep in
    /// the flock is stopped. It reaches the task in one of two shapes, and
    /// they are the same answer: `log_ctl` is `None` because no spawn ever
    /// succeeded for this slot, or the send (or the acknowledgement) fails
    /// because the pump has ended, which is how a stopped sheep normally
    /// presents.
    fn handle_reopen(
        &self,
        selector: &ProcessSelector,
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    ) {
        let mut matched: Vec<(ProcessInfo, Option<mpsc::Sender<LogCtl>>)> = self
            .sheep
            .iter()
            .filter(|(id, slot)| {
                let config = slot.entry.spec.config();
                selector.matches(&config.name, **id, config.fold.as_deref())
            })
            .map(|(_, slot)| (to_info(&slot.entry), slot.log_ctl.clone()))
            .collect();

        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        // Sorted here, where the whole set is in hand, rather than after the
        // reopens: `HashMap` iteration order is arbitrary, and a caller
        // reading the reply as a table wants the same id order `list` gives.
        matched.sort_unstable_by_key(|(info, _)| info.id);
        spawn_reopen_task(matched, reply);
    }

    /// Resolves `selector` and hands every match to a task that flushes its
    /// log pump, truncates its log files and then answers the caller.
    ///
    /// Synchronous and `&self` for the reasons [`Self::handle_reopen`] gives
    /// at length, all of which hold here unchanged: awaiting a pump's
    /// acknowledgement inside the actor loop closes CRITICAL-2's cycle, a
    /// flush changes no actor state, and the compiler re-checks that second
    /// claim on every build.
    ///
    /// # Why the paths come from the entry and not from the pump
    ///
    /// [`ProcessEntry::out_file`]/[`ProcessEntry::err_file`] are what the
    /// assembler resolved at registration, and they are what this truncates
    /// — never the inode the pump currently holds. The two are the same file
    /// right up until an external rotator renames it, and from that moment a
    /// flush that chased the pump's handle would empty the ARCHIVE and leave
    /// the live log untouched: the exact opposite of what was asked, and
    /// silent about it. Being path-based is also what lets a stopped sheep,
    /// which has no pump at all, still be flushed.
    ///
    /// The `PathBuf`s are read rather than `ProcessInfo`'s `out_file`, whose
    /// `String` is lossy by design (see [`to_info`]): a truncate must open
    /// the path the child is really writing to, not a rendering of it.
    ///
    /// # Why the paths are a set
    ///
    /// Several matched sheep can name one path — `merge_logs`, or an
    /// explicit `out_file` on a multi-instance app — and under `O_APPEND` one
    /// truncate empties the file for every handle open on it. Truncating once
    /// per sheep would repeat completed work, and — with every flush already
    /// ahead of every truncate — a second truncate of a path could only
    /// discard what was written between the two truncates, which is a window
    /// this verb makes no promise about in either shape. Deduplicating buys
    /// the work that is not repeated and a failure message that reads the
    /// same every run. The barrier is the phase order below, not this set.
    ///
    /// # Why more pumps are flushed than the reply names
    ///
    /// The truncate set is paths, and a path can have writers the selector
    /// never named — `shep flush 0` on a `merge_logs` app, or on two apps
    /// sharing an explicit `out_file`, empties a file another instance is
    /// also holding open. So every slot writing to a path in the set is
    /// flushed, matched or not. The barrier has to cover every writer to a
    /// file about to be emptied: an unflushed sibling's already-dispatched
    /// `write(2)` lands at offset 0 of the file the operator was just told is
    /// empty, which is the one failure the two phases exist to prevent.
    ///
    /// The reply stays keyed by the selector all the same. It is a
    /// `Vec<ProcessInfo>` — the table `stop` and `reopen` answer with — where
    /// a row means "a sheep you named", not "a file that was emptied"; adding
    /// the sibling would make those two indistinguishable in the one
    /// rendering an operator reads. What happened to the sibling is a fact
    /// about a PATH, and this reply has nowhere to put one.
    fn handle_flush(
        &self,
        selector: &ProcessSelector,
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    ) {
        let mut matched: Vec<ProcessInfo> = Vec::new();
        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
        for (id, slot) in &self.sheep {
            let config = slot.entry.spec.config();
            if !selector.matches(&config.name, *id, config.fold.as_deref()) {
                continue;
            }
            paths.insert(slot.entry.out_file.clone());
            paths.insert(slot.entry.err_file.clone());
            matched.push(to_info(&slot.entry));
        }

        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        let mut pumps: Vec<(u32, mpsc::Sender<LogCtl>)> = self
            .sheep
            .iter()
            .filter(|(_, slot)| {
                paths.contains(&slot.entry.out_file) || paths.contains(&slot.entry.err_file)
            })
            .filter_map(|(id, slot)| slot.log_ctl.clone().map(|log_ctl| (*id, log_ctl)))
            .collect();

        // Both sorted for the reason `handle_reopen` sorts: `HashMap`
        // iteration order is arbitrary, and a caller rendering the reply as a
        // table wants `list`'s id order — while pump failures are reported in
        // the order they are collected, so an unsorted flush set would make a
        // multi-pump failure message read differently run to run. `paths`
        // needs no such step, being a `BTreeSet` already.
        matched.sort_unstable_by_key(|info| info.id);
        pumps.sort_unstable_by_key(|&(id, _)| id);
        let pumps = pumps.into_iter().map(|(_, log_ctl)| log_ctl).collect();
        spawn_flush_task(matched, pumps, paths, reply);
    }

    /// Kills every currently-online sheep, deferring the reply the same way
    /// `Stop` does; returns `true` (break the actor loop) once there was
    /// nothing to wait on, or once the deferred reply's own resolution says
    /// so (propagated back from [`Self::handle_exited`]).
    ///
    /// CRITICAL-1: sets `shutting_down` FIRST, before computing `online` —
    /// every check against it downstream (`Start`/`Restart` rejection,
    /// `handle_restart_due`'s guard) is only meaningful if no new sheep can
    /// register and no `WaitingRestart` sheep can respawn from this point
    /// on, so nothing outside this snapshot of `online` can ever need
    /// killing later.
    fn begin_shutdown(&mut self, reply: oneshot::Sender<()>) -> bool {
        self.shutting_down = true;
        // Every in-flight reload is abandoned here rather than allowed to run
        // out: its next step is always a spawn, and CRITICAL-1 forbids one
        // from this point on. The two entries of a swap need no fixing up —
        // both are `ctl.is_some()`, so both are in the `online` set killed
        // below, and each takes the ordinary path for its own role once no
        // job names it (`handle_exited`).
        self.reloads.clear();

        let online: HashSet<u32> = self
            .sheep
            .iter()
            .filter(|(_, slot)| slot.ctl.is_some())
            .map(|(&id, _)| id)
            .collect();

        if online.is_empty() {
            let _ = reply.send(());
            return true;
        }

        for &id in &online {
            // Same marker rule as `begin_manual` (IMPORTANT-4): an id already
            // mid-kill from an earlier operator Stop/Restart/Delete keeps that
            // command's `manual` marker and doesn't get a redundant Kill,
            // while one held by an automatic restart is taken over. It joins
            // `remaining` below either way.
            self.claim_manual(
                id,
                PendingManual {
                    kind: ManualKind::Stop,
                    origin: CommandOrigin::Operator,
                },
                // A shutdown is a stop, even for an instance a reload was
                // draining: the longer cap buys time to finish work that has
                // nowhere left to be reported, since the engine goes away
                // with it.
                LadderCap::Stop,
            );
        }

        self.pending.push(PendingReply {
            remaining: online,
            results: Vec::new(),
            reply: ReplyKind::Shutdown(reply),
        });
        false
    }

    /// Handles one sheep's terminal exit: computes uptime, consults
    /// `decide_on_exit`, applies the resulting transition, and resolves any
    /// deferred reply waiting on this id. Returns `true` iff this exit just
    /// completed a `Shutdown`'s aggregation (the actor loop should break).
    fn handle_exited(&mut self, id: u32, outcome: ExitOutcome) -> bool {
        let Some(slot) = self.sheep.get_mut(&id) else {
            tracing::warn!(id, "Msg::Exited for an unregistered id");
            return false;
        };
        slot.ctl = None;
        slot.entry.pid = None;
        // Split the moment the marker is taken, because the two halves answer
        // different questions. `kind` decides what this exit BECOMES, and
        // every branch below reads it exactly as it did before origins
        // existed. The origin decides only what the bus SAYS about who caused
        // it, and is read once, on the forced-respawn branch. The Stop and
        // Delete branches stay literal `true`: `Command::Stop`,
        // `Command::Delete` and `begin_shutdown` are the only sites that put
        // either kind on a marker, and all three declare `Operator`.
        let manual = slot.manual.take();
        let kind = manual.map(|pending| pending.kind);
        let pending_delete = std::mem::take(&mut slot.pending_delete);
        // Read off the slot here, with the borrow that is already open,
        // because both branches below hand the actor back to itself.
        let reload = slot.entry.reload;
        let started_at = slot.entry.started_at.take();

        // Neither half of a reload takes the ordinary decision path, and the
        // drainee's case is the sharper one: `decide_on_exit` knows nothing
        // about reloads, so an `autorestart` app's drainee would be respawned
        // straight back into an instance slot its replacement already holds —
        // two live processes for one instance, with no path back.
        match reload {
            ReloadState::SpawningReplacement { .. } => {
                // The drainee's slot belongs to its replacement, so its
                // registration goes with the process — unless an operator's
                // own command reached it first while the swap was still
                // abandonable. That command, not the reload, decides what
                // this exit becomes: `stop` must leave a sheep registered
                // and `Stopped`, and deregistering here would delete an app
                // an operator only asked to stop. A drainee that went with
                // no command behind it (`kind` is `None`) is the reload's
                // business either way, and must not be restarted into a slot
                // its replacement already holds.
                //
                // An operator's is the only command that can be here.
                // `begin_manual` holds every automatic restart off both
                // halves of a swap, and `handle_extra_restart`'s `Online`
                // guard rejects the two triggers that do not come through it,
                // so the warning below can name the operator without hedging.
                let name = self.reload_of(id);
                let abandonable = name
                    .as_deref()
                    .is_some_and(|name| self.reloads[name].swap.phase == ReloadPhase::AwaitReady);
                if !abandonable || kind.is_none() {
                    return self.reap_drainee(id);
                }
                let name = name.expect("a phase was just read off this job");
                self.abort_reload(&name, "an operator's command reached the drainee first");
                // Falls through as an ordinary entry: `abort_reload` has
                // already cleared this one's marker and put its status back.
            }
            ReloadState::Draining { .. } => {
                // Whether this is a failure depends on how far the swap got.
                // Still `AwaitReady` and the replacement never proved it could
                // take over, so the reload is abandoned and the drainee kept.
                // Past that and the swap is committed: this is an ordinary
                // instance now, and its exit is its own restart policy's
                // business.
                let name = self.reload_of(id);
                let abandoning = name
                    .as_deref()
                    .is_some_and(|name| self.reloads[name].swap.phase == ReloadPhase::AwaitReady);
                self.clear_reload(id);
                if abandoning {
                    let name = name.expect("a phase was just read off this job");
                    self.abort_reload(&name, "the replacement exited before it was ready");
                    return self.deregister_on_exit(id);
                }
                // A committed swap normally ends on the drainee's exit, which
                // `reap_drainee` turns into `finish_swap` — but only while
                // there is still a drainee to produce one. A swap `reap_drainee`
                // itself committed has none: it was the drainee's death that
                // committed it, and the deregistration went with it. That left
                // this replacement's readiness result as the last event able to
                // end the job, and clearing its `Draining` marker a line above
                // cancels that too, because `handle_ready_result` routes on the
                // marker. So the job ends here or never, and a job nothing can
                // end refuses every later reload of the app for as long as the
                // daemon runs.
                //
                // The queue goes with it rather than carrying on, per spec §4:
                // this replacement exited before it was ever `Online`, which is
                // a failure of the new instance, and that aborts the rest.
                if let Some(name) = name {
                    let old_id = self.reloads[&name].swap.old_id;
                    if !self.sheep.contains_key(&old_id) {
                        tracing::warn!(
                            name,
                            new_id = id,
                            "reload abandoned: the replacement exited before it was ready, with \
                             the instance it replaced already gone"
                        );
                        self.reloads.remove(&name);
                    }
                }
            }
            ReloadState::None => {}
        }

        let Some(started_at) = started_at else {
            // MINOR-7: shouldn't happen (a duplicate Msg::Exited would
            // violate the one-exit-path invariant) — but resolve any
            // pending reply waiting on this id with a best-effort snapshot
            // instead of leaving its caller parked on `.await` forever.
            tracing::warn!(
                id,
                "Msg::Exited for an entry with no started_at (duplicate?)"
            );
            // Deregistration is NOT reachable here today: `pending_delete`
            // is only ever set for a sheep whose `ctl.is_some()` (see the
            // `is_running` gate in `begin_manual`), which implies a live
            // task and therefore `started_at.is_some()`; and the ONE exit
            // that consumes the flag removes the slot outright, so a second
            // `Msg::Exited` for that id lands in the unregistered-id branch
            // above rather than here. Honoured anyway, because the cost is
            // four lines and the alternative failure is silent: the
            // `std::mem::take` above has already consumed both markers, so
            // a future change to WHEN `pending_delete` is set would drop a
            // Delete on the floor while telling its caller it succeeded.
            //
            // The disarm below is honoured for exactly the same reason and at
            // the same price: deregistering without it would leave the name
            // group's cron worker and watch firing at a name `list()` no
            // longer knows.
            if kind == Some(ManualKind::Delete) || pending_delete {
                let mut removed = self.sheep.remove(&id).expect("checked above");
                removed.entry.status = ProcStatus::Stopped;
                let info = to_info(&removed.entry);
                self.emit(ProcessEventKind::Delete, info.clone(), true);
                self.disarm_extras(id, &info.name);
                return self.resolve_pending(id, info);
            }
            let info = to_info(&self.sheep.get(&id).expect("checked above").entry);
            return self.resolve_pending(id, info);
        };
        let uptime = tokio::time::Instant::now().saturating_duration_since(started_at);

        // A manual Restart normally forces a respawn (spec §4: manual
        // action resets budget), regardless of what `decide_on_exit` would
        // say — `manual_stop = true` would otherwise make it choose
        // CleanStop. CRITICAL-1: NOT while shutting down, though — this
        // branch can still be reached then (a Restart that landed and sent
        // its Kill just before Shutdown began; Shutdown's own dedupe skips
        // sending a second Kill for an id whose `manual` is already set),
        // and respawning here would spawn a child outside the shutdown
        // aggregation's `online` snapshot, orphaning it exactly like the
        // rejected-at-the-command-level cases. Falling through instead: with
        // `kind.is_some()` true, `decide_on_exit` always resolves to
        // CleanStop, landing the sheep in `Stopped` — an honest answer for
        // a restart request that lost the race to a shutdown.
        //
        // Adversarial finding #2 follow-up: also NOT when `pending_delete`
        // is set — this is the ONLY place in `handle_exited` that can
        // resolve an exit without ever consulting `decide_on_exit` (every
        // other path reaches the `Decision::CleanStop if ... || pending_delete`
        // guard below), so it's the one branch a Delete racing a Restart
        // could slip past. Respawning here would hand back a brand-new LIVE
        // process while telling the Delete caller it succeeded — worse than
        // the original bug (a merely-stale `Stopped` entry), since it also
        // shows up in `list()` as `Online`. Falling through instead lets
        // `decide_on_exit` resolve to CleanStop (`kind.is_some()` is still
        // true) and the `pending_delete` guard below correctly deregister.
        if kind == Some(ManualKind::Restart) && !self.shutting_down && !pending_delete {
            // Re-fetched rather than carried down from the top of the
            // function: the reload branches above hand `self` back to itself,
            // which ends the borrow the slot was read through.
            self.sheep
                .get_mut(&id)
                .expect("checked above")
                .entry
                .budget
                .reset();
            // The one place the origin is read. A person's `shep restart` is a
            // user action; a cron occurrence, a change under a watched tree, a
            // memory breach and a liveness failure are the daemon's own doing,
            // and a subscriber told otherwise cannot tell an operator's
            // deploy apart from an app thrashing on its own.
            let manually = matches!(
                manual,
                Some(PendingManual {
                    origin: CommandOrigin::Operator,
                    ..
                })
            );
            let info = self.respawn(id, manually);
            return self.resolve_pending(id, info);
        }

        let decision = {
            let slot = self.sheep.get_mut(&id).expect("checked above");
            decide_on_exit(
                slot.entry.spec.config(),
                &mut slot.entry.budget,
                uptime,
                outcome,
                kind.is_some(),
            )
        };

        let info = match decision {
            Decision::Restart { delay } => {
                let info = self.set_status(id, ProcStatus::WaitingRestart);
                self.emit(ProcessEventKind::Exit, info.clone(), false);
                // IMPORTANT-3: capture the CURRENT epoch so the timer this
                // schedules can tell, when it fires, whether it's still the
                // authoritative one for this id (see `respawn`/`SheepSlot`).
                let epoch = self.sheep.get(&id).expect("checked above").epoch;
                self.schedule_restart(id, epoch, delay);
                info
            }
            Decision::Errored => {
                let info = self.set_status(id, ProcStatus::Errored);
                self.emit(ProcessEventKind::Errored, info.clone(), kind.is_some());
                self.disarm_extras(id, &info.name);
                info
            }
            Decision::CleanStop if kind == Some(ManualKind::Delete) || pending_delete => {
                let mut removed = self.sheep.remove(&id).expect("checked above");
                removed.entry.status = ProcStatus::Stopped;
                let info = to_info(&removed.entry);
                self.emit(ProcessEventKind::Delete, info.clone(), true);
                self.disarm_extras(id, &info.name);
                info
            }
            Decision::CleanStop => {
                let info = self.set_status(id, ProcStatus::Stopped);
                self.disarm_extras(id, &info.name);
                self.emit(
                    ProcessEventKind::Stop,
                    info.clone(),
                    kind == Some(ManualKind::Stop),
                );
                info
            }
        };

        self.resolve_pending(id, info)
    }

    /// A memory breach or a liveness failure asked for a restart. Guarded the
    /// same way `RestartDue` is, on the **pid** rather than the epoch — the
    /// epoch lives on the private [`SheepSlot`], while the pid is already on
    /// both reports and is just as good a generation token (`None` while not
    /// running, different after every respawn):
    ///
    /// 1. NOT shutting down — a graceful shutdown forbids any new spawn.
    ///    Defence in depth, and deliberately untested rather than tested by a
    ///    case that cannot fail: any sheep passing guards 3 and 4 is `Online`
    ///    with a live pid, which means `ctl.is_some()`, which means
    ///    `begin_shutdown` already set its `manual` marker — so
    ///    `claim_manual` would drop this restart even with this guard gone (an
    ///    automatic restart never takes a marker over, least of all an
    ///    operator's). It stays because that chain is four inferences long and
    ///    none of them is this handler's to keep true.
    /// 2. The slot still existing (a `Delete` may have removed it).
    /// 3. The pid still being the one the report was raised against — a
    ///    breach for the process a crash-and-restart already replaced would
    ///    otherwise restart its healthy successor and reset its budget.
    /// 4. The entry still being `Online`. The extras stay armed for the whole
    ///    kill ladder (there is no `Stopping` transition to disarm at for an
    ///    operator's `stop`), so a report raised *during* a `shep stop` can
    ///    arrive seconds after the sheep is `Stopped` — and
    ///    `ProcessSelector::Id` matches regardless of status, so without this
    ///    the daemon would resurrect a process the user explicitly stopped
    ///    and report success. The same check is what rejects a report against
    ///    a reload's drainee, the one entry [`ProcStatus::Stopping`] actually
    ///    reaches: a liveness failure or memory breach raised against it must
    ///    ride out to the drainee's own exit, never claim its manual marker
    ///    and kill it a second time.
    ///
    /// Delegates to `begin_manual` rather than `respawn`: that keeps the kill
    /// ladder, the marker rule, the `pending_delete` interaction and the budget
    /// reset intact. A breaching sheep is normally `Online` with a live pid, so
    /// respawning it directly would put two live pids on one instance.
    ///
    /// It goes in as [`CommandOrigin::Automatic`], which is what lets an
    /// operator's `stop` or `delete` take the sheep back off a restart already
    /// mid-ladder — see `claim_manual`.
    fn handle_extra_restart(&mut self, id: u32, pid: u32) {
        if self.shutting_down {
            tracing::debug!(id, pid, "extra restart dropped: engine is shutting down");
            return;
        }
        let Some(slot) = self.sheep.get(&id) else {
            tracing::debug!(id, pid, "extra restart dropped: no such sheep");
            return;
        };
        if slot.entry.pid != Some(pid) {
            tracing::debug!(
                id,
                pid,
                current = slot.entry.pid,
                "extra restart dropped: the reported pid is no longer this sheep's"
            );
            return;
        }
        if slot.entry.status != ProcStatus::Online {
            tracing::debug!(
                id,
                pid,
                status = %slot.entry.status,
                "extra restart dropped: the sheep is no longer online"
            );
            return;
        }
        // A throwaway reply: `send_reply` already ignores a closed receiver,
        // and there is nobody to answer — the reporter is fire-and-forget by
        // contract.
        let (reply, _dropped) = oneshot::channel();
        self.begin_manual(
            ProcessSelector::Id(id),
            ManualKind::Restart,
            CommandOrigin::Automatic,
            ReplyKind::Info(reply),
        );
    }

    /// A scheduled restart's backoff elapsed. Guarded on:
    ///
    /// 1. NOT shutting down (CRITICAL-1) — a graceful shutdown in progress
    ///    forbids any new spawn, full stop; nothing here would be part of
    ///    the shutdown aggregation's `online` snapshot, so it would leak.
    /// 2. The entry still being `WaitingRestart` — a manual command may
    ///    have intercepted it (see `apply_immediate`'s `Stop` case),
    ///    making this a stale timer. The same check excludes
    ///    [`ProcStatus::Stopping`]: a reload's drainee must never be
    ///    respawned by a backoff timer that predates it becoming one.
    /// 3. Its epoch still matching the slot's current one (IMPORTANT-3) — a
    ///    respawn that happened after this timer was scheduled (a manual
    ///    restart during the wait, most commonly) makes it stale too, even
    ///    though the status check above wouldn't catch it (the sheep is
    ///    legitimately `WaitingRestart` again, just under a NEWER backoff).
    fn handle_restart_due(&mut self, id: u32, epoch: u64) {
        if self.shutting_down {
            return;
        }
        let Some(slot) = self.sheep.get(&id) else {
            return;
        };
        if slot.entry.status != ProcStatus::WaitingRestart {
            return;
        }
        if slot.epoch != epoch {
            return;
        }
        self.respawn(id, false);
    }

    /// Forwards the shepherd channel's readiness signal to the waiting
    /// readiness task for `id`, if one is waiting. A `Ready` that finds no
    /// live wait — no sender at all, or a stale one whose task is already
    /// gone (see [`SheepSlot::ready_tx`]) — is dropped silently: an app is
    /// free to write `{"kind":"ready"}` whenever it likes, including twice.
    fn handle_ready_signal(&mut self, id: u32) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        if let Some(tx) = slot.ready_tx.take() {
            let _ = tx.send(());
        }
    }

    /// A readiness wait resolved. Guarded exactly like `handle_restart_due`:
    ///
    /// 1. NOT shutting down (CRITICAL-1) — no new bus activity for a sheep
    ///    once a graceful shutdown has started.
    /// 2. The slot still existing (a `Delete` may have removed it).
    /// 3. Its epoch still matching the slot's current one (IMPORTANT-3) — a
    ///    respawn (manual or automatic) that happened while this wait was
    ///    still pending makes it stale.
    /// 4. The entry still being `Starting` — an exit that raced the wait
    ///    (see the epoch check above) or a status this wait has no business
    ///    overwriting.
    ///
    /// That guard set is not boilerplate to trim: a sheep that exited and
    /// respawned while its readiness task was still waiting would otherwise
    /// have the old wait mark the new process online.
    ///
    /// Past the guards the wait belongs to one of two callers, and they want
    /// opposite things from a deadline that elapsed — see
    /// [`Self::reload_ready_result`], which owns the reload half.
    fn handle_ready_result(&mut self, id: u32, epoch: u64, manually: bool, readiness: Readiness) {
        if self.shutting_down {
            return;
        }
        let Some(slot) = self.sheep.get(&id) else {
            return;
        };
        if slot.epoch != epoch {
            return;
        }
        if slot.entry.status != ProcStatus::Starting {
            return;
        }
        if matches!(slot.entry.reload, ReloadState::Draining { .. }) {
            self.reload_ready_result(id, manually, readiness);
            return;
        }
        if readiness == Readiness::TimedOut {
            // Rin, 2026-08-08: a readiness timeout goes online anyway rather
            // than erroring — treating it as a spawn failure would turn a
            // slow-starting app into a restart loop, exactly the failure
            // mode `max_restarts` exists to contain.
            tracing::warn!(id, "readiness deadline elapsed; marking online anyway");
        }
        let info = self.set_status(id, ProcStatus::Online);
        // `manually` comes from the spawn that armed this wait, so gating an
        // app changes only WHEN its `Online` fires, never what the event
        // says about who caused it: the same `shep start` reports the same
        // flag whether or not the app configures readiness.
        self.went_online(id, info, manually);
    }

    /// Spawns the backoff timer for a scheduled restart; `None` still hops
    /// through a task + mailbox send rather than respawning inline, so
    /// "immediate" restarts remain observable as a distinct scheduling step.
    fn schedule_restart(&self, id: u32, epoch: u64, delay: Option<Duration>) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let _ = tx.send(Msg::RestartDue { id, epoch }).await;
        });
    }

    /// Removes `id` from every pending reply's `remaining` set, appending
    /// `info` to each match; fulfills (and drops) any pending reply this
    /// empties. Returns `true` iff a `Shutdown` reply was just fulfilled.
    fn resolve_pending(&mut self, id: u32, info: ProcessInfo) -> bool {
        let mut shutdown_completed = false;
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].remaining.remove(&id) {
                self.pending[i].results.push(info.clone());
            }
            if self.pending[i].remaining.is_empty() {
                let mut pending = self.pending.remove(i);
                pending.results.sort_unstable_by_key(|info| info.id);
                if matches!(pending.reply, ReplyKind::Shutdown(_)) {
                    shutdown_completed = true;
                }
                send_reply(pending.reply, Ok(pending.results));
            } else {
                i += 1;
            }
        }
        shutdown_completed
    }

    /// One sheep's transition to `Online`: emits the event, then arms every
    /// lifecycle extra its configuration asks for.
    ///
    /// The single arming site, reached by all three transitions — the two
    /// ungated spawn paths and, for a gated app, `handle_ready_result`.
    /// Arming has to happen AT the transition rather than at the spawn: a
    /// liveness probe armed against an app that has not finished starting
    /// fails its threshold and restarts the app before it ever comes up.
    fn went_online(&mut self, id: u32, info: ProcessInfo, manually: bool) {
        self.emit(ProcessEventKind::Online, info, manually);
        self.arm_extras(id);
    }

    /// Arms `id`'s lifecycle extras, re-assembling the spec the running
    /// process was spawned from.
    ///
    /// Re-assembly is what makes one arming site possible: `handle_ready_result`
    /// holds an id and nothing else, and `assemble` is the same pure function
    /// both spawn paths already call over the same never-changing `spec`,
    /// `instance` and `credentials`, so it returns that spawn's own spec
    /// rather than a second derivation of it.
    fn arm_extras(&mut self, id: u32) {
        let Some(extras) = self.extras.as_ref() else {
            return;
        };
        let Some(slot) = self.sheep.get(&id) else {
            return;
        };
        let supervisor = SupervisorHandle {
            tx: self.tx.clone(),
        };
        // `Credentials` is `Copy`, which is why this needs no clone.
        let spec = assemble(
            &slot.entry.spec,
            slot.entry.instance,
            &self.paths,
            slot.entry.credentials,
        );
        self.registry
            .arm(&slot.entry, spec_prober(&spec), extras, &supervisor);
    }

    /// Disarms `id`'s lifecycle extras, and its name-group's cron worker and
    /// watch when `id` was the last armed instance of `name`.
    ///
    /// Called from **seven** sites, because a sheep reaches a terminal state
    /// through more than one door. Adding a transition means adding a call
    /// here; the list is the checklist:
    ///
    /// 1. `respawn`'s `Err` arm — a restart that could not spawn lands in
    ///    `Errored` without ever going through `handle_exited`.
    /// 2. `apply_immediate`'s Stop arm — a `WaitingRestart` or `Errored`
    ///    sheep has no live task, so its stop resolves synchronously.
    /// 3. `apply_immediate`'s Delete arm — ditto, deregistered on the spot.
    /// 4. `handle_exited`'s duplicate-`Msg::Exited` Delete branch —
    ///    unreachable today, honoured because the alternative is silent.
    /// 5. `handle_exited`'s `Decision::Errored`.
    /// 6. `handle_exited`'s `Decision::CleanStop` that deregisters.
    /// 7. `handle_exited`'s plain `Decision::CleanStop`.
    ///
    /// One further terminal transition reaches `Errored` and correctly does
    /// NOT disarm: `spawn_fresh`'s `Err` arm. A spawn that never came up was
    /// never armed — its id is fresh from `next_id` and has joined no name
    /// group. It is named here so that an auditor who greps
    /// `ProcStatus::Errored` finds that site already accounted for rather
    /// than re-deriving why it is exempt.
    ///
    /// Nothing else disarms, and nothing needs to: a sheep on its way to
    /// `WaitingRestart` deliberately keeps its arming (its liveness loop is
    /// replaced by the re-arm the respawn performs, and any report it raises
    /// in between names a pid `handle_extra_restart`'s guard no longer
    /// recognises), and the teardown of the actor itself is
    /// [`ExtrasRegistry`]'s own `Drop` rather than a call from here — see that
    /// impl for why the shutdown path cannot be the one that covers it.
    ///
    /// Re-disarming an already-disarmed id is a no-op by construction, so a
    /// site that fires twice costs nothing and a site that is missing costs a
    /// leaked task.
    fn disarm_extras(&mut self, id: u32, name: &str) {
        self.registry.disarm(id, name);
    }

    /// Sets `id`'s status and returns its refreshed snapshot.
    fn set_status(&mut self, id: u32, status: ProcStatus) -> ProcessInfo {
        let slot = self.sheep.get_mut(&id).expect("set_status: unknown id");
        slot.entry.status = status;
        to_info(&slot.entry)
    }

    /// Full flock listing, id-sorted.
    fn snapshot_all(&self) -> Vec<ProcessInfo> {
        let mut infos: Vec<ProcessInfo> = self
            .sheep
            .values()
            .map(|slot| to_info(&slot.entry))
            .collect();
        infos.sort_unstable_by_key(|info| info.id);
        infos
    }

    /// Broadcasts one lifecycle transition. Send failures (no receivers)
    /// are not an error — the bus is fire-and-forget from the actor's side.
    fn emit(&self, event: ProcessEventKind, info: ProcessInfo, manually: bool) {
        let _ = self.events.send(BusEvent::Process {
            event,
            info,
            manually,
            at_ms: crate::now_ms(),
        });
    }
}

/// Delivers a deferred (or immediate) reply, converting to the payload
/// shape each [`ReplyKind`] variant expects.
fn send_reply(reply: ReplyKind, outcome: Result<Vec<ProcessInfo>, SupervisorError>) {
    match reply {
        ReplyKind::Info(tx) => {
            let _ = tx.send(outcome);
        }
        ReplyKind::Ids(tx) => {
            let ids = outcome.map(|infos| infos.into_iter().map(|info| info.id).collect());
            let _ = tx.send(ids);
        }
        ReplyKind::Shutdown(tx) => {
            let _ = tx.send(());
        }
    }
}

/// Snapshots one entry into the wire-facing [`ProcessInfo`] shape.
fn to_info(entry: &ProcessEntry) -> ProcessInfo {
    let uptime_ms = entry.started_at.map_or(0, |started_at| {
        tokio::time::Instant::now()
            .saturating_duration_since(started_at)
            .as_millis() as u64
    });
    ProcessInfo {
        id: entry.id,
        name: entry.spec.config().name.clone(),
        status: entry.status,
        pid: entry.pid,
        restarts: entry.restarts,
        uptime_ms,
        fold: entry.spec.config().fold.clone(),
        // Lossy on purpose: `ProcessInfo` carries paths as strings, and a
        // non-UTF-8 log path must not be allowed to fail serialization of
        // the whole reply and blank the listing for every other sheep.
        out_file: Some(entry.out_file.to_string_lossy().into_owned()),
        err_file: Some(entry.err_file.to_string_lossy().into_owned()),
    }
}

/// The prober a gated readiness task — or a sheep's liveness loop — probes
/// with: a fresh [`OsProber`] scoped to the ASSEMBLED spec's `cwd`/`env`, so
/// an exec-kind probe sees the same environment (its `PORT`, most commonly)
/// its sheep does.
///
/// Taking the [`SpawnSpec`] rather than the [`ResolvedApp`] it was assembled
/// from is the whole point, not a refactor. `probe_exec` runs
/// `env_clear().envs(&self.env)`, and the app's own `config.env` is only one
/// of the three things [`assemble`] folds into the child's environment: an
/// app that sets no `env` at all — the ordinary case — would probe with
/// NOTHING, no `PATH`, no `HOME`, no `TZ`. The instance slot var
/// (`SHEP_INSTANCE`, or the app's `increment_var`) is the sharper half: a
/// `&ResolvedApp` structurally cannot reach `instance`, so every instance of
/// a clustered app would probe whatever the unexpanded variable left behind
/// — the same port, every time.
fn spec_prober(spec: &SpawnSpec) -> Arc<dyn Prober> {
    Arc::new(OsProber::new(spec.cwd.clone(), spec.env.clone()))
}

/// Spawns a readiness task for `id` at `epoch`, returning the oneshot
/// sender the actor stores (`SheepSlot::ready_tx`) so a later `Msg::Ready`
/// can wake it. `source` decides which signal [`await_ready`] waits for;
/// `deadline` is the app's `listen_timeout`. The task reports its result
/// back through `actor_tx` as a `Msg::ReadyResult`, which
/// `Actor::handle_ready_result` drops if `epoch` is no longer current.
///
/// `manually` is carried, never inspected here: the task is a courier for
/// the flag the deferred `Online` needs (see `Msg::ReadyResult`'s own doc).
///
/// Must be called from within a Tokio runtime context: it spawns the
/// waiting task immediately, the same way `schedule_restart` already
/// documents for itself.
fn spawn_readiness_task(
    id: u32,
    epoch: u64,
    manually: bool,
    source: ReadinessSource,
    deadline: Duration,
    prober: Arc<dyn Prober>,
    actor_tx: mpsc::Sender<Msg>,
) -> oneshot::Sender<()> {
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(async move {
        let readiness = await_ready(&source, deadline, ready_rx, prober).await;
        let _ = actor_tx
            .send(Msg::ReadyResult {
                id,
                epoch,
                manually,
                readiness,
            })
            .await;
    });
    ready_tx
}

/// Spawns the task that carries out one `Reopen` and answers its caller.
///
/// Every await a reopen needs lives in here, off the actor loop — see
/// [`Actor::handle_reopen`] for the cycle that rules out doing it inline.
///
/// The sheep are visited one after another rather than concurrently. A
/// reopen is two `open(2)`s behind a flush, so the serial cost is
/// microseconds per sheep on a healthy filesystem, and one task with a plain
/// loop is a great deal easier to follow than a join over the flock. The
/// trade is real on a wedged filesystem, where one stalled pump delays every
/// sheep behind it rather than just itself: the request's own deadline
/// (`crate::rpc`'s `budget`) is what bounds the caller either way.
///
/// Must be called from within a Tokio runtime context: it spawns
/// immediately, like `spawn_readiness_task` and `schedule_restart`.
fn spawn_reopen_task(
    matched: Vec<(ProcessInfo, Option<mpsc::Sender<LogCtl>>)>,
    reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
) {
    tokio::spawn(async move {
        let mut reopened = Vec::with_capacity(matched.len());
        let mut failures = Vec::new();
        for (info, log_ctl) in matched {
            if let Some(log_ctl) = log_ctl
                && let Err(error) = reopen_logs(&log_ctl).await
            {
                // Named and id'd, because the reply that would have said
                // which sheep these are is the one being replaced.
                failures.push(format!("{} (id {}): {error}", info.name, info.id));
            }
            reopened.push(info);
        }
        // Every sheep is visited before anything is reported: one sheep
        // whose log directory is gone must not stop the rest of the flock
        // being reopened, and an operator wants every failing path in one
        // answer rather than one per rotation.
        let _ = reply.send(if failures.is_empty() {
            Ok(reopened)
        } else {
            Err(SupervisorError::ReopenFailed(failures.join("; ")))
        });
    });
}

/// Asks one sheep's log pump to reopen both of its files and waits for the
/// acknowledgement.
///
/// Returns once the pump has answered, or as soon as it is clear no pump
/// will.
///
/// # Errors
///
/// [`ReopenError`] — a pump answered, and at least one of its two paths
/// could not be opened again. That sheep is now writing one or both of its
/// streams nowhere, which is worth failing the caller's request over.
///
/// Not reaching a pump at all is a success, not an error. Both shapes of it
/// mean the same thing — there was nothing left to reopen:
///
/// - the send fails when the pump is already gone (a sheep that is not
///   running), which is exactly what [`ProcIo::log_ctl`] promises callers;
/// - the acknowledgement resolves `Err` when the pump ended between
///   accepting the request and answering it, at both-EOF, with its `logs`
///   receiver dropped, or with its last control sender gone. A request still
///   sitting in the channel is dropped with it.
async fn reopen_logs(log_ctl: &mpsc::Sender<LogCtl>) -> Result<(), ReopenError> {
    let (done, ack) = oneshot::channel();
    if log_ctl.send(LogCtl::Reopen { done }).await.is_err() {
        return Ok(());
    }
    ack.await.unwrap_or(Ok(()))
}

/// Spawns the task that carries out one `Flush` and answers its caller.
///
/// Every await a flush needs lives in here, off the actor loop — see
/// [`Actor::handle_reopen`] for the cycle that rules out doing it inline.
///
/// # Why both phases are in one task, in this order
///
/// EVERY pump in `pumps` is flushed before ANY path in `paths` is truncated.
/// That ordering is the whole reason the flush half exists: `write_all` on a
/// [`tokio::fs::File`] returns once the real `write(2)` is queued on the
/// blocking pool, so a line already dispatched can land at offset 0 of a file
/// that was truncated in between. Draining every pump first turns that into a
/// single barrier — and it is the only ordering that is also correct when
/// several sheep share one path, where a per-sheep flush-then-truncate would
/// let one instance's freshly flushed lines be wiped by the next instance's
/// truncate.
///
/// `pumps` is every writer to a path in `paths`, which is a wider set than
/// `matched` whenever a selector names some but not all of the sheep sharing
/// a file — see [`Actor::handle_flush`] for why the barrier is drawn around
/// the file rather than around the selection, and why the reply is not.
///
/// Like the reopen task, pumps and paths are visited one after another rather
/// than concurrently, and every one of them is visited before anything is
/// reported: one unwritable path must not stop the rest of the flock being
/// emptied, and an operator wants every failure in one answer.
///
/// Must be called from within a Tokio runtime context: it spawns immediately.
fn spawn_flush_task(
    matched: Vec<ProcessInfo>,
    pumps: Vec<mpsc::Sender<LogCtl>>,
    paths: BTreeSet<PathBuf>,
    reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
) {
    tokio::spawn(async move {
        let mut failures = Vec::new();

        for log_ctl in &pumps {
            if let Err(error) = flush_logs(log_ctl).await {
                failures.push(error.message);
            }
        }

        for path in paths {
            if let Err(error) = truncate_log(&path).await {
                failures.push(error.message);
            }
        }

        let _ = reply.send(if failures.is_empty() {
            Ok(matched)
        } else {
            Err(SupervisorError::FlushFailed(failures.join("; ")))
        });
    });
}

/// Asks one sheep's log pump to land everything it still owes both of its
/// files, and waits for the acknowledgement.
///
/// # Errors
///
/// [`FlushError`] — a pump answered, and at least one stream's owed bytes
/// never reached its file. The truncate that follows runs regardless: those
/// bytes errored rather than remaining in flight, so nothing is left to race
/// it. It fails the caller's request because a sheep that cannot write its
/// log is news, not because the file is about to be wrong.
///
/// Not reaching a pump at all is a success, exactly as in [`reopen_logs`],
/// and for a reason that is if anything plainer here: a pump that is gone —
/// whether the send failed or the acknowledgement was dropped by a pump that
/// ended mid-request — has no handle and so owes no bytes to anything. The
/// truncate that follows still runs, which is how a stopped sheep's logs get
/// emptied.
async fn flush_logs(log_ctl: &mpsc::Sender<LogCtl>) -> Result<(), FlushError> {
    let (done, ack) = oneshot::channel();
    if log_ctl.send(LogCtl::Flush { done }).await.is_err() {
        return Ok(());
    }
    ack.await.unwrap_or(Ok(()))
}

/// Truncates the log file at `path` to zero length.
///
/// # What ends up being truncated
///
/// Exactly the paths the Flockfile named, for every registered sheep the
/// selector matched, run or not. `out_file`/`err_file` are free-form config
/// that the assembler takes verbatim, a registered slot carries its paths
/// from the moment it exists (so a sheep that has never been spawned still
/// contributes two), and nothing on this route checks either against the log
/// directory. An app pointing `out_file` at something that is not a log file
/// therefore makes `shep flush` empty that file, with whatever privileges the
/// daemon runs under. Before this verb existed, the worst a mistaken
/// `out_file` bought was log lines appended to it by a running sheep.
///
/// There is no check on WHERE the path points, because there is no rule that
/// separates a hostile `out_file` from a legitimate one outside the log
/// directory, and pointing a sheep's logs at `/var/log/myapp.log` is a
/// supported thing to configure. What is checked is WHAT stands at it: the
/// open goes through [`open_log_path`], so a symlink at the path is refused
/// rather than truncated through. See that function for the whole of what
/// `O_NOFOLLOW` does and does not cover — its guarantee is the same one the
/// log pump's own opens get, which is the point of both coming through it.
///
/// Opened write-only with `O_TRUNC` and dropped straight away — this handle
/// never writes, so it is not the exception to `open_append` being the only
/// thing that opens a log file *for logging*. The pump's own handle is
/// untouched and stays `O_APPEND`, which is what makes its next write land at
/// offset 0 of the emptied file rather than past a sparse hole.
///
/// Deliberately not `create(true)`: a log file that is not there is already
/// empty, so a missing path (or a missing log directory, which surfaces the
/// same `NotFound`) is a no-op success rather than a failure. That is the
/// ordinary state of a sheep that has never been started, and failing
/// `shep flush all` over one such sheep would be the same complaint the
/// reopen path answers for stopped sheep. Creating the file instead would
/// leave a stray empty log wherever a rotator had just renamed one away.
///
/// # Errors
///
/// [`FlushError`] — the path could not be opened for truncation: an ancestry a
/// privileged shepherd will not write below
/// ([`check_log_ancestry`](crate::runner::check_log_ancestry)), a symlink
/// standing at the path itself
/// ([`SYMLINK_REFUSED`](crate::runner::SYMLINK_REFUSED)), a mode the daemon
/// cannot write, a read-only filesystem, an IO error.
async fn truncate_log(path: &Path) -> Result<(), FlushError> {
    let refused = |error: &dyn fmt::Display| FlushError {
        message: format!("{}: {error}", path.display()),
    };
    if let Err(error) = check_log_ancestry(path) {
        return Err(refused(&error));
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).truncate(true);
    match open_log_path(&mut options, path).await {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(refused(&error)),
    }
}

/// Spawns the per-sheep task and returns its control sender.
fn spawn_sheep_task<P: RunningProcess>(
    id: u32,
    proc: P,
    io: ProcIo,
    app: ResolvedApp,
    events: broadcast::Sender<BusEvent>,
    actor_tx: mpsc::Sender<Msg>,
) -> mpsc::Sender<SheepCtl> {
    let (ctl_tx, ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
    tokio::spawn(run_sheep(id, proc, io, app, ctl_rx, events, actor_tx));
    ctl_tx
}

/// The per-sheep task body: owns `(proc, io)` for the process's whole
/// lifetime and drains every `ProcIo` channel. Exactly one of the first two
/// `select!` branches ever fires per proc — that's the one-exit-path
/// invariant — after which the task reports `Msg::Exited` and returns.
///
/// A natural exit racing an in-flight `Kill` (both `proc.wait()` and
/// `ctl_rx.recv()` ready in the same poll) can't produce two `Msg::Exited`s
/// or hang a caller: `tokio::select!` picks exactly one ready branch per
/// iteration, that branch alone runs `kill_process`-or-not and `break`s, and
/// the loop never revisits the other. Whichever branch wins, the caller's
/// deferred reply resolves off that ONE `Msg::Exited` the same way either
/// way (verified under stress — 300 same-tick spawn/stop races, no hang).
///
/// The `logs`/`from_child`/`ctl` branches each carry an `if <channel>_open`
/// guard, flipped to `false` the moment that channel closes: without it, a
/// channel that closes before the proc exits (the real runner's stdout/fd-3
/// pumps can outlive or precede the child, depending on timing) would leave
/// its `recv()` resolving to `None` on every single poll, busy-spinning the
/// `select!` instead of just falling out of consideration.
async fn run_sheep<P: RunningProcess>(
    id: u32,
    mut proc: P,
    io: ProcIo,
    app: ResolvedApp,
    mut ctl_rx: mpsc::Receiver<SheepCtl>,
    events: broadcast::Sender<BusEvent>,
    actor_tx: mpsc::Sender<Msg>,
) {
    // `io` is destructured here, in the task's own body, and that placement
    // is load-bearing rather than stylistic. The log pump ends when its
    // `logs` receiver goes as readily as when the last `log_ctl` sender does
    // (`tokio_runner`'s `spawn_log_pump`), and ending it drops the read ends
    // of the child's stdout and stderr, so the child's next write to either
    // gets `EPIPE`/`SIGPIPE`: letting go of ANY of these fields early kills
    // children rather than merely stopping their logs.
    //
    // `_log_ctl` is bound and never read on purpose, to say that out loud —
    // but the binding is documentation, not the mechanism. `io` is this
    // function's own parameter, so a field the pattern leaves unbound
    // (`log_ctl: _`, or a `..`, even inside a narrower inner block) is not
    // moved anywhere and drops when `run_sheep` returns, exactly as a named
    // binding does. Both were tried against the case below and neither
    // shortens the sender's life.
    //
    // What does NOT survive is `io` being taken apart in a scope shorter
    // than the task: a helper that returns the three receivers, or a
    // `let (a, b, c) = { ... };` block, drops whatever the pattern left
    // behind at its own closing brace and stops every child in the flock.
    let ProcIo {
        mut logs,
        mut from_child,
        to_child,
        log_ctl: _log_ctl,
    } = io;
    let mut ctl_open = true;
    let mut logs_open = true;
    let mut from_child_open = true;

    loop {
        tokio::select! {
            outcome = proc.wait() => {
                let _ = actor_tx.send(Msg::Exited { id, outcome }).await;
                break;
            }
            maybe_ctl = ctl_rx.recv(), if ctl_open => {
                match maybe_ctl {
                    Some(SheepCtl::Kill { grace }) => {
                        let outcome =
                            kill_process(&mut proc, app.config(), Some(&to_child), grace).await;
                        let _ = actor_tx.send(Msg::Exited { id, outcome }).await;
                        break;
                    }
                    None => ctl_open = false,
                }
            }
            maybe_line = logs.recv(), if logs_open => {
                match maybe_line {
                    Some(line) => {
                        let event = if line.err {
                            BusEvent::LogErr { id, line: line.line }
                        } else {
                            BusEvent::LogOut { id, line: line.line }
                        };
                        let _ = events.send(event);
                    }
                    None => logs_open = false,
                }
            }
            maybe_msg = from_child.recv(), if from_child_open => {
                match maybe_msg {
                    Some(ChildMessage::Ready) => {
                        let _ = actor_tx.send(Msg::Ready { id }).await;
                    }
                    Some(ChildMessage::Metric { name, value }) => {
                        tracing::debug!(id, name, value, "child metric (the metrics dog reads these; not built yet)");
                    }
                    Some(ChildMessage::ActionReply { action, body }) => {
                        tracing::debug!(id, action, body, "child action reply (custom actions are not built yet)");
                    }
                    None => from_child_open = false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use shep_core::config::{AppConfig, ProbeConfig, ProbeKind, normalize};
    use shep_core::status::ProcStatus;
    use shep_core::values::UpDuration;

    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    // the one crate-root fixture (IR-33)
    use crate::testing::{SharedRunner, armed_entry, probe_config, test_paths};
    // Test-only: `SilentPumpRunner` counts the requests its pumps receive so
    // a case can order itself against the actor. Imported here rather than
    // beside the module's other `tokio::sync` uses, which do not need it.
    use tokio::sync::watch;
    // Test-only: `RefusesOneSpawn` counts every spawn ATTEMPTED, which is
    // the number `ScriptedRunner`'s own counters cannot report. Aliased
    // because `Ordering` in this module already means `cmp::Ordering` to a
    // reader of the sort keys above.
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Drives virtual time by parking on recv(); returns when the id reaches
    /// `kind`, handing back that event's `manually` flag (most callers only
    /// need the arrival and drop it).
    async fn await_event(
        rx: &mut tokio::sync::broadcast::Receiver<BusEvent>,
        id: u32,
        kind: ProcessEventKind,
    ) -> bool {
        loop {
            match rx.recv().await {
                Ok(BusEvent::Process {
                    event,
                    info,
                    manually,
                    ..
                }) if info.id == id && event == kind => {
                    return manually;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("event stream closed before {kind:?} for id {id}: {e}"),
            }
        }
    }

    /// Waits up to `window` for `kind` targeting `id`; panics if it arrives.
    /// A bounded `timeout` + `recv`, not a bare `try_recv` (Global
    /// Constraints rule 11): right after the clock is driven forward, a
    /// message already due has not necessarily reached this receiver's
    /// queue yet, so a bare `try_recv` would read empty regardless of
    /// whether the code under test is correct.
    async fn assert_no_event_within(
        rx: &mut tokio::sync::broadcast::Receiver<BusEvent>,
        id: u32,
        kind: ProcessEventKind,
        window: Duration,
    ) {
        match tokio::time::timeout(window, await_event(rx, id, kind)).await {
            Err(_elapsed) => {} // window elapsed with nothing arriving — expected
            Ok(_manually) => panic!("unexpected {kind:?} for id {id} within {window:?}"),
        }
    }

    // --- The readiness gate ---

    // fails if a `wait_ready` app reaches Online at spawn instead of waiting
    // on the shepherd channel's ready signal
    #[tokio::test(start_paused = true)]
    async fn wait_ready_app_stays_starting_until_the_channel_signals() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Online,
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        // `Msg::Ready` is `pub(crate)`, and `tests` is a descendant module of
        // `supervisor`, so this reaches the actor exactly where the sheep
        // task's forwarded `ChildMessage::Ready` would — that forwarding
        // itself is pre-existing, unchanged code (`run_sheep`'s
        // `from_child.recv()` arm), not the readiness gate's own surface.
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("Online once the channel signals");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    // fails if a readiness_probe app reaches Online at spawn instead of
    // waiting for the probe to pass, or if it never reaches Online once the
    // probe starts passing. Real time, not the paused clock, and no
    // `start_paused`: this drives a real TCP connect against a real
    // listener, and `probes::os::tests` already found that a paused test
    // waiting on real socket I/O can deadlock — the virtual clock inside the
    // test never appears to move while the OS on the other end is
    // unaffected by it.
    #[tokio::test]
    async fn readiness_probe_app_stays_starting_until_the_probe_passes() {
        // Reserve a free port, then release it immediately: the probe
        // target is fixed at this port before anything is listening on it,
        // so probes fail (connection refused) until the fixture below binds
        // it for real, a few lines down.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);

        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.readiness_probe = Some(ProbeConfig {
            interval: UpDuration::from_millis(50),
            timeout: UpDuration::from_millis(200),
            ..probe_config(ProbeKind::Tcp, &addr.to_string())
        });
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);
        // Nothing is listening on `addr` yet: several probe intervals' worth
        // of real time pass with the probe failing every time.
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Online,
            Duration::from_millis(220),
        )
        .await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let _accept = tokio::spawn(async move { while listener.accept().await.is_ok() {} });

        tokio::time::timeout(
            Duration::from_secs(2),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("Online once the probe starts passing");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    // fails if the readiness prober is built from the app's own `config.env`
    // instead of the ASSEMBLED `SpawnSpec::env`. The probe below reads
    // `$SHEP_INSTANCE`, a variable only `assemble` ever writes, so a prober
    // scoped to `config.env` — empty here, as it is for most apps — expands
    // it to nothing under `probe_exec`'s `env_clear()`, watches for a file
    // that will never exist, and rides out the whole `listen_timeout`
    // instead of noticing the one that does appear.
    //
    // A file, not a port: an exec probe flipping fail->pass on `test -f`
    // needs no listener, no reserved port, and no race, so the only thing
    // this test can fail on is the environment the probe ran with.
    //
    // Real time, not the paused clock, for the reason the TCP test above
    // gives: this spawns a real `sh` per probe, and the virtual clock does
    // not move the OS.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exec_readiness_probe_sees_the_assembled_env_not_the_apps_own() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        // Instance 0 is the only slot a single-instance app gets, so this is
        // the exact path a correctly-scoped probe resolves to.
        let ready_file = dir.path().join("ready-0");
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.readiness_probe = Some(ProbeConfig {
            interval: UpDuration::from_millis(50),
            timeout: UpDuration::from_millis(500),
            ..probe_config(
                ProbeKind::Exec,
                &format!(r#"test -f "{}/ready-$SHEP_INSTANCE""#, dir.path().display()),
            )
        });
        // Far longer than this test's own patience below, so an Online it
        // observes can only have come from a probe that really passed —
        // never from the deadline path quietly marking it online anyway.
        app.listen_timeout = UpDuration::from_millis(60_000);
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);
        // Several probe intervals of real time with the file absent.
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Online,
            Duration::from_millis(220),
        )
        .await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        std::fs::write(&ready_file, b"").unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("Online once the exec probe can resolve $SHEP_INSTANCE");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    // fails if gating an app changes its `Online` event's `manually` flag —
    // the gate moves only WHEN that event fires, never what it says about
    // who caused it. Both halves matter: a `Start` is the caller's doing
    // either way, and a manual `Restart` must still say so after riding
    // through the readiness wait.
    #[tokio::test(start_paused = true)]
    async fn a_gated_apps_online_carries_the_same_manually_flag_an_ungated_one_does() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![
            ProcScript::never_exits(), // id 0: gated
            ProcScript::never_exits(), // id 1: ungated
            ProcScript::never_exits(), // id 0 again, after the manual restart
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut gated = AppConfig::minimal("gated", "./srv");
        gated.wait_ready = true;
        let ungated = AppConfig::minimal("ungated", "./srv");
        handle
            .start(vec![normalize(gated).unwrap(), normalize(ungated).unwrap()])
            .await
            .unwrap();

        // The ungated app is the control: whatever it reports for a plain
        // `Start` is what the gated one has to report too.
        let ungated_manually = tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 1, ProcessEventKind::Online),
        )
        .await
        .expect("an ungated app is Online at spawn");
        assert!(ungated_manually, "sanity: a Start is a manual event");

        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        let gated_manually = tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("Online once the channel signals");
        assert_eq!(
            gated_manually, ungated_manually,
            "the same `shep start` must report the same flag, gated or not"
        );

        // Now the respawn path: a manual Restart's own flag has to survive
        // the readiness wait rather than being defaulted at the far end. The
        // sheep is `Starting` but its task is live, so this takes the deferred
        // route — kill ladder, then `handle_exited`'s forced-restart branch —
        // and `restart` resolves at that respawn rather than at Online, so a
        // gated app's reply lands here with the new process still `Starting`:
        // no deadlock, and nothing to signal until after this await.
        let restarted = handle.restart(ProcessSelector::Id(0)).await.unwrap();
        assert_eq!(restarted[0].status, ProcStatus::Starting);
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        let restarted_manually = tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("Online once the respawned sheep signals");
        assert!(
            restarted_manually,
            "a manual Restart's Online must stay manual through the readiness gate"
        );
    }

    // The positive control for every `manually: false` the lifecycle extras
    // assert: an operator's `shep restart` really does reach the bus as a user
    // action, so those cases are reading a flag that still moves rather than
    // one wired shut. It claims the `Restart` event specifically —
    // `a_gated_apps_online_carries_the_same_manually_flag_an_ungated_one_does`
    // covers the deferred `Online` — because that is the event the extras'
    // cases read.
    //
    // fails if `SupervisorHandle::restart` stops declaring
    // `CommandOrigin::Operator`, or if `handle_exited`'s forced-restart branch
    // stops reading the origin at all and reports every restart as automatic.
    #[tokio::test(start_paused = true)]
    async fn an_operators_restart_is_reported_as_a_user_action() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        // Two: the sheep, and the respawn the restart performs. One would
        // leave that respawn `SpawnFailed("script exhausted")`, which emits
        // `Errored` instead of `Restart` — and `await_event` would then wait
        // for a `Restart` that never comes rather than judging its flag.
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits(); 2]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        handle
            .start(vec![normalize(AppConfig::minimal("web", "./srv")).unwrap()])
            .await
            .unwrap();

        let restarted = handle.restart(ProcessSelector::All).await.unwrap();
        assert_eq!(restarted[0].restarts, 1, "the restart really respawned");

        let manually = tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Restart),
        )
        .await
        .expect("the respawn `restart` performed");
        assert!(
            manually,
            "a person typed `shep restart`; the bus must say a user action caused it"
        );
    }

    // fails if a readiness timeout is treated as a spawn failure instead of
    // going online anyway — that would turn every slow-starting app into a
    // restart loop, exactly what max_restarts exists to contain
    #[tokio::test(start_paused = true)]
    async fn a_gated_app_whose_deadline_elapses_goes_online_anyway() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals ready
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        tokio::time::timeout(
            Duration::from_secs(4), // > the 3000ms default listen_timeout
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("Online once the readiness deadline elapses");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    // fails if a stale readiness result — from a sheep that exited and was
    // automatically respawned while starting — marks the respawned process
    // online; this is the epoch guard's test, and it is the one that
    // catches the stale-wait defect. Status alone cannot catch it: the OLD
    // process and the NEW one are both `Starting`, so only the epoch tells
    // them apart. This also doubles as the "an event arrives before the
    // readiness signal does" proof (`Restart` before `Online`) for the
    // respawn path.
    #[tokio::test(start_paused = true)]
    async fn a_gated_app_that_exits_while_starting_never_reaches_online_from_the_old_wait() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![
            ProcScript::stable_then_exit(500, 1), // unstable exit while Starting
            ProcScript::never_exits(),            // the automatic respawn
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals either instance's readiness
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        // The exit at 500ms is unstable (< the 1000ms min_uptime default)
        // and triggers an immediate automatic respawn (no
        // exp_backoff_restart_delay configured, so `restart_delay` is
        // `None`): status goes straight back to `Starting` for the NEW
        // process, epoch bumped. `Restart` fires here; `Online` does not —
        // proving the two emits stay separate on the respawn path too.
        tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Restart),
        )
        .await
        .expect("the automatic respawn after the unstable exit");
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);
        assert_eq!(handle.list().await[0].restarts, 1);

        // The ORIGINAL readiness wait's deadline (~3000ms from the FIRST
        // spawn, ~2500ms from here) elapses next; status reads `Starting`
        // for both the old and the new process, so only the epoch guard —
        // not the status guard — can tell them apart.
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Online,
            Duration::from_millis(2_700),
        )
        .await;
        assert_eq!(
            handle.list().await[0].status,
            ProcStatus::Starting,
            "the OLD wait's stale TimedOut must not have marked the respawned process online"
        );

        // The RESPAWNED process's own deadline elapses next (~3000ms from
        // ITS spawn), and this one legitimately goes online.
        tokio::time::timeout(
            Duration::from_secs(2),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("the new process's own readiness deadline");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    // fails if a stale readiness result marks a STOPPED sheep online. Unlike
    // the two tests above, no respawn ever happens here, so the epoch never
    // changes — only the status guard stands between the stale TimedOut and
    // an incorrect Online, and this is the test that catches its removal
    // specifically (the epoch check alone would let this one through).
    #[tokio::test(start_paused = true)]
    async fn a_gated_app_stopped_while_starting_ignores_the_old_wait() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::stable_then_exit(500, 1)]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals ready
        app.autorestart = false; // straight to Stopped: epoch never bumps
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Stop),
        )
        .await
        .expect("the natural exit at 500ms");
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);

        // The original readiness task's own 3000ms deadline (from spawn) is
        // still running in the background and resolves TimedOut around
        // t=3000ms — roughly 2500ms from here, at the SAME epoch this slot
        // still carries. That stale ReadyResult must never flip this
        // Stopped sheep to Online.
        assert_no_event_within(&mut rx, 0, ProcessEventKind::Online, Duration::from_secs(3)).await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
    }

    // fails if the OLD readiness wait's eventual result marks the
    // RESPAWNED process online instead of being dropped by the epoch guard
    #[tokio::test(start_paused = true)]
    async fn a_gated_app_restarted_while_starting_ignores_the_old_wait() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals either instance's readiness
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        // A gap before the manual restart, so the OLD wait's deadline
        // (~3000ms from spawn) and the NEW wait's deadline (~3000ms from
        // THIS point) land far enough apart to tell them apart below.
        tokio::time::sleep(Duration::from_millis(500)).await;
        handle
            .restart(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();
        assert_eq!(
            handle.list().await[0].status,
            ProcStatus::Starting,
            "the respawned process is gated too, so it must still be Starting"
        );

        // The ORIGINAL readiness wait's deadline elapses first; the epoch
        // guard must drop it rather than mark the respawned process online.
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Online,
            Duration::from_millis(2_700),
        )
        .await;
        assert_eq!(
            handle.list().await[0].status,
            ProcStatus::Starting,
            "the old wait's stale TimedOut must not have marked the new process online"
        );

        // The RESPAWNED process's own deadline elapses next, and this one
        // legitimately goes online.
        tokio::time::timeout(
            Duration::from_secs(2),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("the new process's own readiness deadline");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    #[tokio::test(start_paused = true)]
    async fn start_lists_online_instances() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        let infos = handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        assert_eq!(infos.len(), 2);
        let list = handle.list().await;
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|i| i.status == ProcStatus::Online));
        assert_eq!(list.iter().map(|i| i.id).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn listed_log_paths_are_the_derived_defaults() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let logs = paths.logs.clone();
        let handle = spawn_supervisor(runner, paths, events);
        handle
            .start(vec![normalize(AppConfig::minimal("web", "./srv")).unwrap()])
            .await
            .unwrap();

        let list = handle.list().await;
        assert_eq!(
            list[0].out_file.as_deref(),
            logs.join("web-0-out.log").to_str()
        );
        assert_eq!(
            list[0].err_file.as_deref(),
            logs.join("web-0-err.log").to_str()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn listed_log_paths_honour_an_explicit_out_file() {
        // The entire reason `ProcessInfo` carries these: an explicit
        // `out_file` may point anywhere on the filesystem, so a reader that
        // guessed `logs/<name>-<instance>-out.log` from the convention would
        // silently show nothing for this sheep. `err_file` is left unset in
        // the same app on purpose — the two resolve independently, and
        // pinning both here proves reporting one explicitly does not drag
        // the other off its default.
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let logs = paths.logs.clone();
        let handle = spawn_supervisor(runner, paths, events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.out_file = Some("/var/log/myapp.log".to_string());
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        let list = handle.list().await;
        assert_eq!(list[0].out_file.as_deref(), Some("/var/log/myapp.log"));
        assert_eq!(
            list[0].err_file.as_deref(),
            logs.join("web-0-err.log").to_str(),
            "err_file was not configured, so it must still be the default"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn crash_loop_erroreds_after_budget_with_pinned_delays() {
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        // 16 spawns: initial + 15 restarts; every exit instant (unstable).
        let runner = ScriptedRunner::new((0..16).map(|_| ProcScript::const_exit(1)).collect());
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("crash", "./boom");
        app.exp_backoff_restart_delay = Some("100".parse().unwrap());
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Park on the event stream; auto-advance drives through all pending
        // backoff delays (pinned Task 4 sequence). The budget check itself
        // (spec §4: reaching max_restarts=16 unstable exits errors) fires on
        // the 16th exit, using the script's 16th and final entry — the
        // script is exactly, not incidentally, exhausted at that point.
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        assert_eq!(list[0].restarts, 15); // respawns performed, not exits
    }

    #[tokio::test(start_paused = true)]
    async fn crash_loop_budget_check_fires_before_script_exhaustion_at_real_default() {
        // Coverage gap flagged by the whole-branch review: the pinned-delay
        // crash_loop test above supplies EXACTLY 16 scripted spawns, so a
        // still-buggy `unstable_count > max_restarts` check (which needs a
        // 17th unstable exit to fire) would coincidentally also land on
        // Errored via script exhaustion (spawn failure), masking the bug.
        // Here the script has 20 entries — comfortably more than either
        // check needs — so only the BUDGET path can produce restarts==15;
        // a still-buggy `>` check would consume a 17th spawn and report
        // restarts==16 instead. Real shipped default max_restarts (16, no
        // override) throughout.
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new((0..20).map(|_| ProcScript::const_exit(1)).collect());
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("crash-default", "./boom"); // no max_restarts override
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        // Budget-exhaustion path, not script exhaustion: errors at the 16th
        // unstable exit (15 respawns performed), with 4 scripted spawns left
        // unused.
        assert_eq!(list[0].restarts, 15);
    }

    #[tokio::test(start_paused = true)]
    async fn stable_run_resets_budget() {
        let (events, mut rx) = tokio::sync::broadcast::channel(256);
        let mut script = vec![
            ProcScript::const_exit(1),
            ProcScript::const_exit(1),
            ProcScript::const_exit(1),
        ];
        script.push(ProcScript::stable_then_exit(2000, 1)); // > min_uptime 1000ms => stable
        script.extend((0..16).map(|_| ProcScript::const_exit(1)));
        let runner = ScriptedRunner::new(script);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("flappy", "./f");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // 3 unstable (no backoff => immediate), 1 stable run resets the budget,
        // then 16 more unstable before errored.
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        // 3 + 1 + 15 respawns after the initial spawn = 19
        assert_eq!(list[0].restarts, 19);
    }

    #[tokio::test(start_paused = true)]
    async fn manual_stop_prevents_restart() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript {
            delay_ms: u64::MAX,
            outcome: ExitOutcome {
                code: None,
                signal: None,
            },
            obeys_signal: true,
            lamb_holds_the_pipe: false,
        }]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("svc", "./svc");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        let stopped = handle
            .stop(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();
        assert_eq!(stopped[0].status, ProcStatus::Stopped); // deferred reply: already terminal
        // No restart is ever scheduled: advancing a full minute yields no further
        // events and the status stays Stopped.
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_exit_codes_mean_clean_stop() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::const_exit(0)]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("oneshot", "./job");
        app.stop_exit_codes = vec![0];
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        await_event(&mut rx, 0, ProcessEventKind::Stop).await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_failure_surfaces_and_erroreds() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![]); // exhausted immediately
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("ghost", "./missing");
        let err = handle
            .start(vec![normalize(app).unwrap()])
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::SpawnFailed(_)));
        assert_eq!(handle.list().await[0].status, ProcStatus::Errored);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unresolvable_user_fails_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let handle = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            test_paths(&dir),
            events,
        );
        let mut app = AppConfig::minimal("svc", "./svc");
        app.user = Some("definitely-not-a-real-shep-user".to_string());
        let err = handle
            .start(vec![normalize(app).unwrap()])
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::SpawnFailed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn delete_and_selectors_route() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut a = AppConfig::minimal("api", "./a");
        a.fold = Some("backend".to_string());
        let b = AppConfig::minimal("web", "./w");
        handle
            .start(vec![normalize(a).unwrap(), normalize(b).unwrap()])
            .await
            .unwrap();
        let hits = handle
            .stop(ProcessSelector::Fold("backend".to_string()))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "api");
        let deleted = handle
            .delete(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();
        assert_eq!(deleted, vec![1]);
        assert_eq!(handle.list().await.len(), 1);
    }

    // An operator's `shep restart` aimed at a RUNNING sheep resets the restart
    // budget (spec §4) and respawns, and the operator gets the respawned sheep
    // back as its reply. The not-running half of that claim is a third test,
    // two below.
    //
    // fails if `handle_exited`'s `slot.entry.budget.reset()` is dropped —
    // that leaves the two spent unstable exits on the books, so the crash
    // after the restart is the third of three and errors the sheep out at
    // three restarts instead of carrying it to a fourth.
    #[tokio::test(start_paused = true)]
    async fn manual_restart_resets_budget_and_respawns() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Five procs, sized against the mutation rather than against a correct
        // run: two unstable crashes, the long-lived proc they land on, the
        // respawn the manual restart performs (unstable again, to spend the
        // budget the reset just cleared), and the proc a still-solvent budget
        // restarts onto. A pool of four would answer that last spawn
        // `SpawnFailed("script exhausted")` and land the sheep in `Errored` —
        // the very state a lost budget reset produces — so the assertion
        // below would hold identically whether or not the reset happened.
        let runner = ScriptedRunner::new(vec![
            ProcScript::const_exit(1),
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("svc", "./svc");
        // Three rather than the default sixteen, so the two crashes below
        // leave the budget one short of exhausted and a single further crash
        // decides the test.
        app.max_restarts = 3;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state, not on the repeated Online event: immediate restarts
        // mean restarts==2 once the never_exits proc is up.
        loop {
            let info = handle.list().await.remove(0);
            if info.restarts == 2 && info.status == ProcStatus::Online {
                break;
            }
            tokio::task::yield_now().await;
        }
        let restarted = handle
            .restart(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();
        // The deferred reply is the operator's own answer, snapshotted at the
        // respawn — so it reads Online regardless of what that proc does next.
        assert_eq!(restarted[0].status, ProcStatus::Online);

        // The proc that restart landed on is itself unstable. With the budget
        // reset its exit is the FIRST of three again and the sheep restarts
        // once more; without it, the third, and the sheep errors out.
        // Bounded, unlike the sync loop above: the failing outcome here is a
        // settled `Errored`, so an unbounded wait for the passing one would
        // hang the test rather than fail it (rule 11). Every step in between
        // is ready work — the restarts at this config are immediate — so the
        // round trips below cannot be starved by the paused clock.
        let mut settled = handle.list().await.remove(0);
        for _ in 0..200 {
            if settled.status == ProcStatus::Errored
                || (settled.status == ProcStatus::Online && settled.restarts == 4)
            {
                break;
            }
            tokio::task::yield_now().await;
            settled = handle.list().await.remove(0);
        }
        assert_eq!(
            (settled.status, settled.restarts),
            (ProcStatus::Online, 4),
            "an operator's restart left the two spent unstable exits on the \
             books -- got {settled:?}"
        );
    }

    // The budget reset belongs to `ManualKind::Restart` and to nothing else:
    // a restart the daemon raised itself — a cron occurrence, a watched file
    // changing, a memory breach — resets it exactly as an operator's `shep
    // restart` does. `CommandOrigin` governs only which of two racing
    // commands owns a sheep's next exit (`claim_manual`), and classifying
    // cron and watch as automatic must not make the budget depend on it.
    //
    // The sibling above makes the same claim through `restart`; the two
    // differ in that one call, and in the operator's extra check on the
    // reply only its path has.
    //
    // fails if a budget reset is ever gated on origin — dropping
    // `handle_exited`'s `slot.entry.budget.reset()` for an automatic restart
    // leaves the two spent unstable exits on the books, so the very next
    // crash is the third of three and errors the sheep out instead of
    // restarting it.
    #[tokio::test(start_paused = true)]
    async fn an_automatic_restart_resets_the_budget_like_an_operators_does() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Five procs, which is the most this test can demand: two unstable
        // crashes, the long-lived proc they land on, the respawn the
        // automatic restart performs (unstable again, to spend the budget the
        // reset just cleared), and the proc a still-solvent budget restarts
        // onto. A pool of four would answer that last spawn
        // `SpawnFailed("script exhausted")` and land the sheep in `Errored` —
        // the very state a lost budget reset produces — so the assertion
        // below would fail identically whether the reset worked or not.
        let runner = ScriptedRunner::new(vec![
            ProcScript::const_exit(1),
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("svc", "./svc");
        // Three rather than the default sixteen, so the two crashes below
        // leave the budget one short of exhausted and a single further crash
        // decides the test.
        app.max_restarts = 3;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state, not on the repeated Online event: immediate restarts
        // mean restarts==2 once the never_exits proc is up.
        loop {
            let info = handle.list().await.remove(0);
            if info.restarts == 2 && info.status == ProcStatus::Online {
                break;
            }
            tokio::task::yield_now().await;
        }

        handle
            .restart_automatic(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();

        // The proc that restart landed on is itself unstable. With the budget
        // reset its exit is the FIRST of three again and the sheep restarts
        // once more; without it, the third, and the sheep errors out.
        // Bounded, unlike the sync loop above: the failing outcome here is a
        // settled `Errored`, so an unbounded wait for the passing one would
        // hang the test rather than fail it (rule 11). Every step in between
        // is ready work — the restarts at this config are immediate — so the
        // round trips below cannot be starved by the paused clock.
        let mut settled = handle.list().await.remove(0);
        for _ in 0..200 {
            if settled.status == ProcStatus::Errored
                || (settled.status == ProcStatus::Online && settled.restarts == 4)
            {
                break;
            }
            tokio::task::yield_now().await;
            settled = handle.list().await.remove(0);
        }
        assert_eq!(
            (settled.status, settled.restarts),
            (ProcStatus::Online, 4),
            "an automatic restart left the two spent unstable exits on the \
             books -- got {settled:?}"
        );
    }

    // The same reset, on the other of the two paths that perform it. A
    // `restart` aimed at a sheep with no live task has no exit to ride, so it
    // never reaches `handle_exited` — `apply_immediate` resets and respawns
    // inline instead, and the operator's reply is that respawn rather than a
    // deferred snapshot.
    //
    // `Stopped` is the not-running state used here because it is the settled
    // one: `WaitingRestart` still holds a RestartDue timer scheduled against
    // it, and `Errored` is only reachable with the budget already at the cap,
    // where this proves the reset clears a PARTIAL carry.
    //
    // fails if `apply_immediate`'s `ManualKind::Restart` arm loses its
    // `budget.reset()` -- that leaves the two unstable exits spent before the
    // stop on the books, so the crash after the restart is the third of three
    // and errors the sheep out at three restarts instead of carrying it to a
    // fourth.
    #[tokio::test(start_paused = true)]
    async fn restarting_a_stopped_sheep_resets_the_budget() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Five procs, sized against the mutation rather than against a correct
        // run: two unstable crashes, the long-lived proc they land on and the
        // stop below ends, the respawn the restart performs (unstable again,
        // to spend the budget the reset just cleared), and the proc a
        // still-solvent budget restarts onto. That fifth spawn is the one a
        // correct implementation performs and a mutated one never reaches —
        // with the reset dropped the sheep is already `Errored` by then. A
        // pool of four would answer it `SpawnFailed("script exhausted")`,
        // which `respawn` also lands in `Errored` at three restarts, so the
        // assertion below would fail identically whether the reset worked or
        // not.
        let runner = ScriptedRunner::new(vec![
            ProcScript::const_exit(1),
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("svc", "./svc");
        // Three rather than the default sixteen, so the two crashes below
        // leave the budget one short of exhausted and a single further crash
        // decides the test.
        app.max_restarts = 3;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state, not on the repeated Online event: immediate restarts
        // mean restarts==2 once the never_exits proc is up.
        loop {
            let info = handle.list().await.remove(0);
            if info.restarts == 2 && info.status == ProcStatus::Online {
                break;
            }
            tokio::task::yield_now().await;
        }

        // `stop` is what takes the sheep off its live task without touching
        // the budget: `decide_on_exit` short-circuits to CleanStop on
        // `manual_stop`, before it would ever classify the exit. The deferred
        // reply resolves only once that exit has landed, so the sheep is
        // settled in `Stopped` — no task, no timer — by the time the restart
        // below is sent, and the two spent unstable exits are still on the
        // books.
        let stopped = handle
            .stop(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();
        assert_eq!(stopped[0].status, ProcStatus::Stopped);

        let restarted = handle
            .restart(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();
        // `apply_immediate`'s reply is the respawn itself, sent in the same
        // actor turn that performed it — so it reads Online at the bumped
        // restart count regardless of what that proc does next.
        assert_eq!(
            (restarted[0].status, restarted[0].restarts),
            (ProcStatus::Online, 3)
        );

        // The proc that restart landed on is itself unstable. With the budget
        // reset its exit is the FIRST of three again and the sheep restarts
        // once more; without it, the third, and the sheep errors out.
        // Bounded, unlike the sync loop above: the failing outcome here is a
        // settled `Errored`, so an unbounded wait for the passing one would
        // hang the test rather than fail it (rule 11). Every step in between
        // is ready work — the restarts at this config are immediate — so the
        // round trips below cannot be starved by the paused clock.
        let mut settled = handle.list().await.remove(0);
        for _ in 0..200 {
            if settled.status == ProcStatus::Errored
                || (settled.status == ProcStatus::Online && settled.restarts == 4)
            {
                break;
            }
            tokio::task::yield_now().await;
            settled = handle.list().await.remove(0);
        }
        assert_eq!(
            (settled.status, settled.restarts),
            (ProcStatus::Online, 4),
            "restarting a stopped sheep left the two spent unstable exits on \
             the books -- got {settled:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_kills_all_and_stops_the_engine() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        handle.shutdown().await; // kill ladder on every online sheep, then stop
        // After shutdown the handle's channel is closed; further commands error.
        assert!(handle.list_checked().await.is_err());
    }

    // ---------------------------------------------------------------
    // Concurrency regression guards (opus review, fix round 2026-08-07).
    // Promoted from the reviewer's probe suite (probes A, B, H, I, K, L) —
    // the 9 tests above are structurally incapable of reaching any of these:
    // every one needs either a second in-flight command racing the first,
    // or a timer/channel left pending across a state transition, neither of
    // which the locked 9-test suite's single-command-at-a-time shape
    // produces.
    // ---------------------------------------------------------------

    async fn drain_kinds(
        rx: &mut tokio::sync::broadcast::Receiver<BusEvent>,
    ) -> Vec<(u32, ProcessEventKind)> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let BusEvent::Process { event, info, .. } = ev {
                out.push((info.id, event));
            }
        }
        out
    }

    // CRITICAL-1 (probe A): a pending RestartDue timer for a waiting-restart
    // sheep must not respawn a child once shutdown has begun — that child
    // would never be part of the shutdown's `online` snapshot and so would
    // never be killed.
    #[tokio::test(start_paused = true)]
    async fn shutdown_ignores_a_pending_restart_timer() {
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::const_exit(1),     // crash: instant exit -> waiting-restart
            ProcScript::ignores_signals(), // web: full 1600ms kill ladder
            ProcScript::never_exits(),     // the ghost respawn, if CRITICAL-1 regresses
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut crash = AppConfig::minimal("crash", "./boom");
        crash.exp_backoff_restart_delay = Some("500".parse().unwrap());
        let web = AppConfig::minimal("web", "./srv");
        handle
            .start(vec![normalize(crash).unwrap(), normalize(web).unwrap()])
            .await
            .unwrap();
        // id 0 is now waiting-restart with a 500ms timer pending.
        await_event(&mut rx, 0, ProcessEventKind::Exit).await;

        handle.shutdown().await; // web's ladder burns 1600ms of virtual time

        let seen = drain_kinds(&mut rx).await;
        let ghost = seen
            .iter()
            .any(|(id, k)| *id == 0 && *k == ProcessEventKind::Restart);
        assert!(
            !ghost,
            "GHOST RESPAWN during shutdown: events after shutdown = {seen:?}"
        );
    }

    // CRITICAL-1: a readiness wait that resolves after a shutdown has begun
    // must not mark its sheep online. The sibling guards on this handler are
    // both pinned already — the epoch guard by
    // `a_gated_app_restarted_while_starting_ignores_the_old_wait`, the
    // `Starting` guard by `a_gated_app_stopped_while_starting_ignores_the_old_wait`
    // — and neither reaches this one: the shutdown leaves the slot at the same
    // epoch and in the same `Starting` status the wait was armed under, so
    // this guard is the only thing standing between a mid-shutdown `TimedOut`
    // and an `Online` for a sheep the daemon is in the middle of killing. That
    // `Online` also arms every lifecycle extra the app configures, at the one
    // moment nothing is left to disarm them.
    //
    // The timings: `listen_timeout` is 1000ms and the app ignores signals, so
    // its own kill ladder runs the full 1600ms `kill_timeout` default — the
    // readiness deadline elapses 600ms inside it, while the sheep is still
    // `Starting` with a live pid. `shutdown()` is the call that carries the
    // paused clock across both, and the actor is gone by the time it returns,
    // so the drained stream below can no longer grow.
    //
    // ONE script, and one is right under both implementations: the mutated
    // handler emits and sets status, it never spawns, so there is no ghost
    // spawn to leave a script for. What an exhausted script COULD hide is the
    // setup — a failed spawn leaves the sheep `Errored` with no readiness wait
    // armed and so no `Online` to forbid — which is what the `Starting`
    // assertion rules out before the shutdown, and the `Stop` assertion (the
    // ladder really ran, past the deadline) after it.
    //
    // fails if `handle_ready_result` stops guarding on `shutting_down`.
    #[tokio::test(start_paused = true)]
    async fn shutdown_ignores_a_pending_readiness_wait() {
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![ProcScript::ignores_signals()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("gated", "./g");
        app.wait_ready = true; // nobody ever signals ready
        app.listen_timeout = UpDuration::from_millis(1_000);
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        assert_eq!(
            handle.list().await[0].status,
            ProcStatus::Starting,
            "the readiness wait has to be armed for its result to be droppable"
        );

        handle.shutdown().await; // the ladder burns 1600ms of virtual time

        let seen = drain_kinds(&mut rx).await;
        assert!(
            seen.contains(&(0, ProcessEventKind::Stop)),
            "the kill ladder must have outlasted the readiness deadline: events = {seen:?}"
        );
        assert!(
            !seen.contains(&(0, ProcessEventKind::Online)),
            "a readiness wait resolved during shutdown marked a dying sheep online: \
             events = {seen:?}"
        );
    }

    // CRITICAL-1 (probe H): a Start racing a concurrent Shutdown must never
    // leave an un-killed child — either the actor processes Shutdown first
    // (Start is then rejected outright) or Start lands first (Shutdown's
    // `online` snapshot, computed afterward, catches it).
    #[tokio::test(start_paused = true)]
    async fn late_start_racing_shutdown_never_orphans() {
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(), // web: 1600ms ladder
            ProcScript::never_exits(),     // the late Start, if it lands before Shutdown
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let web = AppConfig::minimal("web", "./srv");
        handle.start(vec![normalize(web).unwrap()]).await.unwrap();

        let h2 = handle.clone();
        let late = tokio::spawn(async move {
            let app = AppConfig::minimal("late", "./l");
            h2.start(vec![normalize(app).unwrap()]).await
        });
        handle.shutdown().await;
        let outcome = late.await.unwrap();

        let seen = drain_kinds(&mut rx).await;
        match outcome {
            Err(SupervisorError::EngineStopped) => {} // rejected: no orphan possible
            Ok(infos) => {
                let late_id = infos[0].id;
                assert!(
                    seen.iter().any(|(id, k)| *id == late_id
                        && matches!(k, ProcessEventKind::Stop | ProcessEventKind::Exit)),
                    "late Start raced ahead of shutdown but was never killed: events = {seen:?}"
                );
            }
            Err(other) => panic!("unexpected error from a late Start during shutdown: {other:?}"),
        }
    }

    // IMPORTANT-3 (probe B): a manual restart during a backoff wait leaves
    // the ORIGINAL RestartDue timer scheduled; it must not fire later and
    // short-circuit the NEW backoff the manual respawn's own eventual exit
    // schedules.
    #[tokio::test(start_paused = true)]
    async fn stale_restart_timer_never_short_circuits_a_newer_backoff() {
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::const_exit(1),             // t=0 exit -> T1 @ 2000
            ProcScript::stable_then_exit(1500, 1), // manual respawn, dies @1500 -> T2 @ 3500
            ProcScript::never_exits(),             // whoever respawns first takes this
            ProcScript::never_exits(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("crash", "./boom");
        app.exp_backoff_restart_delay = Some("2000".parse().unwrap());
        app.min_uptime = "5000".parse().unwrap(); // 1500ms uptime counts as unstable
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        await_event(&mut rx, 0, ProcessEventKind::Exit).await; // waiting-restart, T1 @ 2000

        let out = handle.restart(ProcessSelector::All).await.unwrap();
        assert_eq!(
            out[0].status,
            ProcStatus::Online,
            "manual restart respawned"
        );

        // t=1500 the respawn dies -> waiting-restart with a NEW timer @ 3500.
        // The correct next respawn is at 3500. Look at the world at t=2500.
        tokio::time::advance(Duration::from_millis(2500)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let info = handle.list().await.remove(0);
        assert_eq!(
            (info.status, info.restarts),
            (ProcStatus::WaitingRestart, 1),
            "at t=2500 the sheep should still be waiting on its 3500ms backoff; \
             got {info:?} -- the stale 2000ms timer fired early"
        );
    }

    // IMPORTANT-4 (probe I): Stop and Restart racing on the same running
    // sheep. Chosen semantics (see `claim_manual`'s doc comment): the first
    // command to reach a running sheep owns its `manual` marker and its one
    // live Kill; both callers get back the SAME honest terminal snapshot
    // once it lands, instead of the old last-writer-wins bug handing the
    // `stop()` caller an `Online` `ProcessInfo`.
    //
    // This is also the fence around the carve-out that
    // `an_operators_stop_beats_an_automatic_restart_mid_ladder` covers: BOTH
    // commands here have a caller awaiting an answer, so neither may displace
    // the other.
    //
    // fails if `claim_manual`'s carve-out widens to operator-versus-operator:
    // the later `restart` would take the marker off the earlier `stop`, respawn
    // the sheep, and hand the `stop()` caller an `Online` snapshot again.
    #[tokio::test(start_paused = true)]
    async fn overlapping_stop_and_restart_agree_on_one_outcome() {
        let (events, _rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(), // 1600ms ladder: a wide race window
            ProcScript::never_exits(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("svc", "./svc");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        let h2 = handle.clone();
        let stopper = tokio::spawn(async move { h2.stop(ProcessSelector::All).await });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let restarted = handle.restart(ProcessSelector::All).await.unwrap();
        let stopped = stopper.await.unwrap().unwrap();

        assert_eq!(
            stopped[0].status,
            ProcStatus::Stopped,
            "stop() reported a non-stopped sheep"
        );
        assert_eq!(
            restarted[0].status,
            ProcStatus::Stopped,
            "restart() lost the race to the earlier stop() but got a different \
             answer than the stop() caller -- the two callers disagree about \
             what happened to the same sheep"
        );
    }

    // CRITICAL-2 (probe K): the actor must never block on a busy sheep's
    // kill ladder. A flood of Stop commands against one sheep mid-kill must
    // not delay processing an unrelated sheep's own, unrelated exit.
    #[tokio::test(start_paused = true)]
    async fn actor_never_blocks_behind_a_busy_kill_ladder() {
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(),        // sheep a: 1600ms ladder
            ProcScript::stable_then_exit(800, 0), // sheep b: exits at t=800
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let a = AppConfig::minimal("a", "./a");
        let mut b = AppConfig::minimal("b", "./b");
        b.autorestart = false;
        handle
            .start(vec![normalize(a).unwrap(), normalize(b).unwrap()])
            .await
            .unwrap();

        let t0 = tokio::time::Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                h.stop(ProcessSelector::Name("a".to_string())).await
            }));
        }
        // Sheep b exits on its own at t=800 and is nobody's kill target.
        await_event(&mut rx, 1, ProcessEventKind::Stop).await;
        let seen_at = t0.elapsed();
        for t in tasks {
            let _ = t.await;
        }
        assert!(
            seen_at < Duration::from_millis(1000),
            "sheep b's own exit was only processed at {seen_at:?} -- the actor \
             was parked inside ctl.send() for sheep a's kill ladder"
        );
    }

    // CRITICAL-2 (probe L): the deadlock tail of the above. Actor parked in
    // ctl.send() (ctl full) while the sheep task parks in actor_tx.send()
    // (mailbox full) used to mean neither could ever make progress again.
    #[tokio::test(start_paused = true)]
    async fn mailbox_flood_during_a_kill_never_deadlocks() {
        let (events, mut rx) = tokio::sync::broadcast::channel(4096);
        let runner = ScriptedRunner::new(vec![ProcScript::ignores_signals()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let a = AppConfig::minimal("a", "./a");
        handle.start(vec![normalize(a).unwrap()]).await.unwrap();

        for _ in 0..8 {
            let h = handle.clone();
            tokio::spawn(async move {
                let _ = h.stop(ProcessSelector::All).await;
            });
        }
        for _ in 0..40 {
            tokio::task::yield_now().await;
        }
        // Stuff the 256-slot mailbox while the actor is (formerly) parked on
        // ctl.send().
        for _ in 0..400 {
            let h = handle.clone();
            tokio::spawn(async move {
                let _ = h.list_checked().await;
            });
        }
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }

        let r = tokio::time::timeout(
            Duration::from_secs(600),
            await_event(&mut rx, 0, ProcessEventKind::Stop),
        )
        .await;
        assert!(
            r.is_ok(),
            "DEADLOCK: actor parked in ctl.send() while the sheep task is \
             parked in actor_tx.send() -- the daemon never recovers"
        );
    }

    // Adversarial finding #2 (whole-branch review, Task 9): a `Delete` that
    // lands on an id AFTER `begin_shutdown` already claimed it (set
    // `manual = Some(Stop)`, first-command-wins per IMPORTANT-4) used to hit
    // `claim_manual`'s already-claimed path and only join `remaining`
    // -- it never got a chance to mark its OWN intent anywhere. When the
    // sheep went terminal, `handle_exited` only deregistered on
    // `manual == Some(ManualKind::Delete)` (false here: it's `Stop`), so the
    // sheep stayed registered as `Stopped` while `resolve_pending` still told
    // the `Delete` caller it succeeded. `pending_delete` fixes this by
    // recording delete-intent independently of who won the `manual` race.
    //
    // A second "decoy" sheep, given a far longer `kill_timeout` than the
    // target, keeps Shutdown's own aggregation open (its `remaining` set
    // isn't empty yet) after the target's exit resolves -- so the actor is
    // still alive and its mailbox still open when this test inspects
    // `list()` right after. Without the decoy, the target's exit would be
    // the LAST thing Shutdown is waiting on too, and resolving it would
    // synchronously complete Shutdown and close the actor's mailbox in the
    // same poll -- leaving no window in which `list()` could ever observe
    // anything (a real trap in the brief's original single-sheep sketch of
    // this test: `handle.list()` after `shutter.await` always hit `EngineStopped`,
    // whether or not the fix was applied).
    #[tokio::test(start_paused = true)]
    async fn delete_racing_shutdown_still_deregisters_the_sheep() {
        let (events, _rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(), // target: default 1600ms kill_timeout ladder
            ProcScript::ignores_signals(), // decoy: kept alive far longer, see comment above
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let target = AppConfig::minimal("svc", "./svc");
        let mut decoy = AppConfig::minimal("decoy", "./decoy");
        decoy.kill_timeout = "600000".parse().unwrap(); // outlives the target's ladder by far
        let started = handle
            .start(vec![normalize(target).unwrap(), normalize(decoy).unwrap()])
            .await
            .unwrap();
        let id = started
            .iter()
            .find(|info| info.name == "svc")
            .expect("target sheep registered")
            .id;

        let h2 = handle.clone();
        let shutter = tokio::spawn(async move { h2.shutdown().await });
        for _ in 0..10 {
            tokio::task::yield_now().await; // let Shutdown claim the manual marker first
        }
        let deleted = handle.delete(ProcessSelector::Id(id)).await.unwrap();

        assert_eq!(deleted, vec![id], "the caller was told this id was deleted");
        assert!(
            handle.list().await.iter().all(|info| info.id != id),
            "a Delete that raced a Shutdown must still deregister the sheep, \
             not just tell its caller it did"
        );

        // The decoy's own kill ladder (and therefore Shutdown's aggregation)
        // never resolves under the paused clock within this test -- nothing
        // left to assert once the race outcome above is confirmed, so drop
        // the still-in-flight Shutdown call rather than waiting on it.
        drop(shutter);
    }

    // Fix-round regression (reviewer finding, Task 9): a Delete racing a
    // RESTART, not a Shutdown. `handle_exited`'s manual-Restart branch is
    // the ONE path that resolves an exit without ever consulting
    // `decide_on_exit` -- every other path reaches the
    // `Decision::CleanStop if manual == Some(Delete) || pending_delete`
    // guard, but this one used to `return` straight to `self.respawn(...)`
    // whenever `manual == Some(Restart)`, ignoring `pending_delete`
    // entirely. Reachable exactly like the Shutdown race: `Restart(id)`
    // claims `slot.manual = Some(Restart)` on a running sheep; a racing
    // `Delete(id)` finds the marker already claimed by another operator
    // command, correctly sets `pending_delete = true`, but never touches
    // `manual` -- the carve-out for an AUTOMATIC restart does not apply, and
    // that is the whole point of keeping this path working. Worse than the
    // original bug: instead of leaving a stale `Stopped` entry behind, this
    // one respawned a BRAND-NEW LIVE PROCESS while still telling the
    // `Delete` caller it succeeded.
    #[tokio::test(start_paused = true)]
    async fn delete_racing_restart_still_deregisters_the_sheep() {
        let (events, _rx) = tokio::sync::broadcast::channel(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(), // wide kill-ladder window
            // A second script so the pre-fix bug's illegitimate respawn
            // actually SUCCEEDS into a live, Online process (worse than the
            // original bug: without this, an exhausted script pool would
            // land the buggy respawn attempt in Errored instead, still
            // failing this test but masking exactly how bad the bug is).
            ProcScript::never_exits(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("svc", "./svc");
        let started = handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        let id = started[0].id;

        let h2 = handle.clone();
        let restarter = tokio::spawn(async move { h2.restart(ProcessSelector::All).await });
        for _ in 0..10 {
            tokio::task::yield_now().await; // let Restart claim the manual marker first
        }
        let deleted = handle.delete(ProcessSelector::Id(id)).await.unwrap();
        let restarted = restarter.await.unwrap().unwrap();

        assert_eq!(deleted, vec![id], "the caller was told this id was deleted");
        // Both callers get the SAME honest outcome (IMPORTANT-4 semantics):
        // the restart() caller must NOT see a respawned Online process --
        // if it does, a brand-new live child was spawned behind the
        // Delete's back.
        assert_eq!(restarted[0].id, id);
        assert_eq!(
            restarted[0].status,
            ProcStatus::Stopped,
            "restart() must not report a respawned process once a racing \
             Delete has claimed this id -- got {restarted:?}"
        );
        assert!(
            handle.list().await.iter().all(|info| info.id != id),
            "a Delete that raced a Restart must still deregister the sheep, \
             not respawn a brand-new live process while telling its caller \
             the sheep was deleted"
        );
    }

    // An automatic restart — a memory breach or a liveness failure, arriving
    // through `extra_restart` — is mid-kill-ladder when an operator's `stop`
    // lands on the same sheep. The operator's intent wins: the sheep ends
    // `Stopped`, never respawned, and `stop()` reports that honestly.
    //
    // The counterpart of `overlapping_stop_and_restart_agree_on_one_outcome`:
    // the same race, on the other side of the carve-out. `extra_restart` is
    // the only command with no reply, which is what lets it still be in flight
    // when the next command arrives without a second task holding it there.
    //
    // fails if `claim_manual` stops letting an operator's command take the
    // `manual` marker off an automatic restart: the restart keeps the marker,
    // `handle_exited` respawns, and `stop()` hands its caller an `Online`
    // snapshot of a sheep that is genuinely back up with `restarts: 1`.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_beats_an_automatic_restart_mid_ladder() {
        let (events, _rx) = tokio::sync::broadcast::channel(1024);
        // Two scripts is the most this test can demand: the sheep itself, plus
        // the one respawn a broken implementation performs. Sized for that
        // second spawn on purpose -- a pool of one would answer it
        // `SpawnFailed("script exhausted")`, landing the bug in `Errored`
        // instead of the `Online` that shows how bad it is.
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(), // 1600ms ladder: a wide race window
            ProcScript::never_exits(),     // the respawn a broken implementation performs
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("svc", "./svc");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        let running = handle.list().await.remove(0);
        let pid = running.pid.expect("an online sheep has a pid");

        // Both sends land in the same mailbox in this order, so the actor sets
        // the restart's marker and starts its ladder before it ever sees the
        // stop -- no second task and no yielding needed.
        handle.extra_restart(running.id, pid).await;
        let stopped = handle.stop(ProcessSelector::All).await.unwrap();

        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].id, running.id);
        assert_eq!(
            (stopped[0].status, stopped[0].restarts),
            (ProcStatus::Stopped, 0),
            "an operator's stop was silently converted into the automatic \
             restart it raced -- got {stopped:?}"
        );
        let listed = handle.list().await;
        assert_eq!(
            (listed[0].status, listed[0].pid),
            (ProcStatus::Stopped, None),
            "the sheep an operator stopped is running again -- got {listed:?}"
        );
    }

    // The `Delete` sibling of the test above: an operator's `delete` racing the
    // same in-flight automatic restart deregisters the sheep instead of respawning
    // it. Deliberately belt-and-braces — both `claim_manual`'s carve-out and
    // `pending_delete` independently produce this outcome, which is why it
    // takes disabling both to redden this test.
    //
    // fails if `handle_exited`'s terminal branch stops honouring delete intent
    // (the `Decision::CleanStop if manual == Some(ManualKind::Delete) ||
    // pending_delete` guard): the sheep stays registered as `Stopped` while
    // the `delete()` caller is told it was deleted.
    #[tokio::test(start_paused = true)]
    async fn an_operators_delete_beats_an_automatic_restart_mid_ladder() {
        let (events, _rx) = tokio::sync::broadcast::channel(1024);
        // Two, for the same reason as above: the sheep, plus the respawn a
        // broken implementation performs behind the delete's back.
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(),
            ProcScript::never_exits(),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("svc", "./svc");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        let running = handle.list().await.remove(0);
        let pid = running.pid.expect("an online sheep has a pid");

        handle.extra_restart(running.id, pid).await;
        let deleted = handle
            .delete(ProcessSelector::Id(running.id))
            .await
            .unwrap();

        assert_eq!(deleted, vec![running.id]);
        assert!(
            handle.list().await.is_empty(),
            "a delete that raced an automatic restart must still deregister \
             the sheep, not leave one behind for the restart to bring back"
        );
    }

    // --- `Stopping`: reload's drainee, pinned against the guards it must
    // never pass ---
    //
    // Nothing in production code sets `ProcStatus::Stopping` yet (reload's
    // state machine is a later addition), so there is no black-box path —
    // no `SupervisorHandle` call — that lands a sheep in it. These two cases
    // build the actor directly instead, the same private-module access
    // `spawn`'s own struct literal uses, and call the guarded handlers as
    // plain functions. That is a deliberate, narrower unit test of the
    // guard itself, not a stand-in for coverage a real reload path will
    // also need once it exists.

    /// One sheep already `Stopping`, wired the way a reload's drainee will
    /// be: a live `ctl` sender (its kill ladder owns the next exit) and a
    /// pid a stale report can be raised against. The runner carries no
    /// scripts — a guard that correctly rejects `Stopping` never asks it to
    /// spawn, so an empty script list is what turns a broken guard's spawn
    /// attempt into a loud `SpawnFailed("script exhausted")` instead of a
    /// silent pass.
    fn actor_with_stopping_drainee(
        dir: &tempfile::TempDir,
        pid: u32,
        epoch: u64,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<SheepCtl>) {
        let paths = test_paths(dir);
        let app = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        let mut entry = armed_entry(0, 0, pid, app, &paths);
        entry.status = ProcStatus::Stopping;
        let (ctl_tx, ctl_rx) = mpsc::channel(1);
        let slot = SheepSlot {
            entry,
            ctl: Some(ctl_tx),
            log_ctl: None,
            manual: None,
            pending_delete: false,
            epoch,
            ready_tx: None,
        };
        let mut sheep = HashMap::new();
        sheep.insert(0, slot);
        let (events, _events_rx) = broadcast::channel(16);
        let (tx, _rx) = mpsc::channel(16);
        let actor = Actor {
            runner: ScriptedRunner::new(vec![]),
            paths,
            events,
            tx,
            sheep,
            next_id: 1,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
        };
        (actor, ctl_rx)
    }

    // fails if `handle_extra_restart`'s guard 4 stops checking status ==
    // `Online` — e.g. if it let `Stopping` through too. A liveness failure
    // or memory breach reported against a reload's drainee (which has
    // exactly this shape: `Online`-like, a live `ctl`, a matching pid) must
    // never claim its manual marker or send it a second `Kill` — its own
    // kill ladder already owns its next exit.
    #[test]
    fn a_stopping_sheep_rejects_an_extra_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut ctl_rx) = actor_with_stopping_drainee(&dir, 4242, 0);

        actor.handle_extra_restart(0, 4242);

        let slot = actor.sheep.get(&0).expect("the sheep stays registered");
        assert_eq!(
            slot.entry.status,
            ProcStatus::Stopping,
            "a rejected extra restart must never touch status"
        );
        assert!(
            slot.manual.is_none(),
            "a Stopping sheep must never claim the manual marker off an extra restart"
        );
        assert!(
            ctl_rx.try_recv().is_err(),
            "a Stopping sheep must never receive a second Kill"
        );
    }

    // fails if `handle_restart_due`'s guard stops checking status ==
    // `WaitingRestart` — e.g. if it let `Stopping` through too. A backoff
    // timer scheduled before a reload started draining this sheep must
    // never respawn it: the slot it would respawn into already belongs to
    // the fresh replacement.
    #[test]
    fn a_stopping_sheep_rejects_a_restart_due() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _ctl_rx) = actor_with_stopping_drainee(&dir, 4242, 7);

        actor.handle_restart_due(0, 7);

        let slot = actor.sheep.get(&0).expect("the sheep stays registered");
        assert_eq!(
            slot.entry.status,
            ProcStatus::Stopping,
            "a rejected restart-due must never touch status"
        );
        assert_eq!(
            slot.entry.restarts, 0,
            "a rejected restart-due must never respawn"
        );
    }

    // --- Reload: the per-instance swap machine ---

    /// A window generous enough to cover a whole swap (`listen_timeout` +
    /// `graceful_timeout` + room), so a case whose event never arrives fails
    /// instead of parking the suite. Virtual time, so it costs nothing when
    /// the swap lands early.
    const SWAP_WINDOW: Duration = Duration::from_secs(30);

    /// Drives virtual time until `kind` arrives for `id`, failing rather than
    /// hanging if it never does.
    async fn expect_event(
        rx: &mut tokio::sync::broadcast::Receiver<BusEvent>,
        id: u32,
        kind: ProcessEventKind,
    ) {
        assert!(
            tokio::time::timeout(SWAP_WINDOW, await_event(rx, id, kind))
                .await
                .is_ok(),
            "no {kind:?} for id {id} within {SWAP_WINDOW:?}"
        );
    }

    /// One started app, the runner behind it and a bus subscriber — the
    /// shape every reload case opens with (IR-33).
    ///
    /// The runner is shared rather than moved so a case can read
    /// `kill_counts().len()`, which is the number of spawns that SUCCEEDED
    /// and therefore the only way to ask "did anything spawn that should not
    /// have".
    async fn started(
        dir: &tempfile::TempDir,
        app: AppConfig,
        scripts: Vec<ProcScript>,
    ) -> (
        SupervisorHandle,
        Arc<ScriptedRunner>,
        tokio::sync::broadcast::Receiver<BusEvent>,
    ) {
        let (events, rx) = tokio::sync::broadcast::channel(256);
        let runner = Arc::new(ScriptedRunner::new(scripts));
        let handle = spawn_supervisor(SharedRunner(Arc::clone(&runner)), test_paths(dir), events);
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        (handle, runner, rx)
    }

    /// A bare actor holding one `Online` sheep, for the two cases that drive
    /// a handler directly.
    ///
    /// Direct because what they assert on is not reachable from outside: a
    /// swap's ownership lives in `ProcessEntry::reload`, which is
    /// crate-internal and deliberately never on the wire.
    fn actor_with_one_online_sheep(
        dir: &tempfile::TempDir,
        scripts: Vec<ProcScript>,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<Msg>) {
        let paths = test_paths(dir);
        let app = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        let mut sheep = HashMap::new();
        sheep.insert(
            0,
            SheepSlot {
                entry: armed_entry(0, 0, 1111, app, &paths),
                ctl: None,
                log_ctl: None,
                manual: None,
                pending_delete: false,
                epoch: 0,
                ready_tx: None,
            },
        );
        let (events, _events_rx) = broadcast::channel(64);
        let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
        let actor = Actor {
            runner: ScriptedRunner::new(scripts),
            paths,
            events,
            tx,
            sheep,
            next_id: 1,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
        };
        (actor, rx)
    }

    /// A [`ScriptedRunner`] that refuses one spawn by ordinal and forwards
    /// every other one.
    ///
    /// `ScriptedRunner` can only fail by running out of scripts, and that
    /// fails every spawn from then on — so a reload that wrongly carried on
    /// after a failed replacement would fail its NEXT spawn too, making the
    /// two behaviours indistinguishable. Refusing exactly one keeps them
    /// apart: a correct reload stops there, a broken one produces a live
    /// second replacement.
    struct RefusesOneSpawn {
        inner: ScriptedRunner,
        /// Which spawn, counting from the engine's first, is refused.
        refuse: usize,
        /// Every spawn ATTEMPTED, refused ones included — which is what
        /// `ScriptedRunner`'s own counters cannot report.
        attempts: Arc<AtomicUsize>,
    }

    impl fmt::Debug for RefusesOneSpawn {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RefusesOneSpawn").finish_non_exhaustive()
        }
    }

    impl ProcessRunner for RefusesOneSpawn {
        type Proc = crate::fake::FakeProc;

        fn spawn(
            &self,
            spec: &SpawnSpec,
        ) -> Result<(Self::Proc, ProcIo), crate::runner::RunnerError> {
            let nth = self.attempts.fetch_add(1, AtomicOrdering::SeqCst);
            if nth == self.refuse {
                return Err(crate::runner::RunnerError::SpawnFailed(
                    "refused by the fixture".to_string(),
                ));
            }
            self.inner.spawn(spec)
        }
    }

    // fails if the two halves of a swap land on the wrong entries.
    // `SpawningReplacement` names the replacement and belongs on the
    // drainee; `Draining` names the drainee's OS pid and belongs on the
    // replacement; `Stopping` is the drainee's status and only the
    // drainee's. Asserted against the machine that sets them rather than a
    // rehearsal of it — `ProcessEntry::reload` never reaches the wire, so
    // this is the only tier that can read it back.
    #[tokio::test(start_paused = true)]
    async fn a_swap_puts_each_half_of_a_reload_on_the_entry_that_owns_it() {
        let dir = tempfile::tempdir().unwrap();
        // One script for the one spawn a correct `SpawnNew` performs.
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);

        let new_id = actor
            .spawn_replacement(0)
            .expect("the fixture's one script covers this spawn");

        assert_ne!(new_id, 0, "a replacement never reuses the drainee's id");
        let drainee = &actor.sheep[&0].entry;
        let replacement = &actor.sheep[&new_id].entry;

        assert_eq!(drainee.status, ProcStatus::Stopping);
        assert_eq!(drainee.reload, ReloadState::SpawningReplacement { new_id });
        assert_ne!(
            replacement.status,
            ProcStatus::Stopping,
            "`Stopping` belongs to the instance going away, not the one arriving"
        );
        assert_eq!(replacement.status, ProcStatus::Starting);
        assert_eq!(replacement.reload, ReloadState::Draining { old_pid: 1111 });
        assert_eq!(
            replacement.instance, drainee.instance,
            "a replacement takes the drainee's instance slot, or an app deriving \
             its port from it binds a different one and nothing overlaps"
        );
    }

    // fails if a reload is accepted once a graceful shutdown has begun
    // (CRITICAL-1): its replacement would be a child outside the shutdown
    // aggregation's `online` snapshot, fixed at the moment that ran, and so
    // orphaned when the actor exits. The runner carries NO scripts on
    // purpose — an accepted reload's first act is a spawn, so a broken guard
    // shows up as a registered extra entry rather than as nothing at all.
    #[tokio::test(start_paused = true)]
    async fn a_reload_is_refused_once_a_shutdown_has_begun() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_with_one_online_sheep(&dir, vec![]);
        actor.shutting_down = true;

        let (reply, rx) = oneshot::channel();
        actor.handle_command(Command::Reload {
            selector: ProcessSelector::All,
            reply,
        });

        assert_eq!(rx.await, Ok(Err(SupervisorError::EngineStopped)));
        assert_eq!(actor.sheep.len(), 1, "nothing new was registered");
        assert_eq!(actor.sheep[&0].entry.status, ProcStatus::Online);
        assert!(actor.reloads.is_empty(), "no job was started");
    }

    // fails if a replacement reuses the drainee's id — "never two live
    // processes for one id" is what the property test at the bottom of this
    // file asserts over the whole event stream — or if it takes a fresh
    // instance slot instead of the drainee's, which the log paths are the
    // observable half of. Also fails if the drainee's registration outlives
    // it: nothing else in the engine ever removes it, so the flock would
    // grow a dead row per instance per reload.
    #[tokio::test(start_paused = true)]
    async fn a_reload_gives_the_replacement_a_new_id_in_the_drainees_slot() {
        let dir = tempfile::tempdir().unwrap();
        // Two spawns: the original, and the one replacement a correct reload
        // performs.
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;
        let before = handle.list().await;

        let accepted = handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        assert_eq!(
            accepted.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![0],
            "the answer is the flock as it stood when the reload was accepted"
        );

        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;

        let after = handle.list().await;
        assert_eq!(after.len(), 1, "the drainee's registration goes with it");
        assert_eq!(after[0].id, 1);
        assert_eq!(after[0].status, ProcStatus::Online);
        assert_eq!(after[0].out_file, before[0].out_file);
        assert_eq!(after[0].err_file, before[0].err_file);
        assert_eq!(
            runner.kill_counts().len(),
            2,
            "one original and one replacement, and nothing else"
        );
    }

    // fails if the drainee is only marked `Stopping` when its drain starts,
    // which is what an implementation that set it in `DrainOld` would do.
    // For the whole `AwaitReady` window — up to `listen_timeout` — the app
    // would then have two entries that `snapshot.rs`'s `is_running` counts,
    // so a muster roll written during a reload records an instance count the
    // flock does not have; and `handle_extra_restart`'s `Online` guard would
    // stop rejecting a liveness report raised against the very instance shep
    // is in the middle of replacing.
    #[tokio::test(start_paused = true)]
    async fn a_reload_stops_counting_the_drainee_as_running_before_its_replacement_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, _runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;

        let mid = handle.list().await;
        assert_eq!(mid.len(), 2, "both entries are registered mid-swap");
        assert_eq!(mid[0].status, ProcStatus::Stopping, "the drainee");
        assert_eq!(mid[1].status, ProcStatus::Starting, "its replacement");
        let running = mid
            .iter()
            .filter(|info| {
                matches!(
                    info.status,
                    ProcStatus::Online | ProcStatus::Starting | ProcStatus::WaitingRestart
                )
            })
            .count();
        assert_eq!(
            running, 1,
            "a one-instance app must never count as two running instances"
        );
    }

    // fails if a reload reads a readiness DEADLINE ELAPSING as failure
    // instead of reading the wait's verdict. `await_ready`'s `Heuristic` arm
    // returns `Ready` at the deadline, because for an app that configures
    // neither `wait_ready` nor `readiness_probe` the elapse IS the signal —
    // and that arm has no other caller, so this is the case that runs it. An
    // implementation keyed on the deadline abandons every reload of every
    // such app, which is most apps, and the drainee it correctly leaves
    // behind makes the failure read as a no-op.
    #[tokio::test(start_paused = true)]
    async fn a_reload_of_an_app_with_no_readiness_signal_completes_at_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        // Not the 3000ms default: a distinctive value is what tells "waited
        // for the heuristic" apart from "waited for something else".
        app.listen_timeout = UpDuration::from_millis(2_500);
        let (handle, _runner, mut rx) = started(
            &dir,
            app,
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;

        let start = tokio::time::Instant::now();
        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        assert_eq!(
            tokio::time::Instant::now() - start,
            Duration::from_millis(2_500)
        );

        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        let after = handle.list().await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, 1);
        assert_eq!(after[0].status, ProcStatus::Online);
    }

    // fails if a replacement's readiness task is not armed on the shepherd
    // channel: a `wait_ready` app's `{"kind":"ready"}` is exactly what a
    // reload should be waiting for, and a swap that ignored it would sit out
    // the whole `listen_timeout` before committing.
    #[tokio::test(start_paused = true)]
    async fn a_reload_commits_the_moment_the_replacement_signals_ready() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true;
        let (handle, _runner, mut rx) = started(
            &dir,
            app,
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;
        // The original is gated too, so it needs its own signal before it is
        // `Online` and therefore reloadable.
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        let start = tokio::time::Instant::now();
        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;
        handle.tx.send(Msg::Ready { id: 1 }).await.unwrap();
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;

        assert!(
            tokio::time::Instant::now() - start < Duration::from_millis(3_000),
            "the swap committed at the signal, not at `listen_timeout`"
        );
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        assert_eq!(handle.list().await[0].id, 1);
    }

    // fails if a replacement that never becomes ready is taken online
    // anyway. That is the ordinary readiness rule — a slow app goes online
    // rather than becoming a restart loop — and the one rule a reload must
    // not inherit: committing to an instance that has not proved it can
    // serve means killing the one that can. Also fails if the abandoned
    // replacement is left registered or left running: it got far enough to
    // fork lambs, so it goes through the ladder, and its slot goes with it.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_never_becomes_ready_leaves_the_old_instance_serving() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals the replacement
        let (handle, runner, mut rx) = started(
            &dir,
            app,
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Delete).await;

        let after = handle.list().await;
        assert_eq!(after.len(), 1, "the abandoned replacement is deregistered");
        assert_eq!(after[0].id, 0);
        assert_eq!(
            after[0].status,
            ProcStatus::Online,
            "the instance that can serve keeps serving"
        );
        assert_eq!(
            runner.signals(1),
            vec![15],
            "the replacement went through the stop ladder rather than being dropped"
        );
        assert_eq!(runner.kill_counts(), vec![0, 0], "neither needed SIGKILL");
    }

    // fails if the drain runs under `kill_timeout` rather than
    // `graceful_timeout` — 1600ms against 8000ms by default, the difference
    // between giving an instance time to finish what it is holding and
    // shooting it for taking any.
    #[tokio::test(start_paused = true)]
    async fn a_reload_drains_the_old_instance_under_graceful_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            // The drainee ignores its stop signal, so the drain runs its cap
            // out in full and the cap is what the elapsed time measures.
            vec![ProcScript::ignores_signals(), ProcScript::never_exits()],
        )
        .await;

        let start = tokio::time::Instant::now();
        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;

        assert_eq!(
            tokio::time::Instant::now() - start,
            // listen_timeout (3000) + graceful_timeout (8000)
            Duration::from_millis(11_000)
        );
        assert_eq!(
            runner.kill_counts(),
            vec![1, 0],
            "only the defiant drainee reached the SIGKILL rung"
        );
    }

    // fails if a reload starts every instance's swap at once instead of one
    // at a time: a clustered app would then be entirely `Stopping` for the
    // whole window, with nothing left holding the old listeners, which is
    // the rolling half of a rolling replacement.
    #[tokio::test(start_paused = true)]
    async fn a_reload_replaces_a_clustered_apps_instances_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        // Four spawns: two originals and two replacements. An
        // implementation that started both swaps together performs the same
        // four, so the count is not what tells them apart — WHEN the fourth
        // happens is, which is what the mid-run count below reads.
        let (handle, runner, mut rx) = started(&dir, app, vec![ProcScript::never_exits(); 4]).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");

        expect_event(&mut rx, 2, ProcessEventKind::Start).await;
        assert_eq!(
            runner.kill_counts().len(),
            3,
            "only the first instance's replacement exists yet"
        );
        assert_eq!(
            handle
                .list()
                .await
                .iter()
                .filter(|info| info.status == ProcStatus::Stopping)
                .count(),
            1,
            "one instance is being replaced, and the other is untouched"
        );

        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        expect_event(&mut rx, 3, ProcessEventKind::Start).await;
        assert_eq!(runner.kill_counts().len(), 4);
        expect_event(&mut rx, 1, ProcessEventKind::Delete).await;

        let after = handle.list().await;
        assert_eq!(
            after.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(after.iter().all(|info| info.status == ProcStatus::Online));
    }

    // fails if a replacement that cannot be spawned lets the reload carry on
    // to the next instance — spec §4: failure of the new instance aborts the
    // rest and keeps the old instances running. Also fails if the drainee is
    // left `Stopping` after the spawn that was to replace it never happened,
    // which would take it out of the muster roll and out of reach of a
    // liveness restart for the rest of its life.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_cannot_be_spawned_leaves_every_instance_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        // Four scripts, of which a correct run uses two. The fixture refuses
        // the third spawn — the first replacement — and the fourth script is
        // there for the spawn a reload that wrongly carried on would take.
        // Sizing for the BROKEN run is the point: an exhausted pool would
        // refuse that spawn too and hide the difference.
        let attempts = Arc::new(AtomicUsize::new(0));
        let runner = RefusesOneSpawn {
            inner: ScriptedRunner::new(vec![ProcScript::never_exits(); 4]),
            refuse: 2,
            attempts: Arc::clone(&attempts),
        };
        let (events, mut rx) = tokio::sync::broadcast::channel(256);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted before anything is spawned");

        // Nothing further should happen, so a bounded window is what proves
        // it: a reload that carried on would spawn instance 1's replacement
        // under id 3.
        assert_no_event_within(&mut rx, 3, ProcessEventKind::Start, Duration::from_secs(10)).await;

        let after = handle.list().await;
        assert_eq!(
            after.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(
            after.iter().all(|info| info.status == ProcStatus::Online),
            "both old instances keep serving: {after:?}"
        );
        assert_eq!(
            attempts.load(AtomicOrdering::SeqCst),
            3,
            "two originals and one refused replacement, and no second attempt"
        );
    }

    // fails if a second reload of an app already reloading is accepted: two
    // swaps would put three entries in one instance slot, and neither
    // replacement would know which one it is meant to outlive.
    #[tokio::test(start_paused = true)]
    async fn a_second_reload_of_an_app_already_reloading_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the first reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;

        let refused = handle.reload(ProcessSelector::All).await;

        assert_eq!(
            refused,
            Err(SupervisorError::ReloadInFlight("web".to_string()))
        );
        assert_eq!(
            runner.kill_counts().len(),
            2,
            "no second replacement was spawned"
        );
    }

    // fails if a reload of a sheep that is not `Online` starts one anyway.
    // There is nothing to keep reachable, and starting a stopped instance
    // would surprise an operator who stopped it on purpose. It is reported
    // as a success rather than an error, so one stopped sheep in the flock
    // does not fail a reload of the rest.
    #[tokio::test(start_paused = true)]
    async fn a_reload_of_a_sheep_that_is_not_online_is_a_no_op_success() {
        let dir = tempfile::tempdir().unwrap();
        // One script: a correct reload spawns nothing at all here.
        let (handle, runner, _rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits()],
        )
        .await;
        handle.stop(ProcessSelector::All).await.unwrap();

        let reloaded = handle
            .reload(ProcessSelector::All)
            .await
            .expect("a reload that has nothing to replace still succeeds");

        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].status, ProcStatus::Stopped);
        assert_eq!(handle.list().await.len(), 1, "nothing was registered");
        assert_eq!(runner.kill_counts().len(), 1, "nothing was spawned");
    }

    // fails if the drainee is left `Online` through the `AwaitReady` window.
    // A liveness failure or memory breach raised against it would then pass
    // `handle_extra_restart`'s status guard, claim its marker and kill it —
    // ending the instance shep is in the middle of replacing before the
    // replacement can serve, which is the outage a reload exists to avoid.
    #[tokio::test(start_paused = true)]
    async fn a_report_raised_against_a_drainee_never_takes_it_off_the_reload() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts: original and replacement. A report that wrongly
        // restarted the drainee would take a third and find the pool empty,
        // so the count below reads that as well as the kill.
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;
        handle.extra_restart(0, pid).await;

        // Shorter than `listen_timeout`, so the only thing that could end
        // the drainee inside this window is the report.
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Delete,
            Duration::from_millis(1_000),
        )
        .await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopping);

        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        assert_eq!(
            runner.kill_counts().len(),
            2,
            "the report never caused a spawn"
        );
    }

    // fails if a drainee's own exit is handed to `decide_on_exit`: an
    // `autorestart` app's drainee would be respawned straight back into the
    // instance slot its replacement already holds — two live processes for
    // one instance, with nothing left to reconcile them.
    #[tokio::test(start_paused = true)]
    async fn a_drainee_that_exits_on_its_own_is_never_restarted() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts: the original, which ends 1000ms in — a stable run, so
        // the restart policy would restart it — and the one replacement. A
        // drainee handed to that policy takes a third and finds the pool
        // empty, so it would land `Errored` and still be registered, which
        // is what the assertions below read.
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![
                ProcScript::stable_then_exit(1_000, 1),
                ProcScript::never_exits(),
            ],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;

        let after = handle.list().await;
        assert_eq!(after.len(), 1, "the drainee left no registration behind");
        assert_eq!(after[0].id, 1);
        assert_eq!(after[0].status, ProcStatus::Online);
        assert_eq!(runner.kill_counts().len(), 2, "the drainee never respawned");
    }

    // fails if an operator's `stop` landing mid-reload DELETES the app. The
    // drainee's registration is the reload's to reap only once its
    // replacement is serving; before that, an operator's own command decides
    // what the exit becomes, and `stop` leaves a sheep registered and
    // `Stopped`. Deregistering both entries would take an app out of `shep
    // flock` entirely on a verb that never promised to.
    //
    // The two scripts are asymmetric on purpose, and the case is worthless
    // without it. A `stop` claims both entries at once, so which exit the
    // actor handles first decides what is observable: if the REPLACEMENT's
    // lands first, abandoning the reload clears the drainee's marker before
    // its own exit is ever looked at, and a drainee-always-reaped
    // implementation passes. Making the replacement defy its signal puts its
    // exit a whole `kill_timeout` behind the drainee's, which is the order
    // that exercises the branch.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_mid_reload_leaves_the_app_stopped_and_registered() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts: the original and the one replacement. The stop lands
        // during `AwaitReady`, so no third spawn is correct here either.
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(), ProcScript::ignores_signals()],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;

        let stopped = handle
            .stop(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the stop reaches both entries");
        assert_eq!(stopped.len(), 2, "a stop answers for every id it matched");

        let after = handle.list().await;
        assert_eq!(
            after.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![0],
            "the instance stays registered; only the abandoned replacement goes"
        );
        assert_eq!(after[0].status, ProcStatus::Stopped);
        assert_eq!(runner.kill_counts().len(), 2, "nothing was spawned again");
    }

    // fails if a replacement whose readiness deadline elapses is killed even
    // when the instance it was replacing has already gone. Abandoning exists
    // to keep the instance that can still serve; with none left, killing the
    // replacement too empties the slot outright — no entry, no restart, and
    // nothing in `shep flock` to say the app lost an instance.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_is_kept_when_the_deadline_elapses_with_no_old_instance_left() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals the replacement
        // Two scripts: the original — which ends 1000ms in, before the
        // replacement's 3000ms deadline — and the replacement itself. An
        // implementation that killed the replacement anyway would leave an
        // empty flock, which is what the count below reads.
        let (handle, runner, mut rx) = started(
            &dir,
            app,
            vec![
                ProcScript::stable_then_exit(1_000, 1),
                ProcScript::never_exits(),
            ],
        )
        .await;
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;

        let after = handle.list().await;
        assert_eq!(after.len(), 1, "the app still has its instance");
        assert_eq!(after[0].id, 1);
        assert_eq!(after[0].status, ProcStatus::Online);
        assert_eq!(runner.kill_counts(), vec![0, 0], "neither was SIGKILLed");
    }

    // fails if an automatic restart is allowed to land on either half of an
    // in-flight swap. A cron occurrence and a watched file reach
    // `begin_manual`, which reads no status, so the `Stopping` transition
    // that holds off the other two automatic triggers does nothing for these
    // two. Killing either half abandons the reload and turns the deploy into
    // an ordinary hard restart — for a `watch` app, on any save inside the
    // readiness window, which is the app most likely to be reloaded at all.
    #[tokio::test(start_paused = true)]
    async fn an_automatic_restart_never_lands_on_either_half_of_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        // Both halves are signalled by hand, so the restart below lands
        // squarely inside `AwaitReady` rather than racing a deadline.
        app.wait_ready = true;
        let (handle, runner, mut rx) = started(
            &dir,
            app,
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;

        let restarted = handle
            .restart_automatic(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the selector matches both halves of the swap");
        assert!(
            restarted.is_empty(),
            "neither half of a swap is an automatic restart's to take"
        );

        // The overlap survives, so the swap finishes the way it would have
        // with nothing firing at all.
        handle.tx.send(Msg::Ready { id: 1 }).await.unwrap();
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;

        let after = handle.list().await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, 1, "the replacement, not a restarted drainee");
        assert_eq!(after[0].status, ProcStatus::Online);
        assert_eq!(after[0].restarts, 0, "nothing counted a restart");
        assert_eq!(
            runner.kill_counts().len(),
            2,
            "one original and one replacement, and nothing else"
        );
    }

    // fails if a swap committed by the drainee's OWN death outlives both of
    // its ids. That commit happens in `reap_drainee`, which leaves the job at
    // `DrainOld` with the drainee already deregistered — so no second exit is
    // coming for it, and the replacement's readiness result is the last event
    // that could end the job. When the replacement goes first, clearing its
    // `Draining` marker cancels that event too (`handle_ready_result` routes
    // on the marker), and nothing is left that can reach `finish_swap`. The
    // job then refuses every later reload of the app for as long as the
    // daemon runs, and drops the rest of a clustered reload's queue after the
    // caller was already told `Ok`.
    #[tokio::test(start_paused = true)]
    async fn a_reload_ends_when_its_replacement_goes_with_the_drainee_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals the replacement
        // Two scripts: the original — which ends 1000ms in, inside the
        // replacement's 3000ms readiness window, so the swap commits on its
        // death rather than on a drain — and the replacement itself.
        let (handle, _runner, mut rx) = started(
            &dir,
            app,
            vec![
                ProcScript::stable_then_exit(1_000, 1),
                ProcScript::never_exits(),
            ],
        )
        .await;
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;

        // The cheapest reachable form of the second half: no crash needed,
        // just an operator's `stop` landing before the replacement is ready.
        handle
            .stop(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the stop reaches the replacement");

        let again = handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .map(|infos| infos.iter().map(|info| info.id).collect::<Vec<_>>());
        assert_eq!(
            again,
            Ok(vec![1]),
            "the reload is over, so the app is reloadable again"
        );
    }

    // fails if a replacement starts its restart count at zero. A reload is
    // not a crash, but the count is an operator's view of that instance's
    // history, and resetting it on every deploy makes the number useless.
    #[tokio::test(start_paused = true)]
    async fn a_reload_carries_the_drainees_restart_count_to_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        // Three: the original, the manual restart's, and the replacement.
        let (handle, _runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(); 3],
        )
        .await;
        handle
            .restart(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();
        assert_eq!(handle.list().await[0].restarts, 1);

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;

        let after = handle.list().await;
        assert_eq!(after[0].id, 1);
        assert_eq!(after[0].restarts, 1);
    }

    /// Fails if `run_sheep` lets go of `ProcIo::log_ctl` while its sheep is
    /// still running — an explicit `drop` of the field, or a move of it into
    /// anything that returns before the sheep does. The real runner's log
    /// pump ends with that sender, and the read ends of the child's stdout
    /// and stderr close along with the pump, under a live child.
    ///
    /// Not every way of ignoring the field is such a move, and the two that
    /// look most dangerous are not: `log_ctl: _` and a `..` both leave the
    /// field where it is, and `io` is `run_sheep`'s own parameter, so it
    /// drops when the function returns either way. Both were run against
    /// this case and both stay green, which is why the binding upstairs is
    /// documentation rather than the thing holding the sender up.
    ///
    /// Nothing else in this suite notices a sender that is genuinely
    /// dropped: the scripted fake writes no log files and its procs own no
    /// pipes, so every other case passes without one. This reads the fake's
    /// control task instead, which ends exactly when a real pump would.
    #[tokio::test(start_paused = true)]
    async fn a_live_sheep_keeps_holding_its_log_control_sender() {
        // One script for one spawn, and it must not exit: a proc that exits
        // closes its own control channel, which is indistinguishable here
        // from the dropped sender under test. A second spawn would answer
        // `SpawnFailed("script exhausted")` and never open a channel at all.
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (proc, io) = runner.spawn(&log_ctl_spec()).unwrap();
        assert!(runner.log_ctl_live(0), "sanity: the fake starts it live");

        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let (_ctl_tx, ctl_rx) = mpsc::channel(8);
        let (actor_tx, _actor_rx) = mpsc::channel(8);
        let app = normalize(AppConfig::minimal("svc", "./svc")).unwrap();
        tokio::spawn(run_sheep(7, proc, io, app, ctl_rx, events, actor_tx));

        // A dropped sender closes the channel on the control task's own
        // schedule, so this hands the runtime every chance to run that task
        // before concluding it never had anything to do. Yields rather than
        // a clock advance: the failing path is ready work, not a timer, and
        // the proc under it must stay unexited.
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            runner.log_ctl_live(0),
            "run_sheep dropped ProcIo::log_ctl while its sheep was still \
             running: against the real runner that closes the read ends of \
             the child's stdout and stderr"
        );
    }

    /// Fails if a sheep's log pump is left running once its sheep task is
    /// over, in the one case the sheep's own exit cannot end it: a lamb that
    /// inherited the child's pipes holds both streams open past that exit,
    /// and [`SheepSlot::log_ctl`] keeps a control sender for as long as the
    /// sheep stays registered. What is left to reap the pump is its `logs`
    /// receiver going away with the sheep task — so a pump that noticed that
    /// only when a line arrived would hold its two log files and both pipe
    /// read ends until a `Delete`, or until the daemon exited.
    ///
    /// The slot is deliberately not cleared to fix this, and this case is
    /// what makes keeping the clone free rather than merely convenient. See
    /// [`SheepSlot::log_ctl`] for why the field has one writer and no
    /// second copy of "is the pump still there".
    #[tokio::test(start_paused = true)]
    async fn a_pump_is_reaped_when_its_sheep_ends_even_with_a_lamb_on_the_pipe() {
        let (events, mut rx) = tokio::sync::broadcast::channel(64);
        // One script for one spawn: `autorestart = false` below means the
        // supervisor never asks for a second.
        let runner = Arc::new(ScriptedRunner::new(vec![
            ProcScript::stable_then_exit(1_000, 0).with_a_lamb_holding_the_pipe(),
        ]));
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(SharedRunner(Arc::clone(&runner)), test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.autorestart = false;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        assert!(
            runner.log_ctl_live(0),
            "sanity: a running sheep has a live pump"
        );

        // `Stop`, not `Exit`: with `autorestart` off a clean exit is a clean
        // stop, and `Exit` is the event that announces a pending restart.
        await_event(&mut rx, 0, ProcessEventKind::Stop).await;
        assert_eq!(
            handle.list().await.len(),
            1,
            "sanity: the sheep is still registered, so its slot still holds a \
             clone of the control sender"
        );

        // A bounded poll rather than one read: the pump ends on its own
        // task's schedule, which is after the exit event this woke on.
        let reaped = tokio::time::timeout(Duration::from_secs(5), async {
            while runner.log_ctl_live(0) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            reaped.is_ok(),
            "the pump outlived its sheep task, holding both log files and \
             both pipe read ends open"
        );
    }

    /// A [`ScriptedRunner`] whose spawns hand out a log-control channel
    /// that accepts requests and never answers them.
    ///
    /// Each request is held rather than dropped — holding it holds the
    /// `oneshot` sender inside it, so the acknowledgement stays permanently
    /// owed instead of resolving `Err` the moment the request is discarded.
    /// The scripted fake answers a reopen the instant it arrives, which is
    /// right for every other case and useless for the one below: a reopen
    /// that completes immediately cannot show whether the actor was free
    /// while it was outstanding.
    ///
    /// The `watch` counts requests that have reached a pump. A test needs it
    /// to order itself against the actor: it is proof the actor has already
    /// taken the `Reopen` off its mailbox, without which the probe below
    /// could be answered by an actor that simply had not looked at the
    /// command yet.
    struct SilentPumpRunner {
        inner: ScriptedRunner,
        seen: watch::Sender<u32>,
    }

    impl SilentPumpRunner {
        fn new(scripts: Vec<ProcScript>) -> (Self, watch::Receiver<u32>) {
            let (seen, requests) = watch::channel(0);
            (
                Self {
                    inner: ScriptedRunner::new(scripts),
                    seen,
                },
                requests,
            )
        }
    }

    impl fmt::Debug for SilentPumpRunner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("SilentPumpRunner").finish_non_exhaustive()
        }
    }

    impl ProcessRunner for SilentPumpRunner {
        type Proc = crate::fake::FakeProc;

        fn spawn(
            &self,
            spec: &SpawnSpec,
        ) -> Result<(Self::Proc, ProcIo), crate::runner::RunnerError> {
            let (proc, mut io) = self.inner.spawn(spec)?;
            let (tx, mut rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
            // Replacing the sender drops the fake's own, which ends the
            // control task it spawned — deliberate. This runner's whole
            // purpose is that nothing answers.
            io.log_ctl = tx;
            let seen = self.seen.clone();
            tokio::spawn(async move {
                let mut held = Vec::new();
                while let Some(request) = rx.recv().await {
                    held.push(request);
                    seen.send_modify(|count| *count += 1);
                }
            });
            Ok((proc, io))
        }
    }

    /// Fails if the actor awaits a reopen's acknowledgement inside its own
    /// loop. That is the cycle CRITICAL-2 documents: an actor parked on an
    /// acknowledgement stops draining its mailbox, so its sheep tasks block
    /// sending into it, so nothing drains their `logs`, so the pump that
    /// owes the acknowledgement never gets to answer. Here the pump simply
    /// never answers, which is the same wedge with none of the timing.
    ///
    /// `list` is the probe because it is answered from the actor loop and
    /// from nowhere else, so an answer is proof the loop is still turning.
    ///
    /// Waiting for the request to reach the pump first is what makes the
    /// probe mean anything. Without it the reopen task might not have sent
    /// yet, the actor would answer `list` from an empty mailbox, and an
    /// inline await would pass unnoticed — measured, not assumed: the
    /// version of this case that skipped the wait stayed green under
    /// exactly that mutation.
    #[tokio::test(start_paused = true)]
    async fn the_actor_keeps_answering_while_a_reopen_waits_on_a_silent_pump() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let (runner, mut requests) = SilentPumpRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        handle
            .start(vec![normalize(AppConfig::minimal("web", "./srv")).unwrap()])
            .await
            .unwrap();

        let reopening = tokio::spawn({
            let handle = handle.clone();
            async move { handle.reopen(ProcessSelector::All).await }
        });

        tokio::time::timeout(Duration::from_secs(5), requests.wait_for(|seen| *seen == 1))
            .await
            .expect("the reopen must reach the pump")
            .expect("the runner outlives this wait, so its sender cannot have closed");

        let listed = tokio::time::timeout(Duration::from_secs(5), handle.list())
            .await
            .expect("the actor must keep answering while a reopen is outstanding");
        assert_eq!(listed.len(), 1);
        assert!(
            !reopening.is_finished(),
            "sanity: nothing can acknowledge this reopen, so `list` answering \
             above is not just the reopen having finished first"
        );
        reopening.abort();
    }

    /// Fails if a reopen skips a matched sheep's pump, reaches a sheep the
    /// selector never named, or answers with the wrong set.
    ///
    /// The counts are what make this more than a smoke test: an
    /// implementation that resolved the selector, answered with the right
    /// snapshots and pushed nothing at any pump passes every other
    /// assertion here. Three sheep and a selector naming two of them is the
    /// smallest shape that catches both halves — too narrow and too wide.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_reaches_every_matched_sheep_and_no_others() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Three scripts for three instances, counted: a fourth spawn would
        // answer `SpawnFailed("script exhausted")` and land that sheep in
        // `Errored` with no pump at all — a state this case could not tell
        // apart from the skipped pump it is looking for.
        let runner = Arc::new(ScriptedRunner::new(vec![
            ProcScript::never_exits(),
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]));
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(SharedRunner(Arc::clone(&runner)), test_paths(&dir), events);

        let mut web = AppConfig::minimal("web", "./srv");
        web.instances = 2;
        handle
            .start(vec![
                normalize(web).unwrap(),
                normalize(AppConfig::minimal("api", "./api")).unwrap(),
            ])
            .await
            .unwrap();

        let reopened = handle
            .reopen(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();

        assert_eq!(
            reopened.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![0, 1],
            "the reply must carry both `web` instances, id-sorted, and no `api`"
        );
        assert_eq!(runner.reopens(0), 1, "web's first instance");
        assert_eq!(runner.reopens(1), 1, "web's second instance");
        assert_eq!(runner.reopens(2), 0, "api was never named");
    }

    /// Fails if a respawn leaves [`SheepSlot::log_ctl`] pointing at the pump
    /// of the process it replaced — `slot.log_ctl = Some(log_ctl);` dropped
    /// from [`Actor::respawn`], which is the only line that re-points a slot
    /// after a restart.
    ///
    /// Under that mutation the request goes to a pump whose process is gone,
    /// the send fails, and a failed send is the documented no-op success —
    /// so `shep reopen` and `shep flush` exit 0 having reached nothing, for
    /// every sheep that has restarted since it started. Any restart does it:
    /// an operator's, a crash loop's, a cron occurrence, a watched file, a
    /// memory breach. One verb is enough to pin it, because both read the
    /// same field.
    ///
    /// The counts are the whole case. The reply carries the sheep either
    /// way, and so does its status — an implementation that answered from
    /// the registry and pushed at nothing passes every other assertion here.
    ///
    /// Two scripts, counted: the initial spawn and the restart's. A third is
    /// never asked for, and a pool of one would land the restart in
    /// `SpawnFailed("script exhausted")` — leaving the sheep pumpless, which
    /// reads exactly like the pump that was never reached.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_after_a_restart_reaches_the_pump_the_restart_spawned() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner = Arc::new(ScriptedRunner::new(vec![
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]));
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(SharedRunner(Arc::clone(&runner)), test_paths(&dir), events);
        handle
            .start(vec![normalize(AppConfig::minimal("web", "./srv")).unwrap()])
            .await
            .unwrap();

        let restarted = handle
            .restart(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();
        // The premise, stated rather than assumed: a reopen aimed at a sheep
        // that never actually restarted would reach the first pump and prove
        // nothing.
        assert_eq!(
            (restarted[0].restarts, restarted[0].status),
            (1, ProcStatus::Online),
            "the sheep must be back up on a second process before the reopen"
        );

        let reopened =
            tokio::time::timeout(Duration::from_secs(5), handle.reopen(ProcessSelector::All))
                .await
                .expect("a live pump must answer rather than leave the reopen waiting")
                .expect("a running sheep's reopen must succeed");

        assert_eq!(reopened.len(), 1);
        assert_eq!(
            runner.reopens(1),
            1,
            "the reopen must reach the pump the restart spawned"
        );
        assert_eq!(
            runner.reopens(0),
            0,
            "the pre-restart pump belongs to a process that is gone; a reopen \
             sent there reaches nothing and is reported as a success"
        );
    }

    /// Fails if a selector that matches nothing is answered as a success.
    /// `reopen` is the one selector verb with a default, so a bare `shep
    /// reopen` against an empty flock is the ordinary way to reach this —
    /// and silence would look exactly like a rotation that worked.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_matching_nothing_is_not_found() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        assert_eq!(
            handle.reopen(ProcessSelector::All).await,
            Err(SupervisorError::NotFound)
        );
    }

    /// Fails if a stopped sheep makes a reopen error out, or hang waiting
    /// for an acknowledgement that cannot come. Rotating a flock with one
    /// sheep stopped in it must still succeed and still report that sheep:
    /// there was nothing to reopen, which is not a failure.
    ///
    /// The fake's control task ends with its proc (`ScriptedRunner`'s own
    /// doc), so this sheep's pump is genuinely gone by the time the reopen
    /// is issued rather than merely notionally so.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_sheep_is_a_no_op_success() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // One script, one spawn: `autorestart = false` below means the
        // supervisor never asks for a second.
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.autorestart = false;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        handle
            .stop(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);

        let reopened =
            tokio::time::timeout(Duration::from_secs(5), handle.reopen(ProcessSelector::All))
                .await
                .expect("a reopen aimed at a stopped sheep must not wait for an acknowledgement")
                .expect("a stopped sheep has nothing to reopen, which is a success");
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened[0].status, ProcStatus::Stopped);
    }

    /// Fails if [`reopen_logs`] waits on an acknowledgement that no longer
    /// has anyone to send it — the leg where the pump is already gone and
    /// the send itself fails, which is what a stopped sheep looks like from
    /// the actor's side.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_whose_pump_is_already_gone_returns_at_once() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx); // the pump ended before the request was made

        let outcome = tokio::time::timeout(Duration::from_secs(5), reopen_logs(&tx))
            .await
            .expect("a failed send must end the reopen, not leave it waiting");
        assert_eq!(
            outcome,
            Ok(()),
            "a pump that was never reached reopened nothing, which is a no-op \
             success rather than a reopen that failed"
        );
    }

    /// Fails if [`reopen_logs`] treats a dropped acknowledgement as
    /// something to keep waiting on — the other leg, where the pump ends
    /// between accepting the request and answering it (at both-EOF, or with
    /// its `logs` receiver gone). The request in hand is dropped with it,
    /// taking the `done` sender along.
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_ends_mid_request_still_ends_the_reopen() {
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let request = rx.recv().await.expect("the reopen must reach the pump");
            drop(request); // ends without answering, exactly as a closing pump does
        });

        let outcome = tokio::time::timeout(Duration::from_secs(5), reopen_logs(&tx))
            .await
            .expect("a dropped acknowledgement must end the reopen, not leave it waiting");
        assert_eq!(
            outcome,
            Ok(()),
            "a pump that ended mid-request reopened nothing, which is the same \
             no-op success a failed send is"
        );
    }

    /// The flush half of
    /// [`a_pump_that_ends_mid_request_still_ends_the_reopen`], and the leg
    /// [`flush_logs`] gets wrong most expensively.
    ///
    /// Fails if that function's `ack.await.unwrap_or(Ok(()))` becomes
    /// `unwrap_or(Err(..))`, or waits on an acknowledgement nobody is left to
    /// send. A pump that ended between accepting the request and answering it
    /// holds no handle and so owes no bytes to anything — but the truncate
    /// still has to run, because that is exactly how a sheep which stopped
    /// mid-flush gets its logs emptied. Turning that into a failure would
    /// fail `shep flush all` over any sheep that happened to exit while the
    /// verb was walking the flock.
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_ends_mid_request_still_ends_the_flush() {
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let request = rx.recv().await.expect("the flush must reach the pump");
            drop(request); // ends without answering, exactly as a closing pump does
        });

        let outcome = tokio::time::timeout(Duration::from_secs(5), flush_logs(&tx))
            .await
            .expect("a dropped acknowledgement must end the flush, not leave it waiting");
        assert_eq!(
            outcome,
            Ok(()),
            "a pump that ended mid-request owes no bytes, which is the same \
             no-op success a failed send is"
        );
    }

    /// What [`FailingPumpRunner`]'s pump answers every reopen with. One
    /// owner for the string, because the case below asserts the whole error
    /// it ends up inside — a copy per site could drift and keep passing.
    const PUMP_REFUSAL: &str = "/gone/web-out.log: No such file or directory";

    /// The sheep [`FailingPumpRunner`] gives a failing pump to.
    const REFUSING_SHEEP: &str = "web";

    /// A [`ScriptedRunner`] whose spawn of [`REFUSING_SHEEP`] gets a pump
    /// that answers every reopen with a failure, the way a real one does
    /// when it cannot open a log path again — the rotator took the directory
    /// with it, the mode changed, the disk filled. Every other sheep keeps
    /// the scripted fake's own answering pump.
    ///
    /// By name rather than by spawn order, so one case can hold both halves
    /// — the sheep whose reopen failed and a healthy one beside it — without
    /// either half depending on which was spawned first.
    #[derive(Debug)]
    struct FailingPumpRunner {
        inner: Arc<ScriptedRunner>,
    }

    impl FailingPumpRunner {
        fn new(inner: Arc<ScriptedRunner>) -> Self {
            Self { inner }
        }
    }

    impl ProcessRunner for FailingPumpRunner {
        type Proc = crate::fake::FakeProc;

        fn spawn(
            &self,
            spec: &SpawnSpec,
        ) -> Result<(Self::Proc, ProcIo), crate::runner::RunnerError> {
            let (proc, mut io) = self.inner.spawn(spec)?;
            if spec.name != REFUSING_SHEEP {
                return Ok((proc, io));
            }
            let (tx, mut rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
            // Replacing the sender drops the fake's own, which ends the
            // control task it spawned. This pump answers in its place.
            io.log_ctl = tx;
            tokio::spawn(async move {
                while let Some(ctl) = rx.recv().await {
                    // Both variants, so this pump keeps serving whichever
                    // arrives. Matching on only one would end the loop at the
                    // first of the other and drop the receiver, and every
                    // request after that would look like the no-op success a
                    // vanished pump gets — which is the opposite of what this
                    // runner exists to produce.
                    match ctl {
                        LogCtl::Reopen { done } => {
                            let _ = done.send(Err(ReopenError {
                                message: PUMP_REFUSAL.to_string(),
                            }));
                        }
                        LogCtl::Flush { done } => {
                            let _ = done.send(Err(FlushError {
                                message: PUMP_REFUSAL.to_string(),
                            }));
                        }
                    }
                }
            });
            Ok((proc, io))
        }
    }

    /// Fails if a pump that could not reopen its files is reported as a
    /// success. That sheep is then writing a stream nowhere while `shep
    /// reopen` exits 0 and prints it in the table alongside the sheep that
    /// really were reopened — the silent failure this verb exists to end,
    /// moved one layer up.
    ///
    /// The healthy sheep is the second half of the case: a failure must
    /// name the sheep it belongs to and no other, and must not stop the
    /// rest of the flock being reopened. A `reopen` that gave up at the
    /// first failure would leave `api`'s pump on its renamed inode and
    /// nothing here would say so — except that count.
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_could_not_reopen_fails_the_request_and_names_its_sheep() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Two scripts for two instances, counted: a third spawn would answer
        // `SpawnFailed("script exhausted")` and land that sheep in `Errored`
        // with no pump at all.
        let scripted = Arc::new(ScriptedRunner::new(vec![
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]));
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(
            FailingPumpRunner::new(Arc::clone(&scripted)),
            test_paths(&dir),
            events,
        );
        handle
            .start(vec![
                normalize(AppConfig::minimal("web", "./srv")).unwrap(),
                normalize(AppConfig::minimal("api", "./api")).unwrap(),
            ])
            .await
            .unwrap();

        let error =
            tokio::time::timeout(Duration::from_secs(5), handle.reopen(ProcessSelector::All))
                .await
                .expect("a pump that answers must not leave the reopen waiting")
                .expect_err("a reopen a pump could not carry out must not answer Ok");

        assert_eq!(
            error,
            SupervisorError::ReopenFailed(format!("web (id 0): could not reopen {PUMP_REFUSAL}")),
            "the failure must carry the sheep and the path, and only the sheep that failed"
        );
        assert_eq!(
            scripted.reopens(1),
            1,
            "the healthy sheep must still have been reopened"
        );
    }

    // --- flush -------------------------------------------------------
    //
    // The engine tier can show four of the five things `flush` has to get
    // right: that the request reaches exactly the matched pumps, that the
    // actor stays free while one is outstanding, that the truncate is
    // ordered AFTER the acknowledgement, and that recorded paths are
    // emptied whether or not a pump is there to answer. The fifth — that
    // the path and not the pump's current inode is what gets emptied —
    // needs a pump holding a real handle on a real file, and lives in
    // `tests/daemon_e2e.rs`.

    /// Fails if a flush skips a matched sheep's pump, reaches a sheep the
    /// selector never named, or answers with the wrong set.
    ///
    /// The counts are what make this more than a smoke test, exactly as in
    /// [`a_reopen_reaches_every_matched_sheep_and_no_others`]: an
    /// implementation that resolved the selector, truncated the paths and
    /// pushed nothing at any pump passes every other assertion here — and
    /// that implementation is the one with the bug the flush half exists to
    /// prevent, since nothing would then be waited on before the truncate.
    /// Three sheep and a selector naming two of them is the smallest shape
    /// that catches both halves.
    #[tokio::test(start_paused = true)]
    async fn a_flush_reaches_every_matched_sheep_and_no_others() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Three scripts for three instances, counted, for the reason the
        // reopen case gives: a fourth spawn would fail and land that sheep
        // pumpless, which reads the same as the skip being looked for.
        let runner = Arc::new(ScriptedRunner::new(vec![
            ProcScript::never_exits(),
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]));
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(SharedRunner(Arc::clone(&runner)), test_paths(&dir), events);

        let mut web = AppConfig::minimal("web", "./srv");
        web.instances = 2;
        handle
            .start(vec![
                normalize(web).unwrap(),
                normalize(AppConfig::minimal("api", "./api")).unwrap(),
            ])
            .await
            .unwrap();

        let flushed = handle
            .flush(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();

        assert_eq!(
            flushed.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![0, 1],
            "the reply must carry both `web` instances, id-sorted, and no `api`"
        );
        assert_eq!(runner.flushes(0), 1, "web's first instance");
        assert_eq!(runner.flushes(1), 1, "web's second instance");
        assert_eq!(runner.flushes(2), 0, "api was never named");
        assert_eq!(
            runner.reopens(0),
            0,
            "a flush must push `LogCtl::Flush`, never `LogCtl::Reopen` — a \
             flush wired to the neighbouring variant would swap the flock's \
             handles and empty nothing"
        );
    }

    /// Fails if the actor awaits a flush's acknowledgement inside its own
    /// loop — the same CRITICAL-2 cycle
    /// [`the_actor_keeps_answering_while_a_reopen_waits_on_a_silent_pump`]
    /// describes, reached through the other verb. [`SilentPumpRunner`] holds
    /// every request it is sent whatever the variant, so it wedges a flush
    /// exactly as it wedges a reopen.
    ///
    /// `list` is the probe because it is answered from the actor loop and
    /// from nowhere else. Waiting for the request to reach the pump first is
    /// what makes the probe mean anything — see the reopen case for the
    /// measurement behind that.
    #[tokio::test(start_paused = true)]
    async fn the_actor_keeps_answering_while_a_flush_waits_on_a_silent_pump() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let (runner, mut requests) = SilentPumpRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        handle
            .start(vec![normalize(AppConfig::minimal("web", "./srv")).unwrap()])
            .await
            .unwrap();

        let flushing = tokio::spawn({
            let handle = handle.clone();
            async move { handle.flush(ProcessSelector::All).await }
        });

        tokio::time::timeout(Duration::from_secs(5), requests.wait_for(|seen| *seen == 1))
            .await
            .expect("the flush must reach the pump")
            .expect("the runner outlives this wait, so its sender cannot have closed");

        let listed = tokio::time::timeout(Duration::from_secs(5), handle.list())
            .await
            .expect("the actor must keep answering while a flush is outstanding");
        assert_eq!(listed.len(), 1);
        assert!(
            !flushing.is_finished(),
            "sanity: nothing can acknowledge this flush, so `list` answering \
             above is not just the flush having finished first"
        );
        flushing.abort();
    }

    /// What [`LateWritingPumpRunner`]'s pump appends as it answers a flush.
    /// One owner for the string, because the cases below assert the file it
    /// lands in is empty and a second copy could drift out of agreement with
    /// what was actually written.
    const LATE_LINE: &str = "landed-while-the-flush-was-being-answered\n";

    /// The sheep [`LateWritingPumpRunner`] gives a late-writing pump to.
    const LATE_WRITING_SHEEP: &str = "latecomer";

    /// A [`ScriptedRunner`] whose spawn of [`LATE_WRITING_SHEEP`] gets a pump
    /// that appends [`LATE_LINE`] to that sheep's real stdout log path at the
    /// moment it acknowledges a flush. Every other sheep keeps the scripted
    /// fake's own answering pump.
    ///
    /// This is the whole of the ordering hazard, made deterministic. A real
    /// [`tokio::fs::File`] hands its `write(2)` to the blocking pool and
    /// returns, so at the instant a `Flush` arrives there can be bytes that
    /// have not reached the file yet — and the acknowledgement is what says
    /// they have. Waiting for a real one to land in the right microsecond is
    /// a race; a pump that writes as it answers turns "did the truncate wait
    /// for the acknowledgement?" into a question about file contents, which
    /// is decidable every run.
    ///
    /// By name rather than for every spawn, so a case can point the writing
    /// pump at one particular sheep of several — which is what lets
    /// [`a_sibling_sharing_a_path_is_flushed_even_when_the_selector_skips_it`]
    /// tell the sibling's write apart from the named sheep's.
    ///
    /// The path comes from the [`SpawnSpec`] rather than being derived here,
    /// so this cannot disagree with the assembler about which file the sheep
    /// owns.
    struct LateWritingPumpRunner {
        inner: ScriptedRunner,
    }

    impl LateWritingPumpRunner {
        fn new(inner: ScriptedRunner) -> Self {
            Self { inner }
        }
    }

    impl fmt::Debug for LateWritingPumpRunner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("LateWritingPumpRunner")
                .finish_non_exhaustive()
        }
    }

    impl ProcessRunner for LateWritingPumpRunner {
        type Proc = crate::fake::FakeProc;

        fn spawn(
            &self,
            spec: &SpawnSpec,
        ) -> Result<(Self::Proc, ProcIo), crate::runner::RunnerError> {
            let (proc, mut io) = self.inner.spawn(spec)?;
            if spec.name != LATE_WRITING_SHEEP {
                return Ok((proc, io));
            }
            let (tx, mut rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
            // Replacing the sender drops the fake's own, which ends the
            // control task it spawned. This pump answers in its place.
            io.log_ctl = tx;
            let out_file = spec.out_file.clone();
            tokio::spawn(async move {
                while let Some(ctl) = rx.recv().await {
                    match ctl {
                        LogCtl::Flush { done } => {
                            if let Some(parent) = out_file.parent() {
                                std::fs::create_dir_all(parent).unwrap();
                            }
                            let mut file = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&out_file)
                                .unwrap();
                            std::io::Write::write_all(&mut file, LATE_LINE.as_bytes()).unwrap();
                            // Answered only once the bytes are really on
                            // disk, which is what a real pump's `flush`
                            // promises and what the truncate must wait for.
                            let _ = done.send(Ok(()));
                        }
                        LogCtl::Reopen { done } => {
                            let _ = done.send(Ok(()));
                        }
                    }
                }
            });
            Ok((proc, io))
        }
    }

    /// Fails if the truncate runs before the pump has acknowledged the
    /// flush.
    ///
    /// That ordering is the reason this verb has two halves at all. Without
    /// it the file is emptied while a line is still in flight, the line
    /// lands at offset 0 immediately afterwards under `O_APPEND`, and the
    /// operator is told the log is empty when it holds exactly the one line
    /// they most recently produced.
    ///
    /// Both assertions are load-bearing and catch opposite mutations. That
    /// the file EXISTS is proof the pump was flushed at all — a flush that
    /// only truncated would never create it, and would then pass an
    /// emptiness check vacuously, since the truncate deliberately does not
    /// create a missing path. That it is EMPTY is proof the truncate came
    /// second.
    #[tokio::test(start_paused = true)]
    async fn a_flush_truncates_only_after_its_pump_has_answered() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(
            LateWritingPumpRunner::new(ScriptedRunner::new(vec![ProcScript::never_exits()])),
            test_paths(&dir),
            events,
        );
        handle
            .start(vec![
                normalize(AppConfig::minimal(LATE_WRITING_SHEEP, "./srv")).unwrap(),
            ])
            .await
            .unwrap();

        // Read off the daemon's own snapshot rather than derived here, so
        // the test cannot disagree with the assembler about the path.
        let out_file = PathBuf::from(
            handle.list().await[0]
                .out_file
                .clone()
                .expect("the daemon reports its own resolved log paths"),
        );

        handle.flush(ProcessSelector::All).await.unwrap();

        assert!(
            out_file.exists(),
            "the pump never wrote, so this flush never reached one: {}",
            out_file.display()
        );
        assert_eq!(
            std::fs::read_to_string(&out_file).unwrap(),
            "",
            "a line the pump landed as it answered the flush must not survive \
             the truncate that follows it"
        );
    }

    /// Fails if a flush leaves a stopped sheep's log file alone.
    ///
    /// The operation addresses recorded paths, not open handles, so a sheep
    /// with no pump at all is emptied like any other — and it is worth
    /// emptying, because `shep bleats --no-follow` reads a stopped sheep's
    /// logs. An implementation that flushed only live pumps and truncated
    /// only what it had flushed would answer `Ok` here and change nothing.
    ///
    /// The fake's control task ends with its proc (`ScriptedRunner`'s own
    /// doc), so this sheep's pump is genuinely gone rather than notionally
    /// so, and the truncate is reached through the no-pump leg.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_sheeps_log_file_is_truncated_too() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // One script, one spawn: `autorestart = false` means no second.
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.autorestart = false;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        handle
            .stop(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();

        let listed = handle.list().await;
        assert_eq!(listed[0].status, ProcStatus::Stopped);
        let out_file = PathBuf::from(listed[0].out_file.clone().unwrap());
        std::fs::create_dir_all(out_file.parent().unwrap()).unwrap();
        std::fs::write(&out_file, "what the sheep logged before it stopped\n").unwrap();

        let flushed =
            tokio::time::timeout(Duration::from_secs(5), handle.flush(ProcessSelector::All))
                .await
                .expect("a flush aimed at a stopped sheep must not wait for an acknowledgement")
                .expect("a stopped sheep has no pump to flush, which is not a failure");

        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].status, ProcStatus::Stopped);
        assert_eq!(
            std::fs::read_to_string(&out_file).unwrap(),
            "",
            "a stopped sheep's log is still readable, so it is still emptied"
        );
    }

    /// Fails if instances sharing one log path answer with one row per FILE
    /// rather than one per sheep, or leave the shared file unemptied.
    ///
    /// `merge_logs` points every instance of an app at one path, where each
    /// holds its own independent `O_APPEND` handle. Two decisions meet here.
    /// The answer is keyed by SHEEP — the selector named sheep, `Describe`
    /// would return two rows for the same selector, and an operator reading
    /// `shep flush web` wants to see which sheep it reached. The work is
    /// keyed by PATH — one truncate empties the file for every handle open
    /// on it, so the daemon deduplicates and truncates once.
    ///
    /// The shared path is asserted rather than assumed: without that check a
    /// fixture where `merge_logs` had quietly stopped applying would leave
    /// this case testing two ordinary sheep and proving nothing.
    #[tokio::test(start_paused = true)]
    async fn instances_sharing_one_log_path_answer_one_row_each() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        let mut web = AppConfig::minimal("web", "./srv");
        web.instances = 2;
        web.merge_logs = true;
        handle.start(vec![normalize(web).unwrap()]).await.unwrap();

        let listed = handle.list().await;
        assert_eq!(
            listed[0].out_file, listed[1].out_file,
            "fixture check: `merge_logs` must really point both instances at \
             one path, or this case proves nothing"
        );
        let shared = PathBuf::from(listed[0].out_file.clone().unwrap());
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, "both instances wrote here\n").unwrap();

        let flushed = handle.flush(ProcessSelector::All).await.unwrap();

        assert_eq!(
            flushed.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![0, 1],
            "one row per sheep, not per file emptied"
        );
        assert_eq!(std::fs::read_to_string(&shared).unwrap(), "");
    }

    /// Fails if the set of pumps a flush drains is narrowed back to the sheep
    /// the selector matched, leaving a sibling that writes to the same file
    /// unflushed while that file is emptied under it.
    ///
    /// Two apps, one explicit `out_file` between them, and a selector naming
    /// only the first. The truncate set is paths, so the second app's file is
    /// emptied whether or not the operator named it — and a write it had
    /// already handed to the blocking pool then lands at offset 0 of the file
    /// they were just told is empty. That is the failure the two phases exist
    /// to prevent, in the case the design names as its own motivation.
    ///
    /// [`LateWritingPumpRunner`] is pointed at the UNMATCHED sheep, and the
    /// shared file is deliberately not created up front. With the flush set
    /// narrowed, that pump is never asked, so it never writes, and
    /// [`truncate_log`] creates nothing: the path simply does not exist.
    /// Existence is therefore the proof that the sibling's pump was reached,
    /// and emptiness the proof that the truncate waited for its
    /// acknowledgement.
    #[tokio::test(start_paused = true)]
    async fn a_sibling_sharing_a_path_is_flushed_even_when_the_selector_skips_it() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Two scripts for two apps of one instance each, counted: a third
        // spawn would answer `SpawnFailed("script exhausted")` and land that
        // sheep pumpless, which reads exactly like the skipped pump this case
        // exists to rule out.
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(
            LateWritingPumpRunner::new(ScriptedRunner::new(vec![
                ProcScript::never_exits(),
                ProcScript::never_exits(),
            ])),
            test_paths(&dir),
            events,
        );

        // Two apps pointed at one file, rather than one app's two instances
        // under `merge_logs`: the sibling needs a name of its own for the
        // writing pump to be aimed at it and at nothing else.
        let shared = dir.path().join("shared-out.log");
        let mut named = AppConfig::minimal("web", "./srv");
        named.out_file = Some(shared.display().to_string());
        let mut sibling = AppConfig::minimal(LATE_WRITING_SHEEP, "./api");
        sibling.out_file = Some(shared.display().to_string());
        handle
            .start(vec![normalize(named).unwrap(), normalize(sibling).unwrap()])
            .await
            .unwrap();

        let listed = handle.list().await;
        assert_eq!(
            listed[0].out_file, listed[1].out_file,
            "fixture check: both apps must really resolve to one path, or \
             this case proves nothing"
        );

        let flushed = handle
            .flush(ProcessSelector::Name("web".to_string()))
            .await
            .unwrap();

        assert_eq!(
            flushed.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![0],
            "the reply answers the selector: the sibling's file was emptied \
             too, but the operator never named the sibling and it is not a \
             row here"
        );
        assert!(
            shared.exists(),
            "the sibling's pump writes as it answers a flush, so a path that \
             is not there means it was never asked: {}",
            shared.display()
        );
        assert_eq!(
            std::fs::read_to_string(&shared).unwrap(),
            "",
            "a line the unmatched sibling landed as it answered must not \
             survive the truncate of the path it shares"
        );
    }

    /// Fails if a pump that could not land what it owed is reported as a
    /// success.
    ///
    /// The acknowledgement carries a `Result` for the same reason the
    /// reopen's does, one layer down: pending bytes that never reached the
    /// file are exactly what the truncate is racing, so a flush that
    /// answered `Ok` over them would exit 0 about a log that is about to
    /// gain a line. The failure is keyed by path rather than by sheep — see
    /// [`SupervisorError::FlushFailed`].
    ///
    /// The healthy sheep is the second half: a failure must not stop the
    /// rest of the flock being flushed, and nothing here would say so except
    /// that count.
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_could_not_flush_fails_the_request() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Two scripts for two instances, counted: a third spawn would answer
        // `SpawnFailed("script exhausted")` and land that sheep pumpless.
        let scripted = Arc::new(ScriptedRunner::new(vec![
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]));
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(
            FailingPumpRunner::new(Arc::clone(&scripted)),
            test_paths(&dir),
            events,
        );
        handle
            .start(vec![
                normalize(AppConfig::minimal(REFUSING_SHEEP, "./srv")).unwrap(),
                normalize(AppConfig::minimal("api", "./api")).unwrap(),
            ])
            .await
            .unwrap();

        let error =
            tokio::time::timeout(Duration::from_secs(5), handle.flush(ProcessSelector::All))
                .await
                .expect("a pump that answers must not leave the flush waiting")
                .expect_err("a flush a pump could not carry out must not answer Ok");

        assert_eq!(
            error,
            SupervisorError::FlushFailed(PUMP_REFUSAL.to_string()),
            "the failure must carry the path, and only the path that failed"
        );
        assert_eq!(
            scripted.flushes(1),
            1,
            "the healthy sheep must still have been flushed"
        );
    }

    /// Fails if a selector that matches nothing is answered as a success.
    ///
    /// `flush` demands an explicit selector, so reaching this means the
    /// operator named something — a deleted sheep, a typo — and a zero exit
    /// would tell them the logs they meant are now empty when nothing was
    /// touched at all.
    #[tokio::test(start_paused = true)]
    async fn a_flush_matching_nothing_is_not_found() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(ScriptedRunner::new(vec![]), test_paths(&dir), events);

        assert_eq!(
            handle.flush(ProcessSelector::All).await,
            Err(SupervisorError::NotFound)
        );
    }

    /// Fails if [`truncate_log`] gains a `create(true)`, or treats a missing
    /// path as an error.
    ///
    /// Both halves are the same decision seen from two sides. A log file
    /// that is not there is already empty, so `shep flush all` must not fail
    /// over the sheep in the flock that has never been started — and it must
    /// not leave a stray empty log behind either, which is what creating one
    /// would do at a path a rotator had just renamed away.
    #[tokio::test]
    async fn truncating_a_path_that_is_not_there_creates_nothing_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-started-out.log");

        assert_eq!(truncate_log(&missing).await, Ok(()));
        assert!(
            !missing.exists(),
            "a flush must not create the log file it did not find"
        );
    }

    /// Fails if [`truncate_log`]'s last arm swallows its error — a `_ =>
    /// Ok(())` beside the `NotFound` one, or a `NotFound` guard widened to
    /// every kind. `shep flush` would then exit 0 over a log holding
    /// everything it did before, which is the silent failure the verb's whole
    /// error path exists to end, and the neighbouring case above cannot see
    /// it: a missing path answers `Ok` legitimately.
    ///
    /// A directory in the log's place is the failure with no permission games
    /// in it, the same construction
    /// `a_reopen_that_cannot_open_a_path_again_answers_with_the_failure` uses
    /// one tier down: `open(2)` for writing on a directory fails for every
    /// uid, root included, so this cannot pass for the wrong reason on a
    /// privileged runner.
    #[tokio::test]
    async fn truncating_a_path_that_is_a_directory_reports_the_failure() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("web-out.log");
        std::fs::create_dir(&blocked).unwrap();

        let error = truncate_log(&blocked)
            .await
            .expect_err("a path that could not be truncated must not answer Ok");
        assert!(
            error
                .message
                .starts_with(&format!("{}: ", blocked.display())),
            "the failure must name the path it could not empty: {error}"
        );
    }

    /// Fails if [`truncate_log`] stops opening through
    /// [`open_log_path`] — drop the `O_NOFOLLOW` it adds (or open the path
    /// directly again) and `shep flush` empties whatever the symlink points
    /// at, with the daemon's privileges. That is the write-and-truncate
    /// primitive the flag exists to close, and no other case here can see it:
    /// every one of them truncates a real file, where following a symlink and
    /// not following one look identical.
    ///
    /// Three assertions because a fix could be wrong in three ways. The
    /// target's bytes prove nothing was emptied; the link still BEING a link
    /// proves the open did not replace it with a regular file; and the message
    /// proves an operator with a legitimately symlinked log path is told what
    /// happened rather than being handed `ELOOP`'s own "too many levels of
    /// symbolic links", which reads as a loop they do not have.
    ///
    /// `#[cfg(unix)]`: `O_NOFOLLOW` and `std::os::unix::fs::symlink` are both
    /// unix-only, and so is the refusal being asserted.
    #[cfg(unix)]
    #[tokio::test]
    async fn truncating_a_symlinked_log_path_refuses_and_leaves_its_target_alone() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("precious.txt");
        let link = dir.path().join("web-out.log");
        std::fs::write(&target, b"do not empty me").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = truncate_log(&link)
            .await
            .expect_err("a symlinked log path must not be truncated through");

        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"do not empty me",
            "the symlink's target must still hold every byte it did"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the refusal must leave the symlink itself in place, not replace it"
        );
        assert_eq!(
            error.message,
            format!("{}: {}", link.display(), crate::runner::SYMLINK_REFUSED),
            "the failure must name the path and say the word symlink: {error}"
        );
    }

    /// A spawn spec for the cases that drive [`run_sheep`] directly. The
    /// scripted fake reads none of it — it exists because
    /// [`ProcessRunner::spawn`] takes one.
    fn log_ctl_spec() -> SpawnSpec {
        SpawnSpec {
            name: "svc".to_string(),
            program: "./svc".to_string(),
            args: Vec::new(),
            cwd: None,
            env: std::collections::BTreeMap::new(),
            out_file: std::path::PathBuf::from("out.log"),
            err_file: std::path::PathBuf::from("err.log"),
            channel: false,
            credentials: None,
        }
    }

    // ---------------------------------------------------------------
    // IR-37: supervisor proptest (Task 9, Step 2). A command script (what
    // the operator does) and a process script (how each spawned child
    // behaves) are generated independently; their interleaving emerges from
    // the runtime itself instead of being hand-derived. Invariants are read
    // off successive `list()` snapshots and the event stream, never off
    // tick counts.
    //
    // `Shutdown` is deliberately NOT one of the five steps below: it is a
    // one-shot, engine-ending command (the actor's mailbox closes once it
    // resolves), which doesn't compose with "keep issuing more commands and
    // keep listing" the way Stop/Restart/Delete/Start do. Finding #2 (Delete
    // racing Shutdown, fixed above) is exactly the kind of bug this proptest
    // structurally cannot reach -- that's why it has its own targeted
    // regression test instead of being folded in here.
    //
    // More generally: this proptest's driver below is strictly sequential --
    // each step's command is fully `.await`ed (its deferred reply resolved)
    // before the next step runs -- so it can NEVER put two manual commands
    // in flight against the SAME sheep at once. Every manual-vs-manual race
    // this file guards against (`overlapping_stop_and_restart_agree_on_one_outcome`,
    // `delete_racing_shutdown_still_deregisters_the_sheep`,
    // `delete_racing_restart_still_deregisters_the_sheep`) needs a second
    // command issued via `tokio::spawn` while the first is still mid-flight,
    // which is exactly what this driver's "one command, fully awaited, then
    // the next" shape rules out. Don't read this proptest's invariants
    // holding as evidence that manual-vs-manual races are covered here --
    // they aren't, by construction; that coverage lives entirely in the
    // file's dedicated race tests.
    //
    // The lifecycle extras reach this state machine through exactly two
    // doors, and the driver uses both:
    //
    // - `extra_restart` is the door a memory breach and a liveness failure
    //   share (`Command::ExtraRestart` has no per-kind field, so one step
    //   models both). `Step::Report` raises one against the pid a sheep is
    //   running now; `Step::StaleReport` raises one against a pid it no
    //   longer has, which is the shape a report queued just before a
    //   crash-and-respawn takes.
    // - Readiness is not a step at all: it is a property of the app a
    //   `Step::StartOne` registers (`Gate` below), and it resolves on its own
    //   deadline somewhere inside whatever the driver does next. That is
    //   what puts a still-pending `ReadyResult` underneath an arbitrary
    //   later command instead of at a hand-picked instant.
    // ---------------------------------------------------------------

    #[derive(Debug, Clone, Copy)]
    enum Step {
        List,
        StopAll,
        RestartAll,
        DeleteFirst,
        StartOne,
        /// A memory breach or a liveness failure raised against the pid the
        /// first listed sheep is running right now.
        Report,
        /// The same report, raised against a pid that sheep does not have.
        StaleReport,
    }

    fn step_strategy() -> impl proptest::strategy::Strategy<Value = Step> {
        proptest::prop_oneof![
            proptest::strategy::Just(Step::List),
            proptest::strategy::Just(Step::StopAll),
            proptest::strategy::Just(Step::RestartAll),
            proptest::strategy::Just(Step::DeleteFirst),
            proptest::strategy::Just(Step::StartOne),
            proptest::strategy::Just(Step::Report),
            proptest::strategy::Just(Step::StaleReport),
        ]
    }

    /// How a generated app gates its own `starting -> online` transition.
    #[derive(Debug, Clone, Copy)]
    enum Gate {
        /// Neither `wait_ready` nor `readiness_probe`: `spawn_fresh` marks
        /// the sheep `Online` inline, the pre-readiness behaviour.
        Ungated,
        /// `wait_ready = true` with this `listen_timeout` in milliseconds.
        /// No scripted child ever writes `{"kind":"ready"}`, so every one of
        /// these waits ends at its deadline -- and the deadline is what
        /// decides whether a later step lands while the wait is still
        /// pending, which is the interleaving the epoch and status guards in
        /// `handle_ready_result` exist for.
        Channel(u64),
    }

    fn gate_strategy() -> impl proptest::strategy::Strategy<Value = Gate> {
        use proptest::strategy::Strategy as _; // `prop_map` below
        proptest::prop_oneof![
            2 => proptest::strategy::Just(Gate::Ungated),
            // Spread across the durations the driver's own commands take: a
            // kill ladder is 1600ms (`AppConfig::minimal`'s `kill_timeout`)
            // and a `stable_then_exit` script runs 2000ms, so a deadline
            // drawn from this range lands before, during and after them.
            1 => (1u64..4_000u64).prop_map(Gate::Channel),
        ]
    }

    fn script_strategy() -> impl proptest::strategy::Strategy<Value = ProcScript> {
        // Weighted toward long-lived children so a run explores command
        // handling rather than only exhausting the restart budget.
        proptest::prop_oneof![
            6 => proptest::strategy::Just(ProcScript::never_exits()),
            2 => proptest::strategy::Just(ProcScript::const_exit(1)),
            1 => proptest::strategy::Just(ProcScript::stable_then_exit(2_000, 0)),
            1 => proptest::strategy::Just(ProcScript::ignores_signals()),
        ]
    }

    /// How many scripted procs one generated case may spawn.
    ///
    /// Sized against the MAXIMUM the generator can demand, not against a
    /// correct run: an exhausted `ScriptedRunner` answers
    /// `SpawnFailed("script exhausted")`, the actor turns that into `Errored`
    /// rather than `Restart`, and every claim below about a restart that must
    /// NOT happen would then pass for the wrong reason. The ceiling a
    /// 9-command run can reach is: at most 30 command-driven spawns (`k`
    /// starts and `10 - k` `RestartAll` steps over `k` sheep peaks at 30 at
    /// `k = 5`), plus one per `Report`/`StaleReport` step, plus crash-loop
    /// respawns, which `max_restarts` caps at 16 per sheep -- 9 x 16 = 144.
    /// That is under 200 all told; 512 leaves room for a broken
    /// implementation to restart on every stale report and still be seen
    /// doing it.
    ///
    /// It is finite on purpose, and that is what makes the steady-state claim
    /// terminate: a `stable_then_exit` script resets the restart budget, so
    /// the pool running dry is the only thing that ends a chain of them.
    const SCRIPT_POOL: usize = 512;

    /// How long the steady-state drain waits for one more transition before
    /// concluding there are none left.
    ///
    /// Longer than every deadline a run can leave pending -- a 4000ms
    /// readiness wait, a 1600ms kill ladder, a 2000ms `stable_then_exit`
    /// script -- and far shorter than `fake::NEVER_MS` (30 days), so a
    /// `never_exits` proc stays alive across it instead of being walked to
    /// its own deadline.
    const QUIET_WINDOW: Duration = Duration::from_secs(60);

    /// Ceiling on transitions observed after the last command. Each spawn
    /// produces at most a start/restart, an online and a terminal event, so
    /// `3 * SCRIPT_POOL` bounds a correct run; anything past this ceiling is
    /// a flock that never settles.
    const EVENT_BUDGET: usize = 3 * SCRIPT_POOL;

    proptest::proptest! {
        // 128, not the 24 originally sketched for this task: an injected-bug
        // trial (a Delete on an already-terminal sheep that forgets to
        // deregister -- see the task report) minimizes to the 3-step
        // sequence `[StartOne, StopAll, DeleteFirst]`, which only 4 of the 5
        // equally-weighted `Step` variants touch, so a run needs a handful
        // of lucky draws to land it. Empirically that meant occasional
        // clean-yet-buggy runs at cases=24 (1 miss in 6 fresh-seed trials);
        // 128 caught the same injected bug in 8/8 fresh-seed trials, and the
        // whole run costs ~0.6s under the paused clock (0.2s of that predates
        // the steady-state drain below) -- cheap insurance against a property
        // test that only sometimes gates the regression it exists to catch.
        // `PROPTEST_CASES` still overrides it (IR-37) -- see
        // `testing::proptest_config`.
        #![proptest_config(crate::testing::proptest_config(128))]

        #[test]
        fn supervisor_upholds_its_invariants_under_any_interleaving(
            steps in proptest::collection::vec(step_strategy(), 1..10),
            gates in proptest::collection::vec(gate_strategy(), 1..10),
            scripts in proptest::collection::vec(script_strategy(), SCRIPT_POOL..SCRIPT_POOL + 1),
        ) {
            // A current-thread runtime with a paused clock inside the
            // proptest body: every backoff/kill-ladder/readiness delay is
            // virtual, so even a 128-case run stays cheap regardless of
            // which scripts and deadlines land.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            runtime.block_on(async move {
                // Capacity above `EVENT_BUDGET`: the drain below treats a
                // `Lagged` as a failure rather than skipping past it, since a
                // hole in the stream is a hole in every claim read off it.
                let (events, mut rx) = tokio::sync::broadcast::channel(8192);
                let handle = spawn_supervisor(
                    ScriptedRunner::new(scripts),
                    test_paths(&dir),
                    events,
                );
                let mut started = 0u32;
                let mut highest_restarts = std::collections::HashMap::<u32, u32>::new();
                // `extra_restart` is the one command with no reply, which
                // makes it the only one this driver cannot await -- and
                // therefore the only way a restart can still be
                // mid-kill-ladder when the NEXT step is issued. `Step::StopAll`
                // below keeps its strong claim across that interleaving
                // because an operator's command takes the `manual` marker off
                // an automatic restart (see `claim_manual`): without that
                // carve-out, a `stop` landing behind a report resolved to the
                // RESTART's outcome and handed its caller an `Online` snapshot
                // of a sheep that was genuinely back up.

                for step in steps {
                    match step {
                        Step::StartOne => {
                            let gate = gates[started as usize % gates.len()];
                            started += 1;
                            let mut app = AppConfig::minimal(&format!("sheep-{started}"), "./s");
                            if let Gate::Channel(ms) = gate {
                                app.wait_ready = true;
                                app.listen_timeout = UpDuration::from_millis(ms);
                            }
                            let _ = handle.start(vec![normalize(app).unwrap()]).await;
                        }
                        Step::StopAll => {
                            if let Ok(stopped) = handle.stop(ProcessSelector::All).await {
                                for info in stopped {
                                    // A deferred reply means every match is
                                    // terminal.
                                    proptest::prop_assert_eq!(info.status, ProcStatus::Stopped);
                                }
                            }
                        }
                        Step::RestartAll => {
                            let _ = handle.restart(ProcessSelector::All).await;
                        }
                        Step::DeleteFirst => {
                            if let Some(first) = handle.list().await.first() {
                                let id = first.id;
                                if let Ok(deleted) = handle.delete(ProcessSelector::Id(id)).await {
                                    proptest::prop_assert_eq!(deleted, vec![id]);
                                }
                                proptest::prop_assert!(
                                    handle.list().await.iter().all(|i| i.id != id)
                                );
                            }
                        }
                        Step::Report => {
                            if let Some(first) = handle.list().await.first()
                                && let Some(pid) = first.pid
                            {
                                handle.extra_restart(first.id, pid).await;
                            }
                        }
                        Step::StaleReport => {
                            if let Some(first) = handle.list().await.first() {
                                // Never this sheep's own pid, whatever it is
                                // running. A pid that belongs to some OTHER
                                // sheep would be just as stale here: the guard
                                // compares against THIS id's entry.
                                let stale = first.pid.unwrap_or(0).wrapping_add(1);
                                handle.extra_restart(first.id, stale).await;
                            }
                        }
                        Step::List => {}
                    }

                    let listed = handle.list().await;
                    // (1) ids are unique and the listing is sorted by id.
                    let ids: Vec<u32> = listed.iter().map(|i| i.id).collect();
                    let mut sorted = ids.clone();
                    sorted.sort_unstable();
                    sorted.dedup();
                    proptest::prop_assert_eq!(&ids, &sorted);
                    for info in &listed {
                        // (2) restart counts never decrease for a given id.
                        let seen = highest_restarts.entry(info.id).or_default();
                        proptest::prop_assert!(info.restarts >= *seen);
                        *seen = info.restarts;
                        // (3) no status outside the spec's set ever surfaces.
                        proptest::prop_assert!(matches!(
                            info.status,
                            ProcStatus::Starting | ProcStatus::Online | ProcStatus::Stopping
                                | ProcStatus::Stopped | ProcStatus::Errored | ProcStatus::WaitingRestart
                        ));
                    }
                }

                // (4) steady state: with no further commands, the flock stops
                // transitioning. Drained through a bounded window rather than
                // a bare `try_recv` (Global Constraints rule 11) precisely
                // because a run ends with deadlines still pending -- a
                // readiness wait, a kill ladder, a scripted proc's own exit.
                // A `try_recv` reads empty while all of them are still due and
                // so cannot fail; the window is what walks the paused clock
                // over them and gives the flock a chance to prove it settles.
                let mut observed = Vec::new();
                loop {
                    match tokio::time::timeout(QUIET_WINDOW, rx.recv()).await {
                        Ok(Ok(event)) => {
                            observed.push(event);
                            proptest::prop_assert!(
                                observed.len() <= EVENT_BUDGET,
                                "the flock never reached steady state: {} transitions after \
                                 the last command",
                                observed.len()
                            );
                        }
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                            return Err(proptest::test_runner::TestCaseError::fail(format!(
                                "event stream lagged by {skipped}: the invariants below cannot \
                                 be read off a stream with holes in it"
                            )));
                        }
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                        Err(_elapsed) => break, // nothing left to transition
                    }
                }

                // (5) never two live processes for one id, and never an
                // `Online` for an id with no live process. The first half is
                // the original claim: the stream must not show Start -> Start
                // without a terminal event between them. (Ids are never
                // reused by `spawn_fresh` today, so it is also a regression
                // guard against that invariant quietly changing.) The second
                // half is what readiness added: `handle_ready_result` resolves
                // long after the spawn it belongs to, so a wait that lost its
                // status guard would mark a sheep that has already exited,
                // stopped or errored `Online` -- a live pid on the bus for a
                // process that is not there.
                let mut live = std::collections::HashSet::<u32>::new();
                let mut event_restarts = std::collections::HashMap::<u32, u32>::new();
                for event in observed {
                    let BusEvent::Process { event, info, .. } = event else {
                        // LogOut/LogErr carry no lifecycle transition.
                        continue;
                    };
                    match event {
                        ProcessEventKind::Start => {
                            proptest::prop_assert!(
                                live.insert(info.id),
                                "two live spawns for id {}",
                                info.id
                            );
                        }
                        // A respawn replaces one live process with the next:
                        // the predecessor's `Msg::Exited` is what reached the
                        // actor to cause it, so this is one out and one in and
                        // the id is live either way afterwards.
                        ProcessEventKind::Restart => {
                            live.insert(info.id);
                        }
                        ProcessEventKind::Online => {
                            proptest::prop_assert!(
                                live.contains(&info.id),
                                "id {} was marked online with no live process: a readiness \
                                 wait resolved onto a sheep that had already gone terminal",
                                info.id
                            );
                        }
                        ProcessEventKind::Exit
                        | ProcessEventKind::Stop
                        | ProcessEventKind::Errored
                        | ProcessEventKind::Delete => {
                            live.remove(&info.id);
                        }
                        // `ProcessEventKind` is `#[non_exhaustive]` and lives
                        // in another crate, so E0004 will never fire here. A
                        // variant added later carries no liveness meaning
                        // until this match is taught one, and leaving `live`
                        // untouched is the conservative reading: it can only
                        // make the assertions above stricter, never weaker.
                        _ => {}
                    }
                    // (2) again, off the event stream rather than off
                    // `list()`: a snapshot only sees the counter between two
                    // commands, and the three new trigger paths all bump it
                    // in between.
                    let seen = event_restarts.entry(info.id).or_default();
                    proptest::prop_assert!(
                        info.restarts >= *seen,
                        "restart count for id {} went backwards: {} after {}",
                        info.id,
                        info.restarts,
                        *seen
                    );
                    *seen = info.restarts;
                }
                // The async block's error type is proptest's, so `?` above and
                // this tail agree; block_on hands the Result back to the
                // proptest body.
                Ok::<(), proptest::test_runner::TestCaseError>(())
            })?;
        }
    }
}
