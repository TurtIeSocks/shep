//! The supervisor actor: owns the flock's lifecycle state machine.
//!
//! [`spawn_supervisor`] starts the actor task and hands back a
//! [`SupervisorHandle`]. Every registered instance ("sheep") gets its own task
//! owning the live `(proc, ProcIo)` pair; the actor holds one lifecycle entry
//! plus two senders per id, so the loop never awaits process I/O. However a
//! sheep ends, it reaches the actor as exactly one `Msg::Exited`.
//!
//! `stop`/`restart`/`delete` and `shutdown` resolve their selector up front,
//! then answer once every matched sheep is terminal.

use core::cmp::Ordering;
use core::fmt;
use core::sync::atomic::{self, AtomicU64};
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use shep_core::config::{
    AppConfig, ApplyGroup, DeclaredApp, ResetDepth, ResolvedApp, apply_group, normalize,
};
use shep_core::overrides::{self, AppOverrides};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{
    ActionOutcome, ActionReply, BusEvent, DogSource, EnvValue, ExitInfo, LineOutcome, LineReply,
    ProcessEventKind, ProcessInfo, SheepConfigView, SheepDrift, SignalOutcome, SignalReply, Smit,
    sort_flock,
};
use shep_core::selector::ProcessSelector;
use shep_core::signals::OperatorSignal;
use shep_core::status::ProcStatus;
use shep_core::values::MemSize;

use crate::assemble::{assemble, instance_slots};
use crate::brain::{Decision, decide_on_exit};
use crate::bus::{Bus, SharedEvent};
use crate::channel::{ChildMessage, ShepherdMessage};
use crate::entry::{ProcessEntry, ReloadState, RestartBudget};
use crate::extras::{Extras, ExtrasRegistry};
#[cfg(unix)]
use crate::handover::adopt::AdoptedSheep;
#[cfg(unix)]
use crate::handover::reap::AdoptedReaper;
#[cfg(unix)]
use crate::handover::{
    Candidate, CarriedFds, CarriedSheep, Counters, DaemonFds, Fitness, Handover, OwnedCandidate,
    fitness,
};
use crate::kill::kill_process;
use crate::privilege::{self, Credentials, PrivilegeError, SpawnIdentity};
use crate::probes::Prober;
use crate::probes::os::OsProber;
use crate::probes::ready::{Readiness, ReadinessSource, await_ready};
#[cfg(unix)]
use crate::runner::AdoptSpec;
use crate::runner::{
    ExitOutcome, FlushError, LogCtl, Preflight, ProcIo, ProcessRunner, ReopenError, RunnerError,
    RunningProcess, SpawnSpec, StdinWrite, check_log_ancestry, open_log_path,
};

/// Capacity of the actor's own mailbox (commands + internal events).
const MAILBOX_CAPACITY: usize = 256;

/// Capacity of one sheep task's control mailbox. At most one live `Kill` is
/// ever in flight, so this stays small.
const SHEEP_CTL_CAPACITY: usize = 4;

/// Capacity of one sheep task's signal mailbox.
///
/// Wider than [`SHEEP_CTL_CAPACITY`]: nothing bounds a burst of `shep signal`
/// calls at one sheep, and [`Actor::begin_signal`] reads a `Full` mailbox as
/// "this sheep's task is busy".
const SIGNAL_CAPACITY: usize = 16;

/// How much longer than its own two timeouts one swap of a reload is given
/// before the actor gives up on it (see [`Actor::arm_reload_deadline`]).
///
/// A swap is already bounded by `listen_timeout` then `graceful_timeout`, so
/// this covers scheduling jitter only. Abandonment never ends a serving
/// instance; what is lost is the rest of the reload.
const RELOAD_DEADLINE_SLACK: Duration = Duration::from_secs(5);

/// How long the shepherd waits for one line to land in a sheep's stdin before
/// reporting [`LineOutcome::NotWritten`].
///
/// A pipe fills at 64 KiB and the write then blocks until the app reads. Two
/// seconds, under the 5s an RPC caller gets when it sends no deadline, so the
/// `not_written` row reaches it. The waits run concurrently, so one `sendline`
/// costs at most this whatever the selector matched.
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many replies one sheep may still owe from actions that stopped
/// waiting before the oldest of them is forgotten (see [`ActionWaits`]).
///
/// One entry per trigger whose deadline the app missed, removed when the app
/// answers. A dropped entry costs one late reply going to the wrong wait.
const MAX_ABANDONED_ACTION_REPLIES: usize = 64;

// ---------------------------------------------------------------------
// Public command / handle surface
// ---------------------------------------------------------------------

/// Distinguishes one client connection from another.
///
/// Minted per accepted connection and never reused within a daemon's life.
/// The only thing scoped by it is smits. Here rather than in `server`, which
/// is `#[cfg(unix)]` and so unnameable from a Windows build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ConnId(u64);

impl ConnId {
    /// Mints the next id. Monotonic, and wide enough that a daemon cannot
    /// reach the wrap.
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, atomic::Ordering::Relaxed))
    }
}

/// Every sheep name that currently carries a smit, and which connection
/// painted it.
///
/// Keyed by name, not by instance id, so every instance of a named sheep
/// reads the same mark.
type Smits = HashMap<String, (ConnId, String)>;

/// Whether a [`Command::Start`] is all-or-nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchPolicy {
    /// Refuse the whole batch if any app in it provably cannot run, and
    /// register none of it.
    ///
    /// For `shep start` against a Flockfile: a partial registration leaves a
    /// flock matching neither the file nor its previous state.
    AllOrNothing,
    /// Register and spawn each app on its own merits, so one app that cannot
    /// run costs only itself.
    ///
    /// For a muster roll restore at boot, and for a dog. Every failure leaves
    /// an `Errored` row, and one from an unresolvable `user`/`group` carries
    /// [`SpawnIdentity::Unresolved`], so the app never runs as the daemon.
    PerApp,
}

/// Whether a registration call created the row it hands back, or found one
/// already there.
///
/// [`ProcessEventKind::Errored`] says a sheep transitioned, so the emit keys
/// on this rather than on the row's status, which is alike either way.
#[derive(Debug)]
enum Registration {
    /// This call registered the row, so any event owed for it is owed now.
    Fresh(ProcessInfo),
    /// An app of this name was already registered; the row is untouched.
    AlreadyKnown(ProcessInfo),
}

impl Registration {
    /// The row, whichever variant it is.
    fn into_info(self) -> ProcessInfo {
        match self {
            Self::Fresh(info) | Self::AlreadyKnown(info) => info,
        }
    }
}

/// Commands the supervisor actor accepts (wrapped in [`Msg::Command`]).
#[derive(Debug)]
pub(crate) enum Command {
    /// Registers + spawns each app's instances.
    Start {
        /// Already-validated app specs to expand into instances.
        apps: Vec<ResolvedApp>,
        /// Whether one app that provably cannot run refuses the whole batch.
        policy: BatchPolicy,
        /// Answers with every spawned instance, or the first spawn failure.
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Registers each app as a flock member without spawning anything.
    ///
    /// For restoring a muster roll: a sheep saved while stopped returns
    /// stopped and restartable.
    RegisterAtRest {
        /// Already-validated app specs to register, one entry each.
        apps: Vec<ResolvedApp>,
        /// Answers with every entry now registered, in the order given.
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Reports which of `apps` name a sheep registered under a different
    /// config.
    ///
    /// Read-only, so it is answered during a shutdown rather than refused.
    ConfigDrift {
        /// Already-validated app specs to compare against the flock.
        apps: Vec<ResolvedApp>,
        /// Answers with one entry per app that is registered and different.
        reply: oneshot::Sender<Result<Vec<SheepDrift>, SupervisorError>>,
    },
    /// Registers + spawns one dog, marked with where it came from.
    ///
    /// Separate from [`Self::Start`] only for the marker and for being
    /// idempotent by name. A dog is supervised exactly as a sheep is.
    StartDog {
        /// The dog's already-validated app spec, built by the daemon rather
        /// than read from a Flockfile.
        ///
        /// Boxed: unboxed it would size every [`Msg`] the actor receives.
        app: Box<ResolvedApp>,
        /// Where this dog came from, written onto its entry.
        source: DogSource,
        /// Answers with the dog's instance, started or already registered.
        reply: oneshot::Sender<Result<ProcessInfo, SupervisorError>>,
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
        /// Who asked. Governs whether this restart can be displaced
        /// mid-kill-ladder (see `Actor::claim_manual`) and the `manually` flag
        /// on the bus events it produces, never what the restart does.
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
        /// The liveness epoch the reporting probe was armed under, or `None`
        /// for a memory breach, which has nothing that can go stale.
        epoch: Option<u64>,
        /// The tree size a memory breach was observed at, or `None` for a
        /// liveness failure, which observes nothing.
        ///
        /// A breach is computed against the ceiling armed at that moment and
        /// delivered later, so `Actor::handle_extra_restart` re-asks against
        /// the ceiling in force now.
        observed: Option<MemSize>,
    },
    /// Replaces every matched sheep with a fresh instance, one instance of
    /// each app at a time.
    Reload {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers the moment the reload is accepted, not when it finishes.
        /// See [`Actor::handle_reload`].
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Stops + deregisters every sheep matching `selector`.
    Delete {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers with the deleted ids once every matched sheep is terminal.
        reply: oneshot::Sender<Result<Vec<u32>, SupervisorError>>,
    },
    /// Sets one app's instance count. See [`Actor::handle_scale`].
    Scale {
        /// The app's name, exactly as its config spells it. Not a selector:
        /// see [`shep_core::protocol::Request::Scale`].
        name: String,
        /// How many instances the app has when this returns.
        count: u32,
        /// Answers with the app's surviving instances and its new config.
        reply: oneshot::Sender<Result<Scaled, SupervisorError>>,
    },
    /// Merges a Flockfile's apps into the running flock. See
    /// [`Actor::handle_apply_config`].
    ApplyConfig {
        /// One entry per app the document declared, carrying the keys it
        /// literally wrote as well as the values behind them.
        apps: Vec<DeclaredApp>,
        /// How much of the load overwrites what an operator has set since.
        reset: ResetDepth,
        /// Answers with one report per app given, in the order given.
        reply: oneshot::Sender<Result<Vec<Applied>, SupervisorError>>,
    },
    /// Reads one sheep's effective config for a pane. See
    /// [`Actor::handle_sheep_config`].
    SheepConfig {
        /// The sheep's name, exactly as its config spells it. Not a
        /// selector, for [`Self::Scale`]'s reason.
        name: String,
        /// Answers with the view, or `None` when no sheep has that name.
        reply: oneshot::Sender<Result<Option<SheepConfigView>, SupervisorError>>,
    },
    /// Sets or removes one env key on one sheep, as an operator override.
    /// See [`Actor::handle_set_sheep_env`].
    SetSheepEnv {
        /// The sheep's name, not a selector, for [`Self::Scale`]'s reason.
        name: String,
        /// The env key.
        key: String,
        /// The value, or `None` to remove the key.
        ///
        /// [`EnvValue`], not a bare `String`: this enum derives `Debug` and
        /// an env value is the most secret-dense thing that reaches it
        /// (IR-41). The wire type carries the same protection for the same
        /// reason.
        value: Option<EnvValue>,
        /// Answers the config now parked for the sheep's next spawn, which
        /// `rpc.rs` hands to the registry, or `None` when no sheep has that
        /// name. An error only when the request is refused or the override
        /// store itself could not be read or written.
        reply: oneshot::Sender<Result<Option<ResolvedApp>, SupervisorError>>,
    },
    /// See [`Actor::handle_set_sheep_field`].
    SetSheepField {
        /// The sheep's name, not a selector, for [`Self::Scale`]'s reason.
        name: String,
        /// The `AppConfig` field.
        key: String,
        /// The new value, in the shape that field serializes as.
        value: serde_json::Value,
        /// Answers what the write did: the config now on the stored spec or
        /// parked, and whether the running child still lacks it. `None`
        /// when no sheep has that name. An error only when the request is
        /// refused or the override store itself could not be read or
        /// written.
        reply: oneshot::Sender<Result<Option<FieldSet>, SupervisorError>>,
    },
    /// Attaches a marker to one sheep by name, or clears it.
    ///
    /// Last writer wins: `Some` overwrites whatever is there, including
    /// another connection's mark. A `None` takes effect only when the stored
    /// [`ConnId`] matches.
    SetSmit {
        /// The connection painting it: the scope the mark lives in.
        conn: ConnId,
        /// The sheep's name, exactly as its config spells it. Not a selector,
        /// for [`Self::Scale`]'s reason.
        sheep: String,
        /// The marker, or `None` to clear this connection's own.
        smit: Option<Smit>,
        /// Answers with the named sheep's instances, or
        /// [`SupervisorError::NotFound`] when no sheep holds that name.
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Forgets every smit `conn` painted, leaving every other connection's
    /// alone.
    ///
    /// Sent from the server layer's per-connection tail, and that is the whole
    /// of a smit's cleanup: every way a mark can end also ends a socket.
    ForgetSmits {
        /// The connection that has ended.
        conn: ConnId,
        /// Answers once the marks are gone rather than merely queued.
        reply: oneshot::Sender<()>,
    },
    /// Full flock listing, name-grouped (see [`Actor::snapshot_all`]).
    List {
        /// Answers with the current snapshot.
        reply: oneshot::Sender<Vec<ProcessInfo>>,
    },
    /// Reopens the log files of every sheep matching `selector`.
    Reopen {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers once every matched pump has acknowledged, off a task of
        /// its own (see [`Actor::handle_reopen`]).
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Empties the log files of every sheep matching `selector`: flushes
    /// every pump writing to one of those paths, then truncates them.
    Flush {
        /// Which sheep.
        selector: ProcessSelector,
        /// Answers once every path has been truncated, off a task of its own
        /// (see [`Actor::handle_flush`]).
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Describes the whole flock for a daemon handover: what the fitness gate
    /// needs, and the blob the successor reads.
    #[cfg(unix)]
    HandoverSnapshot {
        /// The daemon's own two descriptors, which only `boot` knows.
        fds: DaemonFds,
        /// Answers once every live pump has flushed and reported, off a task
        /// of its own (see [`Actor::handle_handover_snapshot`]).
        reply: oneshot::Sender<Result<Snapshot, SupervisorError>>,
    },
    /// Answers whether this flock could be handed to a successor in place.
    ///
    /// The same gate [`Command::HandoverSnapshot`]'s caller applies, without
    /// the descriptor round trip: a question must not flush a pump.
    #[cfg(unix)]
    HandoverFitness {
        /// Answers on the actor loop: the gate awaits nothing.
        reply: oneshot::Sender<Result<Fitness, SupervisorError>>,
    },
    /// Puts one named action on the shepherd channel of every sheep matching
    /// `selector` and answers with what each app said back, or with why
    /// nothing came.
    ///
    /// One row per matched sheep: an answer is one reply body from one
    /// process.
    Trigger {
        /// Which sheep.
        selector: ProcessSelector,
        /// The action name, passed to the app verbatim.
        action: String,
        /// Argument text for the action, passed to the app verbatim.
        params: Option<String>,
        /// Answers once every matched sheep has answered, timed out or been
        /// refused, off a task of its own (see [`Actor::begin_action`]).
        reply: oneshot::Sender<Result<Vec<ActionReply>, SupervisorError>>,
    },
    /// Delivers one signal to the own process of every sheep matching
    /// `selector`, never its process group (see [`Actor::begin_signal`]).
    Signal {
        /// Which sheep.
        selector: ProcessSelector,
        /// The signal to deliver.
        sig: OperatorSignal,
        /// Answers once every matched sheep has been signalled or found not
        /// running, off a task of its own (see [`Actor::begin_signal`]).
        reply: oneshot::Sender<Result<Vec<SignalReply>, SupervisorError>>,
    },
    /// Writes one line to every matched sheep's stdin.
    SendLine {
        /// Which sheep.
        selector: ProcessSelector,
        /// The line, without its terminator: the writer appends exactly one
        /// `\n`.
        line: String,
        /// Answers once every matched write has settled or timed out, off a
        /// task of its own (see [`Actor::begin_send_line`]).
        reply: oneshot::Sender<Result<Vec<LineReply>, SupervisorError>>,
    },
    /// Graceful engine shutdown: kill ladder on every online sheep, then stop.
    Shutdown {
        /// Answers once every online sheep is terminal.
        reply: oneshot::Sender<()>,
    },
}

/// The actor's mailbox message: [`Command`]s plus events the actor generates
/// for itself (sheep-task exits, restart timers, readiness signals).
#[derive(Debug)]
pub(crate) enum Msg {
    /// A caller-issued command.
    Command(Command),
    /// A sheep task's proc resolved: natural exit or a completed kill.
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
        /// The sheep's `SheepSlot::epoch` at scheduling time. A timer whose
        /// epoch has moved on was left behind by a respawn, and is dropped.
        epoch: u64,
    },
    /// The sheep's shepherd channel reported readiness.
    ///
    /// Forwarded to `SheepSlot::ready_tx` if a task is waiting, dropped
    /// silently otherwise: an app may report ready whenever it likes.
    Ready {
        /// The sheep's id.
        id: u32,
    },
    /// One swap of a reload ran out of time.
    ///
    /// The only way out of a [`ReloadJob`] the actor raises for itself: every
    /// other one waits on a task it cannot make report.
    ReloadDeadline {
        /// The app whose reload this was armed for.
        name: String,
        /// Which arming this is, off [`Actor::next_deadline`]. A message not
        /// carrying the job's current [`ReloadJob::deadline`] is dropped.
        stamp: u64,
    },
    /// A replacement answered its own probe, or failed to, with the instance
    /// it replaced already gone.
    ///
    /// The result of the second readiness wait [`Actor::post_drain_probe`]
    /// asks for, the first one being answerable by the wrong process (see
    /// [`ReloadMode`]).
    ReloadVerified {
        /// The app whose reload this is the last step of.
        name: String,
        /// The replacement that was re-probed; a stale result is dropped.
        new_id: u32,
        /// Whether the replacement answered inside the deadline.
        readiness: Readiness,
    },
    /// The sheep's shepherd channel carried a reply to an action.
    ///
    /// Routed to the waiting action task if one is waiting, dropped silently
    /// otherwise. Without a `stamp` the only correlation is the action name;
    /// [`ActionWaits::answer`] decides which wait the reply belongs to.
    ActionReply {
        /// The sheep's id.
        id: u32,
        /// The action the app is answering.
        action: String,
        /// The reply body, exactly as the app sent it.
        body: String,
        /// The dispatch stamp the app echoed, if it echoed one.
        stamp: Option<u64>,
    },
    /// An action wait resolved.
    ActionResult {
        /// The sheep's id.
        id: u32,
        /// Which wait on that sheep this is the answer to; see
        /// [`PendingAction::stamp`].
        stamp: u64,
        /// The app's reply, or why none arrived.
        outcome: ActionOutcome,
    },
    /// A readiness wait resolved.
    ReadyResult {
        /// The sheep's id.
        id: u32,
        /// The slot's epoch when the wait began; a stale result is dropped.
        epoch: u64,
        /// The `manually` flag this spawn's `Online` would have carried had
        /// it not been gated. Rides with `epoch` rather than on the slot, so
        /// the two cannot drift apart.
        manually: bool,
        /// Whether the signal arrived or the deadline elapsed.
        readiness: Readiness,
    },
}

/// Error type returned from supervisor commands.
///
/// `#[non_exhaustive]`: shep-daemon is a published library, so a new variant
/// must not break an out-of-tree matcher.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    /// The selector matched no registered sheep.
    NotFound,
    /// Spawn failed (carries the runner's message).
    SpawnFailed(String),
    /// A command that would have started processes was refused before any of
    /// them was registered, because what it was asked to start provably could
    /// not run. Carries `"<name>: <reason>"` per app that could not.
    ///
    /// Unlike [`Self::SpawnFailed`], which can leave earlier apps registered
    /// and running, this guarantees an untouched flock. Both map to
    /// [`RpcErrorCode::SpawnFailed`](shep_core::protocol::RpcErrorCode::SpawnFailed).
    CannotStart(String),
    /// The selector reached an app that is already being reloaded; carries
    /// that app's name.
    ///
    /// A second reload would put a third entry in an instance slot two are
    /// already sharing. The whole command is refused, never half of it.
    ReloadInFlight(String),
    /// A `Scale` the engine will not perform; carries the refusal in plain
    /// English, naming what to do instead.
    ///
    /// Four shapes: a count of `0`, a target that is a dog, an app with
    /// departures still in flight, and a rescaled config that failed
    /// `normalize`. Maps to
    /// [`RpcErrorCode::InvalidConfig`](shep_core::protocol::RpcErrorCode::InvalidConfig),
    /// since each is something the caller can ask differently.
    InvalidScale(String),
    /// At least one log pump could not open a log path again, so that stream
    /// has no file to write to. Carries one
    /// `"<name> (id <id>): <paths and reasons>"` entry per such sheep, joined
    /// by `"; "`, with one sheep's own two paths joined by `", "` (see
    /// [`ReopenError::message`](crate::runner::ReopenError::message)). Every
    /// other pump was reopened.
    ///
    /// A named sheep can be one the selector did not match: the reach is every
    /// writer to a matched path.
    ReopenFailed(String),
    /// At least one matched log file could not be flushed or truncated.
    /// Carries one `"<path>: <reason>"` entry per such file, joined by `"; "`,
    /// where a pump that failed on both streams contributes one entry with the
    /// two paths joined by `", "`. Every other matched path was emptied.
    ///
    /// A failed truncate leaves its file as it was; a failed flush does not
    /// stop the truncate. Keyed by path where [`Self::ReopenFailed`] is keyed
    /// by sheep, one file belonging to several sheep (see
    /// [`FlushError::message`](crate::runner::FlushError::message)).
    FlushFailed(String),
    /// A request whose target is a dog, and which a dog is not a valid
    /// target for; carries the refusal in plain English.
    ///
    /// A dog runs at the daemon's own trust level and its binary is what
    /// `shep adopt` vetted, so its config is not an operator's to edit
    /// through a pane and not a pane's to read. `Actor::apply_one` refuses
    /// a Flockfile the same way and in the same words, and
    /// `Actor::handle_scale` refuses a scale for the neighbouring reason.
    ///
    /// Maps to
    /// [`RpcErrorCode::InvalidConfig`](shep_core::protocol::RpcErrorCode::InvalidConfig),
    /// like [`Self::InvalidScale`] and for its reason: this is something
    /// the caller asked for that it can ask differently.
    IsADog(String),
    /// A `SetSheepEnv` whose result is not a config this build accepts;
    /// carries `normalize`'s own refusal.
    ///
    /// One shape reaches it today: `SHEP_INSTANCE` and `SHEP_NAME` are
    /// injected per instance and refused in a hand-written `env`, so a pane
    /// offering a free-text key can be asked to set one. Nothing is written
    /// on it -- the config is checked before the store is touched -- so the
    /// operator's stored env is exactly what it was.
    ///
    /// Separate from [`Self::Overrides`] because the two want opposite
    /// things of the operator: this one is a request to change, and that
    /// one is a store to fix.
    InvalidEnv(String),
    /// A `SetSheepField` this build will not take; carries the reason.
    ///
    /// [`Self::InvalidEnv`]'s twin, separate for the same reason that one
    /// is separate from [`Self::Overrides`]: the two want opposite things
    /// of the operator. Four shapes reach it, and all four are the caller's
    /// own request rather than a fault -- a key [`AppConfig`] has no field
    /// for, a value that will not deserialize into the field it names, a
    /// config `normalize` refuses once the value is in, and the three keys
    /// this door does not own (`env`, which has
    /// `Handle::set_sheep_env`, and the two Structural ones). Nothing is
    /// written on any of them: the config is checked before the store is
    /// touched.
    InvalidField(String),
    /// The override store at `$SHEP_HOME/overrides.json` could not be read
    /// or written, so an operator's edit was not recorded. Carries
    /// [`OverridesError`](shep_core::overrides::OverridesError)'s own
    /// message, which names which of the three (I/O, a parse, a future
    /// version) it was.
    ///
    /// Its own variant rather than [`Self::SpawnFailed`] or an `Internal`
    /// string, because it is the one failure here that leaves the flock
    /// exactly as it was while telling the operator their change did not
    /// land. Nothing was spawned, nothing was killed, and the file on disk
    /// is what it was before the request.
    Overrides(String),
    /// The actor has shut down; its mailbox is closed.
    EngineStopped,
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("selector matched no registered sheep"),
            Self::SpawnFailed(msg) => write!(f, "spawn failed: {msg}"),
            Self::CannotStart(msg) => write!(f, "start refused: {msg}"),
            Self::ReloadInFlight(name) => write!(f, "{name} is already being reloaded"),
            Self::InvalidScale(msg) => write!(f, "cannot scale: {msg}"),
            Self::ReopenFailed(msg) => write!(f, "log reopen failed: {msg}"),
            Self::FlushFailed(msg) => write!(f, "log flush failed: {msg}"),
            Self::IsADog(msg) => write!(f, "refused: {msg}"),
            Self::InvalidEnv(msg) => write!(f, "cannot set that env key: {msg}"),
            Self::InvalidField(msg) => write!(f, "cannot set that field: {msg}"),
            Self::Overrides(msg) => write!(f, "overrides store unusable: {msg}"),
            Self::EngineStopped => f.write_str("supervisor engine has shut down"),
        }
    }
}

impl core::error::Error for SupervisorError {}

/// What a [`Command::SetSheepField`] did.
///
/// Two facts, and only the second is a judgement: the config the caller
/// should record on the muster roll, and whether the running child has the
/// value yet.
///
/// `Debug` is derived (IR-41): [`ResolvedApp`] wraps an [`AppConfig`], whose
/// own manual `Debug` redacts `env`, and a bool.
#[derive(Debug, Clone)]
pub(crate) struct FieldSet {
    /// The app as it now stands, for `rpc.rs` to hand the registry -- the
    /// parked config when the field parked, the stored spec's when it
    /// reached. `Command::SetSheepEnv` answers with the same thing and for
    /// the same reason: the muster roll is written from the registry.
    pub(crate) app: ResolvedApp,
    /// `true` when the running child does not have the value yet.
    pub(crate) pending: bool,
}

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
    /// - [`SupervisorError::CannotStart`]: at least one app provably could
    ///   not run, so nothing was registered. Carries one
    ///   `"<name>: <reason>"` per refused app, never one per failed check.
    /// - [`SupervisorError::SpawnFailed`]: the first instance that failed to
    ///   spawn (already-registered instances persist regardless).
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub async fn start(&self, apps: Vec<ResolvedApp>) -> Result<Vec<ProcessInfo>, SupervisorError> {
        self.start_with(apps, BatchPolicy::AllOrNothing).await
    }

    /// [`Self::start`], but one app that cannot run costs only itself.
    ///
    /// For restoring a muster roll: a saved app whose binary went missing must
    /// not keep the rest of the flock down.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::SpawnFailed`]: at least one app could not be
    ///   started, carrying one `"<name>: <reason>"` entry per such app joined
    ///   by `"; "`. Every app was attempted, and every one that could not
    ///   start is registered `Errored` and visible.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn start_restored(
        &self,
        apps: Vec<ResolvedApp>,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        self.start_with(apps, BatchPolicy::PerApp).await
    }

    async fn start_with(
        &self,
        apps: Vec<ResolvedApp>,
        policy: BatchPolicy,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Start {
                apps,
                policy,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Registers each app as a flock member without starting it.
    ///
    /// Idempotent by name: an app already known is returned as it stands, so
    /// restoring a roll over a live flock disturbs nothing.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn register_at_rest(
        &self,
        apps: Vec<ResolvedApp>,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::RegisterAtRest { apps, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Names the fields in which each app differs from the flock's own copy of
    /// the sheep of the same name.
    ///
    /// Reads the flock and changes nothing. [`Self::start`] on a name the
    /// flock already has adds instances rather than reconciling config.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn config_drift(
        &self,
        apps: Vec<ResolvedApp>,
    ) -> Result<Vec<SheepDrift>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::ConfigDrift { apps, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Merges each app a Flockfile declared into the sheep of the same name.
    ///
    /// Additive by default: a load appends what nobody has established and
    /// leaves everything else alone unless `reset` says otherwise. Nothing is
    /// registered, nothing is pruned, and nothing running is killed; a field
    /// the running child holds parks in [`ProcessEntry::pending`] for its next
    /// spawn. See [`Actor::handle_apply_config`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`]: the actor is gone. Every per-app
    ///   refusal rides inside the reply instead, in [`Applied::refused`].
    pub(crate) async fn apply_config(
        &self,
        apps: Vec<DeclaredApp>,
        reset: ResetDepth,
    ) -> Result<Vec<Applied>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::ApplyConfig { apps, reset, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// One sheep's effective config, for a pane about to edit it, or `None`
    /// when no sheep has that name.
    ///
    /// Read-only. `env` comes back emptied with its keys listed beside it,
    /// which [`SheepConfigView::new`] is what enforces. See
    /// [`Actor::handle_sheep_config`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::IsADog`] - the name is a dog's.
    /// - [`SupervisorError::EngineStopped`] - the actor is gone. An unknown
    ///   name is `Ok(None)` rather than an error, so the caller can word the
    ///   refusal itself.
    pub(crate) async fn sheep_config(
        &self,
        name: String,
    ) -> Result<Option<SheepConfigView>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::SheepConfig { name, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Sets `key` on `name`'s env, or removes it with `None`, recorded as an
    /// operator override. Answers `Ok(None)` when no sheep has that name.
    ///
    /// The `Some` carries the config now parked for that sheep's next spawn.
    /// `rpc.rs` hands it to [`crate::registry::FlockRegistry::record`], the
    /// way the `Scale` and `ApplyConfig` arms hand it theirs: the muster roll
    /// is written from the registry and nothing on the restore path reads the
    /// override store, so an edit that skipped this survives a
    /// `shep daemon reload` (the handover blob carries `pending`) and is lost
    /// by a cold restart.
    ///
    /// The running child holds the env it was spawned from, so the change
    /// parks for that sheep's next spawn rather than reaching it now, and
    /// `shep reload`/`shep restart` promote it. See
    /// [`Actor::handle_set_sheep_env`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::IsADog`] - the name is a dog's, and a dog's
    ///   config is not an operator's to edit through a pane.
    /// - [`SupervisorError::InvalidEnv`] - `normalize` refuses the result.
    /// - [`SupervisorError::Overrides`] - the override store could not be
    ///   read or written, so nothing was recorded and nothing parked.
    /// - [`SupervisorError::EngineStopped`] - the actor is gone.
    pub(crate) async fn set_sheep_env(
        &self,
        name: String,
        key: String,
        value: Option<EnvValue>,
    ) -> Result<Option<ResolvedApp>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::SetSheepEnv {
                name,
                key,
                value,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Sets one config field on one sheep as an operator override.
    ///
    /// [`Actor::handle_set_sheep_field`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::IsADog`] - the name is a dog's, and a dog's
    ///   config is not an operator's to edit through a pane.
    /// - [`SupervisorError::InvalidField`] - the key, the value, or the
    ///   resulting config is one this build will not take. Nothing was
    ///   written.
    /// - [`SupervisorError::Overrides`] - the override store could not be
    ///   read or written, so nothing was recorded and nothing parked.
    /// - [`SupervisorError::EngineStopped`] - the actor is gone.
    pub(crate) async fn set_sheep_field(
        &self,
        name: String,
        key: String,
        value: serde_json::Value,
    ) -> Result<Option<FieldSet>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::SetSheepField {
                name,
                key,
                value,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Registers and starts one dog, marked as coming from `source`.
    ///
    /// Idempotent by name: a dog already registered under `app`'s name is
    /// reported as it stands rather than started twice.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`]: shutdown has begun, or the
    ///   actor is gone.
    /// - [`SupervisorError::SpawnFailed`]: the binary could not be spawned.
    pub async fn start_dog(
        &self,
        app: ResolvedApp,
        source: DogSource,
    ) -> Result<ProcessInfo, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::StartDog {
                app: Box::new(app),
                source,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Stops every sheep matching `selector`.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
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

    /// Restarts every sheep matching `selector`, resetting its restart budget.
    ///
    /// Declares `CommandOrigin::Operator`: nothing may take the sheep off it
    /// mid-kill-ladder, and the events it emits carry `manually: true`. A
    /// restart the daemon raised itself goes through
    /// [`Self::restart_automatic`] or [`Self::extra_restart`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn restart(
        &self,
        selector: ProcessSelector,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        self.restart_with_origin(selector, CommandOrigin::Operator)
            .await
    }

    /// Restarts every sheep matching `selector` on the daemon's own initiative,
    /// a cron occurrence or a change under a watched tree, resetting its
    /// restart budget exactly as [`Self::restart`] does.
    ///
    /// Declares `CommandOrigin::Automatic`: an operator's `stop` or `delete`
    /// landing mid-kill-ladder takes the sheep off it, and the events it emits
    /// carry `manually: false`.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
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
    /// Silently does nothing when the report is stale, and there is no reply.
    /// Resets the restart budget and goes in as `CommandOrigin::Automatic`, so
    /// it is displaceable by an operator's command.
    pub(crate) async fn extra_restart(
        &self,
        id: u32,
        pid: u32,
        epoch: Option<u64>,
        observed: Option<MemSize>,
    ) {
        let _ = self
            .tx
            .send(Msg::Command(Command::ExtraRestart {
                id,
                pid,
                epoch,
                observed,
            }))
            .await;
    }

    /// Replaces every sheep matching `selector` with a fresh instance of the
    /// same app, one instance at a time. An overlap, not zero downtime: the
    /// old listener's backlog is lost unless the app drains inside
    /// `graceful_timeout`. Answers on acceptance, and re-reads no config.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::ReloadInFlight`]: an app the selector reached is
    ///   mid-reload; carries its name.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone, or a shutdown
    ///   forbids the spawn a reload needs.
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
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
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

    /// Sets `name`'s instance count. A partial scale-up answers `Ok` with
    /// [`Scaled::shortfall`] set and the achieved count on [`Scaled::app`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: no app of that name is registered.
    /// - [`SupervisorError::InvalidScale`]: a count of `0`, or a dog.
    /// - [`SupervisorError::CannotStart`]: a scale-up whose app has a
    ///   `user`/`group` that will not resolve. Nothing spawned or removed, and
    ///   only the growing arm resolves anything.
    /// - [`SupervisorError::ReloadInFlight`]: the app is mid-reload.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn scale(&self, name: &str, count: u32) -> Result<Scaled, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Scale {
                name: name.to_string(),
                count,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Attaches `smit` to the sheep called `sheep`, scoped to `conn`, or
    /// clears this connection's own mark with `None`.
    ///
    /// A clear from a connection that did not paint the mark is a no-op that
    /// still answers `Ok`; see [`Command::SetSmit`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: no sheep of that name is registered.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn set_smit(
        &self,
        conn: ConnId,
        sheep: &str,
        smit: Option<Smit>,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::SetSmit {
                conn,
                sheep: sheep.to_string(),
                smit,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Forgets every smit `conn` painted.
    ///
    /// A stopped engine has dropped every smit it held, so this answers even
    /// then; the caller is a connection tail with no use for a failure.
    pub(crate) async fn forget_smits(&self, conn: ConnId) {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Msg::Command(Command::ForgetSmits { conn, reply }))
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    /// Reopens the log files of every sheep matching `selector`, and of every
    /// other sheep writing to one of their paths, for an external rotator.
    /// When this returns no live pump still holds a renamed inode, which is
    /// what a logrotate `postrotate` stanza needs. The reply names only the
    /// sheep the selector reached.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::ReopenFailed`]: at least one pump could not open a
    ///   log path again; the old handles are closed either way.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
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

    /// Describes the whole flock for a daemon handover: one
    /// [`OwnedCandidate`] per registered sheep for the fitness gate, and the
    /// [`Handover`] blob the successor reads.
    ///
    /// `fds` are the daemon's own listener and pidfile descriptors, which
    /// `boot` opened and the actor has never seen. Answers once every live
    /// pump has flushed and reported its descriptors; a registered sheep that
    /// is not running has no pump and no descriptors, which is not a refusal.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    #[cfg(unix)]
    pub(crate) async fn handover_snapshot(
        &self,
        fds: DaemonFds,
    ) -> Result<Snapshot, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::HandoverSnapshot { fds, reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Whether this shepherd's flock could be handed to a successor in
    /// place, or the reason it could not.
    ///
    /// Read-only: the trigger for a handover is a signal, and this is the
    /// question a client asks before sending one.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    #[cfg(unix)]
    pub(crate) async fn handover_fitness(&self) -> Result<Fitness, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::HandoverFitness { reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Empties the log files of every sheep matching `selector`: flushes every
    /// pump writing to one of the paths those sheep were registered with, then
    /// truncates those paths. The operation addresses paths, so a matched
    /// sheep that is not running has no pump and its files are truncated all
    /// the same.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::FlushFailed`]: at least one matched file could not
    ///   be flushed or truncated. Every other matched path was emptied.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
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

    /// Sends `action` over the shepherd channel of every sheep matching
    /// `selector`, answering with one row per match, by name then by id.
    ///
    /// Answers on completion, bounded per sheep by its own
    /// `AppConfig::action_timeout`; the waits run alongside each other, so a
    /// flock costs the longest of them. A sheep that cannot be reached is
    /// refused in its own row, and `action` and `params` reach it verbatim.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn trigger(
        &self,
        selector: ProcessSelector,
        action: String,
        params: Option<String>,
    ) -> Result<Vec<ActionReply>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Trigger {
                selector,
                action,
                params,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Delivers `sig` to the own process of every sheep matching `selector`,
    /// never its process group, and answers with one row per match, by name
    /// then by id (only partial parity with
    /// `shep_core::protocol::sort_flock`; see `spawn_trigger_task`).
    ///
    /// A `kill(2)` either returns or does not, so this answers as soon as
    /// every matched sheep's delivery has settled.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn signal(
        &self,
        selector: ProcessSelector,
        sig: OperatorSignal,
    ) -> Result<Vec<SignalReply>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::Signal {
                selector,
                sig,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Writes `line` to every matched sheep's stdin, and answers with one row
    /// per match, by name then by id (only partial parity with
    /// `shep_core::protocol::sort_flock`; see `spawn_trigger_task`).
    ///
    /// A pipe write blocks until the app reads, so the reply is bounded per
    /// sheep at [`STDIN_WRITE_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`]: nothing matched.
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn send_line(
        &self,
        selector: ProcessSelector,
        line: String,
    ) -> Result<Vec<LineReply>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::SendLine {
                selector,
                line,
                reply,
            }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)?
    }

    /// Full flock listing, name-grouped (see [`Actor::snapshot_all`]).
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`]: the actor is gone.
    pub(crate) async fn list_checked(&self) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::List { reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)
    }

    /// Full flock listing, by name, then instance slot, then id
    /// (`shep_core::protocol::sort_flock`, which `snapshot_all` calls).
    ///
    /// For callers that need not tell "actor gone" from "empty flock".
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
#[derive(Debug)]
pub(crate) struct SupervisorBuilder<R: ProcessRunner> {
    runner: R,
    paths: ShepPaths,
    events: Bus,
    extras: Option<Extras>,
}

impl<R: ProcessRunner> SupervisorBuilder<R> {
    /// A builder with no lifecycle extras: the engine spawns, restarts and
    /// kills, and nothing watches, schedules or probes. `events` receives
    /// [`BusEvent::Process`] plus logs forwarded from each sheep.
    pub(crate) fn new(runner: R, paths: ShepPaths, events: Bus) -> Self {
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
        let actor = self.build(tx.clone());
        tokio::spawn(actor.run(rx));
        SupervisorHandle { tx }
    }

    /// Spawns the actor around a flock this image inherited, restoring
    /// `counters` before any slot is installed. Every sheep in `flock` goes in
    /// under the id, epoch, status and history the blob carried, around the
    /// descriptors that crossed the `execve`. Nothing here spawns, signals or
    /// reopens anything. `reloads` is the apps that were mid-swap, restored
    /// once the flock is in. Must be called from within a Tokio runtime
    /// context.
    ///
    /// # Errors
    ///
    /// - [`AdoptError::Spec`]: a carried config does not normalize.
    /// - [`AdoptError::Runner`]: the runner refused the inherited handles.
    #[cfg(unix)]
    pub(crate) fn spawn_adopted(
        self,
        flock: Vec<AdoptedSheep>,
        counters: Counters,
        reloads: Vec<CarriedReload>,
    ) -> Result<SupervisorHandle, AdoptError> {
        let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
        let mut actor = self.build(tx.clone());
        // Counters before slots: a fresh sheep must not be handed an id a
        // caller is still holding.
        actor.next_id = counters.next_id;
        actor.next_deadline = counters.next_deadline;
        actor.next_action_stamp = counters.next_action_stamp;
        // One reaper for the whole adopted flock: a status can be collected
        // once, so two reapers racing on one pid would have one take the exit
        // and the other meet `ECHILD`.
        let reaper = Arc::new(AdoptedReaper::new());
        for sheep in flock {
            actor.install_adopted(sheep, &reaper)?;
        }
        // After the loop and before the actor runs: a job names two entries,
        // so it has nowhere to go until every sheep is in, and the readiness
        // waits `install_adopted` armed report into a mailbox nothing drains.
        actor.install_carried_reloads(reloads);
        tokio::spawn(actor.run(rx));
        Ok(SupervisorHandle { tx })
    }

    /// The actor both spawn paths start from: no sheep, counters at zero.
    fn build(self, tx: mpsc::Sender<Msg>) -> Actor<R> {
        Actor {
            runner: self.runner,
            paths: self.paths,
            events: self.events,
            tx,
            sheep: HashMap::new(),
            next_id: 0,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: self.extras,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        }
    }
}

/// Why a flock this image inherited could not be installed.
///
/// Both variants name the sheep: that is which process is now unsupervised.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) enum AdoptError {
    /// The config the blob carried for this sheep does not normalize.
    ///
    /// Reachable when a successor's `normalize` has tightened since the
    /// predecessor accepted the app; the muster roll meets the same refusal.
    Spec {
        /// The sheep whose config was refused.
        sheep: String,
        /// What `normalize` said about it.
        source: shep_core::config::NormalizeError,
    },
    /// The runner would not take this sheep's inherited handles.
    ///
    /// A runner that never took part in a handover refuses by default, and
    /// a real one refuses a handle it cannot wire to a pump.
    Runner {
        /// The sheep whose adoption was refused.
        sheep: String,
        /// What the runner said about it.
        source: RunnerError,
    },
}

#[cfg(unix)]
impl fmt::Display for AdoptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spec { sheep, source } => {
                write!(
                    f,
                    "sheep '{sheep}' carried a config that no longer validates: {source}"
                )
            }
            Self::Runner { sheep, source } => {
                write!(f, "sheep '{sheep}' could not be adopted: {source}")
            }
        }
    }
}

#[cfg(unix)]
impl core::error::Error for AdoptError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Spec { source, .. } => Some(source),
            Self::Runner { source, .. } => Some(source),
        }
    }
}

/// Spawns the actor with no lifecycle extras: shorthand for
/// `SupervisorBuilder::new(runner, paths, events).spawn()`.
///
/// Must be called from within a Tokio runtime context: it spawns the actor
/// task immediately.
pub fn spawn_supervisor<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    events: Bus,
) -> SupervisorHandle {
    SupervisorBuilder::new(runner, paths, events).spawn()
}

// ---------------------------------------------------------------------
// Internal actor state
// ---------------------------------------------------------------------

/// Fire-and-forget control message to one sheep task (see `run_sheep`).
///
/// No acknowledgement: a sheep task's own `Msg::Exited` is the only completion
/// signal the actor waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SheepCtl {
    /// Run the kill ladder and report the resulting exit.
    Kill {
        /// How long the ladder's polite rung gets before it escalates to
        /// `SIGKILL`. On the message rather than read off the app inside
        /// [`kill_process`]: only the sender knows which of `kill_timeout`
        /// and `graceful_timeout` it is asking for.
        grace: Duration,
    },
}

/// Which of an app's two ladder caps a stop runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LadderCap {
    /// `kill_timeout`: an operator's `stop`, `restart` or `delete`, the
    /// daemon's own automatic restarts, and the engine-wide shutdown.
    Stop,
    /// `graceful_timeout`: a reload's drain, the one stop that asks the
    /// instance to finish the work in hand.
    Drain,
}

impl LadderCap {
    /// This cap's value for `app`.
    fn of(self, app: &AppConfig) -> Duration {
        match self {
            Self::Stop => app.kill_timeout,
            Self::Drain => app.graceful_timeout,
        }
        .as_duration()
    }
}

/// One app's in-flight reload.
///
/// Keyed by app name in [`Actor::reloads`]. An entry existing there means the
/// app is mid-reload, which is what makes a second reload of the same app
/// refusable ([`SupervisorError::ReloadInFlight`]).
#[derive(Debug)]
struct ReloadJob {
    /// Instances not yet taken, in slot order. Popped one at a time, so the
    /// app is only ever one instance short of its configured count.
    queue: VecDeque<u32>,
    /// Whether this reload overlaps its two instances or replaces them one
    /// after the other. Decided once, when the job is created: a job that
    /// changed mode half way would drain an instance it had already replaced.
    mode: ReloadMode,
    /// The pair mid-swap right now. Exactly one per job.
    swap: ReloadSwap,
    /// Which of this job's watchdogs is the live one.
    ///
    /// A job arms more than one over its life, and only the newest may end it,
    /// so each arming takes a fresh stamp off [`Actor::next_deadline`] and
    /// [`Actor::handle_reload_deadline`] drops any message not carrying it.
    deadline: u64,
}

/// One app's in-flight reload, as it crosses a handover.
///
/// A [`ReloadJob`] minus [`ReloadJob::deadline`], a stamp on a timer that died
/// with the predecessor's image. The app name is on the row, so the blob holds
/// an array in a stable order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Both sites that build one of these are `cfg(unix)` while the type travels on
// `Handover`, which is not, so a Windows target constructs none. An `expect`,
// so a Windows handover has to delete this line rather than inherit it.
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "no target without a handover ever builds one")
)]
pub(crate) struct CarriedReload {
    /// The app whose reload this is: [`Actor::reloads`]' own key.
    pub(crate) app: String,
    /// Instances not yet taken, in slot order.
    pub(crate) queue: Vec<u32>,
    /// Whether this job overlaps its two instances or replaces them one
    /// after the other.
    pub(crate) mode: ReloadMode,
    /// The pair mid-swap right now.
    pub(crate) swap: ReloadSwap,
}

/// Which of two orderings a reload runs, decided from the app's config.
///
/// A `readiness_probe` asks an address, and an address cannot say which
/// process answered it, so overlapped instances can answer for each other.
/// The app picks by whether it can share a port.
///
/// Serialized because a handover carries the job it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReloadMode {
    /// `SpawnNew → AwaitReady → DrainOld → ReapOld`. Both instances run at
    /// once, so the app is never short one.
    ///
    /// Taken by an app with no probe, by one using `wait_ready`, and by one
    /// whose `reuse_port` says the app sets `SO_REUSEPORT` itself. The last
    /// keeps a residue of the problem above, which
    /// [`Actor::post_drain_probe`] closes.
    Overlap,
    /// `DrainOld → ReapOld → SpawnNew → AwaitReady`. The instance being
    /// replaced goes first, and its replacement is spawned into the empty
    /// slot.
    ///
    /// The default for a probed app. It costs a gap, the drain plus the
    /// replacement's start, and buys a probe only the replacement can answer.
    /// It spares an app without `SO_REUSEPORT` the `EADDRINUSE` an overlapping
    /// reload would take. A single-instance app loses its name-group watch and
    /// cron worker for the width of the gap.
    Serial,
}

impl ReloadMode {
    /// The mode `config` asks for, given the readiness source already derived
    /// from it.
    ///
    /// `source` rather than the config's own two readiness fields, whose
    /// precedence is [`ReadinessSource::of`]'s to state. Only a `Probe` is
    /// answerable by the wrong instance, so only a `Probe` serialises.
    fn of(config: &AppConfig, source: &ReadinessSource) -> Self {
        match source {
            ReadinessSource::Probe(..) if !config.reuse_port => Self::Serial,
            _ => Self::Overlap,
        }
    }
}

/// The drainee/replacement pair a reload is working on right now.
///
/// Serialized because a handover carries the job it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ReloadSwap {
    /// The instance being replaced. Carries `ProcStatus::Stopping` and
    /// [`ReloadState::Drainee`] from the moment the swap starts.
    pub(crate) old_id: u32,
    /// Its replacement, in the same instance slot under a new id. Carries
    /// [`ReloadState::Replacement`] until the swap finishes, and is `None`
    /// exactly while the phase is [`ReloadPhase::DrainFirst`].
    pub(crate) new_id: Option<u32>,
    /// How far along this pair is; see [`ReloadPhase`].
    pub(crate) phase: ReloadPhase,
}

/// Where a [`ReloadSwap`] is in the spec's per-instance state machine.
///
/// `SpawnNew` and `ReapOld` are instants rather than intervals, so they get no
/// variant: what a handler asks is whether the old instance is still there to
/// go back to.
///
/// Serialized because a handover carries the swap it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReloadPhase {
    /// [`ReloadMode::Serial`] only: the instance being replaced is on its kill
    /// ladder and nothing has been spawned yet, so `swap.new_id` is `None`.
    ///
    /// Committed: a phase is abandonable only while there is an instance to go
    /// back to, and this one has already asked that instance to go, so
    /// [`Actor::uncommitted_swap_of`] answers `None` here.
    DrainFirst,
    /// The replacement is registered and starting.
    ///
    /// Under [`ReloadMode::Overlap`] nothing has been killed and the reload is
    /// still abandonable. Under [`ReloadMode::Serial`] the drainee is already
    /// gone, and [`Actor::reap_drainee`] moves the phase to `DrainOld` before
    /// returning.
    AwaitReady,
    /// Committed: the replacement went online and the drainee's ladder is
    /// running, or the drainee is already gone. No old instance to return to.
    DrainOld,
    /// [`ReloadMode::Overlap`] only, and only for a probed app: the drainee is
    /// reaped and the replacement is asked alone whether it can serve. See
    /// [`Actor::post_drain_probe`].
    Verify,
}

/// Which manual command is pending against a sheep, cleared the moment its
/// `Msg::Exited` is processed.
///
/// Serialized because a handover carries it, inside [`PendingManual`].
/// `snake_case` on the wire to match [`ProcStatus`]'s spelling: the blob is a
/// JSON file an operator may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManualKind {
    /// A `Stop` command targeted this sheep.
    Stop,
    /// A `Restart` command targeted this sheep.
    Restart,
    /// A `Delete` command targeted this sheep.
    Delete,
}

/// Who asked for a pending manual command. Decides which of two racing
/// commands owns a sheep's next exit ([`Actor::claim_manual`]) and the
/// `manually` flag on the bus events the command emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommandOrigin {
    /// A person asked for it: a `Stop`, `Restart` or `Delete` off the control
    /// socket, or the daemon-wide `Shutdown`.
    Operator,
    /// The daemon raised it itself: a memory breach or a liveness failure
    /// ([`SupervisorHandle::extra_restart`]), or a cron occurrence or
    /// watched-file change ([`SupervisorHandle::restart_automatic`]).
    ///
    /// Nobody is owed the answer, so an operator's `stop` may take the sheep
    /// off one mid-ladder rather than be converted into it.
    Automatic,
}

/// The manual command that owns a sheep's next exit, and who asked for it.
///
/// Crosses a handover whole, on [`CarriedSheep::manual`]. Without the marker
/// a successor would hand an `autorestart` app its ordinary respawn, so a
/// `shep stop` would come back as a running sheep. [`Self::origin`] crosses
/// unchanged too, answering who caused this exit.
///
/// [`CarriedSheep::manual`]: crate::handover::CarriedSheep::manual
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingManual {
    /// What that exit will be turned into.
    pub(crate) kind: ManualKind,
    /// Who asked. `kind` decides what the command does; `origin` survives into
    /// `handle_exited` only for the `manually` flag on the events that exit
    /// produces.
    pub(crate) origin: CommandOrigin,
}

/// One action the daemon has put on a sheep's shepherd channel and has not
/// finished waiting on.
#[derive(Debug)]
struct PendingAction {
    /// Which wait this is, for the whole life of the daemon.
    ///
    /// A counter of its own: the sheep's id and [`SheepSlot::epoch`] name the
    /// instance, not the action.
    stamp: u64,
    /// The action name this is waiting for a reply to; [`ActionWaits::answer`]
    /// falls back to it when the app echoes no `stamp`.
    action: String,
    /// Wakes the waiting task with the app's reply body.
    ///
    /// Taken by the reply that answers it, so a second reply to one action
    /// finds nothing to hand a body to. The entry stays until the task reports
    /// what it made of the body, which keeps `reply` reachable.
    waiter: Option<oneshot::Sender<String>>,
    /// Where this wait's outcome goes once it has one: the row-building half
    /// of [`Actor::begin_action`], which turns it into one [`ActionReply`].
    ///
    /// A bare outcome and not a `Result`: everything that could fail is
    /// decided before a wait is armed.
    reply: oneshot::Sender<ActionOutcome>,
}

/// One reply a sheep's app still owes a wait that has already ended.
///
/// The `stamp` separates a late reply from a prompt one naming the same
/// action. An app that does not echo one is matched by `action` and by order.
#[derive(Debug)]
struct AbandonedReply {
    /// The wait that ended without this reply.
    stamp: u64,
    /// Its action name: the fallback key for an app that does not echo.
    action: String,
}

/// What one sheep still owes on its shepherd channel: the action waits armed
/// against it, and the replies its app can still send that no wait wants.
///
/// An app may answer an action after its wait has given up. One that echoes
/// the dispatch stamp settles exactly its own debt; for one that does not,
/// order is the only signal, so the next unstamped reply naming that action
/// pays the debt rather than the live wait.
#[derive(Debug, Default)]
struct ActionWaits {
    /// Waits still expecting a message about them, oldest first.
    live: Vec<PendingAction>,
    /// One entry per reply the app still owes a wait that has already ended,
    /// oldest first, capped at [`MAX_ABANDONED_ACTION_REPLIES`].
    abandoned: VecDeque<AbandonedReply>,
}

impl ActionWaits {
    /// Records a wait the caller has already armed a task for.
    fn arm(&mut self, pending: PendingAction) {
        self.live.push(pending);
    }

    /// Routes one reply to `action`, stamped with `stamp` if the app echoed
    /// the dispatch's `id`, to the waiter it belongs to.
    ///
    /// A stamped reply goes to the live wait carrying that stamp, failing that
    /// settles that stamp's own debt. An unstamped one settles the oldest debt
    /// of that name first, and reaches a live wait only once the debt is
    /// clear. `None` is ordinary, not an error.
    fn answer(&mut self, action: &str, stamp: Option<u64>) -> Option<oneshot::Sender<String>> {
        if let Some(stamp) = stamp {
            if let Some(pending) = self
                .live
                .iter_mut()
                .find(|pending| pending.stamp == stamp && pending.waiter.is_some())
            {
                return pending.waiter.take();
            }
            if let Some(owed) = self.abandoned.iter().position(|debt| debt.stamp == stamp) {
                self.abandoned.remove(owed);
            }
            return None;
        }
        if let Some(owed) = self.abandoned.iter().position(|debt| debt.action == action) {
            self.abandoned.remove(owed);
            return None;
        }
        self.live
            .iter_mut()
            .find(|pending| pending.action == action && pending.waiter.is_some())
            .and_then(|pending| pending.waiter.take())
    }

    /// Ends the wait `stamp` names, recording the reply it never got if it
    /// never got one; hands back where its outcome goes.
    ///
    /// `None` for a stamp no live wait carries: [`Self::abandon_all`] answered
    /// it already.
    fn resolve(&mut self, stamp: u64) -> Option<oneshot::Sender<ActionOutcome>> {
        let at = self
            .live
            .iter()
            .position(|pending| pending.stamp == stamp)?;
        let pending = self.live.remove(at);
        // A waiter still sitting here ended without its reply. The debt stops
        // that reply being read as an answer to something else.
        if pending.waiter.is_some() {
            self.abandoned.push_back(AbandonedReply {
                stamp: pending.stamp,
                action: pending.action,
            });
            if self.abandoned.len() > MAX_ABANDONED_ACTION_REPLIES {
                self.abandoned.pop_front();
            }
        }
        Some(pending.reply)
    }

    /// Answers every live wait [`ActionOutcome::NoChannel`] and forgets every
    /// debt, which is what a sheep's process ending does to both halves.
    ///
    /// The debts go with the process that owed them. The live waits are
    /// answered rather than dropped, a dropped `reply` reaching its caller as
    /// the engine having gone away.
    fn abandon_all(&mut self) {
        for pending in self.live.drain(..) {
            let _ = pending.reply.send(ActionOutcome::NoChannel);
        }
        self.abandoned.clear();
    }
}

/// One registered instance: lifecycle state plus its live senders.
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
    /// Never cleared, unlike `ctl`: a send fails the moment the pump ends, so
    /// clearing this too would be a second copy of that fact. It costs the
    /// pump no extra life, `spawn_log_pump`'s `select!` also ending on its
    /// `logs` receiver.
    log_ctl: Option<mpsc::Sender<LogCtl>>,
    /// A clone of the [`ProcIo::to_child`] the most recent successful spawn
    /// handed out: the daemon's writing end of this sheep's shepherd channel,
    /// and the actor's only way to reach a live child directly. `None`
    /// whenever no process is running under this id.
    ///
    /// Not carried as a `ctl` message: [`Actor::claim_manual`] ignores a
    /// `Full` on that mailbox on the strength of [`SheepCtl::Kill`] being its
    /// sole occupant. Cleared where `log_ctl` is not: `tokio_runner`'s writer
    /// task parks on `recv()`, so a clone left here past the exit leaks that
    /// task and the daemon's half of the socketpair.
    to_child: Option<mpsc::Sender<ShepherdMessage>>,
    /// Sender for this sheep's signal mailbox, separate from [`Self::ctl`].
    /// `None` whenever no process is running under this id.
    ///
    /// A mailbox of its own rather than a [`SheepCtl`] variant: a burst
    /// sharing [`Self::ctl`]'s bounded slots would make
    /// [`Actor::claim_manual`] drop a stop. Cleared with [`Self::to_child`].
    signals: Option<mpsc::Sender<SignalRequest>>,
    /// A clone of the [`ProcIo::to_stdin`] the most recent successful spawn
    /// handed out: the daemon's writing end of this sheep's stdin pipe. `None`
    /// whenever no process is running under this id; present but closed when
    /// the running one never asked for a pipe (`AppConfig::stdin == false`),
    /// which is what [`Self::open_stdin`] filters.
    ///
    /// Cleared with [`Self::to_child`], for that field's reason.
    ///
    /// No `.await` on this sender may appear on the actor loop: an app that
    /// has stopped reading fd 0 fills its pipe and blocks the writer task, and
    /// the actor would park with it. [`Actor::begin_send_line`] uses
    /// `try_send`.
    to_stdin: Option<mpsc::Sender<StdinWrite>>,
    /// Which manual command (if any) is waiting on this sheep's next exit,
    /// and who asked for it. Claimed through [`Actor::claim_manual`].
    manual: Option<PendingManual>,
    /// Set whenever a `Delete` targets this id, even if an earlier command
    /// already owns `manual`. `manual` records who owns the next Kill; this
    /// records intent that must survive that race, so a Delete can never be
    /// downgraded to a Stop or a Restart. `handle_exited` checks it on the
    /// manual-Restart early return as well as on the `CleanStop` branch.
    pending_delete: bool,
    /// Bumped on every successful respawn. A `RestartDue` timer carries the
    /// epoch it was scheduled under, and `handle_restart_due` drops one whose
    /// epoch has moved on.
    epoch: u64,
    /// The readiness task's signal sender for the current epoch.
    /// `Msg::Ready`'s handler takes it to wake the task.
    ///
    /// `None` means either that no readiness task was ever armed, or that a
    /// channel `Ready` already took the sender. A wait that resolved another
    /// way leaves its sender here, and a late `Msg::Ready` drops silently.
    ready_tx: Option<oneshot::Sender<()>>,
    /// The action waits armed against this sheep and the replies its app still
    /// owes ones that have ended; see [`ActionWaits`].
    ///
    /// Cleared with [`Self::to_child`]: a wait armed against an exited process
    /// waits for a reply nobody will write.
    actions: ActionWaits,
    /// This instance's readiness wait ended without a signal, and a reload
    /// left it standing anyway: up, registered, and known not to be serving.
    ///
    /// Read by [`Actor::advance_reload`]'s replaceable test. A reload replaces
    /// `Online` instances, so without this the instance a failed reload left
    /// behind would be beyond the reach of the reload that rolls it back.
    ///
    /// Cleared wherever the id gets a new process or a new verdict:
    /// [`Actor::respawn`]'s success arm and [`Actor::went_online`].
    ready_failed: bool,
    /// When this slot's owed respawn falls due, in wall-clock terms.
    ///
    /// The same fact as [`Actor::schedule_restart`]'s monotonic timer, in the
    /// only clock that survives an `execve`: a handover carries this and
    /// re-arms a fresh timer from it. See
    /// [`backoff::adopted_restart_delay`](crate::backoff::adopted_restart_delay).
    ///
    /// `None` on every other status, and not cleared on the way out of
    /// `WaitingRestart`. Nothing reads it without the status.
    restart_due: Option<SystemTime>,
}

impl SheepSlot {
    /// This sheep's shepherd-channel sender while something is still there to
    /// receive on it, and `None` when nothing is.
    ///
    /// Read off the channel rather than `AppConfig::channel`, so there is no
    /// second copy of the fact. `is_closed` catches an app configured without
    /// a channel.
    fn open_channel(&self) -> Option<&mpsc::Sender<ShepherdMessage>> {
        self.to_child
            .as_ref()
            .filter(|to_child| !to_child.is_closed())
    }

    /// This sheep's stdin sender while something is still there to receive on
    /// it, and `None` when nothing is.
    ///
    /// Read off the channel rather than `AppConfig::stdin`, as
    /// [`Self::open_channel`] is. `is_closed` catches an app that never asked
    /// for a pipe, whose receiver the runner dropped at spawn.
    fn open_stdin(&self) -> Option<&mpsc::Sender<StdinWrite>> {
        self.to_stdin
            .as_ref()
            .filter(|to_stdin| !to_stdin.is_closed())
    }
}

/// Where a deferred `Stop`/`Restart`/`Delete`/`Shutdown` reply eventually
/// goes: the three commands differ only in their reply's payload shape.
#[derive(Debug)]
enum ReplyKind {
    /// `Stop`/`Restart`: reply with the matched sheep's terminal snapshots.
    Info(oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>),
    /// `Delete`: reply with the matched (and now deregistered) ids.
    Ids(oneshot::Sender<Result<Vec<u32>, SupervisorError>>),
    /// `Shutdown`: reply once every online sheep is terminal.
    Shutdown(oneshot::Sender<()>),
}

/// What a completed [`Command::Scale`] produced.
///
/// [`Actor::handle_scale`]'s caller replies with `instances` and re-records
/// `app` in the muster roll. A scale that fell short is still a `Scaled`:
/// recording is unconditional, and only the operator's exit code turns on
/// whether the request was fully satisfied.
#[derive(Debug)]
pub(crate) struct Scaled {
    /// The app's surviving instances, by name, then instance slot, then id
    /// (`shep_core::protocol::sort_flock`). On a partial scale-up this is what
    /// came up, never the count asked for.
    pub(crate) instances: Vec<ProcessInfo>,
    /// The app's config as it now stands, with the achieved `instances`
    /// count.
    pub(crate) app: ResolvedApp,
    /// The count the operator asked for. Equal to [`Self::achieved`] unless
    /// [`Self::shortfall`] is `Some`.
    pub(crate) requested: u32,
    /// `Some(message)` when a scale-up ran out part-way: the spawn failure
    /// that stopped it, in the runner's own words. `None` on every path that
    /// reached the requested count.
    pub(crate) shortfall: Option<String>,
}

impl Scaled {
    /// How many instances the app is left running.
    ///
    /// Read off `instances` rather than the stored config so the two cannot
    /// disagree: they are written from the same survivor list.
    pub(crate) fn achieved(&self) -> u32 {
        u32::try_from(self.instances.len()).unwrap_or(u32::MAX)
    }
}

/// What one app's [`Command::ApplyConfig`] did.
///
/// One of these per app the file named, whether or not the app was found and
/// whether or not anything about it changed.
#[derive(Debug)]
pub(crate) struct Applied {
    /// The app's name, exactly as the file spells it.
    pub(crate) name: String,
    /// Fields now in force, in field-name order. [`ApplyGroup::Live`] only:
    /// the daemon reads the new value at its next decision.
    pub(crate) applied: Vec<String>,
    /// Fields the app picks up at its next spawn, in field-name order.
    ///
    /// Not [`ProcessEntry::pending`], which shares its name and is a whole
    /// parked `ResolvedApp`. Both [`ApplyGroup::NextSpawn`] and
    /// [`ApplyGroup::NeedsRespawn`] land here: the running process keeps the
    /// old value either way, and what differs is where the new one waits, on
    /// the stored spec or in [`ProcessEntry::pending`].
    pub(crate) pending: Vec<String>,
    /// Why some or all of this app's change did not land, in the daemon's own
    /// words, or `None` when the whole of it did.
    ///
    /// Every refusal raised before [`Actor::apply_one`] routes the instance
    /// count leaves the app untouched. Two empty lists are not that promise,
    /// though: a later refusal produces the same shape, and only the message
    /// tells them apart.
    pub(crate) refused: Option<String>,
    /// The merged, normalized app. `rpc.rs` hands this to
    /// `FlockRegistry::record`, so a reboot comes up on the applied config.
    /// The full merge, `NeedsRespawn` fields included, since a reboot spawns
    /// every process afresh.
    ///
    /// `None` for an app whose merge never produced one: the refusals above
    /// that touch nothing, plus a merge that does not normalize at the
    /// instance count really running.
    pub(crate) app: Option<ResolvedApp>,
}

/// The `AppConfig` fields a lifecycle extra reads when it is armed, rather
/// than at each decision.
///
/// All eight are [`ApplyGroup::Live`], but a worker already armed against the
/// old value keeps it, so changing one needs [`ExtrasRegistry::rearm_name`] on
/// top of the spec write. Not derivable from [`apply_group`]: `fold` and
/// `action_timeout` are Live too and need no re-arm.
const EXTRAS_FIELDS: &[&str] = &[
    "max_memory",
    "watch",
    "ignore_watch",
    "watch_delay",
    "watch_options",
    "cron_restart",
    "cron_timezone",
    "liveness_probe",
];

/// Records what a load's `env` branch just merged: the file's env keys are
/// established from here, and an override of one is spent -- unless it is a
/// tombstone, which is spent by nothing a load can do.
///
/// # Why a tombstone survives and a value does not
///
/// Spending an override the file declares is right for a VALUE: the file now
/// supplies that key, so the operator's copy of it has nothing left to say
/// and keeping it would mark a sheep that no longer differs. This rule
/// predates `Actor::handle_set_sheep_env`, and it was written when an
/// override could only ever be a value.
///
/// A tombstone (a JSON `null`) means the ABSENCE of a key, and the file
/// declaring that key is exactly the case where it still has work to do: the
/// file says `DB_PASS=fromfile`, the sheep has no `DB_PASS`, and those two
/// are not the same app. Spending it made `AppOverrides::fields` come back
/// empty after the first load, so `ProcessEntry::overridden` stopped naming
/// `env` and the `CFG` column claimed a sheep matched its Flockfile while it
/// did not. That column is the operator's only signal that a sheep diverges,
/// and a removal is the one edit that could make it lie.
///
/// The removal itself never depended on this: `merge_declared`'s env loop
/// skips a key held in `overridden_env`, which counts a null like any other
/// entry, and `declared_env` blocks it again from the second load on. So this
/// was a reporting failure rather than a data one, which is precisely why
/// nothing caught it.
///
/// # This function's position no longer matters, and that is the point
///
/// It is called AFTER the env merge in both arms, and the obvious worry is
/// that swapping the two would let a load resurrect a removed value. It
/// cannot, and preserving the tombstone is what makes that true: the loop
/// skips a key that is in the map, this function no longer takes one out of
/// the map, so there is nothing for an ordering to expose. Measured both
/// ways -- moving the call above the loop changes no test, and so does
/// additionally reading `overridden_env` off `next` rather than off the
/// original record. Take the same pair of edits against a build that spends
/// the tombstone and the file's value comes back.
///
/// Called only from the two arms that merge `env`. [`ResetDepth::Policy`] and
/// [`ResetDepth::File`] touch `env` not at all, so calling it for them would
/// establish keys no plain load could then append. `declared_env` is a
/// high-water mark, never a snapshot of the current file.
///
/// Known gap: under [`ResetDepth::None`] an env key skipped because an
/// override already holds it is established anyway, so clearing that override
/// later leaves the key out of scope for good.
fn establish_env(next: &mut AppOverrides, incoming: &DeclaredApp) {
    if let Some(serde_json::Value::Object(env)) = next.fields.get_mut("env") {
        for key in &incoming.declared_env {
            if env.get(key).is_some_and(serde_json::Value::is_null) {
                continue;
            }
            env.remove(key);
        }
        if env.is_empty() {
            next.fields.remove("env");
        }
    }
    next.declared_env
        .extend(incoming.declared_env.iter().cloned());
}

/// Merges what a Flockfile declares into what a sheep is running, returning
/// the merged config and the override record to write back.
///
/// A key in scope takes the file's value and gives up its override, and
/// `reset` decides scope: `None` only a declared key nobody has established
/// and never `instances`, `File` every declared key, `Policy` every key but
/// `env`, `Env` none (it resets `env` alone), `All` every key and `env` too.
///
/// # Errors
///
/// A description of the failure, for [`Applied::refused`], if either config
/// fails to travel through serde.
fn merge_declared(
    stored: &AppConfig,
    incoming: &DeclaredApp,
    overrides: &AppOverrides,
    reset: ResetDepth,
) -> Result<(AppConfig, AppOverrides), String> {
    // Through serde rather than field by field: a hand-written list of
    // assignments goes stale when a field is added to the struct.
    let object = |config: &AppConfig| match serde_json::to_value(config) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err("an app config must serialize as an object".to_string()),
        Err(err) => Err(err.to_string()),
    };
    let mut merged = object(stored)?;
    let file = object(&incoming.config)?;
    let defaults = object(&AppConfig::default())?;

    let mut next = overrides.clone();
    // What the file established on an earlier load, plus what the operator
    // has set since. Either one makes a key established.
    let established: BTreeSet<&String> = overrides
        .declared
        .iter()
        .chain(overrides.fields.keys())
        .collect();

    for key in defaults.keys() {
        if key == "env" {
            continue;
        }
        let in_scope = match reset {
            // Every key goes back, declared or not; one the file is silent
            // about goes back to what a fresh start off that file would give.
            ResetDepth::Policy | ResetDepth::All => true,
            // A key the template never named has no template value to go back
            // to, so the operator's stands. `instances` gets no exception: a
            // template declaring it is an operator asking for that count.
            ResetDepth::File => incoming.declared.contains(key),
            // `ResetDepth::Env` and `ResetDepth::None`, append-only: a key the
            // template declares that nobody has established. `instances` is
            // held out of both, since the store cannot tell a stocked count
            // from an untouched one, so appending would delete instances.
            _ => {
                key != "instances" && incoming.declared.contains(key) && !established.contains(key)
            }
        };
        if !in_scope {
            continue;
        }
        // The override is spent: one left in `fields` would keep
        // `ProcessEntry::overridden` reporting a field that matches the file.
        next.fields.remove(key);
        // `declared` grows only where the file really spoke: marking an
        // undeclared key would lock a later append out of it.
        if incoming.declared.contains(key) {
            next.declared.insert(key.clone());
        }
        // `file` is `incoming.config`, the CLI-resolved app, so an undeclared
        // key reads back what a fresh `shep start` of this file would give it,
        // not the compiled default.
        if let Some(value) = file.get(key) {
            merged.insert(key.clone(), value.clone());
        }
    }

    match reset {
        // The two depths that keep `env`: it is operator-supplied data where
        // the rest is operator-tuned policy.
        ResetDepth::Policy | ResetDepth::File => {}
        // The two that reset it. They agree about nothing else: `All` resets
        // every setting and `Env` resets none.
        ResetDepth::All | ResetDepth::Env => {
            next.fields.remove("env");
            merged.insert(
                "env".to_string(),
                file.get("env")
                    .cloned()
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
            );
            establish_env(&mut next, incoming);
        }
        _ => {
            let overridden_env: BTreeSet<&String> = overrides
                .fields
                .get("env")
                .and_then(serde_json::Value::as_object)
                .map(|env| env.keys().collect())
                .unwrap_or_default();
            let mut env = stored.env.clone();
            for key in &incoming.declared_env {
                if overrides.declared_env.contains(key) || overridden_env.contains(key) {
                    continue;
                }
                if let Some(value) = incoming.config.env.get(key) {
                    env.insert(key.clone(), value.clone());
                }
            }
            merged.insert(
                "env".to_string(),
                serde_json::to_value(env).map_err(|err| err.to_string())?,
            );
            establish_env(&mut next, incoming);
        }
    }

    let merged: AppConfig =
        serde_json::from_value(serde_json::Value::Object(merged)).map_err(|err| err.to_string())?;
    Ok((merged, next))
}

/// The spec a load leaves on a running instance: what it was spawned from,
/// plus every merged field that can reach it without a replacement, plus the
/// instance count actually achieved.
///
/// `reaching` is the drifted [`ApplyGroup::Live`] and
/// [`ApplyGroup::NextSpawn`] field names; a [`ApplyGroup::NeedsRespawn`] one
/// is left in [`ProcessEntry::pending`] until the instance is replaced.
///
/// # Errors
///
/// A description of the failure if the rebuilt config does not travel through
/// serde or does not normalize, which a subset of a valid merge can.
fn reached_spec(
    stored: &AppConfig,
    merged: &AppConfig,
    reaching: &[String],
    instances: u32,
) -> Result<ResolvedApp, String> {
    let object = |config: &AppConfig| match serde_json::to_value(config) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err("an app config must serialize as an object".to_string()),
        Err(err) => Err(err.to_string()),
    };
    let mut next = object(stored)?;
    let source = object(merged)?;
    for field in reaching {
        if let Some(value) = source.get(field) {
            next.insert(field.clone(), value.clone());
        }
    }
    let mut config: AppConfig =
        serde_json::from_value(serde_json::Value::Object(next)).map_err(|err| err.to_string())?;
    config.instances = instances;
    normalize(config).map_err(|err| err.to_string())
}

/// The field names `entry`'s parked config differs from its running spec on,
/// or empty when nothing is parked.
///
/// Two readers ask this and they must never disagree about the same sheep:
/// [`ProcessInfo::pending`], which every listing renders, and
/// [`Actor::handle_sheep_config`], which a config pane renders. One
/// definition so the pane and the flock table cannot drift, the same reason
/// [`Actor::representative_id`] is one function.
///
/// An empty answer covers two different states -- nothing parked, and a
/// parked config that turned out identical to the spec -- and the callers
/// part company there rather than here: `ProcessInfo::pending` reports
/// `None` for both, because its own doc says `None` means nothing parked and
/// an empty list is not nothing parked.
fn pending_fields(entry: &ProcessEntry) -> Vec<String> {
    entry.pending.as_ref().map_or_else(Vec::new, |parked| {
        entry.spec.config().drifted_fields(parked.config())
    })
}

/// The one sentence every door that refuses to touch a dog's config says.
///
/// Four doors say it: [`Actor::apply_one`], where a Flockfile named a dog;
/// [`Actor::handle_sheep_config`], where a pane asked to read one; and
/// [`Actor::handle_set_sheep_env`] and [`Actor::handle_set_sheep_field`],
/// where a pane asked to write one. An operator who meets it from any of
/// them is being told the same thing and pointed at the same verb, which is
/// the reason it is one function rather than four literals that drift.
fn dog_config_refusal(name: &str) -> String {
    format!(
        "{name} is a dog, and a dog's config comes from `shep adopt` rather than \
         from a Flockfile"
    )
}

/// `app` with its instance count set to `instances`, or `None` if the result
/// does not normalize.
///
/// Re-normalized rather than mutated in place: a [`ResolvedApp`] is a proof
/// token that its config passed `normalize`.
fn with_count(app: &ResolvedApp, instances: u32) -> Option<ResolvedApp> {
    let mut config = app.config().clone();
    config.instances = instances;
    normalize(config).ok()
}

/// One command's aggregation state, replied to once `remaining` is empty.
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
/// control handle, never a live `proc`, plus any deferred command replies
/// still waiting on matched sheep to go terminal.
struct Actor<R: ProcessRunner> {
    /// Spawn seam (real OS processes or, in tests, the scripted fake).
    runner: R,
    /// `$SHEP_HOME` layout, for assembling spawn specs.
    paths: ShepPaths,
    /// Bus: process lifecycle events + forwarded logs.
    events: Bus,
    /// Clone handed to sheep tasks and restart timers so they can report
    /// back into this same actor's mailbox.
    tx: mpsc::Sender<Msg>,
    /// Every registered instance, keyed by id.
    sheep: HashMap<u32, SheepSlot>,
    /// Monotonic id counter: ids are never reused.
    next_id: u32,
    /// Stamps the next reload watchdog, so a job's older ones cannot end it.
    /// Never reused; see [`ReloadJob::deadline`].
    next_deadline: u64,
    /// Monotonic stamp counter for action waits; see
    /// [`PendingAction::stamp`].
    next_action_stamp: u64,
    /// Deferred command replies still waiting on matched sheep.
    pending: Vec<PendingReply>,
    /// Set once a `Shutdown` command starts. While `true`, `Start`/`Restart`
    /// are rejected and `RestartDue` respawns nothing: no child may appear
    /// that the shutdown aggregation, fixed when it ran, cannot know to kill.
    shutting_down: bool,
    /// The lifecycle extras' seams and report wiring, or `None` for an engine
    /// built without them (`spawn_supervisor`).
    extras: Option<Extras>,
    /// What is armed right now, per sheep and per name. Stays empty while
    /// `extras` is `None`: there are no seams to arm anything on.
    registry: ExtrasRegistry,
    /// Every sheep name currently carrying a smit; see [`Smits`]. A dog
    /// painting one puts an entry here, and its connection closing removes it.
    smits: Smits,
    /// Every app currently mid-reload, keyed by app name. An entry is what
    /// makes a second reload of the same app refusable, and what tells a
    /// sheep's exit whether it is a swap's business or its own.
    reloads: HashMap<String, ReloadJob>,
}

impl<R: ProcessRunner> Actor<R> {
    /// Runs the actor to completion: every mailbox message until a `Shutdown`
    /// fully resolves. Dropping `rx` then closes the mailbox, so later
    /// [`SupervisorHandle`] calls see [`SupervisorError::EngineStopped`].
    async fn run(mut self, mut rx: mpsc::Receiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            let should_break = match msg {
                // Synchronous: nothing in the command path awaits, so the
                // actor cannot park on a busy sheep task.
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
                Msg::ReloadDeadline { name, stamp } => {
                    self.handle_reload_deadline(&name, stamp);
                    false
                }
                Msg::ReloadVerified {
                    name,
                    new_id,
                    readiness,
                } => {
                    self.handle_reload_verified(&name, new_id, readiness);
                    false
                }
                Msg::ActionReply {
                    id,
                    action,
                    body,
                    stamp,
                } => {
                    self.handle_action_reply(id, &action, body, stamp);
                    false
                }
                Msg::ActionResult { id, stamp, outcome } => {
                    self.handle_action_result(id, stamp, outcome);
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
            // Rejected once shutdown has begun: it would spawn a child the
            // shutdown aggregation, fixed when it ran, cannot know to kill.
            Command::Start {
                apps,
                policy,
                reply,
            } => {
                let result = if self.shutting_down {
                    Err(SupervisorError::EngineStopped)
                } else {
                    self.do_start(apps, None, policy)
                };
                let _ = reply.send(result);
                false
            }
            Command::RegisterAtRest { apps, reply } => {
                let registered = apps.iter().map(|app| self.register_at_rest(app)).collect();
                let _ = reply.send(Ok(registered));
                false
            }
            Command::ConfigDrift { apps, reply } => {
                let _ = reply.send(Ok(self.config_drift(&apps)));
                false
            }
            // Rejected while shutting down: a dog spawns. The check is inside
            // `do_start_dog`.
            Command::StartDog { app, source, reply } => {
                let result = self.do_start_dog(*app, source);
                let _ = reply.send(result);
                false
            }
            Command::Scale { name, count, reply } => {
                self.handle_scale(&name, count, reply);
                false
            }
            // Answered during a shutdown: `handle_apply_config` declines to
            // route the instance count while `shutting_down`.
            Command::ApplyConfig { apps, reset, reply } => {
                let report = self.handle_apply_config(apps, reset);
                let _ = reply.send(Ok(report));
                false
            }
            // All three are answered during a shutdown: one reads memory,
            // and the others park a config for a spawn a shutdown will not
            // reach.
            Command::SheepConfig { name, reply } => {
                let _ = reply.send(self.handle_sheep_config(&name));
                false
            }
            Command::SetSheepEnv {
                name,
                key,
                value,
                reply,
            } => {
                let _ = reply.send(self.handle_set_sheep_env(
                    &name,
                    &key,
                    value.as_ref().map(EnvValue::as_str),
                ));
                false
            }
            Command::SetSheepField {
                name,
                key,
                value,
                reply,
            } => {
                let _ = reply.send(self.handle_set_sheep_field(&name, &key, &value));
                false
            }
            Command::SetSmit {
                conn,
                sheep,
                smit,
                reply,
            } => {
                let _ = reply.send(self.handle_set_smit(conn, &sheep, smit));
                false
            }
            Command::ForgetSmits { conn, reply } => {
                self.smits.retain(|_, (painter, _)| *painter != conn);
                let _ = reply.send(());
                false
            }
            Command::List { reply } => {
                let _ = reply.send(self.snapshot_all());
                false
            }
            Command::Reopen { selector, reply } => {
                self.handle_reopen(&selector, reply);
                false
            }
            Command::Flush { selector, reply } => {
                self.handle_flush(&selector, reply);
                false
            }
            #[cfg(unix)]
            Command::HandoverSnapshot { fds, reply } => {
                self.handle_handover_snapshot(fds, reply);
                false
            }
            #[cfg(unix)]
            Command::HandoverFitness { reply } => {
                self.handle_handover_fitness(reply);
                false
            }
            Command::Trigger {
                selector,
                action,
                params,
                reply,
            } => {
                self.begin_action(&selector, action, params, reply);
                false
            }
            Command::Signal {
                selector,
                sig,
                reply,
            } => {
                self.begin_signal(&selector, sig, reply);
                false
            }
            Command::SendLine {
                selector,
                line,
                reply,
            } => {
                self.begin_send_line(&selector, line, reply);
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
            Command::ExtraRestart {
                id,
                pid,
                epoch,
                observed,
            } => {
                self.handle_extra_restart(id, pid, epoch, observed);
                false
            }
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

    /// Registers + spawns one dog, or reports the one already registered
    /// under that name.
    ///
    /// The name lookup reads names rather than markers: what it rules out is
    /// two live processes under one name, dog or not.
    fn do_start_dog(
        &mut self,
        app: ResolvedApp,
        source: DogSource,
    ) -> Result<ProcessInfo, SupervisorError> {
        if self.shutting_down {
            return Err(SupervisorError::EngineStopped);
        }
        if let Some(slot) = self
            .sheep
            .values()
            .find(|slot| slot.entry.spec.config().name == app.config().name)
        {
            return Ok(to_info(&slot.entry, &self.smits));
        }
        // `PerApp`: a dog that cannot start must land in the dogs table as
        // `Errored`, which `dogs::spawn_dog_watch` subscribes to.
        let started = self.do_start(vec![app], Some(source), BatchPolicy::PerApp)?;
        started
            .into_iter()
            .next()
            .ok_or_else(|| SupervisorError::SpawnFailed("the dog registered no instance".into()))
    }

    /// Expands each app through `instance_slots` + `assemble`, spawning one
    /// instance per slot, after checking every app in the batch.
    ///
    /// Under [`BatchPolicy::AllOrNothing`] nothing is registered if any app
    /// fails that check, and the error names every one that did. A spawn that
    /// fails anyway still leaves the batch part-registered: only exec knows
    /// for certain.
    ///
    /// `dog` is written onto every entry this registers, and is `None` for
    /// every caller but [`Self::do_start_dog`]; see [`ProcessEntry::dog`].
    fn do_start(
        &mut self,
        apps: Vec<ResolvedApp>,
        dog: Option<DogSource>,
        policy: BatchPolicy,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        // One sequence rather than two: a zip against `apps` misaligns the
        // moment a failure is skipped.
        let mut ready: Vec<(ResolvedApp, Option<Credentials>)> = Vec::with_capacity(apps.len());
        // `AllOrNothing` only: a non-empty `refusals` returns before anything
        // is registered.
        let mut refusals = Vec::new();
        // `PerApp` only, joined into one error at the end.
        let mut failures = Vec::new();
        let total = apps.len();
        for app in apps {
            let name = app.config().name.clone();
            // Once per app: every instance shares one identity, and it is
            // ahead of the registering loop so a lookup failing on the fourth
            // app leaves none of the first three registered.
            let credentials = match privilege::resolve(app.config()) {
                Ok(resolved) => resolved,
                Err(err) => {
                    // One refusal per app, not one per failed check: the
                    // summary below counts apps.
                    match policy {
                        // No row: one `Errored` row alone is the half-state
                        // this policy prevents.
                        BatchPolicy::AllOrNothing => refusals.push(format!("{name}: {err}")),
                        // A row, since the rest of the batch goes on. Safe
                        // only because it carries `SpawnIdentity::Unresolved`,
                        // so a later restart resolves through
                        // `credentials_for_spawn` and meets this same refusal.
                        BatchPolicy::PerApp => {
                            failures.push(format!("{name}: {err}"));
                            // `Fresh` only: the emit announces a transition,
                            // and a repeat restore transitions nothing.
                            if let Registration::Fresh(info) = self.register_without_spawning(
                                &app,
                                ProcStatus::Errored,
                                dog.clone(),
                            ) {
                                self.emit(ProcessEventKind::Errored, info, true);
                            }
                        }
                    }
                    continue;
                }
            };
            // Instance 0 and no credentials: neither changes which file exec
            // names, which is all `preflight` reads.
            match self.runner.preflight(&assemble(&app, 0, &self.paths, None)) {
                Preflight::Unknown => {}
                Preflight::Impossible(reason) if policy == BatchPolicy::AllOrNothing => {
                    refusals.push(format!("{name}: {reason}"));
                    continue;
                }
                // Reported, never refused: the spawn fails on its own and
                // names the same program.
                Preflight::Impossible(reason) | Preflight::Doubtful(reason) => {
                    tracing::warn!(sheep = %name, "{reason}");
                }
            }
            ready.push((app, credentials));
        }
        if !refusals.is_empty() {
            return Err(SupervisorError::CannotStart(format!(
                "nothing was registered; {} of {} apps cannot start: {}",
                refusals.len(),
                total,
                refusals.join("; "),
            )));
        }

        let mut results = Vec::new();
        'apps: for (app, credentials) in ready {
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
                match self.spawn_fresh(&app, instance, credentials, dog.clone()) {
                    Ok(info) => results.push(info),
                    Err(message) => {
                        let failure = format!("{name}: {message}");
                        match policy {
                            BatchPolicy::AllOrNothing => {
                                return Err(SupervisorError::SpawnFailed(failure));
                            }
                            // Every remaining app still gets its turn.
                            // `spawn_fresh` registered this one `Errored`.
                            BatchPolicy::PerApp => {
                                failures.push(failure);
                                // The rest of this app's instances share its
                                // binary and cwd, and would fail the same.
                                continue 'apps;
                            }
                        }
                    }
                }
            }
        }
        // An `Err` rather than a partial `Ok`, so `snapshot::muster`'s log and
        // `do_start_dog`'s refusal still fire.
        if !failures.is_empty() {
            return Err(SupervisorError::SpawnFailed(failures.join("; ")));
        }
        // Built so far in the caller's order, which is the Flockfile's rather
        // than the one every other listing takes.
        sort_flock(&mut results);
        Ok(results)
    }

    /// The credentials `id`'s next spawn must apply, resolved once if this
    /// entry has never had them resolved.
    ///
    /// An entry resolved at its `Start` answers from the stored value, so a
    /// running app's identity cannot change underneath it. A failed resolution
    /// stores nothing, so a later restart asks again.
    ///
    /// # Errors
    /// - Whatever [`privilege::resolve`] refused this app's `user`/`group`
    ///   for, including a non-root daemon asked to change identity.
    fn credentials_for_spawn(&mut self, id: u32) -> Result<Option<Credentials>, PrivilegeError> {
        let slot = self
            .sheep
            .get(&id)
            .expect("credentials_for_spawn: unknown id");
        match slot.entry.credentials {
            SpawnIdentity::Resolved(credentials) => Ok(credentials),
            SpawnIdentity::Unresolved => {
                let credentials = privilege::resolve(slot.entry.spec.config())?;
                self.sheep
                    .get_mut(&id)
                    .expect("credentials_for_spawn: the entry was read a moment ago")
                    .entry
                    .credentials = SpawnIdentity::Resolved(credentials);
                Ok(credentials)
            }
        }
    }

    /// Registers one app as a member of the flock without spawning anything.
    ///
    /// The flock is a membership list, not a list of live processes: `stop`
    /// leaves a sheep registered and `Stopped`, `delete` ends membership.
    ///
    /// One entry per app rather than one per configured instance, at
    /// `instance: 0`, the slot `start` fills first, so a later `restart` lands
    /// where it would have. Idempotent by name.
    fn register_at_rest(&mut self, app: &ResolvedApp) -> ProcessInfo {
        self.register_without_spawning(app, ProcStatus::Stopped, None)
            .into_info()
    }

    /// The field names an operator has overridden for `name`, for a
    /// [`ProcessEntry`] about to be built from scratch.
    ///
    /// Checks a live sibling first and reads the override store only when none
    /// exists, so a scale-up costs no file access. An unreadable store answers
    /// empty rather than refusing: the worst case is a blank CFG cell.
    fn overridden_for(&self, name: &str) -> Vec<String> {
        if let Some(slot) = self
            .sheep
            .values()
            .find(|slot| slot.entry.spec.config().name == name)
        {
            return slot.entry.overridden.clone();
        }
        overrides::get(&self.paths.overrides, name)
            .ok()
            .flatten()
            .map(|record| record.fields.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Registers one app as a flock member in `status`, spawning nothing.
    ///
    /// Registers the status it is handed: [`Self::register_at_rest`] passes
    /// `Stopped`, [`Self::do_start`] passes `Errored` when credential
    /// resolution fails under [`BatchPolicy::PerApp`].
    ///
    /// The entry's identity is [`SpawnIdentity::Unresolved`] whatever the
    /// status, so a later `restart` resolves it through
    /// [`Self::credentials_for_spawn`] instead of reusing a settled `None` and
    /// starting the sheep as the shepherd. Idempotent by name: an app already
    /// known is left as it is, and [`Registration`] says which happened.
    fn register_without_spawning(
        &mut self,
        app: &ResolvedApp,
        status: ProcStatus,
        dog: Option<DogSource>,
    ) -> Registration {
        let name = &app.config().name;
        if let Some(slot) = self
            .sheep
            .values()
            .find(|slot| &slot.entry.spec.config().name == name)
        {
            return Registration::AlreadyKnown(to_info(&slot.entry, &self.smits));
        }

        let overridden = self.overridden_for(name);
        let id = self.next_id;
        self.next_id += 1;
        // Assembled for its log paths only: nothing is spawned, but the entry
        // has to name the files a later `restart` will append to.
        let spec = assemble(app, 0, &self.paths, None);
        let entry = ProcessEntry {
            id,
            spec: app.clone(),
            pending: None,
            pending_reidentifies: false,
            overridden,
            instance: 0,
            status,
            pid: None,
            restarts: 0,
            started_at: None,
            budget: RestartBudget::default(),
            reload: ReloadState::None,
            // `respawn` resolves it, so a restored app comes up under its
            // configured `user`.
            credentials: SpawnIdentity::Unresolved,
            out_file: spec.out_file.clone(),
            err_file: spec.err_file.clone(),
            dog,
            last_exit: None,
        };
        let info = to_info(&entry, &self.smits);
        self.sheep.insert(
            id,
            SheepSlot {
                entry,
                ctl: None,
                log_ctl: None,
                to_child: None,
                signals: None,
                to_stdin: None,
                manual: None,
                pending_delete: false,
                epoch: 0,
                ready_tx: None,
                actions: ActionWaits::default(),
                ready_failed: false,
                restart_due: None,
            },
        );
        Registration::Fresh(info)
    }

    /// Names the fields in which each app differs from the registered sheep
    /// of the same name, skipping every app that matches and every app the
    /// flock does not have.
    ///
    /// Several instances of one app share one config, so the first slot found
    /// under a name answers for all of them.
    fn config_drift(&self, apps: &[ResolvedApp]) -> Vec<SheepDrift> {
        apps.iter()
            .filter_map(|app| {
                let incoming = app.config();
                let stored = self
                    .sheep
                    .values()
                    .find(|slot| slot.entry.spec.config().name == incoming.name)?
                    .entry
                    .spec
                    .config();
                let fields = stored.drifted_fields(incoming);
                (!fields.is_empty()).then(|| SheepDrift::new(&incoming.name, fields))
            })
            .collect()
    }

    /// Registers + spawns one brand-new instance (a fresh id, `restarts: 0`).
    ///
    /// Always inserts a [`SheepSlot`] before returning: on success `Starting`
    /// with a readiness task armed when the app configures `wait_ready` or
    /// `readiness_probe` and `Online` otherwise, `Errored` with no task on
    /// failure. `dog` lands on the entry either way, so a dog whose binary
    /// cannot be spawned still shows up in the dogs table.
    fn spawn_fresh(
        &mut self,
        app: &ResolvedApp,
        instance: u32,
        credentials: Option<Credentials>,
        dog: Option<DogSource>,
    ) -> Result<ProcessInfo, String> {
        // Read before the spawn: a scale-up's new instance must show the same
        // overrides its siblings do, not a blank cell until the next load.
        let overridden = self.overridden_for(&app.config().name);
        let spec = assemble(app, instance, &self.paths, credentials);
        let id = self.next_id;
        self.next_id += 1;

        // Cloned off the spec, which is the only place that knows whether the
        // app set an explicit `out_file`/`err_file` or takes the `merge_logs`
        // default. Both arms below register an entry.
        let out_file = spec.out_file.clone();
        let err_file = spec.err_file.clone();

        // A `ResolvedApp` has already been through `ProbeTarget::parse` in
        // `normalize`, so an `Err` here means an app skipped that step.
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
                    pending: None,
                    pending_reidentifies: false,
                    overridden,
                    instance,
                    status,
                    pid: Some(pid),
                    restarts: 0,
                    started_at: Some(tokio::time::Instant::now()),
                    budget: RestartBudget::default(),
                    reload: ReloadState::None,
                    credentials: SpawnIdentity::Resolved(credentials),
                    out_file,
                    err_file,
                    dog,
                    last_exit: None,
                };
                let info = to_info(&entry, &self.smits);
                let log_ctl = io.log_ctl.clone();
                let to_child = io.to_child.clone();
                let to_stdin = io.to_stdin.clone();
                let handles = spawn_sheep_task::<R::Proc>(
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
                        // A `Start` is always a caller's own doing, matching
                        // the `manually: true` the ungated arm below emits.
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
                        ctl: Some(handles.ctl),
                        log_ctl: Some(log_ctl),
                        to_child: Some(to_child),
                        signals: Some(handles.signals),
                        to_stdin: Some(to_stdin),
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
                        ready_tx,
                        actions: ActionWaits::default(),
                        ready_failed: false,
                        restart_due: None,
                    },
                );
                self.emit(ProcessEventKind::Start, info.clone(), true);
                // A gated app goes `Online` later, from `handle_ready_result`.
                // `Start` is the bus's first word on this sheep either way.
                if !gated {
                    self.went_online(id, info.clone(), true);
                }
                Ok(info)
            }
            Err(error) => {
                let entry = ProcessEntry {
                    id,
                    spec: app.clone(),
                    pending: None,
                    pending_reidentifies: false,
                    overridden,
                    instance,
                    status: ProcStatus::Errored,
                    pid: None,
                    restarts: 0,
                    started_at: None,
                    budget: RestartBudget::default(),
                    reload: ReloadState::None,
                    credentials: SpawnIdentity::Resolved(credentials),
                    out_file,
                    err_file,
                    dog,
                    last_exit: None,
                };
                let info = to_info(&entry, &self.smits);
                self.sheep.insert(
                    id,
                    SheepSlot {
                        entry,
                        ctl: None,
                        log_ctl: None,
                        to_child: None,
                        signals: None,
                        to_stdin: None,
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
                        ready_tx: None,
                        actions: ActionWaits::default(),
                        ready_failed: false,
                        restart_due: None,
                    },
                );
                self.emit(ProcessEventKind::Errored, info, true);
                // `error` names neither the app nor the path, and the caller
                // adds the name. `spec.program` and `spec.cwd` verbatim: they
                // are what the Flockfile said. A `Doubtful` verdict replaces
                // the clause, being its only channel to an operator.
                let attempted = match self.runner.preflight(&spec) {
                    Preflight::Doubtful(reason) => reason,
                    _ => match &spec.cwd {
                        Some(cwd) => format!("tried `{}` in {}", spec.program, cwd.display()),
                        None => format!("tried `{}`", spec.program),
                    },
                };
                Err(format!("{error}; {attempted}"))
            }
        }
    }

    /// Installs one sheep this image inherited rather than started.
    ///
    /// Nothing here spawns, signals or reopens anything, and nothing is
    /// emitted on the bus: the sheep never transitioned. `started_at` cannot
    /// cross the handover, since a `tokio::time::Instant` means nothing
    /// outside the runtime that read it, so it is re-derived via
    /// [`handover::uptime`](crate::handover::uptime).
    ///
    /// # Errors
    ///
    /// - [`AdoptError::Spec`] if the carried config does not normalize.
    /// - [`AdoptError::Runner`] if the runner refused the inherited handles.
    #[cfg(unix)]
    fn install_adopted(
        &mut self,
        sheep: AdoptedSheep,
        reaper: &Arc<AdoptedReaper>,
    ) -> Result<(), AdoptError> {
        let AdoptedSheep {
            carried,
            out_pipe,
            err_pipe,
            out_log,
            err_log,
            stdin_pipe,
            channel,
        } = sheep;
        let app = normalize(carried.app().clone()).map_err(|source| AdoptError::Spec {
            sheep: carried.name().to_string(),
            source,
        })?;
        // Reused as resolved, never re-resolved: the value was pinned at the
        // predecessor's first spawn, so a passwd change cannot move a running
        // app's identity.
        let credentials = match carried.credentials() {
            SpawnIdentity::Resolved(credentials) => credentials,
            SpawnIdentity::Unresolved => None,
        };
        // Assembled for its log paths only: the entry has to name the files
        // this sheep is writing to, and a later rotation reopens them by path.
        let spec = assemble(&app, carried.instance(), &self.paths, credentials);
        let id = carried.id();
        let status = carried.status();
        // `None` for a blob written before this daemon carried a swap; see
        // `CarriedSheep::reload`.
        let reload = carried.reload().unwrap_or(ReloadState::None);
        // Read here rather than at either `SheepSlot` literal below: the
        // readiness re-arm between them has to see it, since a `ready_failed`
        // instance is `Starting` by construction and must not get a wait.
        let ready_failed = carried.ready_failed().unwrap_or(false);
        // Restored as a pair with its reset flag: a parked config promoted
        // without it comes up on the identity the flag exists to replace.
        // A parked config that fails to normalize is dropped and warned about
        // rather than refusing the adoption, which would strand a live flock.
        let pending = carried
            .pending()
            .and_then(|parked| match normalize(parked.clone()) {
                Ok(app) => Some(app),
                Err(source) => {
                    tracing::warn!(
                        sheep = carried.name(),
                        %source,
                        "a config this sheep was owed did not survive the handover: this daemon \
                         will not accept it, so the change is gone and the file must be loaded \
                         again"
                    );
                    None
                }
            });
        // `false` when the config was dropped just above, and for a blob
        // written before the flag existed: nothing left to promote.
        let pending_reidentifies =
            pending.is_some() && carried.pending_reidentifies().unwrap_or(false);
        let mut entry = ProcessEntry {
            id,
            spec: app.clone(),
            pending,
            pending_reidentifies,
            instance: carried.instance(),
            status,
            pid: carried.pid(),
            restarts: carried.restarts(),
            // Filled in below for a sheep with a process, and left `None` for
            // one without, which is what a stopped slot carries anyway.
            started_at: None,
            // The count is carried, since losing it would hand a crash-looping
            // app amnesty, but the window it is counted over is wall-clock this
            // image did not observe.
            budget: RestartBudget::default(),
            // Restored: it decides where this instance's next exit goes.
            // `handle_exited` routes a `Drainee` to `reap_drainee` and reads a
            // `Replacement` out of the swap, while `None` takes
            // `decide_on_exit`.
            reload,
            credentials: carried.credentials(),
            out_file: spec.out_file.clone(),
            err_file: spec.err_file.clone(),
            // Restored: it is the marker that keeps a dog out of the flock.
            // `matching_ids` passes a marked entry over for every selector but
            // an exact one, so dropping it would put the dog in `shep flock`.
            // `None` for a blob written before this daemon carried a dog.
            dog: carried.dog().cloned(),
            last_exit: carried.last_exit(),
            // Not carried by `CarriedSheep`: a handover installs one sheep at a
            // time with no live sibling to ask, so this is one store read.
            overridden: self.overridden_for(&app.config().name),
        };

        let Some(pid) = carried.pid() else {
            // Registered and not running: adopting an absent process would ask
            // the reaper to wait on a pid this image never had.
            self.sheep.insert(
                id,
                SheepSlot {
                    entry,
                    ctl: None,
                    log_ctl: None,
                    to_child: None,
                    signals: None,
                    to_stdin: None,
                    // A marker is only claimed against a sheep with a live
                    // task: there is no ladder here to re-arm and no
                    // `Msg::Exited` coming to clear one.
                    manual: None,
                    // Restored: it needs no task to act on it, and the exit
                    // that consumes it is this slot's next spawn's.
                    pending_delete: carried.pending_delete().unwrap_or(false),
                    epoch: carried.epoch(),
                    ready_tx: None,
                    actions: ActionWaits::default(),
                    // Restored: it needs no task to act on it, and `respawn`
                    // clears it at the spawn that answers it.
                    ready_failed,
                    // Restored verbatim so a second reload during the same
                    // wait does not start the delay over: the re-arm below
                    // computes a fresh timer from this absolute moment.
                    restart_due: carried.restart_due(),
                },
            );
            // Nothing but this raises `Msg::RestartDue`, so a carried
            // `WaitingRestart` sheep would sit there for the daemon's life.
            // The timer is re-armed off the carried deadline, so the successor
            // sleeps out what is left of `restart_delay`.
            if status == ProcStatus::WaitingRestart {
                let delay = crate::backoff::adopted_restart_delay(
                    app.config(),
                    carried.restart_due(),
                    SystemTime::now(),
                );
                self.schedule_restart(id, carried.epoch(), delay);
            }
            return Ok(());
        };

        let (proc, io) = self
            .runner
            .adopt(AdoptSpec {
                pid,
                out_file: spec.out_file.clone(),
                err_file: spec.err_file.clone(),
                out_pipe,
                err_pipe,
                out_log,
                err_log,
                stdin_pipe,
                channel,
                reaper: Arc::clone(reaper),
            })
            .map_err(|source| AdoptError::Runner {
                sheep: carried.name().to_string(),
                source,
            })?;
        // Off the proc rather than off the blob, as both spawn paths take it:
        // the pid this entry reports has to be the one the runner will signal.
        entry.pid = Some(proc.pid());
        // Load-bearing: `handle_exited` reads `started_at` to tell a real exit
        // from a duplicate `Msg::Exited`, and an entry without it would sit
        // `Online` forever after its process died. Derived from the operating
        // system, since a `tokio::time::Instant` cannot cross the exec.
        entry.started_at = Some(crate::handover::uptime::started_at_of(proc.pid()));
        // `manually` reaches only the `Online` event's flag: `false` for a
        // `Starting` sheep this image did not spawn, `true` for a carried
        // `Replacement`, which is an operator's reload.
        let manually = matches!(reload, ReloadState::Replacement);
        // A carried `Starting` sheep has readiness unresolved and nothing else
        // moves it off, so `listen_timeout` starts again. `ready_failed` is the
        // exception: its verdict already stands, and a fresh wait would clear
        // the flag a rollback needs.
        let ready_tx = (status == ProcStatus::Starting && !ready_failed).then(|| {
            let source = ReadinessSource::of(app.config())
                .expect("ResolvedApp already passed ProbeTarget::parse in normalize");
            spawn_readiness_task(
                id,
                carried.epoch(),
                manually,
                source,
                app.config().listen_timeout.as_duration(),
                spec_prober(&spec),
                self.tx.clone(),
            )
        });
        let log_ctl = io.log_ctl.clone();
        let to_child = io.to_child.clone();
        let to_stdin = io.to_stdin.clone();
        let handles = spawn_sheep_task::<R::Proc>(
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
                ctl: Some(handles.ctl),
                log_ctl: Some(log_ctl),
                to_child: Some(to_child),
                signals: Some(handles.signals),
                to_stdin: Some(to_stdin),
                // Re-claimed below rather than written in directly: restoring
                // the marker is only half of what a carried one means.
                manual: None,
                pending_delete: carried.pending_delete().unwrap_or(false),
                epoch: carried.epoch(),
                ready_tx,
                actions: ActionWaits::default(),
                // Restored: `reload_eligible` reads it beside the status, so a
                // rollback reload can replace an instance that never reached
                // `Online`.
                ready_failed,
                // Restored verbatim, though always `None` in practice: a sheep
                // with a pid is not `WaitingRestart`.
                restart_due: carried.restart_due(),
            },
        );
        // The ladder that would kill a carried `manual` sheep went with the
        // predecessor's image, so it runs again from the polite rung. The cap
        // comes off the role: a carried drainee was asked under
        // `graceful_timeout`, the rest under `kill_timeout`.
        if let Some(manual) = carried.manual() {
            let cap = match reload {
                ReloadState::Drainee { .. } => LadderCap::Drain,
                ReloadState::None | ReloadState::Replacement => LadderCap::Stop,
            };
            self.claim_manual(id, manual, cap);
        }
        // A watch, a schedule or a memory limit this image did not arm is one
        // the app quietly stops having.
        if status == ProcStatus::Online {
            self.arm_extras(id);
        }
        Ok(())
    }

    /// Restores the reload jobs the blob carried, and re-arms every timer
    /// each of them was waiting on.
    ///
    /// Runs after the whole flock is installed, not per sheep: a job names two
    /// entries and reads an app's timings off one of them.
    ///
    /// [`Self::arm_reload_deadline`] is the only thing that removes a
    /// [`ReloadJob`] nothing else can finish, and its stamp comes off the
    /// carried `next_deadline`. A swap in [`ReloadPhase::Verify`] is re-armed
    /// through the same [`Self::post_drain_probe`] the original went through.
    /// A job whose swap names no registered entry is dropped: arming against a
    /// missing entry would panic.
    #[cfg(unix)]
    fn install_carried_reloads(&mut self, reloads: Vec<CarriedReload>) {
        for carried in reloads {
            let CarriedReload {
                app,
                queue,
                mode,
                swap,
            } = carried;
            // The replacement first, as it is the half that exists in every
            // phase but `DrainFirst`. This is about which one is registered: a
            // serial reload deregisters its drainee at `ReapOld`.
            let anchor = swap
                .new_id
                .filter(|id| self.sheep.contains_key(id))
                .or_else(|| self.sheep.contains_key(&swap.old_id).then_some(swap.old_id));
            let Some(anchor) = anchor else {
                tracing::warn!(
                    name = app,
                    old_id = swap.old_id,
                    new_id = swap.new_id,
                    "a carried reload named no instance this shepherd was given, so it was \
                     dropped rather than left unfinishable"
                );
                continue;
            };
            self.reloads.insert(
                app.clone(),
                ReloadJob {
                    queue: queue.into_iter().collect(),
                    mode,
                    swap,
                    // Overwritten by the arm below, the one site that stamps a
                    // live watchdog.
                    deadline: 0,
                },
            );
            self.arm_reload_deadline(&app, anchor);
            if swap.phase == ReloadPhase::Verify
                && let Some(new_id) = swap.new_id
                && let Some(source) = self.post_drain_probe(new_id, mode)
            {
                self.spawn_verify_task(&app, new_id, source);
            }
        }
    }

    /// The config `id`'s next child will be spawned from: what a load parked
    /// for it, or its stored spec when nothing is parked.
    ///
    /// The one reader of [`ProcessEntry::pending`] that does not consume it. A
    /// reload decides readiness source, ordering, identity and the assembled
    /// config off this one accessor: deciding one of those from `spec` and
    /// another from `pending` puts two probe-gated instances on one address.
    fn intended_spec(&self, id: u32) -> Option<&ResolvedApp> {
        let slot = self.sheep.get(&id)?;
        Some(slot.entry.pending.as_ref().unwrap_or(&slot.entry.spec))
    }

    /// The identity `id`'s next child runs under, resolving it if the config
    /// it is about to be spawned from asks for a different one.
    ///
    /// Reads and never writes, so a caller that may not go through with the
    /// spawn does not leave the entry claiming an identity it never used.
    /// [`Self::credentials_for_spawn`] is the writing twin.
    ///
    /// # Errors
    ///
    /// - Whatever [`privilege::resolve`] refused the intended config's
    ///   `user`/`group`.
    fn intended_credentials(&self, id: u32) -> Result<Option<Credentials>, PrivilegeError> {
        // `expect` rather than an `Ok(None)` fallback: `None` here means "the
        // app asked for nobody", so a missing slot would spawn as the
        // shepherd.
        let slot = self
            .sheep
            .get(&id)
            .expect("intended_credentials: the instance was found replaceable a moment ago");
        match slot.entry.credentials {
            SpawnIdentity::Resolved(credentials) if !slot.entry.pending_reidentifies => {
                Ok(credentials)
            }
            SpawnIdentity::Resolved(_) | SpawnIdentity::Unresolved => privilege::resolve(
                self.intended_spec(id)
                    .expect("intended_credentials: the slot was read a moment ago")
                    .config(),
            ),
        }
    }

    /// Moves a sheep's pending config onto its stored spec, if it has any.
    ///
    /// Called where a child is about to be replaced under the same entry, the
    /// only moment an [`ApplyGroup::NeedsRespawn`] field can take effect
    /// without a new id. A reload's replacement is a different entry, so it
    /// reads through [`Self::intended_spec`] and leaves the drainee's parked
    /// copy alone. Neither path re-reads a file.
    fn promote_pending(&mut self, id: u32) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        let Some(pending) = slot.entry.pending.take() else {
            return;
        };

        // The one exception to `ProcessEntry::credentials` being resolved once:
        // an operator who edited `user` or `group` asked for exactly that. The
        // decision is read here, never made here, since the spec may have been
        // rewritten by a later load from a sibling that already promoted.
        if core::mem::take(&mut slot.entry.pending_reidentifies) {
            slot.entry.credentials = SpawnIdentity::Unresolved;
        }
        slot.entry.spec = pending;
    }

    /// Respawns an already-registered id in place: reassembles from its stored
    /// spec + instance, bumps `restarts` and resets timing on success, or marks
    /// the entry `Errored` on failure.
    ///
    /// `manually` is narrower than "forced": `true` only for an operator's
    /// `Restart`, `false` for the crash loop and every restart the daemon
    /// raised itself. Callers pass `origin == CommandOrigin::Operator`.
    fn respawn(&mut self, id: u32, manually: bool) -> ProcessInfo {
        // Before the identity below is read, since a promoted `user` or `group`
        // is what clears it. Here rather than in the `Restart` arms: every door
        // a replacement child comes through passes through this one.
        self.promote_pending(id);
        // Reused as-is once resolved: a restart must never re-touch the passwd
        // database, nor silently change identity under a running app. An entry
        // never resolved resolves here, since coming up as the shepherd would
        // be an unreported privilege downgrade.
        let credentials = match self.credentials_for_spawn(id) {
            Ok(credentials) => credentials,
            Err(err) => return self.respawn_failed(id, manually, &err),
        };
        let slot = self.sheep.get(&id).expect("respawn: unknown id");
        let app = slot.entry.spec.clone();
        let instance = slot.entry.instance;
        // Computed ahead of the mutable borrow below.
        let next_epoch = slot.epoch + 1;
        let spec = assemble(&app, instance, &self.paths, credentials);
        let source = ReadinessSource::of(app.config())
            .expect("ResolvedApp already passed ProbeTarget::parse in normalize");
        let gated = !matches!(source, ReadinessSource::Heuristic);

        match self.runner.spawn(&spec) {
            Ok((proc, io)) => {
                let pid = proc.pid();
                let log_ctl = io.log_ctl.clone();
                let to_child = io.to_child.clone();
                let to_stdin = io.to_stdin.clone();
                let handles = spawn_sheep_task::<R::Proc>(
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
                        // Carried, not defaulted: a gated app must report who
                        // caused the respawn as the ungated arm below does.
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
                // A different process under the same id, so an earlier reload's
                // verdict about the last one does not apply to it.
                slot.ready_failed = false;
                slot.ctl = Some(handles.ctl);
                slot.log_ctl = Some(log_ctl);
                slot.to_child = Some(to_child);
                slot.signals = Some(handles.signals);
                slot.to_stdin = Some(to_stdin);
                // A new process under this id makes any RestartDue timer or
                // readiness task scheduled earlier stale. An old readiness task
                // finds its sender gone, rides out its deadline, and its
                // `ReadyResult` is dropped by `handle_ready_result`'s epoch check.
                slot.epoch += 1;
                debug_assert_eq!(slot.epoch, next_epoch);
                slot.ready_tx = ready_tx;
                let info = to_info(&slot.entry, &self.smits);
                self.emit(ProcessEventKind::Restart, info.clone(), manually);
                // A gated app goes `Online` later, from `handle_ready_result`.
                if !gated {
                    self.went_online(id, info.clone(), manually);
                }
                info
            }
            Err(error) => self.respawn_failed(id, manually, &error),
        }
    }

    /// Lands `id` in the terminal state a respawn that could not start
    /// reaches: `Errored`, every handle cleared, its lifecycle extras
    /// disarmed.
    ///
    /// `reason` is logged here rather than by the caller. The `Errored` event
    /// carries no reason and the deferred aggregation reply has no per-id error
    /// slot, so this log line is the only place an operator learns why a
    /// restart produced no process.
    fn respawn_failed(
        &mut self,
        id: u32,
        manually: bool,
        reason: &dyn fmt::Display,
    ) -> ProcessInfo {
        tracing::warn!(id, %reason, "restart did not start a process");
        let slot = self
            .sheep
            .get_mut(&id)
            .expect("respawn: entry vanished mid-respawn");
        slot.entry.status = ProcStatus::Errored;
        slot.entry.pid = None;
        slot.entry.started_at = None;
        // Cleared, because nothing exited here: leaving the previous process's
        // code would show a sheep that once crashed with 1 as still crashing
        // with 1 while it is failing to start at all.
        slot.entry.last_exit = None;
        slot.ctl = None;
        // Already `None` on every route into a respawn, written anyway so that
        // "these two go together" is visible at each site.
        slot.to_child = None;
        slot.signals = None;
        slot.to_stdin = None;
        slot.ready_tx = None;
        let info = to_info(&slot.entry, &self.smits);
        self.emit(ProcessEventKind::Errored, info.clone(), manually);
        // The same terminal status `Decision::Errored` reaches, and it needs
        // the same disarm: otherwise the name-group's cron worker and watch
        // stay live, and the enforcer stays armed against a dead pid.
        self.disarm_extras(id, &info.name);
        info
    }

    /// Offers `manual` the `manual` marker on a running sheep, starting its
    /// kill ladder if nothing else already has.
    ///
    /// The first manual command to reach a running sheep owns its marker and
    /// its one live `Kill`. A later command racing the same in-flight kill
    /// rides the same eventual `Msg::Exited`, so a `stop()` caller is never
    /// handed back an `Online` `ProcessInfo`. One carve-out: an operator's
    /// command takes the marker off an in-flight automatic restart, which has
    /// nobody waiting behind it.
    ///
    /// `cap` decides how long the ladder this may start waits before `SIGKILL`,
    /// and is read only on the arm that sends a `Kill`.
    fn claim_manual(&mut self, id: u32, manual: PendingManual, cap: LadderCap) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        match slot.manual.map(|in_flight| in_flight.origin) {
            // Nothing has claimed this sheep's next exit yet, so this command
            // owns it and starts the one kill ladder that produces it.
            None => {
                slot.manual = Some(manual);
                // `try_send`, never `.await`: the sheep task stops draining its
                // ctl mailbox once the ladder starts, so a blocking send could
                // park the actor for `kill_timeout`. `Full` means a `Kill` is
                // already queued; `Closed` means the sheep already exited.
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
            // command cannot displace: ride that one's outcome. Both variants
            // named so a third origin has to be ruled on here.
            Some(CommandOrigin::Operator | CommandOrigin::Automatic) => {}
        }
    }

    /// Every registered id `selector` names, in id order.
    ///
    /// The one place selection happens. A dog is included only for a selector
    /// that named it ([`ProcessSelector::is_exact`]), so `stop all`, `reload
    /// all`, `delete all` and a `/regex/` sweep pass every dog by while `shep
    /// restart bark` still reaches one.
    fn matching_ids(&self, selector: &ProcessSelector) -> Vec<u32> {
        let exact = selector.is_exact();
        let mut ids: Vec<u32> = self
            .sheep
            .iter()
            .filter(|(_, slot)| exact || slot.entry.dog.is_none())
            .filter_map(|(id, slot)| {
                let config = slot.entry.spec.config();
                selector
                    .matches(
                        &config.name,
                        *id,
                        config.fold.as_deref(),
                        Some(slot.entry.instance),
                    )
                    .then_some(*id)
            })
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Resolves `selector`, then either defers to each matched sheep's next
    /// exit (if running) or applies the command immediately (if not).
    ///
    /// Immediate results are collected up front and folded into the same
    /// `PendingReply` as the deferred ids, so a mixed selector's reply carries
    /// every match and fires once the last running one goes terminal.
    fn begin_manual(
        &mut self,
        selector: ProcessSelector,
        kind: ManualKind,
        origin: CommandOrigin,
        reply: ReplyKind,
    ) {
        let matched = self.matching_ids(&selector);

        if matched.is_empty() {
            send_reply(reply, Err(SupervisorError::NotFound));
            return;
        }

        self.begin_manual_ids(matched, kind, origin, reply);
    }

    /// [`Self::begin_manual`]'s per-id aggregation, taking the matched ids
    /// directly rather than resolving a [`ProcessSelector`] into them.
    ///
    /// The seam [`Self::handle_scale`]'s scale-down needs: the ids it is
    /// deregistering are the highest instance slots of one already-resolved
    /// app, not a fresh selector match.
    fn begin_manual_ids(
        &mut self,
        matched: Vec<u32>,
        kind: ManualKind,
        origin: CommandOrigin,
        reply: ReplyKind,
    ) {
        let mut remaining = HashSet::new();
        let mut results = Vec::new();

        for id in matched {
            // An automatic restart is held off both halves of a swap that has
            // not committed: killing either half turns the deploy into the hard
            // restart the overlap exists to avoid. This drops the restart, it
            // does not defer it, and it stops at the commit.
            let held_off_by_a_swap =
                origin == CommandOrigin::Automatic && self.in_an_uncommitted_swap(id);
            if held_off_by_a_swap {
                // A dropped save for a watched app has nothing else an operator
                // can read.
                tracing::debug!(
                    id,
                    ?kind,
                    "automatic command dropped: this sheep is half of a swap that has not \
                     committed"
                );
                continue;
            }
            let is_running = self.sheep.get(&id).is_some_and(|slot| slot.ctl.is_some());
            if is_running {
                // Whoever ends up owning the marker, this id joins `remaining`
                // and this command is answered off the same `Msg::Exited`.
                self.claim_manual(id, PendingManual { kind, origin }, LadderCap::Stop);
                if kind == ManualKind::Delete {
                    // Whichever command's `manual` marker won, this id must
                    // still be deregistered once it goes terminal.
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
            sort_flock(&mut results);
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
    /// `handle_exited`'s branches cannot cover these: a sheep waiting out its
    /// restart backoff still holds every extra its last `Online` armed, and its
    /// exit already happened, so stopping or deleting it here is the moment it
    /// goes terminal. Without it, `shep stop web` during a backoff leaves the
    /// group's watcher and cron worker armed.
    ///
    /// `origin` feeds the `manually` flag on the events below: a cron
    /// occurrence landing on a mid-backoff name restarts it from here.
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
                    // `WaitingRestart`: cancels the pending restart, since
                    // `handle_restart_due` only respawns an id still in
                    // `WaitingRestart` on a still-current epoch. `Errored`:
                    // `stop` lands it in `Stopped` rather than a no-op.
                    ProcStatus::WaitingRestart | ProcStatus::Errored => {
                        slot.entry.status = ProcStatus::Stopped;
                        let info = to_info(&slot.entry, &self.smits);
                        self.emit(ProcessEventKind::Stop, info.clone(), manually);
                        self.disarm_extras(id, &info.name);
                        Some(info)
                    }
                    _ => Some(to_info(&slot.entry, &self.smits)),
                }
            }
            ManualKind::Delete => {
                let slot = self.sheep.remove(&id)?;
                let info = to_info(&slot.entry, &self.smits);
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

    /// Sets `name`'s instance count to `count`.
    ///
    /// Scaling up, [`instance_slots`] hands out the lowest free slots, exactly
    /// as a `Start` does. Scaling down, the highest-numbered slots are
    /// deregistered first, the same thing `Delete` does: a `Stop` would leave
    /// them holding their slots. The reply does not wait for the departures,
    /// which report themselves on the bus as `process.delete`.
    ///
    /// Refused while departures are in flight, since a departing instance stays
    /// registered and [`SheepSlot::pending_delete`] until its exit lands. That
    /// reaches the operator as [`SupervisorError::InvalidScale`].
    fn handle_scale(
        &mut self,
        name: &str,
        count: u32,
        reply: oneshot::Sender<Result<Scaled, SupervisorError>>,
    ) {
        let mut slots: Vec<(u32, u32)> = self
            .sheep
            .iter()
            .filter(|(_, slot)| slot.entry.spec.config().name == name)
            .map(|(id, slot)| (slot.entry.instance, *id))
            .collect();
        if slots.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }
        slots.sort_unstable();

        if count == 0 {
            let _ = reply.send(Err(SupervisorError::InvalidScale(format!(
                "an app runs at least one instance; use `shep delete {name}` to remove it"
            ))));
            return;
        }
        if self
            .sheep
            .get(&slots[0].1)
            .is_some_and(|slot| slot.entry.dog.is_some())
        {
            let _ = reply.send(Err(SupervisorError::InvalidScale(format!(
                "{name} is a dog, and a dog runs one process"
            ))));
            return;
        }
        if self.reloads.contains_key(name) {
            let _ = reply.send(Err(SupervisorError::ReloadInFlight(name.to_string())));
            return;
        }
        // Counted rather than merely detected: the number tells the operator
        // how much of the flock is still moving.
        let leaving = slots
            .iter()
            .filter(|(_, id)| self.sheep.get(id).is_some_and(|slot| slot.pending_delete))
            .count();
        if leaving > 0 {
            let _ = reply.send(Err(SupervisorError::InvalidScale(format!(
                "{name} has {leaving} instance(s) still shutting down from an \
                 earlier command; wait for them to leave `shep flock` and scale \
                 again"
            ))));
            return;
        }

        // Re-normalized rather than mutated in place: holding a `ResolvedApp`
        // proves it passed `normalize`, and editing the field behind that door
        // would be the one place in the tree holding one that had not.
        let mut config = self
            .sheep
            .get(&slots[0].1)
            .expect("handle_scale: id read off this map a moment ago")
            .entry
            .spec
            .config()
            .clone();
        config.instances = count;
        let rescaled = match normalize(config) {
            Ok(app) => app,
            Err(err) => {
                let _ = reply.send(Err(SupervisorError::InvalidScale(err.to_string())));
                return;
            }
        };

        let current = u32::try_from(slots.len()).unwrap_or(u32::MAX);
        // The spawn/remove pass runs first and the config write-back second:
        // writing `rescaled` onto every slot up front and then failing a spawn
        // leaves every survivor claiming `instances = 4` in a flock of three.
        let mut failure = None;
        // The one slot `spawn_fresh` registers on a failed attempt. Kept out of
        // `survivors`, since it is not a running instance, but the config
        // write-back below still has to reach it.
        let mut orphaned_by_failed_spawn = None;
        let survivors: Vec<u32> = match count.cmp(&current) {
            Ordering::Equal => slots.iter().map(|(_, id)| *id).collect(),
            Ordering::Greater => {
                // Inside this arm, since it is the only one that spawns:
                // resolving an identity for `Equal` or `Less` meant a lookup
                // that could refuse a call using no credentials at all.
                // `CannotStart`: nothing has been spawned or removed yet.
                let credentials = match self.credentials_for_spawn(slots[0].1) {
                    Ok(credentials) => credentials,
                    Err(err) => {
                        let _ =
                            reply.send(Err(SupervisorError::CannotStart(format!("{name}: {err}"))));
                        return;
                    }
                };
                let existing: Vec<u32> = slots.iter().map(|(instance, _)| *instance).collect();
                let mut ids: Vec<u32> = slots.iter().map(|(_, id)| *id).collect();
                for instance in instance_slots(&existing, count - current) {
                    let attempted_id = self.next_id;
                    match self.spawn_fresh(&rescaled, instance, credentials, None) {
                        Ok(info) => ids.push(info.id),
                        Err(message) => {
                            // Partial, and said so: the instances already
                            // spawned are serving real traffic, and unwinding
                            // them would turn one failed spawn into an outage.
                            orphaned_by_failed_spawn = Some(attempted_id);
                            failure = Some(message);
                            break;
                        }
                    }
                }
                ids
            }
            Ordering::Less => {
                let cut = usize::try_from(count).unwrap_or(usize::MAX);
                let (keep, remove) = slots.split_at(cut);
                let removed: Vec<u32> = remove.iter().map(|(_, id)| *id).collect();
                self.begin_manual_ids(
                    removed,
                    ManualKind::Delete,
                    CommandOrigin::Operator,
                    // The removals' own terminal snapshots go nowhere: this
                    // reply is the survivors.
                    ReplyKind::Ids(oneshot::channel().0),
                );
                keep.iter().map(|(_, id)| *id).collect()
            }
        };

        // The count actually achieved: `count` on every path but a partial
        // scale-up. Re-normalized rather than assigned, for the same reason
        // `rescaled` was: a `ResolvedApp` is a proof token.
        let achieved = u32::try_from(survivors.len()).unwrap_or(u32::MAX);
        let stored = if achieved == count {
            rescaled
        } else {
            let mut config = rescaled.config().clone();
            config.instances = achieved;
            match normalize(config) {
                Ok(app) => app,
                Err(err) => {
                    let _ = reply.send(Err(SupervisorError::InvalidScale(err.to_string())));
                    return;
                }
            }
        };

        // The parked config travels onto the slots this call created, which
        // `spawn_fresh` registers with `pending: None`; the loop below also
        // writes `stored` onto the `Errored` slot a failed spawn left, which a
        // later `handle_scale` counts. `with_count` keeps the count agreeing.
        let owed = self.sheep.get(&slots[0].1).and_then(|slot| {
            let parked = slot.entry.pending.clone()?;
            let parked = with_count(&parked, achieved).unwrap_or(parked);
            Some((parked, slot.entry.pending_reidentifies))
        });
        for id in survivors.iter().chain(orphaned_by_failed_spawn.iter()) {
            if let Some(slot) = self.sheep.get_mut(id) {
                slot.entry.spec = stored.clone();
                match &mut slot.entry.pending {
                    // A slot already owed a config keeps it, with the count
                    // brought forward: a parked count of 2 against a spec of 4
                    // is drift that never clears.
                    Some(parked) => {
                        if let Some(recounted) = with_count(parked, achieved) {
                            *parked = recounted;
                        }
                    }
                    None => {
                        if let Some((parked, reidentifies)) = &owed {
                            slot.entry.pending = Some(parked.clone());
                            slot.entry.pending_reidentifies = *reidentifies;
                        }
                    }
                }
            }
        }

        let mut instances: Vec<ProcessInfo> = survivors
            .iter()
            .filter_map(|id| {
                self.sheep
                    .get(id)
                    .map(|slot| to_info(&slot.entry, &self.smits))
            })
            .collect();
        sort_flock(&mut instances);
        // `Ok` even when `failure` is set: the caller records `app`
        // unconditionally and turns `shortfall` into the operator's error,
        // where an `Err` would leave the muster roll on the pre-scale count.
        let _ = reply.send(Ok(Scaled {
            instances,
            app: stored,
            requested: count,
            shortfall: failure,
        }));
    }

    /// Every registered id of `name`, in instance order.
    ///
    /// Not [`Self::matching_ids`] over a name selector: this is a lookup of one
    /// app's own slots, where a selector's folds, wildcards and slot suffixes
    /// would be a second way to answer the same question.
    fn ids_of_name(&self, name: &str) -> Vec<u32> {
        let mut slots: Vec<(u32, u32)> = self
            .sheep
            .iter()
            .filter(|(_, slot)| slot.entry.spec.config().name == name)
            .map(|(id, slot)| (slot.entry.instance, *id))
            .collect();
        slots.sort_unstable();
        slots.into_iter().map(|(_, id)| id).collect()
    }

    /// Merges each declared app into the sheep of the same name, one app at a
    /// time, refusing whole any app whose merge does not normalize.
    ///
    /// A load never registers an app the flock does not have, and never prunes
    /// one the file omits: the loop walks the declared apps, never the flock.
    /// Under [`ResetDepth::None`] it never kills either: a field the running
    /// child holds parks in [`ProcessEntry::pending`] for its next spawn, while
    /// `spec` goes on describing what the child was spawned from. A reset can,
    /// since `instances` routes through [`Self::handle_scale`], whose
    /// `Ordering::Less` arm deletes the instances above the new count.
    /// [`ResetDepth::Env`] and `shutting_down` never reach that function.
    fn handle_apply_config(&mut self, apps: Vec<DeclaredApp>, reset: ResetDepth) -> Vec<Applied> {
        // One locked read for the whole file and one locked write at the end:
        // the store is rewritten whole on every write, so a per-app pair costs
        // an eleven-app Flockfile 22 lock acquisitions on the thread that
        // supervises the flock.
        let store = match overrides::all(&self.paths.overrides) {
            Ok(store) => store,
            Err(err) => {
                // The one refusal that covers the whole file: the merge cannot
                // tell what an operator has set without this store, and those
                // edits would be silently overwritten.
                let message = format!("overrides could not be read: {err}");
                return apps
                    .iter()
                    .map(|incoming| Applied {
                        name: incoming.config.name.clone(),
                        applied: Vec::new(),
                        pending: Vec::new(),
                        refused: Some(message.clone()),
                        app: None,
                    })
                    .collect();
            }
        };

        let mut changes = BTreeMap::new();
        let mut report = Vec::with_capacity(apps.len());
        for incoming in &apps {
            let overrides = store
                .get(&incoming.config.name)
                .cloned()
                .unwrap_or_default();
            report.push(self.apply_one(incoming, reset, &overrides, &mut changes));
        }

        if let Err(err) = overrides::update(&self.paths.overrides, &changes) {
            // Reported next to what landed rather than as a refusal: the
            // flock has already been changed by the time this runs, and an
            // `Applied` claiming nothing happened would be untrue.
            let note = format!("overrides could not be written: {err}");
            for applied in &mut report {
                if !changes.contains_key(&applied.name) {
                    continue;
                }
                applied.refused = Some(match applied.refused.take() {
                    Some(existing) => format!("{existing}; {note}"),
                    None => note.clone(),
                });
            }
        }
        report
    }

    /// The slot that stands in for `name`'s whole app, for a command that
    /// holds a name rather than an id.
    ///
    /// NOT `ids_of_name(name).first()`: during a reload the drainee holds
    /// the lower id, so the first id is the instance on its way out, and a
    /// config read off it describes what the app is leaving behind rather
    /// than what it is becoming. Falls back to the first id when every slot
    /// is draining, so a command arriving inside a serial reload's drain
    /// window still finds its app rather than being told it is not
    /// registered.
    fn representative_id(&self, name: &str) -> Option<u32> {
        let ids = self.ids_of_name(name);
        ids.iter()
            .copied()
            .find(|id| {
                self.sheep
                    .get(id)
                    .is_some_and(|slot| !matches!(slot.entry.reload, ReloadState::Drainee { .. }))
            })
            .or_else(|| ids.first().copied())
    }

    /// One sheep's effective config for a pane, or `Ok(None)` when no sheep
    /// has that name.
    ///
    /// Reads and writes nothing. `env` is emptied on the way out by
    /// [`SheepConfigView::new`], which is the only constructor, so no path
    /// out of here can carry a value.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::IsADog`] - the name is a dog's. No other
    ///   request hands a client a whole `AppConfig`, so serving one here
    ///   would be a read surface that exists for dogs and nothing else.
    fn handle_sheep_config(&self, name: &str) -> Result<Option<SheepConfigView>, SupervisorError> {
        let Some(id) = self.representative_id(name) else {
            return Ok(None);
        };
        let Some(slot) = self.sheep.get(&id) else {
            return Ok(None);
        };
        if slot.entry.dog.is_some() {
            return Err(SupervisorError::IsADog(dog_config_refusal(name)));
        }
        // [`Self::intended_spec`], not the running `spec`: a pane has to
        // show what the sheep is MEANT to be, or an operator's own parked
        // edit reads as never having landed. `pending` beside it is what
        // says the running child does not have it yet, and it is computed
        // off the same helper the listing uses, so the pane and the flock
        // table never disagree about the same sheep.
        let Some(config) = self.intended_spec(id).map(|spec| spec.config().clone()) else {
            return Ok(None);
        };
        Ok(Some(SheepConfigView::new(
            config,
            self.overridden_for(name),
            pending_fields(&slot.entry),
        )))
    }

    /// Records `key` on `name`'s env as an operator override, or removes it
    /// with `value: None`, and parks the result for that sheep's next spawn.
    ///
    /// `Ok(false)` when no sheep has that name.
    ///
    /// # Ordering
    ///
    /// Validate, then write the store, then park. A refusal is therefore
    /// raised before anything is touched, so an operator whose key
    /// `normalize` will not take is left with the env they already had
    /// rather than a store that disagrees with the flock.
    ///
    /// # Why it parks rather than applies
    ///
    /// The running child was handed its environment at `execve` and cannot
    /// be handed another one, so `env` is a `NeedsRespawn` field wherever it
    /// appears (`config::apply`'s table) and this takes the same route
    /// [`Self::apply_one`] takes for one: onto every slot's
    /// [`ProcessEntry::pending`], for a `shep reload` or `shep restart` to
    /// promote.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::IsADog`] - the name is a dog's. Raised before
    ///   the store is read, so nothing was written.
    /// - [`SupervisorError::InvalidEnv`] - the resulting config is one
    ///   `normalize` refuses. Nothing was written.
    /// - [`SupervisorError::Overrides`] - the override store could not be
    ///   read or written. Nothing was parked.
    fn handle_set_sheep_env(
        &mut self,
        name: &str,
        key: &str,
        value: Option<&str>,
    ) -> Result<Option<ResolvedApp>, SupervisorError> {
        let Some(id) = self.representative_id(name) else {
            return Ok(None);
        };
        // BEFORE the store is read and long before it is written. A dog
        // runs at the daemon's own trust level and its binary is what `shep
        // adopt` vetted, so a `PATH`, an `LD_PRELOAD` or a
        // `DYLD_INSERT_LIBRARIES` parked for its next respawn is arbitrary
        // code at that level -- and a dog is never in the override store,
        // so nothing further down this function would have caught it.
        // `Self::apply_one` refuses a Flockfile that names a dog for the
        // same reason and says the same sentence.
        if self
            .sheep
            .get(&id)
            .is_some_and(|slot| slot.entry.dog.is_some())
        {
            return Err(SupervisorError::IsADog(dog_config_refusal(name)));
        }
        // The config an unchanged next spawn would use, which is a parked
        // one when an earlier load left one: building on the RUNNING config
        // instead would drop that load's change on the floor, which is
        // `apply_one`'s own reason for reading `intended` rather than
        // `running`.
        let Some(mut intended) = self.intended_spec(id).map(|spec| spec.config().clone()) else {
            return Ok(None);
        };
        // Captured before the edit, because the removal branch below needs
        // to know whether there was anything to remove.
        let was_in_config = intended.env.contains_key(key);
        match value {
            Some(value) => intended.env.insert(key.to_string(), value.to_string()),
            None => intended.env.remove(key),
        };

        let parked =
            normalize(intended).map_err(|err| SupervisorError::InvalidEnv(err.to_string()))?;

        let mut record = overrides::get(&self.paths.overrides, name)
            .map_err(|err| SupervisorError::Overrides(err.to_string()))?
            .unwrap_or_default();
        // Read before the `entry` below borrows the record mutably.
        let file_declares = record.declared_env.contains(key);
        // A flat JSON object under the `env` key, which is the shape
        // `merge_declared` reads to decide which env keys an operator has
        // established. Anything else there is a store this build cannot act
        // on, and overwriting it would silently discard whatever a later
        // shep wrote (`AppOverrides::fields`' own doc argues the rule).
        let env = record
            .fields
            .entry("env".to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let Some(map) = env.as_object_mut() else {
            return Err(SupervisorError::Overrides(format!(
                "{name}'s stored `env` override is not an object"
            )));
        };
        let was_overridden = map.contains_key(key);
        // A tombstone is left alone rather than removed and re-inserted,
        // which makes a second removal of the same key a no-op instead of a
        // deletion. Without this, removing an already-removed key drops the
        // tombstone (there is nothing in the config to remove, so the
        // re-insert below does not fire) and the next load of a file that
        // still declares the key puts its value back.
        let tombstoned = map.get(key).is_some_and(serde_json::Value::is_null);
        match value {
            Some(value) => map.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            ),
            None if tombstoned => None,
            None => map.remove(key),
        };
        // # What a removal MEANS in the store
        //
        // Dropping the operator's own value is only half an answer, and for
        // a key the operator never set it is no answer at all: `map.remove`
        // is then a no-op, the store comes back `{}`, and the edit lives
        // nowhere but `ProcessEntry::pending` -- lost to a cold restart and
        // invisible in the CFG column, for a change the operator just made.
        //
        // So a removal leaves a JSON `null` TOMBSTONE under the key,
        // whenever the key was in the config and something other than this
        // store put it there. Three cases, and each falls out of the
        // condition rather than needing an arm:
        //
        // - the key came from the app's config and was never overridden:
        //   tombstone, because the removal IS the operator's edit and
        //   nothing else records it;
        // - the key was an operator override the file also declares:
        //   tombstone, because dropping the override alone would let the
        //   file's value read as current when the operator has removed it;
        // - the key was an operator override and nothing else supplies it:
        //   plain removal, because the operator has simply withdrawn their
        //   own value and the sheep no longer differs from its file. A
        //   tombstone here would mark a sheep forever for a key that exists
        //   nowhere.
        //
        // Nothing materializes an override's VALUE into a config -- the env
        // branch of `merge_declared` reads this map for its KEYS only -- so
        // a null is read exactly as "somebody has spoken for this key",
        // which is what stops a later plain load silently putting it back.
        // `--reset=env` and `--reset=all` clear it, which is right: a reset
        // is the operator asking for the file's env. The line that does it
        // is `merge_declared`'s own `next.fields.remove("env")` in that
        // arm, which drops the whole map before `establish_env` is reached
        // -- NOT `establish_env`, which by then has nothing left to find and
        // which deliberately preserves a tombstone it does find.
        if value.is_none() && was_in_config && (!was_overridden || file_declares) {
            map.insert(key.to_string(), serde_json::Value::Null);
        }
        let emptied = map.is_empty();
        // A removal that left nothing behind takes the `env` key with it,
        // for the reason `merge_declared` states where it spends an
        // override: a field nobody is holding a value for must stop
        // reporting in `ProcessEntry::overridden`, or the CFG column marks
        // a sheep that no longer differs from its Flockfile.
        if emptied {
            record.fields.remove("env");
        }
        let overridden: Vec<String> = record.fields.keys().cloned().collect();
        let changes = BTreeMap::from([(name.to_string(), Some(record))]);
        overrides::update(&self.paths.overrides, &changes)
            .map_err(|err| SupervisorError::Overrides(err.to_string()))?;

        // Every slot of the name, exactly as `apply_one` writes its own
        // parked config, and re-read by `ids_of_name` for its reason too.
        // `pending_reidentifies` is deliberately left alone: it tracks a
        // `user`/`group` move, and an env key cannot make one.
        for id in self.ids_of_name(name) {
            let Some(slot) = self.sheep.get_mut(&id) else {
                continue;
            };
            slot.entry.pending = Some(parked.clone());
            slot.entry.overridden.clone_from(&overridden);
        }
        Ok(Some(parked))
    }

    /// Records `key` on `name` as an operator override, applies what can
    /// reach the running process, and parks the rest for its next spawn.
    ///
    /// `Ok(None)` when no sheep has that name.
    ///
    /// # Why this is not `ApplyConfig`
    ///
    /// One [`DeclaredApp`] declaring one key, at [`ResetDepth::File`],
    /// moves exactly this one field and nothing else -- and then
    /// [`merge_declared`] spends the override for it, on correct reasoning
    /// that does not hold here: a key put back to the TEMPLATE is not a key
    /// an operator is still holding a value for, but a pane's value IS the
    /// operator's, and the sheep still differs from its file. Routed that
    /// way, an edit landed and then dropped out of
    /// [`ProcessEntry::overridden`], so the `*` never rendered and `shep
    /// flock`'s CFG column did not count it. This writes the override
    /// directly instead of pretending to be a template.
    ///
    /// # Ordering
    ///
    /// Validate, then write the store, then apply. Every refusal below is
    /// raised before [`overrides::update`], so an operator whose value this
    /// build will not take is left with the config they already had rather
    /// than a store that disagrees with the flock -- the rule
    /// [`Self::handle_set_sheep_env`] states and its own test pins.
    ///
    /// # Which fields reach a running process
    ///
    /// [`apply_group`]'s table, exactly as [`Self::apply_one`] reads it. A
    /// [`ApplyGroup::Live`] field goes onto the stored spec and is in force
    /// at the daemon's next decision; a [`ApplyGroup::NextSpawn`] field
    /// goes onto the stored spec too but is not in force until a spawn
    /// reads it, so it reports as pending -- except `autostart`, which
    /// `restorable()` reads at muster rather than at a spawn and so is in
    /// force the moment it lands, the same carve-out `apply_one` makes. A
    /// [`ApplyGroup::NeedsRespawn`] field only parks.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::IsADog`] - the name is a dog's. Raised before
    ///   the store is read, so nothing was written.
    /// - [`SupervisorError::InvalidField`] - the key is one this door does
    ///   not own or `AppConfig` does not have, the value will not
    ///   deserialize into it, or `normalize` refuses the result. Nothing
    ///   was written.
    /// - [`SupervisorError::Overrides`] - the override store could not be
    ///   read or written. Nothing was parked.
    fn handle_set_sheep_field(
        &mut self,
        name: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<Option<FieldSet>, SupervisorError> {
        // `env` has its own request, and it needs one: a whole env map is
        // never sent (a pane is not told the values), and this door's
        // wholesale replacement of one field would wipe every key but the
        // one being set. The two Structural fields are identity and flock
        // shape rather than runtime knobs -- `handle_scale` owns the count,
        // and a `name` change is a different sheep.
        if key == "env" {
            return Err(SupervisorError::InvalidField(
                "env is set one key at a time; use `SetSheepEnv`".to_string(),
            ));
        }
        if apply_group(key) == ApplyGroup::Structural {
            return Err(SupervisorError::InvalidField(format!(
                "{key} is not a config write; `shep stock` moves an instance count, and a \
                 name change is a different sheep"
            )));
        }
        let Some(id) = self.representative_id(name) else {
            return Ok(None);
        };
        // BEFORE the store is read and long before it is written, and for
        // the reason `handle_set_sheep_env`'s own guard gives at length: a
        // dog runs at the daemon's own trust level, a dog is never in the
        // override store so nothing further down would catch it, and this
        // door reaches `script` and `args` directly. `apply_one` refuses a
        // dog with this same sentence; `handle_scale` refuses one too, but
        // with its own -- a count is not a config write, so it says a dog
        // runs one process, under `InvalidScale` rather than `IsADog`.
        if self
            .sheep
            .get(&id)
            .is_some_and(|slot| slot.entry.dog.is_some())
        {
            return Err(SupervisorError::IsADog(dog_config_refusal(name)));
        }
        let Some(slot) = self.sheep.get(&id) else {
            return Ok(None);
        };
        // `running` is what the child was spawned from; `intended` is what
        // the app is meant to be, which is an earlier edit's parked config
        // when there is one. Building on `running` would drop that edit,
        // which is `apply_one`'s own reason for the same pair.
        let running = slot.entry.spec.config().clone();
        let intended = slot
            .entry
            .pending
            .as_ref()
            .map_or_else(|| running.clone(), |parked| parked.config().clone());

        let Ok(serde_json::Value::Object(mut object)) = serde_json::to_value(&intended) else {
            return Err(SupervisorError::InvalidField(
                "an app config must serialize as an object".to_string(),
            ));
        };
        // Checked rather than inserted blind. `AppConfig` would take an
        // unknown key without complaint or ignore it outright, either way
        // reporting a write that changed nothing, and a pane's key comes
        // off a schema this daemon may not share a version with.
        if !object.contains_key(key) {
            return Err(SupervisorError::InvalidField(format!(
                "no config field named {key}"
            )));
        }
        object.insert(key.to_string(), value.clone());
        let edited: AppConfig = serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|err| SupervisorError::InvalidField(format!("{key}: {err}")))?;
        let merged = normalize(edited)
            .map_err(|err| SupervisorError::InvalidField(format!("{key}: {err}")))?;

        // The one field that moved, if it moved at all. A value identical
        // to what is already intended still records the override -- the
        // operator has spoken for the key, which is the whole point of this
        // door -- but nothing needs applying or parking for it.
        let group = apply_group(key);
        let reaches = matches!(group, ApplyGroup::Live | ApplyGroup::NextSpawn);
        let reaching: Vec<String> = if reaches {
            vec![key.to_string()]
        } else {
            Vec::new()
        };
        // A SUBSET of a valid config can still fail to normalize, because
        // `normalize` checks fields against each other: `watch` needs a
        // `cwd`, and a `cwd` left behind as `NeedsRespawn` takes the watch
        // down with it. `apply_one` treats that as "this app needs a
        // restart" rather than as an invalid request, and so does this.
        let next_spec = reached_spec(&running, merged.config(), &reaching, running.instances).ok();
        let park_all = next_spec.is_none();

        let mut record = overrides::get(&self.paths.overrides, name)
            .map_err(|err| SupervisorError::Overrides(err.to_string()))?
            .unwrap_or_default();
        record.fields.insert(key.to_string(), value.clone());
        let overridden: Vec<String> = record.fields.keys().cloned().collect();
        let changes = BTreeMap::from([(name.to_string(), Some(record))]);
        overrides::update(&self.paths.overrides, &changes)
            .map_err(|err| SupervisorError::Overrides(err.to_string()))?;

        // Parked whenever the field cannot reach a running child, and also
        // whenever an earlier edit already left a config parked: that
        // config predates this one, and promoting it later would put this
        // field back. `apply_one` recomputes it on the same condition.
        let parked_wanted = park_all
            || !reaches
            || self.ids_of_name(name).iter().any(|id| {
                self.sheep
                    .get(id)
                    .is_some_and(|s| s.entry.pending.is_some())
            });
        let parked = parked_wanted.then(|| merged.clone());

        // Every slot of the name, and `ids_of_name` re-read for the reason
        // `handle_set_sheep_env` gives.
        for id in self.ids_of_name(name) {
            let Some(slot) = self.sheep.get_mut(&id) else {
                continue;
            };
            // Against THIS slot's own spec and before it is overwritten,
            // and `|=` rather than `=`. Both halves are `apply_one`'s and
            // the argument for them is stated there, at the same line in
            // that function -- not restated here, because a paraphrase of
            // a reason is what goes stale when the reason changes.
            if let Some(parked) = &parked {
                let spawned = slot.entry.spec.config();
                slot.entry.pending_reidentifies |=
                    spawned.user != parked.config().user || spawned.group != parked.config().group;
            }
            if let Some(next_spec) = next_spec.clone() {
                slot.entry.spec = next_spec;
            }
            if let Some(parked) = parked.clone() {
                slot.entry.pending = Some(parked);
            }
            slot.entry.overridden.clone_from(&overridden);
        }

        // A Live field the extras engine reads is re-armed now rather than
        // at the next spawn, the same line `apply_one` runs for the same
        // set of fields.
        if !park_all && group == ApplyGroup::Live && EXTRAS_FIELDS.contains(&key) {
            self.rearm_name(name);
        }

        // In force, or waiting for a respawn. `autostart` is the
        // `NextSpawn` field that reports as in force, because `restorable()`
        // reads it at muster or boot rather than at a spawn, and telling an
        // operator to restart for it would be telling them to do nothing.
        let in_force = !park_all && (group == ApplyGroup::Live || key == "autostart");
        Ok(Some(FieldSet {
            app: parked.unwrap_or_else(|| next_spec.unwrap_or(merged)),
            pending: !in_force,
        }))
    }

    /// One app's half of [`Self::handle_apply_config`].
    ///
    /// `overrides` is this app's record as the one store read found it, and
    /// `changes` is where its replacement goes: the write is the caller's, so
    /// a file of eleven apps costs one lock rather than eleven.
    ///
    /// Every refusal that answers "this app was not touched" is raised before
    /// the instance count is routed: a refusal after the scale would report an
    /// untouched app while the flock had already been reshaped.
    fn apply_one(
        &mut self,
        incoming: &DeclaredApp,
        reset: ResetDepth,
        overrides: &AppOverrides,
        changes: &mut BTreeMap<String, Option<AppOverrides>>,
    ) -> Applied {
        let name = incoming.config.name.clone();
        let refuse = |message: String| Applied {
            name: name.clone(),
            applied: Vec::new(),
            pending: Vec::new(),
            refused: Some(message),
            app: None,
        };

        let ids = self.ids_of_name(&name);
        // The app's stand-in, and not `ids.first()`: during a reload the
        // drainee holds the lower id, and the `next_spec` derived from this
        // slot is written onto every slot of the name.
        let Some(slot) = self
            .representative_id(&name)
            .and_then(|id| self.sheep.get(&id))
        else {
            return refuse(format!(
                "{name} is not registered; `shep start` it before a config can be applied to it"
            ));
        };
        // Mirroring `handle_scale`'s guard, for a sharper reason: a dog is
        // never in the override store, so every key a file declares for one is
        // unestablished forever and the additive rule never engages.
        if slot.entry.dog.is_some() {
            return refuse(dog_config_refusal(&name));
        }
        // `running` is what the child was spawned from, `intended` what the app
        // is meant to be. The merge builds on `intended`, or a second load of
        // the same file would find its own key established, skip it, and merge
        // the running value over the parked one.
        let running = slot.entry.spec.config().clone();
        let intended = slot
            .entry
            .pending
            .as_ref()
            .map_or_else(|| running.clone(), |parked| parked.config().clone());

        let (merged, mut next_overrides) =
            match merge_declared(&intended, incoming, overrides, reset) {
                Ok(merged) => merged,
                Err(message) => return refuse(message),
            };
        let merged = match normalize(merged) {
            Ok(merged) => merged,
            Err(err) => return refuse(err.to_string()),
        };

        let mut live = Vec::new();
        let mut next_spawn = Vec::new();
        let mut respawn = Vec::new();
        let mut instances = false;
        for field in intended.drifted_fields(merged.config()) {
            match apply_group(&field) {
                ApplyGroup::Live => live.push(field),
                ApplyGroup::NextSpawn => next_spawn.push(field),
                ApplyGroup::NeedsRespawn => respawn.push(field),
                // `name` cannot drift, the app having been found by it, and
                // `increment_var` is refused by `normalize`, so `instances` is
                // the only structural field that reaches here.
                ApplyGroup::Structural => instances |= field == "instances",
                // A group a later shep-core adds. Treated as the table's own
                // fallback treats an unknown field: the conservative answer
                // is that the running process does not have the new value.
                _ => respawn.push(field),
            }
        }

        let mut refusals = Vec::new();
        let mut count = running.instances;
        // A plain load never reshapes a flock, so `merge_declared` keeps
        // `instances` out of the merge under `None` and `Env`, leaving only a
        // note. `File` takes a count the template declares and leaves it alone
        // when the template is silent.
        if !matches!(
            reset,
            ResetDepth::Policy | ResetDepth::All | ResetDepth::File
        ) && incoming.declared.contains("instances")
            && incoming.config.instances != running.instances
        {
            // Names the scope, not just the remedy: the guard fires under
            // `--reset=env` too.
            refusals.push(format!(
                "instances: this load never reshapes a flock; no mode scales without also \
                 putting back every setting the file declares, and `--reset=file` is the \
                 narrowest that does, taking the file's count of {}",
                incoming.config.instances
            ));
        }
        if instances {
            if self.shutting_down {
                refusals.push(
                    "instances: the daemon is shutting down and cannot reshape a flock".to_string(),
                );
            } else {
                let (reply, mut answer) = oneshot::channel();
                self.handle_scale(&name, merged.config().instances, reply);
                match answer
                    .try_recv()
                    .expect("handle_scale answers before it returns")
                {
                    Ok(scaled) => {
                        count = scaled.achieved();
                        if let Some(shortfall) = scaled.shortfall {
                            refusals.push(format!("instances: {shortfall}"));
                        }
                    }
                    // Refused whole: every `Err` `handle_scale` answers with is
                    // raised before it spawns or removes anything, so the flock
                    // really is as it was, and a scale is refused exactly when
                    // something else is already reshaping the app.
                    Err(err) => return refuse(err.to_string()),
                }
            }
        }

        // Built from the running config, plus only the fields that can reach a
        // running process: a `NeedsRespawn` field written here would erase the
        // record of what the child was actually spawned from.
        let reaching: Vec<String> = live.iter().chain(next_spawn.iter()).cloned().collect();
        // A subset of a config that normalized can still fail, since normalize
        // checks fields against each other: `watch` needs a `cwd`, and a file
        // declaring both leaves the `cwd` behind. That parks rather than
        // refuses.
        let next_spec = reached_spec(&running, merged.config(), &reaching, count).ok();
        let park_all = next_spec.is_none();

        // The whole merge, for a next spawn to pick up. Recomputed whenever an
        // earlier load left one parked: that config predates this load's Live
        // changes, and promoting it later would put them back.
        let parked_wanted = park_all
            || !respawn.is_empty()
            || ids.iter().any(|id| {
                self.sheep
                    .get(id)
                    .is_some_and(|slot| slot.entry.pending.is_some())
            });
        let parked = if parked_wanted {
            with_count(&merged, count)
        } else {
            None
        };
        // Nothing could be parked, so nothing may be reported as parked, and
        // the previous parked config is left rather than cleared: it is an
        // earlier load's change and still the one a respawn should pick up.
        let parked_failed = parked_wanted && parked.is_none();

        // Re-read by `get_mut` rather than by index: a scale registered ids
        // this app did not have a moment ago and deregistered some it did, so
        // the pre-scale list can name a slot that is already gone.
        for id in self.ids_of_name(&name) {
            let Some(slot) = self.sheep.get_mut(&id) else {
                continue;
            };
            // Before the spec is overwritten below, and against this slot's own
            // spec rather than the one `next_spec` came from: writing that
            // first would make every sibling look like instance 0. `|=` never
            // `=`, so a change nobody has promoted is not forgotten.
            if let Some(parked) = &parked {
                let running = slot.entry.spec.config();
                slot.entry.pending_reidentifies |=
                    running.user != parked.config().user || running.group != parked.config().group;
            }
            if let Some(next_spec) = next_spec.clone() {
                slot.entry.spec = next_spec;
            }
            if let Some(parked) = parked.clone() {
                slot.entry.pending = Some(parked);
            }
        }

        if !park_all
            && live
                .iter()
                .any(|field| EXTRAS_FIELDS.contains(&field.as_str()))
        {
            self.rearm_name(&name);
        }

        // `autostart` is the one `NextSpawn` field that reports as applied:
        // `restorable()` reads it at muster or boot rather than at a spawn, so
        // it is in force the moment it lands on the stored spec.
        let (autostart, later): (Vec<String>, Vec<String>) = next_spawn
            .into_iter()
            .partition(|field| field == "autostart");
        // What could not be parked, for the refusal below to name. Under
        // `park_all` that is every field; otherwise the `NeedsRespawn` ones
        // alone, since a `NextSpawn` field is already on the stored spec.
        let unparked: Vec<String> = if !parked_failed {
            Vec::new()
        } else if park_all {
            live.iter()
                .cloned()
                .chain(autostart.iter().cloned())
                .chain(later.iter().cloned())
                .chain(respawn.iter().cloned())
                .collect()
        } else {
            respawn.clone()
        };
        // Unconditional on `parked_failed`, never on `unparked` being
        // non-empty: `parked_wanted` is also set by an earlier load's parked
        // config, so a load whose only drift is Live can fail to rebuild it
        // with no field of its own to name.
        if parked_failed {
            let what = if unparked.is_empty() {
                "an earlier load's parked config could not be rebuilt, so a respawn will put \
                 its values back"
                    .to_string()
            } else {
                format!(
                    "{} could not be parked for a next spawn",
                    unparked.join(", ")
                )
            };
            refusals.push(format!(
                "{what}: the merged config does not hold at {count} instance(s)"
            ));
        }
        // A field that went nowhere was established by nobody, so this app's
        // record goes back to what it was. Without this the high-water mark
        // absorbs the refused key, and an operator retrying the identical file
        // gets silence.
        for field in &unparked {
            match overrides.fields.get(field) {
                Some(previous) => next_overrides
                    .fields
                    .insert(field.clone(), previous.clone()),
                None => next_overrides.fields.remove(field),
            };
            if !overrides.declared.contains(field) {
                next_overrides.declared.remove(field);
            }
            if field == "env" {
                next_overrides
                    .declared_env
                    .clone_from(&overrides.declared_env);
            }
        }

        // Cached on every instance so `to_info` reads it with no file access:
        // the store is read once per load and `to_info` runs once per sheep on
        // every listing. `All` alone drops the record, since the record is what
        // holds a later plain load off a key an operator set.
        let overridden_names: Vec<String> = if matches!(reset, ResetDepth::All) {
            Vec::new()
        } else {
            next_overrides.fields.keys().cloned().collect()
        };
        for id in self.ids_of_name(&name) {
            if let Some(slot) = self.sheep.get_mut(&id) {
                slot.entry.overridden.clone_from(&overridden_names);
            }
        }

        // Handed to the caller rather than written here, so one file costs one
        // lock. Recorded only for an app that got this far: a load that refused
        // established nothing. The `Option` is the drop.
        changes.insert(
            name.clone(),
            (!matches!(reset, ResetDepth::All)).then_some(next_overrides),
        );
        let (mut applied, mut pending): (Vec<String>, Vec<String>) = if park_all {
            // Nothing reached the running instances at all, so nothing may be
            // reported as applied.
            (
                Vec::new(),
                if parked_failed {
                    Vec::new()
                } else {
                    live.iter()
                        .cloned()
                        .chain(autostart)
                        .chain(later)
                        .chain(respawn)
                        .collect()
                },
            )
        } else {
            (
                live.iter().cloned().chain(autostart).collect(),
                if parked_failed {
                    later
                } else {
                    later.into_iter().chain(respawn).collect()
                },
            )
        };
        if instances && count != running.instances {
            applied.push("instances".to_string());
        }
        applied.sort_unstable();
        pending.sort_unstable();

        let app = if count == merged.config().instances {
            Some(merged)
        } else {
            with_count(&merged, count)
        };
        Applied {
            name,
            applied,
            pending,
            refused: (!refusals.is_empty()).then(|| refusals.join("; ")),
            app,
        }
    }

    /// Rebuilds every lifecycle extra armed for `name`, so a changed
    /// [`EXTRAS_FIELDS`] value reaches the worker enforcing it.
    ///
    /// [`ExtrasRegistry::rearm_name`] rather than [`Self::arm_extras`] per id:
    /// `arm` preserves a live cron or watch task, which is right for a reload's
    /// overlap and wrong here.
    ///
    /// Gated on `Online` with a live pid, as every other arming site is:
    /// [`ExtrasRegistry::arm`] decides group membership from the configuration,
    /// so arming a stopped instance puts it in a group whose cron occurrence
    /// would start a process the operator had stopped. One prober per instance,
    /// since [`assemble`] bakes `SHEP_INSTANCE` into a prober's environment.
    fn rearm_name(&mut self, name: &str) {
        let Some(extras) = self.extras.as_ref() else {
            return;
        };
        // Constructed inline, as `arm_extras` does, so the registry can be
        // borrowed mutably while the flock is read.
        let supervisor = SupervisorHandle {
            tx: self.tx.clone(),
        };
        let mut armable: Vec<(u32, &ProcessEntry)> = self
            .sheep
            .iter()
            .filter(|(_, slot)| {
                slot.entry.spec.config().name == name
                    && slot.entry.status == ProcStatus::Online
                    && slot.entry.pid.is_some()
            })
            .map(|(id, slot)| (*id, &slot.entry))
            .collect();
        // No early return on an empty list: a name whose instances are all
        // momentarily non-`Online` still has to reach `rearm_name`, which is
        // what aborts the group holding the old config. Sorted, so the rebuild
        // order does not depend on a `HashMap`'s iteration order.
        armable.sort_unstable_by_key(|(id, _)| *id);
        let entries: Vec<&ProcessEntry> = armable.into_iter().map(|(_, entry)| entry).collect();
        let paths = &self.paths;
        self.registry.rearm_name(
            name,
            &entries,
            |entry| {
                // `Credentials` is `Copy`, so this needs no clone. The
                // unresolved arm costs nothing: an entry reached here is
                // running and resolved its identity before it started.
                let credentials = match entry.credentials {
                    SpawnIdentity::Resolved(credentials) => credentials,
                    SpawnIdentity::Unresolved => None,
                };
                spec_prober(&assemble(&entry.spec, entry.instance, paths, credentials))
            },
            extras,
            &supervisor,
        );
    }

    /// Accepts a reload: answers the caller at once, then starts one swap per
    /// matched app.
    ///
    /// The answer is an acceptance, not a result. One instance costs
    /// `listen_timeout` + `graceful_timeout` against `crate::rpc`'s 60s ceiling
    /// on a request budget, so the reply is the matched sheep as they stood
    /// when the reload was accepted; the swaps report themselves on the bus.
    ///
    /// Only an `Online` instance is replaced, and everything else the selector
    /// matched is a no-op success: a `Starting` sheep is excluded too, since a
    /// second live process in its slot buys an overlap nobody can use yet.
    /// Nothing here re-reads configuration.
    fn handle_reload(
        &mut self,
        selector: &ProcessSelector,
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    ) {
        let matched = self.matching_ids(selector);

        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        // Refused whole, before anything is spawned: a partly-accepted selector
        // is worse than a refused one.
        let in_flight = matched.iter().find_map(|id| {
            let slot = self
                .sheep
                .get(id)
                .expect("handle_reload: `matched` holds ids read off this map a moment ago");
            let name = &slot.entry.spec.config().name;
            self.reloads.contains_key(name).then(|| name.clone())
        });
        if let Some(name) = in_flight {
            let _ = reply.send(Err(SupervisorError::ReloadInFlight(name)));
            return;
        }

        let mut accepted: Vec<ProcessInfo> = matched
            .iter()
            .map(|id| {
                let slot = self
                    .sheep
                    .get(id)
                    .expect("handle_reload: `matched` holds ids read off this map a moment ago");
                to_info(&slot.entry, &self.smits)
            })
            .collect();
        // The table `shep reload` prints, so it takes the order every
        // operator-facing listing takes, not `matching_ids`' id order.
        sort_flock(&mut accepted);

        // Grouped by app, since a reload runs one instance of an app at a time,
        // and ordered by instance slot, since that is the order an operator
        // reads a clustered app in.
        let mut queues: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
        for id in matched {
            let slot = self
                .sheep
                .get(&id)
                .expect("handle_reload: `matched` holds ids read off this map a moment ago");
            if !reload_eligible(slot) {
                continue;
            }
            let entry = &slot.entry;
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
    /// The one door into `SpawnNew`, so "one instance at a time" and, through
    /// [`Self::arm_reload_deadline`], "every swap is bounded" are properties of
    /// this function rather than rules spread over its callers.
    ///
    /// An instance that stopped being `Online`, or whose next exit something
    /// already claimed, is skipped: a swap against a claimed exit is doomed,
    /// landing inside `AwaitReady` carrying the marker. A replacement that
    /// cannot be spawned ends the reload, leaving the old instances running.
    /// [`ReloadMode::Serial`] reaches `SpawnNew` from [`Self::reap_drainee`].
    fn advance_reload(&mut self, name: &str, mut queue: VecDeque<u32>) {
        // Defence in depth: a shutdown clears every job before anything can
        // reach here. A replacement spawned now would be a child outside the
        // shutdown aggregation.
        if self.shutting_down {
            self.reloads.remove(name);
            return;
        }
        while let Some(old_id) = queue.pop_front() {
            let slot = self.sheep.get(&old_id);
            let replaceable =
                slot.is_some_and(|slot| reload_eligible(slot) && slot.manual.is_none());
            if !replaceable {
                // A whole reload can end here having replaced nothing, long
                // after its caller was told `Ok`, so this is its only account.
                tracing::debug!(
                    name,
                    old_id,
                    status = ?slot.map(|slot| slot.entry.status),
                    "reload skipped an instance: it is gone, no longer online, or its next \
                     exit is already claimed"
                );
                continue;
            }
            if self.reload_mode_of(old_id) == ReloadMode::Serial {
                self.begin_serial_drain(name, old_id, queue);
                return;
            }
            match self.spawn_replacement(old_id, ReloadMode::Overlap) {
                Ok(new_id) => {
                    self.reloads.insert(
                        name.to_string(),
                        ReloadJob {
                            queue,
                            mode: ReloadMode::Overlap,
                            deadline: 0,
                            swap: ReloadSwap {
                                old_id,
                                new_id: Some(new_id),
                                phase: ReloadPhase::AwaitReady,
                            },
                        },
                    );
                    self.arm_reload_deadline(name, new_id);
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
                    // The one end a reload reaches without `abort_reload`: no
                    // replacement was ever registered, and `spawn_replacement`
                    // has already put the drainee back to `Online`.
                    if let Some(slot) = self.sheep.get(&old_id) {
                        let info = to_info(&slot.entry, &self.smits);
                        self.emit(ProcessEventKind::ReloadAbandoned, info, true);
                    }
                    return;
                }
            }
        }
        self.reloads.remove(name);
    }

    /// The reload ordering the instance `old_id` occupies asks for.
    ///
    /// Read off [`Self::intended_spec`], the config the replacement will be
    /// spawned from, so the mode and the spawn cannot decide from two different
    /// files. Not off the stored spec: `readiness_probe` lands there at once
    /// while `wait_ready` parks, so a migrating app holds both, `wait_ready`
    /// wins in [`ReadinessSource::of`], and the mode would overlap against a
    /// probe the drainee can answer.
    fn reload_mode_of(&self, old_id: u32) -> ReloadMode {
        let config = self
            .intended_spec(old_id)
            .expect("reload_mode_of: the instance was found replaceable a moment ago")
            .config();
        let source = ReadinessSource::of(config)
            .expect("ResolvedApp already passed ProbeTarget::parse in normalize");
        ReloadMode::of(config, &source)
    }

    /// `DrainOld`, run first: a serial reload asks the instance it is about to
    /// replace to go, and spawns nothing until it has.
    ///
    /// The mirror of the overlap arm of [`Self::advance_reload`], with the
    /// spawn moved to [`Self::reap_drainee`]. The drainee's `Reload` is emitted
    /// here, since a reload silent until its drain was over would look like one
    /// that never started. The watchdog is armed against the drainee's entry,
    /// there being no replacement yet: without one, an instance wedged past its
    /// own `SIGKILL` leaves a job nothing can remove.
    fn begin_serial_drain(&mut self, name: &str, old_id: u32, queue: VecDeque<u32>) {
        let drainee = self
            .sheep
            .get_mut(&old_id)
            .expect("begin_serial_drain: the instance was found replaceable a moment ago");
        drainee.entry.status = ProcStatus::Stopping;
        // `None`: there is no replacement until this instance's exit is
        // handled. The marker routes that exit to `reap_drainee` rather than to
        // `decide_on_exit`, which would respawn the old code into the slot.
        drainee.entry.reload = ReloadState::Drainee { new_id: None };
        let info = to_info(&drainee.entry, &self.smits);

        self.reloads.insert(
            name.to_string(),
            ReloadJob {
                queue,
                mode: ReloadMode::Serial,
                deadline: 0,
                swap: ReloadSwap {
                    old_id,
                    new_id: None,
                    phase: ReloadPhase::DrainFirst,
                },
            },
        );
        self.arm_reload_deadline(name, old_id);
        self.emit(ProcessEventKind::Reload, info, true);
        self.claim_manual(
            old_id,
            PendingManual {
                kind: ManualKind::Stop,
                origin: CommandOrigin::Operator,
            },
            LadderCap::Drain,
        );
    }

    /// `SpawnNew`: spawns a replacement in `old_id`'s instance slot under a
    /// new id, and returns that id.
    ///
    /// Same slot, so `SHEP_INSTANCE`, templated env and log paths follow the
    /// drainee; new id, since two live processes per id breaks the property
    /// test; readiness gated even for `Heuristic`, or `DrainOld` would kill the
    /// drainee at once. `restarts` carries over, the budget does not.
    ///
    /// Marks the drainee `Stopping` before spawning, or `handle_extra_restart`
    /// would restart a drainee mid-`AwaitReady`. Under [`ReloadMode::Serial`]
    /// the drainee is already dead, so the `Reload` event and restore-on-failure
    /// are skipped, and this must run before `deregister_on_exit`.
    fn spawn_replacement(&mut self, old_id: u32, mode: ReloadMode) -> Result<u32, String> {
        // `Credentials` is `Copy`; reused, and re-resolved only when the config
        // being promoted is what changed `user` or `group`. A drainee is
        // running, so the seam finds a resolved identity and touches nothing.
        let credentials = self
            .intended_credentials(old_id)
            .map_err(|err| err.to_string())?;
        // The promotion, and it is a read: the replacement is built from the
        // config the drainee was owed, and the drainee keeps its own copy until
        // it is deregistered, which is what makes an abandoned reload harmless.
        let app = self
            .intended_spec(old_id)
            .expect("spawn_replacement: the instance was found replaceable a moment ago")
            .clone();
        let drainee = &self.sheep[&old_id].entry;
        let instance = drainee.instance;
        let restarts = drainee.restarts;
        // Carried across the swap: nothing here could re-derive it, and a dog
        // has to stay a dog across a reload.
        let dog = drainee.dog.clone();
        // Carried across the swap: a reload is not an exit, so the answer to
        // "why did this instance last stop" is still the drainee's, and `None`
        // would read as "this instance has never exited".
        let last_exit = drainee.last_exit;
        // Carried across the swap: a reload is not a config load, and without
        // this a reload would blank the cache `shep describe` reads.
        let overridden = drainee.overridden.clone();

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
        drainee.entry.reload = ReloadState::Drainee {
            new_id: Some(new_id),
        };

        match self.runner.spawn(&spec) {
            Ok((proc, io)) => {
                let pid = proc.pid();
                let entry = ProcessEntry {
                    id: new_id,
                    spec: app.clone(),
                    pending: None,
                    pending_reidentifies: false,
                    overridden,
                    instance,
                    status: ProcStatus::Starting,
                    pid: Some(pid),
                    restarts,
                    started_at: Some(tokio::time::Instant::now()),
                    budget: RestartBudget::default(),
                    reload: ReloadState::Replacement,
                    credentials: SpawnIdentity::Resolved(credentials),
                    out_file,
                    err_file,
                    dog,
                    last_exit,
                };
                let info = to_info(&entry, &self.smits);
                let log_ctl = io.log_ctl.clone();
                let to_child = io.to_child.clone();
                let to_stdin = io.to_stdin.clone();
                let handles = spawn_sheep_task::<R::Proc>(
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
                        ctl: Some(handles.ctl),
                        log_ctl: Some(log_ctl),
                        to_child: Some(to_child),
                        signals: Some(handles.signals),
                        to_stdin: Some(to_stdin),
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
                        ready_tx: Some(ready_tx),
                        actions: ActionWaits::default(),
                        ready_failed: false,
                        restart_due: None,
                    },
                );
                // The instance being replaced announces itself before its
                // replacement's `Start`: a reload's reply is an acceptance, so
                // a subscriber's whole account of the swap arrives here.
                if mode == ReloadMode::Overlap {
                    let drainee = to_info(&self.sheep[&old_id].entry, &self.smits);
                    self.emit(ProcessEventKind::Reload, drainee, true);
                }
                self.emit(ProcessEventKind::Start, info, true);
                Ok(new_id)
            }
            Err(error) => {
                // Nothing is registered for a replacement that never existed:
                // the drainee still owns this instance slot, and an `Errored`
                // row beside it would double every name-keyed verb. The id is
                // spent all the same, since ids are never reused.
                if mode == ReloadMode::Overlap {
                    let drainee = self
                        .sheep
                        .get_mut(&old_id)
                        .expect("spawn_replacement: the drainee was marked a moment ago");
                    drainee.entry.status = restored_status(drainee);
                    drainee.entry.reload = ReloadState::None;
                }
                Err(error.to_string())
            }
        }
    }

    /// `AwaitReady` resolved for a replacement: commit the swap, or abandon
    /// the reload.
    ///
    /// A reload is the one caller for which a readiness deadline elapsing is a
    /// failure: a replacement that cannot answer has not proved it can take
    /// over, so it is never marked `Online` on a `TimedOut`.
    ///
    /// Keyed on the [`Readiness`] verdict and never on the deadline, which is
    /// what makes it correct for all three sources: `await_ready`'s `Heuristic`
    /// arm reports `Ready`, since for a heuristic the elapse is the signal.
    fn reload_ready_result(&mut self, new_id: u32, manually: bool, readiness: Readiness) {
        let Some(name) = self.reload_of(new_id) else {
            // Defensive: nothing leaves a `Replacement` marker behind without a
            // job naming it. Take the ordinary transition rather than strand
            // the sheep at `Starting`.
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
            // Abandoning protects the instance that can still serve, so the
            // full abandonment, killing the replacement and putting the
            // drainee back, is available only while there is one.
            let old_id = self.reloads[&name].swap.old_id;
            if self.sheep.contains_key(&old_id) {
                self.abort_reload(&name, "the replacement was not ready inside listen_timeout");
                return;
            }
            // With nothing to fall back to, the reload ends here. The
            // replacement is left running and `Starting`, never `Online`: a
            // deploy tool reads `Online` as "the new release is serving". That
            // costs it the extras `went_online` arms.
            tracing::warn!(
                name,
                new_id,
                "reload abandoned: the replacement was not ready inside listen_timeout, with \
                 the instance it replaced already gone; it is left running and not online"
            );
            self.reloads.remove(&name);
            self.clear_reload(new_id);
            if let Some(slot) = self.sheep.get_mut(&new_id) {
                slot.ready_failed = true;
                let info = to_info(&slot.entry, &self.smits);
                self.emit(ProcessEventKind::ReloadAbandoned, info, true);
            }
            return;
        }

        let info = self.set_status(new_id, ProcStatus::Online);
        self.went_online(new_id, info, manually);
        self.begin_drain(&name);
    }

    /// `DrainOld`: the replacement is serving, so ask the instance it
    /// replaced to go.
    ///
    /// The ladder runs under `graceful_timeout` rather than `kill_timeout` (see
    /// [`LadderCap`]): this is the stop that expects the instance to stop
    /// accepting, finish what it already has, and exit.
    ///
    /// Marks the swap committed first, so a later abandonment leaves the
    /// replacement where it is instead of undoing a kill already in flight.
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
    ///
    /// The `Reloaded` this announces is the one event that says a swap
    /// succeeded, and it goes out before the next swap begins so a clustered
    /// app's reload reads in order. It is owed only to a replacement that is
    /// serving: one that went down inside the drain window keeps its row, and
    /// the event would name a process that is not there. Anything else ends
    /// the reload as an abandonment, the queue with it being the rest of a
    /// clustered app left on the old code. An overlapping reload of a probed
    /// app defers the success and asks again with the drainee gone, since its
    /// `Online` may rest on a probe the drainee answered.
    fn finish_swap(&mut self, name: &str) {
        let Some(job) = self.reloads.remove(name) else {
            return;
        };
        let new_id = job
            .swap
            .new_id
            .expect("finish_swap: a swap with no replacement never reaches the drain");

        let serving = self
            .sheep
            .get(&new_id)
            .is_some_and(|slot| slot.entry.status == ProcStatus::Online);
        if serving && let Some(source) = self.post_drain_probe(new_id, job.mode) {
            // The job goes back rather than ending, and the replacement keeps
            // its `Replacement` marker: the swap is not over, so a second
            // reload is still refused and a replacement that dies inside the
            // window still reaches `handle_exited`'s reload arm.
            self.reloads.insert(
                name.to_string(),
                ReloadJob {
                    swap: ReloadSwap {
                        phase: ReloadPhase::Verify,
                        ..job.swap
                    },
                    ..job
                },
            );
            // Armed again: the running watchdog was sized for `AwaitReady` plus
            // the drain, and the probe below can take another `listen_timeout`
            // on top of both.
            self.arm_reload_deadline(name, new_id);
            self.spawn_verify_task(name, new_id, source);
            return;
        }
        self.clear_reload(new_id);

        if serving {
            let info = to_info(&self.sheep[&new_id].entry, &self.smits);
            self.emit(ProcessEventKind::Reloaded, info, true);
            self.advance_reload(name, job.queue);
            return;
        }

        tracing::warn!(
            name,
            new_id,
            "reload abandoned: the replacement was no longer serving when the instance it \
             replaced went"
        );
        if let Some(slot) = self.sheep.get(&new_id) {
            let info = to_info(&slot.entry, &self.smits);
            self.emit(ProcessEventKind::ReloadAbandoned, info, true);
        }
    }

    /// The readiness source a drained swap still has to prove, if it has one.
    ///
    /// `Some` only for an overlapping reload of an app whose readiness comes
    /// from a probe: the two instances shared a `SO_REUSEPORT` group, so the
    /// probe that put the replacement `Online` may have been answered by the
    /// instance now reaped.
    ///
    /// `None` everywhere else. A serial reload already asked with the slot
    /// empty; `Channel` readiness is the replacement's own; `Heuristic` has
    /// nothing to re-run.
    fn post_drain_probe(&self, new_id: u32, mode: ReloadMode) -> Option<ReadinessSource> {
        if mode != ReloadMode::Overlap {
            return None;
        }
        let config = self.sheep.get(&new_id)?.entry.spec.config();
        let source = ReadinessSource::of(config)
            .expect("ResolvedApp already passed ProbeTarget::parse in normalize");
        matches!(source, ReadinessSource::Probe(..)).then_some(source)
    }

    /// Asks the replacement, alone this time, whether it can serve, and reports
    /// back as [`Msg::ReloadVerified`].
    ///
    /// The wait is bounded by the app's own `listen_timeout`, which extends how
    /// long a reload of a `reuse_port` app can take: a deploy tool sizing its
    /// patience off `listen_timeout + graceful_timeout` has to know.
    ///
    /// A task rather than a call, since a probe is I/O and the actor loop never
    /// awaits. The reply comes back as a message.
    fn spawn_verify_task(&self, name: &str, new_id: u32, source: ReadinessSource) {
        let slot = self
            .sheep
            .get(&new_id)
            .expect("spawn_verify_task: the replacement was read a moment ago");
        let deadline = slot.entry.spec.config().listen_timeout.as_duration();
        // Re-assembled for the prober's sake alone, as `arm_extras` does:
        // `spec`, `instance` and `credentials` never change after
        // registration, so `assemble` returns this instance's own spawn spec.
        let credentials = match slot.entry.credentials {
            SpawnIdentity::Resolved(creds) => creds,
            SpawnIdentity::Unresolved => None,
        };
        let spec = assemble(
            &slot.entry.spec,
            slot.entry.instance,
            &self.paths,
            credentials,
        );
        let prober = spec_prober(&spec);

        // `await_ready` wants a channel receiver and its `Probe` arm never
        // reads one, which is the only arm reachable here. Dropping the sender
        // signals nothing; nothing on the other end is listening.
        let (_ready_tx, ready_rx) = oneshot::channel();
        let tx = self.tx.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let readiness = await_ready(&source, deadline, ready_rx, prober).await;
            let _ = tx
                .send(Msg::ReloadVerified {
                    name,
                    new_id,
                    readiness,
                })
                .await;
        });
    }

    /// The post-drain probe answered, or did not: finish the swap or abandon
    /// the reload.
    ///
    /// Exact for a single-instance app, where one process is left. Weaker for a
    /// clustered app: surviving old instances are still in the `SO_REUSEPORT`
    /// group and can answer for a bad replacement until the last swap. Closing
    /// that takes a per-instance identity in the response, which is
    /// `wait_ready`'s job.
    fn handle_reload_verified(&mut self, name: &str, new_id: u32, readiness: Readiness) {
        let Some(job) = self.reloads.get(name) else {
            return;
        };
        // Ids are never reused, so a verdict naming anything but this swap's
        // replacement belongs to a swap that has already ended.
        if job.swap.new_id != Some(new_id) || job.swap.phase != ReloadPhase::Verify {
            return;
        }
        let job = self
            .reloads
            .remove(name)
            .expect("handle_reload_verified: the job was read a moment ago");
        self.clear_reload(new_id);

        if readiness == Readiness::Ready {
            let info = to_info(&self.sheep[&new_id].entry, &self.smits);
            self.emit(ProcessEventKind::Reloaded, info, true);
            self.advance_reload(name, job.queue);
            return;
        }

        // Demoted out of `Online`: that status was written on a probe the
        // now-reaped instance may have answered. The process is not killed,
        // since a replacement that is up but not answering is more than none,
        // and nothing will restart it on its own.
        tracing::warn!(
            name,
            new_id,
            "reload abandoned: the replacement did not answer its readiness probe once the \
             instance it replaced was gone, so the probe that put it online was answered by \
             that instance"
        );
        let info = self.set_status(new_id, ProcStatus::Starting);
        if let Some(slot) = self.sheep.get_mut(&new_id) {
            slot.ready_failed = true;
        }
        self.emit(ProcessEventKind::ReloadAbandoned, info, true);
    }

    /// Abandons `name`'s reload: the instance it was replacing goes back to
    /// serving where that is still available to it, the instances it had not
    /// reached yet are left alone, and the replacement is killed and
    /// deregistered.
    ///
    /// The replacement goes through the kill ladder, since it may already have
    /// forked lambs and the `SIGKILL` rung is what sweeps the group. Its entry
    /// is deregistered rather than left `Errored`: the instance slot belongs to
    /// the drainee, and a second permanent row would double every name-keyed
    /// verb. Only reachable while the swap is still `AwaitReady`.
    fn abort_reload(&mut self, name: &str, reason: &str) {
        let Some(job) = self.reloads.remove(name) else {
            return;
        };
        // Guards the restore below and nothing else: a job outliving both of
        // its ids is a different failure, handled in `handle_exited`.
        debug_assert_eq!(
            job.swap.phase,
            ReloadPhase::AwaitReady,
            "abort_reload: a committed swap has no old instance to go back to"
        );
        tracing::warn!(
            name,
            old_id = job.swap.old_id,
            new_id = job.swap.new_id,
            reason,
            "reload abandoned"
        );

        // Read back out of the map rather than emitted inside the block: the
        // event carries the status the restore decides, and that block holds a
        // mutable borrow while it decides it.
        let kept = self.sheep.get_mut(&job.swap.old_id).map(|drainee| {
            drainee.entry.reload = ReloadState::None;
            // Restored only where going back is still available. A drainee
            // whose own exit triggered this has no task left, and one mid-kill
            // ladder holds an operator's marker: `Stopping` is honest for both,
            // and writing over it hands an operator a live pid.
            if drainee.ctl.is_some() && drainee.manual.is_none() {
                drainee.entry.status = restored_status(drainee);
            }
            to_info(&drainee.entry, &self.smits)
        });
        if let Some(info) = kept {
            self.emit(ProcessEventKind::ReloadAbandoned, info, true);
        }

        let Some(new_id) = job.swap.new_id else {
            // `DrainFirst` has no replacement, and the drainee has already been
            // put back by the block above.
            return;
        };
        let Some(replacement) = self.sheep.get_mut(&new_id) else {
            return;
        };
        replacement.entry.reload = ReloadState::None;
        if replacement.ctl.is_none() {
            // Already terminal: this abandonment is its exit being handled,
            // and `handle_exited` deregisters it. A `Kill` to an ended task
            // would claim a marker no exit will ever clear.
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

    /// Bounds the swap that has just started: after
    /// `listen_timeout + graceful_timeout + `[`RELOAD_DEADLINE_SLACK`], a
    /// `Msg::ReloadDeadline` comes back to end it if nothing else has.
    ///
    /// Every other transition out of a [`ReloadJob`] is driven by a message
    /// from a task the actor cannot make report, and [`kill_process`]'s wait
    /// after `SIGKILL` has no timeout, so a wedged instance would leave
    /// `handle_reload` refusing the app until the daemon restarts.
    ///
    /// Every swap is armed at the door it starts from, and each arming replaces
    /// the last: the fresh stamp goes on [`ReloadJob::deadline`] and older
    /// timers are dropped when they fire. `id` names any entry of the app.
    fn arm_reload_deadline(&mut self, name: &str, id: u32) {
        // Loud rather than silent: a swap that failed to arm one is the state
        // this exists to make impossible.
        let app = self
            .sheep
            .get(&id)
            .expect("arm_reload_deadline: the swap's entry was read a moment ago")
            .entry
            .spec
            .config();
        let deadline = app.listen_timeout.as_duration()
            + app.graceful_timeout.as_duration()
            + RELOAD_DEADLINE_SLACK;

        let stamp = self.next_deadline;
        self.next_deadline += 1;
        let Some(job) = self.reloads.get_mut(name) else {
            // Every caller arms with its job already in the map; without one
            // there is nothing for a watchdog to end, so arming would leak a
            // timer that could only be dropped as stale.
            debug_assert!(false, "arm_reload_deadline: no job to arm for");
            return;
        };
        job.deadline = stamp;

        let tx = self.tx.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            let _ = tx.send(Msg::ReloadDeadline { name, stamp }).await;
        });
    }

    /// A swap ran out of time: end the reload rather than leave a job nothing
    /// can remove.
    ///
    /// Stale deadlines are dropped on the swap's `new_id`; ids are never
    /// reused, so a finished swap and a later reload of the same app are both
    /// covered. Before the commit there is still an instance to go back to, so
    /// this is an ordinary abandonment; after it the job is dropped where it
    /// stands and the replacement is left as it is.
    ///
    /// The instance being replaced keeps [`ReloadState::Drainee`] through that
    /// second ending: it routes a late exit to [`Self::reap_drainee`], where a
    /// cleared marker would respawn a second live process into the slot.
    fn handle_reload_deadline(&mut self, name: &str, stamp: u64) {
        let Some(job) = self.reloads.get(name) else {
            return;
        };
        if job.deadline != stamp {
            return;
        }
        let old_id = job.swap.old_id;
        match job.swap.phase {
            ReloadPhase::DrainFirst => {
                // A serial reload whose drain never produced an exit, with no
                // replacement to kill. The marker comes off, unlike the
                // `DrainOld` arm: the slot is this instance's own, so a late
                // exit leaves the operator a `Stopped` row to restart.
                tracing::warn!(
                    name,
                    old_id,
                    "reload abandoned: the instance being drained passed the swap's deadline \
                     without exiting, so its replacement was never spawned"
                );
                self.reloads.remove(name);
                self.clear_reload(old_id);
                if let Some(slot) = self.sheep.get(&old_id) {
                    let info = to_info(&slot.entry, &self.smits);
                    self.emit(ProcessEventKind::ReloadAbandoned, info, true);
                }
            }
            ReloadPhase::AwaitReady => {
                self.abort_reload(
                    name,
                    "the swap passed its deadline with no readiness result",
                );
            }
            ReloadPhase::DrainOld | ReloadPhase::Verify => {
                let new_id = job
                    .swap
                    .new_id
                    .expect("a swap past AwaitReady has a replacement");
                tracing::warn!(
                    name,
                    old_id,
                    new_id,
                    drainee_registered = self.sheep.contains_key(&old_id),
                    "reload abandoned: the swap passed its deadline, so the message that would \
                     have ended it is not coming"
                );
                self.reloads.remove(name);
                self.clear_reload(new_id);
                if let Some(slot) = self.sheep.get(&new_id) {
                    let info = to_info(&slot.entry, &self.smits);
                    self.emit(ProcessEventKind::ReloadAbandoned, info, true);
                }
            }
        }
    }

    /// `ReapOld`: the drainee has exited, so its registration goes with it.
    ///
    /// Nothing else would remove it: a drainee is not deleted and does not
    /// respawn, so without this its `SheepSlot` outlives the process, one dead
    /// row per instance per reload. Returns what [`Self::resolve_pending`]
    /// returned, so an operator's `stop`/`delete` waiting on this exit is still
    /// answered.
    ///
    /// A [`ReloadMode::Serial`] reload's replacement is created here, before
    /// the deregistration: it inherits its instance slot, restart count,
    /// credentials and dog marker off the entry deregistration removes.
    fn reap_drainee(&mut self, old_id: u32) -> bool {
        if let Some(name) = self.serial_drain_of(old_id) {
            // An operator's `delete` can reach the instance a serial reload is
            // draining, and `DrainFirst` is the one phase with no other guard
            // against it. Without this the delete would leave a replacement
            // running under a new id for an app nobody has.
            if self.sheep[&old_id].pending_delete {
                tracing::warn!(
                    name,
                    old_id,
                    "reload abandoned: the instance being drained was deleted, so no \
                     replacement was spawned"
                );
                self.reloads.remove(&name);
            } else {
                self.spawn_serial_replacement(&name, old_id);
            }
        }
        let terminal = self.deregister_on_exit(old_id);
        let Some(name) = self.reload_of(old_id) else {
            return terminal;
        };
        match self.reloads[&name].swap.phase {
            ReloadPhase::DrainOld | ReloadPhase::Verify => self.finish_swap(&name),
            // An overlapping swap whose drainee died before the replacement was
            // ready has nothing left to abandon back to, so it commits here. A
            // serial one is in the same position by construction.
            ReloadPhase::AwaitReady => {
                self.reloads
                    .get_mut(&name)
                    .expect("reap_drainee: the phase was read a moment ago")
                    .swap
                    .phase = ReloadPhase::DrainOld;
            }
            // The serial spawn above failed and ended the job, so `reload_of`
            // found nothing. `DrainFirst` is entered once and left by the spawn
            // a few lines up.
            ReloadPhase::DrainFirst => {
                debug_assert!(false, "reap_drainee: DrainFirst outlived its own spawn");
            }
        }
        terminal
    }

    /// The app whose serial reload is draining `old_id` right now, if one is.
    ///
    /// Kept apart from the answer so the borrow of `self.reloads` ends before
    /// [`Self::reap_drainee`]'s spawn begins.
    fn serial_drain_of(&self, old_id: u32) -> Option<String> {
        self.reloads
            .iter()
            .find(|(_, job)| job.swap.phase == ReloadPhase::DrainFirst && job.swap.old_id == old_id)
            .map(|(name, _)| name.clone())
    }

    /// `SpawnNew`, for a serial reload: the instance is drained, so put its
    /// replacement in the slot it has just left.
    ///
    /// The mirror of the `Ok`/`Err` pair in [`Self::advance_reload`]'s overlap
    /// arm. On success the swap moves to `AwaitReady` and arms a watchdog that
    /// makes the drain's own stale. On failure nothing is still serving, so the
    /// abandonment names an instance that is already dead.
    fn spawn_serial_replacement(&mut self, name: &str, old_id: u32) {
        match self.spawn_replacement(old_id, ReloadMode::Serial) {
            Ok(new_id) => {
                let job = self
                    .reloads
                    .get_mut(name)
                    .expect("spawn_serial_replacement: the job was read a moment ago");
                job.swap.new_id = Some(new_id);
                job.swap.phase = ReloadPhase::AwaitReady;
                self.arm_reload_deadline(name, new_id);
            }
            Err(error) => {
                tracing::warn!(
                    name,
                    old_id,
                    error,
                    "reload abandoned: the instance was drained and its replacement could not \
                     be spawned, so the instance slot is empty"
                );
                self.reloads.remove(name);
                if let Some(slot) = self.sheep.get(&old_id) {
                    let info = to_info(&slot.entry, &self.smits);
                    self.emit(ProcessEventKind::ReloadAbandoned, info, true);
                }
            }
        }
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
        let info = to_info(&removed.entry, &self.smits);
        self.emit(ProcessEventKind::Delete, info.clone(), true);
        self.disarm_extras(id, &info.name);
        self.resolve_pending(id, info)
    }

    /// The app whose swap `id` is half of, while that swap has not committed
    /// yet: the window in which ending either half loses the reload's overlap.
    ///
    /// The one spelling of that rule, shared by both of
    /// [`Self::handle_exited`]'s reload arms and by [`Self::begin_manual`].
    fn uncommitted_swap_of(&self, id: u32) -> Option<String> {
        self.reloads
            .iter()
            .find(|(_, job)| {
                job.swap.phase == ReloadPhase::AwaitReady
                    && (job.swap.old_id == id || job.swap.new_id == Some(id))
            })
            .map(|(name, _)| name.clone())
    }

    /// Whether `id` is half of a swap that has not committed yet; see
    /// [`Self::uncommitted_swap_of`].
    fn in_an_uncommitted_swap(&self, id: u32) -> bool {
        self.uncommitted_swap_of(id).is_some()
    }

    /// The app whose in-flight reload names `id`, in either role.
    fn reload_of(&self, id: u32) -> Option<String> {
        self.reloads
            .iter()
            .find(|(_, job)| job.swap.old_id == id || job.swap.new_id == Some(id))
            .map(|(name, _)| name.clone())
    }

    /// Takes `id` out of any reload it is half of, leaving an ordinary entry.
    fn clear_reload(&mut self, id: u32) {
        if let Some(slot) = self.sheep.get_mut(&id) {
            slot.entry.reload = ReloadState::None;
        }
    }

    /// Resolves `selector` and hands every pump writing to a matched sheep's
    /// log paths to a task that reopens their files and then answers the
    /// caller.
    ///
    /// Nothing is awaited here: awaiting a pump inside the actor loop
    /// deadlocks, since the actor stops draining the mailbox its answer must
    /// come through. `&self` keeps the handler free of state changes, which is
    /// what lets it skip the epoch check.
    ///
    /// A matched sheep with no pump is a success. The pumps asked are every
    /// slot writing to a path a matched sheep writes to, matched or not; the
    /// reply stays keyed by the selector.
    fn handle_reopen(
        &self,
        selector: &ProcessSelector,
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    ) {
        let mut matched: Vec<ProcessInfo> = Vec::new();
        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
        for id in self.matching_ids(selector) {
            let slot = self
                .sheep
                .get(&id)
                .expect("`matching_ids` answers with ids read off this map a moment ago");
            paths.insert(slot.entry.out_file.clone());
            paths.insert(slot.entry.err_file.clone());
            matched.push(to_info(&slot.entry, &self.smits));
        }

        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        let mut pumps: Vec<(ProcessInfo, mpsc::Sender<LogCtl>)> = self
            .sheep
            .values()
            .filter(|slot| {
                paths.contains(&slot.entry.out_file) || paths.contains(&slot.entry.err_file)
            })
            .filter_map(|slot| {
                slot.log_ctl
                    .clone()
                    .map(|log_ctl| (to_info(&slot.entry, &self.smits), log_ctl))
            })
            .collect();

        // `HashMap` iteration order is arbitrary and pump failures are
        // reported in collection order, so an unsorted set would word a
        // multi-pump failure differently run to run. Id order is fine here,
        // unlike `matched` below: this list is never rendered.
        pumps.sort_unstable_by_key(|(info, _)| info.id);
        // The table `shep reopen` prints, so it takes the listing order.
        sort_flock(&mut matched);
        spawn_reopen_task(matched, pumps, reply);
    }

    /// Reads everything a handover needs off the actor and hands it to a
    /// task that asks each live pump for its descriptors.
    ///
    /// Synchronous and `&self` for the reason [`Self::handle_reopen`] gives.
    /// The entries are cloned because the assembly runs after the loop, which
    /// is what [`OwnedCandidate`](OwnedCandidate) exists for.
    ///
    /// Every registered sheep is described, with no selector: a handover
    /// carries one process image, so the flock goes whole or not at all.
    #[cfg(unix)]
    fn handle_handover_snapshot(
        &self,
        fds: DaemonFds,
        reply: oneshot::Sender<Result<Snapshot, SupervisorError>>,
    ) {
        let mut drafts: Vec<HandoverDraft> = self
            .sheep
            .values()
            .map(|slot| HandoverDraft {
                entry: slot.entry.clone(),
                // Whole, not `is_some()`: the kind decides what the
                // successor's `handle_exited` makes of this exit, the origin
                // what its bus events say caused it.
                manual: slot.manual,
                pending_delete: slot.pending_delete,
                epoch: slot.epoch,
                ready_failed: slot.ready_failed,
                restart_due: slot.restart_due,
                log_ctl: slot.log_ctl.clone(),
                channel_open: slot.open_channel().is_some(),
            })
            .collect();
        // `HashMap` iteration order is arbitrary; id order makes two
        // snapshots of an unchanged flock identical.
        drafts.sort_unstable_by_key(|draft| draft.entry.id);
        // Read in the same synchronous step as the entries above: a job and
        // the two entries it names are one picture, and a swap could otherwise
        // finish between the two halves.
        let mut reloads: Vec<CarriedReload> = self
            .reloads
            .iter()
            .map(|(app, job)| CarriedReload {
                app: app.clone(),
                queue: job.queue.iter().copied().collect(),
                mode: job.mode,
                swap: job.swap,
                // `ReloadJob::deadline` is absent: it stamps a timer that
                // dies with this image, and the successor re-stamps from the
                // carried `next_deadline`.
            })
            .collect();
        // `HashMap` iteration order is arbitrary, for the reason the sheep
        // are sorted above.
        reloads.sort_unstable_by(|left, right| left.app.cmp(&right.app));
        spawn_handover_task(
            drafts,
            fds,
            Counters {
                next_id: self.next_id,
                next_deadline: self.next_deadline,
                next_action_stamp: self.next_action_stamp,
            },
            reloads,
            reply,
        );
    }

    /// Answers whether every sheep this actor holds could be carried across
    /// a handover.
    ///
    /// On the actor loop, unlike [`Self::handle_handover_snapshot`]: this
    /// awaits nothing. Visited in id order, so a flock with two unsupported
    /// sheep names the same one every time.
    #[cfg(unix)]
    fn handle_handover_fitness(&self, reply: oneshot::Sender<Result<Fitness, SupervisorError>>) {
        let mut slots: Vec<&SheepSlot> = self.sheep.values().collect();
        slots.sort_unstable_by_key(|slot| slot.entry.id);
        let candidates: Vec<Candidate<'_>> = slots
            .iter()
            .map(|slot| Candidate {
                entry: &slot.entry,
                // Always `false`: this gate awaits nothing, so it cannot ask
                // a pump, and a pump wedged now may answer by the time a
                // SIGHUP arrives. `spawn_handover_task` holds the real gate.
                pump_unresponsive: false,
            })
            .collect();
        let _ = reply.send(Ok(fitness(&candidates)));
    }

    /// Resolves `selector` and hands every match to a task that flushes its
    /// log pump, truncates its log files and then answers the caller.
    /// Synchronous and `&self` for the reason [`Self::handle_reopen`] gives.
    ///
    /// The paths are [`ProcessEntry::out_file`]/[`ProcessEntry::err_file`],
    /// never the inode the pump holds: chasing the handle after an external
    /// rotator's rename would empty the archive and leave the live log alone.
    /// A stopped sheep has no pump and is still flushed.
    ///
    /// Every slot writing to one of those paths is flushed, matched or not: an
    /// unflushed sibling's dispatched `write(2)` would land at offset 0 of the
    /// file just reported empty. The reply stays keyed by the selector.
    fn handle_flush(
        &self,
        selector: &ProcessSelector,
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    ) {
        let mut matched: Vec<ProcessInfo> = Vec::new();
        let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
        for id in self.matching_ids(selector) {
            let slot = self
                .sheep
                .get(&id)
                .expect("`matching_ids` answers with ids read off this map a moment ago");
            paths.insert(slot.entry.out_file.clone());
            paths.insert(slot.entry.err_file.clone());
            matched.push(to_info(&slot.entry, &self.smits));
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

        // Sorted for the reason `handle_reopen` sorts: `HashMap` iteration
        // order is arbitrary and failures are reported in collection order.
        pumps.sort_unstable_by_key(|&(id, _)| id);
        // The table `shep empty` prints, so it takes the listing order.
        sort_flock(&mut matched);
        let pumps = pumps.into_iter().map(|(_, log_ctl)| log_ctl).collect();
        spawn_flush_task(matched, pumps, paths, reply);
    }

    /// Kills every currently-online sheep, deferring the reply the same way
    /// `Stop` does; returns `true` (break the actor loop) once there was
    /// nothing to wait on, or once the deferred reply resolves.
    ///
    /// Sets `shutting_down` before computing `online`, so nothing outside that
    /// snapshot can ever need killing later: every downstream check against
    /// the flag rests on no sheep registering or respawning from here on.
    fn begin_shutdown(&mut self, reply: oneshot::Sender<()>) -> bool {
        self.shutting_down = true;
        // Every in-flight reload is abandoned: its next step is always a
        // spawn, forbidden from here on. Both halves are `ctl.is_some()`, so
        // both are killed below; the replacement stops registered, and the
        // drainee still carries `ReloadState::Drainee` and is deregistered.
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
            // Same marker rule as `begin_manual`: an id already mid-kill
            // under an operator's command keeps its marker; one held by an
            // automatic restart is taken over. It joins `remaining` either way.
            self.claim_manual(
                id,
                PendingManual {
                    kind: ManualKind::Stop,
                    origin: CommandOrigin::Operator,
                },
                // A shutdown is a stop even for a draining instance: the
                // longer cap buys time to finish work.
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
        // Goes with `ctl`: the writer task cannot notice a child that exited
        // quietly, and would hold the daemon's half of the socketpair for as
        // long as the entry lived. See `SheepSlot::to_child`.
        slot.to_child = None;
        // Same: a sheep task parked on `signal_rx.recv()` would hold this
        // sender's receiver open. See `SheepSlot::signals`.
        slot.signals = None;
        // Same: the writer task parks on `recv()`. See `SheepSlot::to_stdin`.
        slot.to_stdin = None;
        // The one place a process under a registered id stops existing, so a
        // wait cannot survive into a second process's life.
        slot.actions.abandon_all();
        slot.entry.pid = None;
        // Cleared for the reason `pid` is: it is a verdict about a process.
        // Left set, `reload_eligible` would let a reload drain a row with no
        // process behind it. See `SheepSlot::ready_failed`.
        slot.ready_failed = false;
        // Set before any branch below decides what this exit becomes, and
        // unconditionally: an operator's own `stop` reaches this line exactly
        // as a crash does. A branch that removes the entry carries the value
        // into the `ProcessInfo` its removal emits.
        slot.entry.last_exit = Some(outcome.into());
        // `kind` decides what this exit becomes; the origin decides only what
        // the bus says caused it, and is read once, on the forced-respawn
        // branch. The Stop and Delete branches stay literal `true`: every site
        // that puts either kind on a marker declares `Operator`.
        let manual = slot.manual.take();
        let kind = manual.map(|pending| pending.kind);
        let pending_delete = std::mem::take(&mut slot.pending_delete);
        // Read off the slot with the borrow already open, because both
        // branches below hand the actor back to itself.
        let reload = slot.entry.reload;
        let started_at = slot.entry.started_at.take();

        // Neither half of a reload takes the ordinary decision path.
        // `decide_on_exit` knows nothing about reloads, so an `autorestart`
        // app's drainee would be respawned into its replacement's slot.
        match reload {
            ReloadState::Drainee { .. } => {
                // The drainee's slot belongs to its replacement, so its
                // registration goes with the process, unless an operator's own
                // command reached it first: `stop` must leave a sheep
                // registered and `Stopped`, not deleted.
                match self.uncommitted_swap_of(id) {
                    Some(name) if kind.is_some() => {
                        self.abort_reload(&name, "an operator's command reached the drainee first");
                        // Falls through as an ordinary entry: `abort_reload`
                        // cleared the marker and put the status back.
                    }
                    _ => return self.reap_drainee(id),
                }
            }
            ReloadState::Replacement => {
                // Still `AwaitReady` means the replacement never proved it
                // could take over, so the reload is abandoned and the drainee
                // kept. Past that the swap is committed and this is an
                // ordinary instance, whose exit is its restart policy's.
                self.clear_reload(id);
                if let Some(name) = self.uncommitted_swap_of(id) {
                    self.abort_reload(&name, "the replacement exited before it was ready");
                    return self.deregister_on_exit(id);
                }
                // A committed swap normally ends on the drainee's exit, but
                // one `reap_drainee` committed has none left, and the marker
                // cleared above cancels the readiness route. So the job ends
                // here or never, and one nothing can end blocks every reload.
                if let Some(name) = self.reload_of(id) {
                    let old_id = self.reloads[&name].swap.old_id;
                    if !self.sheep.contains_key(&old_id) {
                        debug_assert_ne!(
                            self.sheep.get(&id).map(|slot| slot.entry.status),
                            Some(ProcStatus::Online),
                            "a swap committed by the drainee's death cannot have had a live \
                             replacement"
                        );
                        tracing::warn!(
                            name,
                            new_id = id,
                            "reload abandoned: the replacement exited before it was ready, with \
                             the instance it replaced already gone"
                        );
                        self.reloads.remove(&name);
                        if let Some(slot) = self.sheep.get(&id) {
                            let info = to_info(&slot.entry, &self.smits);
                            self.emit(ProcessEventKind::ReloadAbandoned, info, true);
                        }
                    }
                }
            }
            ReloadState::None => {}
        }

        let Some(started_at) = started_at else {
            // Should not happen: a duplicate `Msg::Exited` would violate the
            // one-exit-path invariant. Any pending reply is still resolved
            // with a best-effort snapshot rather than parked forever.
            tracing::warn!(
                id,
                "Msg::Exited for an entry with no started_at (duplicate?)"
            );
            // Unreachable today: `pending_delete` is only set for a sheep
            // with `ctl.is_some()`. Honoured anyway, because the takes above
            // have consumed both markers, so a Delete would otherwise be
            // dropped while its caller was told it succeeded.
            if kind == Some(ManualKind::Delete) || pending_delete {
                let mut removed = self.sheep.remove(&id).expect("checked above");
                removed.entry.status = ProcStatus::Stopped;
                let info = to_info(&removed.entry, &self.smits);
                self.emit(ProcessEventKind::Delete, info.clone(), true);
                self.disarm_extras(id, &info.name);
                return self.resolve_pending(id, info);
            }
            let info = to_info(
                &self.sheep.get(&id).expect("checked above").entry,
                &self.smits,
            );
            return self.resolve_pending(id, info);
        };
        let uptime = tokio::time::Instant::now().saturating_duration_since(started_at);

        // A manual Restart forces a respawn, where `decide_on_exit` would
        // choose CleanStop off `manual_stop`. Not while shutting down, which
        // would orphan a child outside the shutdown snapshot; not under
        // `pending_delete`, which would hand back a live process as deleted.
        if kind == Some(ManualKind::Restart) && !self.shutting_down && !pending_delete {
            // Re-fetched rather than carried down: the reload branches above
            // hand `self` back to itself, ending the earlier borrow.
            self.sheep
                .get_mut(&id)
                .expect("checked above")
                .entry
                .budget
                .reset();
            // The one place the origin is read. A `shep restart` is a user
            // action; cron, watch, memory and liveness restarts are the
            // daemon's own, and a subscriber told otherwise cannot tell a
            // deploy from an app thrashing.
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
                // Capture the current epoch so this timer can tell, when it
                // fires, whether it is still this id's authoritative one.
                let epoch = self.sheep.get(&id).expect("checked above").epoch;
                // In the one clock that survives an `execve`, stamped here
                // because `install_adopted` re-arms a deadline this produced.
                // `checked_add` avoids a panic on overflow; overflow or a
                // pre-epoch result falls back to the whole delay.
                self.sheep.get_mut(&id).expect("checked above").restart_due = SystemTime::now()
                    .checked_add(delay.unwrap_or(Duration::ZERO))
                    .filter(|due| due.duration_since(SystemTime::UNIX_EPOCH).is_ok());
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
                let info = to_info(&removed.entry, &self.smits);
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

    /// A memory breach or a liveness failure asked for a restart.
    ///
    /// Guarded on the pid rather than [`SheepSlot`]'s respawn epoch: the pid is
    /// on both reports and is as good a generation token. A liveness report
    /// carries its probe's own epoch too, since `InstanceExtras::disarm` does
    /// not await the aborted task, so a probe already inside `failures.send`
    /// can deliver against the same pid in the same status, and a config apply
    /// must never kill a process. A memory breach re-asks the ceiling instead.
    ///
    /// Delegates to `begin_manual`, not `respawn`, which keeps the kill ladder
    /// and the budget reset; it goes in as [`CommandOrigin::Automatic`], so an
    /// operator's `stop` can take the sheep back off a restart mid-ladder.
    fn handle_extra_restart(
        &mut self,
        id: u32,
        pid: u32,
        epoch: Option<u64>,
        observed: Option<MemSize>,
    ) {
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
        // A probe replaced because its config changed leaves the pid and the
        // status untouched, so a failure already in flight passes both guards
        // above. `epoch` is `None` for a memory breach, which has no
        // per-instance task that can go stale.
        if let Some(epoch) = epoch {
            let current = self.registry.liveness_epoch(id);
            if current != epoch {
                tracing::debug!(
                    id,
                    pid,
                    epoch,
                    current,
                    "extra restart dropped: the reporting probe has been replaced"
                );
                return;
            }
        }
        // A breach is computed under `PollingEnforcer`'s lock and sent after
        // it is released, so a ceiling re-armed in between leaves a report in
        // flight against a ceiling nobody enforces. Re-asking rather than
        // comparing ceilings: a lowered one makes an old measurement real.
        if let Some(observed) = observed {
            let ceiling = self
                .sheep
                .get(&id)
                .and_then(|slot| slot.entry.spec.config().max_memory);
            if ceiling.is_none_or(|limit| observed <= limit) {
                tracing::debug!(
                    id,
                    pid,
                    observed = observed.bytes(),
                    "extra restart dropped: the ceiling it breached is no longer in force"
                );
                return;
            }
        }
        // A throwaway reply: the reporter is fire-and-forget by contract.
        let (reply, _dropped) = oneshot::channel();
        self.begin_manual(
            ProcessSelector::Id(id),
            ManualKind::Restart,
            CommandOrigin::Automatic,
            ReplyKind::Info(reply),
        );
    }

    /// A scheduled restart's backoff elapsed.
    ///
    /// Dropped while shutting down, since nothing here would be in the
    /// shutdown's `online` snapshot. Dropped unless the entry is still
    /// `WaitingRestart`, which a manual command may have intercepted and which
    /// also excludes a reload's drainee, and unless the epoch still matches: a
    /// respawn since this timer was scheduled makes it stale even though the
    /// sheep is legitimately `WaitingRestart` again, under a newer backoff.
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

    /// Forwards the shepherd channel's readiness signal to `id`'s waiting
    /// readiness task, if one is waiting. A `Ready` with no live wait is
    /// dropped silently: an app may write `{"kind":"ready"}` twice.
    fn handle_ready_signal(&mut self, id: u32) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        if let Some(tx) = slot.ready_tx.take() {
            let _ = tx.send(());
        }
    }

    /// A readiness wait resolved.
    ///
    /// Dropped while shutting down, or when the slot is gone, its epoch has
    /// moved, or its status has left `Starting`. Without those, a sheep that
    /// exited and respawned while its readiness task was still waiting would
    /// have the old wait mark the new process online.
    ///
    /// Past the guards the wait belongs to one of two callers, which want
    /// opposite things from a deadline that elapsed; see
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
        if matches!(slot.entry.reload, ReloadState::Replacement) {
            self.reload_ready_result(id, manually, readiness);
            return;
        }
        if readiness == Readiness::TimedOut {
            // Online anyway: treating a readiness timeout as a spawn failure
            // would turn a slow-starting app into a restart loop.
            tracing::warn!(id, "readiness deadline elapsed; marking online anyway");
        }
        let info = self.set_status(id, ProcStatus::Online);
        // `manually` comes from the spawn that armed this wait, so gating an
        // app changes only when `Online` fires, never what the event says
        // caused it.
        self.went_online(id, info, manually);
    }

    /// Resolves `selector`, puts one action on the shepherd channel of every
    /// matched sheep that can take one, and answers with a row per match once
    /// the last of those waits has ended.
    ///
    /// Shaped like [`Self::begin_manual`], except that the rows are joined in
    /// [`spawn_trigger_task`] rather than in `pending`. Nothing is awaited
    /// here, for the reason [`Self::handle_reopen`] gives.
    ///
    /// Both refusals are decided ahead of the wait, since a sheep refused
    /// after one was armed would leave a wait nothing drives home: no open
    /// [`SheepSlot::open_channel`] is [`ActionOutcome::NoChannel`], a drainee
    /// is [`ActionOutcome::Skipped`]. A replacement is not skipped.
    fn begin_action(
        &mut self,
        selector: &ProcessSelector,
        action: String,
        params: Option<String>,
        reply: oneshot::Sender<Result<Vec<ActionReply>, SupervisorError>>,
    ) {
        // `matching_ids` answers in id order, so delivery is not arbitrary;
        // the answer's own order rests on `spawn_trigger_task`'s final sort.
        let matched = self.matching_ids(selector);

        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        let mut refused = Vec::new();
        let mut waits = Vec::new();
        for id in matched {
            let slot = self
                .sheep
                .get(&id)
                .expect("begin_action: `matched` holds ids read off this map a moment ago");
            let config = slot.entry.spec.config();
            let name = config.name.clone();
            // Each sheep's own `action_timeout` bounds its own wait.
            let action_timeout = config.action_timeout.as_duration();
            if matches!(slot.entry.reload, ReloadState::Drainee { .. }) {
                refused.push(ActionReply {
                    id,
                    name,
                    outcome: ActionOutcome::Skipped,
                });
                continue;
            }
            let Some(to_child) = slot.open_channel().cloned() else {
                refused.push(ActionReply {
                    id,
                    name,
                    outcome: ActionOutcome::NoChannel,
                });
                continue;
            };
            let answer =
                self.arm_action(id, to_child, action.clone(), params.clone(), action_timeout);
            waits.push((id, name, answer));
        }

        if waits.is_empty() {
            // Not a silent success. The rows say what happened to each sheep;
            // this says it once in the daemon's own log, where a flock with
            // `channel` unset everywhere is one cause rather than a table of
            // refusals.
            let skipped = refused
                .iter()
                .filter(|row| row.outcome == ActionOutcome::Skipped)
                .count();
            tracing::warn!(
                action,
                matched = refused.len(),
                skipped,
                "no matched sheep could take this action; nothing was delivered"
            );
            let _ = reply.send(Ok(refused));
            return;
        }

        spawn_trigger_task(refused, waits, reply);
    }

    /// Delivers one signal to every matched sheep's own process.
    ///
    /// Off the actor loop, like [`Self::begin_action`]: each delivery is a
    /// round trip through a sheep task. There is nothing to wait out, so the
    /// fan-out is bounded by the syscall rather than a configured timeout.
    ///
    /// A sheep with no live task answers [`SignalOutcome::NotRunning`] without
    /// a round trip: `slot.signals` is `None` for exactly the states with no
    /// process. A reload drainee is signalled like any other live sheep,
    /// unlike in [`Self::begin_action`]: a signal expects nothing back.
    fn begin_signal(
        &mut self,
        selector: &ProcessSelector,
        sig: OperatorSignal,
        reply: oneshot::Sender<Result<Vec<SignalReply>, SupervisorError>>,
    ) {
        let matched = self.matching_ids(selector);
        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        let mut settled = Vec::new();
        let mut waits = Vec::new();
        for id in matched {
            let slot = self
                .sheep
                .get(&id)
                .expect("begin_signal: `matched` holds ids read off this map a moment ago");
            let name = slot.entry.spec.config().name.clone();
            let Some(signals) = slot.signals.clone() else {
                settled.push(SignalReply {
                    id,
                    name,
                    outcome: SignalOutcome::NotRunning,
                });
                continue;
            };
            let (done, answer) = oneshot::channel();
            if signals.try_send(SignalRequest { sig, done }).is_err() {
                // A full queue means this sheep's task has not drained several
                // signals, which for a syscall-fast handler means it is busy
                // dying; a closed one means it already has.
                settled.push(SignalReply {
                    id,
                    name,
                    outcome: SignalOutcome::NotRunning,
                });
                continue;
            }
            waits.push((id, name, answer));
        }

        spawn_signal_task(settled, waits, reply);
    }

    /// Writes one line to every matched sheep's stdin. Off the actor loop,
    /// like [`Self::begin_signal`]: each write is a round trip through a sheep
    /// task.
    ///
    /// A sheep with no live task, or one running without `stdin = true`,
    /// answers [`LineOutcome::NoStdin`] off [`SheepSlot::open_stdin`], the one
    /// fact that decides whether a pipe exists. A reload drainee gets the
    /// line, as [`Self::begin_signal`] delivers a signal to one.
    ///
    /// The enqueue is `try_send`, never an awaited send: a full queue is the
    /// condition [`LineOutcome::NotWritten`] names, and awaiting into one
    /// would park the actor loop on a wedged sheep's pipe.
    fn begin_send_line(
        &mut self,
        selector: &ProcessSelector,
        line: String,
        reply: oneshot::Sender<Result<Vec<LineReply>, SupervisorError>>,
    ) {
        let matched = self.matching_ids(selector);
        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        let mut settled = Vec::new();
        let mut waits = Vec::new();
        for id in matched {
            let slot = self
                .sheep
                .get(&id)
                .expect("begin_send_line: `matched` holds ids read off this map a moment ago");
            let name = slot.entry.spec.config().name.clone();
            let Some(to_stdin) = slot.open_stdin().cloned() else {
                settled.push(LineReply {
                    id,
                    name,
                    outcome: LineOutcome::NoStdin,
                });
                continue;
            };
            let (done, answer) = oneshot::channel();
            match to_stdin.try_send(StdinWrite {
                line: line.clone(),
                done,
            }) {
                Ok(()) => waits.push((id, name, answer)),
                // The queue is full: the writer is blocked on a pipe the app
                // is not draining. The reason names no duration, since
                // `try_send` looked at the queue once and an operator would
                // read an elapsed time as the timeout path's bound.
                Err(mpsc::error::TrySendError::Full(_)) => settled.push(LineReply {
                    id,
                    name,
                    outcome: LineOutcome::NotWritten {
                        reason: "the app is not reading its stdin (its queue was \
                                 already full when this line arrived)"
                            .to_string(),
                    },
                }),
                // Closed: the writer task is gone, so the process is too.
                Err(mpsc::error::TrySendError::Closed(_)) => settled.push(LineReply {
                    id,
                    name,
                    outcome: LineOutcome::NoStdin,
                }),
            }
        }

        spawn_send_line_task(settled, waits, reply);
    }

    /// Puts one action on `id`'s shepherd channel and arms the wait for its
    /// reply, handing back the receiver its outcome will arrive on.
    ///
    /// Infallible: every question an action can be refused over is answered in
    /// [`Self::begin_action`]'s selector pass, before this is called.
    fn arm_action(
        &mut self,
        id: u32,
        to_child: mpsc::Sender<ShepherdMessage>,
        action: String,
        params: Option<String>,
        timeout: Duration,
    ) -> oneshot::Receiver<ActionOutcome> {
        let (reply, answer) = oneshot::channel();
        let stamp = self.next_action_stamp;
        self.next_action_stamp += 1;
        let waiter = spawn_action_task(
            id,
            stamp,
            ShepherdMessage::Action {
                name: action.clone(),
                params,
                id: stamp,
            },
            to_child,
            timeout,
            self.tx.clone(),
        );
        // After the task is spawned, and safely: its first act is a send that
        // must reach a child and come back through `run_sheep`, none of which
        // can happen before this handler returns.
        self.sheep
            .get_mut(&id)
            .expect("arm_action: the slot was read a moment ago")
            .actions
            .arm(PendingAction {
                stamp,
                action,
                waiter: Some(waiter),
                reply,
            });
        answer
    }

    /// Forwards one shepherd-channel reply to the action wait it belongs to,
    /// if it belongs to one. A reply with nowhere to go is dropped silently;
    /// see [`ActionWaits::answer`].
    fn handle_action_reply(&mut self, id: u32, action: &str, body: String, stamp: Option<u64>) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        if let Some(waiter) = slot.actions.answer(action, stamp) {
            let _ = waiter.send(body);
        }
    }

    /// An action wait resolved: answer its caller.
    ///
    /// Guarded on the stamp alone, where [`Self::handle_ready_result`] guards
    /// on four things: an action's result changes no flock state, and no
    /// respawn can make the stamp ambiguous.
    fn handle_action_result(&mut self, id: u32, stamp: u64, outcome: ActionOutcome) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        if let Some(reply) = slot.actions.resolve(stamp) {
            let _ = reply.send(outcome);
        }
    }

    /// Spawns the backoff timer for a scheduled restart. `None` still hops
    /// through a task and a mailbox send, so an immediate restart stays
    /// observable as a scheduling step.
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
                sort_flock(&mut pending.results);
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
    /// The single arming site, reached by all three transitions. Arming
    /// happens at the transition, not the spawn: a liveness probe armed
    /// against an app that has not finished starting fails its threshold and
    /// restarts the app before it ever comes up.
    fn went_online(&mut self, id: u32, info: ProcessInfo, manually: bool) {
        // Whatever an earlier reload concluded about this instance, it is
        // serving now. See `SheepSlot::ready_failed`.
        if let Some(slot) = self.sheep.get_mut(&id) {
            slot.ready_failed = false;
        }
        self.emit(ProcessEventKind::Online, info, manually);
        self.arm_extras(id);
    }

    /// Arms `id`'s lifecycle extras, re-assembling the spec the running
    /// process was spawned from.
    ///
    /// Re-assembly is what makes one arming site possible:
    /// `handle_ready_result` holds an id and nothing else, and `assemble` is
    /// pure over a `spec`, `instance` and `credentials` that never change.
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
        // This spec is assembled for `spec_prober` alone and never spawned, so
        // the unresolved arm costs nothing: extras are armed only for a
        // process already running, which resolved its identity to start.
        let credentials = match slot.entry.credentials {
            SpawnIdentity::Resolved(creds) => creds,
            SpawnIdentity::Unresolved => None,
        };
        let spec = assemble(
            &slot.entry.spec,
            slot.entry.instance,
            &self.paths,
            credentials,
        );
        self.registry
            .arm(&slot.entry, spec_prober(&spec), extras, &supervisor);
    }

    /// Disarms `id`'s lifecycle extras, and its name-group's cron worker and
    /// watch when `id` was the last armed instance of `name`.
    ///
    /// Called from every terminal transition: `respawn_failed`,
    /// `apply_immediate`'s Stop and Delete arms, and each of `handle_exited`'s
    /// four. `spawn_fresh`'s `Err` arm is the one terminal `Errored` that does
    /// not disarm, its id fresh from `next_id` and never armed. A sheep on its
    /// way to `WaitingRestart` keeps its arming, the respawn replacing its
    /// liveness loop.
    ///
    /// Re-disarming is a no-op, so a duplicate site costs nothing and a
    /// missing one leaks a task.
    fn disarm_extras(&mut self, id: u32, name: &str) {
        self.registry.disarm(id, name);
    }

    /// Sets `id`'s status and returns its refreshed snapshot.
    fn set_status(&mut self, id: u32, status: ProcStatus) -> ProcessInfo {
        let slot = self.sheep.get_mut(&id).expect("set_status: unknown id");
        slot.entry.status = status;
        to_info(&slot.entry, &self.smits)
    }

    /// Paints or clears one sheep's smit and answers with that sheep's
    /// instances as they now stand.
    ///
    /// Every instance of the name, not one row: the map is keyed by name, so
    /// every instance carries the mark.
    fn handle_set_smit(
        &mut self,
        conn: ConnId,
        sheep: &str,
        smit: Option<Smit>,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        if !self
            .sheep
            .values()
            .any(|slot| slot.entry.spec.config().name == sheep)
        {
            // Refused rather than stored against a name nothing holds: an
            // orphan entry would show in no listing.
            return Err(SupervisorError::NotFound);
        }
        match smit {
            Some(smit) => {
                self.smits
                    .insert(sheep.to_string(), (conn, smit.to_string()));
            }
            // Only this connection's own; see `Command::SetSmit`'s doc.
            None => {
                if self
                    .smits
                    .get(sheep)
                    .is_some_and(|(painter, _)| *painter == conn)
                {
                    self.smits.remove(sheep);
                }
            }
        }
        Ok(self
            .snapshot_all()
            .into_iter()
            .filter(|info| info.name == sheep)
            .collect())
    }

    /// Full flock listing, grouped by app name.
    ///
    /// Sorted by [`sort_flock`], the one rule every operator-facing listing in
    /// shep takes: name, then instance slot, then id. The slot is in the key
    /// because a replacement gets a fresh id at the drainee's slot number, so
    /// id alone puts slot 0 last. A listing whose rows all report `None` for
    /// [`ProcessInfo::instance`] collapses to `(name, id)` order.
    ///
    /// Applied here once rather than per verb: every listing reply is built
    /// from this function, so the metrics dog and bark read the operator's
    /// order.
    fn snapshot_all(&self) -> Vec<ProcessInfo> {
        let mut listing: Vec<ProcessInfo> = self
            .sheep
            .values()
            .map(|slot| to_info(&slot.entry, &self.smits))
            .collect();
        sort_flock(&mut listing);
        listing
    }

    /// Broadcasts one lifecycle transition. Send failures (no receivers)
    /// are not an error: the bus is fire-and-forget from the actor's side.
    fn emit(&self, event: ProcessEventKind, info: ProcessInfo, manually: bool) {
        let _ = self.events.send(SharedEvent::new(BusEvent::Process {
            event,
            info,
            manually,
            at_ms: crate::now_ms(),
        }));
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
///
/// Takes the smit map rather than hanging off `&self`: several call sites hold
/// a `&mut` borrow of `self.sheep` here, and a method on `Actor` would borrow
/// the whole actor. A free function lets the two fields stay disjoint.
fn to_info(entry: &ProcessEntry, smits: &Smits) -> ProcessInfo {
    let uptime_ms = entry.started_at.map_or(0, |started_at| {
        tokio::time::Instant::now()
            .saturating_duration_since(started_at)
            .as_millis() as u64
    });
    ProcessInfo::builder(entry.id, entry.spec.config().name.clone(), entry.status)
        .pid(entry.pid)
        .restarts(entry.restarts)
        .uptime_ms(uptime_ms)
        .fold(entry.spec.config().fold.clone())
        // Lossy on purpose: a non-UTF-8 log path must not fail serialization
        // of the whole reply and blank the listing for every other sheep.
        .out_file(Some(entry.out_file.to_string_lossy().into_owned()))
        .err_file(Some(entry.err_file.to_string_lossy().into_owned()))
        // Filled in by the RPC layer for the two verbs that read resource
        // usage: the numbers cost a syscall walk over the host's whole process
        // table, and the actor must never block.
        .cpu_percent(None)
        .memory_bytes(None)
        .dog(entry.dog.clone())
        .last_exit(entry.last_exit)
        // By name: every instance shows the same mark, including one spawned
        // after it was painted.
        .smit(
            smits
                .get(&entry.spec.config().name)
                .map(|(_, smit)| smit.clone()),
        )
        .instance(Some(entry.instance))
        // The field names `spec` and `pending` differ on, or `None` when
        // nothing is parked. Emptiness collapses to `None` here rather than
        // in `pending_fields`: a parked config identical to `spec` must
        // report `None`, per `ProcessInfo::pending`.
        .pending({
            let fields = pending_fields(entry);
            (!fields.is_empty()).then_some(fields)
        })
        // Read off the cached field, never the override store: every path
        // that can register or replace a sheep keeps `ProcessEntry::overridden`
        // correct, so this listing path does no I/O.
        .overridden((!entry.overridden.is_empty()).then(|| entry.overridden.clone()))
        .build()
}

/// Converts the spawn-runner's own exit observation into the wire-facing shape
/// `ProcessEntry::last_exit` stores.
///
/// A separate `From` rather than [`ExitOutcome`] on the wire directly: that
/// type lives behind the [`ProcessRunner`] seam and is free to grow without
/// dragging a breaking wire change behind it.
impl From<ExitOutcome> for ExitInfo {
    fn from(outcome: ExitOutcome) -> Self {
        Self {
            code: outcome.code,
            signal: outcome.signal,
        }
    }
}

/// Whether this instance's status lets a reload replace it.
///
/// `Online` is the ordinary answer; `ready_failed` is the exception, an
/// instance a failed reload left up and not serving (see
/// [`SheepSlot::ready_failed`]). Both doors into a reload ask this,
/// [`Actor::handle_reload`]'s selector pass and [`Actor::advance_reload`], so
/// a reload cannot drop an instance silently or reach one it had ruled out.
/// `advance_reload` adds the `manual` half on its own.
fn reload_eligible(slot: &SheepSlot) -> bool {
    slot.entry.status == ProcStatus::Online || slot.ready_failed
}

/// The status a drainee goes back to when its swap is undone.
///
/// `Online` for an instance that was serving. `Starting` for one an earlier
/// reload had already parked, the case [`reload_eligible`] opened up: it never
/// proved it could serve. `ready_failed` stays set, so the next attempt can
/// still replace it.
///
/// Shared by the two sites that undo a swap, as [`reload_eligible`] is shared
/// by the two that start one.
fn restored_status(slot: &SheepSlot) -> ProcStatus {
    if slot.ready_failed {
        ProcStatus::Starting
    } else {
        ProcStatus::Online
    }
}

/// The prober a gated readiness task, or a sheep's liveness loop, probes with:
/// a fresh [`OsProber`] scoped to the assembled spec's `cwd`/`env`.
///
/// Taking the [`SpawnSpec`] rather than the [`ResolvedApp`] it was assembled
/// from: `probe_exec` runs `env_clear().envs(&self.env)`, and `config.env` is
/// one of three things [`assemble`] folds into the child's environment, so an
/// app that sets no `env` would probe with no `PATH`. A `&ResolvedApp` also
/// cannot reach `instance`, so every instance would probe the same port.
fn spec_prober(spec: &SpawnSpec) -> Arc<dyn Prober> {
    Arc::new(OsProber::new(spec.cwd.clone(), spec.env.clone()))
}

/// Spawns a readiness task for `id` at `epoch`, returning the oneshot sender
/// the actor stores (`SheepSlot::ready_tx`) so a later `Msg::Ready` can wake
/// it. `source` decides which signal [`await_ready`] waits for; `deadline` is
/// the app's `listen_timeout`. The task reports back through `actor_tx` as a
/// `Msg::ReadyResult`, which `Actor::handle_ready_result` drops against a
/// stale `epoch`.
///
/// `manually` is carried, never inspected here.
///
/// Must be called from within a Tokio runtime context.
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

/// Spawns the task that delivers one action to `id`'s child and waits for the
/// reply, returning the oneshot sender the actor stores
/// (`PendingAction::waiter`) so a later [`Msg::ActionReply`] can wake it. The
/// task reports back through `actor_tx` as a [`Msg::ActionResult`].
///
/// The send is in here rather than at the call site because both it and the
/// wait are awaits, which the actor loop may not do. `deadline` covers the two
/// together, since a child that has stopped reading can stall either.
///
/// Only [`ActionOutcome::Replied`] proves the action landed. A failed send or
/// a dropped waiter is [`ActionOutcome::NoChannel`]; a send that succeeds
/// proves nothing, the first one after a child exits being discarded.
fn spawn_action_task(
    id: u32,
    stamp: u64,
    message: ShepherdMessage,
    to_child: mpsc::Sender<ShepherdMessage>,
    deadline: Duration,
    actor_tx: mpsc::Sender<Msg>,
) -> oneshot::Sender<String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tokio::spawn(async move {
        let delivered = tokio::time::timeout(deadline, async move {
            // The send is inside the deadline rather than ahead of it: a
            // child that has stopped reading fd 3 backs its socket up, and an
            // unbounded send would park this task, and its caller, for as long
            // as that child stays wedged.
            if to_child.send(message).await.is_err() {
                return None;
            }
            reply_rx.await.ok()
        })
        .await;
        let outcome = match delivered {
            Ok(Some(body)) => ActionOutcome::Replied { body },
            Ok(None) => ActionOutcome::NoChannel,
            Err(_elapsed) => ActionOutcome::TimedOut,
        };
        let _ = actor_tx
            .send(Msg::ActionResult { id, stamp, outcome })
            .await;
    });
    reply_tx
}

/// Spawns the task that collects one trigger's rows and answers its caller,
/// folding in the sheep already refused in [`Actor::begin_action`]. Must be
/// called from within a Tokio runtime context.
///
/// Awaiting them in a loop is not serial: every wait in `waits` is already
/// running its own task under its own deadline, so the loop is a join. Ten
/// unresponsive apps cost the longest `action_timeout`, not the sum.
///
/// A wait whose sender is dropped rather than answered reports
/// [`ActionOutcome::NoChannel`]: the sender lives on the sheep's slot, so
/// losing it means that slot let go of the wait.
fn spawn_trigger_task(
    mut rows: Vec<ActionReply>,
    waits: Vec<(u32, String, oneshot::Receiver<ActionOutcome>)>,
    reply: oneshot::Sender<Result<Vec<ActionReply>, SupervisorError>>,
) {
    tokio::spawn(async move {
        for (id, name, answer) in waits {
            let outcome = answer.await.unwrap_or(ActionOutcome::NoChannel);
            rows.push(ActionReply { id, name, outcome });
        }
        // Keyed by name with the id breaking ties, so `shep flock` and `shep
        // trigger` do not read two orders. Parity with `sort_flock` is partial
        // and cannot be closed here: its key is `(name, instance, id)` and
        // `ActionReply` carries no slot.
        rows.sort_unstable_by(|a, b| (a.name.as_str(), a.id).cmp(&(b.name.as_str(), b.id)));
        let _ = reply.send(Ok(rows));
    });
}

/// One matched sheep's pending signal delivery: its id, name, and the receiver
/// its outcome will arrive on. An alias for `clippy::type_complexity`.
type SignalWait = (u32, String, oneshot::Receiver<Result<(), RunnerError>>);

/// Spawns the task that collects one signal's rows and answers its caller,
/// folding in the sheep already settled in [`Actor::begin_signal`]. Mirrors
/// [`spawn_trigger_task`], except that a dropped `done` sender, the sheep task
/// ending between the send and the delivery, reports
/// [`SignalOutcome::NotRunning`] rather than `NoChannel`.
fn spawn_signal_task(
    mut rows: Vec<SignalReply>,
    waits: Vec<SignalWait>,
    reply: oneshot::Sender<Result<Vec<SignalReply>, SupervisorError>>,
) {
    tokio::spawn(async move {
        for (id, name, answer) in waits {
            let outcome = match answer.await {
                Ok(Ok(())) => SignalOutcome::Delivered,
                Ok(Err(err)) => SignalOutcome::Failed {
                    reason: err.to_string(),
                },
                Err(_dropped) => SignalOutcome::NotRunning,
            };
            rows.push(SignalReply { id, name, outcome });
        }
        // Name then id, per `spawn_trigger_task`.
        rows.sort_unstable_by(|a, b| (a.name.as_str(), a.id).cmp(&(b.name.as_str(), b.id)));
        let _ = reply.send(Ok(rows));
    });
}

/// One matched sheep's arming: its id and name, and the receiver its write's
/// acknowledgement will arrive on.
type LineWait = (u32, String, oneshot::Receiver<Result<(), RunnerError>>);

/// Awaits every write's acknowledgement, each under its own
/// [`STDIN_WRITE_TIMEOUT`], and answers `reply` with `settled` and the results
/// by name and then by id.
///
/// `join_all`, not a `for` loop: every wait here shares one bound, so awaiting
/// them in turn would make the total `STDIN_WRITE_TIMEOUT * matched` and put a
/// flock-wide `sendline` over an RPC caller's default budget.
fn spawn_send_line_task(
    settled: Vec<LineReply>,
    waits: Vec<LineWait>,
    reply: oneshot::Sender<Result<Vec<LineReply>, SupervisorError>>,
) {
    tokio::spawn(async move {
        let mut rows = settled;
        rows.extend(
            futures_util::future::join_all(waits.into_iter().map(
                |(id, name, answer)| async move {
                    let outcome = match tokio::time::timeout(STDIN_WRITE_TIMEOUT, answer).await {
                        Ok(Ok(Ok(()))) => LineOutcome::Sent,
                        Ok(Ok(Err(err))) => LineOutcome::NotWritten {
                            reason: err.to_string(),
                        },
                        // The sender was dropped: the writer task ended before it
                        // served this request, which means the process did too.
                        Ok(Err(_recv)) => LineOutcome::NoStdin,
                        // The shepherd stopped waiting; it did not stop the
                        // write. The bytes may still land in full when the app
                        // drains, so the reason says so rather than letting an
                        // operator retry into a double delivery.
                        Err(_elapsed) => LineOutcome::NotWritten {
                            reason: format!(
                                "the app did not read its stdin within {}s; this line \
                                 may still land if it drains",
                                STDIN_WRITE_TIMEOUT.as_secs()
                            ),
                        },
                    };
                    LineReply { id, name, outcome }
                },
            ))
            .await,
        );
        // Name then id, per `spawn_trigger_task`.
        rows.sort_unstable_by(|a, b| (a.name.as_str(), a.id).cmp(&(b.name.as_str(), b.id)));
        let _ = reply.send(Ok(rows));
    });
}

/// What a [`Command::HandoverSnapshot`] answers with: one candidate per
/// registered sheep for the fitness gate, and the blob a successor reads.
///
/// Three types because they go three places, and only the blob crosses the
/// exec: the candidates decide whether there is to be an exec at all, and the
/// parked pumps matter only if there is not.
#[cfg(unix)]
type Snapshot = (Vec<OwnedCandidate>, Handover, ParkedPumps);

/// The log pumps a snapshot stopped, so a handover that is then abandoned
/// can start them reading again.
///
/// Taking the snapshot parks every pump that answers it, because a report is
/// only true while nothing moves behind it. A pump that missed
/// [`REPORT_DEADLINE`] is not in here: it never parked. The exec normally ends
/// the park by replacing the image; every other way out leaves this daemon
/// running with pumps that have stopped reading.
///
/// The party that parked them hands them back rather than the abort path
/// re-reading the actor's slots: what is owed a resume is what was reported.
#[cfg(unix)]
#[derive(Debug, Default)]
pub(crate) struct ParkedPumps(Vec<mpsc::Sender<LogCtl>>);

#[cfg(unix)]
impl ParkedPumps {
    /// Lets every pump this snapshot parked read its sheep's streams again.
    ///
    /// A send that fails is a pump that has ended since the report, which is
    /// nothing to repair.
    pub(crate) async fn resume(&self) {
        for pump in &self.0 {
            let _ = pump.send(LogCtl::Resume).await;
        }
    }
}

/// One sheep on its way from the actor to the handover task.
///
/// Everything here is read off a [`SheepSlot`] inside the actor loop, and
/// carried out owned because the assembly runs on a task of its own.
#[cfg(unix)]
#[derive(Debug)]
struct HandoverDraft {
    /// The sheep's lifecycle entry, cloned off the slot.
    entry: ProcessEntry,
    /// The manual command owning this sheep's next exit, if one does.
    manual: Option<PendingManual>,
    /// Whether a `Delete` targets this sheep.
    pending_delete: bool,
    /// The slot's respawn epoch, so a timer armed before the exec is still
    /// recognised as stale after it.
    epoch: u64,
    /// Whether a reload's readiness verification has already failed against
    /// this sheep. See [`SheepSlot::ready_failed`].
    ready_failed: bool,
    /// When this sheep's owed respawn falls due, so the successor re-arms
    /// for what is left of the delay rather than for the whole of it. See
    /// [`SheepSlot::restart_due`].
    restart_due: Option<SystemTime>,
    /// This sheep's log pump, or `None` for a slot whose spawn never
    /// succeeded.
    log_ctl: Option<mpsc::Sender<LogCtl>>,
    /// Whether this sheep's shepherd channel is still one the daemon can
    /// write to.
    ///
    /// Read here rather than taken from the pump's report: the pump reports
    /// the number it was told at the spawn and cannot learn that the channel
    /// died, after which the number names whatever the kernel has since handed
    /// to the next `open`. [`SheepSlot::open_channel`] is the fact that decides
    /// delivery, and is only reachable from the actor loop.
    channel_open: bool,
}

/// Spawns the task that assembles one handover snapshot and answers its
/// caller; must be called from within a Tokio runtime context.
///
/// Every await lives in here, off the actor loop; see
/// [`Actor::handle_reopen`] for the cycle that rules out doing it inline.
///
/// The sheep are visited concurrently because the deadline on the other end is
/// fixed at `shep-cli`'s `admin::KILL_TEARDOWN_WAIT`. Serially, N wedged pumps
/// cost N times [`REPORT_DEADLINE`], and past six that outlasts the client,
/// which falls back to a predecessor still serving and exits 0 mid-sweep.
/// `join_all` returns results in input order, so `drafts`' id-sorted order
/// survives into `candidates` and `carried` with no re-sort.
#[cfg(unix)]
fn spawn_handover_task(
    drafts: Vec<HandoverDraft>,
    fds: DaemonFds,
    counters: Counters,
    reloads: Vec<CarriedReload>,
    reply: oneshot::Sender<Result<Snapshot, SupervisorError>>,
) {
    tokio::spawn(async move {
        let visited = futures_util::future::join_all(drafts.into_iter().map(|draft| async move {
            // Only a pump that answered parked, so only that one is owed a
            // resume. Only a wedged pump refuses the flock: it is the one case
            // where a live sheep's descriptors are unknown, and the gate below
            // is what stands between that and a successor with no stdout.
            let (mut fds, pump_unresponsive, parked_pump) = match &draft.log_ctl {
                Some(log_ctl) => match report_fds(log_ctl).await {
                    PumpReport::Parked(fds) => (fds, false, Some(log_ctl.clone())),
                    PumpReport::Gone => (CarriedFds::none(), false, None),
                    PumpReport::Unresponsive => (CarriedFds::none(), true, None),
                },
                None => (CarriedFds::none(), false, None),
            };
            // The one number the pump can report and be wrong about; see
            // `HandoverDraft::channel_open`. `UnixStream::from(OwnedFd)` checks
            // nothing, so a reissued number would take a shepherd message into
            // whatever that descriptor is now.
            if !draft.channel_open {
                fds.channel = None;
            }
            let carried = CarriedSheep::from_entry(
                &draft.entry,
                draft.epoch,
                fds,
                draft.pending_delete,
                draft.manual,
                draft.ready_failed,
                draft.restart_due,
            );
            let candidate = OwnedCandidate {
                entry: draft.entry,
                pump_unresponsive,
            };
            (candidate, carried, parked_pump)
        }))
        .await;

        // `join_all` hands `visited` back in `drafts`' id-sorted order, not
        // completion order, so this loop needs no sort of its own.
        let mut candidates = Vec::with_capacity(visited.len());
        let mut carried = Vec::with_capacity(visited.len());
        let mut parked = ParkedPumps::default();
        for (candidate, sheep, parked_pump) in visited {
            candidates.push(candidate);
            carried.push(sheep);
            if let Some(log_ctl) = parked_pump {
                parked.0.push(log_ctl);
            }
        }
        let blob = Handover::new(carried, fds, counters, reloads);
        let _ = reply.send(Ok((candidates, blob, parked)));
    });
}

/// What one log pump answered a snapshot's [`LogCtl::ReportFds`] with.
///
/// A pump that cannot be reached and a pump that is wedged both have no
/// descriptors to give and mean opposite things: the first is a sheep that has
/// stopped, the second a live sheep whose four descriptors this daemon does
/// not know. Folding the second into [`CarriedFds::none`] would carry it,
/// silently dropping its stdout, stderr and both log handles.
///
/// [`CarriedFds::none`]: CarriedFds::none
#[cfg(unix)]
#[derive(Debug)]
enum PumpReport {
    /// The pump answered, and has stopped reading its streams until the
    /// exec. It is owed a [`LogCtl::Resume`] if the handover is abandoned.
    Parked(CarriedFds),
    /// There is no pump on the other end any more: the send found a closed
    /// mailbox, or the answer channel dropped unanswered. Either way it is
    /// reading nothing and is owed nothing.
    ///
    /// Not a refusal: a registered sheep that is not running reaches the gate
    /// exactly like this, and there is nothing to carry.
    Gone,
    /// The pump did not answer inside [`REPORT_DEADLINE`].
    ///
    /// It never parked, so it is still reading its sheep's streams and must
    /// not be resumed.
    Unresponsive,
}

/// How long a snapshot waits for one log pump to report its descriptors.
///
/// The work it bounds is a handful of `write(2)`s of at most 8 KiB:
/// microseconds on a healthy filesystem, single-digit milliseconds on a busy
/// one, so two seconds is three orders of magnitude clear of it. Firing early
/// costs the whole flock its handover; firing late costs a few seconds.
///
/// Per pump but paid once: [`spawn_handover_task`] visits every pump
/// concurrently, so a flock of wedged pumps costs one deadline, not N. `shep
/// daemon reload` gives the successor `admin::KILL_TEARDOWN_WAIT` (10s), which
/// a sweep scaling with N would outlast at six wedged pumps.
#[cfg(unix)]
const REPORT_DEADLINE: Duration = Duration::from_secs(2);

/// Asks one sheep's log pump to write out everything it is holding, report
/// the four descriptors it owns, and stop reading until the exec; waits up to
/// [`REPORT_DEADLINE`] for the answer.
///
/// Not reaching a pump at all is [`PumpReport::Gone`], not an error: a stopped
/// sheep has nothing to carry and nothing to lose, and the fitness gate does
/// not refuse it.
///
/// The deadline covers the send as well as the answer, since a pump that has
/// stopped serving its mailbox fills it and blocks the send instead.
#[cfg(unix)]
async fn report_fds(log_ctl: &mpsc::Sender<LogCtl>) -> PumpReport {
    let (done, ack) = oneshot::channel();
    let answer = async {
        if log_ctl.send(LogCtl::ReportFds { done }).await.is_err() {
            return PumpReport::Gone;
        }
        ack.await.map_or(PumpReport::Gone, PumpReport::Parked)
    };
    tokio::time::timeout(REPORT_DEADLINE, answer)
        .await
        .unwrap_or(PumpReport::Unresponsive)
}

/// Spawns the task that carries out one `Reopen` and answers its caller; must
/// be called from within a Tokio runtime context.
///
/// Every await a reopen needs lives in here, off the actor loop; see
/// [`Actor::handle_reopen`] for the cycle that rules out doing it inline, and
/// for why `pumps` is every writer to a path a sheep in `matched` writes to,
/// a wider set than `matched`, while the reply is not.
///
/// Visited one after another, unlike [`spawn_handover_task`]: the caller
/// carries `rpc`'s own per-request budget.
fn spawn_reopen_task(
    matched: Vec<ProcessInfo>,
    pumps: Vec<(ProcessInfo, mpsc::Sender<LogCtl>)>,
    reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
) {
    tokio::spawn(async move {
        let mut failures = Vec::new();
        for (info, log_ctl) in &pumps {
            if let Err(error) = reopen_logs(log_ctl).await {
                // Named and id'd: the reply that would have said which sheep
                // these are is the one being replaced, and a widened set can
                // fail on a sheep the operator never named.
                failures.push(format!("{} (id {}): {error}", info.name, info.id));
            }
        }
        // Every pump is visited before anything is reported: one sheep whose
        // log directory is gone must not stop the rest being reopened.
        let _ = reply.send(if failures.is_empty() {
            Ok(matched)
        } else {
            Err(SupervisorError::ReopenFailed(failures.join("; ")))
        });
    });
}

/// Asks one sheep's log pump to reopen both of its files and waits for the
/// acknowledgement.
///
/// Not reaching a pump at all is a success: a failed send or a dropped
/// acknowledgement both mean there was no pump left to reopen anything.
///
/// # Errors
///
/// [`ReopenError`] if a pump answered and at least one of its two paths could
/// not be opened again. That sheep is now writing a stream nowhere.
async fn reopen_logs(log_ctl: &mpsc::Sender<LogCtl>) -> Result<(), ReopenError> {
    let (done, ack) = oneshot::channel();
    if log_ctl.send(LogCtl::Reopen { done }).await.is_err() {
        return Ok(());
    }
    ack.await.unwrap_or(Ok(()))
}

/// Spawns the task that carries out one `Flush` and answers its caller; must
/// be called from within a Tokio runtime context.
///
/// Every pump in `pumps` is flushed before any path in `paths` is truncated:
/// `write_all` on a [`tokio::fs::File`] returns once the real `write(2)` is
/// queued, so a dispatched line can land at offset 0 of a file truncated in
/// between. One barrier is also the only ordering that stays correct when
/// several sheep share one path.
///
/// `pumps` is every writer to a path in `paths`, a wider set than `matched`;
/// see [`Actor::handle_flush`]. All of them are visited before anything is
/// reported.
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
/// Not reaching a pump at all is a success, as in [`reopen_logs`]: a pump that
/// is gone owes no bytes. The truncate still runs, which is how a stopped
/// sheep's logs get emptied.
///
/// # Errors
///
/// [`FlushError`] if a pump answered and at least one stream's owed bytes
/// never reached its file. The truncate that follows runs regardless.
async fn flush_logs(log_ctl: &mpsc::Sender<LogCtl>) -> Result<(), FlushError> {
    let (done, ack) = oneshot::channel();
    if log_ctl.send(LogCtl::Flush { done }).await.is_err() {
        return Ok(());
    }
    ack.await.unwrap_or(Ok(()))
}

/// Truncates the log file at `path` to zero length.
///
/// Exactly the path the Flockfile named, with no check on where it points: a
/// flush empties whatever an `out_file` names, under the daemon's own
/// privileges. The open goes through [`open_log_path`], which refuses a
/// symlink at the path. Not `create(true)`, so a missing path is a no-op.
///
/// # Errors
///
/// [`FlushError`] if the path could not be opened: an ancestry a privileged
/// shepherd will not write below, a symlink at the path, an unwritable mode,
/// a read-only filesystem, an IO error.
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

/// One signal delivery a sheep task is asked to perform, plus where the answer
/// goes.
///
/// A mailbox of its own rather than a [`SheepCtl`] variant: see
/// [`SheepSlot::signals`].
#[derive(Debug)]
struct SignalRequest {
    /// What to deliver, to this sheep's own pid.
    sig: OperatorSignal,
    /// Fires with what the delivery came to. A dropped sender means the sheep
    /// task ended between the send and the delivery.
    done: oneshot::Sender<Result<(), RunnerError>>,
}

/// The two mailboxes a live sheep task listens on.
struct SheepHandles {
    /// The kill ladder's, whose one-message-kind invariant is documented on
    /// [`SheepSlot::signals`].
    ctl: mpsc::Sender<SheepCtl>,
    /// Signal deliveries.
    signals: mpsc::Sender<SignalRequest>,
}

/// Spawns the per-sheep task and returns its two mailbox senders.
fn spawn_sheep_task<P: RunningProcess>(
    id: u32,
    proc: P,
    io: ProcIo,
    app: ResolvedApp,
    events: Bus,
    actor_tx: mpsc::Sender<Msg>,
) -> SheepHandles {
    let (ctl_tx, ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
    let (signal_tx, signal_rx) = mpsc::channel(SIGNAL_CAPACITY);
    tokio::spawn(run_sheep(
        id, proc, io, app, ctl_rx, signal_rx, events, actor_tx,
    ));
    SheepHandles {
        ctl: ctl_tx,
        signals: signal_tx,
    }
}

/// The per-sheep task body: owns `(proc, io)` for the process's whole lifetime
/// and drains every `ProcIo` channel. Exactly one of the first two `select!`
/// branches ever fires per proc, which is the one-exit-path invariant, after
/// which the task reports `Msg::Exited` and returns.
///
/// A natural exit racing an in-flight `Kill` cannot produce two `Msg::Exited`s
/// or hang a caller: `tokio::select!` picks one ready branch per iteration,
/// that branch alone breaks, and the loop never revisits the other.
///
/// The `if <channel>_open` guards take a closed channel out of consideration;
/// without them its `recv()` resolves to `None` on every poll, busy-spinning
/// the `select!`. Eight parameters: each is threaded through that `select!`.
#[allow(clippy::too_many_arguments)]
async fn run_sheep<P: RunningProcess>(
    id: u32,
    mut proc: P,
    io: ProcIo,
    app: ResolvedApp,
    mut ctl_rx: mpsc::Receiver<SheepCtl>,
    mut signal_rx: mpsc::Receiver<SignalRequest>,
    events: Bus,
    actor_tx: mpsc::Sender<Msg>,
) {
    // Destructured in the task's own body, never in a shorter scope: dropping
    // the log pump's `logs` receiver or its last `log_ctl` sender ends the
    // pump, which drops the read ends of the child's stdout and stderr and
    // gets the child `EPIPE` on its next write.
    let ProcIo {
        mut logs,
        mut from_child,
        to_child,
        log_ctl: _log_ctl,
        to_stdin: _to_stdin,
        //        ^ bound, not `_`: `to_stdin: _` drops the sender inside the
        // `let`, closing the child's stdin at spawn and giving an opted-in app
        // immediate EOF. `SheepSlot` holds a clone, so nothing in the fast
        // loop would show it.
    } = io;
    let mut ctl_open = true;
    let mut logs_open = true;
    let mut from_child_open = true;
    let mut signals_open = true;

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
            maybe_signal = signal_rx.recv(), if signals_open => {
                match maybe_signal {
                    Some(SignalRequest { sig, done }) => {
                        // Delivered from the task that owns the proc, never
                        // from the actor off a recorded pid: only the owning
                        // task knows the child has not been reaped, which
                        // closes the pid-reuse ABA race.
                        let _ = done.send(proc.signal_process(sig));
                    }
                    None => signals_open = false,
                }
            }
            maybe_line = logs.recv(), if logs_open => {
                match maybe_line {
                    Some(line) => {
                        // Through the bus's own gate rather than `send`: with
                        // nobody subscribed this costs one relaxed load, where
                        // a publish costs an allocation, a mutex and a wakeup.
                        events.publish_log(if line.err {
                            BusEvent::LogErr { id, line: line.line }
                        } else {
                            BusEvent::LogOut { id, line: line.line }
                        });
                    }
                    None => logs_open = false,
                }
            }
            maybe_msg = from_child.recv(), if from_child_open => {
                match maybe_msg {
                    Some(message) => {
                        // Forwarded before it is acted on, and
                        // unconditionally: a subscriber's view of fd 3 must
                        // not depend on this daemon having a consumer for that
                        // kind.
                        let _ = events.send(SharedEvent::new(BusEvent::Channel {
                            id,
                            message: message.clone(),
                        }));
                        match message {
                            ChildMessage::Ready => {
                                let _ = actor_tx.send(Msg::Ready { id }).await;
                            }
                            ChildMessage::Metric { name, value } => {
                                tracing::debug!(
                                    id,
                                    name,
                                    value,
                                    "child metric forwarded to the bus as channel.metric"
                                );
                            }
                            ChildMessage::ActionReply {
                                action,
                                body,
                                // The child's `id` is the dispatch's, not the
                                // sheep's. Renamed at the boundary so no line
                                // downstream holds both meanings.
                                id: stamp,
                            } => {
                                let _ = actor_tx
                                    .send(Msg::ActionReply { id, action, body, stamp })
                                    .await;
                            }
                        }
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
    use shep_core::protocol::DogSource;
    use shep_core::status::ProcStatus;
    use shep_core::values::{MemSize, UpDuration};

    use super::*;
    use crate::cron::{DEFAULT_MAX_CRON_SLEEP, SystemClock};
    use crate::extras::{ExtrasReports, spawn_extras_reporter};
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::limits::LimitEnforcer;
    use crate::testing::{
        Harness, RecordingEnforcer, ScriptedProber, SharedRunner, app_with, armed_entry, harness,
        idle_stats, probe_config, test_paths,
    };
    // unix only: its one caller drives an unresolvable `user`.
    #[cfg(unix)]
    use crate::testing::capture_logs;
    #[cfg(unix)]
    use crate::tokio_runner::TokioRunner;
    use tokio::sync::watch;
    // aliased: `Ordering` in this module means `cmp::Ordering`
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Every process event queued right now, in order, for a case whose
    /// handler is synchronous.
    fn drained_process_kinds(
        rx: &mut tokio::sync::broadcast::Receiver<SharedEvent>,
    ) -> Vec<ProcessEventKind> {
        let mut kinds = Vec::new();
        while let Ok(BusEvent::Process { event, .. }) = rx.try_recv().map(|event| event.to_event())
        {
            kinds.push(event);
        }
        kinds
    }

    async fn await_event(
        rx: &mut tokio::sync::broadcast::Receiver<SharedEvent>,
        id: u32,
        kind: ProcessEventKind,
    ) -> bool {
        loop {
            match rx.recv().await.map(|event| event.to_event()) {
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
    ///
    /// Bounded `timeout` + `recv` rather than `try_recv`: a message already due
    /// may not have reached this receiver's queue yet.
    async fn assert_no_event_within(
        rx: &mut tokio::sync::broadcast::Receiver<SharedEvent>,
        id: u32,
        kind: ProcessEventKind,
        window: Duration,
    ) {
        match tokio::time::timeout(window, await_event(rx, id, kind)).await {
            Err(_elapsed) => {} // window elapsed with nothing arriving: expected
            Ok(_manually) => panic!("unexpected {kind:?} for id {id} within {window:?}"),
        }
    }

    // --- The readiness gate ---

    #[tokio::test(start_paused = true)]
    async fn wait_ready_app_stays_starting_until_the_channel_signals() {
        let (events, mut rx) = crate::bus::test_bus(64);
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

        // Reaches the actor where the sheep task's forwarded `Ready` would.
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();

        tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("Online once the channel signals");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    // Real time, no `start_paused`: a paused test waiting on real socket I/O
    // can deadlock, since the virtual clock does not move the OS.
    #[tokio::test]
    async fn readiness_probe_app_stays_starting_until_the_probe_passes() {
        // Reserve a free port and release it: probes fail with connection
        // refused until the listener below binds it for real.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);

        let (events, mut rx) = crate::bus::test_bus(64);
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
        // Nothing is listening on `addr` yet: the probe fails every interval.
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

    // The probe reads `$SHEP_INSTANCE`, which only `assemble` writes: a prober
    // built from `config.env` expands it to nothing under `probe_exec`'s
    // `env_clear()`. Real time, since this spawns a real `sh` per probe.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exec_readiness_probe_sees_the_assembled_env_not_the_apps_own() {
        let (events, mut rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        // Instance 0 is the only slot a single-instance app gets.
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
        // Far longer than this test's own patience, so an Online can only have
        // come from a passing probe, never from the deadline path.
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

    #[tokio::test(start_paused = true)]
    async fn a_gated_apps_online_carries_the_same_manually_flag_an_ungated_one_does() {
        let (events, mut rx) = crate::bus::test_bus(64);
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

        // The ungated app is the control for what a plain `Start` reports.
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

        // The sheep is `Starting` with a live task, so `restart` takes the
        // deferred route and resolves at the respawn, still `Starting`.
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

    #[tokio::test(start_paused = true)]
    async fn an_operators_restart_is_reported_as_a_user_action() {
        let (events, mut rx) = crate::bus::test_bus(64);
        // Two: the sheep, and the respawn the restart performs.
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

    #[tokio::test(start_paused = true)]
    async fn a_gated_app_whose_deadline_elapses_goes_online_anyway() {
        let (events, mut rx) = crate::bus::test_bus(64);
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

    // The epoch guard: both processes are `Starting`, so status alone cannot
    // tell them apart.
    #[tokio::test(start_paused = true)]
    async fn a_gated_app_that_exits_while_starting_never_reaches_online_from_the_old_wait() {
        let (events, mut rx) = crate::bus::test_bus(64);
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

        // The 500ms exit is unstable (< the 1000ms `min_uptime` default) and
        // respawns after the 100ms default backoff: `Starting` again, epoch up.
        tokio::time::timeout(
            Duration::from_secs(1),
            await_event(&mut rx, 0, ProcessEventKind::Restart),
        )
        .await
        .expect("the automatic respawn after the unstable exit");
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);
        assert_eq!(handle.list().await[0].restarts, 1);

        // The old wait's deadline (~3000ms from the first spawn) elapses next,
        // with both processes reading `Starting`.
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

        // The respawned process's own deadline elapses next.
        tokio::time::timeout(
            Duration::from_secs(2),
            await_event(&mut rx, 0, ProcessEventKind::Online),
        )
        .await
        .expect("the new process's own readiness deadline");
        assert_eq!(handle.list().await[0].status, ProcStatus::Online);
    }

    // No respawn, so the epoch never changes: only the status guard stands
    // between the stale `TimedOut` and an incorrect `Online`.
    #[tokio::test(start_paused = true)]
    async fn a_gated_app_stopped_while_starting_ignores_the_old_wait() {
        let (events, mut rx) = crate::bus::test_bus(64);
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

        // The old wait resolves `TimedOut` at the epoch this slot still carries.
        assert_no_event_within(&mut rx, 0, ProcessEventKind::Online, Duration::from_secs(3)).await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gated_app_restarted_while_starting_ignores_the_old_wait() {
        let (events, mut rx) = crate::bus::test_bus(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals either instance's readiness
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        assert_eq!(handle.list().await[0].status, ProcStatus::Starting);

        // A gap before the restart, so the old wait's deadline and the new one
        // land far enough apart to tell apart below.
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

        // The old wait's deadline elapses first; the epoch guard must drop it.
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

        // The respawned process's own deadline elapses next.
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
        let (events, _rx) = crate::bus::test_bus(64);
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
        let (events, _rx) = crate::bus::test_bus(64);
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
        // `err_file` is left unset: the two resolve independently.
        let (events, _rx) = crate::bus::test_bus(64);
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
        let (events, mut rx) = crate::bus::test_bus(1024);
        // 16 spawns: initial + 15 restarts; every exit instant (unstable).
        let runner = ScriptedRunner::new((0..16).map(|_| ProcScript::const_exit(1)).collect());
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("crash", "./boom");
        app.exp_backoff_restart_delay = Some("100".parse().unwrap());
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // The budget check (16 unstable exits errors) fires on the 16th exit,
        // the script's last entry.
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        assert_eq!(list[0].restarts, 15); // respawns performed, not exits
        // `last_exit` is what tells a boot loop from a spawn failure.
        assert_eq!(
            list[0].last_exit,
            Some(ExitInfo {
                code: Some(1),
                signal: None,
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn crash_loop_budget_check_fires_before_script_exhaustion_at_real_default() {
        // Twenty scripts, more than either check needs, so only the budget
        // path can produce restarts==15: an `unstable_count > max_restarts`
        // check would consume a 17th spawn and report 16.
        let (events, mut rx) = crate::bus::test_bus(1024);
        let runner = ScriptedRunner::new((0..20).map(|_| ProcScript::const_exit(1)).collect());
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("crash-default", "./boom"); // no max_restarts override
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        // Budget exhaustion, not script exhaustion: 4 scripted spawns unused.
        assert_eq!(list[0].restarts, 15);
    }

    #[tokio::test(start_paused = true)]
    async fn stable_run_resets_budget() {
        let (events, mut rx) = crate::bus::test_bus(256);
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
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        // 3 + 1 + 15 respawns after the initial spawn = 19
        assert_eq!(list[0].restarts, 19);
    }

    #[tokio::test(start_paused = true)]
    async fn manual_stop_prevents_restart() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript {
            delay_ms: u64::MAX,
            outcome: ExitOutcome {
                code: None,
                signal: None,
            },
            obeys_signal: true,
            obeys_kill: true,
            lamb_holds_the_pipe: false,
            reads_stdin: true,
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
        // No restart is scheduled: a full minute yields no further events.
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_exit_codes_mean_clean_stop() {
        let (events, mut rx) = crate::bus::test_bus(64);
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
        let (events, _rx) = crate::bus::test_bus(64);
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
        let (events, _rx) = crate::bus::test_bus(64);
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
        // `CannotStart`, not `SpawnFailed`: passwd resolution runs before
        // anything is registered, so nothing was spawned.
        assert!(matches!(err, SupervisorError::CannotStart(_)), "{err:?}");
        assert!(
            handle.list().await.is_empty(),
            "a refusal before the registering pass must leave nothing registered"
        );
    }

    /// An app failing two checks must push one refusal, or a two-app batch
    /// reports "4 of 2 apps cannot start".
    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_counted_once_per_app_not_once_per_failed_check() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let handle = spawn_supervisor(
            // `refusing`, or the fake answers `Preflight::Unknown` and only
            // the credentials check can refuse.
            ScriptedRunner::new(vec![ProcScript::never_exits()]).refusing(&["one", "two"]),
            test_paths(&dir),
            events,
        );
        // Two apps, each failing both checks: user, and script preflight.
        let both_bad = |name: &str| {
            let mut app = AppConfig::minimal(name, "./definitely-not-here");
            app.cwd = Some(dir.path().display().to_string());
            app.user = Some("definitely-not-a-real-shep-user".to_string());
            normalize(app).unwrap()
        };
        let err = handle
            .start(vec![both_bad("one"), both_bad("two")])
            .await
            .unwrap_err();
        let SupervisorError::CannotStart(msg) = &err else {
            panic!("expected CannotStart, got {err:?}");
        };
        assert!(
            msg.contains("2 of 2 apps cannot start"),
            "one refusal per app, never one per failed check: {msg}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delete_and_selectors_route() {
        let (events, _rx) = crate::bus::test_bus(64);
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

    /// Waits until the flock's single process reads `restarts` restarts and
    /// `Online`.
    ///
    /// Bounded: its callers set `exp_backoff_restart_delay = None`, and a delay
    /// reintroduced for that shape would spin here forever.
    async fn wait_for_restarts_online(handle: &SupervisorHandle, restarts: u32) -> ProcessInfo {
        let mut info = handle.list().await.remove(0);
        for _ in 0..200 {
            if info.restarts == restarts && info.status == ProcStatus::Online {
                break;
            }
            tokio::task::yield_now().await;
            info = handle.list().await.remove(0);
        }
        info
    }

    #[tokio::test(start_paused = true)]
    async fn manual_restart_resets_budget_and_respawns() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Five procs: two unstable crashes, the long-lived proc they land on,
        // the unstable respawn the manual restart performs, and the proc a
        // still-solvent budget restarts onto.
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
        // Three rather than sixteen: two crashes leave the budget one short.
        app.max_restarts = 3;
        // The sync loop below busy-polls under a paused clock, so a non-zero
        // `exp_backoff_restart_delay` would spin it forever.
        app.exp_backoff_restart_delay = None;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state: immediate restarts mean restarts==2 once `never_exits`
        // is up.
        let info = wait_for_restarts_online(&handle, 2).await;
        assert_eq!(
            (info.status, info.restarts),
            (ProcStatus::Online, 2),
            "never reached the never_exits proc -- got {info:?}"
        );
        let restarted = handle
            .restart(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();
        // The deferred reply is snapshotted at the respawn.
        assert_eq!(restarted[0].status, ProcStatus::Online);

        // The proc restart landed on is itself unstable: with the budget reset
        // its exit is the first of three, without it the third. Bounded, since
        // the failing outcome is a settled `Errored`.
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

    // The budget reset belongs to `ManualKind::Restart` and to nothing else: a
    // restart the daemon raised itself resets it as an operator's does.
    // `CommandOrigin` governs only which of two racing commands owns a sheep's
    // next exit (`claim_manual`).
    #[tokio::test(start_paused = true)]
    async fn an_automatic_restart_resets_the_budget_like_an_operators_does() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Five procs: two unstable crashes, the long-lived proc they land on,
        // the unstable respawn the automatic restart performs, and the proc a
        // still-solvent budget restarts onto.
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
        // Three rather than sixteen: two crashes leave the budget one short.
        app.max_restarts = 3;
        // The sync loop below busy-polls under a paused clock, so a non-zero
        // `exp_backoff_restart_delay` would spin it forever.
        app.exp_backoff_restart_delay = None;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state: immediate restarts mean restarts==2 once `never_exits`
        // is up.
        let info = wait_for_restarts_online(&handle, 2).await;
        assert_eq!(
            (info.status, info.restarts),
            (ProcStatus::Online, 2),
            "never reached the never_exits proc -- got {info:?}"
        );

        handle
            .restart_automatic(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();

        // The proc restart landed on is itself unstable: with the budget reset
        // its exit is the first of three, without it the third. Bounded, since
        // the failing outcome is a settled `Errored`.
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

    // A `restart` aimed at a sheep with no live task has no exit to ride, so it
    // never reaches `handle_exited`: `apply_immediate` resets and respawns
    // inline, and the reply is that respawn. `Stopped` is the settled
    // not-running state; `WaitingRestart` still holds a `RestartDue` timer.
    #[tokio::test(start_paused = true)]
    async fn restarting_a_stopped_sheep_resets_the_budget() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Five procs: two unstable crashes, the long-lived proc they land on
        // and the stop below ends, the unstable respawn the restart performs,
        // and the proc a still-solvent budget restarts onto.
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
        // Three rather than sixteen: two crashes leave the budget one short.
        app.max_restarts = 3;
        // The sync loop below busy-polls under a paused clock, so a non-zero
        // `exp_backoff_restart_delay` would spin it forever.
        app.exp_backoff_restart_delay = None;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state: immediate restarts mean restarts==2 once `never_exits`
        // is up.
        let info = wait_for_restarts_online(&handle, 2).await;
        assert_eq!(
            (info.status, info.restarts),
            (ProcStatus::Online, 2),
            "never reached the never_exits proc -- got {info:?}"
        );

        // `stop` takes the sheep off its live task without touching the budget:
        // `decide_on_exit` short-circuits to `CleanStop` on `manual_stop`,
        // before it classifies the exit. The two spent exits stay on the books.
        let stopped = handle
            .stop(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();
        assert_eq!(stopped[0].status, ProcStatus::Stopped);

        let restarted = handle
            .restart(ProcessSelector::Name("svc".to_string()))
            .await
            .unwrap();
        // `apply_immediate`'s reply is the respawn, sent in the same actor turn.
        assert_eq!(
            (restarted[0].status, restarted[0].restarts),
            (ProcStatus::Online, 3)
        );

        // The proc restart landed on is itself unstable: with the budget reset
        // its exit is the first of three, without it the third. Bounded, since
        // the failing outcome is a settled `Errored`.
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
        let (events, _rx) = crate::bus::test_bus(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        handle.shutdown().await; // kill ladder on every online sheep, then stop
        assert!(handle.list_checked().await.is_err());
    }

    // --- Concurrency regression guards ---

    async fn drain_kinds(
        rx: &mut tokio::sync::broadcast::Receiver<SharedEvent>,
    ) -> Vec<(u32, ProcessEventKind)> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv().map(|event| event.to_event()) {
            if let BusEvent::Process { event, info, .. } = ev {
                out.push((info.id, event));
            }
        }
        out
    }

    // A pending `RestartDue` timer must not respawn once shutdown has begun:
    // that child is outside the shutdown's `online` snapshot, so never killed.
    #[tokio::test(start_paused = true)]
    async fn shutdown_ignores_a_pending_restart_timer() {
        let (events, mut rx) = crate::bus::test_bus(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::const_exit(1),     // crash: instant exit -> waiting-restart
            ProcScript::ignores_signals(), // web: full 1600ms kill ladder
            ProcScript::never_exits(),     // catches a pending timer respawning during shutdown
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

    // A readiness wait that resolves after a shutdown has begun must not mark
    // its sheep online. The sibling guards do not reach this: the shutdown
    // leaves the slot at the same epoch and the same `Starting` status the wait
    // was armed under, and its 1000ms deadline lands inside the kill ladder.
    #[tokio::test(start_paused = true)]
    async fn shutdown_ignores_a_pending_readiness_wait() {
        let (events, mut rx) = crate::bus::test_bus(1024);
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

    // A Start racing a concurrent Shutdown must never leave an un-killed
    // child: either Shutdown is processed first and the Start is rejected, or
    // the Start lands first and Shutdown's `online` snapshot catches it.
    #[tokio::test(start_paused = true)]
    async fn late_start_racing_shutdown_never_orphans() {
        let (events, mut rx) = crate::bus::test_bus(1024);
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

    // A manual restart during a backoff wait leaves the original `RestartDue`
    // timer scheduled; it must not fire later and short-circuit the new backoff
    // the manual respawn's own exit schedules.
    #[tokio::test(start_paused = true)]
    async fn stale_restart_timer_never_short_circuits_a_newer_backoff() {
        let (events, mut rx) = crate::bus::test_bus(1024);
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

        // t=1500 the respawn dies, giving a new timer @ 3500, so look at the
        // world at t=2500.
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

    // Stop and Restart racing on the same running sheep: the first to reach it
    // owns the `manual` marker and its one live Kill, and both callers get the
    // same terminal snapshot back. Both have a caller awaiting an answer, so
    // neither may displace the other.
    #[tokio::test(start_paused = true)]
    async fn overlapping_stop_and_restart_agree_on_one_outcome() {
        let (events, _rx) = crate::bus::test_bus(1024);
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

    // A flood of Stop commands against one sheep mid-kill must not delay
    // processing an unrelated sheep's exit.
    #[tokio::test(start_paused = true)]
    async fn actor_never_blocks_behind_a_busy_kill_ladder() {
        let (events, mut rx) = crate::bus::test_bus(1024);
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

    // The deadlock shape: the actor parked in `ctl.send()` with `ctl` full
    // while the sheep task parks in `actor_tx.send()` with the mailbox full.
    #[tokio::test(start_paused = true)]
    async fn mailbox_flood_during_a_kill_never_deadlocks() {
        let (events, mut rx) = crate::bus::test_bus(4096);
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
        // Stuff the 256-slot mailbox while the actor is inside a kill ladder.
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

    // A `Delete` landing on an id `begin_shutdown` already claimed records its
    // intent in `pending_delete` rather than in `manual`: `handle_exited`
    // deregisters on either, so the caller is never told of a deletion that did
    // not happen. The decoy outlives the target, keeping the actor alive.
    #[tokio::test(start_paused = true)]
    async fn delete_racing_shutdown_still_deregisters_the_sheep() {
        let (events, _rx) = crate::bus::test_bus(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(), // target: default 1600ms kill_timeout ladder
            ProcScript::ignores_signals(), // decoy: kept alive far longer
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

        // The decoy's ladder never resolves under the paused clock: drop the
        // in-flight Shutdown rather than waiting on it.
        drop(shutter);
    }

    // `handle_exited`'s manual-Restart branch is the one path that resolves an
    // exit without consulting `decide_on_exit`, so it must still honour
    // `pending_delete`: a racing `Delete` finds the marker already claimed and
    // sets that field without touching `manual`.
    #[tokio::test(start_paused = true)]
    async fn delete_racing_restart_still_deregisters_the_sheep() {
        let (events, _rx) = crate::bus::test_bus(1024);
        let runner = ScriptedRunner::new(vec![
            ProcScript::ignores_signals(), // wide kill-ladder window
            // A second script, so a broken run's respawn succeeds into a live
            // process rather than landing in `Errored`.
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
        // A respawned `Online` here would mean a child spawned behind the
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

    // An automatic restart is mid-kill-ladder when an operator's `stop` lands
    // on the same sheep. The operator's intent wins: the sheep ends `Stopped`,
    // never respawned. `extra_restart` is the only command with no reply, so it
    // can still be in flight when the next command arrives.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_beats_an_automatic_restart_mid_ladder() {
        let (events, _rx) = crate::bus::test_bus(1024);
        // Two: the sheep, plus the respawn a broken implementation performs.
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
        // the restart's marker and starts its ladder before it sees the stop.
        handle.extra_restart(running.id, pid, None, None).await;
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

    // The `Delete` sibling of the case above. `claim_manual`'s carve-out and
    // `pending_delete` each produce this outcome, so it takes disabling both.
    #[tokio::test(start_paused = true)]
    async fn an_operators_delete_beats_an_automatic_restart_mid_ladder() {
        let (events, _rx) = crate::bus::test_bus(1024);
        // Two: the sheep, plus the respawn a broken run performs.
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

        handle.extra_restart(running.id, pid, None, None).await;
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

    /// `never_exits` obeys the ladder's first rung, so the wait resolves on
    /// `SIGTERM` rather than `kill_tree`'s `SIGKILL`: the number pinned is 15.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_still_shows_its_signal_as_the_last_exit() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("svc", "./svc");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        let stopped = handle.stop(ProcessSelector::All).await.unwrap();
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].status, ProcStatus::Stopped);
        assert_eq!(
            stopped[0].last_exit,
            Some(ExitInfo {
                code: None,
                signal: Some(15),
            }),
            "an operator's own stop must still show up as a last exit: {stopped:?}"
        );
    }

    /// Both failures land in `Errored`, so `last_exit` is all that separates
    /// "your app crashed with 1" from "shep could not start your app at all".
    ///
    /// One script that exits `1` gives the sequence: a real exit that records a
    /// code, then a respawn that fails to spawn.
    #[tokio::test(start_paused = true)]
    async fn a_respawn_that_fails_to_spawn_clears_the_previous_exit_code() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::const_exit(1)]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let app = AppConfig::minimal("svc", "./svc");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        // Let the exit, the restart decision and the failed respawn all land.
        tokio::time::sleep(Duration::from_secs(30)).await;

        let listing = handle.list().await;
        assert_eq!(listing.len(), 1);
        assert_eq!(
            listing[0].status,
            ProcStatus::Errored,
            "a respawn that cannot spawn is still terminal: {listing:?}"
        );
        assert_eq!(
            listing[0].last_exit, None,
            "nothing exited on the failed respawn, so the earlier code must \
             not still be showing: {listing:?}"
        );
    }

    // --- `Stopping`: the drainee, against the guards it must never pass ---
    //
    // These cases call the guarded handlers directly, so a failure names the
    // guard rather than a later consequence.

    /// One sheep already `Stopping`, wired the way a reload's drainee is: a live
    /// `ctl` sender and a pid a stale report can be raised against. No scripts,
    /// so a broken guard's spawn attempt fails loudly.
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
            to_child: None,
            signals: None,
            to_stdin: None,
            manual: None,
            pending_delete: false,
            epoch,
            ready_tx: None,
            actions: ActionWaits::default(),
            ready_failed: false,
            restart_due: None,
        };
        let mut sheep = HashMap::new();
        sheep.insert(0, slot);
        let (events, _events_rx) = crate::bus::test_bus(16);
        let (tx, _rx) = mpsc::channel(16);
        let actor = Actor {
            runner: ScriptedRunner::new(vec![]),
            paths,
            events,
            tx,
            sheep,
            next_id: 1,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        };
        (actor, ctl_rx)
    }

    /// One sheep marked as a reload's drainee, `Stopping` on `status` and
    /// `ReloadState::Drainee` on `reload`, holding a live signal mailbox whose
    /// receiver the caller keeps. `begin_action` filters on that marker and
    /// `begin_signal` must not.
    fn actor_with_a_drainee_holding_a_signal_mailbox(
        dir: &tempfile::TempDir,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<SignalRequest>) {
        // No scripts: an unwanted spawn fails loudly.
        let (mut actor, _mailbox) = actor_with_one_online_sheep(dir, vec![]);
        let slot = actor.sheep.get_mut(&0).expect("the fixture registers id 0");
        slot.entry.status = ProcStatus::Stopping;
        slot.entry.reload = ReloadState::Drainee { new_id: Some(1) };
        // Wide enough that a `try_send` returning `Full` means a bug.
        let (signals, signal_rx) = mpsc::channel(16);
        slot.signals = Some(signals);
        (actor, signal_rx)
    }

    // A liveness failure or memory breach reported against a reload's drainee
    // must never claim its manual marker or send a second `Kill`: its kill
    // ladder already owns its next exit.
    #[test]
    fn a_stopping_sheep_rejects_an_extra_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut ctl_rx) = actor_with_stopping_drainee(&dir, 4242, 0);

        actor.handle_extra_restart(0, 4242, None, None);

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

    /// A config-only re-arm changes neither the pid nor the status, so
    /// `handle_extra_restart`'s first two guards pass and only the epoch stops
    /// the replaced probe from restarting the sheep.
    ///
    /// `slot.manual` is the observable: the actor is driven directly, so
    /// `begin_manual` claiming the marker is the proof a restart got through.
    #[tokio::test]
    async fn a_stale_liveness_failure_from_a_replaced_probe_does_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let mut app = AppConfig::minimal("web", "./srv");
        app.liveness_probe = Some(ProbeConfig {
            failure_threshold: 1,
            ..probe_config(ProbeKind::Tcp, "localhost:5432")
        });
        let app = normalize(app).unwrap();
        let pid = 1111;
        let entry = armed_entry(0, 0, pid, app, &paths);
        // A live `ctl`: `begin_manual_ids` reads `ctl.is_some()` to decide a
        // sheep is running, and without one this takes the "already stopped"
        // path instead of the guard under test.
        let (ctl_tx, mut ctl_rx) = mpsc::channel(1);
        let mut sheep = HashMap::new();
        sheep.insert(
            0,
            SheepSlot {
                entry,
                ctl: Some(ctl_tx),
                log_ctl: None,
                to_child: None,
                signals: None,
                to_stdin: None,
                manual: None,
                pending_delete: false,
                epoch: 0,
                ready_tx: None,
                actions: ActionWaits::default(),
                ready_failed: false,
                restart_due: None,
            },
        );
        let (events, _events_rx) = crate::bus::test_bus(16);
        let (tx, _mailbox) = mpsc::channel(16);
        let mut actor = Actor {
            runner: ScriptedRunner::new(vec![]),
            paths,
            events,
            tx,
            sheep,
            next_id: 1,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        };
        let entry = actor
            .sheep
            .get(&0)
            .expect("the fixture registers id 0")
            .entry
            .clone();

        let supervisor = SupervisorHandle {
            tx: actor.tx.clone(),
        };
        let (breach_tx, _breaches) = mpsc::channel(1);
        let (live_tx, _liveness) = mpsc::channel(1);
        let extras = Extras {
            clock: Arc::new(SystemClock),
            enforcer: Arc::new(RecordingEnforcer::default()),
            max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
            reports: ExtrasReports {
                breaches: breach_tx,
                liveness: live_tx,
            },
            stats: idle_stats(),
        };
        // Never fails on its own: the epoch mismatch is driven directly.
        let prober: Arc<dyn Prober> = Arc::new(ScriptedProber::new(vec![]));

        // The epoch this arming gets is the stale one fed back in below.
        actor
            .registry
            .arm(&entry, Arc::clone(&prober), &extras, &supervisor);
        let stale_epoch = actor.registry.liveness_epoch(0);

        // Re-arm the same id: the pid and status stay, the epoch moves.
        actor.registry.arm(&entry, prober, &extras, &supervisor);
        assert_ne!(
            actor.registry.liveness_epoch(0),
            stale_epoch,
            "a re-arm must advance the epoch"
        );

        // A failure from the replaced probe, at the epoch it was armed under.
        actor.handle_extra_restart(0, pid, Some(stale_epoch), None);

        let slot = actor.sheep.get(&0).expect("the sheep stays registered");
        assert!(
            slot.manual.is_none(),
            "a stale liveness failure from a replaced probe must never claim the manual marker"
        );
        assert!(
            ctl_rx.try_recv().is_err(),
            "a stale liveness failure from a replaced probe must never queue a Kill"
        );
        assert_eq!(
            slot.entry.restarts, 0,
            "a dropped report must never bump the restart count"
        );
        assert_eq!(
            slot.entry.pid,
            Some(pid),
            "a dropped report must never touch the running pid"
        );
    }

    // A backoff timer scheduled before a reload started draining this sheep
    // must never respawn it: the slot belongs to the fresh replacement.
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

    // --- Reload: which of the two orderings an app gets ---

    /// An app that probes a TCP address, the shape whose readiness answer the
    /// wrong instance can give.
    fn probed_app(name: &str) -> AppConfig {
        let mut app = AppConfig::minimal(name, "./srv");
        app.readiness_probe = Some(probe_config(ProbeKind::Tcp, "127.0.0.1:9"));
        app
    }

    // The `wait_ready` row has a probe configured and no `reuse_port`, so a
    // rule reading the config's two fields directly would serialise it.
    // `ReadinessSource::of` prefers the channel, and a replacement's channel is
    // its own, so the mode derives from the source.
    #[test]
    fn reload_mode_serialises_exactly_the_apps_whose_probe_can_lie() {
        let cases = [
            (false, false, false, ReloadMode::Overlap),
            (false, true, false, ReloadMode::Overlap),
            (true, false, false, ReloadMode::Serial),
            (true, true, false, ReloadMode::Overlap),
            (true, false, true, ReloadMode::Overlap),
        ];
        for (probe, wait_ready, reuse_port, want) in cases {
            let mut app = if probe {
                probed_app("web")
            } else {
                AppConfig::minimal("web", "./srv")
            };
            app.wait_ready = wait_ready;
            app.reuse_port = reuse_port;

            let source = ReadinessSource::of(&app).expect("the target parses");
            assert_eq!(
                ReloadMode::of(&app, &source),
                want,
                "probe={probe} wait_ready={wait_ready} reuse_port={reuse_port}"
            );
        }
    }

    // An overlap would let the drainee answer the replacement's probe: the
    // first probe lands at t=0, with the drainee still bound to the address the
    // probe names. No scripts, so a `SpawnNew` here would panic.
    #[tokio::test(start_paused = true)]
    async fn a_probed_reload_drains_before_it_spawns_anything() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_with_one_online_sheep_of(&dir, probed_app("web"), vec![]);
        let (ctl_tx, mut ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        actor.sheep.get_mut(&0).expect("the fixture's sheep").ctl = Some(ctl_tx);

        actor.advance_reload("web", VecDeque::from([0]));

        let job = &actor.reloads["web"];
        assert_eq!(job.mode, ReloadMode::Serial);
        assert_eq!(job.swap.phase, ReloadPhase::DrainFirst);
        assert_eq!(
            job.swap.new_id, None,
            "a serial reload has no replacement until the drain is over"
        );
        assert_eq!(
            actor.sheep.len(),
            1,
            "nothing was spawned into the slot the drain is emptying"
        );

        let drainee = &actor.sheep[&0];
        assert_eq!(drainee.entry.status, ProcStatus::Stopping);
        assert_eq!(
            drainee.entry.reload,
            ReloadState::Drainee { new_id: None },
            "the marker is what keeps this exit off the ordinary respawn path"
        );
        assert!(
            drainee.manual.is_some(),
            "the drain owns the drainee's next exit"
        );
        assert!(
            ctl_rx.try_recv().is_ok(),
            "the drain asked the instance to go"
        );
    }

    // The case above's app with one line added: spawn the replacement at once,
    // and leave the instance being replaced running.
    #[tokio::test(start_paused = true)]
    async fn reuse_port_buys_back_the_overlap_a_probe_would_otherwise_cost() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = probed_app("web");
        app.reuse_port = true;
        // One script for the replacement: the fixture's own instance is
        // registered without spawning.
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep_of(&dir, app, vec![ProcScript::never_exits()]);

        actor.advance_reload("web", VecDeque::from([0]));

        let job = &actor.reloads["web"];
        assert_eq!(job.mode, ReloadMode::Overlap);
        assert_eq!(job.swap.phase, ReloadPhase::AwaitReady);
        let new_id = job
            .swap
            .new_id
            .expect("an overlapping reload spawns at once");
        assert_eq!(
            actor.sheep[&new_id].entry.instance, actor.sheep[&0].entry.instance,
            "the replacement takes the same instance slot"
        );
        assert_eq!(
            actor.sheep[&0].entry.status,
            ProcStatus::Stopping,
            "the instance being replaced is still there, marked and serving"
        );
    }

    // The replacement inherits what the drainee's entry is the last copy of,
    // the instance slot most of all: `assemble` writes it into the child's
    // environment, and a different slot binds a different port.
    #[tokio::test(start_paused = true)]
    async fn a_serial_reload_spawns_into_the_slot_the_drain_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_with_one_online_sheep_of(
            &dir,
            probed_app("web"),
            vec![ProcScript::never_exits()],
        );
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        actor.sheep.get_mut(&0).expect("the fixture's sheep").ctl = Some(ctl_tx);
        let instance = actor.sheep[&0].entry.instance;
        let restarts = actor.sheep[&0].entry.restarts;

        actor.advance_reload("web", VecDeque::from([0]));
        actor.handle_exited(
            0,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        );

        assert!(
            !actor.sheep.contains_key(&0),
            "the drained instance is deregistered, not left as a second row"
        );
        let new_id = actor.reloads["web"]
            .swap
            .new_id
            .expect("the reap is where a serial reload spawns");
        let replacement = &actor.sheep[&new_id].entry;
        assert_eq!(replacement.instance, instance);
        assert_eq!(replacement.restarts, restarts);
        assert_eq!(replacement.status, ProcStatus::Starting);
        assert_eq!(replacement.reload, ReloadState::Replacement);
        assert_eq!(
            actor.reloads["web"].swap.phase,
            ReloadPhase::DrainOld,
            "with the drainee gone there is nothing left to abandon back to"
        );
    }

    // A reload replaces `Online` instances and a parked replacement is not one,
    // so without this a rollback gets `Ok` from a daemon that skipped the app's
    // only instance.
    #[tokio::test(start_paused = true)]
    async fn a_reload_can_still_replace_the_instance_a_failed_reload_parked() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_with_one_online_sheep_of(&dir, probed_app("web"), vec![]);
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        let slot = actor.sheep.get_mut(&0).expect("the fixture's sheep");
        slot.ctl = Some(ctl_tx);
        // What `reload_ready_result` leaves behind when a replacement never
        // answers and there is no drainee to hand back to.
        slot.entry.status = ProcStatus::Starting;
        slot.ready_failed = true;

        // Through `handle_reload`: the selector pass has its own eligibility
        // filter, and it is the one a rollback meets first.
        let (reply, _rx) = oneshot::channel();
        actor.handle_reload(&ProcessSelector::Name("web".to_string()), reply);

        assert!(
            actor.reloads.contains_key("web"),
            "a parked instance is still replaceable, or nothing can roll it back"
        );
        assert_eq!(actor.reloads["web"].swap.old_id, 0);
    }

    // An instance still inside its own readiness wait has a verdict coming, so
    // draining it would throw away a start already under way.
    #[tokio::test(start_paused = true)]
    async fn an_ordinarily_starting_sheep_is_still_skipped_by_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_with_one_online_sheep_of(&dir, probed_app("web"), vec![]);
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        let slot = actor.sheep.get_mut(&0).expect("the fixture's sheep");
        slot.ctl = Some(ctl_tx);
        slot.entry.status = ProcStatus::Starting;

        let (reply, _rx) = oneshot::channel();
        actor.handle_reload(&ProcessSelector::Name("web".to_string()), reply);

        assert!(
            actor.reloads.is_empty(),
            "a sheep still waiting on its own readiness is left to finish"
        );
        assert_eq!(actor.sheep[&0].entry.status, ProcStatus::Starting);
    }

    // A parked instance can be a drainee, so a rollback whose replacement fails
    // to spawn would leave the app reading `online` while nothing answers.
    #[tokio::test(start_paused = true)]
    async fn undoing_a_swap_never_promotes_a_parked_drainee_to_online() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = probed_app("web");
        // Overlap, so the drainee is marked and then restored in place; a
        // serial swap has no drainee left to restore when it can fail.
        app.reuse_port = true;
        // No scripts, so `runner.spawn` fails and `spawn_replacement` takes
        // the `Err` arm that does the restoring.
        let (mut actor, _mailbox) = actor_with_one_online_sheep_of(&dir, app, vec![]);
        let slot = actor.sheep.get_mut(&0).expect("the fixture's sheep");
        slot.entry.status = ProcStatus::Starting;
        slot.ready_failed = true;

        actor
            .spawn_replacement(0, ReloadMode::Overlap)
            .expect_err("a runner with no scripts left cannot spawn");

        let slot = actor.sheep.get(&0).expect("the drainee stays registered");
        assert_eq!(
            slot.entry.status,
            ProcStatus::Starting,
            "an instance that never proved itself is not restored to online"
        );
        assert_eq!(slot.entry.reload, ReloadState::None);
        assert!(
            slot.ready_failed,
            "and it stays replaceable, or the next attempt cannot reach it"
        );
    }

    // The `abort_reload` half: the sibling above covers the spawn that never
    // happened, this the swap that started and was given up.
    #[tokio::test(start_paused = true)]
    async fn abandoning_a_swap_never_promotes_a_parked_drainee_to_online() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = probed_app("web");
        app.reuse_port = true;
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep_of(&dir, app, vec![ProcScript::never_exits()]);
        // A live control sender says this instance's task is still there to go
        // back to; the fixture leaves it `None`.
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        let slot = actor.sheep.get_mut(&0).expect("the fixture's sheep");
        slot.ctl = Some(ctl_tx);
        slot.entry.status = ProcStatus::Starting;
        slot.ready_failed = true;

        let (reply, _rx) = oneshot::channel();
        actor.handle_reload(&ProcessSelector::Name("web".to_string()), reply);
        assert!(
            actor.reloads.contains_key("web"),
            "the parked instance is the one being replaced"
        );

        actor.abort_reload("web", "the replacement was not ready inside listen_timeout");

        assert_eq!(
            actor.sheep[&0].entry.status,
            ProcStatus::Starting,
            "an instance that never proved itself is not restored to online"
        );
        assert!(actor.sheep[&0].ready_failed);
    }

    // --- Reload: the post-drain check an overlap still owes ---

    // Only an overlapping reload of a probed app has a first answer the reaped
    // instance could have given: a serial one asked with the slot empty, a
    // channel is per instance, and a heuristic has nothing to re-run.
    #[test]
    fn only_an_overlapping_probe_is_re_asked_after_the_drain() {
        let dir = tempfile::tempdir().unwrap();
        let mut probed = probed_app("web");
        probed.reuse_port = true;
        let mut channel = AppConfig::minimal("web", "./srv");
        channel.wait_ready = true;

        for (app, mode, want) in [
            (probed.clone(), ReloadMode::Overlap, true),
            (probed, ReloadMode::Serial, false),
            (channel, ReloadMode::Overlap, false),
            (
                AppConfig::minimal("web", "./srv"),
                ReloadMode::Overlap,
                false,
            ),
        ] {
            let (actor, _mailbox) = actor_with_one_online_sheep_of(&dir, app, vec![]);
            assert_eq!(
                actor.post_drain_probe(0, mode).is_some(),
                want,
                "mode={mode:?}"
            );
        }
    }

    /// An actor holding one `Online` instance whose reload has reached
    /// [`ReloadPhase::Verify`]: the drainee reaped, the replacement serving,
    /// and only the second probe's verdict left to arrive.
    fn actor_awaiting_a_verdict(
        dir: &tempfile::TempDir,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<Msg>) {
        let mut app = probed_app("web");
        app.reuse_port = true;
        let (mut actor, mailbox) = actor_with_one_online_sheep_of(dir, app, vec![]);
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture's sheep")
            .entry
            .reload = ReloadState::Replacement;
        actor.reloads.insert(
            "web".to_string(),
            ReloadJob {
                queue: VecDeque::new(),
                mode: ReloadMode::Overlap,
                deadline: 0,
                swap: ReloadSwap {
                    // Reaped, which is what puts the swap in `Verify` at all.
                    old_id: 99,
                    new_id: Some(0),
                    phase: ReloadPhase::Verify,
                },
            },
        );
        (actor, mailbox)
    }

    // Both instances were in one `SO_REUSEPORT` group, so the first probe
    // proves nothing about which of them answered.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_fails_the_second_probe_is_demoted_and_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_awaiting_a_verdict(&dir);
        let mut rx = actor.events.subscribe();

        actor.handle_reload_verified("web", 0, Readiness::TimedOut);

        assert!(actor.reloads.is_empty(), "the reload is over either way");
        assert!(
            actor.sheep.contains_key(&0),
            "the only instance the app has left is not killed as well"
        );
        assert_eq!(
            actor.sheep[&0].entry.status,
            ProcStatus::Starting,
            "an instance that never answered alone is not online"
        );
        assert_eq!(
            drained_process_kinds(&mut rx),
            vec![ProcessEventKind::ReloadAbandoned],
            "an abandonment, and never the `Reloaded` that claims a success"
        );
    }

    // The control for the case above: same swap, same handler, opposite
    // verdict.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_answers_alone_finishes_the_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_awaiting_a_verdict(&dir);
        let mut rx = actor.events.subscribe();

        actor.handle_reload_verified("web", 0, Readiness::Ready);

        assert!(actor.reloads.is_empty());
        assert_eq!(actor.sheep[&0].entry.status, ProcStatus::Online);
        assert_eq!(actor.sheep[&0].entry.reload, ReloadState::None);
        assert_eq!(
            drained_process_kinds(&mut rx),
            vec![ProcessEventKind::Reloaded]
        );
    }

    // Each arming replaces the last, so only the stamp the job carries may end
    // the swap. The second probe takes up to another `listen_timeout`, so a job
    // whose first probe and drain ate the original window would otherwise be
    // abandoned mid-verify with the rest of the queue on the old code.
    #[tokio::test(start_paused = true)]
    async fn a_re_armed_watchdog_retires_the_one_it_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_awaiting_a_verdict(&dir);
        // Both armings are made here: the fixture builds its job by hand, so its
        // stamp is not one this counter issued.
        actor.arm_reload_deadline("web", 0);
        let first = actor.reloads["web"].deadline;

        actor.arm_reload_deadline("web", 0);
        let second = actor.reloads["web"].deadline;
        assert_ne!(first, second, "each arming takes a stamp of its own");

        actor.handle_reload_deadline("web", first);
        assert!(
            actor.reloads.contains_key("web"),
            "the retired watchdog must not end a swap the live one is still watching"
        );

        actor.handle_reload_deadline("web", second);
        assert!(actor.reloads.is_empty(), "the live one still ends it");
    }

    // `DrainFirst` is the one phase an operator's command reaches without
    // `uncommitted_swap_of` seeing it: the instance has already been asked to
    // go, so there is nothing to abandon back to.
    #[tokio::test(start_paused = true)]
    async fn a_delete_during_a_serial_drain_spawns_no_replacement() {
        let dir = tempfile::tempdir().unwrap();
        // No scripts: a spawn here would panic, which is half the assertion.
        let (mut actor, _mailbox) = actor_with_one_online_sheep_of(&dir, probed_app("web"), vec![]);
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        actor.sheep.get_mut(&0).expect("the fixture's sheep").ctl = Some(ctl_tx);

        actor.advance_reload("web", VecDeque::from([0]));
        assert_eq!(actor.reloads["web"].swap.phase, ReloadPhase::DrainFirst);
        // What `begin_manual` leaves behind for a `delete` that matched a
        // running sheep whose `manual` marker the drain already owns.
        actor.sheep.get_mut(&0).expect("the drainee").pending_delete = true;

        actor.handle_exited(
            0,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        );

        assert!(actor.sheep.is_empty(), "the delete emptied the flock");
        assert!(actor.reloads.is_empty(), "and the reload ended with it");
    }

    // `ready_failed` makes a parked instance replaceable; left set after its
    // process exits without a respawn it makes a `Stopped` row replaceable, and
    // a reload against one of those drains a row with nothing behind it.
    #[tokio::test(start_paused = true)]
    async fn a_parked_instances_verdict_does_not_outlive_its_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = probed_app("web");
        // The arrangement that reaches `Stopped` rather than a respawn.
        app.autorestart = false;
        let (mut actor, _mailbox) = actor_with_one_online_sheep_of(&dir, app, vec![]);
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        let slot = actor.sheep.get_mut(&0).expect("the fixture's sheep");
        slot.ctl = Some(ctl_tx);
        slot.entry.status = ProcStatus::Starting;
        slot.ready_failed = true;

        actor.handle_exited(
            0,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        );

        let slot = actor.sheep.get(&0).expect("a clean stop keeps the row");
        assert_ne!(slot.entry.status, ProcStatus::Online);
        assert!(!slot.ready_failed);
        assert!(
            !reload_eligible(slot),
            "a row with no process behind it is not something a reload may replace"
        );
    }

    // --- Reload: the per-instance swap machine ---

    /// A window covering a whole swap (`listen_timeout` + `graceful_timeout` +
    /// room), so a case whose event never arrives fails instead of parking the
    /// suite. Virtual time, so an early swap costs nothing.
    const SWAP_WINDOW: Duration = Duration::from_secs(30);

    /// Drives virtual time until `kind` arrives for `id`, failing rather than
    /// hanging if it never does.
    async fn expect_event(
        rx: &mut tokio::sync::broadcast::Receiver<SharedEvent>,
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

    /// One started app, the runner behind it and a bus subscriber.
    ///
    /// The runner is shared rather than moved so a case can read
    /// `kill_counts().len()`, the number of spawns that succeeded.
    async fn started(
        dir: &tempfile::TempDir,
        app: AppConfig,
        scripts: Vec<ProcScript>,
    ) -> (
        SupervisorHandle,
        Arc<ScriptedRunner>,
        tokio::sync::broadcast::Receiver<SharedEvent>,
    ) {
        let (events, rx) = crate::bus::test_bus(256);
        let runner = Arc::new(ScriptedRunner::new(scripts));
        let handle = spawn_supervisor(SharedRunner(Arc::clone(&runner)), test_paths(dir), events);
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        (handle, runner, rx)
    }

    /// A bare actor holding one `Online` sheep, for the cases that drive a
    /// handler directly: a swap's ownership lives in `ProcessEntry::reload`,
    /// which is crate-internal and never on the wire.
    fn actor_with_one_online_sheep(
        dir: &tempfile::TempDir,
        scripts: Vec<ProcScript>,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<Msg>) {
        actor_with_one_online_sheep_of(dir, AppConfig::minimal("web", "./srv"), scripts)
    }

    /// [`actor_with_one_online_sheep`] for a case that needs a particular app:
    /// a `readiness_probe`, a `reuse_port`, or both, which between them decide
    /// which reload the instance gets.
    fn actor_with_one_online_sheep_of(
        dir: &tempfile::TempDir,
        app: AppConfig,
        scripts: Vec<ProcScript>,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<Msg>) {
        let paths = test_paths(dir);
        let app = normalize(app).unwrap();
        let mut sheep = HashMap::new();
        sheep.insert(
            0,
            SheepSlot {
                entry: armed_entry(0, 0, 1111, app, &paths),
                ctl: None,
                log_ctl: None,
                to_child: None,
                signals: None,
                to_stdin: None,
                manual: None,
                pending_delete: false,
                epoch: 0,
                ready_tx: None,
                actions: ActionWaits::default(),
                ready_failed: false,
                restart_due: None,
            },
        );
        let (events, _events_rx) = crate::bus::test_bus(64);
        let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
        let actor = Actor {
            runner: ScriptedRunner::new(scripts),
            paths,
            events,
            tx,
            sheep,
            next_id: 1,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        };
        (actor, rx)
    }

    /// [`ProcessEntry::id`] of the fixture's sheep, and of its dog.
    const SHEEP_ID: u32 = 0;
    const DOG_ID: u32 = 1;

    /// A bare actor holding one `Online` sheep and one `Online` dog.
    ///
    /// Alike in everything a selector can read, so the dog marker is the only
    /// difference a case can attribute an answer to.
    fn actor_with_a_sheep_and_a_dog(
        dir: &tempfile::TempDir,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<Msg>) {
        let paths = test_paths(dir);
        let mut sheep = HashMap::new();
        for (id, name, dog) in [
            (SHEEP_ID, "web", None),
            (DOG_ID, "bark", Some(DogSource::BuiltIn)),
        ] {
            let app = app_with(name, |config| config.fold = Some("svc".to_string()));
            let mut entry = armed_entry(id, 0, 1111 + id, app, &paths);
            entry.dog = dog;
            sheep.insert(
                id,
                SheepSlot {
                    entry,
                    ctl: None,
                    log_ctl: None,
                    to_child: None,
                    signals: None,
                    to_stdin: None,
                    manual: None,
                    pending_delete: false,
                    epoch: 0,
                    ready_tx: None,
                    actions: ActionWaits::default(),
                    ready_failed: false,
                    restart_due: None,
                },
            );
        }
        let (events, _events_rx) = crate::bus::test_bus(64);
        let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
        let actor = Actor {
            runner: ScriptedRunner::new(Vec::new()),
            paths,
            events,
            tx,
            sheep,
            next_id: DOG_ID + 1,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        };
        (actor, rx)
    }

    /// Without the last two assertions, a helper that excluded dogs from
    /// everything passes and `shep disable bark` would match nothing.
    #[test]
    fn a_wildcard_passes_a_dog_by_and_its_own_name_still_reaches_it() {
        let dir = tempfile::tempdir().unwrap();
        let (actor, _mailbox) = actor_with_a_sheep_and_a_dog(&dir);

        assert_eq!(
            actor.matching_ids(&ProcessSelector::All),
            vec![SHEEP_ID],
            "`all` is the flock, not the kennel"
        );
        assert_eq!(
            actor.matching_ids(&ProcessSelector::parse("/^(web|bark)$/").unwrap()),
            vec![SHEEP_ID],
            "a sweep that spells both names out is still a sweep"
        );
        assert_eq!(
            actor.matching_ids(&ProcessSelector::Fold("svc".into())),
            vec![SHEEP_ID],
            "a dog shares its fold with the flock and is still not swept by it"
        );
        assert_eq!(
            actor.matching_ids(&ProcessSelector::Name("bark".into())),
            vec![DOG_ID]
        );
        assert_eq!(
            actor.matching_ids(&ProcessSelector::Id(DOG_ID)),
            vec![DOG_ID]
        );
    }

    #[test]
    fn a_listing_reports_where_a_dog_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let (actor, _mailbox) = actor_with_a_sheep_and_a_dog(&dir);

        assert_eq!(
            to_info(&actor.sheep[&DOG_ID].entry, &actor.smits).dog,
            Some(DogSource::BuiltIn)
        );
        assert_eq!(
            to_info(&actor.sheep[&SHEEP_ID].entry, &actor.smits).dog,
            None
        );
    }

    /// Starts `app` (normalized) through `h`'s supervisor and hands back the
    /// snapshot the start answers with.
    ///
    /// # Panics
    ///
    /// Panics if `app` does not normalize, or if the actor refuses the start.
    /// No `#[track_caller]`: it is a no-op on an async fn.
    async fn start_app(h: &Harness, app: AppConfig) -> Vec<ProcessInfo> {
        h.ctx
            .supervisor
            .start(vec![normalize(app).unwrap()])
            .await
            .unwrap()
    }

    /// The instance slot of every registered instance of `name`, ascending.
    ///
    /// Read off `out_file` rather than off the entry: a build deriving the log
    /// path from something else would pass an assertion on the internal field.
    ///
    /// # Panics
    ///
    /// If a matched row carries no `out_file`, or one this fixture cannot parse.
    async fn instance_slots_of(h: &Harness, name: &str) -> Vec<u32> {
        let mut slots: Vec<u32> = h
            .ctx
            .supervisor
            .list()
            .await
            .iter()
            .filter(|info| info.name == name)
            .map(|info| {
                let out = info
                    .out_file
                    .as_deref()
                    .expect("a listed sheep has a log path");
                let stem = std::path::Path::new(out)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .and_then(|file| file.strip_suffix("-out.log"))
                    .and_then(|stem| stem.strip_prefix(&format!("{name}-")))
                    .expect("a derived log path is `<name>-<instance>-out.log`");
                stem.parse().expect("the instance slot is a number")
            })
            .collect();
        slots.sort_unstable();
        slots
    }

    /// Waits until `name` has exactly `count` registered instances, or fails.
    ///
    /// A scale-down's reply is the survivors and does not wait for the
    /// departures, so a case asserting on the flock afterwards has to wait for
    /// the kill ladders it started. Bounded, or the poll loop never ends.
    async fn settle_to(h: &Harness, name: &str, count: usize) {
        let settled = tokio::time::timeout(SWAP_WINDOW, async {
            loop {
                let live = h
                    .ctx
                    .supervisor
                    .list()
                    .await
                    .iter()
                    .filter(|info| info.name == name)
                    .count();
                if live == count {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(settled.is_ok(), "{name} never settled to {count} instances");
    }

    /// A bare actor holding `instances` online instances of one app, all
    /// carrying the same normalized spec. The stored-count cases need it,
    /// since that field reaches no reply.
    fn actor_with_a_scaled_app(
        dir: &tempfile::TempDir,
        instances: u32,
        scripts: Vec<ProcScript>,
    ) -> Actor<ScriptedRunner> {
        let paths = test_paths(dir);
        let app = normalize(AppConfig {
            instances,
            ..AppConfig::minimal("web", "./srv")
        })
        .unwrap();
        let mut sheep = HashMap::new();
        for instance in 0..instances {
            sheep.insert(
                instance,
                SheepSlot {
                    entry: armed_entry(instance, instance, 1111 + instance, app.clone(), &paths),
                    ctl: None,
                    log_ctl: None,
                    to_child: None,
                    signals: None,
                    to_stdin: None,
                    manual: None,
                    pending_delete: false,
                    epoch: 0,
                    ready_tx: None,
                    actions: ActionWaits::default(),
                    ready_failed: false,
                    restart_due: None,
                },
            );
        }
        let (events, _events_rx) = crate::bus::test_bus(64);
        let (tx, _rx) = mpsc::channel(MAILBOX_CAPACITY);
        Actor {
            runner: ScriptedRunner::new(scripts),
            paths,
            events,
            tx,
            sheep,
            next_id: instances,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        }
    }

    /// Every registered slot's stored instance count, ascending by id.
    fn stored_instance_counts(actor: &Actor<ScriptedRunner>) -> Vec<u32> {
        let mut ids: Vec<u32> = actor.sheep.keys().copied().collect();
        ids.sort_unstable();
        ids.iter()
            .map(|id| actor.sheep[id].entry.spec.config().instances)
            .collect()
    }

    /// A `ReloadJob` built the way `advance_reload` builds one, for a case that
    /// only needs `self.reloads` to hold an entry under `name`. `name` is for
    /// the call site's readability: the map key is what associates a job with
    /// an app.
    fn reload_job_for(name: &str) -> ReloadJob {
        let _ = name;
        ReloadJob {
            queue: VecDeque::new(),
            mode: ReloadMode::Overlap,
            deadline: 0,
            swap: ReloadSwap {
                old_id: 0,
                new_id: Some(1),
                phase: ReloadPhase::AwaitReady,
            },
        }
    }

    /// Slot numbers reach the app (`SHEP_INSTANCE`) and the filesystem
    /// (`web-2-out.log`), so which ones a scale hands out is a contract.
    #[tokio::test(start_paused = true)]
    async fn scaling_up_fills_the_lowest_free_slots() {
        let h = harness(vec![ProcScript::never_exits(); 4]);
        start_app(
            &h,
            AppConfig {
                instances: 2,
                ..AppConfig::minimal("web", "./srv")
            },
        )
        .await;

        let scaled = h.ctx.supervisor.scale("web", 4).await.unwrap();

        assert_eq!(scaled.instances.len(), 4);
        assert_eq!(instance_slots_of(&h, "web").await, vec![0, 1, 2, 3]);
    }

    /// Taking the highest makes 2 -> 4 -> 2 a round trip back to slots 0 and 1;
    /// taking the lowest leaves 2 and 3, with different log files.
    #[tokio::test(start_paused = true)]
    async fn scaling_down_removes_the_highest_slots_so_a_round_trip_returns() {
        let h = harness(vec![ProcScript::never_exits(); 4]);
        start_app(
            &h,
            AppConfig {
                instances: 2,
                ..AppConfig::minimal("web", "./srv")
            },
        )
        .await;

        h.ctx.supervisor.scale("web", 4).await.unwrap();
        let scaled = h.ctx.supervisor.scale("web", 2).await.unwrap();

        assert_eq!(scaled.instances.len(), 2);
        // The reply is the survivors: the two removals still have ladders to run.
        settle_to(&h, "web", 2).await;
        assert_eq!(instance_slots_of(&h, "web").await, vec![0, 1]);
    }

    /// An operator re-running a provisioning script must not restart the flock.
    #[tokio::test(start_paused = true)]
    async fn scaling_to_the_current_count_is_a_no_op() {
        let h = harness(vec![ProcScript::never_exits(); 2]);
        start_app(
            &h,
            AppConfig {
                instances: 2,
                ..AppConfig::minimal("web", "./srv")
            },
        )
        .await;
        let before = h.ctx.supervisor.list().await;

        let scaled = h.ctx.supervisor.scale("web", 2).await.unwrap();

        assert_eq!(scaled.instances.len(), 2);
        let after = h.ctx.supervisor.list().await;
        assert_eq!(
            after.iter().map(|i| i.id).collect::<Vec<_>>(),
            before.iter().map(|i| i.id).collect::<Vec<_>>(),
            "a no-op scale replaced processes"
        );
        // Two scripts for two spawns: a scale that respawned would need a third.
    }

    /// `normalize` refuses `instances == 0` on every other path into the daemon.
    #[tokio::test(start_paused = true)]
    async fn scaling_to_zero_is_refused_and_names_delete() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_app(&h, AppConfig::minimal("web", "./srv")).await;

        let err = h.ctx.supervisor.scale("web", 0).await.unwrap_err();

        let SupervisorError::InvalidScale(message) = err else {
            panic!("expected InvalidScale, got {err:?}");
        };
        assert!(message.contains("delete"), "{message}");
    }

    /// `shep stock typo 4` must not exit 0.
    #[tokio::test(start_paused = true)]
    async fn scaling_an_unregistered_app_is_not_found() {
        let h = harness(vec![]);
        assert_eq!(
            h.ctx.supervisor.scale("ghost", 2).await.unwrap_err(),
            SupervisorError::NotFound
        );
    }

    /// A dog is one process by contract: two metrics dogs would race for the
    /// same listen port, and two bark dogs would double every alert.
    #[tokio::test(start_paused = true)]
    async fn a_dog_cannot_be_scaled() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_with_a_sheep_and_a_dog(&dir);

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Scale {
            name: "bark".to_string(),
            count: 2,
            reply,
        });

        let SupervisorError::InvalidScale(message) = answer.await.unwrap().unwrap_err() else {
            panic!("expected InvalidScale");
        };
        assert!(message.contains("dog"), "{message}");
    }

    /// A reload holds two live processes in one instance slot; a scale-down
    /// picking that slot removes one and leaves the swap with nothing to finish.
    ///
    /// Actor-tier: the guard reads `Actor::reloads`, which has no reply-side
    /// spelling to fill from a handle.
    #[tokio::test(start_paused = true)]
    async fn an_app_mid_reload_refuses_a_scale() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_a_scaled_app(&dir, 2, vec![]);
        actor
            .reloads
            .insert("web".to_string(), reload_job_for("web"));

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Scale {
            name: "web".to_string(),
            count: 4,
            reply,
        });

        assert_eq!(
            answer.await.unwrap().unwrap_err(),
            SupervisorError::ReloadInFlight("web".to_string())
        );
    }

    /// A scale-down's reply is the survivors and does not wait for the
    /// departures, so those slots stay registered: a second scale counting them
    /// calls itself a no-op and lets `rpc` record `instances = 4` for a flock
    /// that settles to one.
    ///
    /// The doomed three `never_reports_its_exit`, so the case is deterministic
    /// rather than a race against its own kill ladders.
    #[tokio::test(start_paused = true)]
    async fn a_scale_is_refused_while_an_earlier_ones_departures_are_still_leaving() {
        // Scripts go out in spawn order, so the survivor gets the first.
        let h = harness(vec![
            ProcScript::never_exits(),
            ProcScript::never_reports_its_exit(),
            ProcScript::never_reports_its_exit(),
            ProcScript::never_reports_its_exit(),
        ]);
        start_app(
            &h,
            AppConfig {
                instances: 4,
                ..AppConfig::minimal("web", "./srv")
            },
        )
        .await;

        let down = h.ctx.supervisor.scale("web", 1).await.unwrap();
        assert_eq!(down.instances.len(), 1);

        let err = h.ctx.supervisor.scale("web", 4).await.unwrap_err();

        let SupervisorError::InvalidScale(message) = err else {
            panic!("expected InvalidScale, got {err:?}");
        };
        assert!(
            message.contains("3 instance(s) still shutting down"),
            "the refusal has to say how much of the flock is still moving: {message}"
        );
        assert!(
            message.contains("shep flock"),
            "the refusal has to name what to wait for: {message}"
        );
    }

    /// A refusal that never lifts leaves the operator unable to scale the app.
    ///
    /// `never_exits` rather than the case above's wedged scripts: these obey
    /// `SIGTERM`, so `settle_to` is the forcing mechanism.
    #[tokio::test(start_paused = true)]
    async fn a_scale_is_accepted_again_once_the_departures_have_left() {
        // Four for the first flock, three for the scale back up.
        let h = harness(vec![ProcScript::never_exits(); 7]);
        start_app(
            &h,
            AppConfig {
                instances: 4,
                ..AppConfig::minimal("web", "./srv")
            },
        )
        .await;

        h.ctx.supervisor.scale("web", 1).await.unwrap();
        settle_to(&h, "web", 1).await;

        let up = h.ctx.supervisor.scale("web", 4).await.unwrap();

        assert_eq!(up.instances.len(), 4);
        assert_eq!(up.shortfall, None);
        assert_eq!(instance_slots_of(&h, "web").await, vec![0, 1, 2, 3]);
    }

    /// Without the write-back, `shep stock web 4 && shep save` records
    /// `instances = 2` and the next reboot reverts the scale.
    ///
    /// Actor-tier: the stored count is `SheepSlot::entry.spec`, which reaches no
    /// reply and no bus event. `Scaled::app` is checked too, since a build that
    /// returned the right config and stored the wrong one passes either alone.
    #[tokio::test(start_paused = true)]
    async fn a_scale_updates_the_stored_instance_count_on_every_slot() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_a_scaled_app(&dir, 2, vec![ProcScript::never_exits(); 2]);

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Scale {
            name: "web".to_string(),
            count: 4,
            reply,
        });

        let scaled = answer.await.unwrap().unwrap();
        assert_eq!(scaled.app.config().instances, 4);
        assert_eq!(stored_instance_counts(&actor), vec![4, 4, 4, 4]);
    }

    /// The instances that did spawn stay, since unwinding them would turn one
    /// failed spawn into an outage, but every registered slot must claim the
    /// number really running.
    ///
    /// One script for two requested spawns, this module's way to make exactly
    /// one spawn fail. Three entries, not two: `spawn_fresh` registers an
    /// `Errored` slot for the failed attempt, which a later scale counts.
    #[tokio::test(start_paused = true)]
    async fn a_partial_scale_up_stores_the_count_it_achieved() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_a_scaled_app(&dir, 1, vec![ProcScript::never_exits()]);

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Scale {
            name: "web".to_string(),
            count: 3,
            reply,
        });

        let scaled = answer.await.unwrap().expect(
            "a partial scale-up is a partial success: an `Err` here would take \
             the achieved config with it and leave the roll pre-scale",
        );
        assert_eq!(scaled.requested, 3);
        assert_eq!(scaled.achieved(), 2);
        assert!(
            scaled.shortfall.is_some(),
            "the shortfall has to survive the reply, or nothing downstream can \
             tell the operator they got two of three"
        );
        assert_eq!(scaled.app.config().instances, 2);
        assert_eq!(
            stored_instance_counts(&actor),
            vec![2, 2, 2],
            "the flock achieved two, so every registered slot — including the \
             errored attempt — must say two"
        );
    }

    /// Built so id order and name order disagree: a fixture whose two orders
    /// coincide cannot tell the two implementations apart.
    #[tokio::test]
    async fn a_listing_groups_an_apps_instances_under_its_name() {
        let h = harness(vec![ProcScript::never_exits(); 4]);
        start_app(
            &h,
            AppConfig {
                instances: 2,
                ..AppConfig::minimal("zebra", "./z")
            },
        )
        .await;
        start_app(
            &h,
            AppConfig {
                instances: 2,
                ..AppConfig::minimal("alpha", "./a")
            },
        )
        .await;

        let listed = h.ctx.supervisor.list().await;
        let names: Vec<&str> = listed.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["alpha", "alpha", "zebra", "zebra"]);
        let ids: Vec<u32> = listed.iter().map(|i| i.id).collect();
        assert_ne!(
            ids,
            {
                let mut sorted = ids.clone();
                sorted.sort_unstable();
                sorted
            },
            "the fixture must make id order and name order disagree, or it proves nothing"
        );
    }

    /// One dog's app spec. The path is a label: [`ScriptedRunner`] replays a
    /// script instead of exec'ing anything, so nothing has to exist there.
    fn dog_app(name: &str) -> ResolvedApp {
        normalize(AppConfig::minimal(name, "/nonexistent/shep")).unwrap()
    }

    /// The `bark` row of a listing, or a panic naming what was there instead.
    fn dog_row(listed: &[ProcessInfo], id: u32) -> ProcessInfo {
        listed
            .iter()
            .find(|info| info.id == id)
            .unwrap_or_else(|| panic!("id {id} left the flock: {listed:?}"))
            .clone()
    }

    /// A marker written by the start path rather than carried by the entry is
    /// invisible until a dog crashes once: the dog leaves the dogs table and
    /// reappears among the flock, with no error anywhere.
    ///
    /// Two scripts, both used by a correct run: the crash, and the process the
    /// restart produces.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_restarts_is_still_a_dog() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = crate::bus::test_bus(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::const_exit(1), ProcScript::never_exits()]);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        let dog = handle
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();
        assert_eq!(dog.dog, Some(DogSource::BuiltIn));

        // `Restart` is emitted from inside the respawn, after the entry has been
        // rewritten, so a listing taken once it lands cannot read the entry
        // mid-flight.
        expect_event(&mut rx, dog.id, ProcessEventKind::Restart).await;
        let after = dog_row(&handle.list().await, dog.id);

        assert_eq!(
            after.restarts, 1,
            "the ordinary restart path, not a dog one"
        );
        assert_eq!(after.dog, Some(DogSource::BuiltIn));
    }

    /// A dog whose binary is not there, which is what `adopt` with a bad path
    /// produces, has to be visible in the dogs table as `Errored`; an unmarked
    /// one is a sheep nobody started.
    ///
    /// No scripts: [`ScriptedRunner`] fails a spawn by running out of them.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_cannot_be_spawned_is_still_a_dog() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let handle = spawn_supervisor(ScriptedRunner::new(Vec::new()), test_paths(&dir), events);

        let failed = handle
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .expect_err("a spawn with no script behind it cannot succeed");
        assert!(matches!(failed, SupervisorError::SpawnFailed(_)));

        let listed = handle.list().await;
        let errored = dog_row(&listed, 0);
        assert_eq!(errored.status, ProcStatus::Errored);
        assert_eq!(errored.dog, Some(DogSource::BuiltIn));
    }

    /// `PerApp` skips an app whose credentials do not resolve, so `credentials`
    /// is shorter than `apps` while still in order: a naive `zip` against `apps`
    /// pairs app 2 with app 3's credentials and drops the last app. The first of
    /// three fails, the ordering where the drift starts at the next app.
    ///
    /// Asserts the identity each app got, not merely that it is registered:
    /// under the broken pairing all three are present, holding each other's
    /// credentials.
    ///
    /// `#[cfg(unix)]`: `nix` is a unix-only dependency, and `privilege::resolve`
    /// refuses any user request off-platform.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_credential_failure_never_shifts_another_apps_identity() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts, for the two apps that must survive the first's failure.
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits(); 2]);

        let own_uid = nix::unistd::geteuid();
        let own_user = nix::unistd::User::from_uid(own_uid)
            .expect("the passwd database is readable")
            .expect("this process's own uid has a passwd entry")
            .name;

        let mut bad = AppConfig::minimal("a-bad", "./a");
        bad.user = Some("definitely-not-a-real-shep-user".to_string());
        let mut mine = AppConfig::minimal("b-mine", "./b");
        mine.user = Some(own_user);
        let plain = AppConfig::minimal("c-plain", "./c");

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Start {
            apps: vec![
                normalize(bad).unwrap(),
                normalize(mine).unwrap(),
                normalize(plain).unwrap(),
            ],
            policy: BatchPolicy::PerApp,
            reply,
        });

        let err = answer
            .await
            .expect("the actor answers every Start")
            .expect_err("one app has no resolvable user");
        assert!(
            err.to_string().contains("a-bad"),
            "the failure must name the app whose user could not be resolved: {err}"
        );

        let identity = |name: &str| {
            actor
                .sheep
                .values()
                .find(|slot| slot.entry.spec.config().name == name)
                .map(|slot| slot.entry.credentials)
        };
        assert_eq!(
            identity("a-bad"),
            Some(SpawnIdentity::Unresolved),
            "an app with no resolvable user is registered `Errored` so it is \
             visible, and must hold NO identity: `Unresolved` is what stops a \
             later restart reusing one, and it is also the proof this app did \
             not inherit b-mine's"
        );
        assert_eq!(
            identity("b-mine"),
            Some(SpawnIdentity::Resolved(Some(Credentials {
                uid: own_uid.as_raw(),
                gid: None
            }))),
            "the app that asked for a user must run as that user"
        );
        assert_eq!(
            identity("c-plain"),
            Some(SpawnIdentity::Resolved(None)),
            "the app that asked for no user must be registered, and must run \
             as the daemon rather than as somebody else's uid. `Resolved(None)` \
             rather than a bare `None`: this app was ASKED, and answered \
             nobody, which is not the same fact as never having been asked"
        );
    }

    /// The policy is a parameter: carrying on past a failure is right for a
    /// muster restore and wrong for an operator's `shep start`.
    ///
    /// `failing_to_spawn` on the first app, so the assertion reads the second
    /// app's absence rather than its failure.
    #[tokio::test(start_paused = true)]
    async fn an_all_or_nothing_start_stops_at_the_first_failed_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let handle = spawn_supervisor(
            // A script for the second app, which must not get as far as using
            // it: an exhausted pool would fail it for a reason of its own.
            ScriptedRunner::new(vec![ProcScript::never_exits()]).failing_to_spawn(&["first"]),
            test_paths(&dir),
            events,
        );

        let err = handle
            .start(vec![
                normalize(AppConfig::minimal("first", "./first")).unwrap(),
                normalize(AppConfig::minimal("second", "./second")).unwrap(),
            ])
            .await
            .expect_err("the first app cannot spawn");

        assert!(matches!(err, SupervisorError::SpawnFailed(_)), "{err:?}");
        let listed = handle.list().await;
        assert_eq!(
            listed
                .iter()
                .map(|i| (i.name.as_str(), i.status))
                .collect::<Vec<_>>(),
            vec![("first", ProcStatus::Errored)],
            "an operator's `shep start` must not go on past a failure: the \
             second app is never reached: {listed:?}"
        );
        handle.shutdown().await;
    }

    /// `do_start_dog` shares `do_start` with `Request::Start`, so a
    /// pre-registration check written for a Flockfile batch can reach dogs and
    /// leave no trace of the wreck. [`Actor::spawn_fresh`]'s failure arm
    /// registers the row and `shep dogs` renders it.
    ///
    /// `refusing` is load-bearing: without it `ScriptedRunner`'s `preflight`
    /// answers `Unknown` and the assertions hold whichever policy was passed.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_cannot_spawn_is_registered_errored_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        // Empty scripts, so the spawn fails on its own.
        let runner = ScriptedRunner::new(Vec::new()).refusing(&["bark"]);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        let err = handle
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .expect_err("nothing can spawn against an empty script pool");

        assert!(
            matches!(err, SupervisorError::SpawnFailed(_)),
            "a dog must reach its spawn and fail there, not be refused \
             before it registers: {err:?}"
        );
        let listed = handle.list().await;
        assert_eq!(
            listed
                .iter()
                .map(|i| (i.name.as_str(), i.status, i.dog.clone()))
                .collect::<Vec<_>>(),
            vec![("bark", ProcStatus::Errored, Some(DogSource::BuiltIn))],
            "the dogs table must still show the dog, and show it broken"
        );
        handle.shutdown().await;
    }

    /// `shep reload bark` names the dog exactly, so it reaches it where a
    /// wildcard would not, and an unmarked replacement turns the dog into a
    /// sheep while the swap reports success either way.
    ///
    /// Three scripts, of which a correct run uses two: the third lets a broken
    /// run's extra spawn land as a live entry.
    #[tokio::test(start_paused = true)]
    async fn a_reloaded_dog_is_still_a_dog() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = crate::bus::test_bus(256);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits(); 3]);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        let dog = handle
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();
        handle
            .reload(ProcessSelector::Name("bark".to_string()))
            .await
            .expect("a reload that names the dog is accepted");

        // The replacement is the next id the actor hands out; `Reloaded` on it
        // means the drainee is already deregistered.
        let replacement = dog.id + 1;
        expect_event(&mut rx, replacement, ProcessEventKind::Reloaded).await;
        let listed = handle.list().await;

        assert_eq!(
            listed.len(),
            1,
            "the swap is over, not in flight: {listed:?}"
        );
        assert_eq!(
            dog_row(&listed, replacement).dog,
            Some(DogSource::BuiltIn),
            "the half that arrived is the same dog the half that left was"
        );
    }

    /// The rule `Start` follows: the shutdown aggregation's `online` snapshot
    /// was fixed when it ran, so a child registered after it is one nothing will
    /// kill. The runner carries a script so a broken run's spawn succeeds.
    #[tokio::test(start_paused = true)]
    async fn a_dog_is_refused_once_a_shutdown_has_begun() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);
        actor.shutting_down = true;

        let (reply, rx) = oneshot::channel();
        actor.handle_command(Command::StartDog {
            app: Box::new(dog_app("bark")),
            source: DogSource::BuiltIn,
            reply,
        });

        assert_eq!(rx.await, Ok(Err(SupervisorError::EngineStopped)));
        assert_eq!(actor.sheep.len(), 1, "nothing new was registered");
    }

    /// `shep enable` runs against a daemon that may already have the dog from
    /// `enabled_dogs` at boot, and a second live process under one name gives
    /// two metrics listeners on one port and two copies of every bark.
    ///
    /// Two scripts, of which a correct run uses one: the second lets a
    /// non-idempotent `start_dog` show up as the extra entry it is.
    #[tokio::test(start_paused = true)]
    async fn enabling_a_dog_twice_starts_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits(); 2]);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        let first = handle
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();
        let second = handle
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.pid, first.pid, "the same process, not a fresh one");
        let listed = handle.list().await;
        assert_eq!(listed.iter().filter(|i| i.name == "bark").count(), 1);
    }

    /// A [`ScriptedRunner`] that refuses one spawn by ordinal and forwards
    /// every other one.
    ///
    /// `ScriptedRunner` can only fail by running out of scripts, which fails
    /// every spawn from then on. Refusing exactly one tells a correct reload,
    /// which stops there, from a broken one, which spawns a second replacement.
    struct RefusesOneSpawn {
        inner: ScriptedRunner,
        /// Which spawn, counting from the engine's first, is refused.
        refuse: usize,
        /// Every spawn attempted, refused ones included, which
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

    // `Drainee` names the replacement and belongs on the instance being
    // replaced; `Replacement` belongs on the replacement and names nothing;
    // `Stopping` is the drainee's status alone. `ProcessEntry::reload` never
    // reaches the wire, so this is the only tier that can read it back.
    #[tokio::test(start_paused = true)]
    async fn a_swap_puts_each_half_of_a_reload_on_the_entry_that_owns_it() {
        let dir = tempfile::tempdir().unwrap();
        // One script for the one spawn a correct `SpawnNew` performs.
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);

        let new_id = actor
            .spawn_replacement(0, ReloadMode::Overlap)
            .expect("the fixture's one script covers this spawn");

        assert_ne!(new_id, 0, "a replacement never reuses the drainee's id");
        let drainee = &actor.sheep[&0].entry;
        let replacement = &actor.sheep[&new_id].entry;

        assert_eq!(drainee.status, ProcStatus::Stopping);
        assert_eq!(
            drainee.reload,
            ReloadState::Drainee {
                new_id: Some(new_id)
            }
        );
        assert_ne!(
            replacement.status,
            ProcStatus::Stopping,
            "`Stopping` belongs to the instance going away, not the one arriving"
        );
        assert_eq!(replacement.status, ProcStatus::Starting);
        assert_eq!(replacement.reload, ReloadState::Replacement);
        assert_eq!(
            replacement.instance, drainee.instance,
            "a replacement takes the drainee's instance slot, or an app deriving \
             its port from it binds a different one and nothing overlaps"
        );
    }

    // A reload's replacement would be a child outside the shutdown
    // aggregation's `online` snapshot, and so orphaned when the actor exits.
    // Two guards stand between a shutdown and that child: the reply witnesses
    // `Command::Reload`'s, and the `advance_reload` call below reaches the other.
    #[tokio::test(start_paused = true)]
    async fn a_reload_is_refused_once_a_shutdown_has_begun() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);
        actor.shutting_down = true;

        let (reply, rx) = oneshot::channel();
        actor.handle_command(Command::Reload {
            selector: ProcessSelector::All,
            reply,
        });
        assert_eq!(rx.await, Ok(Err(SupervisorError::EngineStopped)));

        // `advance_reload` is the one door into `SpawnNew`.
        actor.advance_reload("web", VecDeque::from([0]));

        assert_eq!(actor.sheep.len(), 1, "nothing new was registered");
        assert_eq!(actor.sheep[&0].entry.status, ProcStatus::Online);
        assert_eq!(actor.sheep[&0].entry.reload, ReloadState::None);
        assert!(actor.reloads.is_empty(), "no job was started");
    }

    // A replacement takes a new id in the drainee's instance slot, of which the
    // log paths are the observable half. The drainee's registration goes with
    // it: nothing else removes it, so the flock would grow a dead row per
    // instance per reload.
    #[tokio::test(start_paused = true)]
    async fn a_reload_gives_the_replacement_a_new_id_in_the_drainees_slot() {
        let dir = tempfile::tempdir().unwrap();
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

    // Marking the drainee `Stopping` only when its drain starts leaves the app
    // two entries that `snapshot.rs`'s `is_running` counts for the whole
    // `AwaitReady` window, so a muster roll written during a reload records an
    // instance count the flock does not have.
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

    // `await_ready`'s `Heuristic` arm returns `Ready` at the deadline, since for
    // an app configuring neither `wait_ready` nor `readiness_probe` the elapse
    // is the signal. An implementation keyed on the deadline instead abandons
    // every reload of every such app.
    #[tokio::test(start_paused = true)]
    async fn a_reload_of_an_app_with_no_readiness_signal_completes_at_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        // Not the 3000ms default: a distinctive value says which wait ran.
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

    // A swap that ignored the shepherd channel's ready signal would sit out the
    // whole `listen_timeout` before committing.
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

    // The ordinary readiness rule takes a slow app online rather than looping
    // it, and a reload must not inherit that: committing to an instance that has
    // not proved it can serve means killing the one that can. The abandoned
    // replacement got far enough to fork lambs, so it goes through the ladder.
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

    // The drain runs under `graceful_timeout`, not `kill_timeout`: 8000ms
    // against 1600ms by default.
    #[tokio::test(start_paused = true)]
    async fn a_reload_drains_the_old_instance_under_graceful_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            // The drainee ignores its stop signal: the elapsed time is the cap.
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

    // Starting every swap at once would leave a clustered app entirely
    // `Stopping` for the whole window, with nothing holding the old listeners.
    #[tokio::test(start_paused = true)]
    async fn a_reload_replaces_a_clustered_apps_instances_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        // Four spawns: two originals and two replacements. Starting both swaps
        // together performs the same four, so only the timing tells them apart.
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

    // Failure of the new instance aborts the rest and keeps the old instances
    // running. The drainee must not be left `Stopping` after the spawn that was
    // to replace it never happened, which takes it out of the muster roll and
    // out of reach of a liveness restart.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_that_cannot_be_spawned_leaves_every_instance_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        // Four scripts, of which a correct run uses two. The fixture refuses the
        // third; the fourth is for the spawn a reload carrying on would take.
        let attempts = Arc::new(AtomicUsize::new(0));
        let runner = RefusesOneSpawn {
            inner: ScriptedRunner::new(vec![ProcScript::never_exits(); 4]),
            refuse: 2,
            attempts: Arc::clone(&attempts),
        };
        let (events, mut rx) = crate::bus::test_bus(256);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted before anything is spawned");

        // A reload that carried on would spawn instance 1's replacement under
        // id 3.
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

    // The app is clustered so an acceptance has somewhere to go: with instance 0
    // mid-swap, a second reload finds instance 1 still `Online` and
    // `advance_reload`'s insert overwrites the first job, whose drainee is never
    // reaped. A single-instance fixture shows none of that.
    #[tokio::test(start_paused = true)]
    async fn a_second_reload_of_an_app_already_reloading_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        // Four scripts, of which a correct run uses three. The fourth lets a
        // wrongly-accepted second reload succeed into a live entry rather than
        // hide behind an exhausted pool.
        let (handle, runner, mut rx) = started(&dir, app, vec![ProcScript::never_exits(); 4]).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the first reload is accepted");
        expect_event(&mut rx, 2, ProcessEventKind::Start).await;

        let refused = handle.reload(ProcessSelector::All).await;

        assert_eq!(
            refused,
            Err(SupervisorError::ReloadInFlight("web".to_string()))
        );
        assert_eq!(
            runner.kill_counts().len(),
            3,
            "no second replacement was spawned"
        );
        assert_eq!(
            handle.list().await.len(),
            3,
            "one drainee, its replacement, and the instance untouched so far"
        );
    }

    // Reported as a success, so one stopped sheep does not fail a reload of
    // the rest.
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

    // A `claim_manual` ladder leaves the status `Online`, so a swap started
    // against one is abandoned by that ladder's exit, which kills the
    // replacement and warns of an operator command nobody issued.
    #[tokio::test(start_paused = true)]
    async fn a_reload_skips_an_instance_whose_kill_ladder_is_already_running() {
        let dir = tempfile::tempdir().unwrap();
        // The original defies its signal, so the breach's ladder is still
        // running when the reload lands.
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::ignores_signals(), ProcScript::never_exits()],
        )
        .await;
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        handle.extra_restart(0, pid, None, None).await;

        let reloaded = handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("a reload with nothing left to replace still succeeds");

        assert_eq!(reloaded.len(), 1, "the reload still answers for the match");
        assert_eq!(
            handle.list().await.len(),
            1,
            "no replacement was registered against an instance on its way out"
        );
        assert_eq!(runner.kill_counts().len(), 1, "nothing was spawned");

        expect_event(&mut rx, 0, ProcessEventKind::Restart).await;
        let after = handle.list().await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, 0);
        assert_eq!(
            runner.kill_counts().len(),
            2,
            "the original and its respawn"
        );
    }

    // Pins the outcome rather than either mechanism behind it:
    // `handle_extra_restart`'s guard 4 rejects a status that is not `Online`,
    // and `begin_manual` drops an automatic restart against either half of an
    // uncommitted swap.
    #[tokio::test(start_paused = true)]
    async fn a_report_raised_against_a_drainee_never_takes_it_off_the_reload() {
        let dir = tempfile::tempdir().unwrap();
        // Three scripts for two spawns: the third lets a wrongly restarted
        // drainee succeed, so the count below reads the extra spawn.
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(); 3],
        )
        .await;
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;
        handle.extra_restart(0, pid, None, None).await;

        // `Restart`, not `Delete`: the bug respawns the drainee into the slot
        // its replacement holds, and never emits a `Delete` for this id.
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Restart,
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

    // The end-to-end form of
    // `a_report_raised_against_a_drainee_never_takes_it_off_the_reload`: a real
    // `liveness_probe`, the real `OsProber`, the daemon's own extras reporter,
    // and a reload the supervisor performs.
    #[tokio::test(start_paused = true)]
    async fn a_drainee_whose_liveness_probe_fails_is_reaped_rather_than_restarted() {
        let dir = tempfile::tempdir().unwrap();
        // Reserve a port and release it: nothing ever listens there, so every
        // probe below fails.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);

        let mut app = AppConfig::minimal("web", "./srv");
        // Both halves go online by hand, so the liveness failure lands inside
        // `AwaitReady` rather than racing a deadline.
        app.wait_ready = true;
        // The window cannot elapse while this case waits out a probe interval.
        app.listen_timeout = UpDuration::from_millis(60_000);
        app.liveness_probe = Some(ProbeConfig {
            // The floor `spawn_liveness_task` honours anyway.
            interval: UpDuration::from_millis(1_000),
            timeout: UpDuration::from_millis(500),
            failure_threshold: 1,
            ..probe_config(ProbeKind::Tcp, &addr.to_string())
        });

        // Four scripts for three spawns: the fourth lets the respawn a broken
        // implementation makes land live rather than `Errored`.
        let (events, mut rx) = crate::bus::test_bus(256);
        let runner = Arc::new(ScriptedRunner::new(vec![ProcScript::never_exits(); 4]));
        let (breaches_tx, breaches_rx) = mpsc::channel(8);
        let (liveness_tx, liveness_rx) = mpsc::channel(8);
        let handle =
            SupervisorBuilder::new(SharedRunner(Arc::clone(&runner)), test_paths(&dir), events)
                .extras(Extras {
                    // The liveness half of `reports` is why the extras are
                    // wired here; no `cron_restart` and no `max_memory`.
                    clock: Arc::new(SystemClock),
                    enforcer: Arc::new(RecordingEnforcer::default()),
                    max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
                    reports: ExtrasReports {
                        breaches: breaches_tx,
                        liveness: liveness_tx,
                    },
                    stats: idle_stats(),
                })
                .spawn();
        // The reporter is the step that turns a `LivenessFailure` into the
        // `extra_restart` the two rejections rule on.
        let _reporter = spawn_extras_reporter(breaches_rx, liveness_rx, handle.clone());
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;

        // Five probe intervals, and far short of the 60s readiness deadline.
        // `Restart`, not `Delete`: the bug respawns rather than deregisters.
        assert_no_event_within(
            &mut rx,
            0,
            ProcessEventKind::Restart,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopping);
        assert_eq!(
            runner.kill_counts().len(),
            2,
            "the failing probe never caused a spawn"
        );

        handle.tx.send(Msg::Ready { id: 1 }).await.unwrap();
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        let after = handle.list().await;
        assert_eq!(after.len(), 1, "the drainee left no registration behind");
        assert_eq!(after[0].id, 1);

        // The control: the replacement's own probe failure does restart it, so
        // the chain this case rests on is delivering.
        expect_event(&mut rx, 1, ProcessEventKind::Restart).await;
    }

    // An `autorestart` app's drainee handed to `decide_on_exit` would be
    // respawned into the instance slot its replacement already holds.
    #[tokio::test(start_paused = true)]
    async fn a_drainee_that_exits_on_its_own_is_never_restarted() {
        let dir = tempfile::tempdir().unwrap();
        // The original ends 1000ms in, a stable run the policy would restart.
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

    // A `stop` leaves a sheep registered and `Stopped`; deregistering both
    // entries would take the app out of `shep flock`. The replacement defies
    // its signal, putting its exit a whole `kill_timeout` behind the drainee's,
    // which is the order that reaches the branch.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_mid_reload_leaves_the_app_stopped_and_registered() {
        let dir = tempfile::tempdir().unwrap();
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

    // Killing the replacement anyway empties the slot outright, with no entry
    // and no restart. It stays `Starting`, never having been signalled; the
    // abandonment on the bus is what says the reload gave up.
    #[tokio::test(start_paused = true)]
    async fn a_replacement_is_kept_but_not_online_when_the_deadline_elapses_with_no_drainee() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals the replacement
        // The original ends 1000ms in, before the replacement's 3000ms deadline.
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
        expect_event(&mut rx, 1, ProcessEventKind::ReloadAbandoned).await;

        let after = handle.list().await;
        assert_eq!(after.len(), 1, "the app still has its instance");
        assert_eq!(after[0].id, 1);
        assert_eq!(
            after[0].status,
            ProcStatus::Starting,
            "a replacement that never answered is kept, and never called online"
        );
        assert_eq!(runner.kill_counts(), vec![0, 0], "neither was SIGKILLed");
    }

    // The restore is for a drainee that goes back to serving. One an operator's
    // `stop` already claimed hands `shep flock` a live pid for a process on its
    // way out, and re-opens `handle_extra_restart`'s `Online` guard for the
    // length of the ladder.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_reload_never_reports_a_dying_drainee_as_online() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        // Nobody signals the replacement, so the swap is still `AwaitReady`
        // when the stop lands.
        app.wait_ready = true;
        let (handle, _runner, mut rx) = started(
            &dir,
            app,
            vec![ProcScript::ignores_signals(), ProcScript::never_exits()],
        )
        .await;
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;

        // Not awaited: the drainee defies its signal, so the stop's own reply
        // is a whole `kill_timeout` away.
        let stopper = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.stop(ProcessSelector::Name("web".to_string())).await })
        };
        expect_event(&mut rx, 1, ProcessEventKind::Delete).await;

        let mid = handle.list().await;
        assert_eq!(mid.len(), 1, "only the abandoned replacement has gone");
        assert_eq!(mid[0].id, 0);
        assert_eq!(
            mid[0].status,
            ProcStatus::Stopping,
            "a drainee an operator already claimed is not back to serving"
        );

        let stopped = stopper.await.unwrap().expect("the stop is answered");
        assert_eq!(stopped.len(), 2, "a stop answers for every id it matched");
        assert_eq!(handle.list().await[0].status, ProcStatus::Stopped);
    }

    // A cron occurrence and a watched file reach `begin_manual`, which reads no
    // status, so the `Stopping` transition does nothing for them.
    #[tokio::test(start_paused = true)]
    async fn an_automatic_restart_never_lands_on_either_half_of_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        // Signalled by hand, so the restart lands inside `AwaitReady`.
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

    // `reap_drainee` leaves the job at `DrainOld` with the drainee
    // deregistered, so the replacement's readiness result is the last event
    // that could end the job; clearing its `Replacement` marker cancels that
    // too, and nothing is left that can reach `finish_swap`.
    #[tokio::test(start_paused = true)]
    async fn a_reload_ends_when_its_replacement_goes_with_the_drainee_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals the replacement
        // The original ends 1000ms in, inside the replacement's 3000ms
        // readiness window, so the swap commits on its death.
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

        // An operator's `stop` before the replacement is ready, no crash needed.
        handle
            .stop(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the stop reaches the replacement");

        let seen = events_through(&mut rx, 1, ProcessEventKind::Stop).await;
        assert!(
            at(&seen, 1, ProcessEventKind::ReloadAbandoned) < at(&seen, 1, ProcessEventKind::Stop),
            "the reload gives up before the exit that ended it is reported, so a \
             subscriber reads the two in the order they happened: {seen:?}"
        );

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

    // The count is an operator's view of that instance's history, and
    // resetting it on every deploy makes the number useless.
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

    /// `spawn_replacement` reads the drainee's `last_exit` before the drainee
    /// exits again, so the replacement carries the manual restart's kill.
    #[tokio::test(start_paused = true)]
    async fn a_reload_carries_the_drainees_last_exit_to_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
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
        let restarted = handle.list().await;
        let last_exit = restarted[0].last_exit;
        assert!(
            last_exit.is_some(),
            "a manual restart is itself an exit, so this must not be None: {restarted:?}"
        );

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;

        let after = handle.list().await;
        assert_eq!(after[0].id, 1);
        assert_eq!(
            after[0].last_exit, last_exit,
            "a reload is not an exit -- the replacement must inherit the drainee's \
             last_exit rather than reset it to None"
        );
    }

    /// One process event, flattened to what a reload's bus claims are made of.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Seen {
        id: u32,
        kind: ProcessEventKind,
        status: ProcStatus,
        manually: bool,
    }

    /// Every process event in arrival order, up to and including `kind` for
    /// `id`.
    ///
    /// Bounded by [`SWAP_WINDOW`]. A `Lagged` is fatal: a hole in the stream is
    /// a hole in every claim read off it.
    async fn events_through(
        rx: &mut tokio::sync::broadcast::Receiver<SharedEvent>,
        id: u32,
        kind: ProcessEventKind,
    ) -> Vec<Seen> {
        let collect = async {
            let mut seen = Vec::new();
            loop {
                match rx.recv().await.map(|event| event.to_event()) {
                    Ok(BusEvent::Process {
                        event,
                        info,
                        manually,
                        ..
                    }) => {
                        seen.push(Seen {
                            id: info.id,
                            kind: event,
                            status: info.status,
                            manually,
                        });
                        if info.id == id && event == kind {
                            return seen;
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        panic!("the event stream lagged by {n}; no ordering claim survives that")
                    }
                    Err(e) => panic!("event stream closed before {kind:?} for id {id}: {e}"),
                }
            }
        };
        tokio::time::timeout(SWAP_WINDOW, collect)
            .await
            .unwrap_or_else(|_| panic!("no {kind:?} for id {id} within {SWAP_WINDOW:?}"))
    }

    /// Where `seen` first records `kind` for `id`, or a panic naming the run.
    fn at(seen: &[Seen], id: u32, kind: ProcessEventKind) -> usize {
        seen.iter()
            .position(|e| e.id == id && e.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} for id {id} in {seen:?}"))
    }

    // The reply is an acceptance, so these frames are the whole of what a
    // client learns. `Reload` names the drainee before the replacement's
    // `Start` and carries `Stopping`; `Reloaded` lands only once the drainee's
    // `Delete` has.
    #[tokio::test(start_paused = true)]
    async fn a_completed_swap_reports_itself_on_the_bus() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, _runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits(); 3],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");

        let seen = events_through(&mut rx, 1, ProcessEventKind::Reloaded).await;

        assert!(
            at(&seen, 0, ProcessEventKind::Reload) < at(&seen, 1, ProcessEventKind::Start),
            "the instance being replaced is named before its replacement starts: {seen:?}"
        );
        assert_eq!(
            seen[at(&seen, 0, ProcessEventKind::Reload)],
            Seen {
                id: 0,
                kind: ProcessEventKind::Reload,
                status: ProcStatus::Stopping,
                manually: true,
            }
        );
        assert!(
            at(&seen, 0, ProcessEventKind::Delete) < at(&seen, 1, ProcessEventKind::Reloaded),
            "a swap is not over until the instance it replaced is gone: {seen:?}"
        );
        assert_eq!(
            seen[at(&seen, 1, ProcessEventKind::Reloaded)],
            Seen {
                id: 1,
                kind: ProcessEventKind::Reloaded,
                status: ProcStatus::Online,
                manually: true,
            }
        );
    }

    // A replacement that goes down inside the drain window keeps its row in the
    // map, so a registration test passes and `Reloaded` names a process that is
    // down. The drainee ignores its stop signal, so its drain runs the full
    // 8000ms `graceful_timeout`; the replacement exits 5000ms in.
    #[tokio::test(start_paused = true)]
    async fn a_swap_is_not_announced_for_a_replacement_that_is_no_longer_serving() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.autorestart = false; // the replacement's exit is terminal, and registered
        let (handle, _runner, mut rx) = started(
            &dir,
            app,
            vec![
                ProcScript::ignores_signals(),
                ProcScript::stable_then_exit(5_000, 1),
            ],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");

        let seen = events_through(&mut rx, 1, ProcessEventKind::ReloadAbandoned).await;
        assert_eq!(
            seen[at(&seen, 1, ProcessEventKind::ReloadAbandoned)],
            Seen {
                id: 1,
                kind: ProcessEventKind::ReloadAbandoned,
                status: ProcStatus::Stopped,
                manually: true,
            },
            "a replacement that died inside the drain window is what the \
             abandonment names, carrying the status it actually reached"
        );
        assert!(
            !seen.iter().any(|e| e.kind == ProcessEventKind::Reloaded),
            "no swap succeeded, so nothing may say one did: {seen:?}"
        );
    }

    // Every transition out of a `ReloadJob` is driven by a `Msg::Exited` or a
    // `Msg::ReadyResult`, and neither is guaranteed: `kill_process`'s
    // post-`SIGKILL` `wait` is unbounded. `never_reports_its_exit` delivers and
    // counts the `SIGKILL` and withholds only the exit.
    #[tokio::test(start_paused = true)]
    async fn a_swap_whose_drainee_never_reports_its_exit_gives_up_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![
                ProcScript::never_reports_its_exit(),
                ProcScript::never_exits(),
                ProcScript::never_exits(),
            ],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");

        // The swap commits, the replacement is serving, and then stalls: the
        // drain's ladder runs to its `SIGKILL` and no exit ever follows it.
        let seen = events_through(&mut rx, 1, ProcessEventKind::ReloadAbandoned).await;
        assert_eq!(
            seen[at(&seen, 1, ProcessEventKind::ReloadAbandoned)],
            Seen {
                id: 1,
                kind: ProcessEventKind::ReloadAbandoned,
                status: ProcStatus::Online,
                manually: true,
            },
            "the replacement took the slot over and is what is left holding it"
        );
        assert_eq!(
            runner.kill_counts(),
            vec![1, 0],
            "the drain did reach `SIGKILL`; what never came back was the exit"
        );

        // The point of giving up: the verb works again without a daemon restart.
        let again = handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .map(|infos| infos.iter().map(|info| info.id).collect::<Vec<_>>());
        assert_eq!(
            again,
            Ok(vec![0, 1]),
            "a wedged instance must not refuse the app's next reload"
        );
    }

    // Staleness is read off the swap's `new_id`, since ids are never reused. A
    // clustered app puts two swaps in one window: each drainee ignores its stop
    // signal, so a swap runs its full 3000 + 8000ms, the first ends at 11000
    // and its deadline comes home at 16000, by when the job is the second swap.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_from_a_finished_swap_never_ends_the_one_that_followed_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        let (handle, _runner, mut rx) = started(
            &dir,
            app,
            vec![
                ProcScript::ignores_signals(),
                ProcScript::ignores_signals(),
                ProcScript::never_exits(),
                ProcScript::never_exits(),
            ],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");

        expect_event(&mut rx, 2, ProcessEventKind::Reloaded).await;
        expect_event(&mut rx, 3, ProcessEventKind::Reloaded).await;

        let after = handle.list().await;
        assert_eq!(
            after.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![2, 3],
            "both instances were replaced: {after:?}"
        );
        assert!(after.iter().all(|info| info.status == ProcStatus::Online));
    }

    // Taking the committed ending instead drops the job and leaves the instance
    // being replaced `Stopping` under a drain nothing started. Driven directly:
    // every readiness task carries its own `listen_timeout` and always sends.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_before_the_commit_puts_the_instance_being_replaced_back() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);
        // A live control sender says this instance's task is still there.
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        actor.sheep.get_mut(&0).expect("the fixture's sheep").ctl = Some(ctl_tx);

        let new_id = actor
            .spawn_replacement(0, ReloadMode::Overlap)
            .expect("the fixture's one script covers this spawn");
        actor.reloads.insert(
            "web".to_string(),
            ReloadJob {
                queue: VecDeque::new(),
                mode: ReloadMode::Overlap,
                deadline: 0,
                swap: ReloadSwap {
                    old_id: 0,
                    new_id: Some(new_id),
                    phase: ReloadPhase::AwaitReady,
                },
            },
        );

        actor.handle_reload_deadline("web", actor.reloads["web"].deadline);

        assert!(
            actor.reloads.is_empty(),
            "the job is gone, so the app is reloadable again"
        );
        let drainee = &actor.sheep[&0];
        assert_eq!(
            drainee.entry.status,
            ProcStatus::Online,
            "nothing was ever killed, so the instance being replaced goes back to serving"
        );
        assert_eq!(drainee.entry.reload, ReloadState::None);
        assert_eq!(
            actor.sheep[&new_id].manual.map(|pending| pending.kind),
            Some(ManualKind::Delete),
            "the replacement that never proved itself is taken back down"
        );
    }

    // The reply was an acceptance, so a subscriber that hears a `Reload` and
    // never hears again cannot tell a reload still running from one that gave
    // up. `wait_ready` with nothing signalling the replacement is the
    // abandonment reached here.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_reload_says_so_on_the_bus() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true; // nobody ever signals the replacement
        let (handle, _runner, mut rx) =
            started(&dir, app, vec![ProcScript::never_exits(); 2]).await;
        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");

        let seen = events_through(&mut rx, 0, ProcessEventKind::ReloadAbandoned).await;
        assert_eq!(
            seen[at(&seen, 0, ProcessEventKind::ReloadAbandoned)],
            Seen {
                id: 0,
                kind: ProcessEventKind::ReloadAbandoned,
                status: ProcStatus::Online,
                manually: true,
            },
            "the abandoned reload's own instance is still the one serving"
        );
    }

    // No replacement is registered, so there is no `Start` and no `Delete`
    // unless `advance_reload`'s own failure arm says so. The exhausted pool is
    // the injected failure.
    #[tokio::test(start_paused = true)]
    async fn a_reload_whose_replacement_cannot_spawn_says_so_on_the_bus() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, _runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits()],
        )
        .await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted before anything is spawned");

        let seen = events_through(&mut rx, 0, ProcessEventKind::ReloadAbandoned).await;
        assert_eq!(
            seen[at(&seen, 0, ProcessEventKind::ReloadAbandoned)],
            Seen {
                id: 0,
                kind: ProcessEventKind::ReloadAbandoned,
                status: ProcStatus::Online,
                manually: true,
            },
            "a failed spawn leaves the instance it was replacing serving"
        );
        let after = handle.list().await;
        assert_eq!(after.len(), 1, "no replacement was registered: {after:?}");
        assert_eq!(after[0].id, 0);
    }

    /// Fails if `run_sheep` lets go of `ProcIo::log_ctl` while its sheep is
    /// still running. The real runner's log pump ends with that sender, and the
    /// read ends of the child's stdout and stderr close with the pump. Reads
    /// the fake's control task, which ends exactly when a real pump would.
    #[tokio::test(start_paused = true)]
    async fn a_live_sheep_keeps_holding_its_log_control_sender() {
        // One script, and it must not exit: an exiting proc closes its own
        // control channel, which is indistinguishable from the dropped sender.
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (proc, io) = runner.spawn(&log_ctl_spec()).unwrap();
        assert!(runner.log_ctl_live(0), "sanity: the fake starts it live");

        let (events, _rx) = crate::bus::test_bus(64);
        let (_ctl_tx, ctl_rx) = mpsc::channel(8);
        let (_signal_tx, signal_rx) = mpsc::channel(8);
        let (actor_tx, _actor_rx) = mpsc::channel(8);
        let app = normalize(AppConfig::minimal("svc", "./svc")).unwrap();
        tokio::spawn(run_sheep(
            7, proc, io, app, ctl_rx, signal_rx, events, actor_tx,
        ));

        // Yields rather than a clock advance: the failing path is ready work,
        // not a timer, and the proc under it must stay unexited.
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

    /// Fails if a sheep's log pump outlives its sheep task, in the one case the
    /// sheep's own exit cannot end it: a lamb inheriting the child's pipes
    /// holds both streams open, and [`SheepSlot::log_ctl`] keeps a control
    /// sender. What reaps the pump is its `logs` receiver going away.
    #[tokio::test(start_paused = true)]
    async fn a_pump_is_reaped_when_its_sheep_ends_even_with_a_lamb_on_the_pipe() {
        let (events, mut rx) = crate::bus::test_bus(64);
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
        // stop.
        await_event(&mut rx, 0, ProcessEventKind::Stop).await;
        assert_eq!(
            handle.list().await.len(),
            1,
            "sanity: the sheep is still registered, so its slot still holds a \
             clone of the control sender"
        );

        // A bounded poll: the pump ends on its own task's schedule.
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

    /// Fails if [`SheepSlot::to_child`] outlives the process it was cloned for:
    /// delete the clearing line in `handle_exited` and this reddens. The far
    /// end is a writer task parked on `recv()`, so every sender being dropped
    /// is the only thing that retires it. Asserts the task ending rather than
    /// the field being clear.
    #[tokio::test(start_paused = true)]
    async fn a_writer_task_is_reaped_when_its_sheep_exits() {
        let dir = tempfile::tempdir().unwrap();
        // `autorestart` off so the exit is terminal and the slot stays registered.
        let mut app = AppConfig::minimal("web", "./srv");
        app.autorestart = false;
        // The writer task exists only for a sheep with a channel.
        app.channel = true;
        // Exits on its own rather than under a kill: a `Kill` can put a
        // `Shutdown` on this very channel, and the read below wants its end.
        let (handle, runner, mut rx) =
            started(&dir, app, vec![ProcScript::stable_then_exit(1_000, 0)]).await;
        let mut io = runner.io_handles(0);
        assert_eq!(
            io.to_child_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty),
            "sanity: a running sheep's channel is open and quiet, so the \
             `None` below is a close and not a channel that never opened"
        );

        // `Stop`, not `Exit`: with `autorestart` off a clean exit is a clean
        // stop.
        await_event(&mut rx, 0, ProcessEventKind::Stop).await;
        assert_eq!(
            handle.list().await.len(),
            1,
            "sanity: the sheep is still registered, so nothing but the \
             clearing can have let go of the slot's clone"
        );

        // Bounded, so a leak fails the case instead of hanging it.
        let reaped = tokio::time::timeout(Duration::from_secs(5), io.to_child_rx.recv()).await;
        assert_eq!(
            reaped.ok(),
            Some(None),
            "the writer task outlived its sheep, parked on `recv()` and \
             holding the daemon's end of the shepherd channel"
        );
    }

    /// Fails if a spawn failure goes back to naming neither the sheep nor the
    /// path. An exact string: the message is the whole product here.
    ///
    /// The `cwd` half is left to the end-to-end tier, which has a real one.
    #[tokio::test(start_paused = true)]
    async fn a_failed_spawn_names_the_sheep_and_the_path_it_tried() {
        let dir = tempfile::tempdir().unwrap();
        // An empty script pool, so `ScriptedRunner` refuses the first spawn.
        let (mut actor, _mailbox) = actor_with_one_online_sheep(&dir, Vec::new());

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Start {
            apps: vec![normalize(AppConfig::minimal("api", "./api")).unwrap()],
            policy: BatchPolicy::AllOrNothing,
            reply,
        });

        let err = answer
            .await
            .expect("the actor answers every Start")
            .expect_err("an empty script pool cannot spawn");
        assert_eq!(
            err.to_string(),
            "spawn failed: api: process spawn failed: script exhausted; tried `./api`"
        );
    }

    /// Fails if `config_drift` stops naming an edited field, names a field
    /// nobody edited, reports an app the flock has never heard of, or lets a
    /// value out. The last assertion is the other half of the contract: asking
    /// must not apply the edit.
    #[tokio::test(start_paused = true)]
    async fn config_drift_names_an_edited_sheeps_fields_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (actor, _mailbox) = actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);

        // Two fields edited, so a comparator stopping at the first difference
        // fails here. `env` is one of them, reported by name only.
        let mut edited = AppConfig::minimal("web", "./srv");
        edited.cwd = Some("/srv/new".to_string());
        edited
            .env
            .insert("DATABASE_URL".to_string(), "postgres://hunter2".to_string());
        // A name the flock does not have: `start` will register it, so it must
        // not appear in the answer.
        let unknown = AppConfig::minimal("api", "./api");

        let drift = actor.config_drift(&[normalize(edited).unwrap(), normalize(unknown).unwrap()]);

        assert_eq!(
            drift,
            vec![SheepDrift::new(
                "web",
                vec!["cwd".to_string(), "env".to_string()]
            )]
        );
        assert!(
            !format!("{drift:?}").contains("hunter2"),
            "a value must never travel with the field name that changed: {drift:?}"
        );
        assert_eq!(
            actor.sheep[&0].entry.spec.config().cwd,
            None,
            "asking which fields differ must not apply them"
        );
    }

    /// Fails if any of the three spawns stops putting its [`ProcIo::to_child`]
    /// clone on the slot, the handle the actor reaches a live child through.
    /// Nothing yet reads the field, so a spawn that stopped taking its clone
    /// changes no behaviour any other case can see.
    #[tokio::test(start_paused = true)]
    async fn every_spawn_leaves_the_daemons_end_of_the_channel_on_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        // Three scripts, none of which exits: a proc that went on its own would
        // put a second, unasked-for `Msg::Exited` in play.
        let (mut actor, _mailbox) = actor_with_one_online_sheep(
            &dir,
            vec![
                ProcScript::never_exits(),
                ProcScript::never_exits(),
                ProcScript::never_exits(),
            ],
        );

        // `spawn_fresh`, the door every sheep arrives by, registering id 1
        // beside the fixture's own. `autorestart` off so the entry stays.
        let mut app = AppConfig::minimal("api", "./api");
        app.autorestart = false;
        let (reply, _answer) = oneshot::channel();
        actor.handle_command(Command::Start {
            apps: vec![normalize(app).unwrap()],
            policy: BatchPolicy::AllOrNothing,
            reply,
        });
        assert!(
            actor.sheep[&1].to_child.is_some(),
            "a fresh spawn's slot holds the daemon's end of its shepherd channel"
        );

        actor.handle_exited(
            1,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        );
        assert!(
            actor.sheep[&1].to_child.is_none(),
            "the clone goes with the process it was cloned for"
        );

        // `respawn`, the door a crash loop and a manual restart come back
        // through. A new process under the same id needs a new handle.
        actor.respawn(1, false);
        assert!(
            actor.sheep[&1].to_child.is_some(),
            "a respawn hands the slot the new process's channel, not the dead \
             one's"
        );

        // `spawn_replacement`, the reload's door: a new id in the drainee's
        // instance slot needs a handle of its own.
        actor.advance_reload("web", VecDeque::from([0]));
        let new_id = actor.reloads["web"]
            .swap
            .new_id
            .expect("an overlapping reload spawns its replacement at once");
        assert!(
            actor.sheep[&new_id].to_child.is_some(),
            "a reload's replacement holds the daemon's end of its own channel"
        );
    }

    // --- Custom actions: one action out, one answer back or none ---

    /// What an action gets to answer in. Virtual time, and long enough that no
    /// scheduling order inside a case reaches it by accident.
    const ACTION_TIMEOUT: Duration = Duration::from_secs(20);

    /// A window generous enough for any action wait to report home, so a case
    /// whose result never arrives fails instead of parking the suite.
    const ACTION_WINDOW: Duration = Duration::from_secs(120);

    /// A bare actor holding one sheep whose shepherd channel is open, plus the
    /// mailbox every spawned wait reports to and the child's end of the
    /// channel. Driven by hand so a case can put a reply on the channel at an
    /// exact point relative to a wait's deadline.
    fn actor_with_an_open_channel(
        dir: &tempfile::TempDir,
    ) -> (
        Actor<ScriptedRunner>,
        mpsc::Receiver<Msg>,
        mpsc::Receiver<ShepherdMessage>,
    ) {
        // No scripts: a spawn that should not have happened fails loudly.
        let (mut actor, mailbox) = actor_with_one_online_sheep(dir, vec![]);
        // Wide enough that no case can fill it, so a blocking `send` is a bug.
        let (to_child, child_rx) = mpsc::channel(16);
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registers id 0")
            .to_child = Some(to_child);
        (actor, mailbox, child_rx)
    }

    /// Puts one action on the fixture's sheep and hands back the receiver its
    /// answer will arrive on. Arms the wait directly rather than through
    /// `Command::Trigger`.
    fn trigger_action(
        actor: &mut Actor<ScriptedRunner>,
        action: &str,
    ) -> oneshot::Receiver<ActionOutcome> {
        let to_child = actor.sheep[&0]
            .to_child
            .clone()
            .expect("the fixture's sheep holds the daemon's end of a channel");
        actor.arm_action(0, to_child, action.to_string(), None, ACTION_TIMEOUT)
    }

    /// Drives the one message an action wait sends home and applies it,
    /// returning what it carried.
    async fn settle_action(
        actor: &mut Actor<ScriptedRunner>,
        mailbox: &mut mpsc::Receiver<Msg>,
    ) -> ActionOutcome {
        let msg = tokio::time::timeout(ACTION_WINDOW, mailbox.recv())
            .await
            .expect("an action wait reported nothing within the window")
            .expect("the actor's mailbox closed");
        match msg {
            Msg::ActionResult { id, stamp, outcome } => {
                actor.handle_action_result(id, stamp, outcome.clone());
                outcome
            }
            other => panic!("expected an action result, got {other:?}"),
        }
    }

    /// Reads the action the daemon put on the child's end of the channel,
    /// failing rather than hanging if nothing was sent.
    async fn sent_action(child_rx: &mut mpsc::Receiver<ShepherdMessage>) -> ShepherdMessage {
        tokio::time::timeout(ACTION_WINDOW, child_rx.recv())
            .await
            .expect("nothing reached the child's end of the channel")
            .expect("the child's end of the channel closed")
    }

    /// Fails if a reply stops reaching the wait that asked for it: the whole
    /// path, from `SupervisorHandle::trigger` through the actor, the writer's
    /// end of the channel, `run_sheep`'s relay of a `ChildMessage` and back.
    /// `params` are asserted on the wire; nothing in the daemon reads them.
    #[tokio::test(start_paused = true)]
    async fn a_triggered_action_answers_with_the_apps_reply() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.channel = true;
        let (handle, runner, _events) = started(&dir, app, vec![ProcScript::never_exits()]).await;
        let mut io = runner.io_handles(0);

        // Spawned rather than awaited: the reply below is what ends this wait.
        let triggered = tokio::spawn(async move {
            handle
                .trigger(
                    ProcessSelector::Name("web".to_string()),
                    "gc".to_string(),
                    Some("--full".to_string()),
                )
                .await
        });

        assert_eq!(
            sent_action(&mut io.to_child_rx).await,
            ShepherdMessage::Action {
                name: "gc".to_string(),
                params: Some("--full".to_string()),
                // `next_action_stamp` is read before it is incremented, so a
                // freshly-built actor's first dispatch is always stamp 0.
                id: 0,
            },
            "the action reaches the child's end of the channel as it was asked for"
        );

        io.from_child_tx
            .send(ChildMessage::ActionReply {
                action: "gc".to_string(),
                body: "swept 3".to_string(),
                // Echoing the dispatch's own stamp makes this a real round trip.
                id: Some(0),
            })
            .await
            .unwrap();

        assert_eq!(
            triggered.await.unwrap(),
            Ok(vec![ActionReply {
                id: 0,
                name: "web".to_string(),
                outcome: ActionOutcome::Replied {
                    body: "swept 3".to_string()
                },
            }]),
            "the app's reply body is what the caller is answered with"
        );
    }

    /// fails if a `ready` on fd 3 reaches only the readiness machinery and
    /// never the bus. Forwarding is a second thing the arm does.
    #[tokio::test(start_paused = true)]
    async fn a_ready_on_the_channel_reaches_both_the_bus_and_the_readiness_wait() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.channel = true;
        app.wait_ready = true;
        // `started` subscribes the bus receiver before the start.
        let (handle, runner, mut events) =
            started(&dir, app, vec![ProcScript::never_exits()]).await;
        let io = runner.io_handles(0);

        io.from_child_tx.send(ChildMessage::Ready).await.unwrap();

        // Bounded: a bus that never receives would park this case.
        let seen = tokio::time::timeout(ACTION_WINDOW, async {
            loop {
                if let BusEvent::Channel { id, message } = events.recv().await.unwrap().to_event() {
                    break (id, message);
                }
            }
        })
        .await
        .expect("no channel event within the window");

        assert_eq!(seen, (0, ChildMessage::Ready));

        // The readiness half still works: the sheep goes Online off this message.
        let listed = handle.list().await;
        assert_eq!(listed[0].status, ProcStatus::Online);
    }

    /// fails if a metric is still only a `tracing::debug!`, which no
    /// subscriber can read.
    #[tokio::test(start_paused = true)]
    async fn a_metric_on_the_channel_reaches_the_bus_with_its_name_and_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.channel = true;
        let (_handle, runner, mut events) =
            started(&dir, app, vec![ProcScript::never_exits()]).await;
        let io = runner.io_handles(0);

        io.from_child_tx
            .send(ChildMessage::Metric {
                name: "rps".to_string(),
                value: 42.0,
            })
            .await
            .unwrap();

        let seen = tokio::time::timeout(ACTION_WINDOW, async {
            loop {
                if let BusEvent::Channel { id, message } = events.recv().await.unwrap().to_event() {
                    break (id, message);
                }
            }
        })
        .await
        .expect("no channel event within the window");

        assert_eq!(
            seen,
            (
                0,
                ChildMessage::Metric {
                    name: "rps".to_string(),
                    value: 42.0,
                }
            )
        );
    }

    /// fails if an `action-reply` nobody is waiting for is dropped before the
    /// bus sees it. `handle_action_reply` finds no wait and discards it.
    #[tokio::test(start_paused = true)]
    async fn an_action_reply_no_trigger_is_waiting_for_still_reaches_the_bus() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.channel = true;
        let (_handle, runner, mut events) =
            started(&dir, app, vec![ProcScript::never_exits()]).await;
        let io = runner.io_handles(0);

        io.from_child_tx
            .send(ChildMessage::ActionReply {
                action: "gc".to_string(),
                body: "unprompted".to_string(),
                id: None,
            })
            .await
            .unwrap();

        let seen = tokio::time::timeout(ACTION_WINDOW, async {
            loop {
                if let BusEvent::Channel { message, .. } = events.recv().await.unwrap().to_event() {
                    break message;
                }
            }
        })
        .await
        .expect("no channel event within the window");

        let ChildMessage::ActionReply { body, .. } = seen else {
            panic!("expected an action reply, got {seen:?}");
        };
        assert_eq!(body, "unprompted");
    }

    /// Fails if a wait for an app that never answers does not end on its own.
    ///
    /// The action is read off the channel after the answer: a build that never
    /// sent it would time out too, and look identical without that read.
    #[tokio::test(start_paused = true)]
    async fn a_triggered_action_times_out_when_the_app_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("api", "./api");
        app.channel = true;
        let (handle, runner, _events) = started(&dir, app, vec![ProcScript::never_exits()]).await;
        let mut io = runner.io_handles(0);

        assert_eq!(
            handle
                .trigger(
                    ProcessSelector::Name("api".to_string()),
                    "stats".to_string(),
                    None,
                )
                .await,
            Ok(vec![ActionReply {
                id: 0,
                name: "api".to_string(),
                outcome: ActionOutcome::TimedOut,
            }]),
            "an app that says nothing is reported as saying nothing, not waited on forever"
        );
        assert_eq!(
            sent_action(&mut io.to_child_rx).await,
            ShepherdMessage::Action {
                name: "stats".to_string(),
                params: None,
                // The actor's first and only dispatch carries stamp 0.
                id: 0,
            },
            "the timeout is an app that did not answer, not an action that was never sent"
        );
    }

    /// T1 times out and leaves a `gc` debt, T2 is triggered and is live, and
    /// the app's next `gc` reply, carrying T2's stamp, must reach T2.
    #[test]
    fn a_stamped_reply_wakes_its_own_wait_even_with_a_debt_outstanding() {
        let mut waits = ActionWaits::default();

        // T1: armed, then resolved without its reply: the timeout path.
        let (t1_reply, _t1_out) = oneshot::channel();
        let (t1_waiter, _t1_body) = oneshot::channel();
        waits.arm(PendingAction {
            stamp: 1,
            action: "gc".to_string(),
            waiter: Some(t1_waiter),
            reply: t1_reply,
        });
        assert!(waits.resolve(1).is_some(), "T1 must have been live");

        // T2: armed and still live.
        let (t2_reply, _t2_out) = oneshot::channel();
        let (t2_waiter, t2_body) = oneshot::channel();
        waits.arm(PendingAction {
            stamp: 2,
            action: "gc".to_string(),
            waiter: Some(t2_waiter),
            reply: t2_reply,
        });

        let woken = waits
            .answer("gc", Some(2))
            .expect("a reply stamped with the live wait's own stamp must reach it");
        woken.send("collected".to_string()).unwrap();
        assert_eq!(t2_body.blocking_recv().unwrap(), "collected");
    }

    /// An app that does not echo the stamp must see the debt paid first and the
    /// live wait left alone.
    #[test]
    fn an_unstamped_reply_still_settles_the_oldest_debt_first() {
        let mut waits = ActionWaits::default();

        let (t1_reply, _t1_out) = oneshot::channel();
        let (t1_waiter, _t1_body) = oneshot::channel();
        waits.arm(PendingAction {
            stamp: 1,
            action: "gc".to_string(),
            waiter: Some(t1_waiter),
            reply: t1_reply,
        });
        waits.resolve(1);

        let (t2_reply, _t2_out) = oneshot::channel();
        let (t2_waiter, _t2_body) = oneshot::channel();
        waits.arm(PendingAction {
            stamp: 2,
            action: "gc".to_string(),
            waiter: Some(t2_waiter),
            reply: t2_reply,
        });

        assert!(
            waits.answer("gc", None).is_none(),
            "an unstamped reply pays the debt, exactly as it did before stamping"
        );
        assert!(
            waits.answer("gc", None).is_some(),
            "and the next one reaches the live wait, exactly as it did before"
        );
    }

    /// The stamped path has to settle its own debt, not just skip the queue.
    #[test]
    fn a_stamped_reply_for_a_dead_wait_does_not_reach_a_live_one() {
        let mut waits = ActionWaits::default();

        let (t1_reply, _t1_out) = oneshot::channel();
        let (t1_waiter, _t1_body) = oneshot::channel();
        waits.arm(PendingAction {
            stamp: 1,
            action: "gc".to_string(),
            waiter: Some(t1_waiter),
            reply: t1_reply,
        });
        waits.resolve(1);

        let (t2_reply, _t2_out) = oneshot::channel();
        let (t2_waiter, _t2_body) = oneshot::channel();
        waits.arm(PendingAction {
            stamp: 2,
            action: "gc".to_string(),
            waiter: Some(t2_waiter),
            reply: t2_reply,
        });

        assert!(
            waits.answer("gc", Some(1)).is_none(),
            "T1's own late reply belongs to T1's debt, not to T2"
        );
        assert!(
            waits.answer("gc", Some(2)).is_some(),
            "and T2 is still waiting for its own"
        );
    }

    /// Fails if a reply owed by a wait that already timed out answers a later
    /// wait for the same action. The one failure that produces a wrong answer
    /// rather than an error: an app's reply names the action and nothing else.
    /// Delete the `abandoned` bookkeeping in `ActionWaits::resolve` and the
    /// second trigger is answered `Replied` with the first trigger's body.
    #[tokio::test(start_paused = true)]
    async fn a_reply_owed_by_a_timed_out_action_never_answers_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);

        let first = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::TimedOut
        );
        assert_eq!(first.await.unwrap(), ActionOutcome::TimedOut);

        let second = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;
        // The app finally answers the first `gc`, having no way to say so.
        actor.handle_action_reply(0, "gc", "swept 3".to_string(), None);
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::TimedOut,
            "a reply the first `gc` was owed was handed to the second one"
        );
        assert_eq!(second.await.unwrap(), ActionOutcome::TimedOut);

        // One reply per debt: two `gc` waits have given up and the reply above
        // settled the first.
        let third = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;
        actor.handle_action_reply(0, "gc", "swept 7".to_string(), None);
        actor.handle_action_reply(0, "gc", "swept 11".to_string(), None);
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::Replied {
                body: "swept 11".to_string()
            },
            "the debts outlived the replies that settled them"
        );
        assert_eq!(
            third.await.unwrap(),
            ActionOutcome::Replied {
                body: "swept 11".to_string()
            }
        );
    }

    /// Fails if a second reply to an already-answered action is kept for
    /// anything. An app is free to write two, and the second is neither an
    /// error nor a debt. Proved by a wait armed after it still timing out.
    #[tokio::test(start_paused = true)]
    async fn a_second_reply_to_an_answered_action_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);

        let answered = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;
        actor.handle_action_reply(0, "gc", "swept 3".to_string(), None);
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::Replied {
                body: "swept 3".to_string()
            }
        );
        assert_eq!(
            answered.await.unwrap(),
            ActionOutcome::Replied {
                body: "swept 3".to_string()
            }
        );

        actor.handle_action_reply(0, "gc", "spent".to_string(), None);

        let next = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::TimedOut,
            "a spare reply was kept and used to answer a wait that came after it"
        );
        assert_eq!(next.await.unwrap(), ActionOutcome::TimedOut);
    }

    /// Fails if two waits for the same action on one sheep are not answered in
    /// the order they were asked. Neither reply says which is which; order is a
    /// property of the channel rather than of anything the daemon records.
    #[tokio::test(start_paused = true)]
    async fn two_waits_for_one_action_are_answered_in_the_order_they_were_asked() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);

        let first = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;
        let second = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;

        actor.handle_action_reply(0, "gc", "swept 3".to_string(), None);
        actor.handle_action_reply(0, "gc", "swept 7".to_string(), None);
        settle_action(&mut actor, &mut mailbox).await;
        settle_action(&mut actor, &mut mailbox).await;

        assert_eq!(
            first.await.unwrap(),
            ActionOutcome::Replied {
                body: "swept 3".to_string()
            },
            "the earlier trigger was answered with the later reply"
        );
        assert_eq!(
            second.await.unwrap(),
            ActionOutcome::Replied {
                body: "swept 7".to_string()
            },
            "the second reply was dropped and its wait left waiting"
        );
    }

    /// Fails if the deadline stops covering the delivery of an action and
    /// covers only the reply to it. A child that has stopped reading fd 3 backs
    /// its socket up, and a send onto a full channel waits for room that is not
    /// coming. The channel here is built full and left unread.
    #[tokio::test(start_paused = true)]
    async fn an_action_that_cannot_even_be_delivered_still_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox) = actor_with_one_online_sheep(&dir, vec![]);

        // Held, never read: dropping it would make the send fail outright.
        let (to_child, _wedged) = mpsc::channel(1);
        to_child.try_send(ShepherdMessage::Shutdown).unwrap();
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registers id 0")
            .to_child = Some(to_child);

        let answer = trigger_action(&mut actor, "gc");
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::TimedOut,
            "an action that never got onto the channel left its wait parked"
        );
        assert_eq!(answer.await.unwrap(), ActionOutcome::TimedOut);
    }

    // --- Custom actions: one selector in, one row per matched sheep out ---

    /// Registers one more `Online` sheep under `name`, holding `to_child` as
    /// its shepherd-channel sender, and hands back its id. Direct because the
    /// fake runner wires a live channel for every spawn whatever `channel` says.
    fn register_sheep(
        actor: &mut Actor<ScriptedRunner>,
        dir: &tempfile::TempDir,
        name: &str,
        to_child: Option<mpsc::Sender<ShepherdMessage>>,
    ) -> u32 {
        let id = actor.next_id;
        actor.next_id += 1;
        let paths = test_paths(dir);
        let app = normalize(AppConfig::minimal(name, "./srv")).unwrap();
        actor.sheep.insert(
            id,
            SheepSlot {
                entry: armed_entry(id, 0, 2000 + id, app, &paths),
                ctl: None,
                log_ctl: None,
                to_child,
                signals: None,
                to_stdin: None,
                manual: None,
                pending_delete: false,
                epoch: 0,
                ready_tx: None,
                actions: ActionWaits::default(),
                ready_failed: false,
                restart_due: None,
            },
        );
        id
    }

    /// Puts one action on every sheep matching `selector` and hands back the
    /// receiver the whole answer will arrive on.
    fn trigger_flock(
        actor: &mut Actor<ScriptedRunner>,
        selector: ProcessSelector,
        action: &str,
    ) -> oneshot::Receiver<Result<Vec<ActionReply>, SupervisorError>> {
        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Trigger {
            selector,
            action: action.to_string(),
            params: None,
            reply,
        });
        answer
    }

    /// Reads one trigger's whole answer, failing rather than hanging if it
    /// never comes. A request that armed a wait nothing resolves never answers.
    async fn triggered(
        answer: oneshot::Receiver<Result<Vec<ActionReply>, SupervisorError>>,
    ) -> Result<Vec<ActionReply>, SupervisorError> {
        tokio::time::timeout(ACTION_WINDOW, answer)
            .await
            .expect("a trigger reported nothing within the window")
            .expect("the trigger's reply channel was dropped")
    }

    /// One expected row, spelled out at the call site.
    fn row(id: u32, name: &str, outcome: ActionOutcome) -> ActionReply {
        ActionReply {
            id,
            name: name.to_string(),
            outcome,
        }
    }

    /// Fails if a trigger answers before every sheep it matched has been heard
    /// from, drops any of them, or returns the rows in settle order.
    ///
    /// The names are chosen so no two candidate orders agree. Ids run 0, 1, 2
    /// in registration order; the rows come out as `[1, 0, 2]`, id order would
    /// be `[0, 1, 2]`, and name order `[0, 2, 1]`.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_answers_every_sheep_it_matched_before_it_answers_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);
        register_sheep(&mut actor, &dir, "zeus", None);
        let (silent_tx, mut silent_rx) = mpsc::channel(16);
        register_sheep(&mut actor, &dir, "worker", Some(silent_tx));

        let mut answer = trigger_flock(&mut actor, ProcessSelector::All, "gc");
        sent_action(&mut child_rx).await;
        sent_action(&mut silent_rx).await;

        actor.handle_action_reply(0, "gc", "swept 3".to_string(), None);
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::Replied {
                body: "swept 3".to_string()
            }
        );
        // Yielded first: `settle_action` returns without the collecting task
        // having been polled, so an unyielded `try_recv` reads `Empty` anyway.
        tokio::task::yield_now().await;
        assert!(
            matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "a trigger answered while a sheep it matched was still being waited on"
        );

        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::TimedOut
        );
        assert_eq!(
            triggered(answer).await,
            Ok(vec![
                row(
                    0,
                    "web",
                    ActionOutcome::Replied {
                        body: "swept 3".to_string()
                    }
                ),
                row(2, "worker", ActionOutcome::TimedOut),
                row(1, "zeus", ActionOutcome::NoChannel),
            ])
        );
    }

    /// Fails if a sheep with no live channel is waited out instead of refused
    /// on the spot, or if that refusal takes the rest of the selector's matches
    /// with it. Refusing takes no wait, and the mailbox carrying exactly one
    /// result is what says so.
    #[tokio::test(start_paused = true)]
    async fn a_sheep_with_no_channel_is_refused_in_its_own_row() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);
        register_sheep(&mut actor, &dir, "api", None);

        let answer = trigger_flock(&mut actor, ProcessSelector::All, "gc");
        sent_action(&mut child_rx).await;
        actor.handle_action_reply(0, "gc", "swept 3".to_string(), None);
        settle_action(&mut actor, &mut mailbox).await;

        assert_eq!(
            triggered(answer).await,
            Ok(vec![
                row(1, "api", ActionOutcome::NoChannel),
                row(
                    0,
                    "web",
                    ActionOutcome::Replied {
                        body: "swept 3".to_string()
                    }
                ),
            ])
        );
        assert!(
            matches!(mailbox.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "a sheep with no channel armed a wait anyway"
        );
    }

    /// Fails if "can this sheep be triggered" is answered off the presence of a
    /// sender rather than off whether anything is still receiving on it. An app
    /// configured without a channel has its receiving end dropped at spawn.
    #[tokio::test(start_paused = true)]
    async fn a_sheep_whose_channel_has_no_far_end_is_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox) = actor_with_one_online_sheep(&dir, vec![]);
        let (to_child, receiver) = mpsc::channel(16);
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registers id 0")
            .to_child = Some(to_child);
        drop(receiver);

        assert_eq!(
            triggered(trigger_flock(&mut actor, ProcessSelector::All, "gc")).await,
            Ok(vec![row(0, "web", ActionOutcome::NoChannel)])
        );
        assert!(
            matches!(mailbox.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "a sheep whose channel has no far end armed a wait anyway"
        );
    }

    /// Fails if a flock where nothing can be reached is answered as though the
    /// action had been delivered. It is a success, and the rows are what stop
    /// it being a silent one.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_no_sheep_can_take_is_a_success_that_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox) = actor_with_one_online_sheep(&dir, vec![]);
        register_sheep(&mut actor, &dir, "api", None);

        assert_eq!(
            triggered(trigger_flock(&mut actor, ProcessSelector::All, "gc")).await,
            Ok(vec![
                row(0, "web", ActionOutcome::NoChannel),
                row(1, "api", ActionOutcome::NoChannel),
            ])
        );
        assert!(
            matches!(mailbox.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "a flock with nothing to deliver to armed a wait anyway"
        );
    }

    /// Fails if a reload drainee is sent the action rather than skipped. Both
    /// halves answer to the app's name and the drainee still holds a live
    /// channel, so an operator asking `web` gets two rows for one instance.
    /// Built by hand: the skip is decided by the crate-internal
    /// `ProcessEntry::reload`.
    #[tokio::test(start_paused = true)]
    async fn a_reload_drainee_is_skipped_and_its_replacement_answers() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);
        let (replacement_tx, mut replacement_rx) = mpsc::channel(16);
        let new_id = register_sheep(&mut actor, &dir, "web", Some(replacement_tx));
        let drainee = actor.sheep.get_mut(&0).expect("the fixture registers id 0");
        drainee.entry.status = ProcStatus::Stopping;
        drainee.entry.reload = ReloadState::Drainee {
            new_id: Some(new_id),
        };
        actor
            .sheep
            .get_mut(&new_id)
            .expect("the replacement was just registered")
            .entry
            .reload = ReloadState::Replacement;

        let answer = trigger_flock(&mut actor, ProcessSelector::Name("web".to_string()), "gc");
        sent_action(&mut replacement_rx).await;
        actor.handle_action_reply(new_id, "gc", "swept 3".to_string(), None);
        settle_action(&mut actor, &mut mailbox).await;

        assert_eq!(
            triggered(answer).await,
            Ok(vec![
                row(0, "web", ActionOutcome::Skipped),
                row(
                    new_id,
                    "web",
                    ActionOutcome::Replied {
                        body: "swept 3".to_string()
                    }
                ),
            ])
        );
        assert!(
            matches!(child_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "an action was delivered to a process that is on its way out"
        );
    }

    /// Fails if a selector matching nothing is answered with rows rather than
    /// an error. Every [`ActionOutcome`] is a statement about a sheep.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_matching_no_sheep_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) = actor_with_one_online_sheep(&dir, vec![]);

        assert_eq!(
            triggered(trigger_flock(
                &mut actor,
                ProcessSelector::Name("ghost".to_string()),
                "gc"
            ))
            .await,
            Err(SupervisorError::NotFound)
        );
    }

    /// Fails if a wait armed against a process that then exits is left for its
    /// own deadline to end, or dropped without an answer. The debts go with the
    /// waits: a replacement under this id has written none of the replies the
    /// dead process owed.
    #[tokio::test(start_paused = true)]
    async fn a_sheep_exiting_answers_every_action_waiting_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);

        // One wait that will be left waiting, and one debt for a wait that
        // already gave up: the two halves the exit has to clear.
        let timed_out = trigger_action(&mut actor, "gc");
        sent_action(&mut child_rx).await;
        settle_action(&mut actor, &mut mailbox).await;
        assert_eq!(timed_out.await.unwrap(), ActionOutcome::TimedOut);

        let waiting = trigger_action(&mut actor, "stats");
        sent_action(&mut child_rx).await;

        actor.handle_exited(
            0,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        );
        // Bounded: nothing else will answer a wait the exit failed to.
        let answered = tokio::time::timeout(ACTION_WINDOW, waiting)
            .await
            .expect("a wait outlived the process it was waiting on")
            .unwrap();
        assert_eq!(
            answered,
            ActionOutcome::NoChannel,
            "a wait outlived the process it was waiting on"
        );
        assert!(
            actor.sheep[&0].actions.abandoned.is_empty(),
            "a debt owed by a process that has exited outlived it, and would \
             have swallowed a reply from whatever runs under this id next"
        );
    }

    /// A [`ScriptedRunner`] whose spawns hand out a log-control channel that
    /// accepts requests and never answers them.
    ///
    /// Each request is held rather than dropped, so the `oneshot` sender inside
    /// stays owed instead of resolving `Err`. The `watch` counts requests that
    /// reached a pump.
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
            // Replacing the sender drops the fake's own, ending the control
            // task it spawned. This runner exists so nothing answers.
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
    /// loop. An actor parked on one stops draining its mailbox, so its sheep
    /// tasks block sending into it, so nothing drains their `logs`.
    ///
    /// `list` is the probe: it is answered from the actor loop and nowhere
    /// else, and the request must reach the pump first for it to mean anything.
    #[tokio::test(start_paused = true)]
    async fn the_actor_keeps_answering_while_a_reopen_waits_on_a_silent_pump() {
        let (events, _rx) = crate::bus::test_bus(64);
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
    /// The counts are what make this more than a smoke test: three sheep and a
    /// selector naming two of them catches both too narrow and too wide.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_reaches_every_matched_sheep_and_no_others() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Three scripts for three instances: a fourth spawn would land that
        // sheep `Errored` with no pump, which reads like the skip under test.
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

    /// Fails if a respawn leaves [`SheepSlot::log_ctl`] pointing at the pump of
    /// the process it replaced: `slot.log_ctl = Some(log_ctl);` dropped from
    /// [`Actor::respawn`]. The send then fails, a failed send is the documented
    /// no-op success, and `shep reopen` exits 0 having reached nothing.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_after_a_restart_reaches_the_pump_the_restart_spawned() {
        let (events, _rx) = crate::bus::test_bus(64);
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
        // that never restarted would reach the first pump and prove nothing.
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
    /// `reopen` is the one selector verb with a default, so silence would look
    /// like a rotation that worked.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_matching_nothing_is_not_found() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![]);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        assert_eq!(
            handle.reopen(ProcessSelector::All).await,
            Err(SupervisorError::NotFound)
        );
    }

    /// Fails if a stopped sheep makes a reopen error out, or hang waiting for
    /// an acknowledgement that cannot come. The fake's control task ends with
    /// its proc, so this sheep's pump is gone by the time the reopen is issued.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_sheep_is_a_no_op_success() {
        let (events, _rx) = crate::bus::test_bus(64);
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

    /// Fails if [`reopen_logs`] waits on an acknowledgement nobody is left to
    /// send: the leg where the pump is gone and the send itself fails.
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

    /// Fails if [`reopen_logs`] keeps waiting on a dropped acknowledgement: the
    /// leg where the pump ends between accepting the request and answering it.
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
    /// [`a_pump_that_ends_mid_request_still_ends_the_reopen`]. A pump that
    /// ended mid-request owes no bytes, but the truncate still has to run.
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

    /// What [`FailingPumpRunner`]'s pump answers every reopen with. One owner
    /// for the string: the case below asserts the whole error it ends up in.
    const PUMP_REFUSAL: &str = "/gone/web-out.log: No such file or directory";

    /// The sheep [`FailingPumpRunner`] gives a failing pump to.
    const REFUSING_SHEEP: &str = "web";

    /// A [`ScriptedRunner`] whose spawn of [`REFUSING_SHEEP`] gets a pump that
    /// answers every reopen with a failure. Every other sheep keeps the
    /// scripted fake's own answering pump. By name rather than by spawn order,
    /// so one case can hold a failed reopen and a healthy sheep beside it.
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
            // Replacing the sender drops the fake's own, ending the control
            // task it spawned. This pump answers in its place.
            io.log_ctl = tx;
            tokio::spawn(async move {
                while let Some(ctl) = rx.recv().await {
                    // Both variants, so this pump keeps serving whichever
                    // arrives.
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
                        // This runner exists for the reopen and flush refusals.
                        #[cfg(unix)]
                        LogCtl::ReportFds { done } => {
                            let _ = done.send(CarriedFds::none());
                        }
                        // Nothing to start reading again: this runner reads no
                        // streams.
                        #[cfg(unix)]
                        LogCtl::Resume => {}
                    }
                }
            });
            Ok((proc, io))
        }
    }

    /// Fails if a pump that could not reopen its files is reported as a
    /// success. That sheep writes a stream nowhere while `shep reopen` exits 0.
    /// The healthy sheep is the second half: a failure must name its own sheep
    /// and must not stop the rest of the flock being reopened.
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_could_not_reopen_fails_the_request_and_names_its_sheep() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Two scripts for two instances: a third spawn would land that sheep
        // `Errored` with no pump at all.
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

    // --- Signal: `shep signal`, one selector in, one row per matched sheep
    // out ---

    /// fails if `signal` reaches the group instead of the process. A supervisor
    /// calling `signal` rather than `signal_process` would look correct in
    /// every other respect and deliver SIGHUP to every lamb.
    #[tokio::test(start_paused = true)]
    async fn a_signal_reaches_the_sheeps_own_process_and_not_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, runner, _events) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits()],
        )
        .await;

        let rows = handle
            .signal(ProcessSelector::Id(0), OperatorSignal::Hup)
            .await
            .unwrap();

        assert_eq!(
            rows,
            vec![SignalReply {
                id: 0,
                name: "web".to_string(),
                outcome: SignalOutcome::Delivered,
            }]
        );
        assert_eq!(runner.process_signals(0), vec![OperatorSignal::Hup]);
        assert!(
            runner.signals(0).is_empty(),
            "shep signal must not reach the process group"
        );
    }

    /// fails if a registered-but-dead sheep is reported as delivered.
    /// `Delivered` is the only outcome that claims the kernel took the signal.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_sheep_answers_not_running_rather_than_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, _runner, _events) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits()],
        )
        .await;
        handle.stop(ProcessSelector::Id(0)).await.unwrap();

        let rows = handle
            .signal(ProcessSelector::Id(0), OperatorSignal::Hup)
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, SignalOutcome::NotRunning);
    }

    /// fails if a selector matching nothing is answered with an empty success.
    /// It is `NotFound`, as for every other selector-taking verb.
    #[tokio::test(start_paused = true)]
    async fn a_selector_that_matches_nothing_is_not_found() {
        let h = harness(vec![]);
        let err = h
            .ctx
            .supervisor
            .signal(
                ProcessSelector::Name("ghost".to_string()),
                OperatorSignal::Hup,
            )
            .await
            .unwrap_err();
        assert_eq!(err, SupervisorError::NotFound);
    }

    /// fails if a reload drainee is skipped. `begin_action` skips one because
    /// an action expects a reply; a signal expects nothing back, and the
    /// drainee is a live process the selector matched. Actor-tier:
    /// `ProcessEntry::reload` is crate-internal.
    #[tokio::test(start_paused = true)]
    async fn a_reload_drainee_is_signalled_like_any_other_live_sheep() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut signal_rx) = actor_with_a_drainee_holding_a_signal_mailbox(&dir);

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Signal {
            selector: ProcessSelector::Id(0),
            sig: OperatorSignal::Hup,
            reply,
        });

        // The request really left the actor for the sheep task's mailbox.
        let request = tokio::time::timeout(ACTION_WINDOW, signal_rx.recv())
            .await
            .expect("no signal reached the drainee's mailbox within the window")
            .expect("the drainee's signal mailbox closed");
        assert_eq!(request.sig, OperatorSignal::Hup);
        // Answer it as a live sheep task would, so the fan-out can settle.
        let _ = request.done.send(Ok(()));

        let rows = tokio::time::timeout(ACTION_WINDOW, answer)
            .await
            .expect("the signal reported nothing within the window")
            .expect("the signal's reply channel was dropped")
            .unwrap();
        assert_eq!(rows[0].outcome, SignalOutcome::Delivered);
    }

    // --- SendLine: `shep whisper`, one selector in, one row per matched
    // sheep out ---

    /// fails if a line does not reach the sheep's pipe. The fake records what
    /// it was handed, so this asserts the line itself.
    #[tokio::test(start_paused = true)]
    async fn a_line_reaches_a_sheep_that_asked_for_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("repl", "./repl");
        app.stdin = true;
        let (handle, runner, _events) = started(&dir, app, vec![ProcScript::never_exits()]).await;

        let rows = handle
            .send_line(ProcessSelector::Id(0), "reload-config".to_string())
            .await
            .unwrap();

        assert_eq!(
            rows,
            vec![LineReply {
                id: 0,
                name: "repl".to_string(),
                outcome: LineOutcome::Sent,
            }]
        );
        assert_eq!(runner.stdin_lines(0), vec!["reload-config".to_string()]);
    }

    /// fails if a sheep without `stdin = true` is answered anything but
    /// `no_stdin`, `Sent` above all: there is no pipe for a line to land in.
    #[tokio::test(start_paused = true)]
    async fn a_sheep_without_stdin_answers_no_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, _runner, _events) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::never_exits()],
        )
        .await;

        let rows = handle
            .send_line(ProcessSelector::Id(0), "hello".to_string())
            .await
            .unwrap();

        assert_eq!(rows[0].outcome, LineOutcome::NoStdin);
    }

    /// fails if a mixed flock is refused as a whole. Half the sheep having a
    /// pipe is the normal case under `all`. `Reopen`, `Flush`, `Trigger` and
    /// `Signal` follow the same rule.
    #[tokio::test(start_paused = true)]
    async fn a_mixed_flock_reports_per_sheep_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let mut piped = AppConfig::minimal("repl", "./repl");
        piped.stdin = true;
        // `started` starts one app, the handle the second: the ids are 0 and 1.
        let (handle, runner, _events) = started(
            &dir,
            piped,
            vec![ProcScript::never_exits(), ProcScript::never_exits()],
        )
        .await;
        handle
            .start(vec![normalize(AppConfig::minimal("web", "./srv")).unwrap()])
            .await
            .unwrap();

        let rows = handle
            .send_line(ProcessSelector::All, "hello".to_string())
            .await
            .unwrap();

        let outcome = |id| rows.iter().find(|r| r.id == id).unwrap().outcome.clone();
        assert_eq!(outcome(0), LineOutcome::Sent);
        assert_eq!(outcome(1), LineOutcome::NoStdin);
        assert_eq!(runner.stdin_lines(1), Vec::<String>::new());
        // id-sorted, like every other row-shaped reply.
        assert!(rows.windows(2).all(|w| w[0].id < w[1].id));
    }

    /// fails if a wait on an app that never reads its stdin has no bound. The
    /// outcome names the bound: "the app is not reading" and "the pipe broke"
    /// have different fixes.
    #[tokio::test(start_paused = true)]
    async fn a_write_that_never_lands_times_out_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("stuck", "./stuck");
        app.stdin = true;
        // `never_reads_its_stdin` accepts the write and answers nothing, which
        // is what a full pipe looks like from this side.
        let (handle, _runner, _events) =
            started(&dir, app, vec![ProcScript::never_reads_its_stdin()]).await;

        let rows = tokio::time::timeout(
            STDIN_WRITE_TIMEOUT * 4,
            handle.send_line(ProcessSelector::Id(0), "hello".to_string()),
        )
        .await
        .expect("send_line did not honour its own bound")
        .unwrap();

        let LineOutcome::NotWritten { reason } = rows[0].outcome.clone() else {
            panic!("expected NotWritten, got {:?}", rows[0].outcome);
        };
        assert!(reason.contains("read"), "{reason}");
    }

    /// fails if a flock of wedged sheep costs `STDIN_WRITE_TIMEOUT` each.
    ///
    /// Two seconds sits under the 5s an RPC caller gets by default, and that
    /// only holds if the waits run concurrently. Awaited in a `for` loop, three
    /// wedged sheep cost six seconds and `shep whisper all` answers
    /// `DeadlineExceeded` instead of three `not_written` rows.
    #[tokio::test(start_paused = true)]
    async fn a_flock_of_wedged_sheep_is_bounded_once_and_not_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("stuck", "./stuck");
        app.stdin = true;
        app.instances = 3;
        let (handle, _runner, _events) =
            started(&dir, app, vec![ProcScript::never_reads_its_stdin(); 3]).await;

        let started_at = tokio::time::Instant::now();
        let rows = handle
            .send_line(ProcessSelector::All, "hello".to_string())
            .await
            .unwrap();
        let elapsed = started_at.elapsed();

        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| matches!(row.outcome, LineOutcome::NotWritten { .. })),
            "{rows:?}"
        );
        // Under the paused clock the auto-advance is exact: sequential waits
        // would read 6s.
        assert!(
            elapsed < STDIN_WRITE_TIMEOUT * 2,
            "three wedged sheep cost {elapsed:?}; the bound is per-CALL, not per-sheep"
        );
    }

    /// fails if a selector matching nothing is answered with an empty
    /// success.
    #[tokio::test(start_paused = true)]
    async fn a_selector_that_matches_nothing_is_not_found_for_send_line() {
        let h = harness(vec![]);
        assert_eq!(
            h.ctx
                .supervisor
                .send_line(ProcessSelector::Name("ghost".to_string()), "x".to_string())
                .await
                .unwrap_err(),
            SupervisorError::NotFound
        );
    }

    // --- flush -------------------------------------------------------
    //
    // That the path and not the pump's current inode is what gets emptied needs
    // a real handle on a real file, and lives in `tests/daemon_e2e.rs`.

    /// Fails if a flush skips a matched sheep's pump, reaches a sheep the
    /// selector never named, or answers with the wrong set.
    ///
    /// The counts are what make this more than a smoke test, as in
    /// [`a_reopen_reaches_every_matched_sheep_and_no_others`].
    #[tokio::test(start_paused = true)]
    async fn a_flush_reaches_every_matched_sheep_and_no_others() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Three scripts for three instances: a fourth spawn would land that
        // sheep pumpless, which reads like the skip being looked for.
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

    /// Fails if the actor awaits a flush's acknowledgement inside its own loop,
    /// the cycle
    /// [`the_actor_keeps_answering_while_a_reopen_waits_on_a_silent_pump`]
    /// describes, reached through the other verb. `list` is the probe: it is
    /// answered from the actor loop and nowhere else.
    #[tokio::test(start_paused = true)]
    async fn the_actor_keeps_answering_while_a_flush_waits_on_a_silent_pump() {
        let (events, _rx) = crate::bus::test_bus(64);
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

    /// What [`LateWritingPumpRunner`]'s pump appends as it answers a flush. One
    /// owner: the cases below assert the file it lands in is empty.
    const LATE_LINE: &str = "landed-while-the-flush-was-being-answered\n";

    /// The sheep [`LateWritingPumpRunner`] gives a late-writing pump to.
    const LATE_WRITING_SHEEP: &str = "latecomer";

    /// A [`ScriptedRunner`] whose spawn of [`LATE_WRITING_SHEEP`] gets a pump
    /// that appends [`LATE_LINE`] to that sheep's stdout log path as it
    /// acknowledges a flush. Every other sheep keeps the fake's own pump.
    ///
    /// A real [`tokio::fs::File`] hands its `write(2)` to the blocking pool, so
    /// a `Flush` can arrive with bytes not yet in the file; the acknowledgement
    /// is what says they landed.
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
            // Replacing the sender drops the fake's own, ending the control
            // task it spawned. This pump answers in its place.
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
                            // Answered only once the bytes are on disk.
                            let _ = done.send(Ok(()));
                        }
                        LogCtl::Reopen { done } => {
                            let _ = done.send(Ok(()));
                        }
                        // This runner exists for the flush ordering.
                        #[cfg(unix)]
                        LogCtl::ReportFds { done } => {
                            let _ = done.send(CarriedFds::none());
                        }
                        // Nothing to start reading again: no streams are read.
                        #[cfg(unix)]
                        LogCtl::Resume => {}
                    }
                }
            });
            Ok((proc, io))
        }
    }

    /// Fails if the truncate runs before the pump has acknowledged the flush:
    /// the file is emptied with a line still in flight, and the line lands at
    /// offset 0 afterwards under `O_APPEND`.
    ///
    /// That the file exists proves the pump was flushed at all, since the
    /// truncate does not create a missing path; that it is empty proves the
    /// truncate came second.
    #[tokio::test(start_paused = true)]
    async fn a_flush_truncates_only_after_its_pump_has_answered() {
        let (events, _rx) = crate::bus::test_bus(64);
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

        // Read off the daemon's own snapshot, so the test cannot disagree with
        // the assembler about the path.
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

    /// Fails if a flush leaves a stopped sheep's log file alone. The operation
    /// addresses recorded paths, not open handles, and `shep bleats
    /// --no-follow` reads a stopped sheep's logs. The fake's control task ends
    /// with its proc, so the truncate is reached through the no-pump leg.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_sheeps_log_file_is_truncated_too() {
        let (events, _rx) = crate::bus::test_bus(64);
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

    /// Fails if instances sharing one log path answer with one row per file
    /// rather than one per sheep, or leave the shared file unemptied.
    ///
    /// The answer is keyed by sheep, since the selector named sheep; the work
    /// is keyed by path, since one truncate empties the file for every handle
    /// open on it. The shared path is asserted rather than assumed.
    #[tokio::test(start_paused = true)]
    async fn instances_sharing_one_log_path_answer_one_row_each() {
        let (events, _rx) = crate::bus::test_bus(64);
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
    /// the selector matched, leaving a sibling sharing that file unflushed.
    ///
    /// [`LateWritingPumpRunner`] is pointed at the unmatched sheep and the
    /// shared file is not created up front, so existence proves the sibling's
    /// pump was reached and emptiness that the truncate waited.
    #[tokio::test(start_paused = true)]
    async fn a_sibling_sharing_a_path_is_flushed_even_when_the_selector_skips_it() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Two scripts for two apps of one instance each: a third spawn would
        // land that sheep pumpless, which reads like the skipped pump.
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(
            LateWritingPumpRunner::new(ScriptedRunner::new(vec![
                ProcScript::never_exits(),
                ProcScript::never_exits(),
            ])),
            test_paths(&dir),
            events,
        );

        // Two apps pointed at one file rather than one app's two instances
        // under `merge_logs`: the sibling needs a name to aim the pump at.
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
    /// success. The failure is keyed by path rather than by sheep: see
    /// [`SupervisorError::FlushFailed`]. The healthy sheep is the second half:
    /// a failure must not stop the rest of the flock being flushed.
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_could_not_flush_fails_the_request() {
        let (events, _rx) = crate::bus::test_bus(64);
        // Two scripts for two instances: a third spawn would land that sheep
        // pumpless.
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
    /// `flush` demands an explicit selector, so a zero exit would tell the
    /// operator the logs they named are empty when nothing was touched.
    #[tokio::test(start_paused = true)]
    async fn a_flush_matching_nothing_is_not_found() {
        let (events, _rx) = crate::bus::test_bus(64);
        let dir = tempfile::tempdir().unwrap();
        let handle = spawn_supervisor(ScriptedRunner::new(vec![]), test_paths(&dir), events);

        assert_eq!(
            handle.flush(ProcessSelector::All).await,
            Err(SupervisorError::NotFound)
        );
    }

    /// Fails if [`truncate_log`] gains a `create(true)`, or treats a missing
    /// path as an error. A log file that is not there is already empty, and a
    /// stray empty log at a path a rotator just renamed away is worse.
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

    /// Fails if [`truncate_log`]'s last arm swallows its error: a `_ => Ok(())`
    /// beside the `NotFound` one, or a `NotFound` guard widened to every kind.
    /// The case above cannot see it, since a missing path answers `Ok`.
    ///
    /// A directory in the log's place fails `open(2)` for writing for every
    /// uid, root included, so this cannot pass for the wrong reason.
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

    /// Fails if [`truncate_log`] stops opening through [`open_log_path`]: drop
    /// the `O_NOFOLLOW` it adds and `shep flush` empties whatever the symlink
    /// points at, with the daemon's privileges.
    ///
    /// The target's bytes prove nothing was emptied, the link still being a
    /// link proves the open did not replace it, and the message names the
    /// symlink rather than `ELOOP`. `#[cfg(unix)]`: `O_NOFOLLOW` and the
    /// refusal are both unix-only.
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

    // --- the log plane mid-reload ------------------------------------
    //
    // A swap's drainee and its replacement derive identical log paths. Both
    // cases name the replacement by id, the form that cannot match the drainee.

    /// Fails if the set of pumps a flush drains is narrowed back to the sheep
    /// the selector matched, leaving a reload's drainee appending to a file
    /// being emptied under it. The same mechanism as
    /// [`a_sibling_sharing_a_path_is_flushed_even_when_the_selector_skips_it`],
    /// reached without configuring anything.
    #[tokio::test(start_paused = true)]
    async fn a_flush_naming_a_replacement_still_drains_the_drainee_sharing_its_path() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts: the original spawn and the one replacement. A third
        // would abandon the reload, leaving one entry and no overlap to see.
        let (handle, runner, mut rx) = started(
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
        assert_eq!(
            mid.len(),
            2,
            "fixture check: both halves of the swap must be registered, or \
             there is no shared path to widen to"
        );
        assert_eq!(
            mid[0].out_file, mid[1].out_file,
            "fixture check: one instance slot must really give both entries \
             one out path, or this case proves nothing"
        );
        assert_eq!(
            mid[0].err_file, mid[1].err_file,
            "fixture check: and one err path"
        );

        let flushed = handle.flush(ProcessSelector::Id(1)).await.unwrap();

        assert_eq!(
            flushed.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![1],
            "the reply answers the selector: the drainee's pump was drained \
             too, but the operator named only the replacement"
        );
        assert_eq!(
            runner.flushes(0),
            1,
            "the drainee's pump is what this case exists for — it is still \
             holding the file the truncate is about to empty"
        );
        assert_eq!(
            runner.flushes(1),
            1,
            "the replacement's pump, which the selector did name"
        );
    }

    /// Fails if a reopen is keyed on the selector alone, leaving a reload's
    /// drainee holding the inode an external rotator has just renamed. The
    /// drainee goes on appending to the archive while the recreated path takes
    /// only the replacement's lines. The counts are the whole case.
    #[tokio::test(start_paused = true)]
    async fn a_reopen_naming_a_replacement_still_reaches_the_drainee_sharing_its_path() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts, counted, for the reason the flush case above gives.
        let (handle, runner, mut rx) = started(
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
        assert_eq!(
            mid.len(),
            2,
            "fixture check: both halves of the swap must be registered"
        );
        assert_eq!(
            mid[0].out_file, mid[1].out_file,
            "fixture check: one instance slot must really give both entries \
             one out path, or this case proves nothing"
        );
        assert_eq!(
            mid[0].err_file, mid[1].err_file,
            "fixture check: and one err path"
        );

        let reopened = handle.reopen(ProcessSelector::Id(1)).await.unwrap();

        assert_eq!(
            reopened.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![1],
            "the reply answers the selector, the same way `flush`'s does"
        );
        assert_eq!(
            runner.reopens(0),
            1,
            "the drainee's pump is what this case exists for — unasked, it \
             keeps the renamed inode open and goes on filling the archive"
        );
        assert_eq!(
            runner.reopens(1),
            1,
            "the replacement's pump, which the selector did name"
        );
        assert_eq!(
            runner.flushes(0),
            0,
            "a reopen must push `LogCtl::Reopen`, never `LogCtl::Flush` — the \
             neighbouring variant would land the drainee's owed bytes and \
             leave it on the renamed inode regardless"
        );
    }

    /// A spawn spec for the cases that drive [`run_sheep`] directly. The
    /// scripted fake reads none of it; [`ProcessRunner::spawn`] takes one.
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
            stdin: false,
            credentials: None,
        }
    }

    // --- Identity: which user a spawn actually runs as ----------------
    //
    // Nothing on the wire reports the uid a child comes up under, so the cases
    // below read it off the `SpawnSpec` through `ScriptedRunner::spawned_as`.

    /// A bare actor holding nothing at all, for the cases that drive
    /// registration and respawn directly. Direct because
    /// [`ProcessEntry::credentials`] is crate-internal.
    fn actor_with_an_empty_flock(
        dir: &tempfile::TempDir,
        scripts: Vec<ProcScript>,
    ) -> Actor<ScriptedRunner> {
        let (events, _events_rx) = crate::bus::test_bus(64);
        let (tx, _rx) = mpsc::channel(MAILBOX_CAPACITY);
        Actor {
            runner: ScriptedRunner::new(scripts),
            paths: test_paths(dir),
            events,
            tx,
            sheep: HashMap::new(),
            next_id: 0,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        }
    }

    /// The name this test process is already running under, the only user a
    /// non-root test can ask for: `privilege::resolve` refuses any request that
    /// would change identity unless the daemon is root.
    #[cfg(unix)]
    fn own_user_name() -> String {
        nix::unistd::User::from_uid(nix::unistd::geteuid())
            .unwrap()
            .expect("this process has a passwd entry")
            .name
    }

    /// A user name no passwd database has an entry for, so `resolve` fails
    /// the same way whether or not the test runs as root.
    const NO_SUCH_USER: &str = "definitely-not-a-real-shep-user";

    // `register_at_rest` records membership and resolves nothing, so the
    // identity is unresolved until the first spawn; reading that as "asked for
    // nobody" hands the child the daemon's own identity.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_restored_app_restarts_under_its_configured_user() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits()]);
        let app = app_with("web", |app| app.user = Some(own_user_name()));

        let info = actor.register_at_rest(&app);
        assert_eq!(
            actor.sheep[&info.id].entry.credentials,
            SpawnIdentity::Unresolved,
            "a membership record is not a `Start`: nothing has looked this app's user up yet"
        );

        actor.respawn(info.id, true);

        let wanted = Credentials {
            uid: nix::unistd::geteuid().as_raw(),
            gid: None,
        };
        assert_eq!(
            actor.runner.spawned_as(0),
            Some(wanted),
            "the restarted child must carry the identity its `user` resolves to; `None` here is \
             the child running as the shepherd, which is the downgrade"
        );
        assert_eq!(
            actor.sheep[&info.id].entry.credentials,
            SpawnIdentity::Resolved(Some(wanted)),
            "resolved once at the first spawn and stored, so no later restart looks it up again"
        );
    }

    // A `Start` refuses this config outright; reaching a spawn through the
    // muster roll must not be a way around that.
    #[tokio::test(start_paused = true)]
    async fn a_restored_app_whose_user_cannot_be_resolved_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits()]);
        let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));

        let info = actor.register_at_rest(&app);
        let after = actor.respawn(info.id, true);

        assert_eq!(
            actor.runner.spawn_count(),
            0,
            "nothing may be spawned for an app whose identity could not be resolved"
        );
        assert_eq!(after.status, ProcStatus::Errored);
        assert_eq!(
            actor.sheep[&info.id].entry.credentials,
            SpawnIdentity::Unresolved,
            "a failed resolution settles nothing: the next restart must ask again"
        );
    }

    // The pinned uid below is one no lookup of this app's `user` could produce,
    // so a respawn that touched the passwd database again would be seen.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_restart_reuses_a_resolved_identity_rather_than_looking_it_up_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _rx) = actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);
        let pinned = Credentials {
            uid: 4242,
            gid: Some(4243),
        };
        let entry = &mut actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registers id 0")
            .entry;
        entry.spec = app_with("web", |app| app.user = Some(own_user_name()));
        entry.credentials = SpawnIdentity::Resolved(Some(pinned));

        actor.respawn(0, true);

        assert_eq!(
            actor.runner.spawned_as(0),
            Some(pinned),
            "a running app's identity must survive its restart untouched"
        );
    }

    // `PerApp` is the muster restore and the dog, where nobody reads the
    // shepherd's log, so a refused app has to be visible as `Errored` rather
    // than missing. `#[cfg(unix)]`: the refusal needs a passwd database.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_start_refused_over_credentials_leaves_an_errored_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits()]);
        let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));

        let err = actor
            .do_start(vec![app], None, BatchPolicy::PerApp)
            .expect_err("an unresolvable user must refuse the start");
        assert!(
            err.to_string().contains(NO_SUCH_USER),
            "the refusal must name the user it could not resolve: {err}"
        );

        assert_eq!(actor.runner.spawn_count(), 0);
        let slot = actor
            .sheep
            .values()
            .next()
            .expect("the refusal must leave a row rather than vanishing");
        assert_eq!(slot.entry.status, ProcStatus::Errored);
        assert_eq!(
            slot.entry.credentials,
            SpawnIdentity::Unresolved,
            "the row must not claim a settled identity: a `restart` would reuse it and bring the \
             sheep up as the shepherd, which is the bug the row was added to make visible"
        );

        // And the row is inert: restarting it meets the same refusal.
        let id = slot.entry.id;
        let after = actor.respawn(id, true);
        assert_eq!(after.status, ProcStatus::Errored);
        assert_eq!(actor.runner.spawn_count(), 0);
    }

    // One `Errored` row and no others is the half-registered flock
    // `AllOrNothing` exists to prevent. `#[cfg(unix)]`: the refusal needs a
    // passwd database.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_all_or_nothing_start_refused_over_credentials_registers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits()]);
        let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));

        let err = actor
            .do_start(vec![app], None, BatchPolicy::AllOrNothing)
            .expect_err("an unresolvable user must refuse the start");
        assert!(
            err.to_string().contains(NO_SUCH_USER),
            "the refusal must name the user it could not resolve: {err}"
        );
        assert!(
            actor.sheep.is_empty(),
            "`AllOrNothing` registers nothing at all, this app included"
        );
        assert_eq!(actor.runner.spawn_count(), 0);
    }

    // An app registered at rest has never resolved its identity, so the scale
    // is the first call that asks.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn scaling_an_app_whose_user_will_not_resolve_leaves_the_flock_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits(); 4]);
        let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));
        let registered = actor.register_at_rest(&app);

        let (reply, answer) = oneshot::channel();
        actor.handle_scale("web", 3, reply);
        let err = answer
            .await
            .expect("handle_scale answers every call")
            .expect_err("an unresolvable user must refuse the scale");

        let SupervisorError::CannotStart(message) = &err else {
            panic!("expected CannotStart, got {err:?}");
        };
        assert!(
            message.contains("web") && message.contains(NO_SUCH_USER),
            "the refusal must name the app and the user it could not resolve: {message}"
        );
        assert_eq!(actor.runner.spawn_count(), 0);
        assert_eq!(
            actor.sheep.len(),
            1,
            "no instance may be registered by a scale that could not resolve an identity"
        );
        assert_eq!(
            actor.sheep[&registered.id].entry.spec.config().instances,
            1,
            "the stored count must not be written back for a scale that did not happen"
        );
        assert_eq!(
            actor.sheep[&registered.id].entry.credentials,
            SpawnIdentity::Unresolved,
            "a failed resolution settles nothing: the next attempt must ask again"
        );
    }

    // `register_without_spawning` is idempotent: the second restore finds the
    // row it made, so nothing transitioned and no event is owed. An emit keyed
    // on the row's status cannot tell the two apart, both being `Errored`.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_second_restore_does_not_re_announce_an_errored_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits(); 2]);
        let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));
        let mut events = actor.events.subscribe();

        for attempt in 1..=2 {
            assert!(
                actor
                    .do_start(vec![app.clone()], None, BatchPolicy::PerApp)
                    .is_err(),
                "restore {attempt} must refuse the app"
            );
        }

        assert_eq!(
            actor.sheep.len(),
            1,
            "the second restore must find the row, not add a second one"
        );

        let mut errored = 0;
        while let Ok(event) = events.try_recv().map(|event| event.to_event()) {
            if let BusEvent::Process {
                event: ProcessEventKind::Errored,
                info,
                ..
            } = event
                && info.name == "web"
            {
                errored += 1;
            }
        }
        assert_eq!(
            errored, 1,
            "one Errored event, for the restore that actually registered the row: a second is a \
             transition that did not happen"
        );
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn scaling_up_the_row_a_refused_start_left_meets_the_same_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits(); 4]);
        let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));

        // The row, made the way an unattended boot makes it.
        actor
            .do_start(vec![app], None, BatchPolicy::PerApp)
            .expect_err("an unresolvable user must refuse the start");
        let id = actor
            .sheep
            .values()
            .next()
            .expect("the refused start leaves a row")
            .entry
            .id;
        assert_eq!(actor.sheep[&id].entry.status, ProcStatus::Errored);

        let (reply, answer) = oneshot::channel();
        actor.handle_scale("web", 3, reply);
        let err = answer
            .await
            .expect("handle_scale answers every call")
            .expect_err("the row's user still will not resolve");

        assert!(
            matches!(err, SupervisorError::CannotStart(_)),
            "expected CannotStart, got {err:?}"
        );
        assert_eq!(actor.runner.spawn_count(), 0);
        assert_eq!(
            actor.sheep.len(),
            1,
            "no instance may be registered by a scale that could not resolve an identity"
        );
    }

    // The user is resolvable, so an identity still `Unresolved` afterwards is
    // what says nothing asked.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_no_op_scale_asks_no_passwd_question() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits(); 2]);
        let app = app_with("web", |app| app.user = Some(own_user_name()));
        let registered = actor.register_at_rest(&app);

        // `register_at_rest` registers `instance: 0` whatever `instances`
        // says, so `current` is 1 and this is the `Ordering::Equal` arm.
        let (reply, answer) = oneshot::channel();
        actor.handle_scale("web", 1, reply);
        answer
            .await
            .expect("handle_scale answers every call")
            .expect("scaling to the count an app already has is a no-op");

        assert_eq!(
            actor.sheep[&registered.id].entry.credentials,
            SpawnIdentity::Unresolved,
            "a scale that spawns nothing must not resolve an identity: nothing would use it, \
             and asking is what lets the answer refuse the call"
        );
        assert_eq!(actor.runner.spawn_count(), 0);
    }

    // The user cannot be resolved, so a scale that asks is a scale that fails.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_no_op_scale_survives_a_user_that_will_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits(); 2]);
        let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));
        let registered = actor.register_at_rest(&app);

        let (reply, answer) = oneshot::channel();
        actor.handle_scale("web", 1, reply);
        let scaled = answer
            .await
            .expect("handle_scale answers every call")
            .expect("a no-op scale must not be refused over an identity it never needed");

        assert_eq!(scaled.instances.len(), 1);
        assert_eq!(actor.runner.spawn_count(), 0);
        assert_eq!(
            actor.sheep[&registered.id].entry.status,
            ProcStatus::Stopped,
            "a no-op must leave the sheep exactly as it found it"
        );
    }

    // The `Errored` event carries no reason and the deferred reply has no
    // per-id error slot, so this log line is all an operator has. `#[test]`
    // with a `block_on` inside: `capture_logs` scopes its subscriber to one
    // thread and needs a synchronous closure.
    #[cfg(unix)]
    #[test]
    fn a_restart_that_starts_nothing_says_why_in_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // Route one: no script to pop, so the runner refuses the spawn.
        let refused = capture_logs(|| {
            let mut actor = actor_with_an_empty_flock(&dir, Vec::new());
            let info = actor.register_at_rest(&app_with("web", |_| {}));
            rt.block_on(async { actor.respawn(info.id, true) });
        });
        assert!(
            refused.contains("script exhausted"),
            "the runner's own reason must reach the log: {refused}"
        );

        // Route two: the identity cannot be resolved, so no spawn is
        // attempted at all.
        let unresolved = capture_logs(|| {
            let mut actor = actor_with_an_empty_flock(&dir, vec![ProcScript::never_exits()]);
            let app = app_with("web", |app| app.user = Some(NO_SUCH_USER.to_string()));
            let info = actor.register_at_rest(&app);
            rt.block_on(async { actor.respawn(info.id, true) });
        });
        assert!(
            unresolved.contains(NO_SUCH_USER),
            "the unresolvable user must reach the log by name: {unresolved}"
        );
    }

    // ---------------------------------------------------------------
    // Supervisor proptest
    // ---------------------------------------------------------------

    // The command script and the process script are generated independently;
    // their interleaving emerges from the runtime. Invariants are read off
    // successive `list()` snapshots and the event stream, never off tick
    // counts.

    // No `Shutdown` step: it closes the actor's mailbox, so nothing composes
    // after it. Each step is fully awaited before the next, so manual-vs-manual
    // races belong in this file's dedicated race tests.

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
        /// Neither `wait_ready` nor `readiness_probe`: `spawn_fresh` marks the
        /// sheep `Online` inline.
        Ungated,
        /// `wait_ready = true` with this `listen_timeout` in milliseconds. No
        /// scripted child writes `{"kind":"ready"}`, so every wait ends at its
        /// deadline, and the deadline decides which later step lands under it.
        Channel(u64),
    }

    fn gate_strategy() -> impl proptest::strategy::Strategy<Value = Gate> {
        use proptest::strategy::Strategy as _; // `prop_map` below
        proptest::prop_oneof![
            2 => proptest::strategy::Just(Gate::Ungated),
            // Spans the driver's own durations, a 1600ms kill ladder and a
            // 2000ms `stable_then_exit`, so a deadline drawn here lands
            // before, during and after them.
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
    /// A 9-command run reaches at most 30 command-driven spawns plus
    /// crash-loop respawns capped at 16 per sheep: under 200 all told. An
    /// exhausted `ScriptedRunner` answers `SpawnFailed`, which the actor turns
    /// into `Errored`, so a claim about a restart that must not happen would
    /// pass for the wrong reason. Still finite: a `stable_then_exit` script
    /// resets the restart budget, and the pool running dry ends the chain.
    const SCRIPT_POOL: usize = 512;

    /// How long the steady-state drain waits for one more transition before
    /// concluding there are none left.
    ///
    /// Longer than every deadline a run can leave pending (a 4000ms readiness
    /// wait, a 1600ms kill ladder, a 2000ms `stable_then_exit` script) and far
    /// shorter than `fake::NEVER_MS`, so a `never_exits` proc stays alive
    /// across it.
    const QUIET_WINDOW: Duration = Duration::from_secs(60);

    /// Ceiling on transitions observed after the last command. Each spawn
    /// produces at most a start/restart, an online and a terminal event, so
    /// `3 * SCRIPT_POOL` bounds a correct run; anything past this ceiling is
    /// a flock that never settles.
    const EVENT_BUDGET: usize = 3 * SCRIPT_POOL;

    proptest::proptest! {
        // 128 cases: 24 misses the 3-step `[StartOne, StopAll, DeleteFirst]`
        // sequence that equally-weighted draws rarely land. ~0.6s under the
        // paused clock. `PROPTEST_CASES` overrides.
        #![proptest_config(crate::testing::proptest_config(128))]

        #[test]
        fn supervisor_upholds_its_invariants_under_any_interleaving(
            steps in proptest::collection::vec(step_strategy(), 1..10),
            gates in proptest::collection::vec(gate_strategy(), 1..10),
            scripts in proptest::collection::vec(script_strategy(), SCRIPT_POOL..SCRIPT_POOL + 1),
        ) {
            // Paused clock: every backoff, kill ladder and readiness delay is
            // virtual, so a 128-case run stays cheap whatever it draws.
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
                let (events, mut rx) = crate::bus::test_bus(8192);
                let handle = spawn_supervisor(
                    ScriptedRunner::new(scripts),
                    test_paths(&dir),
                    events,
                );
                let mut started = 0u32;
                let mut highest_restarts = std::collections::HashMap::<u32, u32>::new();
                // `extra_restart` is the one command with no reply, so it is
                // the only way a restart is still mid-kill-ladder when the next
                // step is issued. `Step::StopAll` keeps its claim across that:
                // `claim_manual` takes the `manual` marker off it.

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
                                handle.extra_restart(first.id, pid, None, None).await;
                            }
                        }
                        Step::StaleReport => {
                            if let Some(first) = handle.list().await.first() {
                                // Never this sheep's own pid. A pid belonging
                                // to another sheep is just as stale: the guard
                                // compares against this id's entry.
                                let stale = first.pid.unwrap_or(0).wrapping_add(1);
                                handle.extra_restart(first.id, stale, None, None).await;
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
                // transitioning. A bounded window, not `try_recv`: a run ends
                // with deadlines still pending, and the window walks the
                // paused clock over them.
                let mut observed = Vec::new();
                loop {
                    match tokio::time::timeout(QUIET_WINDOW, rx.recv()).await {
                        Ok(Ok(event)) => {
                            observed.push(event.to_event());
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
                // `Online` for an id with no live process. `spawn_fresh` never
                // reuses an id, and `handle_ready_result` resolves long after
                // the spawn it belongs to.
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
                        // One out and one in: the predecessor's `Msg::Exited`
                        // is what caused the respawn, so the id stays live.
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
                        // `ProcessEventKind` is `#[non_exhaustive]`, so E0004
                        // never fires here. Leaving `live` untouched for a
                        // later variant only makes the assertions stricter.
                        _ => {}
                    }
                    // (2) again, off the event stream rather than `list()`: a
                    // snapshot only sees the counter between two commands.
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
                // this tail agree.
                Ok::<(), proptest::test_runner::TestCaseError>(())
            })?;
        }
    }

    /// Two descriptors standing in for the daemon's own, so a snapshot taken
    /// in a test names numbers that are really open. The actor never learns
    /// the real listener and pidfile, so they are an argument.
    #[cfg(unix)]
    fn daemon_fds(dir: &tempfile::TempDir) -> (DaemonFds, [std::fs::File; 2]) {
        use std::os::fd::AsRawFd as _;

        let listener = std::fs::File::create(dir.path().join("listener.stand-in")).unwrap();
        let pidfile = std::fs::File::create(dir.path().join("pidfile.stand-in")).unwrap();
        let fds = DaemonFds {
            listener: listener.as_raw_fd(),
            pidfile: pidfile.as_raw_fd(),
        };
        // Returned alongside so the caller holds both files open: a closed
        // descriptor's number is free to be handed to the next open.
        (fds, [listener, pidfile])
    }

    /// Whether `fd` names something open in this process.
    #[cfg(unix)]
    fn is_open(fd: std::os::fd::RawFd) -> bool {
        nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).is_ok()
    }

    /// One of the two sheep has a shepherd channel and the other does not. The
    /// channel is the one number a snapshot may drop, so a case with only
    /// channelled sheep could not tell "every number is open" from "every
    /// number is present".
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_blob_from_a_live_flock_names_open_descriptors_per_sheep() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut talkative = AppConfig::minimal("api", "./srv");
        talkative.channel = true;
        handle
            .start(vec![
                normalize(AppConfig::minimal("web", "./srv")).unwrap(),
                normalize(talkative).unwrap(),
            ])
            .await
            .unwrap();

        let (candidates, blob, _parked) = handle.handover_snapshot(fds).await.unwrap();

        assert_eq!(candidates.len(), 2, "both sheep must reach the gate");
        assert_eq!(blob.sheep().len(), 2);
        for sheep in blob.sheep() {
            for fd in sheep.fds().all().into_iter().flatten() {
                assert!(is_open(fd), "blob names a closed descriptor: {fd}");
            }
        }
    }

    /// The pump reports the number it was told at the spawn and cannot learn
    /// the channel is gone, so the number can name whatever the kernel has
    /// since handed to the next `open` and the successor would write a
    /// shepherd message into a log file. `SheepSlot::open_channel` decides
    /// delivery, and the snapshot masks the field with it. The fake reports a
    /// number for every field, so without the mask this case sees one.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_snapshot_names_no_channel_for_a_sheep_that_has_none() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut talkative = AppConfig::minimal("api", "./srv");
        talkative.channel = true;
        handle
            .start(vec![
                normalize(AppConfig::minimal("web", "./srv")).unwrap(),
                normalize(talkative).unwrap(),
            ])
            .await
            .unwrap();

        let (_candidates, blob, _parked) = handle.handover_snapshot(fds).await.unwrap();

        let named: Vec<(&str, bool)> = blob
            .sheep()
            .iter()
            .map(|sheep| (sheep.name(), sheep.fds().channel.is_some()))
            .collect();
        assert!(
            named.contains(&("web", false)),
            "a sheep with no channel must carry no channel descriptor: {named:?}"
        );
        assert!(
            named.contains(&("api", true)),
            "a sheep with a live channel must carry its descriptor: {named:?}"
        );
    }

    /// A pump that never answers has no deadline above it but this one: the
    /// SIGHUP path awaits the snapshot before it can fall back, so one stall
    /// takes out the handover and the graceful stop with it. An empty
    /// `CarriedFds` is what a stopped sheep reports, so collapsing a wedged
    /// live pump into one would pass the gate with descriptors dropped. Two
    /// sheep, one of each kind, so a gate that refused every flock with a pump
    /// in it would pass here for the wrong reason.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_never_reports_refuses_the_snapshot_instead_of_hanging() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits(); 2])
            .with_a_pump_that_never_reports(&["wedged"]);
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        handle
            .start(vec![
                normalize(AppConfig::minimal("answering", "./srv")).unwrap(),
                normalize(AppConfig::minimal("wedged", "./srv")).unwrap(),
            ])
            .await
            .unwrap();

        // Far longer than the deadline under test, and never actually waited:
        // it buys a failure rather than a hung suite if the snapshot has no
        // deadline of its own.
        let snapshot =
            tokio::time::timeout(Duration::from_secs(3600), handle.handover_snapshot(fds))
                .await
                .expect("a snapshot over a wedged pump must answer rather than hang")
                .unwrap();
        let (candidates, _blob, _parked) = snapshot;

        let borrowed: Vec<crate::handover::Candidate<'_>> = candidates
            .iter()
            .map(crate::handover::OwnedCandidate::as_candidate)
            .collect();
        assert_eq!(
            crate::handover::fitness(&borrowed),
            crate::handover::Fitness::Refused(crate::handover::RefusedReason::PumpUnresponsive {
                sheep: "wedged".to_string(),
            }),
            "the gate must refuse, and must name the sheep whose pump went quiet"
        );
        for candidate in &candidates {
            assert_eq!(
                candidate.pump_unresponsive,
                candidate.entry.spec.config().name == "wedged",
                "exactly the wedged sheep is unresponsive, not its neighbour"
            );
        }
    }

    /// A report parks the pump that answers it, and only that one: a pump that
    /// missed the deadline is still reading its streams. An abandoned handover
    /// is audited by the resume counter, so a spurious resume reads as a
    /// repaired pump forever after. Asserted by sending the resumes, so this
    /// is a claim about which pump is told, not how many senders are held.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_pump_that_missed_the_deadline_is_not_in_the_parked_set() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = Arc::new(
            ScriptedRunner::new(vec![ProcScript::never_exits(); 2])
                .with_a_pump_that_never_reports(&["wedged"]),
        );
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let handle = spawn_supervisor(SharedRunner(Arc::clone(&runner)), test_paths(&dir), events);
        handle
            .start(vec![
                normalize(AppConfig::minimal("answering", "./srv")).unwrap(),
                normalize(AppConfig::minimal("wedged", "./srv")).unwrap(),
            ])
            .await
            .unwrap();

        let (_candidates, _blob, parked) =
            tokio::time::timeout(Duration::from_secs(3600), handle.handover_snapshot(fds))
                .await
                .expect("a snapshot over a wedged pump must answer rather than hang")
                .unwrap();
        parked.resume().await;

        // By name, because spawn order is the supervisor's business and
        // every counter here is indexed by it.
        let answering = runner.spawn_index_of("answering").expect("started above");
        let wedged = runner.spawn_index_of("wedged").expect("started above");
        // A resume carries no acknowledgement, so a send that has returned
        // has only been queued. Bounded, and instant under the paused clock.
        let delivered = async {
            while runner.resumes(answering) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), delivered)
            .await
            .expect("the pump that answered was parked, and is owed a resume");
        assert_eq!(
            runner.resumes(wedged),
            0,
            "a pump that never answered never parked, so nothing may resume it"
        );
    }

    /// Six is where a serial sweep first outlasts `shep-cli`'s
    /// `admin::KILL_TEARDOWN_WAIT` (10s): past it the client gives up first,
    /// falls back to a predecessor still serving, and exits 0 before the gate
    /// refuses for real. Under the paused clock a serial sweep reads six
    /// [`REPORT_DEADLINE`]s (12s) and a concurrent one about one (2s); the
    /// bound below sits in that gap.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_sweep_of_six_wedged_pumps_costs_one_deadline_not_six() {
        const WEDGED: usize = 6;
        let names: Vec<String> = (0..WEDGED).map(|i| format!("wedged{i}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();

        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits(); WEDGED])
            .with_a_pump_that_never_reports(&name_refs);
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        handle
            .start(
                names
                    .iter()
                    .map(|name| normalize(AppConfig::minimal(name, "./srv")).unwrap())
                    .collect(),
            )
            .await
            .unwrap();

        let started_at = tokio::time::Instant::now();
        let (candidates, blob, _parked) =
            tokio::time::timeout(Duration::from_secs(3600), handle.handover_snapshot(fds))
                .await
                .expect("a snapshot over six wedged pumps must answer rather than hang")
                .unwrap();
        let elapsed = started_at.elapsed();

        assert!(
            elapsed < REPORT_DEADLINE * 2,
            "six wedged pumps swept concurrently should cost about one \
             REPORT_DEADLINE, not six; took {elapsed:?}"
        );

        assert_eq!(candidates.len(), WEDGED);
        for candidate in &candidates {
            assert!(
                candidate.pump_unresponsive,
                "{} must still be reported unresponsive; racing the reports \
                 must not blur which pump answered",
                candidate.entry.spec.config().name
            );
        }
        // `join_all` returns in input order and `handle_handover_snapshot`
        // sorted `drafts` by id, so `spawn_handover_task` needs no re-sort.
        assert!(
            candidates.windows(2).all(|w| w[0].entry.id < w[1].entry.id),
            "candidates must stay in id order across a concurrent sweep"
        );
        assert!(
            blob.sheep().windows(2).all(|w| w[0].id() < w[1].id()),
            "the blob must stay in id order too: it is a file an operator may read"
        );
    }

    /// A successor that reissued a live id would collide with a caller still
    /// holding it, and a manual command the successor never sees is an
    /// operator's `stop` that comes back as a running sheep. The manual marker
    /// is asserted whole, since kind and origin decide different things on the
    /// far side. The sheep has no live pump, which is also the
    /// registered-but-not-running case: no descriptors, and not a refusal.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn the_snapshot_carries_the_actors_counters_and_slot_state() {
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let (mut actor, _ctl_rx) = actor_with_stopping_drainee(&dir, 4242, 7);
        actor.sheep.get_mut(&0).unwrap().manual = Some(PendingManual {
            kind: ManualKind::Stop,
            origin: CommandOrigin::Operator,
        });
        actor.sheep.get_mut(&0).unwrap().ready_failed = true;

        let (reply, rx) = oneshot::channel();
        actor.handle_handover_snapshot(fds, reply);
        let (candidates, blob, _parked) = rx.await.unwrap().unwrap();

        assert!(
            !candidates.is_empty(),
            "the flock must still reach the gate at all"
        );
        assert_eq!(
            blob.sheep()[0].manual(),
            Some(PendingManual {
                kind: ManualKind::Stop,
                origin: CommandOrigin::Operator,
            }),
            "a manual stop must reach the successor, kind and origin both"
        );
        assert_eq!(
            blob.sheep()[0].pending_delete(),
            Some(false),
            "nothing asked for this sheep to be deleted"
        );
        assert_eq!(
            blob.sheep()[0].ready_failed(),
            Some(true),
            "an earlier reload's failed verdict must reach the successor, or the rollback that \
             follows it has nothing left to replace"
        );
        assert!(
            blob.next_id() > 0,
            "a successor that reissues a live id collides"
        );
        assert_eq!(blob.sheep()[0].epoch(), 7, "a stale timer must stay stale");
        assert_eq!(
            blob.sheep()[0].fds(),
            CarriedFds::none(),
            "a sheep with no pump has no descriptors to carry"
        );
    }

    /// A runner that takes an inherited sheep without a real process behind
    /// it, so the install path can be driven under the paused clock.
    ///
    /// `wait` never resolves: these cases assert on what an install puts in
    /// the flock, and an exit that arrived on its own would race them.
    #[cfg(unix)]
    #[derive(Debug, Default)]
    struct AdoptingRunner;

    /// The pid [`AdoptingRunner`] gives anything it spawns fresh. Not a pid
    /// any carried sheep in these cases holds.
    #[cfg(unix)]
    const STAND_IN_SPAWN_PID: u32 = 7000;

    /// A proc with a pid and no process: it reports what it was built with
    /// and never exits.
    #[cfg(unix)]
    #[derive(Debug)]
    struct StandInProc {
        pid: u32,
    }

    #[cfg(unix)]
    impl RunningProcess for StandInProc {
        fn pid(&self) -> u32 {
            self.pid
        }

        async fn wait(&mut self) -> ExitOutcome {
            core::future::pending().await
        }

        fn signal(&mut self, _sig: crate::runner::StopSignal) -> Result<(), RunnerError> {
            Ok(())
        }

        fn kill_tree(&mut self) -> Result<(), RunnerError> {
            Ok(())
        }
    }

    /// Four channels shaped the way an adopted sheep's are: logs and
    /// shepherd traffic closed, since nothing here writes either.
    #[cfg(unix)]
    fn stand_in_io() -> ProcIo {
        let (_logs_tx, logs) = mpsc::channel(1);
        let (_from_child_tx, from_child) = mpsc::channel(1);
        let (to_child, to_child_rx) = mpsc::channel(1);
        drop(to_child_rx);
        let (log_ctl, log_ctl_rx) = mpsc::channel(1);
        drop(log_ctl_rx);
        let (to_stdin, to_stdin_rx) = mpsc::channel(1);
        drop(to_stdin_rx);
        ProcIo {
            logs,
            from_child,
            to_child,
            log_ctl,
            to_stdin,
        }
    }

    #[cfg(unix)]
    impl ProcessRunner for AdoptingRunner {
        type Proc = StandInProc;

        fn spawn(&self, _spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
            Ok((
                StandInProc {
                    pid: STAND_IN_SPAWN_PID,
                },
                stand_in_io(),
            ))
        }

        fn adopt(
            &self,
            spec: crate::runner::AdoptSpec,
        ) -> Result<(Self::Proc, ProcIo), RunnerError> {
            Ok((StandInProc { pid: spec.pid }, stand_in_io()))
        }
    }

    /// The wait that would resolve it belonged to a task of the predecessor's,
    /// which the `execve` took. Nothing else moves a sheep off `Starting`
    /// except its own exit, so a successor that adopts one without re-arming
    /// leaves it there with none of `arm_extras`'s watch, cron or memory
    /// limits, which fire at the `Online` transition. Nothing here writes
    /// `{"kind":"ready"}` and `handle_ready_result` puts a timed-out sheep
    /// `Online` anyway, so reaching `Online` at all proves a wait was armed.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_adopted_sheep_that_was_still_starting_reaches_online() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried("web", 7, Some(4242), |entry| {
                    let mut app = AppConfig::minimal("web", "./srv");
                    app.autorestart = false;
                    app.wait_ready = true;
                    entry.spec = normalize(app).unwrap();
                    entry.status = ProcStatus::Starting;
                }))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        assert_eq!(
            sup.list().await[0].status,
            ProcStatus::Starting,
            "the blob said this sheep was still starting, so the successor must agree"
        );

        // Polled, not advanced by a fixed amount: the wait runs on a task, so
        // its result reaches the actor a message later than the deadline. The
        // bound sits past the app's 3s `listen_timeout`.
        let goes_online = async {
            while sup.list().await[0].status != ProcStatus::Online {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(30), goes_online)
            .await
            .expect("an adopted sheep left `Starting` must still have a readiness wait over it");
    }

    /// One sheep as a blob describes it, with `mutate` free to give it a
    /// history no fresh registration could have.
    #[cfg(unix)]
    fn carried(
        name: &str,
        id: u32,
        pid: Option<u32>,
        mutate: impl FnOnce(&mut ProcessEntry),
    ) -> CarriedSheep {
        carried_marked(name, id, pid, false, None, false, mutate)
    }

    /// [`carried`], for an instance an earlier reload's readiness
    /// verification failed against.
    #[cfg(unix)]
    fn carried_ready_failed(
        name: &str,
        id: u32,
        pid: Option<u32>,
        mutate: impl FnOnce(&mut ProcessEntry),
    ) -> CarriedSheep {
        carried_marked(name, id, pid, false, None, true, mutate)
    }

    /// [`carried`], with the slot facts a blob carries beside the entry set
    /// explicitly: a pending delete, the manual command that owns this
    /// sheep's next exit, and an earlier reload's failed readiness verdict.
    #[cfg(unix)]
    fn carried_marked(
        name: &str,
        id: u32,
        pid: Option<u32>,
        pending_delete: bool,
        manual: Option<PendingManual>,
        ready_failed: bool,
        mutate: impl FnOnce(&mut ProcessEntry),
    ) -> CarriedSheep {
        let mut app = AppConfig::minimal(name, "./srv");
        // Nothing here wants a respawn: an automatic restart would spawn a
        // second process behind the assertions.
        app.autorestart = false;
        let mut entry = ProcessEntry {
            id,
            spec: normalize(app).unwrap(),
            pending: None,
            pending_reidentifies: false,
            overridden: Vec::new(),
            instance: 0,
            status: ProcStatus::Online,
            pid,
            restarts: 0,
            started_at: None,
            budget: RestartBudget::default(),
            reload: ReloadState::None,
            credentials: SpawnIdentity::Resolved(None),
            out_file: PathBuf::new(),
            err_file: PathBuf::new(),
            dog: None,
            last_exit: None,
        };
        mutate(&mut entry);
        CarriedSheep::from_entry(
            &entry,
            0,
            CarriedFds::none(),
            pending_delete,
            manual,
            ready_failed,
            None,
        )
    }

    /// One sheep as a blob describes it, owed a respawn at a named moment.
    ///
    /// Needs a `restart_delay` on the app, since a deadline means nothing
    /// without a configured delay to be shorter than. `autorestart` is left
    /// on, unlike [`carried_marked`]'s: these cases want the respawn.
    #[cfg(unix)]
    fn carried_owed_a_restart(
        name: &str,
        id: u32,
        delay: shep_core::values::UpDuration,
        due: Option<SystemTime>,
    ) -> CarriedSheep {
        let mut app = AppConfig::minimal(name, "./srv");
        app.restart_delay = Some(delay);
        let entry = ProcessEntry {
            id,
            spec: normalize(app).unwrap(),
            pending: None,
            pending_reidentifies: false,
            overridden: Vec::new(),
            instance: 0,
            status: ProcStatus::WaitingRestart,
            pid: None,
            restarts: 1,
            started_at: None,
            budget: RestartBudget::default(),
            reload: ReloadState::None,
            credentials: SpawnIdentity::Resolved(None),
            out_file: PathBuf::new(),
            err_file: PathBuf::new(),
            dog: None,
            last_exit: None,
        };
        CarriedSheep::from_entry(&entry, 0, CarriedFds::none(), false, None, false, due)
    }

    /// A carried sheep with no descriptors to rebuild, which is every case
    /// below that does not open real pipes.
    #[cfg(unix)]
    fn without_handles(carried: CarriedSheep) -> crate::handover::adopt::AdoptedSheep {
        crate::handover::adopt::AdoptedSheep {
            carried,
            out_pipe: None,
            err_pipe: None,
            out_log: None,
            err_log: None,
            stdin_pipe: None,
            channel: None,
        }
    }

    /// Counters as a blob carries them, with `next_id` the one a case cares
    /// about.
    #[cfg(unix)]
    const fn counters(next_id: u32) -> Counters {
        Counters {
            next_id,
            next_deadline: 0,
            next_action_stamp: 0,
        }
    }

    /// Every row of a listing by id, which is the order these cases name
    /// their sheep in.
    #[cfg(unix)]
    fn by_id(mut info: Vec<ProcessInfo>) -> Vec<ProcessInfo> {
        info.sort_unstable_by_key(|row| row.id);
        info
    }

    /// A sheep whose pid moved was respawned.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_adopted_flock_keeps_its_pids_and_ids() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![
                    without_handles(carried("web", 7, Some(4242), |_| {})),
                    without_handles(carried("api", 8, Some(4243), |_| {})),
                ],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let info = by_id(sup.list().await);

        assert_eq!(
            info.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![7, 8],
            "the successor reissued ids instead of carrying them"
        );
        assert_eq!(
            info.iter().map(|row| row.pid).collect::<Vec<_>>(),
            vec![Some(4242), Some(4243)],
            "a sheep whose pid moved was respawned"
        );
    }

    /// Losing the marker is invisible to a pid check: the dog runs on. What
    /// changes is that `matching_ids` stops passing it by, so `shep restart
    /// all` reaches it and `shep dogs` loses it entirely.
    ///
    /// A `stop` over a kennel with no sheep in it, so nothing waits on a kill
    /// ladder and `NotFound` answers the wildcard. With the marker dropped the
    /// wildcard matches the dog and parks on an exit `StandInProc` never
    /// delivers, so the timeout turns a hung suite into a failure.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_adopted_dog_keeps_its_marker_and_stays_out_of_a_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried(
                    "log-rotate",
                    7,
                    Some(4242),
                    |entry| {
                        entry.dog = Some(DogSource::Adopted {
                            path: "/opt/bin/shep-log-rotate".to_string(),
                        });
                    },
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let info = sup.list().await;
        assert_eq!(
            info[0].dog,
            Some(DogSource::Adopted {
                path: "/opt/bin/shep-log-rotate".to_string(),
            }),
            "a dog adopted as an ordinary sheep is one `shep dogs` has lost"
        );

        let swept = tokio::time::timeout(Duration::from_secs(5), sup.stop(ProcessSelector::All))
            .await
            .expect("a wildcard over a flock of nothing but dogs must answer rather than hang");
        assert!(
            matches!(swept, Err(SupervisorError::NotFound)),
            "`all` is the flock, not the kennel: {swept:?}"
        );
    }

    /// Losing these is silent. `restarts` resetting to zero hands a
    /// crash-looping app amnesty it did not earn, and a lost `last_exit`
    /// answers "why did it stop" with nothing.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_adopted_sheep_keeps_its_counters_and_last_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried("web", 7, Some(4242), |entry| {
                    entry.restarts = 4;
                    entry.last_exit = Some(ExitInfo {
                        code: Some(2),
                        signal: None,
                    });
                }))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let info = sup.list().await;

        assert_eq!(info[0].restarts, 4, "the restart count was reset");
        assert_eq!(
            info[0].last_exit,
            Some(ExitInfo {
                code: Some(2),
                signal: None
            }),
            "the last exit was lost"
        );
    }

    /// An adopted sheep that exits must reach `handle_exited` and be judged by
    /// `decide_on_exit` like any other, not sit there `Online` forever.
    ///
    /// A real child and the real runner, since a scripted exit would prove the
    /// fake rather than the targeted `waitpid` an adopted pid needs. `/bin/sh`
    /// is spawned by `std::process::Command`, so tokio holds no `Child` for it
    /// and nothing but the reaper waits on it.
    #[cfg(unix)]
    #[tokio::test]
    #[expect(
        clippy::zombie_processes,
        reason = "the adopted flock's reaper collects this status; a Child::wait would take it first"
    )]
    async fn an_adopted_sheeps_exit_flows_through_the_ordinary_path() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("a test host can spawn a shell");
        let pid = child.id();
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried("web", 7, Some(pid), |_| {}))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("the adopted child is signalable");

        let info = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let info = sup.list().await;
                if info[0].status == ProcStatus::Stopped {
                    return info;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("an adopted sheep's exit reaches the actor");

        assert_eq!(
            info[0].last_exit,
            Some(ExitInfo {
                code: None,
                signal: Some(9)
            }),
            "the exit must be recorded, not lost"
        );
    }

    /// [`handover::fitness`] carries a pending delete rather than refusing it,
    /// so a sheep whose delete was in flight when the predecessor exec'd must
    /// still be deregistered on its next exit, not left registered or
    /// respawned. A real child and the real runner: an adopted pid has no
    /// `Child` handle, and only the reaper's `Msg::Exited` proves the carried
    /// marker reaches `handle_exited`.
    #[cfg(unix)]
    #[tokio::test]
    #[expect(
        clippy::zombie_processes,
        reason = "the adopted flock's reaper collects this status; a Child::wait would take it first"
    )]
    async fn a_carried_pending_delete_deregisters_on_the_next_exit() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("a test host can spawn a shell");
        let pid = child.id();
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_marked(
                    "web",
                    7,
                    Some(pid),
                    true,
                    None,
                    false,
                    |_| {},
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("the adopted child is signalable");

        let info = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let info = sup.list().await;
                if info.is_empty() {
                    return info;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect(
            "a carried pending delete must deregister the sheep on its next exit, not merely \
             stop it",
        );

        assert!(
            info.is_empty(),
            "the deleted sheep must be gone, not just stopped"
        );
    }

    /// A real child for the adoption cases below to take over, running
    /// `script` under `/bin/sh`, and its pid.
    ///
    /// Its own process group is load-bearing: `TokioProc::signal` and
    /// `kill_tree` both address the group, so a child that inherited this test
    /// binary's group is not a group leader, `killpg` answers `ESRCH`, and the
    /// ladder delivers nothing. The `Child` handle is dropped rather than
    /// waited on, so the adopted flock's own reaper collects the status.
    #[cfg(unix)]
    fn adoptable_child(script: &str) -> u32 {
        use std::os::unix::process::CommandExt as _;

        std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .process_group(0)
            // Null, not inherited: under `cargo test ... | <anything>` the
            // harness's stdout is a pipe, and a child that outlives a failing
            // case holds it open, turning the assertion into a hang.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a test host can spawn a shell")
            .id()
    }

    /// The app an adopted sheep in these cases runs, respawnable for real:
    /// `mutate` gets it before it is normalized onto the entry.
    #[cfg(unix)]
    fn respawnable(mutate: impl FnOnce(&mut AppConfig)) -> impl FnOnce(&mut ProcessEntry) {
        move |entry: &mut ProcessEntry| {
            let mut app = AppConfig::minimal("web", "/bin/sh");
            // Real: one case lets the successor respawn the sheep and asserts
            // on the pid it comes back with. `./srv` would land in `Errored`.
            app.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
            mutate(&mut app);
            entry.spec = normalize(app).unwrap();
        }
    }

    /// Polls the flock until `done` accepts it, or fails the case.
    ///
    /// Real children and real signals, so the clock is real too: nothing here
    /// can advance a paused one on the child's behalf. The bound sits past
    /// anything these cases ask for, so a stall fails rather than hangs.
    #[cfg(unix)]
    async fn flock_until(
        sup: &SupervisorHandle,
        done: impl Fn(&[ProcessInfo]) -> bool,
        what: &str,
    ) -> Vec<ProcessInfo> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let info = sup.list().await;
                if done(&info) {
                    return info;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"))
    }

    /// The ladder ran inside the predecessor's sheep task and the `execve`
    /// took it, so the sheep reaching a terminal status is what proves the
    /// re-arm. `decide_on_exit` reads the marker as `manual_stop`, and this
    /// app has `autorestart` on, so a ladder armed without it would kill the
    /// sheep and then respawn it. A real child and the real runner: only the
    /// reaper's `Msg::Exited` proves a carried marker reaches `handle_exited`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_manual_stop_stops_the_sheep_instead_of_respawning_it() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let pid = adoptable_child("sleep 30");
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_marked(
                    "web",
                    7,
                    Some(pid),
                    false,
                    Some(PendingManual {
                        kind: ManualKind::Stop,
                        origin: CommandOrigin::Operator,
                    }),
                    false,
                    respawnable(|app| app.autorestart = true),
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let info = flock_until(
            &sup,
            |info| info[0].status == ProcStatus::Stopped,
            "a carried manual stop must still stop the sheep after the exec",
        )
        .await;

        assert_eq!(
            info[0].restarts, 0,
            "a stop must not come back as a respawn"
        );
        assert_eq!(
            info[0].last_exit,
            Some(ExitInfo {
                code: None,
                signal: Some(15)
            }),
            "the re-armed ladder starts at its polite rung, not at SIGKILL"
        );
        assert_eq!(info[0].pid, None, "a stopped sheep holds no pid");
    }

    /// The escalation is a `tokio::time::timeout` inside `kill_process`, which
    /// runs on the sheep task the `execve` takes. A successor that re-sends
    /// only the polite signal leaves a child that traps `SIGTERM` running
    /// forever. The signal in `last_exit` is the whole assertion: a pid check
    /// cannot tell a ladder that escalated from one that got lucky.
    /// `kill_timeout` is shortened to a second, well inside [`flock_until`].
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_manual_stop_still_escalates_to_sigkill() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        // A loop, not a bare `sleep`: the polite rung reaches the whole group,
        // so a shell waiting on one `sleep` would exit carrying that sleep's
        // status and look like it had obeyed. The touchfile closes the startup
        // race, since a `SIGTERM` arriving before `trap` runs kills the shell.
        let armed = dir.path().join("trap-armed");
        // Bounded, so a case that fails before the ladder reaches it still
        // leaves a child that goes away inside `flock_until`'s bound.
        let pid = adoptable_child(&format!(
            "trap '' TERM; : > {}; i=0; while [ $i -lt 60 ]; do sleep 1; i=$((i+1)); done",
            armed.display()
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            while !armed.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the defiant child must arm its trap before anything signals it");
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_marked(
                    "web",
                    7,
                    Some(pid),
                    false,
                    Some(PendingManual {
                        kind: ManualKind::Stop,
                        origin: CommandOrigin::Operator,
                    }),
                    false,
                    respawnable(|app| {
                        app.autorestart = false;
                        app.kill_timeout = "1000".parse().unwrap();
                    }),
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let info = flock_until(
            &sup,
            |info| info[0].status == ProcStatus::Stopped,
            "a sheep carried mid-ladder must still die on its own, without a hand-sent SIGKILL",
        )
        .await;

        assert_eq!(
            info[0].last_exit,
            Some(ExitInfo {
                code: None,
                signal: Some(9)
            }),
            "the ladder must escalate: this child ignores SIGTERM, so anything else means it \
             was never killed"
        );
    }

    /// Kind and origin are two separate facts on one marker and neither may be
    /// defaulted: a hardcoded `Stop` leaves the sheep down when an operator
    /// asked for it back, and a hardcoded `Operator` broadcasts a memory
    /// breach as a user action. `Automatic` here, so the flag can come back
    /// `false`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_manual_restart_respawns_and_keeps_its_origin() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = crate::bus::test_bus(64);
        let pid = adoptable_child("sleep 30");
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_marked(
                    "web",
                    7,
                    Some(pid),
                    false,
                    Some(PendingManual {
                        kind: ManualKind::Restart,
                        origin: CommandOrigin::Automatic,
                    }),
                    false,
                    // Off, so the respawn below can only be the carried
                    // `Restart`: an `autorestart` app would come back anyway.
                    respawnable(|app| app.autorestart = false),
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let manually = tokio::time::timeout(
            Duration::from_secs(20),
            await_event(&mut rx, 7, ProcessEventKind::Restart),
        )
        .await
        .expect("a carried manual restart must respawn the sheep after the exec");
        assert!(
            !manually,
            "an automatic restart carried across a handover must not be broadcast as a user \
             action"
        );

        let info = flock_until(
            &sup,
            |info| info[0].restarts == 1,
            "the respawn must be counted against the sheep that was carried",
        )
        .await;
        assert_ne!(
            info[0].pid,
            Some(pid),
            "a restart is a new process, not the adopted one"
        );
        assert_eq!(
            info[0].status,
            ProcStatus::Online,
            "an ungated app is Online the moment it respawns"
        );

        // The respawn is a real `sleep 30`; the shutdown stops it being left
        // behind when the tempdir goes.
        sup.shutdown().await;
    }

    /// Counters are restored before any slot is installed. A successor that
    /// starts `next_id` at zero hands a new sheep an id the predecessor
    /// already gave out.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn the_successor_does_not_reissue_a_live_id() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried("web", 7, Some(4242), |_| {}))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let fresh = sup
            .start(vec![normalize(AppConfig::minimal("api", "./srv")).unwrap()])
            .await
            .expect("a fresh app starts under a successor");

        assert!(fresh[0].id >= 9, "reissued a live id: {}", fresh[0].id);
    }

    /// `schedule_restart`'s timer task dies with the process image, and
    /// `handle_restart_due` is the only thing that moves a sheep off
    /// [`ProcStatus::WaitingRestart`]. The pid is the assertion, not the
    /// status: [`STAND_IN_SPAWN_PID`] is what [`AdoptingRunner`] hands a fresh
    /// spawn and is no carried sheep's pid, so a row reporting it can only
    /// have been respawned by the re-armed timer.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_adopted_sheep_owed_a_restart_still_gets_one() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried("web", 7, None, |entry| {
                    entry.status = ProcStatus::WaitingRestart;
                }))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        // Virtual, and instant: the paused clock advances itself once every
        // task is idle, so this waits out the re-armed backoff for free.
        tokio::time::sleep(Duration::from_secs(5)).await;

        let info = sup.list().await;
        assert_eq!(
            info[0].status,
            ProcStatus::Online,
            "a sheep owed a respawn was left waiting for a timer that died with the exec: {info:?}"
        );
        assert_eq!(
            info[0].pid,
            Some(STAND_IN_SPAWN_PID),
            "the restart must be a real respawn, not a status edit: {info:?}"
        );
    }

    /// Both sheep are paced by a fixed hour and owed a respawn. One carries a
    /// due moment two seconds out, the other carries none, and they share one
    /// paused clock and one advance: only the pair tells "it waited out what
    /// was left" from "it did not wait at all". Ten virtual seconds is five
    /// times the carried deadline and a fraction of the hour, so each status
    /// after it can only have come from one of the two behaviours.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_adopted_sheep_waits_out_only_what_is_left_of_its_delay() {
        let hour = "1h".parse().unwrap();
        let anchored_dir = tempfile::tempdir().unwrap();
        let (anchored_events, _anchored_rx) = crate::bus::test_bus(64);
        let anchored =
            SupervisorBuilder::new(AdoptingRunner, test_paths(&anchored_dir), anchored_events)
                .spawn_adopted(
                    vec![without_handles(carried_owed_a_restart(
                        "web",
                        7,
                        hour,
                        Some(SystemTime::now() + Duration::from_secs(2)),
                    ))],
                    counters(9),
                    Vec::new(),
                )
                .expect("a carried flock installs");

        let control_dir = tempfile::tempdir().unwrap();
        let (control_events, _control_rx) = crate::bus::test_bus(64);
        let control =
            SupervisorBuilder::new(AdoptingRunner, test_paths(&control_dir), control_events)
                .spawn_adopted(
                    vec![without_handles(carried_owed_a_restart(
                        "web", 7, hour, None,
                    ))],
                    counters(9),
                    Vec::new(),
                )
                .expect("a carried flock installs");

        tokio::time::sleep(Duration::from_secs(10)).await;

        let anchored = anchored.list().await;
        assert_eq!(
            anchored[0].status,
            ProcStatus::Online,
            "a sheep two seconds from its own due time must not be made to wait another hour by \
             the handover: {anchored:?}"
        );
        assert_eq!(
            anchored[0].pid,
            Some(STAND_IN_SPAWN_PID),
            "the restart must be a real respawn, not a status edit: {anchored:?}"
        );

        let control = control.list().await;
        assert_eq!(
            control[0].status,
            ProcStatus::WaitingRestart,
            "with no carried deadline the whole delay is re-armed, so an hourly app must still \
             be waiting ten seconds in — a control that respawned would mean the case above \
             proved nothing: {control:?}"
        );
    }

    /// Drives a real exit through `decide_on_exit`, the only thing that writes
    /// the moment. A window, not an equality: the deadline is wall-clock,
    /// since a monotonic instant cannot cross an `execve`, and a paused tokio
    /// clock does not pause the wall clock.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_snapshot_of_a_waiting_sheep_carries_the_moment_its_respawn_falls_due() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::const_exit(1)]);
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        // An hour, so the sheep is still waiting when the snapshot is taken
        // rather than racing its own respawn.
        app.restart_delay = Some("1h".parse().unwrap());
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        // The sheep starts `Online`, so waiting for `WaitingRestart` stops
        // this passing on the state before the crash.
        let waits = async {
            while handle.list().await[0].status != ProcStatus::WaitingRestart {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(30), waits)
            .await
            .expect("an app that exits immediately with autorestart on must reach WaitingRestart");

        let (_candidates, blob, _parked) = handle.handover_snapshot(fds).await.unwrap();

        let due = blob.sheep()[0]
            .restart_due()
            .expect("a sheep owed a respawn must carry the moment it is owed at");
        let left = due
            .duration_since(SystemTime::now())
            .expect("a deadline an hour out has not passed");
        assert!(
            left > Duration::from_secs(3595) && left <= Duration::from_secs(3600),
            "the carried moment must be this sheep's own exit plus its own delay: {left:?}"
        );
    }

    /// The successor has to restore the moment onto its own slot, not merely
    /// read it on the way past. An absolute moment keeps the chain flat: a
    /// carried remainder would have each hop add its own handover duration
    /// back on, which is the drift the window asserts is absent.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_second_handover_inside_one_delay_still_names_the_original_moment() {
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let (events, _rx) = crate::bus::test_bus(64);
        let due = SystemTime::now() + Duration::from_secs(3000);
        let handle = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_owed_a_restart(
                    "web",
                    7,
                    "1h".parse().unwrap(),
                    Some(due),
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        let (_candidates, blob, _parked) = handle.handover_snapshot(fds).await.unwrap();

        let carried = blob.sheep()[0]
            .restart_due()
            .expect("a successor must hand its own successor the moment it was given");
        let drift = carried
            .duration_since(due)
            .or_else(|_| due.duration_since(carried))
            .unwrap();
        assert!(
            drift < Duration::from_secs(1),
            "the second hop must name the same moment as the first, not a fresh one: {drift:?} \
             of drift"
        );
    }

    // --- a swap in flight, carried across the exec ----------------------

    /// A serial drain is what holds still long enough to be snapshotted: a
    /// `readiness_probe` with no `reuse_port` is the one arrangement
    /// `ReloadMode::of` sends down the serial ordering, and
    /// [`ProcScript::never_reports_its_exit`] models the child a kill ladder
    /// cannot end, so the swap stays in [`ReloadPhase::DrainFirst`].
    ///
    /// A real clock, unlike its neighbours: a paused one auto-advances
    /// whenever every task is idle, and the awaits inside `handover_snapshot`
    /// are such a window.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_snapshot_taken_mid_swap_carries_the_job_and_the_markers() {
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_reports_its_exit(); 2]);
        let dir = tempfile::tempdir().unwrap();
        let (fds, _held) = daemon_fds(&dir);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        let mut app = AppConfig::minimal("web", "./srv");
        app.listen_timeout = UpDuration::from_millis(200);
        app.readiness_probe = Some(probe_config(ProbeKind::Tcp, "127.0.0.1:9"));
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // A probed app is `Starting` until its probe answers or
        // `listen_timeout` elapses, and `reload_eligible` refuses anything not
        // serving, so without this the reload skips the only instance there is.
        let online = loop {
            let info = handle.list().await;
            if info[0].status == ProcStatus::Online {
                break info;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let old_id = online[0].id;

        handle
            .reload(ProcessSelector::All)
            .await
            .expect("an online app reloads");

        let (_candidates, blob, _parked) = handle.handover_snapshot(fds).await.unwrap();

        assert_eq!(
            blob.reloads(),
            &[CarriedReload {
                app: "web".to_owned(),
                queue: Vec::new(),
                mode: ReloadMode::Serial,
                swap: ReloadSwap {
                    old_id,
                    new_id: None,
                    phase: ReloadPhase::DrainFirst,
                },
            }],
            "the job the successor continues has to be in the blob, whole"
        );
        assert_eq!(
            blob.sheep()[0].reload(),
            Some(ReloadState::Drainee { new_id: None }),
            "and the marker that routes this instance's exit with it"
        );
    }

    /// A carried sheep that is half of a swap, with the role and the status
    /// the predecessor's own entry carried.
    #[cfg(unix)]
    fn carried_in_swap(
        name: &str,
        id: u32,
        pid: Option<u32>,
        role: ReloadState,
        status: ProcStatus,
        manual: Option<PendingManual>,
        mutate: impl FnOnce(&mut ProcessEntry),
    ) -> CarriedSheep {
        carried_marked(name, id, pid, false, manual, false, move |entry| {
            mutate(entry);
            entry.reload = role;
            entry.status = status;
        })
    }

    /// One app's in-flight reload, as a blob carries it, with an empty queue.
    ///
    /// Empty because a queue behind the swap would let a case pass on the next
    /// instance's swap rather than on the carried one.
    #[cfg(unix)]
    fn carried_job(
        app: &str,
        mode: ReloadMode,
        old_id: u32,
        new_id: Option<u32>,
        phase: ReloadPhase,
    ) -> CarriedReload {
        CarriedReload {
            app: app.to_owned(),
            queue: Vec::new(),
            mode,
            swap: ReloadSwap {
                old_id,
                new_id,
                phase,
            },
        }
    }

    /// The watchdog is a `tokio::spawn`ed sleep of the predecessor's that the
    /// `execve` takes. With the job restored and no timer over it,
    /// `handle_reload` refuses on the map key forever, and the refusal is
    /// whole-selector, so `shep reload all` goes with it. That refusal is the
    /// assertion both ways. Nothing here ever exits, since
    /// [`AdoptingRunner`]'s `wait` never resolves, so only a timer this image
    /// armed can produce the second half.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_carried_swap_that_cannot_finish_is_still_abandoned_on_time() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![
                    without_handles(carried_in_swap(
                        "web",
                        7,
                        Some(4242),
                        ReloadState::Drainee { new_id: Some(8) },
                        ProcStatus::Stopping,
                        None,
                        |_| {},
                    )),
                    without_handles(carried_in_swap(
                        "web",
                        8,
                        Some(4243),
                        ReloadState::Replacement,
                        ProcStatus::Online,
                        None,
                        |_| {},
                    )),
                ],
                counters(9),
                vec![carried_job(
                    "web",
                    ReloadMode::Overlap,
                    7,
                    Some(8),
                    ReloadPhase::DrainOld,
                )],
            )
            .expect("a carried flock installs");

        let refused = sup.reload(ProcessSelector::All).await;
        assert!(
            matches!(refused, Err(SupervisorError::ReloadInFlight(ref name)) if name == "web"),
            "the carried job must be in the map, or this case proves nothing: {refused:?}"
        );

        // Parking on `recv` advances the paused clock, so this waits out the
        // 16s watchdog for free. The bound is load-bearing: with no timer
        // armed the clock has nothing to advance to, so an unbounded wait
        // hangs the suite instead of failing the case.
        tokio::time::timeout(
            Duration::from_secs(3600),
            await_event(&mut rx, 8, ProcessEventKind::ReloadAbandoned),
        )
        .await
        .expect("a carried swap must still be bounded by a watchdog this image armed");

        sup.reload(ProcessSelector::All)
            .await
            .expect("once the watchdog has ended the job, the app must be reloadable again");
    }

    /// A snapshot cannot produce one, but the blob is a file, the same
    /// residual `refuse_repeated_fds` guards on the descriptor side. Arming a
    /// watchdog against an absent entry panics, so a build without the guard
    /// fails the reload below by aborting.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_carried_reload_naming_no_registered_instance_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried("web", 7, Some(4242), |_| {}))],
                counters(9),
                vec![carried_job(
                    "web",
                    ReloadMode::Overlap,
                    98,
                    Some(99),
                    ReloadPhase::DrainOld,
                )],
            )
            .expect("a carried flock installs");

        sup.reload(ProcessSelector::All)
            .await
            .expect("a job naming nothing must be dropped, not left refusing every later reload");
    }

    /// `spawn_verify_task` is a task of the predecessor's too, so a successor
    /// that re-arms only the watchdog abandons a deploy that worked: the
    /// replacement serves on, but `Reloaded` never fires and the rest of a
    /// clustered app's queue is dropped.
    ///
    /// A real listener and a real clock. The probe answers in microseconds
    /// where the watchdog is 16s, so the bound below separates the two by an
    /// order of magnitude whatever the host's speed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_swap_in_verify_is_asked_again_rather_than_abandoned() {
        let probe_target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe_target.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = crate::bus::test_bus(64);
        // Bound, never used: dropping the handle would take a sender off the
        // actor's mailbox.
        let _sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_in_swap(
                    "web",
                    8,
                    Some(4243),
                    ReloadState::Replacement,
                    ProcStatus::Online,
                    None,
                    move |entry| {
                        let mut app = AppConfig::minimal("web", "./srv");
                        app.autorestart = false;
                        // `Probe` readiness alone takes the serial ordering,
                        // which has no post-drain probe to re-arm. `reuse_port`
                        // puts this app on the overlapping one.
                        app.reuse_port = true;
                        app.readiness_probe = Some(probe_config(ProbeKind::Tcp, &addr.to_string()));
                        entry.spec = normalize(app).unwrap();
                    },
                ))],
                counters(9),
                // No drainee: `Verify` is entered once the drainee is reaped,
                // so the re-ask happens with one process left.
                vec![carried_job(
                    "web",
                    ReloadMode::Overlap,
                    7,
                    Some(8),
                    ReloadPhase::Verify,
                )],
            )
            .expect("a carried flock installs");

        tokio::time::timeout(
            Duration::from_secs(5),
            await_event(&mut rx, 8, ProcessEventKind::Reloaded),
        )
        .await
        .expect("a carried swap in Verify must be probed again, not left to its watchdog");
    }

    /// [`ReloadPhase::DrainFirst`] is the one phase with no replacement yet:
    /// `Drainee { new_id: None }` routes the exit to `reap_drainee` and so to
    /// `spawn_serial_replacement`, not to `decide_on_exit`. A successor that
    /// dropped it deregisters a `Stopping` sheep and leaves the slot empty
    /// with the job still in the map. A real child and the real runner: only
    /// the reaper's `Msg::Exited` proves a carried marker reaches
    /// `handle_exited`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_serial_drain_still_spawns_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let pid = adoptable_child("sleep 30");
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_in_swap(
                    "web",
                    7,
                    Some(pid),
                    ReloadState::Drainee { new_id: None },
                    ProcStatus::Stopping,
                    Some(PendingManual {
                        kind: ManualKind::Stop,
                        origin: CommandOrigin::Operator,
                    }),
                    swappable,
                ))],
                counters(9),
                vec![carried_job(
                    "web",
                    ReloadMode::Serial,
                    7,
                    None,
                    ReloadPhase::DrainFirst,
                )],
            )
            .expect("a carried flock installs");

        let info = flock_until(
            &sup,
            |info| info.len() == 1 && info[0].id != 7,
            "a carried serial drain must spawn its replacement once the instance it drained goes",
        )
        .await;

        assert_eq!(
            info[0].id, 9,
            "the replacement takes the next carried id, not a reissued one"
        );
        assert_eq!(
            info[0].restarts, 0,
            "a reload is not a restart, so the count carries across the swap unchanged"
        );
        assert_ne!(info[0].pid, Some(pid), "the replacement is a new process");

        sup.shutdown().await;
    }

    /// The readiness wait, the `Replacement` marker and the job all have to
    /// survive. Without the wait the replacement sits `Starting` forever
    /// beside a drainee still serving: two live instances of a one-instance
    /// app. Without the marker or the job, `handle_ready_result` takes the
    /// ordinary path and the swap never commits; the `manually` flag below is
    /// that route's fingerprint, since `spawn_replacement` passes `true` and
    /// an adopted sheep that is not a replacement is armed with `false`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_replacement_awaiting_readiness_commits_its_swap() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = crate::bus::test_bus(64);
        let drainee = adoptable_child("sleep 30");
        let replacement = adoptable_child("sleep 30");
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![
                    without_handles(carried_in_swap(
                        "web",
                        7,
                        Some(drainee),
                        ReloadState::Drainee { new_id: Some(8) },
                        ProcStatus::Stopping,
                        // No marker: an overlapping swap does not ask the
                        // drainee to go until its replacement is serving.
                        None,
                        swappable,
                    )),
                    without_handles(carried_in_swap(
                        "web",
                        8,
                        Some(replacement),
                        ReloadState::Replacement,
                        ProcStatus::Starting,
                        None,
                        swappable,
                    )),
                ],
                counters(9),
                vec![carried_job(
                    "web",
                    ReloadMode::Overlap,
                    7,
                    Some(8),
                    ReloadPhase::AwaitReady,
                )],
            )
            .expect("a carried flock installs");

        let manually = tokio::time::timeout(
            Duration::from_secs(20),
            await_event(&mut rx, 8, ProcessEventKind::Online),
        )
        .await
        .expect("a carried replacement must still resolve its readiness after the exec");
        assert!(
            manually,
            "a replacement's Online is an operator's doing; reporting otherwise broadcasts a \
             deploy as the daemon's own"
        );

        let info = flock_until(
            &sup,
            |info| info.len() == 1,
            "committing the swap must drain the instance it replaced",
        )
        .await;
        assert_eq!(info[0].id, 8, "the replacement is what is left: {info:?}");
        assert_eq!(info[0].pid, Some(replacement));

        sup.shutdown().await;
    }

    /// [`ReloadPhase::DrainOld`] is the committed phase: the replacement is
    /// serving and the instance it replaced is on its ladder. The `Drainee`
    /// marker sends that instance's exit to `reap_drainee` rather than to
    /// `decide_on_exit`, and this app has `autorestart` on, so a successor
    /// that dropped the marker would respawn the old code into an instance
    /// slot the replacement owns.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_drainee_still_finishes_its_swap() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = crate::bus::test_bus(64);
        let drainee = adoptable_child("sleep 30");
        let replacement = adoptable_child("sleep 30");
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![
                    without_handles(carried_in_swap(
                        "web",
                        7,
                        Some(drainee),
                        ReloadState::Drainee { new_id: Some(8) },
                        ProcStatus::Stopping,
                        Some(PendingManual {
                            kind: ManualKind::Stop,
                            origin: CommandOrigin::Operator,
                        }),
                        |entry| swappable_with(entry, |app| app.autorestart = true),
                    )),
                    without_handles(carried_in_swap(
                        "web",
                        8,
                        Some(replacement),
                        ReloadState::Replacement,
                        ProcStatus::Online,
                        None,
                        |entry| swappable_with(entry, |app| app.autorestart = true),
                    )),
                ],
                counters(9),
                vec![carried_job(
                    "web",
                    ReloadMode::Overlap,
                    7,
                    Some(8),
                    ReloadPhase::DrainOld,
                )],
            )
            .expect("a carried flock installs");

        tokio::time::timeout(
            Duration::from_secs(20),
            await_event(&mut rx, 8, ProcessEventKind::Reloaded),
        )
        .await
        .expect("a carried drainee's exit must finish the swap it was half of");

        let info = sup.list().await;
        assert_eq!(
            info.len(),
            1,
            "the drainee is deregistered by the swap, never respawned: {info:?}"
        );
        assert_eq!(info[0].id, 8);
        assert_eq!(info[0].pid, Some(replacement));

        sup.shutdown().await;
    }

    /// The cap is not recorded anywhere, so it is derived from the role: both
    /// sites that pass `LadderCap::Drain` leave the `Drainee` marker on the
    /// entry. The child ignores `SIGTERM`, so only the escalation ends it, and
    /// the two timeouts are three orders of magnitude apart: under the drain's
    /// cap the `SIGKILL` lands a quarter of a second in, under the stop's five
    /// minutes later, past [`flock_until`]'s bound.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_drainee_is_capped_by_graceful_timeout_not_kill_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let drainee = adoptable_child("trap '' TERM; sleep 300");
        let replacement = adoptable_child("sleep 30");
        let capped = |entry: &mut ProcessEntry| {
            swappable_with(entry, |app| {
                app.graceful_timeout = UpDuration::from_millis(250);
                app.kill_timeout = UpDuration::from_millis(300_000);
            });
        };
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![
                    without_handles(carried_in_swap(
                        "web",
                        7,
                        Some(drainee),
                        ReloadState::Drainee { new_id: Some(8) },
                        ProcStatus::Stopping,
                        Some(PendingManual {
                            kind: ManualKind::Stop,
                            origin: CommandOrigin::Operator,
                        }),
                        capped,
                    )),
                    without_handles(carried_in_swap(
                        "web",
                        8,
                        Some(replacement),
                        ReloadState::Replacement,
                        ProcStatus::Online,
                        None,
                        capped,
                    )),
                ],
                counters(9),
                vec![carried_job(
                    "web",
                    ReloadMode::Overlap,
                    7,
                    Some(8),
                    ReloadPhase::DrainOld,
                )],
            )
            .expect("a carried flock installs");

        flock_until(
            &sup,
            |info| info.len() == 1,
            "a carried drainee must escalate at graceful_timeout, not at kill_timeout",
        )
        .await;

        sup.shutdown().await;
    }

    /// An abandoned reload leaves its replacement `Starting`, and a reload
    /// replaces `Online` instances, so `SheepSlot::ready_failed` is the whole
    /// of what keeps the leftover reachable. Asserted through a real reload
    /// rather than by reading the slot back: `handle_reload` replies `Ok` with
    /// the row in it either way, before its selector pass has run.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_carried_ready_failed_instance_is_still_replaceable() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let pid = adoptable_child("sleep 30");
        let sup = SupervisorBuilder::new(TokioRunner::new(), test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_ready_failed(
                    "web",
                    7,
                    Some(pid),
                    |entry| {
                        swappable(entry);
                        entry.status = ProcStatus::Starting;
                    },
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        sup.reload(ProcessSelector::All)
            .await
            .expect("a registered app is one a reload can name");

        let info = flock_until(
            &sup,
            |info| info.len() == 1 && info[0].id != 7,
            "a carried `ready_failed` instance must still be replaceable by the reload that \
             rolls its release back",
        )
        .await;

        assert_eq!(
            info[0].id, 9,
            "the replacement takes the next carried id, not a reissued one"
        );
        assert_ne!(info[0].pid, Some(pid), "the replacement is a new process");

        sup.shutdown().await;
    }

    /// Both sheep are `Starting` and only the carried flag tells them apart:
    /// an ordinary one is mid-wait and owed a fresh one, this one's wait
    /// already ran and failed. `handle_ready_result`'s `TimedOut` arm goes
    /// `Online` anyway and `went_online` clears `ready_failed` on its way
    /// past, so arming one would report an abandoned release as serving. The
    /// clock runs past the app's `listen_timeout`, so any wait that was armed
    /// has fired by the time the status is read.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_carried_ready_failed_instance_gets_no_fresh_readiness_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let sup = SupervisorBuilder::new(AdoptingRunner, test_paths(&dir), events)
            .spawn_adopted(
                vec![without_handles(carried_ready_failed(
                    "web",
                    7,
                    Some(4242),
                    |entry| {
                        let mut app = AppConfig::minimal("web", "./srv");
                        app.autorestart = false;
                        app.wait_ready = true;
                        entry.spec = normalize(app).unwrap();
                        entry.status = ProcStatus::Starting;
                    },
                ))],
                counters(9),
                Vec::new(),
            )
            .expect("a carried flock installs");

        tokio::time::sleep(Duration::from_secs(120)).await;

        assert_eq!(
            sup.list().await[0].status,
            ProcStatus::Starting,
            "an instance whose readiness already failed must not be handed a second verdict by \
             the successor that adopted it"
        );
    }

    /// The app a carried swap's two halves run: a real `sleep`, so a
    /// replacement can be spawned and a drainee signalled, with a
    /// `listen_timeout` short enough for a real-clock case.
    #[cfg(unix)]
    fn swappable(entry: &mut ProcessEntry) {
        swappable_with(entry, |_| {});
    }

    /// [`swappable`], with `mutate` free to change the app first.
    #[cfg(unix)]
    fn swappable_with(entry: &mut ProcessEntry, mutate: impl FnOnce(&mut AppConfig)) {
        let mut app = AppConfig::minimal("web", "/bin/sh");
        app.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
        app.autorestart = false;
        app.listen_timeout = UpDuration::from_millis(200);
        mutate(&mut app);
        entry.spec = normalize(app).unwrap();
    }

    // --- `ApplyConfig`: a Flockfile merged onto a running flock ---
    //
    // Actor-tier, all but one: a load leaves behind `spec`, `pending` and a pid
    // that must not have moved, and no reply reports those three together.

    /// The pid the first fixture instance carries. Distinctive, so a case
    /// asserting the child was left alone cannot be reading a default.
    const APPLY_FIRST_PID: u32 = 7100;

    /// An actor over one `Online` instance of each app, plus the recording
    /// enforcer its extras are armed against.
    ///
    /// One slot per instance the config declares, ids in order. The extras are
    /// real but nothing is armed, since no instance went through
    /// `went_online`. Every slot is `Online` with a pid and no `ctl`, so a
    /// stopped instance written onto the entry is removed synchronously.
    fn actor_over(
        dir: &tempfile::TempDir,
        apps: &[ResolvedApp],
    ) -> (Actor<ScriptedRunner>, Arc<RecordingEnforcer>) {
        let paths = test_paths(dir);
        let mut sheep = HashMap::new();
        let mut next_id = 0;
        for app in apps {
            for instance in 0..app.config().instances {
                let id = next_id;
                next_id += 1;
                sheep.insert(
                    id,
                    SheepSlot {
                        entry: armed_entry(id, instance, APPLY_FIRST_PID + id, app.clone(), &paths),
                        ctl: None,
                        log_ctl: None,
                        to_child: None,
                        signals: None,
                        to_stdin: None,
                        manual: None,
                        pending_delete: false,
                        epoch: 0,
                        ready_tx: None,
                        actions: ActionWaits::default(),
                        ready_failed: false,
                        restart_due: None,
                    },
                );
            }
        }
        let enforcer = Arc::new(RecordingEnforcer::default());
        let (breach_tx, _breaches) = mpsc::channel(1);
        let (live_tx, _liveness) = mpsc::channel(1);
        let extras = Extras {
            clock: Arc::new(SystemClock),
            enforcer: Arc::clone(&enforcer) as Arc<dyn LimitEnforcer>,
            max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
            reports: ExtrasReports {
                breaches: breach_tx,
                liveness: live_tx,
            },
            stats: idle_stats(),
        };
        let (events, _events_rx) = crate::bus::test_bus(64);
        let (tx, _rx) = mpsc::channel(MAILBOX_CAPACITY);
        let actor = Actor {
            // Enough scripts for a scale-up to come up: without them a case
            // that scales would assert on a shortfall rather than the apply.
            runner: ScriptedRunner::new(vec![ProcScript::never_exits(); 4]),
            paths,
            events,
            tx,
            sheep,
            next_id,
            next_deadline: 0,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: Some(extras),
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        };
        (actor, enforcer)
    }

    /// A [`DeclaredApp`] whose document wrote exactly `keys`, plus every key
    /// of its own `env` table when `keys` names `env`.
    fn declared_app(config: AppConfig, keys: &[&str]) -> DeclaredApp {
        let declared: BTreeSet<String> = keys.iter().map(|key| (*key).to_string()).collect();
        let declared_env = if declared.contains("env") {
            config.env.keys().cloned().collect()
        } else {
            BTreeSet::new()
        };
        DeclaredApp {
            config,
            declared,
            declared_env,
        }
    }

    /// The override record an earlier load of `keys` would have left, with
    /// `fields` set since by an operator.
    fn established(
        keys: &[&str],
        fields: Vec<(&str, serde_json::Value)>,
    ) -> shep_core::overrides::AppOverrides {
        shep_core::overrides::AppOverrides {
            fields: fields
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
            declared: keys.iter().map(|key| (*key).to_string()).collect(),
            declared_env: BTreeSet::new(),
        }
    }

    /// `handle_command` answers before it returns, so the `await` here
    /// resolves rather than hopes.
    async fn apply_config(
        actor: &mut Actor<ScriptedRunner>,
        apps: Vec<DeclaredApp>,
        reset: ResetDepth,
    ) -> Vec<Applied> {
        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::ApplyConfig { apps, reset, reply });
        answer
            .await
            .expect("the actor answers an apply before it returns")
            .expect("the fixture flock is registered")
    }

    /// Additive is the default because a Flockfile arrives from the app's own
    /// repository: a merged pull request must not change a running flock.
    #[tokio::test(start_paused = true)]
    async fn a_file_load_does_not_overwrite_an_established_key() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) =
            actor_over(&dir, &[app_with("web", |app| app.max_restarts = 3)]);
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(
                &["name", "script", "max_restarts"],
                vec![("max_restarts", serde_json::json!(3))],
            ),
        )
        .unwrap();

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_restarts = 99;
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_restarts"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(
            actor.sheep[&0].entry.spec.config().max_restarts,
            3,
            "the file overwrote a key the operator had set"
        );
        assert!(
            reply[0].applied.is_empty(),
            "nothing applied, so nothing may be reported as applied: {reply:?}"
        );
    }

    /// Appending an unestablished key is what makes a template update reach an
    /// app at all.
    #[tokio::test(start_paused = true)]
    async fn a_file_load_appends_a_key_nobody_had_established() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(&["name", "script"], Vec::new()),
        )
        .unwrap();

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_memory = Some(MemSize::from_bytes(512 << 20));
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_memory"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(
            actor.sheep[&0].entry.spec.config().max_memory,
            Some(MemSize::from_bytes(512 << 20)),
            "a key nobody had established must be appended"
        );
        assert_eq!(reply[0].applied, vec!["max_memory".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_field_lands_on_the_stored_spec() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_restarts = 42;
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_restarts"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(actor.sheep[&0].entry.spec.config().max_restarts, 42);
        assert_eq!(reply[0].applied, vec!["max_restarts".to_string()]);
        assert!(reply[0].pending.is_empty(), "{reply:?}");
    }

    /// A load must never kill a process.
    #[tokio::test(start_paused = true)]
    async fn a_needs_respawn_field_parks_as_pending_and_leaves_the_child_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "env"])],
            ResetDepth::None,
        )
        .await;

        let entry = &actor.sheep[&0].entry;
        assert_eq!(
            entry.pid,
            Some(APPLY_FIRST_PID),
            "the running child must not have been replaced"
        );
        assert!(
            entry.spec.config().env.is_empty(),
            "the running child's own config must keep describing what it was spawned from"
        );
        assert_eq!(
            entry
                .pending
                .as_ref()
                .expect("a NeedsRespawn change parks as pending")
                .config()
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue")
        );
        assert_eq!(reply[0].pending, vec!["env".to_string()]);
        assert!(reply[0].applied.is_empty(), "{reply:?}");
        assert_eq!(
            reply[0]
                .app
                .as_ref()
                .expect("an applied app is recorded")
                .config()
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue"),
            "a reboot spawns everything afresh, so what it comes up on is the \
             full merge and not what the running child is on"
        );
    }

    /// An app whose merge is invalid refuses whole; the rest of the flock
    /// still applies.
    #[tokio::test(start_paused = true)]
    async fn an_unnormalizable_merge_refuses_one_app_and_applies_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) =
            actor_over(&dir, &[app_with("web", |_| {}), app_with("worker", |_| {})]);

        // Two instances sharing one explicit log path, with no `{{instance}}`
        // in it and no `merge_logs`: the one refusal a merge can produce out
        // of two individually-legal keys.
        let mut broken = AppConfig::minimal("web", "./srv");
        broken.instances = 2;
        broken.out_file = Some("/tmp/web.log".to_string());
        let mut worker = AppConfig::minimal("worker", "./srv");
        worker.max_restarts = 7;

        // `Policy`, not `None`: a plain load holds `instances` out of the
        // merge, so the two keys could not meet and the merge would be valid.
        let reply = apply_config(
            &mut actor,
            vec![
                declared_app(broken, &["name", "script", "instances", "out_file"]),
                declared_app(worker, &["name", "script", "max_restarts"]),
            ],
            ResetDepth::Policy,
        )
        .await;

        assert!(
            reply[0].refused.is_some(),
            "an unnormalizable merge must refuse: {reply:?}"
        );
        assert_eq!(
            actor.sheep[&0].entry.spec.config().instances,
            1,
            "a refused app's stored config must be untouched"
        );
        assert!(actor.sheep[&0].entry.spec.config().out_file.is_none());
        assert_eq!(actor.sheep[&1].entry.spec.config().max_restarts, 7);
        assert_eq!(reply[1].applied, vec!["max_restarts".to_string()]);
    }

    /// A load never prunes: the daemon has no record of which Flockfile an app
    /// came from, so `shep start ./a/Flockfile.toml` followed by
    /// `./b/Flockfile.toml` would have the second wipe the first's flock.
    #[tokio::test(start_paused = true)]
    async fn an_app_absent_from_the_file_is_left_running() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) =
            actor_over(&dir, &[app_with("web", |_| {}), app_with("worker", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_restarts = 5;
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_restarts"])],
            ResetDepth::None,
        )
        .await;

        let worker = &actor.sheep[&1].entry;
        assert_eq!(worker.spec.config().name, "worker");
        assert_eq!(worker.status, ProcStatus::Online);
        assert_eq!(worker.pid, Some(APPLY_FIRST_PID + 1));
        assert!(
            reply.iter().all(|applied| applied.name != "worker"),
            "a load must not claim to have touched an app the file never named: {reply:?}"
        );
    }

    /// The drainee holds the lower id, so `ids.first()` reaches the instance on
    /// its way out, and a spec derived from it lands on the live replacement.
    #[tokio::test(start_paused = true)]
    async fn a_load_during_a_reload_reads_the_replacement_and_not_the_drainee() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.instances = 2;
                app.cwd = Some("/srv/new".to_string());
            })],
        );
        // Instance 0 is the drainee: the lower id, still on the config the
        // reload is replacing, and already `Stopping`.
        {
            let slot = actor
                .sheep
                .get_mut(&0)
                .expect("the fixture registered two slots");
            slot.entry.status = ProcStatus::Stopping;
            slot.entry.reload = ReloadState::Drainee { new_id: Some(1) };
            slot.entry.spec = app_with("web", |app| {
                app.instances = 2;
                app.cwd = Some("/srv/old".to_string());
            });
        }

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_restarts = 7;
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_restarts"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(
            actor.sheep[&1].entry.spec.config().cwd.as_deref(),
            Some("/srv/new"),
            "the replacement's spec must keep describing what the replacement \
             was spawned from: {reply:?}"
        );
        assert_eq!(actor.sheep[&1].entry.spec.config().max_restarts, 7);
        assert_eq!(reply[0].applied, vec!["max_restarts".to_string()]);
        assert!(reply[0].pending.is_empty(), "{reply:?}");
    }

    /// A dog runs at the daemon's own trust level, so a file naming one and
    /// carrying a `script` would replace an adopted binary without adopting
    /// anything, while `shep dogs` went on reporting the previous dog.
    #[tokio::test(start_paused = true)]
    async fn a_file_naming_a_dog_is_refused_rather_than_merged() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("metrics", |_| {})]);
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registered one slot")
            .entry
            .dog = Some(DogSource::BuiltIn);

        let mut file = AppConfig::minimal("metrics", "/opt/evil");
        file.max_restarts = 42;
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_restarts"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(
            reply[0].refused.as_deref(),
            Some(
                "metrics is a dog, and a dog's config comes from `shep adopt` rather than \
                 from a Flockfile"
            ),
            "{reply:?}"
        );
        let entry = &actor.sheep[&0].entry;
        assert_eq!(entry.spec.config().script, "./srv");
        assert_eq!(
            entry.spec.config().max_restarts,
            AppConfig::default().max_restarts
        );
        assert!(entry.pending.is_none(), "a refused app parks nothing");
        assert!(
            shep_core::overrides::get(&actor.paths.overrides, "metrics")
                .unwrap()
                .is_none(),
            "a refused app establishes nothing"
        );
    }

    /// A reset resolves an undeclared key to the file as loaded, not to the
    /// compiled default. The CLI defaults `cwd` to the Flockfile's own
    /// directory without the document declaring it, so the compiled default
    /// would park `cwd: None` and the next restart could not find the script.
    /// `fold` and `interpreter` arrive the same way.
    #[tokio::test(start_paused = true)]
    async fn a_reset_against_an_unchanged_file_keeps_a_defaulted_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.cwd = Some("/srv/web".to_string());
                app.fold = Some("edge".to_string());
            })],
        );

        // What `shep start Flockfile.toml --reset` sends for an unmodified
        // two-line file: the resolved config carries the defaulted `cwd` and
        // the `--fold`, and the document declared neither.
        let mut file = AppConfig::minimal("web", "./srv");
        file.cwd = Some("/srv/web".to_string());
        file.fold = Some("edge".to_string());
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script"])],
            ResetDepth::Policy,
        )
        .await;

        // `cwd` is `NeedsRespawn`, so the damage lands in `pending` rather
        // than on the running spec.
        assert!(
            actor.sheep[&0].entry.pending.is_none(),
            "an unchanged file has nothing to park: {reply:?}"
        );
        assert!(reply[0].pending.is_empty(), "{reply:?}");
        let config = actor.sheep[&0].entry.spec.config().clone();
        assert_eq!(config.cwd.as_deref(), Some("/srv/web"));
        assert_eq!(config.fold.as_deref(), Some("edge"));
    }

    /// `--reset=policy` restores settings, declared or not, and leaves env;
    /// `--reset=all` takes env with it and drops the record.
    #[tokio::test(start_paused = true)]
    async fn reset_restores_every_setting_and_only_reset_all_takes_env() {
        // The operator's three edits since the file established name, script
        // and max_restarts: one over a key the file declares, one env key and
        // one field the file has never mentioned.
        let stored = |name: &str| {
            app_with(name, |app| {
                app.max_restarts = 3;
                app.env = BTreeMap::from([("OPERATOR".to_string(), "1".to_string())]);
                app.min_uptime = UpDuration::from_millis(9000);
            })
        };
        let record = || {
            established(
                &["name", "script", "max_restarts"],
                vec![
                    ("max_restarts", serde_json::json!(3)),
                    ("env", serde_json::json!({ "OPERATOR": "1" })),
                    ("min_uptime", serde_json::json!("9000ms")),
                ],
            )
        };
        let file = || {
            let mut file = AppConfig::minimal("web", "./srv");
            file.max_restarts = 10;
            declared_app(file, &["name", "script", "max_restarts"])
        };

        let settings_dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&settings_dir, &[stored("web")]);
        shep_core::overrides::put(&actor.paths.overrides, "web", &record()).unwrap();
        apply_config(&mut actor, vec![file()], ResetDepth::Policy).await;
        let settings = actor.sheep[&0].entry.spec.config().clone();
        let settings_pending = actor.sheep[&0].entry.pending.clone();
        assert_eq!(
            settings.max_restarts, 10,
            "--reset=policy puts a declared setting back to the file's"
        );
        assert_eq!(
            settings.min_uptime,
            AppConfig::default().min_uptime,
            "--reset=policy puts a field the file never declared back to the \
             file's own value, which for an undeclared key is the compiled \
             default"
        );
        assert_eq!(
            settings_pending
                .as_ref()
                .map_or(&settings.env, |app| &app.config().env)
                .get("OPERATOR")
                .map(String::as_str),
            Some("1"),
            "--reset=policy keeps env"
        );

        let all_dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&all_dir, &[stored("web")]);
        shep_core::overrides::put(&actor.paths.overrides, "web", &record()).unwrap();
        apply_config(&mut actor, vec![file()], ResetDepth::All).await;
        let all = actor.sheep[&0].entry.spec.config().clone();
        let all_pending = actor.sheep[&0].entry.pending.clone();
        assert_eq!(all.max_restarts, 10);
        assert_eq!(
            all.min_uptime,
            AppConfig::default().min_uptime,
            "--reset=all drops a field the operator added"
        );
        assert!(
            all_pending
                .as_ref()
                .expect("dropping an env key needs a respawn")
                .config()
                .env
                .is_empty(),
            "--reset=all drops an env key the operator added"
        );
        assert!(
            shep_core::overrides::get(&actor.paths.overrides, "web")
                .unwrap()
                .is_none(),
            "--reset=all removes the override record"
        );
    }

    /// The four-mode fixture: a template declaring `max_restarts` and `env`,
    /// against a sheep whose operator has overridden `max_restarts`, added
    /// `max_memory` (undeclared) and edited the one declared env key.
    ///
    /// Returns the stored config, the template and the override record, in the
    /// order [`merge_declared`] takes them. Every mode is asserted on the same
    /// three values, since the four are not a two-by-two grid.
    fn reset_grid() -> (AppConfig, DeclaredApp, shep_core::overrides::AppOverrides) {
        let mut stored = AppConfig::minimal("web", "./srv");
        stored.max_restarts = 3;
        stored.max_memory = Some(MemSize::from_bytes(2 << 30));
        stored.env = BTreeMap::from([("DB".to_string(), "operator".to_string())]);

        let mut template = AppConfig::minimal("web", "./srv");
        template.max_restarts = 10;
        template.env = BTreeMap::from([("DB".to_string(), "template".to_string())]);
        let incoming = declared_app(template, &["name", "script", "max_restarts", "env"]);

        let mut record = established(
            &["name", "script", "max_restarts", "env"],
            vec![
                ("max_restarts", serde_json::json!(3)),
                ("max_memory", serde_json::json!("2G")),
                ("env", serde_json::json!({ "DB": "operator" })),
            ],
        );
        // The template established `DB` on the load before this one, which is
        // how an operator's later edit to it became an override at all.
        record.declared_env = BTreeSet::from(["DB".to_string()]);
        (stored, incoming, record)
    }

    /// The merged config a mode produces over [`reset_grid`].
    fn merged_over_grid(reset: ResetDepth) -> AppConfig {
        let (stored, incoming, record) = reset_grid();
        merge_declared(&stored, &incoming, &record, reset)
            .expect("the grid fixture travels through serde")
            .0
    }

    /// The override record a mode leaves over [`reset_grid`].
    fn record_over_grid(reset: ResetDepth) -> shep_core::overrides::AppOverrides {
        let (stored, incoming, record) = reset_grid();
        merge_declared(&stored, &incoming, &record, reset)
            .expect("the grid fixture travels through serde")
            .1
    }

    /// Both axes in one test: the mode is the pair, and a test checking one
    /// axis would pass for two different modes.
    #[test]
    fn file_puts_back_what_the_template_declares_and_leaves_the_rest() {
        let merged = merged_over_grid(ResetDepth::File);
        assert_eq!(merged.max_restarts, 10, "a declared key goes back");
        assert_eq!(
            merged.max_memory,
            Some(MemSize::from_bytes(2 << 30)),
            "a key the template never declares is not the template's to reset"
        );
        assert_eq!(
            merged.env.get("DB").map(String::as_str),
            Some("operator"),
            "`file` keeps env"
        );
    }

    #[test]
    fn policy_puts_back_every_setting_declared_or_not_and_keeps_env() {
        let merged = merged_over_grid(ResetDepth::Policy);
        assert_eq!(merged.max_restarts, 10, "a declared key goes back");
        assert_eq!(
            merged.max_memory,
            AppConfig::default().max_memory,
            "`policy` resets a key the template is silent about, which for an \
             undeclared key means the value a fresh start off that template gives it"
        );
        assert_eq!(
            merged.env.get("DB").map(String::as_str),
            Some("operator"),
            "`policy` keeps env"
        );
    }

    /// An operator typing `--reset=env` does not expect their restart budget
    /// put back because the template happens to mention it.
    #[test]
    fn env_resets_env_and_touches_no_setting_at_all() {
        let merged = merged_over_grid(ResetDepth::Env);
        assert_eq!(
            merged.max_restarts, 3,
            "a declared policy field is not `env`'s to reset"
        );
        assert_eq!(
            merged.max_memory,
            Some(MemSize::from_bytes(2 << 30)),
            "and neither is one the template is silent on"
        );
        assert_eq!(
            merged.env.get("DB").map(String::as_str),
            Some("template"),
            "`env` puts env back to the template"
        );
    }

    /// `all` is the widest mode: every setting back, declared or not, and env
    /// with it.
    #[test]
    fn all_puts_back_every_setting_and_env_with_it() {
        let merged = merged_over_grid(ResetDepth::All);
        assert_eq!(merged.max_restarts, 10, "a declared key goes back");
        assert_eq!(
            merged.max_memory,
            AppConfig::default().max_memory,
            "`all` resets a key the template is silent about"
        );
        assert_eq!(
            merged.env.get("DB").map(String::as_str),
            Some("template"),
            "`all` puts env back to the template"
        );
    }

    /// An override is spent exactly where the merge overwrote it, so `file`
    /// spends the declared setting and keeps both the undeclared one and env.
    #[test]
    fn file_spends_only_the_override_it_put_back() {
        let record = record_over_grid(ResetDepth::File);
        let mut held: Vec<&String> = record.fields.keys().collect();
        // Sorted: `serde_json::Map` is insertion-ordered, and this is a set.
        held.sort();
        assert_eq!(
            held,
            vec!["env", "max_memory"],
            "`file` keeps the undeclared override and env, and spends the rest"
        );
    }

    /// Every setting is in scope, so every setting override is spent; env is
    /// untouched, so the env override stands.
    #[test]
    fn policy_spends_every_setting_override_and_keeps_env() {
        let record = record_over_grid(ResetDepth::Policy);
        let mut held: Vec<&String> = record.fields.keys().collect();
        // Sorted: `serde_json::Map` is insertion-ordered, and this is a set.
        held.sort();
        assert_eq!(
            held,
            vec!["env"],
            "`policy` spends both setting overrides and keeps env"
        );
    }

    /// An override is spent exactly where the merge overwrote it, and this mode
    /// overwrites no setting, so both survive. Those record entries keep a
    /// later plain load from appending the template's values over them.
    #[test]
    fn env_spends_only_the_env_override() {
        let record = record_over_grid(ResetDepth::Env);
        let mut held: Vec<&String> = record.fields.keys().collect();
        // Sorted: `serde_json::Map` is insertion-ordered, and this is a set.
        held.sort();
        assert_eq!(
            held,
            vec!["max_memory", "max_restarts"],
            "`env` spends env and keeps every setting override"
        );
    }

    /// Everything is in scope, so nothing is still overridden.
    #[test]
    fn all_spends_every_override() {
        let record = record_over_grid(ResetDepth::All);
        assert!(
            record.fields.is_empty(),
            "`all` holds nothing back: {:?}",
            record.fields.keys().collect::<Vec<_>>()
        );
    }

    /// Resetting a key's value and dropping its record entry are different
    /// operations, and only `all` does both: under `env` the record still holds
    /// `max_memory`, which is what stands between the operator's ceiling and
    /// the next plain load.
    #[tokio::test(start_paused = true)]
    async fn an_env_reset_keeps_the_override_record() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.max_memory = Some(MemSize::from_bytes(2 << 30));
                app.env = BTreeMap::from([("DB".to_string(), "operator".to_string())]);
            })],
        );
        let mut record = established(
            &["name", "script", "env"],
            vec![
                ("max_memory", serde_json::json!("2G")),
                ("env", serde_json::json!({ "DB": "operator" })),
            ],
        );
        record.declared_env = BTreeSet::from(["DB".to_string()]);
        shep_core::overrides::put(&actor.paths.overrides, "web", &record).unwrap();

        let mut file = AppConfig::minimal("web", "./srv");
        file.env = BTreeMap::from([("DB".to_string(), "template".to_string())]);
        let file = declared_app(file, &["name", "script", "env"]);
        let reply = apply_config(&mut actor, vec![file], ResetDepth::Env).await;

        let written = shep_core::overrides::get(&actor.paths.overrides, "web")
            .unwrap()
            .unwrap_or_else(|| panic!("`env` keeps the record: {reply:?}"));
        assert!(
            written.fields.contains_key("max_memory"),
            "the undeclared override must survive an env reset: {reply:?}"
        );
        assert_eq!(
            actor.sheep[&0].entry.overridden,
            vec!["max_memory".to_string()],
            "and `shep flock`'s CFG column must still say so"
        );
    }

    /// The flag widens a load and never narrows one, so the additive default
    /// underneath it still runs. The second assertion keeps that from reading
    /// as an overwrite: `max_restarts` is established, and does not move.
    #[test]
    fn an_env_reset_still_appends_a_key_nobody_established() {
        let (stored, _, record) = reset_grid();
        let mut template = AppConfig::minimal("web", "./srv");
        template.max_restarts = 10;
        template.min_uptime = UpDuration::from_millis(9000);
        template.env = BTreeMap::from([("DB".to_string(), "template".to_string())]);
        let incoming = declared_app(
            template,
            &["name", "script", "max_restarts", "min_uptime", "env"],
        );

        let (merged, _) = merge_declared(&stored, &incoming, &record, ResetDepth::Env)
            .expect("the grid fixture travels through serde");
        assert_eq!(
            merged.min_uptime,
            UpDuration::from_millis(9000),
            "a declared key nobody established is appended under `env` too"
        );
        assert_eq!(
            merged.max_restarts, 3,
            "and an established one is still not overwritten"
        );
    }

    /// `instances` is held out of this depth as it is out of a plain load: the
    /// store cannot tell a stocked count from a count nobody has touched, so
    /// taking the file's would delete instances.
    #[tokio::test(start_paused = true)]
    async fn an_env_reset_never_reshapes_a_flock() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 4)]);
        let mut file = AppConfig::minimal("web", "./srv");
        file.instances = 2;
        let file = declared_app(file, &["name", "script", "instances"]);
        let reply = apply_config(&mut actor, vec![file], ResetDepth::Env).await;
        assert_eq!(
            actor.ids_of_name("web").len(),
            4,
            "`env` scaled a flock: {reply:?}"
        );
        // Exact, not a `contains("instances")`: the sentence an `env` operator
        // reads is the assertion, down to which mode it names and what that
        // mode costs.
        assert_eq!(
            reply[0].refused.as_deref(),
            Some(
                "instances: this load never reshapes a flock; no mode scales without also \
                 putting back every setting the file declares, and `--reset=file` is the \
                 narrowest that does, taking the file's count of 2"
            ),
            "an operator whose count did not move must be told why, and what the \
             mode they are pointed at would cost them: {reply:?}"
        );
    }

    /// An app stocked to four against a template carrying no `instances` line
    /// keeps four. Under `policy` it drops to one, since the compiled default
    /// wins an argument the file never entered. The second half keeps this from
    /// passing on a `file` that refuses to scale at all.
    #[tokio::test(start_paused = true)]
    async fn a_file_reset_does_not_scale_an_app_the_template_says_nothing_about() {
        let stocked = || app_with("web", |app| app.instances = 4);

        let silent_dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&silent_dir, &[stocked()]);
        let silent = declared_app(AppConfig::minimal("web", "./srv"), &["name", "script"]);
        let reply = apply_config(&mut actor, vec![silent], ResetDepth::File).await;
        assert_eq!(
            actor.ids_of_name("web").len(),
            4,
            "`file` scaled against a file with no `instances` line: {reply:?}"
        );
        assert!(
            !reply[0].applied.contains(&"instances".to_string()),
            "{reply:?}"
        );

        let declaring_dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&declaring_dir, &[stocked()]);
        let mut declaring = AppConfig::minimal("web", "./srv");
        declaring.instances = 2;
        let declaring = declared_app(declaring, &["name", "script", "instances"]);
        let reply = apply_config(&mut actor, vec![declaring], ResetDepth::File).await;
        assert_eq!(
            actor.ids_of_name("web").len(),
            2,
            "`file` must apply a count the template does declare: {reply:?}"
        );
        assert!(
            reply[0].applied.contains(&"instances".to_string()),
            "{reply:?}"
        );
    }

    /// The `Policy` depth touches env not at all, so a template that has grown
    /// `NEW_KEY` reports nothing and merges nothing. Recording the key as
    /// established anyway would leave no plain load able to append it, with
    /// only `--reset=all` to recover, taking every other env value.
    #[tokio::test(start_paused = true)]
    async fn a_settings_reset_does_not_establish_an_env_key_it_never_merged() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(&["name", "script"], Vec::new()),
        )
        .unwrap();

        let file = || {
            let mut file = AppConfig::minimal("web", "./srv");
            file.env = BTreeMap::from([("NEW_KEY".to_string(), "1".to_string())]);
            declared_app(file, &["name", "script", "env"])
        };
        let reset = apply_config(&mut actor, vec![file()], ResetDepth::Policy).await;

        assert!(
            actor.sheep[&0].entry.spec.config().env.is_empty(),
            "a `--reset=policy` merges no env at all: {reset:?}"
        );
        assert!(
            shep_core::overrides::get(&actor.paths.overrides, "web")
                .unwrap()
                .expect("the load records what it established")
                .declared_env
                .is_empty(),
            "a key that never merged is not established"
        );

        // The plain load after it can still append.
        let plain = apply_config(&mut actor, vec![file()], ResetDepth::None).await;
        assert_eq!(
            actor.sheep[&0]
                .entry
                .pending
                .as_ref()
                .expect("an env change parks for the next spawn")
                .config()
                .env
                .get("NEW_KEY")
                .map(String::as_str),
            Some("1"),
            "{plain:?}"
        );
    }

    /// `max_memory`, `watch` and the cron pair are read when a worker is
    /// armed, so a spec write alone leaves the old value enforced for as long
    /// as that arming lives.
    #[tokio::test(start_paused = true)]
    async fn a_changed_extras_field_rearms_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.max_memory = Some(MemSize::from_bytes(100 << 20));
            })],
        );

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_memory = Some(MemSize::from_bytes(512 << 20));
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_memory"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(
            enforcer
                .arms()
                .last()
                .expect("a changed ceiling re-arms the name")
                .limit,
            MemSize::from_bytes(512 << 20),
            "the registry was re-armed with the old ceiling"
        );
    }

    /// `PollingEnforcer` computes a breach under its lock and sends it after
    /// releasing that lock, so a re-arm landing in between leaves a report in
    /// flight speaking for a limit nobody enforces. Reachable only because a
    /// load re-arms an id that is already armed.
    #[tokio::test(start_paused = true)]
    async fn a_breach_measured_under_a_since_raised_ceiling_does_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.max_memory = Some(MemSize::from_bytes(100 << 20));
            })],
        );

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_memory = Some(MemSize::from_bytes(512 << 20));
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_memory"])],
            ResetDepth::None,
        )
        .await;

        // Over the ceiling that was armed when the sample was taken, well
        // under the one the load just put in force.
        actor.handle_extra_restart(
            0,
            APPLY_FIRST_PID,
            None,
            Some(MemSize::from_bytes(200 << 20)),
        );

        let slot = &actor.sheep[&0];
        assert!(
            slot.manual.is_none(),
            "a breach against a ceiling the operator has raised must never claim the manual marker"
        );
        assert_eq!(slot.entry.pid, Some(APPLY_FIRST_PID));
    }

    /// The merge builds on the app's intended config, the parked one when
    /// there is one, not on what the running child was spawned from: on the
    /// second load the key is established, a plain load skips it, and a merge
    /// based on the running config would carry the old value forward over the
    /// parked one.
    #[tokio::test(start_paused = true)]
    async fn a_second_load_of_the_same_file_keeps_the_first_loads_parked_config() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        let file = || {
            let mut file = AppConfig::minimal("web", "./srv");
            file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
            declared_app(file, &["name", "script", "env"])
        };

        let first = apply_config(&mut actor, vec![file()], ResetDepth::None).await;
        assert_eq!(first[0].pending, vec!["env".to_string()]);

        let second = apply_config(&mut actor, vec![file()], ResetDepth::None).await;

        assert_eq!(
            actor.sheep[&0]
                .entry
                .pending
                .as_ref()
                .expect("the parked config survives a second load")
                .config()
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue"),
            "the second load erased what the first parked"
        );
        assert_eq!(
            second[0]
                .app
                .as_ref()
                .expect("an applied app is recorded")
                .config()
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue"),
            "the recorded app lost it too, so a reboot would come up without it"
        );
        assert!(
            second[0].pending.is_empty(),
            "the second load changes nothing that was not already coming: {second:?}"
        );
    }

    /// An instance with no live task is deregistered synchronously inside
    /// `handle_scale`, so the id list read before the scale can name a slot
    /// that is already gone by the time the spec write walks it.
    #[tokio::test(start_paused = true)]
    async fn a_scale_down_removing_a_non_running_instance_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);
        // One instance exited and never came back. No `ctl`, so its delete
        // resolves on the spot rather than through a kill ladder.
        let stopped = actor.sheep.get_mut(&1).expect("the fixture registers two");
        stopped.entry.status = ProcStatus::Stopped;
        stopped.entry.pid = None;

        let mut file = AppConfig::minimal("web", "./srv");
        file.instances = 1;
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "instances"])],
            ResetDepth::Policy,
        )
        .await;

        assert_eq!(actor.ids_of_name("web"), vec![0]);
        assert_eq!(reply[0].applied, vec!["instances".to_string()]);
        assert_eq!(actor.sheep[&0].entry.spec.config().instances, 1);
    }

    /// `Applied::refused` with two empty lists promises nothing happened, so a
    /// reply with `app` as `None` leaves the muster roll on the old count while
    /// a second instance runs. A file declaring `watch` and `cwd` together
    /// merges cleanly and cannot be reached by a running instance, since `cwd`
    /// needs a respawn and `watch` does not.
    #[tokio::test(start_paused = true)]
    async fn a_load_that_scales_and_cannot_reach_the_running_spec_reports_what_landed() {
        let root = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.instances = 2;
        file.watch = true;
        file.cwd = Some(root.path().display().to_string());
        let reply = apply_config(
            &mut actor,
            vec![declared_app(
                file,
                &["name", "script", "instances", "watch", "cwd"],
            )],
            ResetDepth::Policy,
        )
        .await;

        assert_eq!(
            actor.ids_of_name("web").len(),
            2,
            "the scale really did happen"
        );
        assert!(
            reply[0].refused.is_none(),
            "a merge that normalizes is not an invalid file: {reply:?}"
        );
        assert!(
            reply[0].app.is_some(),
            "the muster roll must not be left on the pre-load config: {reply:?}"
        );
        assert_eq!(reply[0].applied, vec!["instances".to_string()]);
        assert!(
            reply[0].pending.contains(&"watch".to_string())
                && reply[0].pending.contains(&"cwd".to_string()),
            "a change no running instance can take must park, not vanish: {reply:?}"
        );
        assert!(
            actor.sheep[&0].entry.pending.is_some(),
            "and it must be parked on the entry, not only reported"
        );
    }

    /// Group membership is decided by the config rather than by what is
    /// running, and both group triggers restart every member, so arming a
    /// stopped instance lets a cron occurrence or a file save start it again.
    /// Nothing heals that: the member is terminal, so no transition calls
    /// `disarm` for it.
    #[tokio::test(start_paused = true)]
    async fn a_load_does_not_arm_a_stopped_instance() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.cron_restart = Some("0 * * * *".to_string());
            })],
        );
        let stopped = actor.sheep.get_mut(&0).expect("the fixture registers one");
        stopped.entry.status = ProcStatus::Stopped;
        stopped.entry.pid = None;

        let mut file = AppConfig::minimal("web", "./srv");
        file.cron_restart = Some("*/5 * * * *".to_string());
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "cron_restart"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(
            actor.registry.group_members("web"),
            None,
            "a load armed a schedule that will start a sheep the operator stopped"
        );
    }

    /// A plain load skips the count and says why; a `--reset` takes it. The
    /// override store cannot tell a stocked count from an untouched one, so a
    /// plain load acting on the field would delete instances.
    #[tokio::test(start_paused = true)]
    async fn a_plain_load_never_scales_and_a_reset_does() {
        let file = || {
            let mut file = AppConfig::minimal("web", "./srv");
            file.instances = 1;
            declared_app(file, &["name", "script", "instances"])
        };

        let plain_dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) =
            actor_over(&plain_dir, &[app_with("web", |app| app.instances = 2)]);
        let reply = apply_config(&mut actor, vec![file()], ResetDepth::None).await;
        assert_eq!(
            actor.ids_of_name("web").len(),
            2,
            "a plain load deleted an instance"
        );
        assert!(!reply[0].applied.contains(&"instances".to_string()));
        assert_eq!(
            reply[0].refused.as_deref(),
            Some(
                "instances: this load never reshapes a flock; no mode scales without also \
                 putting back every setting the file declares, and `--reset=file` is the \
                 narrowest that does, taking the file's count of 1"
            ),
            "the refusal must name a mode an operator can actually type, \
             not the bare flag `shep start --reset` now refuses on its \
             own, and it must name what following the advice costs: {reply:?}"
        );
        assert_eq!(
            reply[0]
                .app
                .as_ref()
                .expect("an applied app is recorded")
                .config()
                .instances,
            2,
            "the recorded count must be the one really running"
        );

        let reset_dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) =
            actor_over(&reset_dir, &[app_with("web", |app| app.instances = 2)]);
        let reply = apply_config(&mut actor, vec![file()], ResetDepth::Policy).await;
        assert_eq!(actor.ids_of_name("web"), vec![0], "--reset must take it");
        assert_eq!(reply[0].applied, vec!["instances".to_string()]);
    }

    /// All eight [`EXTRAS_FIELDS`] are read when a worker is armed, so a spec
    /// write alone leaves the old value enforced for the life of that worker.
    /// The observable is the memory ceiling, unchanged across these apps: a
    /// re-arm arms every instance, so any arming recorded is proof of one.
    #[tokio::test(start_paused = true)]
    async fn every_extras_field_triggers_a_rearm() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().display().to_string();
        let ceiling = MemSize::from_bytes(100 << 20);
        // The "before" values every case edits away from.
        let base = |cwd: &str| {
            let cwd = cwd.to_string();
            move |app: &mut AppConfig| {
                app.max_memory = Some(ceiling);
                app.cwd = Some(cwd.clone());
                app.watch = true;
                app.ignore_watch = vec!["target/**".to_string()];
                app.watch_delay = Some(UpDuration::from_millis(1000));
                app.watch_options = vec!["src/**".to_string()];
                app.cron_restart = Some("0 * * * *".to_string());
                app.cron_timezone = Some("UTC".to_string());
                // An interval no paused-clock case advances to, so the probe
                // never runs and this stays about arming.
                app.liveness_probe = Some(ProbeConfig {
                    failure_threshold: 3,
                    interval: UpDuration::from_millis(600_000),
                    timeout: UpDuration::from_millis(1000),
                    ..probe_config(ProbeKind::Tcp, "127.0.0.1:1")
                });
            }
        };
        // One edit per entry in `EXTRAS_FIELDS`, named by the field it moves,
        // so a field dropped from that list goes red under its own name.
        type Edit = (&'static str, fn(&mut AppConfig));
        let edits: Vec<Edit> = vec![
            ("max_memory", |app| {
                app.max_memory = Some(MemSize::from_bytes(512 << 20));
            }),
            ("watch", |app| app.watch = false),
            ("ignore_watch", |app| {
                app.ignore_watch = vec!["dist/**".to_string()];
            }),
            ("watch_delay", |app| {
                app.watch_delay = Some(UpDuration::from_millis(2500));
            }),
            ("watch_options", |app| {
                app.watch_options = vec!["lib/**".to_string()];
            }),
            ("cron_restart", |app| {
                app.cron_restart = Some("*/5 * * * *".to_string());
            }),
            ("cron_timezone", |app| {
                app.cron_timezone = Some("Europe/Berlin".to_string());
            }),
            ("liveness_probe", |app| app.liveness_probe = None),
        ];

        for (field, edit) in edits {
            let dir = tempfile::tempdir().unwrap();
            let (mut actor, enforcer) = actor_over(&dir, &[app_with("web", base(&cwd))]);
            let mut file = AppConfig::minimal("web", "./srv");
            base(&cwd)(&mut file);
            edit(&mut file);
            let reply = apply_config(
                &mut actor,
                vec![declared_app(
                    file,
                    &[
                        "name",
                        "script",
                        "cwd",
                        "max_memory",
                        "watch",
                        "ignore_watch",
                        "watch_delay",
                        "watch_options",
                        "cron_restart",
                        "cron_timezone",
                        "liveness_probe",
                    ],
                )],
                ResetDepth::Policy,
            )
            .await;
            assert!(
                reply[0].applied.contains(&field.to_string()),
                "{field} did not apply at all: {reply:?}"
            );
            assert!(
                !enforcer.arms().is_empty(),
                "changing {field} left the armed worker on the old value"
            );
        }
    }

    /// A name whose instances are all momentarily non-`Online`, each inside a
    /// crash-restart backoff, has nothing to arm, and an early return there
    /// would skip the teardown too. `disarm_extras` leaves a `WaitingRestart`
    /// sheep armed, so it stays a group member and the group is never torn
    /// down. Arms first, which makes this a test of the teardown;
    /// `a_load_does_not_arm_a_stopped_instance` arms nothing, so its assertion
    /// holds either way.
    #[tokio::test(start_paused = true)]
    async fn a_load_tears_down_a_group_whose_instances_are_all_down() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.cron_restart = Some("0 * * * *".to_string());
            })],
        );
        actor.arm_extras(0);
        assert_eq!(
            actor.registry.group_members("web"),
            Some(vec![0]),
            "the fixture must really be armed, or this pins nothing"
        );

        // Mid-backoff: no pid, not online, still registered and still a group
        // member.
        let waiting = actor.sheep.get_mut(&0).expect("the fixture registers one");
        waiting.entry.status = ProcStatus::WaitingRestart;
        waiting.entry.pid = None;

        let mut file = AppConfig::minimal("web", "./srv");
        file.cron_restart = Some("*/5 * * * *".to_string());
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "cron_restart"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(reply[0].applied, vec!["cron_restart".to_string()]);
        assert_eq!(
            actor.registry.group_members("web"),
            None,
            "a worker built from the replaced config survived a load that reported the \
             field as applied"
        );
    }

    /// The merge normalizes against the count the intended config carries, and
    /// the flock can be running a different one, where the same config
    /// refuses. The earlier parked config is left alone, still the one a
    /// respawn picks up, so the report must not claim this load's fields are
    /// in it.
    #[tokio::test(start_paused = true)]
    async fn a_change_that_cannot_be_parked_is_not_reported_as_parked() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);
        // An earlier load parked a one-instance config. Two are running.
        let earlier = app_with("web", |app| {
            app.instances = 1;
            app.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        });
        for id in [0, 1] {
            actor
                .sheep
                .get_mut(&id)
                .expect("the fixture registers two")
                .entry
                .pending = Some(earlier.clone());
        }

        // One explicit log path, no `{{instance}}` and no `merge_logs`: legal
        // for the one instance the parked config declares, refused for the
        // two really running.
        let mut file = AppConfig::minimal("web", "./srv");
        file.out_file = Some("/tmp/web.log".to_string());
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "out_file"])],
            ResetDepth::None,
        )
        .await;

        assert!(
            !reply[0].pending.contains(&"out_file".to_string()),
            "a field that went nowhere must not be reported as coming: {reply:?}"
        );
        assert!(
            reply[0]
                .refused
                .as_deref()
                .is_some_and(|why| why.contains("out_file")),
            "and the operator must be told which field it was: {reply:?}"
        );
        assert_eq!(
            actor.sheep[&0]
                .entry
                .pending
                .as_ref()
                .expect("the earlier load's parked config survives")
                .config()
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue"),
            "the earlier load's parked config must not be cleared"
        );
        assert!(
            reply[0].app.is_none(),
            "there is no config that holds at the count really running, so nothing may be \
             recorded: {reply:?}"
        );
    }

    /// A plain load holds `instances` out of the merge, so the sibling case
    /// above does not cover the depth every `shep start` uses. Two running
    /// instances plus one shared explicit `out_file` is the reachable shape.
    #[tokio::test(start_paused = true)]
    async fn a_plain_load_whose_merge_cannot_normalize_refuses_and_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.out_file = Some("/tmp/web.log".to_string());
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "out_file"])],
            ResetDepth::None,
        )
        .await;

        assert!(reply[0].refused.is_some(), "{reply:?}");
        assert!(reply[0].applied.is_empty() && reply[0].pending.is_empty());
        assert!(reply[0].app.is_none());
        assert!(actor.sheep[&0].entry.spec.config().out_file.is_none());
        assert!(actor.sheep[&0].entry.pending.is_none());
        assert_eq!(actor.ids_of_name("web").len(), 2);
    }

    /// `parked_wanted` is set by an earlier parked config as well as by this
    /// load's own `NeedsRespawn` drift, so a load whose only drift is a Live
    /// field can fail the rebuild with no field of its own to name. The stale
    /// parked config then puts the old value back at the next respawn.
    /// Reachable whenever the instance count moved between two loads.
    #[tokio::test(start_paused = true)]
    async fn a_parked_config_that_cannot_be_rebuilt_is_reported_even_with_no_field_to_name() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);
        // Parked when the app ran one instance: a shared explicit log path is
        // legal for one and refused for the two running now.
        let earlier = app_with("web", |app| {
            app.instances = 1;
            app.max_restarts = 10;
            app.out_file = Some("/tmp/web.log".to_string());
        });
        for id in [0, 1] {
            actor
                .sheep
                .get_mut(&id)
                .expect("the fixture registers two")
                .entry
                .pending = Some(earlier.clone());
        }

        let mut file = AppConfig::minimal("web", "./srv");
        file.max_restarts = 99;
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "max_restarts"])],
            ResetDepth::None,
        )
        .await;

        assert_eq!(actor.sheep[&0].entry.spec.config().max_restarts, 99);
        assert!(
            reply[0]
                .refused
                .as_deref()
                .is_some_and(|why| why.contains("could not be rebuilt")),
            "a respawn is going to put max_restarts back to 10 and nobody was told: {reply:?}"
        );
        assert_eq!(
            actor.sheep[&0]
                .entry
                .pending
                .as_ref()
                .expect("the earlier parked config is still there")
                .config()
                .max_restarts,
            10,
            "and that is what makes the refusal true"
        );
    }

    /// The store's `declared` set is what `ResetDepth::None` skips over, so a
    /// refused key entering it makes the refusal's own advice useless: the
    /// retry meets silence rather than the same refusal.
    #[tokio::test(start_paused = true)]
    async fn a_refused_key_is_not_established_so_the_same_file_still_tries() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);
        let earlier = app_with("web", |app| app.instances = 1);
        for id in [0, 1] {
            actor
                .sheep
                .get_mut(&id)
                .expect("the fixture registers two")
                .entry
                .pending = Some(earlier.clone());
        }
        let file = || {
            let mut file = AppConfig::minimal("web", "./srv");
            file.out_file = Some("/tmp/web.log".to_string());
            declared_app(file, &["name", "script", "out_file"])
        };

        let names_it = |why: &str| why.contains("out_file");
        let first = apply_config(&mut actor, vec![file()], ResetDepth::None).await;
        assert!(
            first[0].refused.as_deref().is_some_and(names_it),
            "{first:?}"
        );

        let second = apply_config(&mut actor, vec![file()], ResetDepth::None).await;
        assert!(
            second[0].refused.as_deref().is_some_and(names_it),
            "a retry of the same file must meet the same refusal, not silence: {second:?}"
        );
        assert!(
            !shep_core::overrides::get(&actor.paths.overrides, "web")
                .unwrap()
                .expect("the load recorded what it established")
                .declared
                .contains("out_file"),
            "a key that went nowhere was established by nobody"
        );
    }

    /// `ResetDepth::None` skips a key somebody has established, so a load that
    /// records nothing leaves every key permanently re-writable. Three loads,
    /// because two cannot tell the difference: the first establishes the key,
    /// the second drops it from the file, and the third re-adds it with a
    /// different value, which only a record written by the first can refuse.
    #[tokio::test(start_paused = true)]
    async fn a_key_a_load_took_is_established_against_the_next_load() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        let with_budget = |budget: u32| {
            let mut file = AppConfig::minimal("web", "./srv");
            file.max_restarts = budget;
            declared_app(file, &["name", "script", "max_restarts"])
        };

        let first = apply_config(&mut actor, vec![with_budget(99)], ResetDepth::None).await;
        assert_eq!(first[0].applied, vec!["max_restarts".to_string()]);
        assert_eq!(actor.sheep[&0].entry.spec.config().max_restarts, 99);

        // The key leaves the file. Nothing happens, and nothing may forget
        // that it was established.
        let dropped = apply_config(
            &mut actor,
            vec![declared_app(
                AppConfig::minimal("web", "./srv"),
                &["name", "script"],
            )],
            ResetDepth::None,
        )
        .await;
        assert!(dropped[0].applied.is_empty(), "{dropped:?}");

        // And comes back with a different value. The app has run on 99 since
        // the first load, so a plain load must not take it.
        let third = apply_config(&mut actor, vec![with_budget(5)], ResetDepth::None).await;

        assert_eq!(
            actor.sheep[&0].entry.spec.config().max_restarts,
            99,
            "a file overwrote a key an earlier load had established"
        );
        assert!(third[0].applied.is_empty(), "{third:?}");
        assert!(
            shep_core::overrides::get(&actor.paths.overrides, "web")
                .unwrap()
                .expect("a load records what it established")
                .declared
                .contains("max_restarts"),
            "and the record is what makes that true"
        );
    }

    /// A key the file declares gives up its override during the merge, so a
    /// load that then fails to park has to hand it back. `env` is the one
    /// field that reaches this under the default depth: top-level keys hold
    /// their override by being established and never spend anything, while
    /// `env` merges one key at a time and spends the whole override table for
    /// any key the file declares.
    #[tokio::test(start_paused = true)]
    async fn a_refused_env_change_gives_the_operators_override_back() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);
        // Parked when the app ran one instance, with a shared explicit log
        // path: legal for one, refused for the two running now.
        let earlier = app_with("web", |app| {
            app.instances = 1;
            app.out_file = Some("/tmp/web.log".to_string());
        });
        for id in [0, 1] {
            actor
                .sheep
                .get_mut(&id)
                .expect("the fixture registers two")
                .entry
                .pending = Some(earlier.clone());
        }
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(
                &["name", "script"],
                vec![("env", serde_json::json!({ "OPERATOR": "1" }))],
            ),
        )
        .unwrap();

        // Two env keys: the one the operator already holds, which spends the
        // override table, and one nobody has, which makes `env` drift.
        let mut file = AppConfig::minimal("web", "./srv");
        file.env = BTreeMap::from([
            ("OPERATOR".to_string(), "2".to_string()),
            ("MODE".to_string(), "blue".to_string()),
        ]);
        let reply = apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "env"])],
            ResetDepth::None,
        )
        .await;
        assert!(
            reply[0]
                .refused
                .as_deref()
                .is_some_and(|why| why.contains("env")),
            "the fixture must really refuse the env change: {reply:?}"
        );

        let record = shep_core::overrides::get(&actor.paths.overrides, "web")
            .unwrap()
            .expect("a load records what it established");
        assert_eq!(
            record
                .fields
                .get("env")
                .and_then(|env| env.get("OPERATOR"))
                .and_then(serde_json::Value::as_str),
            Some("1"),
            "a load that changed nothing spent the operator's override"
        );
        assert!(
            record.declared_env.is_empty(),
            "and it established env keys that never landed: {:?}",
            record.declared_env
        );
    }

    // --- Promotion: a parked config reaching its replacement process ---

    // Actor-tier: a promotion moves `spec`, `pending` and `credentials`, and no
    // reply reports those three together.

    /// `spawn_replacement` carries `restarts`, `dog` and `last_exit` off the
    /// drainee on the same grounds: the replacement is the same instance
    /// continuing, not a new one.
    #[tokio::test(start_paused = true)]
    async fn reload_carries_the_overridden_cache_to_its_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        actor.sheep.get_mut(&0).unwrap().entry.overridden = vec!["max_restarts".to_string()];

        actor.advance_reload("web", VecDeque::from([0]));

        let new_id = actor.reloads["web"]
            .swap
            .new_id
            .expect("an overlapping reload spawns its replacement at once");
        assert_eq!(
            actor.sheep[&new_id].entry.overridden,
            vec!["max_restarts".to_string()],
            "the replacement must carry the drainee's own overridden cache, not start blank"
        );
    }

    /// Without this the pending slot is written and never read, so an operator
    /// sees a pending field forever with no way to apply it.
    #[tokio::test(start_paused = true)]
    async fn reload_promotes_pending_config() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "env"])],
            ResetDepth::None,
        )
        .await;
        assert!(
            actor.sheep[&0].entry.pending.is_some(),
            "the fixture must really park the change, or this case proves nothing"
        );

        actor.advance_reload("web", VecDeque::from([0]));

        let new_id = actor.reloads["web"]
            .swap
            .new_id
            .expect("an overlapping reload spawns its replacement at once");
        assert_eq!(
            actor.sheep[&new_id]
                .entry
                .spec
                .config()
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue"),
            "the replacement must come up on the config the load parked"
        );
        assert!(
            actor.sheep[&new_id].entry.pending.is_none(),
            "and it is owed nothing further, having been built from what was owed"
        );
        assert!(
            actor.sheep[&0].entry.pending.is_some(),
            "the drainee keeps its copy until it is deregistered: this swap can still be \
             abandoned, and the child it would go back to serving has not got the change"
        );
        assert_ne!(
            actor.sheep[&new_id].entry.pid,
            Some(APPLY_FIRST_PID),
            "a promotion is only reachable through a process that actually replaced the old one"
        );
    }

    /// Both verbs replace the child, so both are chances to apply what is owed.
    #[tokio::test(start_paused = true)]
    async fn restart_promotes_pending_config() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "env"])],
            ResetDepth::None,
        )
        .await;
        assert!(
            actor.sheep[&0].entry.pending.is_some(),
            "the fixture must really park the change, or this case proves nothing"
        );

        // The door `shep restart` takes on a running sheep: `begin_manual`
        // claims the next exit and `handle_exited` respawns. The other door is
        // `apply_immediate`'s `Restart` arm; both end in `respawn`.
        let (reply, _answer) = oneshot::channel();
        actor.begin_manual(
            ProcessSelector::Name("web".to_string()),
            ManualKind::Restart,
            CommandOrigin::Operator,
            ReplyKind::Info(reply),
        );
        actor.handle_exited(
            0,
            ExitOutcome {
                code: Some(0),
                signal: None,
            },
        );

        let entry = &actor.sheep[&0].entry;
        assert_eq!(
            entry.spec.config().env.get("MODE").map(String::as_str),
            Some("blue"),
            "the restarted child must come up on the config the load parked"
        );
        assert!(
            entry.pending.is_none(),
            "a promoted config is owed no longer, so the slot must be empty"
        );
        assert_ne!(
            entry.pid,
            Some(APPLY_FIRST_PID),
            "a promotion is only reachable through a process that actually replaced the old one"
        );
    }

    /// A pending field an operator cannot see is a silent divergence.
    #[tokio::test(start_paused = true)]
    async fn to_info_reports_the_pending_fields_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "env"])],
            ResetDepth::None,
        )
        .await;

        let entry = &actor.sheep[&0].entry;
        assert!(
            entry.pending.is_some(),
            "the fixture must really park the change, or this case proves nothing"
        );
        let info = to_info(entry, &actor.smits);
        assert_eq!(info.pending, Some(vec!["env".to_string()]));
    }

    /// A scale-up calls `overridden_for` once per new instance, so a cache miss
    /// costs one locked file read per slot. The store is seeded with a
    /// different answer from the sibling's cache, so the sibling winning is the
    /// assertion.
    #[tokio::test(start_paused = true)]
    async fn overridden_for_prefers_a_live_sibling_over_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        actor.sheep.get_mut(&0).unwrap().entry.overridden = vec!["cwd".to_string()];
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(
                &["name", "script"],
                vec![("max_restarts", serde_json::json!(9))],
            ),
        )
        .unwrap();

        assert_eq!(actor.overridden_for("web"), vec!["cwd".to_string()]);
    }

    /// A muster restore and a handover installation both install one sheep at a
    /// time, before there is a sibling to ask.
    #[tokio::test(start_paused = true)]
    async fn overridden_for_reads_the_store_when_no_sibling_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (actor, _enforcer) = actor_over(&dir, &[]);
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(
                &["name", "script"],
                vec![("max_restarts", serde_json::json!(9))],
            ),
        )
        .unwrap();

        assert_eq!(
            actor.overridden_for("web"),
            vec!["max_restarts".to_string()]
        );
        assert_eq!(
            actor.overridden_for("nobody-has-heard-of-this-app"),
            Vec::<String>::new(),
            "an unreadable-or-empty answer for a name the store has never seen"
        );
    }

    /// An override with nothing to show it is a silent divergence, the same
    /// class as an unreported `pending`.
    #[tokio::test(start_paused = true)]
    async fn to_info_reports_the_overridden_field_names_the_store_holds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) =
            actor_over(&dir, &[app_with("web", |app| app.max_restarts = 7)]);
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(
                &["name", "script"],
                vec![("max_restarts", serde_json::json!(7))],
            ),
        )
        .unwrap();

        let file = AppConfig::minimal("web", "./srv");
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script"])],
            ResetDepth::None,
        )
        .await;

        let entry = &actor.sheep[&0].entry;
        assert_eq!(
            entry.overridden,
            vec!["max_restarts".to_string()],
            "the cache must mirror what this load wrote back to the override store"
        );
        let info = to_info(entry, &actor.smits);
        assert_eq!(info.overridden, Some(vec!["max_restarts".to_string()]));
    }

    /// `AppOverrides::fields` is a `serde_json::Map` that can hold anything, so
    /// the guarantee is that `Actor::apply_one` and `Actor::overridden_for`
    /// extract `.keys()` and never a value. Asserted at the producer, over a
    /// store seeded with a secret-shaped value the way `env` arrives there.
    #[tokio::test(start_paused = true)]
    async fn to_info_never_carries_an_override_value() {
        const SENTINEL: &str = "postgres://sentinel-value-that-must-never-appear";

        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.env = BTreeMap::from([("DATABASE_URL".to_string(), SENTINEL.to_string())]);
            })],
        );
        shep_core::overrides::put(
            &actor.paths.overrides,
            "web",
            &established(
                &["name", "script"],
                vec![("env", serde_json::json!({ "DATABASE_URL": SENTINEL }))],
            ),
        )
        .unwrap();

        let file = AppConfig::minimal("web", "./srv");
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script"])],
            ResetDepth::None,
        )
        .await;

        let entry = &actor.sheep[&0].entry;
        let info = to_info(entry, &actor.smits);
        assert_eq!(
            info.overridden,
            Some(vec!["env".to_string()]),
            "the name must still reach the operator"
        );
        let json = serde_json::to_string(&info).unwrap();
        assert!(
            !json.contains(SENTINEL),
            "an override value reached the wire: {json}"
        );
    }

    /// `credentials` is resolved once so a restart does not change a running
    /// app's identity by accident; an operator editing `user` is the one case
    /// that must re-resolve.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn promoting_a_user_change_re_resolves_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.user = Some(own_user_name());
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "user"])],
            ResetDepth::None,
        )
        .await;
        assert!(
            actor.sheep[&0].entry.pending.is_some(),
            "the fixture must really park the change, or this case proves nothing"
        );

        actor.advance_reload("web", VecDeque::from([0]));

        let wanted = Credentials {
            uid: nix::unistd::geteuid().as_raw(),
            gid: None,
        };
        assert_eq!(
            actor.runner.spawned_as(0),
            Some(wanted),
            "the replacement must carry the identity the promoted `user` resolves to; `None` \
             here is the fixture's stale resolution, which is the change being ignored"
        );
        let new_id = actor.reloads["web"]
            .swap
            .new_id
            .expect("an overlapping reload spawns its replacement at once");
        assert_eq!(
            actor.sheep[&new_id].entry.credentials,
            SpawnIdentity::Resolved(Some(wanted)),
            "and the replacement records it, so the restart after this one reuses it"
        );
        assert_eq!(
            actor.sheep[&0].entry.credentials,
            SpawnIdentity::Resolved(None),
            "while the drainee's own identity is untouched: it is still serving under it, and \
             an abandoned swap must not leave it recorded as never looked up"
        );
    }

    /// Re-resolving on every promotion would mean a passwd lookup per config
    /// change, and would defeat the once-only rule.
    #[tokio::test(start_paused = true)]
    async fn promoting_an_unrelated_change_keeps_the_resolved_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(
            &dir,
            &[app_with("web", |app| {
                app.user = Some(NO_SUCH_USER.to_string())
            })],
        );
        // An unresolvable name makes reuse observable: this value cannot be
        // re-derived, so a spawn carrying it is a spawn that reused it.
        let settled = Credentials {
            uid: 4242,
            gid: None,
        };
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registers one instance")
            .entry
            .credentials = SpawnIdentity::Resolved(Some(settled));

        let mut file = AppConfig::minimal("web", "./srv");
        file.args = vec!["--port=8080".to_string()];
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "args"])],
            ResetDepth::None,
        )
        .await;
        assert!(
            actor.sheep[&0].entry.pending.is_some(),
            "the fixture must really park the change, or this case proves nothing"
        );

        actor.advance_reload("web", VecDeque::from([0]));

        assert_eq!(
            actor.runner.spawn_count(),
            1,
            "the replacement must have been spawned at all: an identity re-resolved here could \
             only refuse, and a refusal would abandon the reload"
        );
        assert_eq!(
            actor.runner.spawned_as(0),
            Some(settled),
            "an `args` change is not an identity change, so the replacement runs as whoever the \
             instance was already running as"
        );
        assert_eq!(
            actor.sheep[&0].entry.credentials,
            SpawnIdentity::Resolved(Some(settled)),
            "and the stored resolution is untouched, so no passwd lookup was spent"
        );
        assert!(
            actor.sheep[&0].entry.pending.is_some(),
            "the drainee keeps what it was owed; the replacement is what carries it"
        );
        let new_id = actor.reloads["web"]
            .swap
            .new_id
            .expect("an overlapping reload spawns its replacement at once");
        assert_eq!(
            actor.sheep[&new_id].entry.spec.config().args,
            vec!["--port=8080".to_string()],
            "the promotion itself must still have happened"
        );
    }

    /// `apply_one` derives one spec from `ids_of_name`'s first id, always
    /// instance 0, and writes it onto every sibling. A promotion that diffed
    /// `pending` against `spec` would find the `user` change instance 1 has not
    /// applied already sitting on instance 1's spec. Three loads, not two:
    /// by the third, instance 1's spec is already flattened, so a load that
    /// recomputed the flag would clear the first load's decision.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_sibling_that_has_not_promoted_yet_still_re_resolves_after_later_loads() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 3)]);
        // The identity the three instances already run under, and one no lookup
        // could produce, so a spawn carrying it is a spawn that reused it.
        let settled = Credentials {
            uid: 4242,
            gid: None,
        };
        for id in [0, 1, 2] {
            actor
                .sheep
                .get_mut(&id)
                .expect("the fixture registers three")
                .entry
                .credentials = SpawnIdentity::Resolved(Some(settled));
        }

        let user_change = || {
            let mut file = AppConfig::minimal("web", "./srv");
            file.instances = 3;
            file.user = Some(own_user_name());
            vec![declared_app(file, &["name", "script", "instances", "user"])]
        };
        apply_config(&mut actor, user_change(), ResetDepth::None).await;

        // Instance 0 alone: the shape every automatic restart takes.
        actor.respawn(0, true);

        // The same file twice. Each reads its base config off instance 0, which
        // has now promoted, and writes it over instances 1 and 2.
        apply_config(&mut actor, user_change(), ResetDepth::None).await;
        apply_config(&mut actor, user_change(), ResetDepth::None).await;

        actor.respawn(1, true);

        let wanted = Credentials {
            uid: nix::unistd::geteuid().as_raw(),
            gid: None,
        };
        assert_eq!(
            actor.runner.spawned_as(1),
            Some(wanted),
            "instance 1 has still never applied the `user` change, so its promotion must \
             re-resolve; the settled 4242 here is the change being silently dropped"
        );
        assert_eq!(
            actor.sheep[&1].entry.spec.config().user,
            Some(own_user_name()),
            "and its own spec must record what it came up on"
        );
    }

    /// The drainee goes back to the child it already had, which was never
    /// spawned with the parked config, so an entry claiming it with an empty
    /// pending slot leaves the next load seeing no drift while the child runs
    /// superseded code.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_reload_leaves_the_parked_config_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        // A live control sender says this instance's task is still there to
        // go back to; the fixture leaves it `None`.
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        actor.sheep.get_mut(&0).expect("the fixture's sheep").ctl = Some(ctl_tx);

        let mut file = AppConfig::minimal("web", "./srv");
        file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "env"])],
            ResetDepth::None,
        )
        .await;

        actor.advance_reload("web", VecDeque::from([0]));
        actor.handle_reload_deadline("web", actor.reloads["web"].deadline);

        assert!(actor.reloads.is_empty(), "the swap must really be off");
        let entry = &actor.sheep[&0].entry;
        assert_eq!(
            entry.status,
            ProcStatus::Online,
            "the drainee is serving again, so it is the child spawned before the load"
        );
        assert!(
            entry.spec.config().env.is_empty(),
            "and its spec must still describe what that child was spawned from"
        );
        assert_eq!(
            entry
                .pending
                .as_ref()
                .expect("the config is still owed: no child ever came up on it")
                .config()
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue")
        );
    }

    /// `SpawnIdentity::Unresolved` makes a later spawn resolve from scratch, so
    /// for a `user` that has stopped resolving it is a running app whose next
    /// restart is refused over the identity it already runs under.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_reload_leaves_the_drainees_identity_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |_| {})]);
        let settled = Credentials {
            uid: 4242,
            gid: None,
        };
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registers one instance")
            .entry
            .credentials = SpawnIdentity::Resolved(Some(settled));

        // A `user` that cannot resolve, so the reload is abandoned at the one
        // point that runs before anything else in `spawn_replacement`.
        let mut file = AppConfig::minimal("web", "./srv");
        file.user = Some(NO_SUCH_USER.to_string());
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "user"])],
            ResetDepth::None,
        )
        .await;

        actor.advance_reload("web", VecDeque::from([0]));

        assert_eq!(
            actor.runner.spawn_count(),
            0,
            "the fixture must really refuse the replacement, or this case proves nothing"
        );
        let entry = &actor.sheep[&0].entry;
        assert_eq!(
            entry.credentials,
            SpawnIdentity::Resolved(Some(settled)),
            "the drainee is still serving under this identity, so the abandoned swap must not \
             record it as never looked up"
        );
        assert!(
            entry.pending.is_some(),
            "and it is still owed the config that swap was going to bring"
        );
    }

    /// `readiness_probe` is `NextSpawn` and lands on the stored spec at once,
    /// while `wait_ready` is `NeedsRespawn` and parks, so an app moving from
    /// channel readiness to an HTTP probe holds both. `wait_ready` wins in
    /// `ReadinessSource::of`, so an ordering read from the stored spec says
    /// overlap while the replacement comes up probe-gated: two instances on one
    /// address, with a probe the drainee answers.
    #[tokio::test(start_paused = true)]
    async fn a_reload_orders_itself_by_the_config_its_replacement_will_carry() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) =
            actor_over(&dir, &[app_with("web", |app| app.wait_ready = true)]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.wait_ready = false;
        file.readiness_probe = Some(probe_config(ProbeKind::Tcp, "127.0.0.1:9"));
        apply_config(
            &mut actor,
            vec![declared_app(
                file,
                &["name", "script", "wait_ready", "readiness_probe"],
            )],
            ResetDepth::None,
        )
        .await;
        let entry = &actor.sheep[&0].entry;
        assert!(
            entry.spec.config().wait_ready && entry.spec.config().readiness_probe.is_some(),
            "the fixture must really hold both at once, or this case proves nothing"
        );

        actor.advance_reload("web", VecDeque::from([0]));

        let job = &actor.reloads["web"];
        assert_eq!(
            job.mode,
            ReloadMode::Serial,
            "the replacement is probe-gated, and a probe cannot say which of two overlapping \
             instances answered it"
        );
        assert_eq!(
            job.swap.phase,
            ReloadPhase::DrainFirst,
            "so the drain runs first and nothing is spawned yet"
        );
        assert_eq!(
            actor.runner.spawn_count(),
            0,
            "an overlap here would put a second instance on the drainee's address"
        );
    }

    /// New instances are spawned from the config the old ones are running, read
    /// off instance 0, and `spawn_fresh` registers no pending slot, so a
    /// `shep stock` during a parking window would leave them on superseded
    /// config with nothing saying a restart is due.
    #[tokio::test(start_paused = true)]
    async fn a_scale_up_carries_the_parked_config_onto_the_instances_it_creates() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.instances = 2;
        file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "instances", "env"])],
            ResetDepth::None,
        )
        .await;

        // The standalone verb, not the count inside a load: `apply_one` parks
        // onto every slot after its own scale.
        let (reply, mut answer) = oneshot::channel();
        actor.handle_scale("web", 4, reply);
        answer
            .try_recv()
            .expect("handle_scale answers before it returns")
            .expect("the fixture has scripts enough to scale to four");

        for id in [2, 3] {
            assert_eq!(
                actor.sheep[&id]
                    .entry
                    .pending
                    .as_ref()
                    .unwrap_or_else(|| panic!("instance {id} must be owed the parked config"))
                    .config()
                    .env
                    .get("MODE")
                    .map(String::as_str),
                Some("blue"),
                "an instance created during a parking window is owed the same config as its \
                 siblings"
            );
            assert!(
                actor.sheep[&id].entry.spec.config().env.is_empty(),
                "and its own spec still describes what it was actually spawned from"
            );
        }
    }

    /// A parked config copied verbatim leaves every slot holding
    /// `pending.instances = 2` against a spec of 4, so `drifted_fields` reports
    /// `instances` pending forever and the reload that promotes writes the
    /// count back down.
    #[tokio::test(start_paused = true)]
    async fn a_scale_updates_the_count_inside_the_config_it_carries() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[app_with("web", |app| app.instances = 2)]);

        let mut file = AppConfig::minimal("web", "./srv");
        file.instances = 2;
        file.env = BTreeMap::from([("MODE".to_string(), "blue".to_string())]);
        apply_config(
            &mut actor,
            vec![declared_app(file, &["name", "script", "instances", "env"])],
            ResetDepth::None,
        )
        .await;

        let (reply, mut answer) = oneshot::channel();
        actor.handle_scale("web", 4, reply);
        answer
            .try_recv()
            .expect("handle_scale answers before it returns")
            .expect("the fixture has scripts enough to scale to four");

        for id in 0..4 {
            let entry = &actor.sheep[&id].entry;
            assert_eq!(
                entry
                    .pending
                    .as_ref()
                    .unwrap_or_else(|| panic!("instance {id} must be owed the parked config"))
                    .config()
                    .instances,
                4,
                "the count a scale achieved, not the one an earlier load parked"
            );
            assert!(
                !to_info(entry, &actor.smits)
                    .pending
                    .unwrap_or_default()
                    .contains(&"instances".to_string()),
                "a reload owes this instance nothing about the count"
            );
        }
    }

    /// The `Live` fields ride across on the carried `AppConfig`; everything
    /// parked would vanish, and the next load would compare against a spec that
    /// already matched. Asserted through a promotion, since a config that
    /// arrives without its flag promotes on the identity the flag exists to
    /// replace, and only a spawn shows that.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_parked_config_and_its_reset_decision_survive_a_handover() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _enforcer) = actor_over(&dir, &[]);

        // The predecessor's entry: registered, not running, owed a `user`
        // change, and settled on an identity no lookup could produce.
        let settled = Credentials {
            uid: 4242,
            gid: None,
        };
        let entry = ProcessEntry {
            id: 7,
            spec: app_with("web", |_| {}),
            pending: Some(app_with("web", |app| app.user = Some(own_user_name()))),
            pending_reidentifies: true,
            overridden: Vec::new(),
            instance: 0,
            status: ProcStatus::Stopped,
            pid: None,
            restarts: 0,
            started_at: None,
            budget: RestartBudget::default(),
            reload: ReloadState::None,
            credentials: SpawnIdentity::Resolved(Some(settled)),
            out_file: PathBuf::new(),
            err_file: PathBuf::new(),
            dog: None,
            last_exit: None,
        };
        let carried =
            CarriedSheep::from_entry(&entry, 0, CarriedFds::none(), false, None, false, None);

        // Through serde, the boundary a handover crosses: an accessor reading
        // the source entry proves nothing about the blob.
        let crossed: CarriedSheep = serde_json::from_value(serde_json::to_value(&carried).unwrap())
            .expect("this daemon reads what it writes");

        actor
            .install_adopted(without_handles(crossed), &Arc::new(AdoptedReaper::new()))
            .expect("a registered-and-stopped sheep installs with nothing to adopt");

        assert!(
            actor.sheep[&7].entry.pending.is_some(),
            "the successor must still owe this sheep the change its predecessor parked"
        );

        actor.respawn(7, true);

        assert_eq!(
            actor.runner.spawned_as(0),
            Some(Credentials {
                uid: nix::unistd::geteuid().as_raw(),
                gid: None,
            }),
            "and promoting it must re-resolve: the settled 4242 here is the reset decision \
             lost in the blob, which is the identity change silently dropped"
        );
        assert_eq!(
            actor.sheep[&7].entry.spec.config().user,
            Some(own_user_name()),
            "and the promoted config is what the successor now records"
        );
    }
}
