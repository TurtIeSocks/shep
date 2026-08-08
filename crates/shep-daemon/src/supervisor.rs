//! The supervisor actor: owns the flock's lifecycle state machine.
//!
//! [`spawn_supervisor`] starts the actor task and hands back a
//! [`SupervisorHandle`]. Every registered instance ("sheep") additionally
//! gets its own per-sheep task, spawned by the actor, that owns the live
//! `(proc, ProcIo)` pair for that instance's whole lifetime and forwards its
//! logs/shepherd-channel traffic — the actor itself never touches a live
//! process directly, only [`RunningProcess`] handles held by those tasks and
//! a fire-and-forget control sender per sheep.
//!
//! # One exit path
//!
//! Every sheep, however it ends — a natural exit or a kill request — reaches
//! the actor as exactly one `Msg::Exited`. The actor's map never holds a
//! `proc`; it holds one lifecycle entry plus a control sender per id, so the
//! actor loop never awaits process I/O and never blocks.
//!
//! # Deferred, aggregated replies
//!
//! [`SupervisorHandle::stop`]/[`restart`](SupervisorHandle::restart)/
//! [`delete`](SupervisorHandle::delete) and
//! [`shutdown`](SupervisorHandle::shutdown) resolve their selector into a
//! set of matched ids up front, then wait until every matched sheep is
//! terminal before answering the caller. Automatic restarts internal to the
//! crash loop go through the same exit path without ever registering a
//! deferred reply.

use core::fmt;
use core::time::Duration;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

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
use crate::kill::kill_process;
use crate::runner::{ExitOutcome, ProcIo, ProcessRunner, RunningProcess};

/// Capacity of the actor's own mailbox (commands + internal events).
const MAILBOX_CAPACITY: usize = 256;

/// Capacity of one sheep task's control mailbox — at most one live `Kill` is
/// ever in flight, so this stays small on purpose.
const SHEEP_CTL_CAPACITY: usize = 4;

// ---------------------------------------------------------------------
// Public command / handle surface
// ---------------------------------------------------------------------

/// Commands the supervisor actor accepts (wrapped in `Msg::Command`).
#[derive(Debug)]
pub enum Command {
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
        /// Answers once every matched sheep is back online (or errored).
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
    /// Graceful engine shutdown: kill ladder on every online sheep, then stop.
    Shutdown {
        /// Answers once every online sheep is terminal, right before the
        /// actor returns.
        reply: oneshot::Sender<()>,
    },
}

/// The actor's mailbox message: public [`Command`]s plus events the actor
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
    },
    /// The sheep's shepherd channel reported readiness.
    ///
    /// Drained and logged in Phase 2a — the `wait_ready` gate that consumes
    /// this lands in Phase 4.
    Ready {
        /// The sheep's id.
        id: u32,
    },
}

/// Error type returned from supervisor commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    /// The selector matched no registered sheep.
    NotFound,
    /// Spawn failed (carries the runner's message).
    SpawnFailed(String),
    /// The actor has shut down; its mailbox is closed.
    EngineStopped,
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("selector matched no registered sheep"),
            Self::SpawnFailed(msg) => write!(f, "spawn failed: {msg}"),
            Self::EngineStopped => f.write_str("supervisor engine has shut down"),
        }
    }
}

impl core::error::Error for SupervisorError {}

/// Handle to a running supervisor actor.
///
/// Cloning shares the same actor; every clone's commands are serialized
/// through its single mailbox.
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
    pub async fn stop(
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
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub async fn restart(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Restart { selector, reply }))
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
    pub async fn delete(&self, selector: ProcessSelector) -> Result<Vec<u32>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Delete { selector, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Full flock listing, id-sorted.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub async fn list_checked(&self) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::List { reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)
    }

    /// Full flock listing, id-sorted.
    ///
    /// Convenience over [`Self::list_checked`] for callers that don't need
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

