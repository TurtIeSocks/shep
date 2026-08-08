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
use crate::privilege::{self, Credentials};
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
        /// The sheep's `SheepSlot::epoch` at scheduling time. Ignored if it
        /// no longer matches the slot's current epoch — a stale timer left
        /// behind by a respawn that happened in the meantime (IMPORTANT-3).
        epoch: u64,
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
        shutting_down: false,
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
                    tracing::debug!(id, "shepherd-channel ready (wait_ready gating is Phase 4)");
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
            Command::Stop { selector, reply } => {
                self.begin_manual(selector, ManualKind::Stop, ReplyKind::Info(reply));
                false
            }
            // CRITICAL-1: Restart is rejected outright once shutdown has
            // begun, for the same reason as Start — its forced respawn
            // (handle_exited's manual-Restart branch, or apply_immediate's)
            // would spawn a child outside the shutdown aggregation.
            Command::Restart { selector, reply } => {
                if self.shutting_down {
                    send_reply(ReplyKind::Info(reply), Err(SupervisorError::EngineStopped));
                } else {
                    self.begin_manual(selector, ManualKind::Restart, ReplyKind::Info(reply));
                }
                false
            }
            Command::Delete { selector, reply } => {
                self.begin_manual(selector, ManualKind::Delete, ReplyKind::Ids(reply));
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
    /// Always inserts a [`SheepSlot`] — `Online` with a live sheep task on
    /// success, `Errored` with no task on failure — before returning, so the
    /// entry persists regardless of the outcome.
    fn spawn_fresh(
        &mut self,
        app: &ResolvedApp,
        instance: u32,
        credentials: Option<Credentials>,
    ) -> Result<ProcessInfo, String> {
        let spec = assemble(app, instance, &self.paths, credentials);
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
                    credentials,
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
                        pending_delete: false,
                        epoch: 0,
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
                    credentials,
                };
                let info = to_info(&entry);
                self.sheep.insert(
                    id,
                    SheepSlot {
                        entry,
                        ctl: None,
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
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
        // Reused as-is from the initial Start (never re-resolved): a
        // restart must never re-touch the passwd database, and must never
        // silently change identity out from under an already-running app.
        let credentials = slot.entry.credentials;
        let spec = assemble(&app, instance, &self.paths, credentials);

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
                // IMPORTANT-3: a new process now exists for this id — any
                // RestartDue timer scheduled before this point (targeting
                // the process this replaced) is stale the moment it fires.
                slot.epoch += 1;
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
    ///
    /// Mixed selectors (some matched ids already terminal, some still
    /// running) are handled uniformly: immediate results are collected into
    /// `results` up front and folded into the SAME `PendingReply` as the
    /// deferred ids, so the reply carries every match and only fires once
    /// the last running one goes terminal too (confirmed by probe C's
    /// equivalent scenario — no code change needed there, this comment is
    /// the "why it already works").
    fn begin_manual(&mut self, selector: ProcessSelector, kind: ManualKind, reply: ReplyKind) {
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
                // IMPORTANT-4: chosen semantics — the FIRST manual command
                // to reach a running sheep owns its `manual` marker and its
                // one live Kill; a later command racing in against the SAME
                // in-flight kill (this sheep already has `manual.is_some()`)
                // does not overwrite that marker or send a second Kill — it
                // just joins `remaining` and rides the same eventual
                // Msg::Exited to whatever terminal state the FIRST command's
                // intent produces. A `stop()` that lands first still wins
                // over a racing `restart()`, and vice versa; both callers
                // get the SAME honest terminal snapshot instead of one of
                // them being lied to (the old last-writer-wins bug handed a
                // `stop()` caller back an `Online` `ProcessInfo`).
                let already_in_flight = self
                    .sheep
                    .get(&id)
                    .is_some_and(|slot| slot.manual.is_some());
                if !already_in_flight {
                    let slot = self.sheep.get_mut(&id).expect("checked is_running above");
                    slot.manual = Some(kind);
                    // CRITICAL-2: try_send, never `.await`. The sheep task
                    // stops draining its ctl mailbox the moment it starts
                    // the kill ladder, so a blocking send here could park
                    // the actor for up to `kill_timeout` — or, with the
                    // mailbox-full tail (a flood of commands after this
                    // one), deadlock forever (actor parked in `ctl.send()`,
                    // sheep parked in `actor_tx.send()`, neither drains the
                    // other). `Full`/`Closed` are both fine to ignore: a
                    // Kill already queued means the ladder is already
                    // running (a second would be redundant); `Closed` means
                    // the sheep already exited and its own `Msg::Exited` is
                    // already in flight (or about to be).
                    if let Some(ctl) = &slot.ctl {
                        let _ = ctl.try_send(SheepCtl::Kill);
                    }
                }
                if kind == ManualKind::Delete {
                    // Regardless of which command's `manual` marker won,
                    // this id must still be deregistered once it goes
                    // terminal — see the SheepSlot::pending_delete doc.
                    if let Some(slot) = self.sheep.get_mut(&id) {
                        slot.pending_delete = true;
                    }
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
                        self.emit(ProcessEventKind::Stop, info.clone(), true);
                        Some(info)
                    }
                    _ => Some(to_info(&slot.entry)),
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
    ///
    /// CRITICAL-1: sets `shutting_down` FIRST, before computing `online` —
    /// every check against it downstream (`Start`/`Restart` rejection,
    /// `handle_restart_due`'s guard) is only meaningful if no new sheep can
    /// register and no `WaitingRestart` sheep can respawn from this point
    /// on, so nothing outside this snapshot of `online` can ever need
    /// killing later.
    fn begin_shutdown(&mut self, reply: oneshot::Sender<()>) -> bool {
        self.shutting_down = true;

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
            // Same first-command-wins dedupe as `begin_manual` (IMPORTANT-4):
            // an id already mid-kill from an earlier Stop/Restart/Delete
            // keeps that command's `manual` marker and doesn't get a
            // redundant Kill — it still joins `remaining` below either way.
            let already_in_flight = self
                .sheep
                .get(&id)
                .is_some_and(|slot| slot.manual.is_some());
            if !already_in_flight {
                let slot = self.sheep.get_mut(&id).expect("checked online above");
                slot.manual = Some(ManualKind::Stop);
                // CRITICAL-2: try_send — see `begin_manual` for why this
                // must never be a blocking `.await`.
                if let Some(ctl) = &slot.ctl {
                    let _ = ctl.try_send(SheepCtl::Kill);
                }
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
        let pending_delete = std::mem::take(&mut slot.pending_delete);
        let Some(started_at) = slot.entry.started_at.take() else {
            // MINOR-7: shouldn't happen (a duplicate Msg::Exited would
            // violate the one-exit-path invariant) — but resolve any
            // pending reply waiting on this id with a best-effort snapshot
            // instead of leaving its caller parked on `.await` forever.
            tracing::warn!(
                id,
                "Msg::Exited for an entry with no started_at (duplicate?)"
            );
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
        // `manual.is_some()` true, `decide_on_exit` always resolves to
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
        // `decide_on_exit` resolve to CleanStop (manual.is_some() is still
        // true) and the `pending_delete` guard below correctly deregister.
        if manual == Some(ManualKind::Restart) && !self.shutting_down && !pending_delete {
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
                // IMPORTANT-3: capture the CURRENT epoch so the timer this
                // schedules can tell, when it fires, whether it's still the
                // authoritative one for this id (see `respawn`/`SheepSlot`).
                let epoch = self.sheep.get(&id).expect("checked above").epoch;
                self.schedule_restart(id, epoch, delay);
                info
            }
            Decision::Errored => {
                let info = self.set_status(id, ProcStatus::Errored);
                self.emit(ProcessEventKind::Errored, info.clone(), manual.is_some());
                info
            }
            Decision::CleanStop if manual == Some(ManualKind::Delete) || pending_delete => {
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

    /// A scheduled restart's backoff elapsed. Guarded on:
    ///
    /// 1. NOT shutting down (CRITICAL-1) — a graceful shutdown in progress
    ///    forbids any new spawn, full stop; nothing here would be part of
    ///    the shutdown aggregation's `online` snapshot, so it would leak.
    /// 2. The entry still being `WaitingRestart` — a manual command may
    ///    have intercepted it (see `apply_immediate`'s `Stop` case),
    ///    making this a stale timer.
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
    use crate::testing::test_paths; // the one crate-root fixture (IR-33)

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
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
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
    // sheep. Chosen semantics (see `begin_manual`'s doc comment): the first
    // command to reach a running sheep owns its `manual` marker and its one
    // live Kill; both callers get back the SAME honest terminal snapshot
    // once it lands, instead of the old last-writer-wins bug handing the
    // `stop()` caller an `Online` `ProcessInfo`.
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
    // `begin_manual`'s `already_in_flight` branch and only join `remaining`
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
    // `Delete(id)` hits `already_in_flight`, correctly sets
    // `pending_delete = true`, but never touches `manual`. Worse than the
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
    // ---------------------------------------------------------------

    #[derive(Debug, Clone, Copy)]
    enum Step {
        List,
        StopAll,
        RestartAll,
        DeleteFirst,
        StartOne,
    }

    fn step_strategy() -> impl proptest::strategy::Strategy<Value = Step> {
        proptest::prop_oneof![
            proptest::strategy::Just(Step::List),
            proptest::strategy::Just(Step::StopAll),
            proptest::strategy::Just(Step::RestartAll),
            proptest::strategy::Just(Step::DeleteFirst),
            proptest::strategy::Just(Step::StartOne),
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

    proptest::proptest! {
        // 128, not the 24 originally sketched for this task: an injected-bug
        // trial (a Delete on an already-terminal sheep that forgets to
        // deregister -- see the task report) minimizes to the 3-step
        // sequence `[StartOne, StopAll, DeleteFirst]`, which only 4 of the 5
        // equally-weighted `Step` variants touch, so a run needs a handful
        // of lucky draws to land it. Empirically that meant occasional
        // clean-yet-buggy runs at cases=24 (1 miss in 6 fresh-seed trials);
        // 128 caught the same injected bug in 8/8 fresh-seed trials, still
        // in ~0.1-0.3s under the paused clock -- cheap insurance against a
        // property test that only sometimes gates the regression it exists
        // to catch.
        #![proptest_config(proptest::test_runner::Config {
            cases: 128,
            ..proptest::test_runner::Config::default()
        })]

        #[test]
        fn supervisor_upholds_its_invariants_under_any_interleaving(
            steps in proptest::collection::vec(step_strategy(), 1..10),
            scripts in proptest::collection::vec(script_strategy(), 128..129),
        ) {
            // A current-thread runtime with a paused clock inside the
            // proptest body: every backoff/kill-ladder delay is virtual, so
            // even a 128-case run stays well under a second regardless of
            // which scripts land.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            runtime.block_on(async move {
                let (events, mut rx) = tokio::sync::broadcast::channel(4096);
                let handle = spawn_supervisor(
                    ScriptedRunner::new(scripts),
                    test_paths(&dir),
                    events,
                );
                let mut started = 0u32;
                let mut highest_restarts = std::collections::HashMap::<u32, u32>::new();

                for step in steps {
                    match step {
                        Step::StartOne => {
                            started += 1;
                            let app = AppConfig::minimal(&format!("sheep-{started}"), "./s");
                            let _ = handle.start(vec![normalize(app).unwrap()]).await;
                        }
                        Step::StopAll => {
                            if let Ok(stopped) = handle.stop(ProcessSelector::All).await {
                                // A deferred reply means every match is terminal.
                                for info in stopped {
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

                // (4) never two live processes for one id: the event stream
                // must never show Start -> Start for an id without a
                // terminal event between them. (Ids are never reused by
                // `spawn_fresh` today, so this is also a regression guard
                // against that invariant quietly changing later.)
                let mut live = std::collections::HashSet::<u32>::new();
                while let Ok(event) = rx.try_recv() {
                    if let BusEvent::Process { event, info, .. } = event {
                        match event {
                            ProcessEventKind::Start => {
                                proptest::prop_assert!(
                                    live.insert(info.id),
                                    "two live spawns for id {}",
                                    info.id
                                );
                            }
                            ProcessEventKind::Exit
                            | ProcessEventKind::Stop
                            | ProcessEventKind::Errored
                            | ProcessEventKind::Delete => {
                                live.remove(&info.id);
                            }
                            _ => {}
                        }
                    }
                }
                // The async block's error type is proptest's, so `?` above and
                // this tail agree; block_on hands the Result back to the
                // proptest body.
                Ok::<(), proptest::test_runner::TestCaseError>(())
            })?;
        }
    }
}