/// Spawns the actor; `events` receives [`BusEvent::Process`] (+
/// `LogOut`/`LogErr` forwarded from each sheep's `ProcIo::logs`) — Phase 2b
/// plugs its bus straight in.
///
/// Must be called from within a Tokio runtime context: it spawns the actor
/// task immediately.
pub fn spawn_supervisor<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    events: broadcast::Sender<BusEvent>,
) -> SupervisorHandle {
    let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
    let actor = Actor {
        runner,
        paths,
        events,
        tx: tx.clone(),
        sheep: HashMap::new(),
        next_id: 0,
        pending: Vec::new(),
    };
    tokio::spawn(actor.run(rx));
    SupervisorHandle { tx }
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
    Kill,
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

/// One registered instance: its lifecycle state plus a live control sender
/// (`None` once its sheep task has ended).
#[derive(Debug)]
struct SheepSlot {
    /// Lifecycle state (spec, status, restart budget, ...).
    entry: ProcessEntry,
    /// Sender for this sheep's control mailbox; `None` when not running.
    ctl: Option<mpsc::Sender<SheepCtl>>,
    /// Which manual command (if any) is waiting on this sheep's next exit.
    manual: Option<ManualKind>,
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
}

impl<R: ProcessRunner> Actor<R> {
    /// Runs the actor to completion: processes every mailbox message until
    /// a `Shutdown` fully resolves, then returns (dropping `rx` closes the
    /// mailbox, so subsequent [`SupervisorHandle`] calls see
    /// [`SupervisorError::EngineStopped`]).
    async fn run(mut self, mut rx: mpsc::Receiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            let should_break = match msg {
                Msg::Command(cmd) => self.handle_command(cmd).await,
                Msg::Exited { id, outcome } => self.handle_exited(id, outcome),
                Msg::RestartDue { id } => {
                    self.handle_restart_due(id);
                    false
                }
                Msg::Ready { id } => {
                    tracing::debug!(id, "shepherd-channel ready (wait_ready gating is Phase 4)");
                    false
                }
            };
            if should_break {
                break;
            }
        }
    }

    async fn handle_command(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::Start { apps, reply } => {
                let result = self.do_start(apps);
                let _ = reply.send(result);
                false
            }
            Command::List { reply } => {
                let _ = reply.send(self.snapshot_all());
                false
            }
            Command::Stop { selector, reply } => {
                self.begin_manual(selector, ManualKind::Stop, ReplyKind::Info(reply))
                    .await;
                false
            }
            Command::Restart { selector, reply } => {
                self.begin_manual(selector, ManualKind::Restart, ReplyKind::Info(reply))
                    .await;
                false
            }
            Command::Delete { selector, reply } => {
                self.begin_manual(selector, ManualKind::Delete, ReplyKind::Ids(reply))
                    .await;
                false
            }
            Command::Shutdown { reply } => self.begin_shutdown(reply).await,
        }
    }

    /// Expands each app through `instance_slots` + `assemble`, spawning one
    /// instance per slot. Already-registered entries persist even when a
    /// later spawn in the batch fails.
    fn do_start(&mut self, apps: Vec<ResolvedApp>) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let mut results = Vec::new();
        for app in apps {
            let name = app.config().name.clone();
            let mut existing: Vec<u32> = self
                .sheep
                .values()
                .filter(|slot| slot.entry.spec.config().name == name)
                .map(|slot| slot.entry.instance)
                .collect();
            existing.sort_unstable();
            let slots = instance_slots(&existing, app.config().instances);

            for instance in slots {
                match self.spawn_fresh(&app, instance) {
                    Ok(info) => results.push(info),
                    Err(message) => return Err(SupervisorError::SpawnFailed(message)),
                }
            }
        }
        Ok(results)
    }

    /// Registers + spawns one brand-new instance (a fresh id, `restarts: 0`).
    ///
    /// Always inserts a [`SheepSlot`] — `Online` with a live sheep task on
    /// success, `Errored` with no task on failure — before returning, so the
    /// entry persists regardless of the outcome.
    fn spawn_fresh(&mut self, app: &ResolvedApp, instance: u32) -> Result<ProcessInfo, String> {
        let spec = assemble(app, instance, &self.paths);
        let id = self.next_id;
        self.next_id += 1;

        match self.runner.spawn(&spec) {
            Ok((proc, io)) => {
                let pid = proc.pid();
                let entry = ProcessEntry {
                    id,
                    spec: app.clone(),
                    instance,
                    status: ProcStatus::Online,
                    pid: Some(pid),
                    restarts: 0,
                    started_at: Some(tokio::time::Instant::now()),
                    budget: RestartBudget::default(),
                    reload: ReloadState::None,
                };
                let info = to_info(&entry);
                let ctl = spawn_sheep_task::<R::Proc>(
                    id,
                    proc,
                    io,
                    app.clone(),
                    self.events.clone(),
                    self.tx.clone(),
                );
                self.sheep.insert(
                    id,
                    SheepSlot {
                        entry,
                        ctl: Some(ctl),
                        manual: None,
                    },
                );
                self.emit(ProcessEventKind::Start, info.clone(), true);
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
                };
                let info = to_info(&entry);
                self.sheep.insert(
                    id,
                    SheepSlot {
                        entry,
                        ctl: None,
                        manual: None,
                    },
                );
                self.emit(ProcessEventKind::Errored, info, true);
                Err(error.to_string())
            }
        }
    }

    /// Respawns an already-registered id in place: reassembles from its
    /// stored spec + instance, bumps `restarts` and resets timing on
    /// success, or marks the entry `Errored` on failure. Used for both
    /// automatic (`RestartDue`) and manual (forced) respawns.
    fn respawn(&mut self, id: u32, manually: bool) -> ProcessInfo {
        let slot = self.sheep.get(&id).expect("respawn: unknown id");
        let app = slot.entry.spec.clone();
        let instance = slot.entry.instance;
        let spec = assemble(&app, instance, &self.paths);

        match self.runner.spawn(&spec) {
            Ok((proc, io)) => {
                let pid = proc.pid();
                let ctl = spawn_sheep_task::<R::Proc>(
                    id,
                    proc,
                    io,
                    app,
                    self.events.clone(),
                    self.tx.clone(),
                );
                let slot = self
                    .sheep
                    .get_mut(&id)
                    .expect("respawn: entry vanished mid-respawn");
                slot.entry.status = ProcStatus::Online;
                slot.entry.pid = Some(pid);
                slot.entry.started_at = Some(tokio::time::Instant::now());
                slot.entry.restarts += 1;
                slot.ctl = Some(ctl);
                let info = to_info(&slot.entry);
                self.emit(ProcessEventKind::Restart, info.clone(), manually);
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
                let info = to_info(&slot.entry);
                self.emit(ProcessEventKind::Errored, info.clone(), manually);
                info
            }
        }
    }

    /// Resolves `selector`, then either defers to each matched sheep's next
    /// exit (if running) or applies the command immediately (if not).
    async fn begin_manual(
        &mut self,
        selector: ProcessSelector,
        kind: ManualKind,
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
            let is_running = self.sheep.get(&id).is_some_and(|slot| slot.ctl.is_some());
            if is_running {
                let ctl = {
                    let slot = self.sheep.get_mut(&id).expect("checked is_running above");
                    slot.manual = Some(kind);
                    slot.ctl.clone()
                };
                if let Some(ctl) = ctl {
                    let _ = ctl.send(SheepCtl::Kill).await;
                }
                remaining.insert(id);
            } else if let Some(info) = self.apply_immediate(id, kind) {
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
    fn apply_immediate(&mut self, id: u32, kind: ManualKind) -> Option<ProcessInfo> {
        match kind {
            ManualKind::Stop => {
                let slot = self.sheep.get_mut(&id)?;
                if slot.entry.status == ProcStatus::WaitingRestart {
                    // Cancels the pending restart: `handle_restart_due` only
                    // respawns an id still in `WaitingRestart`.
                    slot.entry.status = ProcStatus::Stopped;
                    let info = to_info(&slot.entry);
                    self.emit(ProcessEventKind::Stop, info.clone(), true);
                    Some(info)
                } else {
                    Some(to_info(&slot.entry))
                }
            }
            ManualKind::Delete => {
                let slot = self.sheep.remove(&id)?;
                let info = to_info(&slot.entry);
                self.emit(ProcessEventKind::Delete, info.clone(), true);
                Some(info)
            }
            ManualKind::Restart => {
                self.sheep.get_mut(&id)?.entry.budget.reset();
                Some(self.respawn(id, true))
            }
        }
    }

    /// Kills every currently-online sheep, deferring the reply the same way
    /// `Stop` does; returns `true` (break the actor loop) once there was
    /// nothing to wait on, or once the deferred reply's own resolution says
    /// so (propagated back from [`Self::handle_exited`]).
    async fn begin_shutdown(&mut self, reply: oneshot::Sender<()>) -> bool {
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
            let ctl = {
                let slot = self.sheep.get_mut(&id).expect("checked online above");
                slot.manual = Some(ManualKind::Stop);
                slot.ctl.clone()
            };
            if let Some(ctl) = ctl {
                let _ = ctl.send(SheepCtl::Kill).await;
            }
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
        let manual = slot.manual.take();
        let Some(started_at) = slot.entry.started_at.take() else {
            tracing::warn!(
                id,
                "Msg::Exited for an entry with no started_at (duplicate?)"
            );
            return false;
        };
        let uptime = tokio::time::Instant::now().saturating_duration_since(started_at);

        // A manual Restart always forces a respawn (spec §4: manual action
        // resets budget), regardless of what `decide_on_exit` would say —
        // `manual_stop = true` would otherwise make it choose CleanStop.
        if manual == Some(ManualKind::Restart) {
            slot.entry.budget.reset();
            let info = self.respawn(id, true);
            return self.resolve_pending(id, info);
        }

        let decision = {
            let slot = self.sheep.get_mut(&id).expect("checked above");
            decide_on_exit(
                slot.entry.spec.config(),
                &mut slot.entry.budget,
                uptime,
                outcome,
                manual.is_some(),
            )
        };

        let info = match decision {
            Decision::Restart { delay } => {
                let info = self.set_status(id, ProcStatus::WaitingRestart);
                self.emit(ProcessEventKind::Exit, info.clone(), false);
                self.schedule_restart(id, delay);
                info
            }
            Decision::Errored => {
                let info = self.set_status(id, ProcStatus::Errored);
                self.emit(ProcessEventKind::Errored, info.clone(), manual.is_some());
                info
            }
            Decision::CleanStop if manual == Some(ManualKind::Delete) => {
                let mut removed = self.sheep.remove(&id).expect("checked above");
                removed.entry.status = ProcStatus::Stopped;
                let info = to_info(&removed.entry);
                self.emit(ProcessEventKind::Delete, info.clone(), true);
                info
            }
            Decision::CleanStop => {
                let info = self.set_status(id, ProcStatus::Stopped);
                self.emit(
                    ProcessEventKind::Stop,
                    info.clone(),
                    manual == Some(ManualKind::Stop),
                );
                info
            }
        };

        self.resolve_pending(id, info)
    }

    /// A scheduled restart's backoff elapsed. Guarded on the entry still
    /// being `WaitingRestart`: a manual command may have intercepted it
    /// (see `apply_immediate`'s `Stop` case), making this a stale timer.
    fn handle_restart_due(&mut self, id: u32) {
        let Some(slot) = self.sheep.get(&id) else {
            return;
        };
        if slot.entry.status != ProcStatus::WaitingRestart {
            return;
        }
        self.respawn(id, false);
    }

    /// Spawns the backoff timer for a scheduled restart; `None` still hops
    /// through a task + mailbox send rather than respawning inline, so
    /// "immediate" restarts remain observable as a distinct scheduling step.
    fn schedule_restart(&self, id: u32, delay: Option<Duration>) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let _ = tx.send(Msg::RestartDue { id }).await;
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
            at_ms: now_ms(),
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
    }
}

/// The one real-time read in the supervisor: wall-clock milliseconds since
/// the Unix epoch, for [`BusEvent::Process::at_ms`]. Everything else in the
/// actor uses the paused-clock-aware `tokio::time::Instant`.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
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
    let ProcIo {
        mut logs,
        mut from_child,
        to_child,
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
                    Some(SheepCtl::Kill) => {
                        let outcome = kill_process(&mut proc, app.config(), Some(&to_child)).await;
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
                        tracing::debug!(id, name, value, "child metric (full handling is Phase 4)");
                    }
                    Some(ChildMessage::ActionReply { action, body }) => {
                        tracing::debug!(id, action, body, "child action reply (full handling is Phase 4)");
                    }
                    None => from_child_open = false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use shep_core::config::{AppConfig, normalize};
    use shep_core::status::ProcStatus;

    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};

    // WHY: an isolated $SHEP_HOME per test, resolved over a real tempdir so
    // `assemble`'s log-path computation has somewhere plausible to point —
    // nothing in these tests actually touches the filesystem (the scripted
    // runner never opens the log files), so leaking the tempdir (no cleanup
    // guard kept alive) is fine for test-process-lifetime isolation.
    fn test_paths() -> ShepPaths {
        let dir = tempfile::tempdir().expect("tempdir");
        // `into_path()` (not `keep()`) deliberately: `keep()` isn't available
        // on the workspace's tempfile floor ("3", i.e. 3.0.2 under
        // -Z minimal-versions), only added in a later 3.x release.
        #[allow(deprecated, reason = "keep() postdates the workspace's tempfile floor")]
        let path = dir.into_path();
        ShepPaths::resolve(&|_| None, &path)
    }

    // Drives virtual time by parking on recv(); returns when the id reaches `kind`.
    async fn await_event(
        rx: &mut tokio::sync::broadcast::Receiver<BusEvent>,
        id: u32,
        kind: ProcessEventKind,
    ) {
        loop {
            match rx.recv().await {
                Ok(BusEvent::Process { event, info, .. }) if info.id == id && event == kind => {
                    return;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("event stream closed before {kind:?} for id {id}: {e}"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn start_lists_online_instances() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let handle = spawn_supervisor(runner, test_paths(), events);
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
    async fn crash_loop_erroreds_after_budget_with_pinned_delays() {
        let (events, mut rx) = tokio::sync::broadcast::channel(1024);
        // 16 spawns: initial + 15 restarts; every exit instant (unstable).
        let runner = ScriptedRunner::new((0..16).map(|_| ProcScript::const_exit(1)).collect());
        let handle = spawn_supervisor(runner, test_paths(), events);
        let mut app = AppConfig::minimal("crash", "./boom");
        app.exp_backoff_restart_delay = Some("100".parse().unwrap());
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Park on the event stream; auto-advance drives through all pending
        // backoff delays (pinned Task 4 sequence) until the script runs out.
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        assert_eq!(list[0].restarts, 15); // respawns performed, not exits
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
        let handle = spawn_supervisor(runner, test_paths(), events);
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
        }]);
        let handle = spawn_supervisor(runner, test_paths(), events);
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
        let handle = spawn_supervisor(runner, test_paths(), events);
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
        let handle = spawn_supervisor(runner, test_paths(), events);
        let app = AppConfig::minimal("ghost", "./missing");
        let err = handle
            .start(vec![normalize(app).unwrap()])
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::SpawnFailed(_)));
        assert_eq!(handle.list().await[0].status, ProcStatus::Errored);
    }

    #[tokio::test(start_paused = true)]
    async fn delete_and_selectors_route() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let handle = spawn_supervisor(runner, test_paths(), events);
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

    #[tokio::test(start_paused = true)]
    async fn manual_restart_resets_budget_and_respawns() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        // Two unstable crashes bring the budget to 2; then a manual restart must
        // reset it (spec §4) and respawn. Script needs FOUR procs: initial +
        // 2 crash-respawns landing on the long-lived third, + the respawn the
        // manual restart itself performs.
        let runner = ScriptedRunner::new(vec![
            ProcScript::const_exit(1),
            ProcScript::const_exit(1),
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]);
        let handle = spawn_supervisor(runner, test_paths(), events);
        let app = AppConfig::minimal("svc", "./svc");
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
        assert_eq!(restarted[0].status, ProcStatus::Online);
        // Budget reset by the manual action: online, not errored.
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
        assert_eq!(handle.list().await[0].restarts, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_kills_all_and_stops_the_engine() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let handle = spawn_supervisor(runner, test_paths(), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        handle.shutdown().await; // kill ladder on every online sheep, then stop
        // After shutdown the handle's channel is closed; further commands error.
        assert!(handle.list_checked().await.is_err());
    }
}
