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

use core::cmp::Ordering;
use core::fmt;
use core::sync::atomic::{self, AtomicU64};
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use shep_core::config::{AppConfig, ResolvedApp, normalize};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{
    ActionOutcome, ActionReply, BusEvent, DogSource, ExitInfo, LineOutcome, LineReply,
    ProcessEventKind, ProcessInfo, SheepDrift, SignalOutcome, SignalReply, Smit,
};
use shep_core::selector::ProcessSelector;
use shep_core::signals::OperatorSignal;
use shep_core::status::ProcStatus;

use crate::assemble::{assemble, instance_slots};
use crate::brain::{Decision, decide_on_exit};
use crate::channel::{ChildMessage, ShepherdMessage};
use crate::entry::{ProcessEntry, ReloadState, RestartBudget};
use crate::extras::{Extras, ExtrasRegistry};
use crate::kill::kill_process;
use crate::privilege::{self, Credentials};
use crate::probes::Prober;
use crate::probes::os::OsProber;
use crate::probes::ready::{Readiness, ReadinessSource, await_ready};
use crate::runner::{
    ExitOutcome, FlushError, LogCtl, Preflight, ProcIo, ProcessRunner, ReopenError, RunnerError,
    RunningProcess, SpawnSpec, StdinWrite, check_log_ancestry, open_log_path,
};

/// Capacity of the actor's own mailbox (commands + internal events).
const MAILBOX_CAPACITY: usize = 256;

/// Capacity of one sheep task's control mailbox — at most one live `Kill` is
/// ever in flight, so this stays small on purpose.
const SHEEP_CTL_CAPACITY: usize = 4;

/// Capacity of one sheep task's signal mailbox.
///
/// Wider than [`SHEEP_CTL_CAPACITY`] on purpose: unlike the kill ladder,
/// nothing bounds how many `shep signal` calls an operator can fire off
/// against one sheep in a burst, and [`Actor::begin_signal`] reads a `Full`
/// mailbox as "this sheep's task is busy" rather than as a hint the ladder is
/// already running (see [`SheepSlot::signals`]'s own doc for why the two
/// mailboxes cannot share one queue).
const SIGNAL_CAPACITY: usize = 16;

/// How much longer than its own two timeouts one swap of a reload is given
/// before the actor gives up on it (see [`Actor::arm_reload_deadline`]).
///
/// A swap that is going to finish is bounded by `listen_timeout` and then
/// `graceful_timeout`, back to back, with one synchronous pass through the
/// actor loop between them — so this covers scheduling jitter and nothing
/// else, and does not need to be generous to be safe.
///
/// Cutting a healthy swap short is cheap, which is what lets it be this
/// tight. Neither abandonment ends an instance that is serving: before the
/// commit it puts the instance being replaced back and kills a replacement
/// that never proved itself, which is what a readiness timeout already does;
/// after it, the replacement is the live instance and is left exactly where
/// it is. What is lost either way is the rest of the reload, and the event
/// says so.
const RELOAD_DEADLINE_SLACK: Duration = Duration::from_secs(5);

/// How long the shepherd waits for one line to land in a sheep's stdin before
/// reporting [`LineOutcome::NotWritten`].
///
/// A bound is not optional here (IR-46): a pipe fills at 64 KiB and the write
/// then blocks until the app reads, which an app that never reads never does —
/// so an unbounded wait is a request that can only end by the caller's own
/// deadline expiring, which tells the operator nothing about why.
///
/// Two seconds, and fixed rather than per-app. `AppConfig::action_timeout` is
/// per-app because an action's duration is the APP's work; a pipe write is the
/// kernel's, and the only thing a longer wait would buy is more time for an app
/// that is not reading its stdin to start. Comfortably under the 5s an RPC
/// caller gets when it sends no deadline of its own, so the honest
/// `not_written` row reaches the caller rather than racing its budget.
///
/// **That last sentence is only true because the waits run concurrently.** One
/// `sendline` costs at most this, whatever the selector matched; it is a bound
/// on the CALL, not on each sheep. Awaited one after another, `sendline all`
/// against three wedged sheep would cost six seconds and the caller's budget
/// would expire first. See [`Actor::begin_send_line`].
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many replies one sheep may still owe from actions that stopped
/// waiting before the oldest of them is forgotten (see [`ActionWaits`]).
///
/// Sized against what it defends, not measured: each entry is one action
/// name, and an entry is only created when an app misses an action's deadline
/// and only removed when that app finally answers. An app that answers late
/// needs one or two; an app that never answers at all accumulates one per
/// trigger for as long as its process lives, which is what the cap is for. A
/// dropped entry costs one late reply going to a wait it does not belong to
/// — the failure this whole mechanism exists to prevent — so the cap is set
/// far above any plausible number of triggers an operator has outstanding
/// against one instance rather than at the one or two the honest case needs.
const MAX_ABANDONED_ACTION_REPLIES: usize = 64;

// ---------------------------------------------------------------------
// Public command / handle surface
// ---------------------------------------------------------------------

/// Distinguishes one client connection from another, for the lifetime of that
/// connection and no longer.
///
/// Minted per accepted connection by the server layer and never reused within
/// a daemon's life. The only thing scoped by it today is smits, whose whole
/// lifecycle rule is "they belong to the connection that painted them" — which
/// is what makes them ephemeral without cleanup logic on every path that can
/// stop a dog.
///
/// It lives here rather than in `server`, which is where it is minted and
/// where its doc would otherwise sit: that module is `#[cfg(unix)]`, and this
/// one is not. A `ConnId` named from here on a Windows build would not
/// resolve, and the workspace's own windows-gnu cross-check is a gate that
/// would catch it.
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
/// Keyed by NAME, not by instance id. A sheep can run several instances, and
/// one smit per entry would mean fanning out at publish time and then keeping
/// it in step as instances come and go — an instance spawned five seconds
/// after a publish would show nothing until the publisher's next tick. Every
/// instance of a named sheep reads the same mark, including one spawned a
/// moment ago.
type Smits = HashMap<String, (ConnId, String)>;

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
    /// Registers each app as a flock member without spawning anything.
    ///
    /// Restoring a muster roll is the only caller. The roll records
    /// membership and running counts separately, and this is the half that
    /// puts membership back: a sheep saved while stopped returns stopped and
    /// restartable rather than vanishing.
    RegisterAtRest {
        /// Already-validated app specs to register, one entry each.
        apps: Vec<ResolvedApp>,
        /// Answers with every entry now registered, in the order given.
        reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
    },
    /// Reports which of `apps` name a sheep registered under a different
    /// config.
    ///
    /// Read-only, so it is answered during a shutdown like
    /// [`Self::RegisterAtRest`] rather than refused like [`Self::Start`]:
    /// nothing is registered, spawned or changed, so there is no child it
    /// could leave outside the shutdown aggregation.
    ConfigDrift {
        /// Already-validated app specs to compare against the flock, exactly
        /// as [`Self::Start`] would carry them.
        apps: Vec<ResolvedApp>,
        /// Answers with one entry per app that is both registered and
        /// different, and no entry for anything else.
        reply: oneshot::Sender<Result<Vec<SheepDrift>, SupervisorError>>,
    },
    /// Registers + spawns one dog, marked with where it came from.
    ///
    /// Separate from [`Self::Start`] only because of what it WRITES — the
    /// marker, which no Flockfile may declare — and because it is idempotent
    /// by name. Everything downstream of the registration is the same code
    /// path, deliberately: a dog is supervised exactly as a sheep is.
    StartDog {
        /// The dog's already-validated app spec, built by the daemon rather
        /// than read from a Flockfile.
        ///
        /// Boxed where [`Self::Start`]'s `Vec` is already indirection: a
        /// bare [`ResolvedApp`] here is the largest thing in this enum by an
        /// order of magnitude, and every [`Msg`] the actor ever receives
        /// would be sized for it.
        app: Box<ResolvedApp>,
        /// Where this dog came from, written onto its entry.
        source: DogSource,
        /// Answers with the dog's instance — the one just started, or the
        /// one that was already registered under this name.
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
    /// Sets one app's instance count. See [`Actor::handle_scale`].
    Scale {
        /// The app's name, exactly as its config spells it. Not a selector —
        /// see [`shep_core::protocol::Request::Scale`] for why.
        name: String,
        /// How many instances the app has when this returns.
        count: u32,
        /// Answers with the app's surviving instances and its new config.
        reply: oneshot::Sender<Result<Scaled, SupervisorError>>,
    },
    /// Attaches a marker to one sheep by name, or clears it.
    ///
    /// # Last writer wins
    ///
    /// `Some` overwrites whatever is already there, including a mark another
    /// connection painted. That is deliberate rather than an oversight: there
    /// is one column and one string, and shep is not going to arbitrate
    /// between dogs. A `None`, by contrast, only takes effect when the stored
    /// [`ConnId`] matches, so one dog cannot wipe another's mark — clearing
    /// somebody else's would be a silent removal nobody could attribute,
    /// where an overwrite at least leaves a mark on screen.
    SetSmit {
        /// The connection painting it — the scope the mark lives in.
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
    /// Sent from the server layer's per-connection tail, which is the one
    /// block that runs on every path out of a connection. That is the whole
    /// of a smit's cleanup: `shep disable`, `shep rehome`, a dog crashing, a
    /// daemon restart and a deliberate reconnect all end a socket, so all
    /// five drop the marks without anyone editing five code paths.
    ForgetSmits {
        /// The connection that has ended.
        conn: ConnId,
        /// Answers once the actor has processed the removal, so the caller
        /// knows the marks are gone rather than merely queued.
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
    /// Puts one named action on the shepherd channel of every sheep matching
    /// `selector` and answers with what each app said back, or with why
    /// nothing came.
    ///
    /// One row per matched sheep, because an answer is per-instance — one
    /// reply body from one process — where every other selector-in verb has
    /// one lifecycle outcome to report per sheep and can list them as a
    /// flock.
    Trigger {
        /// Which sheep.
        selector: ProcessSelector,
        /// The action name, passed to the app verbatim.
        action: String,
        /// Argument text for the action, passed to the app verbatim.
        params: Option<String>,
        /// Answers once every matched sheep has answered, timed out or been
        /// refused — off a task of its own, never the actor loop (see
        /// [`Actor::begin_action`]).
        reply: oneshot::Sender<Result<Vec<ActionReply>, SupervisorError>>,
    },
    /// Delivers one signal to the OWN process of every sheep matching
    /// `selector` — never its process group (see [`Actor::begin_signal`]).
    Signal {
        /// Which sheep.
        selector: ProcessSelector,
        /// The signal to deliver.
        sig: OperatorSignal,
        /// Answers once every matched sheep has been signalled or found not
        /// running — off a task of its own, never the actor loop (see
        /// [`Actor::begin_signal`]).
        reply: oneshot::Sender<Result<Vec<SignalReply>, SupervisorError>>,
    },
    /// Writes one line to every matched sheep's stdin.
    SendLine {
        /// Which sheep.
        selector: ProcessSelector,
        /// The line, without its terminator — the writer appends exactly one
        /// `\n`.
        line: String,
        /// Answers once every matched sheep's write has settled or timed
        /// out, off a task of its own, never the actor loop (see
        /// [`Actor::begin_send_line`]).
        reply: oneshot::Sender<Result<Vec<LineReply>, SupervisorError>>,
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
    /// One swap of a reload ran out of time.
    ///
    /// The only way out of a [`ReloadJob`] that the actor raises for itself.
    /// Every other one is a message from a task the actor cannot make report
    /// — `Msg::Exited` from a sheep task, `Msg::ReadyResult` from a readiness
    /// task — so a swap whose message never arrives has, without this, no way
    /// to end at all.
    ReloadDeadline {
        /// The app whose reload this was armed for.
        name: String,
        /// The replacement's id when it was armed, which is what stamps the
        /// swap. Ids are never reused, so a deadline naming anything other
        /// than the app's current `swap.new_id` belongs to a swap that has
        /// already ended and is dropped — the same staleness rule
        /// [`Self::RestartDue`] applies with an epoch.
        new_id: u32,
    },
    /// The sheep's shepherd channel carried a reply to an action.
    ///
    /// Routed to the waiting action task, if one is waiting — dropped
    /// silently otherwise, exactly as `Msg::Ready` is. `stamp` is the
    /// dispatch id the app echoed, when it echoed one; without it the only
    /// correlation the app gave us is the action NAME, and which wait the
    /// reply belongs to is a question [`ActionWaits::answer`] answers rather
    /// than one the message carries.
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
        /// Which wait on that sheep this is the answer to. A stamp of its
        /// own, not an existing never-reused fact, for the reason
        /// [`PendingAction::stamp`] gives.
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
///
/// `#[non_exhaustive]`: eight variants today cover lookup, spawn, a batch
/// refused before it was registered, reload overlap, an invalid scale, and
/// the two log-maintenance failure classes.
/// The doc here used to forecast "a scale or pause verb" adding its own
/// failure variant; the scale half of that has now landed as
/// [`Self::InvalidScale`], and a pause verb, if one is ever built, is the
/// next candidate for the same treatment rather than a reason to overload an
/// existing variant. shep-daemon is a published library an out-of-tree
/// matcher should not break for (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    /// The selector matched no registered sheep.
    NotFound,
    /// Spawn failed (carries the runner's message).
    SpawnFailed(String),
    /// A `Start` batch was refused before anything was registered, because
    /// at least one app in it provably could not run. Carries one
    /// `"<name>: <reason>"` entry per such app, joined by `"; "`, behind a
    /// count and the fact that nothing was registered.
    ///
    /// Separate from [`Self::SpawnFailed`] because NOTHING WAS SPAWNED, and
    /// an operator reading "spawn failed" about a spawn that never happened
    /// is being told something untrue about where to look. The two also
    /// differ in what they leave behind, which is the part that matters
    /// operationally: a `SpawnFailed` can leave earlier apps in the batch
    /// registered and running, while this one guarantees an untouched flock.
    ///
    /// Maps to
    /// [`RpcErrorCode::SpawnFailed`](shep_core::protocol::RpcErrorCode::SpawnFailed)
    /// all the same, on the rule this file already applies to
    /// [`Self::ReloadInFlight`]: `RpcErrorCode` is versioned, a client that
    /// predates a new code cannot decode the reply at all, and that would
    /// cost the operator the message as well as the code. "Could not start
    /// it", and the exit code that goes with it, is true of both.
    CannotStart(String),
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
    /// A `Scale` the engine will not perform; carries the refusal in plain
    /// English, naming what to do instead.
    ///
    /// Four shapes reach it: a count of `0` (`normalize` refuses
    /// `instances == 0`, so accepting it here would admit a config the
    /// engine's own validator rejects — `shep delete` is the verb), a target
    /// that is a dog (one process by contract, spec §8), an app with
    /// departures still in flight (`Actor::handle_scale` has the account), and a
    /// rescaled config that failed `normalize`, which is unreachable through
    /// this path and is carried rather than `expect`ed because a supervisor
    /// does not panic on peer input.
    ///
    /// Maps to [`RpcErrorCode::InvalidConfig`](shep_core::protocol::RpcErrorCode::InvalidConfig),
    /// not `Internal`: every one of those is something the caller asked for
    /// that it can ask differently.
    ///
    /// The departures shape is a CONFLICT — ask again in a moment, not ask
    /// something else — and so has the same claim to a code of its own that
    /// [`Self::ReloadInFlight`] has and does not get. It rides here rather
    /// than in a variant of its own for two reasons. It is a scale the
    /// engine will not perform, carrying its refusal in plain English and
    /// naming the wait, which is exactly this variant's contract. And the
    /// only place a new variant could map to today is `Internal`, where
    /// `ReloadInFlight` already sits under protest: filing an actionable
    /// refusal under "unexpected daemon-side failure" would be strictly
    /// worse for the operator than `InvalidConfig`, which at least says the
    /// request is the thing to change. A conflict code on the wire is the
    /// real fix, and it is a protocol change both refusals should move to at
    /// once.
    InvalidScale(String),
    /// At least one log pump could not open a log path again, so that stream
    /// has no file to write to. Carries one
    /// `"<name> (id <id>): <paths and reasons>"` entry per such sheep,
    /// joined by `"; "`. Every other pump was reopened.
    ///
    /// The sheep named can be one the selector did not match. The reach is
    /// every writer to a matched path — several instances of an app can share
    /// one log file — so a sibling that could not be reopened is reported
    /// rather than swallowed.
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
            Self::CannotStart(msg) => write!(f, "start refused: {msg}"),
            Self::ReloadInFlight(name) => write!(f, "{name} is already being reloaded"),
            Self::InvalidScale(msg) => write!(f, "cannot scale: {msg}"),
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

    /// Registers each app as a flock member without starting it.
    ///
    /// Idempotent by name: an app already known is returned as it stands, so
    /// restoring a roll over a live flock disturbs nothing.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
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

    /// Names the fields in which each app differs from the flock's own copy
    /// of the sheep of the same name.
    ///
    /// Reads the flock and changes nothing. [`Self::start`] on a name the
    /// flock already has adds instances rather than reconciling config,
    /// which is what `shep stock` depends on; this is how a caller finds out
    /// that an edit it just read from a Flockfile is one `start` will not
    /// apply, rather than the edit vanishing without a word.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`] - the actor is gone.
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

    /// Registers and starts one dog, marked as coming from `source`.
    ///
    /// Idempotent by name: a dog already registered under `app`'s name is
    /// reported as it stands rather than started twice, which is what makes
    /// `shep enable` safe to run against a daemon that already has the dog.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::EngineStopped`] — shutdown has begun, or the
    ///   actor is gone.
    /// - [`SupervisorError::SpawnFailed`] — the binary could not be spawned.
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

    /// Sets `name`'s instance count.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — no app of that name is registered.
    /// - [`SupervisorError::InvalidScale`] — a count of `0`, or a target that
    ///   is a dog.
    /// - [`SupervisorError::ReloadInFlight`] — the app is mid-reload.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    ///
    /// A scale-up that ran out of instances part-way is NOT an error here: it
    /// answers `Ok` with [`Scaled::shortfall`] set, the achieved count on
    /// [`Scaled::app`], and the instances that did come up still running. See
    /// [`Scaled`]'s own doc for why, and `crate::rpc` for what the operator is
    /// told.
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
    /// `smit` arrives as a [`Smit`](shep_core::protocol::Smit) and stays one
    /// the whole way down. The type did its real work at the wire's edge,
    /// which is where a third party's text has to be refused, but carrying
    /// it further costs nothing and means the compiler rather than a comment
    /// is what stops a later caller inside this crate from handing over a
    /// string nothing checked. It becomes a `String` only at the map insert,
    /// where `Smits` stores the rendered text against the name.
    ///
    /// A clear from a connection that did not paint the mark is a no-op that
    /// still answers `Ok` — see [`Command::SetSmit`] for why one dog may
    /// overwrite another's mark but not remove it.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — no sheep of that name is registered.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
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
    /// Answers `Ok(())` on an engine that has already stopped: a daemon on
    /// its way down has dropped every smit it held by definition, and the
    /// caller is a connection tail that has nothing useful to do with a
    /// failure.
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

    /// Reopens the log files of every sheep matching `selector` — and of
    /// every other sheep writing to one of their paths — for an external
    /// rotator that has renamed them.
    ///
    /// Answers only once every one of those pumps has swapped both handles,
    /// which is the contract a logrotate `postrotate` stanza needs: when this
    /// returns, no live pump is still holding a renamed inode. A matched sheep
    /// that is not running has no pump and nothing to reopen, and is reported
    /// as a success alongside the rest. The reply names the sheep the selector
    /// reached and no others — see [`Actor::handle_reopen`] for why the work
    /// is wider than the answer.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::ReopenFailed`] — every pump reached answered,
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

    /// Sends `action` over the shepherd channel of every sheep matching
    /// `selector` and answers with one id-sorted row per match, carrying what
    /// each app said back or why nothing came.
    ///
    /// Answers on completion rather than on acceptance: an action has no
    /// floor on how long it takes, so an acceptance would tell a caller
    /// nothing it did not already know. Each matched sheep's own
    /// `AppConfig::action_timeout` is what bounds that — read at wait time
    /// from its config, not passed in here — and it should be set below the
    /// caller's own RPC budget, or the caller gives up before this honest
    /// answer reaches it. The waits run alongside each other, so a whole
    /// flock costs whichever matched sheep's own timeout is longest, never
    /// the sum of them.
    ///
    /// A sheep that cannot be reached is refused in its own row rather than
    /// failing the whole request: spec §9's selector grammar (`all`,
    /// `/regex/`, `fold:`) makes a mixed flock the normal case, and a refusal
    /// that took the reachable sheep down with it would leave the operator
    /// unable to tell which half was taken. [`ActionOutcome::NoChannel`] is
    /// one such row, [`ActionOutcome::Skipped`] the other.
    ///
    /// `action` and `params` are passed to the app verbatim and are never
    /// read here. The daemon holds no list of an app's actions and does not
    /// validate the name against one: what an app does with an action it does
    /// not recognise, including replying to say so, is the app's to decide.
    ///
    /// An outcome is a report and not a promise of delivery.
    /// [`ActionOutcome::Replied`] is the only one that proves the action
    /// reached the app, because a reply is the only proof there is — the
    /// daemon's own send says nothing, since the first one after a child has
    /// exited is accepted and discarded.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
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

    /// Delivers `sig` to the OWN process of every sheep matching `selector` —
    /// never its process group — and answers with one id-sorted row per
    /// match.
    ///
    /// Unlike [`Self::trigger`], there is nothing to wait out: a `kill(2)`
    /// either returns or does not, so this answers as soon as every matched
    /// sheep's delivery has settled rather than on some app-configured
    /// timeout.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
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

    /// Writes `line` to every matched sheep's stdin, and answers with one
    /// id-sorted row per match.
    ///
    /// Unlike [`Self::signal`], each write can genuinely wait — a pipe write
    /// blocks until the app reads — so the reply is bounded per sheep at
    /// [`STDIN_WRITE_TIMEOUT`] rather than resolving as soon as delivery is
    /// attempted.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — nothing matched.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
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
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn list_checked(&self) -> Result<Vec<ProcessInfo>, SupervisorError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Command(Command::List { reply }))
            .await
            .map_err(|_| SupervisorError::EngineStopped)?;
        rx.await.map_err(|_| SupervisorError::EngineStopped)
    }

    /// Full flock listing, grouped by app name (each app's instances kept
    /// in their own instance-slot order, ties from a reload broken by id).
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
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: self.extras,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
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
    /// [`ReloadState::Drainee`] from the moment the replacement is spawned.
    old_id: u32,
    /// Its replacement, in the same instance slot under a new id. Carries
    /// [`ReloadState::Replacement`] until the swap finishes.
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

/// One action the daemon has put on a sheep's shepherd channel and has not
/// finished waiting on.
#[derive(Debug)]
struct PendingAction {
    /// Which wait this is, for the whole life of the daemon.
    ///
    /// A counter of its own, where a reload's deadline stamps itself with the
    /// replacement's `new_id` instead (see [`Msg::ReloadDeadline`]). That
    /// swap could reuse an id because a swap IS a replacement id — one fact
    /// serving two purposes, with no second copy free to drift. An action has
    /// no such fact to borrow. The sheep's id names the instance, not the
    /// action, and so does [`SheepSlot::epoch`]: both stay put while a
    /// process is triggered a dozen times, so either would stamp a dozen
    /// waits identically. The counter is not a duplicate of anything, because
    /// there was nothing to duplicate.
    stamp: u64,
    /// The action name this is waiting for a reply to. What
    /// [`ActionWaits::answer`] falls back to matching on when the app's
    /// reply does not echo this wait's `stamp`.
    action: String,
    /// Wakes the waiting task with the app's reply body.
    ///
    /// Taken by the reply that answers it, so the second reply to one action
    /// finds nothing to hand a body to. The entry stays behind until the task
    /// reports what it made of the body, which is what keeps `reply` below
    /// reachable for the one message that resolves this wait.
    waiter: Option<oneshot::Sender<String>>,
    /// Where this wait's outcome goes once it has one — the row-building
    /// half of [`Actor::begin_action`], which turns it into one
    /// [`ActionReply`].
    ///
    /// A bare outcome and not a `Result`: everything that could fail about
    /// one sheep's action is decided before a wait is armed, by the selector
    /// pass that found the sheep in the first place.
    reply: oneshot::Sender<ActionOutcome>,
}

/// One reply a sheep's app still owes a wait that has already ended.
///
/// The `stamp` is what separates a late reply from a prompt one when both
/// name the same action: an app that echoes lets the daemon settle exactly
/// the debt it belongs to, and an app that does not is matched by `action`
/// and by order, the only signal the channel gives on its own.
#[derive(Debug)]
struct AbandonedReply {
    /// The wait that ended without this reply.
    stamp: u64,
    /// Its action name — the fallback key for an app that does not echo.
    action: String,
}

/// What one sheep still owes on its shepherd channel: the action waits armed
/// against it, and the replies its app can still send that no wait wants.
///
/// # Why the second half exists
///
/// An app is free to answer an action after that action's wait has given up.
/// Since Phase 10 the daemon stamps every dispatch and an app may echo that
/// stamp back, in which case a late reply is unambiguous and settles its own
/// debt with nothing else at risk. An app that does not echo leaves the
/// daemon with the action NAME and nothing else, and a late reply to a `gc`
/// that timed out is then byte-identical to a prompt reply to a `gc`
/// triggered afterwards. Handing that to the second wait answers an
/// operator's question with another operator's answer — a wrong answer, not
/// an error, and the sharpest failure this type exists to prevent.
///
/// What separates them for an unstamped app is order, which is the one thing
/// the channel preserves on its own: a child reads its actions in the order
/// they were written and its replies arrive in the order it wrote them. So a
/// wait that ends without its reply leaves a debt behind, and the next
/// unstamped reply naming that action pays the debt instead of the live wait.
/// Only once the debt is settled does an unstamped reply of that name reach a
/// wait again. Echoing the stamp is how an app opts out of that whole
/// mechanism.
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

    /// Routes one reply to `action` — stamped with `stamp` if the app echoed
    /// the dispatch's `id` — to the waiter it belongs to, or `None` if it
    /// belongs to nothing.
    ///
    /// Two paths, and which one runs is the app's choice, not a mode:
    ///
    /// - **Stamped.** The reply names its own dispatch, so it goes to the
    ///   live wait carrying that stamp; failing that, it settles that stamp's
    ///   own debt; failing that, it belongs to nothing. A live wait for the
    ///   same action name is never touched by another wait's reply, which is
    ///   the correctness gap this path closes (wire.md #2).
    /// - **Unstamped.** Byte-identical to the behaviour before stamping
    ///   existed: the oldest debt of that name is settled first, and only
    ///   once the debt is clear does a reply of that name reach a live wait.
    ///   Order is the only signal an unstamped channel gives, and this is
    ///   what makes of it what can be made.
    ///
    /// `None` still covers three ordinary shapes on both paths and none of
    /// them is an error: a debt settled, a second reply to an action already
    /// answered, or a reply the app volunteered without being asked.
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
    /// `None` for a stamp no live wait carries, which is what a result for a
    /// sheep whose process has since gone looks like — [`Self::abandon_all`]
    /// answered it already.
    fn resolve(&mut self, stamp: u64) -> Option<oneshot::Sender<ActionOutcome>> {
        let at = self
            .live
            .iter()
            .position(|pending| pending.stamp == stamp)?;
        let pending = self.live.remove(at);
        // A waiter still sitting here is a wait that ended without the reply
        // it asked for. The app owes that reply for as long as its process
        // lives, and the debt is what stops the reply being read as an answer
        // to something else.
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
    /// debt — what a sheep's process ending does to both halves at once.
    ///
    /// The debts go because they are owed by a process that no longer exists:
    /// a replacement under the same id has written none of those replies, and
    /// keeping the entries would have it swallow the first few answers it
    /// gives. The live waits are answered rather than dropped because a
    /// dropped `reply` reaches its caller as the engine having gone away,
    /// which is not what happened — the sheep's channel did.
    fn abandon_all(&mut self) {
        for pending in self.live.drain(..) {
            let _ = pending.reply.send(ActionOutcome::NoChannel);
        }
        self.abandoned.clear();
    }
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
    /// A clone of the [`ProcIo::to_child`] the most recent successful spawn
    /// handed out — the daemon's writing end of this sheep's shepherd
    /// channel, and the actor's only way to reach a live child directly.
    /// `None` whenever no process is running under this id.
    ///
    /// Deliberately not carried as a `ctl` message. That mailbox holds
    /// [`SHEEP_CTL_CAPACITY`] messages, [`Self::ctl`]'s senders `try_send`
    /// into it, and [`Actor::claim_manual`] ignores a `Full` there *because a
    /// queued [`SheepCtl::Kill`] means the ladder is already running*. That
    /// argument holds only while `Kill` is the sole occupant of those four
    /// slots: put anything else in them and the same code can drop a `Kill`.
    /// The shepherd channel is the child's own, several times wider, and
    /// carries nothing but child traffic — so a full one there means what it
    /// says, that the child is not reading fd 3.
    ///
    /// # Why this one is cleared and `log_ctl` is not
    ///
    /// Every argument in `log_ctl`'s doc above turns on a pump having a
    /// second way to end — its `logs` receiver going away with the sheep task
    /// — so a clone kept on the slot can delay nothing. The other end of THIS
    /// channel has no such branch. `tokio_runner`'s writer task parks on
    /// `recv()`, and its only other exit is a write that fails, which cannot
    /// happen while nothing is being sent. A clone left here past the exit
    /// therefore parks that task for as long as the sheep stays registered,
    /// holding the daemon's half of the socketpair with it: one leaked task
    /// and one leaked descriptor per exit, on a daemon that runs for months.
    to_child: Option<mpsc::Sender<ShepherdMessage>>,
    /// Sender for this sheep's signal mailbox — a live sheep task's second
    /// mailbox, separate from [`Self::ctl`]. `None` whenever no process is
    /// running under this id.
    ///
    /// A mailbox of its own rather than a [`SheepCtl`] variant, and this is
    /// not tidiness. [`Self::ctl`]'s queue is bounded at
    /// [`SHEEP_CTL_CAPACITY`], its senders `try_send` into it, and
    /// [`Actor::claim_manual`] ignores a `Full` there *because a queued
    /// [`SheepCtl::Kill`] means the ladder is already running*. That argument
    /// holds only while `Kill` is the sole occupant of those four slots: put
    /// anything else in them and the same code can drop a `Kill`. A burst of
    /// signals sharing that queue would make `claim_manual` drop a stop and
    /// report success for it. Cleared alongside [`Self::to_child`], for the
    /// same reason that field is: the receiving task parks on `recv()`, and a
    /// sender left on a dead slot parks it for as long as the sheep stays
    /// registered.
    signals: Option<mpsc::Sender<SignalRequest>>,
    /// A clone of the [`ProcIo::to_stdin`] the most recent successful spawn
    /// handed out — the daemon's writing end of this sheep's stdin pipe.
    /// `None` whenever no process is running under this id, or whenever the
    /// running one never asked for a pipe at all (`AppConfig::stdin ==
    /// false`), in which case the sender is present but closed — see
    /// [`Self::open_stdin`].
    ///
    /// Cleared with [`Self::to_child`], for the reason that field's doc
    /// gives: the far end parks on `recv()`, and a sender left here past the
    /// process's exit parks that task for as long as the sheep stays
    /// registered.
    ///
    /// **No `.await` on this sender may appear on the actor loop.** A sheep
    /// whose app has stopped reading fd 0 fills its 64 KiB pipe; the writer
    /// task then blocks in `write_all` and stops draining its `mpsc`, so an
    /// `.await`ed send into a full queue would park whatever task made the
    /// call — and on the actor loop that task is the actor itself, which
    /// would stop supervising every other sheep in the flock over one app
    /// that is not reading its stdin. [`Actor::begin_send_line`] enqueues
    /// with `try_send` for exactly this reason.
    to_stdin: Option<mpsc::Sender<StdinWrite>>,
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
    /// The action waits armed against this sheep and the replies its app
    /// still owes ones that have ended — see [`ActionWaits`].
    ///
    /// Cleared with [`Self::to_child`], and for a sharper version of the same
    /// reason: a wait armed against a process that has exited is waiting for
    /// a reply nobody will ever write, and the debts it leaves behind belong
    /// to a process a replacement under this id never was.
    actions: ActionWaits,
}

impl SheepSlot {
    /// This sheep's shepherd-channel sender while something is still there to
    /// receive on it, and `None` when nothing is.
    ///
    /// The one fact that decides whether an action can be delivered at all,
    /// read off the channel rather than off `AppConfig::channel` so there is
    /// no second copy of it free to disagree. Both halves matter and neither
    /// implies the other: [`Self::to_child`] is cleared when a process ends
    /// under this id, while `is_closed` catches a sender whose far end went
    /// first — which is what an app configured without a channel has from the
    /// moment it spawns, since the runner drops the receiving end rather than
    /// leaving it dangling.
    fn open_channel(&self) -> Option<&mpsc::Sender<ShepherdMessage>> {
        self.to_child
            .as_ref()
            .filter(|to_child| !to_child.is_closed())
    }

    /// This sheep's stdin sender while something is still there to receive on
    /// it, and `None` when nothing is.
    ///
    /// Read off the channel rather than off `AppConfig::stdin` so there is no
    /// second copy of the fact free to disagree — exactly as
    /// [`Self::open_channel`] reads the channel rather than `AppConfig::channel`.
    /// Both halves matter: `to_stdin` is cleared when a process ends under this
    /// id, and `is_closed` catches an app that never asked for a pipe, whose
    /// receiver the runner dropped at spawn.
    fn open_stdin(&self) -> Option<&mpsc::Sender<StdinWrite>> {
        self.to_stdin
            .as_ref()
            .filter(|to_stdin| !to_stdin.is_closed())
    }
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

/// What a completed [`Command::Scale`] produced.
///
/// Two things, because two different layers need one each:
/// [`Actor::handle_scale`]'s caller replies with `instances` and re-records
/// `app` in the muster roll. Returning the config rather than having the
/// actor reach into the roll keeps [`crate::snapshot::FlockRegistry`] a
/// thing the rpc layer owns, which is where `Request::Start` already keeps
/// it.
///
/// # Why a scale that fell short is still a `Scaled`
///
/// A partial scale-up is a partial SUCCESS: some instances came up, they are
/// serving traffic, and every registered slot has been rewritten to the count
/// really running. Reporting that as a flat `Err` throws away the config the
/// caller needs to record, which is how the muster roll came to keep the
/// PRE-scale count after a partial scale — a `shep muster` or a reboot then
/// discarding healthy instances nobody asked it to stop. So the shortfall
/// rides along in [`Self::shortfall`] and the caller decides what the operator
/// is told; recording is unconditional, and only the operator's exit code
/// turns on whether the request was fully satisfied.
#[derive(Debug)]
pub(crate) struct Scaled {
    /// The app's surviving instances, in instance-slot order. On a partial
    /// scale-up this is what came up, never the count asked for.
    pub(crate) instances: Vec<ProcessInfo>,
    /// The app's config as it now stands, with the ACHIEVED `instances` count.
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
    /// disagree: they are written from the same survivor list, and a build
    /// that let them drift is one this returns the wrong number for loudly.
    pub(crate) fn achieved(&self) -> u32 {
        u32::try_from(self.instances.len()).unwrap_or(u32::MAX)
    }
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
    /// Monotonic stamp counter for action waits — see
    /// [`PendingAction::stamp`] for why an action needs one of its own.
    next_action_stamp: u64,
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
    /// Every sheep name currently carrying a smit — see [`Smits`]. Empty is
    /// the ordinary state; a dog painting one is what puts an entry here, and
    /// that dog's connection closing is what takes it away again.
    smits: Smits,
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
                Msg::ReloadDeadline { name, new_id } => {
                    self.handle_reload_deadline(&name, new_id);
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
            // CRITICAL-1: Start is rejected outright once shutdown has
            // begun — it would register + spawn a child the shutdown
            // aggregation (computed from `online` ids at the moment it ran)
            // can never know to kill, orphaning it after the actor exits.
            Command::Start { apps, reply } => {
                let result = if self.shutting_down {
                    Err(SupervisorError::EngineStopped)
                } else {
                    self.do_start(apps, None)
                };
                let _ = reply.send(result);
                false
            }
            // Not rejected while `shutting_down`, unlike Start: this
            // spawns nothing, so it can leave no child outside the shutdown
            // aggregation. It is still pointless during a shutdown, and
            // nothing calls it there.
            Command::RegisterAtRest { apps, reply } => {
                let registered = apps.iter().map(|app| self.register_at_rest(app)).collect();
                let _ = reply.send(Ok(registered));
                false
            }
            // Answered during a shutdown, unlike Start and for the mirror of
            // CRITICAL-1's reason: it registers and spawns nothing, so there
            // is no child it could leave outside the shutdown aggregation.
            Command::ConfigDrift { apps, reply } => {
                let _ = reply.send(Ok(self.config_drift(&apps)));
                false
            }
            // Rejected while `shutting_down` under CRITICAL-1's rule, the one
            // Start follows and for the same reason: a dog spawned after the
            // shutdown aggregation was computed is a child nothing will kill.
            Command::StartDog { app, source, reply } => {
                let result = self.do_start_dog(*app, source);
                let _ = reply.send(result);
                false
            }
            Command::Scale { name, count, reply } => {
                self.handle_scale(&name, count, reply);
                false
            }
            // Neither is rejected while `shutting_down`: a smit registers
            // nothing and spawns nothing, and a daemon on its way down is
            // about to drop the whole map anyway.
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
            // Not rejected while `shutting_down` either, for the reason
            // above: an action registers nothing and spawns nothing. What it
            // reaches is a child the shutdown's own kill ladder is already
            // taking down, and the wait's timeout is what bounds the answer
            // if that child stops reading its channel first.
            Command::Trigger {
                selector,
                action,
                params,
                reply,
            } => {
                self.begin_action(&selector, action, params, reply);
                false
            }
            // Not rejected while `shutting_down`, for the same reason as
            // Trigger above: a signal registers nothing and spawns nothing.
            Command::Signal {
                selector,
                sig,
                reply,
            } => {
                self.begin_signal(&selector, sig, reply);
                false
            }
            // Not rejected while `shutting_down`, for the same reason as
            // Signal above: a line registers nothing and spawns nothing.
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

    /// Registers + spawns one dog, or reports the one already registered
    /// under that name.
    ///
    /// The name lookup is what makes this idempotent, and it reads names
    /// rather than markers: two live processes under one name is the outcome
    /// being ruled out, whichever population the entry already there belongs
    /// to.
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
        let started = self.do_start(vec![app], Some(source))?;
        started
            .into_iter()
            .next()
            .ok_or_else(|| SupervisorError::SpawnFailed("the dog registered no instance".into()))
    }

    /// Expands each app through `instance_slots` + `assemble`, spawning one
    /// instance per slot, after checking every app in the batch.
    ///
    /// Nothing at all is registered if any app fails that check, and the
    /// error names every one that did rather than the first. The defect
    /// this exists for: the third app of an eleven-app Flockfile pointed at
    /// an unbuilt binary, so apps one and two registered and started, app
    /// three failed to spawn, and apps four through eleven were never
    /// reached. The flock then matched neither the file nor its previous
    /// state.
    ///
    /// A spawn that fails ANYWAY still leaves the batch part-registered, and
    /// that is not a case this can close: exec is the only thing that knows
    /// for certain, and the first instance is already running by the time
    /// the second one is attempted. What it closes is the knowable half, and
    /// [`ProcessRunner::preflight`] is deliberately narrow about what counts
    /// as knowable.
    ///
    /// `dog` is written onto every entry this registers, and is `None` for
    /// every caller but [`Self::do_start_dog`] — see [`ProcessEntry::dog`]
    /// for why the marker rides the entry rather than a registry of its own.
    fn do_start(
        &mut self,
        apps: Vec<ResolvedApp>,
        dog: Option<DogSource>,
    ) -> Result<Vec<ProcessInfo>, SupervisorError> {
        // Assembled at instance 0 and with no credentials: neither changes
        // which file exec names, which is all `preflight` reads. The real
        // per-instance spec is built again below.
        let mut refusals = Vec::new();
        // Resolved once per app in this Start batch, not once per instance:
        // every instance of the same app shares one identity, and respawn()
        // reuses this same value from ProcessEntry for every future restart
        // instead of re-touching the passwd database (crate::privilege's
        // module doc). Hoisted above the registering loop for this
        // function's own reason: a passwd lookup that fails on the fourth
        // app must not leave three registered either.
        let mut credentials = Vec::with_capacity(apps.len());
        for app in &apps {
            let name = &app.config().name;
            match privilege::resolve(app.config()) {
                Ok(resolved) => credentials.push(resolved),
                Err(err) => refusals.push(format!("{name}: {err}")),
            }
            match self.runner.preflight(&assemble(app, 0, &self.paths, None)) {
                Preflight::Unknown => {}
                Preflight::Impossible(reason) => refusals.push(format!("{name}: {reason}")),
                // Reported, never refused. A `Doubtful` is a claim about the
                // daemon's environment rather than about a path, and the
                // daemon's environment under the unit `shep startup`
                // installs is not the shell an operator tested in: refusing
                // here would keep a whole flock down at boot because one
                // app's `node` is in `/opt/homebrew/bin`. That app's spawn
                // fails on its own in a moment, naming the same program.
                //
                // The log rather than the reply: the reply is for the batch,
                // and this is not a batch failure. At boot there is no
                // operator holding a terminal to send it to either, and the
                // shepherd's log is where they look.
                Preflight::Doubtful(reason) => {
                    tracing::warn!(sheep = %name, "{reason}");
                }
            }
        }
        if !refusals.is_empty() {
            return Err(SupervisorError::CannotStart(format!(
                "nothing was registered; {} of {} apps cannot start: {}",
                refusals.len(),
                apps.len(),
                refusals.join("; "),
            )));
        }

        let mut results = Vec::new();
        // Lengths agree exactly here: every app above pushed onto one of the
        // two vectors, and a non-empty `refusals` has already returned.
        for (app, credentials) in apps.into_iter().zip(credentials) {
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
                    // The sheep's name, which `spawn_fresh`'s message
                    // deliberately leaves to its caller: one app of eleven
                    // failing must say WHICH one.
                    Err(message) => {
                        return Err(SupervisorError::SpawnFailed(format!("{name}: {message}")));
                    }
                }
            }
        }
        Ok(results)
    }

    /// Registers one app as a member of the flock without spawning anything.
    ///
    /// The flock is a membership list, not a list of live processes. `stop`
    /// leaves a sheep registered and `Stopped`; `delete` is what ends
    /// membership. Restoring a roll has to be able to say the same thing, and
    /// before this existed it could not: the only way into the flock was
    /// [`Self::do_start`], so a sheep saved while stopped came back as
    /// nothing at all and `shep restart <name>` could not find it.
    ///
    /// One entry per app rather than one per configured instance. A stopped
    /// sheep has no instances running by definition, and `instance: 0` is the
    /// slot `start` would fill first, so a later `restart` lands where it
    /// would have.
    ///
    /// Idempotent by name, like [`Self::do_start_dog`]: an app already known
    /// is left exactly as it is, so restoring a roll over a live flock never
    /// disturbs what is already running.
    fn register_at_rest(&mut self, app: &ResolvedApp) -> ProcessInfo {
        let name = &app.config().name;
        if let Some(slot) = self
            .sheep
            .values()
            .find(|slot| &slot.entry.spec.config().name == name)
        {
            return to_info(&slot.entry, &self.smits);
        }

        let id = self.next_id;
        self.next_id += 1;
        // Assembled for its log paths only: nothing is spawned here, but the
        // entry has to name the files a later `restart` will append to, the
        // same way the failure arm of `spawn_fresh` does.
        let spec = assemble(app, 0, &self.paths, None);
        let entry = ProcessEntry {
            id,
            spec: app.clone(),
            instance: 0,
            status: ProcStatus::Stopped,
            pid: None,
            restarts: 0,
            started_at: None,
            budget: RestartBudget::default(),
            reload: ReloadState::None,
            credentials: None,
            out_file: spec.out_file.clone(),
            err_file: spec.err_file.clone(),
            dog: None,
            // A fresh registration, never spawned: nothing has exited yet.
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
            },
        );
        info
    }

    /// Names the fields in which each app differs from the registered sheep
    /// of the same name, skipping every app that matches and every app the
    /// flock does not have.
    ///
    /// `&self`: this reads the flock and changes nothing, which is the whole
    /// point of it being a separate command rather than something `do_start`
    /// reports on its way past. Applying an edit under a running sheep is
    /// the outcome being ruled out, not the one being built.
    ///
    /// Several instances of one app share one config, so the first slot
    /// found under a name answers for all of them. Instance count is not
    /// what makes them differ; the stored config is.
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
    /// Always inserts a [`SheepSlot`] before returning, so the entry
    /// persists regardless of the outcome: on success, `Starting` with a
    /// readiness task armed when the app configures `wait_ready` or
    /// `readiness_probe`, `Online` immediately otherwise; `Errored` with no
    /// task on failure.
    ///
    /// `dog` lands on the entry both arms register, not just the successful
    /// one: a dog whose binary cannot be spawned still has to show up in the
    /// dogs table as `Errored`, which is exactly what adopting a bad path
    /// produces.
    fn spawn_fresh(
        &mut self,
        app: &ResolvedApp,
        instance: u32,
        credentials: Option<Credentials>,
        dog: Option<DogSource>,
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
                    dog,
                    // A fresh spawn, `id` newly allocated: nothing has
                    // exited yet under it.
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
                    dog,
                    // Spawn itself failed: no process ever existed to exit.
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
                    },
                );
                self.emit(ProcessEventKind::Errored, info, true);
                // Names the file exec was pointed at. `error` on its own was
                // the whole message an operator got: "process spawn failed:
                // No such file or directory (os error 2)", which told
                // somebody starting an eleven-app Flockfile neither which
                // app nor which path. The app's NAME is added by the caller
                // rather than here, so `scale`'s own reply -- which already
                // opens with the sheep's name -- does not say it twice.
                //
                // `spec.program` and `spec.cwd` verbatim, not a resolution of
                // the two: they are what the Flockfile said, which is where
                // the operator has to make the edit.
                let attempted = match &spec.cwd {
                    Some(cwd) => format!("`{}` in {}", spec.program, cwd.display()),
                    None => format!("`{}`", spec.program),
                };
                Err(format!("{error}; tried {attempted}"))
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
                slot.ctl = Some(handles.ctl);
                slot.log_ctl = Some(log_ctl);
                slot.to_child = Some(to_child);
                slot.signals = Some(handles.signals);
                slot.to_stdin = Some(to_stdin);
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
                let info = to_info(&slot.entry, &self.smits);
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
                // Cleared, because nothing exited here: the spawn itself
                // failed, so there is no exit to report. Leaving the
                // previous process's code in place would show a sheep that
                // once crashed with 1 as still crashing with 1 while it is
                // in fact failing to start at all -- and telling those two
                // apart is the whole reason this field exists.
                slot.entry.last_exit = None;
                slot.ctl = None;
                // Already `None` on every route into a respawn: all three
                // callers reach it only for an id whose `ctl` is clear, and
                // the two are cleared in the same breath. Written anyway, so
                // that "these two go together" is something a reader can see
                // at each site rather than infer from the callers.
                slot.to_child = None;
                slot.signals = None;
                slot.to_stdin = None;
                slot.ready_tx = None;
                let info = to_info(&slot.entry, &self.smits);
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
                    .matches(&config.name, *id, config.fold.as_deref())
                    .then_some(*id)
            })
            .collect();
        ids.sort_unstable();
        ids
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
    /// app, not a fresh selector match, and re-deriving a selector that
    /// happens to match exactly those ids would be a second, fragile way to
    /// say what `handle_scale` already knows directly.
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
            // An automatic restart is held off BOTH halves of a swap that has
            // not committed yet. A reload's whole point is the overlap, and a
            // cron occurrence or a watched file landing inside it destroys
            // that from either side: killing the drainee abandons the reload,
            // and killing the replacement abandons it just as surely — the
            // deploy becomes the ordinary hard restart the feature exists to
            // avoid. For a `watch` app, the archetypal reload-often one, any
            // save inside the readiness window did it. Those two are the only
            // automatic triggers that reach here at all: a memory breach and a
            // liveness failure arrive through `handle_extra_restart`, whose
            // `Online` guard has already rejected a `Stopping` drainee, and
            // the replacement has no extras armed until it goes `Online`.
            //
            // Dropping the trigger costs nobody an answer, which is
            // `claim_manual`'s own carve-out argument applied one step
            // earlier: an operator's command is the only one with a party
            // waiting behind it. What it does cost is the trigger itself. This
            // DROPS the restart, it does not defer it, and for a watched tree
            // that loses a real change: a save inside the readiness window
            // happened after the replacement was spawned, so the replacement
            // cannot be carrying it, and nothing re-fires it — the watcher
            // reads the empty `Ok` as a restart that matched nothing and goes
            // back to waiting, leaving that one instance on the older code
            // until something else restarts it. The trade is one lost change
            // against the overlap, and it is taken because the other half of
            // it is losing the overlap on EVERY save inside the window, for
            // the app class most likely to be reloaded at all. A cron
            // occurrence loses nothing new: a missed occurrence is already not
            // replayed. Instances of the app the reload has not reached yet
            // are not half of any swap and restart as usual.
            //
            // It stops at the commit rather than running to the end of the
            // job, and that boundary is the point: past `AwaitReady` the
            // replacement IS the app's live instance, and a memory breach or
            // liveness failure against it deserves the same restart it would
            // get an hour later. The drainee needs nothing from here by then
            // — the drain holds its marker, and `claim_manual` drops an
            // automatic restart against a marker an operator's command
            // already owns.
            let held_off_by_a_swap =
                origin == CommandOrigin::Automatic && self.in_an_uncommitted_swap(id);
            if held_off_by_a_swap {
                // The trade above loses a real change for a watched app, and
                // an operator who sees a save go nowhere has nothing else to
                // read. Same level and shape as `handle_extra_restart`'s
                // drops.
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
    /// # Slot allocation, both ways
    ///
    /// Up: [`instance_slots`] hands out the lowest free slots, exactly as a
    /// `Start` does. Down: the HIGHEST-numbered slots are deregistered first,
    /// which is what makes the two symmetric — scale a two-instance app to
    /// four and back and it is running slots 0 and 1 again, with the same log
    /// paths and the same `SHEP_INSTANCE` values it started with. Taking the
    /// lowest first would leave slots 2 and 3: the same count, a different
    /// flock.
    ///
    /// # What a scale-down does to the instances it removes
    ///
    /// Deregisters them — the same thing `Delete` does, through the same
    /// machinery, because a `Stop` would leave them registered and still
    /// holding their slots, and the next `Start` of the app would then find
    /// four slots taken and allocate a fifth.
    ///
    /// # Why the reply does not wait for them
    ///
    /// Each removal runs a kill ladder capped by the app's own `kill_timeout`,
    /// and a caller's RPC budget is capped at 60s (`crate::rpc`'s
    /// `MAX_DEADLINE_MS`), so a large scale-down cannot be covered by any reply
    /// a caller is allowed to wait for. The answer is the survivors; the
    /// departures report themselves on the bus as `process.delete`. Same split
    /// [`Self::handle_reload`] already makes.
    ///
    /// # Why a scale is refused while departures are still in flight
    ///
    /// Because the reply does not wait for them, a departing instance stays
    /// REGISTERED — marked [`SheepSlot::pending_delete`], partway through a
    /// kill ladder — until its exit lands. It is still in the map this
    /// function counts `current` off. So `shep stock web 1 && shep stock web
    /// 4` against an app that does not die instantly on `SIGTERM` (which is
    /// every app that drains connections on shutdown) used to find four
    /// slots, three of them doomed, call the second scale a no-op, answer
    /// `Ok` with four instances and no shortfall, and let `rpc` record
    /// `instances = 4` into the muster roll. The three then finished their
    /// ladders and the flock settled to one. The roll said four. A later
    /// `shep save` froze the lie, and a reboot brought up a count that had
    /// never run — the same class the partial-scale write-back below exists
    /// to close, arriving through a door it does not cover.
    ///
    /// Refused rather than counted around, and this is the choice worth
    /// arguing. Counting only the slots not marked for deletion would make
    /// the second scale spawn three fresh instances — into slots the
    /// departures still hold, while their ladders are still running, so the
    /// app briefly runs seven processes and the new ones' `SHEP_INSTANCE`
    /// values and log paths depend on when the old ones happen to die. The
    /// flock's shape is the thing being changed; a second command issued
    /// against it mid-change is a question with no stable answer, and
    /// answering it with a guess is what produced the divergence in the
    /// first place. A refusal names the wait, and the wait is bounded by the
    /// app's own `kill_timeout`.
    ///
    /// Symmetric with the `reloads` guard directly above it, for the same
    /// reason: both refuse a command that would reshape an app another
    /// command is still reshaping. The refusal reaches the operator as
    /// [`SupervisorError::InvalidScale`] rather than a variant of its own —
    /// see that variant's doc for why a conflict is carried by the scale's
    /// own refusal type here.
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
        // Counted rather than merely detected: the number is the only thing
        // that tells the operator how much of the flock is still moving, and
        // `current - leaving` is the count the next scale will actually start
        // from.
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

        // Re-normalized rather than mutated in place: `ResolvedApp` keeps its
        // config private precisely so that holding one proves it passed
        // `normalize` (`normalize.rs`'s own note), and a scale that edited the
        // field behind that door would be the first thing in the tree to hold
        // one that had not.
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
        let credentials = self
            .sheep
            .get(&slots[0].1)
            .expect("handle_scale: id read off this map a moment ago")
            .entry
            .credentials;

        // The spawn/remove pass runs FIRST and the config write-back second,
        // which is the opposite of the obvious order and is the whole of this
        // function's care about a partial scale. Writing `rescaled` onto every
        // slot up front and then failing a spawn leaves every survivor
        // claiming `instances = 4` in a flock of three: `shep describe` and the
        // next `respawn` read the new number, `shep save` writes it, and
        // nothing in the tree notices until a reboot brings up a count that was
        // never running.
        let mut failure = None;
        // The one slot `spawn_fresh` registers on a failed attempt itself
        // (its own doc: it "always inserts a `SheepSlot`... `Errored` with no
        // task on failure"). Kept out of `survivors` — it is not a running
        // instance, and `Scaled::instances` must not claim it is — but the
        // config write-back below still has to reach it, or this one
        // registered slot is left holding `rescaled` (the COUNT ASKED FOR)
        // forever, which is exactly the lie this function exists to prevent
        // on every OTHER slot.
        let mut orphaned_by_failed_spawn = None;
        let survivors: Vec<u32> = match count.cmp(&current) {
            Ordering::Equal => slots.iter().map(|(_, id)| *id).collect(),
            Ordering::Greater => {
                let existing: Vec<u32> = slots.iter().map(|(instance, _)| *instance).collect();
                let mut ids: Vec<u32> = slots.iter().map(|(_, id)| *id).collect();
                for instance in instance_slots(&existing, count - current) {
                    let attempted_id = self.next_id;
                    match self.spawn_fresh(&rescaled, instance, credentials, None) {
                        Ok(info) => ids.push(info.id),
                        Err(message) => {
                            // Partial, and said so. The instances already
                            // spawned stay: they are real processes serving
                            // real traffic, and unwinding them would turn one
                            // failed spawn into an outage of everything this
                            // call had already brought up. What they do NOT
                            // keep is the requested count — see the write-back
                            // below.
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
                    // reply is the survivors, and the departures report
                    // themselves on the bus.
                    ReplyKind::Ids(oneshot::channel().0),
                );
                keep.iter().map(|(_, id)| *id).collect()
            }
        };

        // The count actually achieved — `count` on every path but a partial
        // scale-up. Re-normalized rather than assigned for the same reason
        // `rescaled` was: a `ResolvedApp` is a proof token, and the one place
        // in the tree holding one that had not passed `normalize` would be
        // here.
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

        // Every surviving slot, the ones this call just spawned included, plus
        // the one `Errored` slot a failed spawn attempt registered for
        // itself: `respawn` reassembles from a slot's own stored spec, and a
        // future `handle_scale` reads `current` off every registered slot of
        // this name regardless of status, so any slot left holding a stale
        // config keeps lying to everything that reads it, `shep describe`
        // included.
        for id in survivors.iter().chain(orphaned_by_failed_spawn.iter()) {
            if let Some(slot) = self.sheep.get_mut(id) {
                slot.entry.spec = stored.clone();
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
        instances.sort_unstable_by_key(|info| info.id);
        // `Ok` even when `failure` is set, and that is deliberate: see
        // `Scaled`'s own doc. The caller records `app` unconditionally and
        // turns `shortfall` into the operator's error; an `Err` here would
        // take the achieved config with it and leave the muster roll holding
        // the pre-scale count.
        let _ = reply.send(Ok(Scaled {
            instances,
            app: stored,
            requested: count,
            shortfall: failure,
        }));
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
        let matched = self.matching_ids(selector);

        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        // Refused whole, before anything is spawned: see
        // `SupervisorError::ReloadInFlight` for why a partly-accepted
        // selector is worse than a refused one.
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

        let accepted: Vec<ProcessInfo> = matched
            .iter()
            .map(|id| {
                let slot = self
                    .sheep
                    .get(id)
                    .expect("handle_reload: `matched` holds ids read off this map a moment ago");
                to_info(&slot.entry, &self.smits)
            })
            .collect();

        // Grouped by app because a reload runs one instance of an app at a
        // time, and ordered by instance slot because that is the order an
        // operator reads a clustered app in — an id order would be the same
        // thing until the first respawn and then quietly stop being.
        let mut queues: BTreeMap<String, Vec<(u32, u32)>> = BTreeMap::new();
        for id in matched {
            let entry = &self
                .sheep
                .get(&id)
                .expect("handle_reload: `matched` holds ids read off this map a moment ago")
                .entry;
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
    /// [`Self::arm_reload_deadline`] rides on that: arming from the one door
    /// is what makes "every swap is bounded" a property of this function too,
    /// rather than something each phase has to remember.
    ///
    /// # What counts as replaceable
    ///
    /// An instance that stopped being `Online` between acceptance and its turn
    /// is skipped and the reload carries on — an operator stopping one
    /// instance is not a reason to abandon the others.
    ///
    /// So is one whose next exit something already owns, which the status does
    /// not show: `claim_manual` sets the `manual` marker and sends the `Kill`
    /// without writing a status, so a sheep an operator's `restart`, a cron
    /// occurrence or a memory breach claimed a moment ago reads `Online` for
    /// its whole kill ladder — up to `kill_timeout`. A swap against one is
    /// doomed the instant it starts: the exit is already coming, it lands
    /// inside `AwaitReady` carrying that marker, and `handle_exited` abandons
    /// the reload and kills the replacement it had just spawned, after the
    /// caller was told `Ok`. Skipping costs nothing that was not already lost.
    /// The claimed exit is either terminal — a `stop` or a `delete`, leaving
    /// nothing to keep reachable — or a restart that brings the instance back
    /// on the same code a replacement would have carried, at the price of the
    /// downtime that restart was always going to cost. The rest of the app
    /// still gets its overlap.
    ///
    /// A replacement that cannot be SPAWNED is the opposite and ends the
    /// reload, per spec §4: failure of a new instance aborts the rest and
    /// leaves the old instances running.
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
            let slot = self.sheep.get(&old_id);
            let replaceable = slot.is_some_and(|slot| {
                slot.entry.status == ProcStatus::Online && slot.manual.is_none()
            });
            if !replaceable {
                // Logged for the reason `handle_extra_restart` logs its four
                // drops: a whole reload can end here having replaced nothing,
                // long after its caller was told `Ok`, and there is no
                // instance left for an abandonment to name — so this line is
                // the only account of it there is.
                tracing::debug!(
                    name,
                    old_id,
                    status = ?slot.map(|slot| slot.entry.status),
                    "reload skipped an instance: it is gone, no longer online, or its next \
                     exit is already claimed"
                );
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
                    // The other end a reload can come to, and the only one
                    // that reaches the bus without `abort_reload`: no
                    // replacement was ever registered, so nothing but this
                    // says the swap is off. `spawn_replacement` has already
                    // put the drainee back to `Online`, which is what the
                    // event carries.
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
    ///   function's own carve-out on the swap's phase instead.
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
        // Carried across the swap for the same reason `restarts` is: the
        // replacement is the same instance continuing, not a new one, and
        // `shep reload bark` names a dog exactly enough to reach it. Read off
        // the drainee rather than re-derived, because nothing here could
        // re-derive it.
        let dog = drainee.dog.clone();
        // Carried across the swap for the same reason `restarts`/`dog` are:
        // a reload is not an exit, so the replacement's honest answer to
        // "why did this instance last stop" is still whatever the drainee's
        // was, not `None` — `None` here would read as "this instance has
        // never exited", which is only true the first time an app is ever
        // reloaded.
        let last_exit = drainee.last_exit;

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
        drainee.entry.reload = ReloadState::Drainee { new_id };

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
                    reload: ReloadState::Replacement,
                    credentials,
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
                    },
                );
                // The instance being replaced announces itself BEFORE its
                // replacement's `Start`, and the order is the useful half. A
                // reload's reply is an acceptance, so a subscriber's whole
                // account of the swap is what arrives here; a second `Start`
                // in an instance slot that already had a live entry is not
                // self-explanatory, and `Reload` is what explains it. Emitted
                // from the drainee's own entry, which is `Stopping` by now,
                // so a reader following one id sees it change hands rather
                // than having to pair two events by slot.
                let drainee = to_info(&self.sheep[&old_id].entry, &self.smits);
                self.emit(ProcessEventKind::Reload, drainee, true);
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
            // Defensive: nothing leaves a `Replacement` marker behind without a
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
    ///
    /// The `Reloaded` this announces is the one event that says a swap
    /// SUCCEEDED, and it goes out before the next swap begins so a clustered
    /// app's reload reads in order. It is owed only to a replacement that is
    /// actually SERVING, which is strictly narrower than one that is still
    /// registered. A replacement that went down inside the drain window keeps
    /// its row — `Stopped` for an app that does not autorestart, `Errored`
    /// for one whose budget ran out, `WaitingRestart` for one still owed a
    /// respawn — and an event claiming an instance took over would name a
    /// process that is not there.
    ///
    /// Anything else is a failed new instance, whichever side of `Online` it
    /// died on, so the reload ends here rather than carrying on: spec §4,
    /// failure of a new instance aborts the rest. It is announced as an
    /// abandonment rather than passed over in silence, because the queue that
    /// goes with it is the rest of a clustered app left on the old code long
    /// after its caller was told `Ok`.
    ///
    /// The one shape that reaches the bus with nothing is a replacement that
    /// is not registered at all — `shep delete <replacement>` during the
    /// drain, which deregisters it while the instance it replaced is still
    /// draining. There is no entry left to name, and that delete's own
    /// `Delete` event has already said the process is gone.
    fn finish_swap(&mut self, name: &str) {
        let Some(job) = self.reloads.remove(name) else {
            return;
        };
        let new_id = job.swap.new_id;
        self.clear_reload(new_id);

        let serving = self
            .sheep
            .get(&new_id)
            .is_some_and(|slot| slot.entry.status == ProcStatus::Online);
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

    /// Abandons `name`'s reload: the instance it was replacing goes back to
    /// serving where that is still available to it, the instances it had not
    /// reached yet are left alone, and the replacement is killed and
    /// deregistered.
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
        // The doc above claims `AwaitReady`, and every caller does check —
        // but the weakest of the three checks it indirectly, by asking
        // whether the drainee is still registered rather than by reading the
        // phase. This turns that argument into something the suite has to
        // keep true. It guards the restore below and nothing else: a job
        // outliving both of its ids is a different failure, and lives in
        // `handle_exited`.
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
        // event has to carry the status the restore below decides, and that
        // block holds a mutable borrow while it decides it.
        let kept = self.sheep.get_mut(&job.swap.old_id).map(|drainee| {
            drainee.entry.reload = ReloadState::None;
            // `Online` only where going back to serving is actually true.
            // Two abandonments reach a drainee for which it is not. When the
            // drainee's OWN exit is what triggered this, its task is already
            // gone and the ordinary decision path is about to set the status
            // anyway. When an operator's `stop` matched both halves and the
            // replacement's exit landed first, the drainee is mid-kill-ladder
            // with that command holding its marker. `Stopping` is the honest
            // status for both, and writing `Online` over it hands an operator
            // a live pid for a process on its way out — and starts
            // `handle_extra_restart`'s `Online` guard passing for it again.
            if drainee.ctl.is_some() && drainee.manual.is_none() {
                drainee.entry.status = ProcStatus::Online;
            }
            to_info(&drainee.entry, &self.smits)
        });
        if let Some(info) = kept {
            self.emit(ProcessEventKind::ReloadAbandoned, info, true);
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

    /// Bounds the swap that has just started: after
    /// `listen_timeout + graceful_timeout + `[`RELOAD_DEADLINE_SLACK`], a
    /// `Msg::ReloadDeadline` comes back to end it if nothing else has.
    ///
    /// # Why a reload needs a watchdog when nothing else here does
    ///
    /// Every other transition out of a [`ReloadJob`] is driven by a message
    /// from a task the actor cannot make report: `Msg::ReadyResult` from a
    /// readiness task, `Msg::Exited` from a sheep task. The second one is not
    /// merely theoretical — [`kill_process`]'s wait after `SIGKILL` has no
    /// timeout, so a single instance wedged in uninterruptible sleep produces
    /// no exit at all. What that costs is out of proportion to how rare it is:
    /// `handle_reload` refuses on the presence of the map key, so one such app
    /// answers `<name> is already being reloaded` until the daemon is
    /// restarted, and takes `shep reload all` down with it because the refusal
    /// is whole-selector.
    ///
    /// # Why this cannot be starved by what it guards
    ///
    /// The timer is a task of its own with nothing in it but a sleep and a
    /// mailbox send, so nothing a wedged sheep, a lost readiness task or a
    /// stalled swap can do reaches it. It is armed from
    /// [`Self::advance_reload`], the one door every swap goes through, so no
    /// swap can exist without one; it is never extended, re-armed or
    /// cancelled by the swap it is watching, so the thing it is waiting for
    /// cannot postpone it; and it covers both phases at once rather than
    /// being re-armed at the commit, so there is no handover for a stall to
    /// land in. The only thing it shares with the machinery is the actor's
    /// mailbox, which drains unconditionally (the command path never awaits —
    /// CRITICAL-2).
    fn arm_reload_deadline(&self, name: &str, new_id: u32) {
        // Loud rather than silent: a swap that quietly failed to arm one is
        // the exact state this exists to make impossible, and the
        // registration is a line old — `spawn_replacement` returned `Ok`.
        let app = self
            .sheep
            .get(&new_id)
            .expect("arm_reload_deadline: the replacement was registered a moment ago")
            .entry
            .spec
            .config();
        let deadline = app.listen_timeout.as_duration()
            + app.graceful_timeout.as_duration()
            + RELOAD_DEADLINE_SLACK;

        let tx = self.tx.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            let _ = tx.send(Msg::ReloadDeadline { name, new_id }).await;
        });
    }

    /// A swap ran out of time: end the reload rather than leave a job nothing
    /// can remove.
    ///
    /// Stale deadlines are dropped on the same rule `handle_restart_due`
    /// applies to a stale `RestartDue`, reading the swap's `new_id` where that
    /// one reads an epoch — a swap that finished, an app whose whole reload is
    /// over, and a later reload of the same app are all covered by it, because
    /// ids are never reused.
    ///
    /// What the ending is depends on the phase, and it is the same split
    /// [`Self::abort_reload`] documents. Before the commit there is still an
    /// instance to go back to, so this is an ordinary abandonment: the
    /// instance being replaced goes back to serving and the replacement that
    /// never proved itself is killed. After it there is not, so the job is
    /// dropped where it stands and the replacement — the app's live instance
    /// by then — is left exactly as it is.
    ///
    /// The instance being replaced keeps [`ReloadState::Drainee`]
    /// through that second ending, deliberately. It is what routes a late exit
    /// to [`Self::reap_drainee`], and a cleared marker would send that exit to
    /// `decide_on_exit` instead — which either leaves a dead row in an
    /// instance slot the replacement owns, or, for an `autorestart` app,
    /// respawns a second live process into it.
    fn handle_reload_deadline(&mut self, name: &str, new_id: u32) {
        let Some(job) = self.reloads.get(name) else {
            return;
        };
        if job.swap.new_id != new_id {
            return;
        }
        match job.swap.phase {
            ReloadPhase::AwaitReady => {
                self.abort_reload(
                    name,
                    "the swap passed its deadline with no readiness result",
                );
            }
            ReloadPhase::DrainOld => {
                let old_id = job.swap.old_id;
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
        let info = to_info(&removed.entry, &self.smits);
        self.emit(ProcessEventKind::Delete, info.clone(), true);
        self.disarm_extras(id, &info.name);
        self.resolve_pending(id, info)
    }

    /// The app whose swap `id` is half of, while that swap has not committed
    /// yet — the window in which ending either half loses the overlap the
    /// reload exists for.
    ///
    /// The one spelling of that rule. Both of [`Self::handle_exited`]'s reload
    /// arms ask it and want the app's name as well as the answer, and
    /// [`Self::begin_manual`] asks it and wants only the answer — three sites
    /// in which two readings of "has this swap committed" disagreeing is at
    /// its most expensive, since one of them decides whether an instance that
    /// is still serving gets killed.
    fn uncommitted_swap_of(&self, id: u32) -> Option<String> {
        self.reloads
            .iter()
            .find(|(_, job)| {
                job.swap.phase == ReloadPhase::AwaitReady
                    && (job.swap.old_id == id || job.swap.new_id == id)
            })
            .map(|(name, _)| name.clone())
    }

    /// Whether `id` is half of a swap that has not committed yet; see
    /// [`Self::uncommitted_swap_of`], which this is the name-less reading of.
    fn in_an_uncommitted_swap(&self, id: u32) -> bool {
        self.uncommitted_swap_of(id).is_some()
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

    /// Resolves `selector` and hands every pump writing to a matched sheep's
    /// log paths to a task that reopens their files and then answers the
    /// caller.
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
    /// the flock is stopped. It shows up in one of two shapes, and they are
    /// the same answer: the slot's `log_ctl` is `None` because no spawn ever
    /// succeeded for it, so it contributes no pump below and is a row in the
    /// reply and nothing more; or the send (or the acknowledgement) fails
    /// because the pump has ended, which is how a stopped sheep normally
    /// presents, and [`reopen_logs`] reads that as the no-op it is.
    ///
    /// # Why more pumps are reopened than the reply names
    ///
    /// A log path can have writers the selector never named. Two apps can be
    /// given one explicit `out_file`; `merge_logs` points every instance of an
    /// app at one path; and a reload makes every app one of these for the
    /// length of a swap, because both entries sharing an instance slot derive
    /// byte-identical paths from it. So the reach is every slot writing to a
    /// path a matched sheep writes to, matched or not.
    ///
    /// The rule has to be "every writer to this path" rather than "every sheep
    /// the selector matched" because what an external rotator renamed is a
    /// FILE, and a pump left unasked goes on appending to the renamed inode:
    /// the archive keeps growing, the recreated path stays empty, and the
    /// `postrotate` stanza that waited for a zero exit was told the opposite.
    /// Selector-keying cannot express that, since the operator naming one
    /// writer of a file is not a statement about the others.
    ///
    /// [`Actor::handle_flush`] draws its barrier around the same set for a
    /// harsher reason — an unflushed sibling's write lands in a file the
    /// operator was just told is empty — and the two verbs agreeing on the
    /// reach is worth more than either argument alone: an operator rotating
    /// logs should not have to hold two different mental models of which
    /// sheep a log-plane verb touches.
    ///
    /// The reply stays keyed by the selector, exactly as `flush`'s does: a row
    /// means "a sheep you named", and adding the unnamed writers would make
    /// the one table an operator reads unable to say which was which.
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

        // Sorted here, where the whole set is in hand, rather than after the
        // reopens: `HashMap` iteration order is arbitrary, and pump failures
        // are reported in the order they are collected, so an unsorted pump
        // set would make a multi-pump failure message read differently run to
        // run. `matched` needs no such step — it is built in the id order
        // `matching_ids` answers in, and a caller reading the reply as a
        // table wants a stable order over `list`'s own (name-grouped) one.
        pumps.sort_unstable_by_key(|(info, _)| info.id);
        spawn_reopen_task(matched, pumps, reply);
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
        // order is arbitrary, and pump failures are reported in the order
        // they are collected, so an unsorted flush set would make a
        // multi-pump failure message read differently run to run. Neither
        // `matched` nor `paths` needs the step — the first is built in the id
        // order `matching_ids` answers in, which is `list`'s and so the one a
        // caller rendering the reply as a table wants, and the second is a
        // `BTreeSet` already.
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
        // from this point on. Neither entry of a swap needs fixing up — both
        // are `ctl.is_some()`, so both are in the `online` set killed below,
        // and `handle_exited` deals with each once no job names it.
        //
        // It does not give them the same ending, and the asymmetry is the
        // point rather than an oversight. The replacement takes the ordinary
        // clean-stop path and stays registered as `Stopped`, like every other
        // sheep a shutdown kills. The drainee still carries
        // `ReloadState::Drainee`, so it takes `reap_drainee` and
        // is DEREGISTERED — which is right for the same reason it is right
        // mid-reload: the instance slot belongs to the replacement now, and a
        // second permanent row in it would double every name-keyed verb for
        // as long as the flock lives.
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
        // Goes with `ctl`, and the slot stays registered on most of the paths
        // below, so this is the clearing that matters: the writer task on the
        // far end of that sender has no way to notice a child that exited
        // quietly, and would sit on `recv()` holding the daemon's half of the
        // socketpair for as long as the entry lived. See `SheepSlot::to_child`.
        slot.to_child = None;
        // Cleared alongside `to_child` for the same reason: a sheep task
        // parked on `signal_rx.recv()` would otherwise hold this sender's
        // receiver open for as long as the entry lived, past the process it
        // was meant to reach. See `SheepSlot::signals`.
        slot.signals = None;
        // Cleared alongside `to_child` for the same reason again: the
        // writer task on the far end parks on `recv()`. See
        // `SheepSlot::to_stdin`.
        slot.to_stdin = None;
        // The one site that clears these, because it is the one place a
        // process under a registered id stops existing: every respawn and
        // every deregistration is downstream of an exit handled here, so a
        // wait cannot survive into a second process's life by any route.
        slot.actions.abandon_all();
        slot.entry.pid = None;
        // Set here, before any branch below decides what this exit BECOMES
        // (a respawn, an error, a clean stop, a deregistration): this is the
        // one place a process under a registered id stops existing, so it is
        // the one place that can record what it stopped WITH. Unconditional
        // — an operator's own `stop`/`delete` reaches this line exactly like
        // a crash does, and the process genuinely still exited by whatever
        // signal ended it, which stays true and stays worth showing
        // regardless of who asked for it. A branch that goes on to remove
        // this entry entirely (`Delete`, or a committed reload's drainee)
        // takes this value with it into the `ProcessInfo` its own removal
        // emits, rather than losing it a line before that snapshot is taken.
        slot.entry.last_exit = Some(outcome.into());
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
            ReloadState::Drainee { .. } => {
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
                // An operator's is the only command that can be here, and it
                // takes all three of these to be true. `advance_reload` starts
                // a swap only against an instance whose `manual` marker is
                // clear, so nothing is carried in from before it; once it has
                // started, `begin_manual` holds every automatic restart off
                // both halves of a swap that has not committed — which is the
                // only phase the branch below runs in — and
                // `handle_extra_restart`'s `Online` guard rejects the two
                // triggers that reach it a second time. So the warning can
                // name the operator without hedging.
                match self.uncommitted_swap_of(id) {
                    Some(name) if kind.is_some() => {
                        self.abort_reload(&name, "an operator's command reached the drainee first");
                        // Falls through as an ordinary entry: `abort_reload`
                        // has already cleared this one's marker and put its
                        // status back.
                    }
                    _ => return self.reap_drainee(id),
                }
            }
            ReloadState::Replacement => {
                // Whether this is a failure depends on how far the swap got.
                // Still `AwaitReady` and the replacement never proved it could
                // take over, so the reload is abandoned and the drainee kept.
                // Past that and the swap is committed: this is an ordinary
                // instance now, and its exit is its own restart policy's
                // business.
                self.clear_reload(id);
                if let Some(name) = self.uncommitted_swap_of(id) {
                    self.abort_reload(&name, "the replacement exited before it was ready");
                    return self.deregister_on_exit(id);
                }
                // A committed swap normally ends on the drainee's exit, which
                // `reap_drainee` turns into `finish_swap` — but only while
                // there is still a drainee to produce one. A swap `reap_drainee`
                // itself committed has none: it was the drainee's death that
                // committed it, and the deregistration went with it. That left
                // this replacement's readiness result as the last event able to
                // end the job, and clearing its `Replacement` marker a line above
                // cancels that too, because `handle_ready_result` routes on the
                // marker. So the job ends here or never, and a job nothing can
                // end refuses every later reload of the app for as long as the
                // daemon runs.
                //
                // The queue goes with it rather than carrying on, per spec §4:
                // this replacement exited before it was ever `Online`, which is
                // a failure of the new instance, and that aborts the rest.
                //
                // That last claim is an inference, not something the condition
                // reads, so the assert below turns it into something the suite
                // has to keep true. `DrainOld` with the drainee gone has one
                // cause — `reap_drainee` committing the swap on the drainee's
                // own death — because the other route into `DrainOld`,
                // `begin_drain`, leaves the drainee registered until
                // `reap_drainee` removes it and ends the job in the same call.
                // A swap committed that way never had a replacement go
                // `Online`, and a kill ladder writes no status, so an `Online`
                // here would mean the inference had stopped holding.
                //
                // It is the third way a reload can end, and it says so on the
                // bus like the other two: a subscriber told the reload was
                // accepted and never told otherwise cannot tell one still
                // running from one that gave up, and this is the ending where
                // knowing matters most, since the queue went with it. The
                // event carries this entry as it stands, which is what every
                // event in this file carries — the exit's own transition has
                // not run yet, so the `Stop` or `Exit` that reports where the
                // replacement actually landed follows a moment later.
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
    ///    and report success. The same check also rejects a report against a
    ///    reload's drainee, the one entry [`ProcStatus::Stopping`] otherwise
    ///    reaches: a liveness failure or memory breach raised against it must
    ///    ride out to the drainee's own exit, never claim its manual marker
    ///    and kill it a second time. For that one case this is the first of
    ///    two rejections and the only one that runs: `begin_manual`, which
    ///    every automatic restart goes through including this one, would hold
    ///    it off both halves of an uncommitted swap on its own, but the
    ///    `return` here means it never gets the chance. The stopped-sheep case
    ///    above is this guard's alone, so it is not redundant; a drainee is
    ///    covered twice over, and that is the right amount for the one case
    ///    where getting it wrong ends the instance being replaced.
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
        if matches!(slot.entry.reload, ReloadState::Replacement) {
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

    /// Resolves `selector`, puts one action on the shepherd channel of every
    /// matched sheep that can take one, and answers with a row per match once
    /// the last of those waits has ended.
    ///
    /// Shaped like [`Self::begin_manual`]: one request, N matched sheep, an
    /// answer that carries every one of them and fires only when the last is
    /// settled. What differs is where the aggregation lives. A manual
    /// command's per-sheep outcome is an exit the actor itself handles, so it
    /// is collected in `pending` and folded in from `handle_exited`; an
    /// action's is a message from a task the actor spawned, already addressed
    /// to a channel of its own. So the rows are joined up in a task rather
    /// than in the actor's own state — see [`spawn_trigger_task`], which also
    /// says why the waits do not run one after another despite being awaited
    /// in a loop.
    ///
    /// # Why nothing here is awaited
    ///
    /// Both halves of an action are awaits, and neither may happen in the
    /// actor loop. Awaiting one closes the permanent cycle
    /// [`Self::handle_reopen`] sets out in full: the actor stops draining its
    /// mailbox, so a sheep task blocks in `actor_tx.send`, so nothing drains
    /// that sheep — and here that sheep is precisely the party being waited
    /// on, since its reply reaches the actor through `run_sheep` and that
    /// same mailbox. A `try_send` would keep the loop moving but would answer
    /// a busy child by dropping its action, which is the one thing an
    /// operator cannot be told from a child that ignored it. So the send goes
    /// to the task along with the wait, and this handler does nothing but
    /// record what it armed.
    ///
    /// # Why a refusal is a row and not the answer
    ///
    /// Spec §9's selector grammar (`all`, `/regex/`, `fold:`) makes a mixed
    /// flock the normal case, so a sheep that cannot take the action refuses
    /// in its own row and the rest are still asked. `Reopen` and `Flush` set
    /// the precedent: a per-item failure inside a success, rather than a
    /// refusal that takes every other match down with it and leaves the
    /// operator unable to tell which half was taken.
    ///
    /// Both refusals are decided HERE, ahead of the wait, and that ordering
    /// is load-bearing rather than tidy: a wait is resolved by a message from
    /// the task that armed it, so a sheep refused after one was armed would
    /// leave a wait nothing drives home.
    ///
    /// - **No channel to deliver over** — [`ActionOutcome::NoChannel`], read
    ///   off [`SheepSlot::open_channel`] rather than off `AppConfig::channel`,
    ///   because the channel is the fact that actually decides delivery and a
    ///   second copy of it could disagree. Answered here rather than waited
    ///   out: a wait would say the same thing one whole timeout later.
    /// - **A reload drainee** — [`ActionOutcome::Skipped`]. Both halves of a
    ///   swap match a name selector, so an operator asking for `web` reaches
    ///   the process being replaced as well as the one replacing it, and an
    ///   answer from a process on its way out is worse than no answer. It is
    ///   also the half that would pay most for a wait: a sheep mid-kill-ladder
    ///   has stopped reading its channel, so its row would cost a full
    ///   timeout to say nothing. The replacement is not skipped — it is the
    ///   instance that will still be there afterwards.
    ///
    /// A live sender is not proof of the opposite. `to_child.send` returning
    /// `Ok` says a message was queued for the writer task, not that a child
    /// read it: the first send after a child has exited is accepted and
    /// vanishes, and only the second one fails. The app's reply is the only
    /// proof an action landed, which is why every other outcome here comes
    /// from the wait rather than from the send.
    fn begin_action(
        &mut self,
        selector: &ProcessSelector,
        action: String,
        params: Option<String>,
        reply: oneshot::Sender<Result<Vec<ActionReply>, SupervisorError>>,
    ) {
        // `matching_ids` answers in id order, so the actions go out in that
        // order rather than in whatever order the map happened to yield them.
        // The final sort in `spawn_trigger_task` is what the ANSWER's order
        // rests on; this one only keeps delivery from being arbitrary as well.
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
            // Each sheep's own `action_timeout` bounds its own wait — a
            // slow-flushing cache and an instant `gc` on the same flock no
            // longer share one number picked for neither of them.
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
            // Not a silent success. The rows say what happened to each sheep,
            // which is the half a caller reads; this is the half a daemon's
            // own log reads, and it is here because a request that delivered
            // NOTHING is worth one line even though no single row is
            // surprising on its own. A flock where nothing is reachable is
            // usually one misconfiguration — `channel` left unset — repeated
            // across every app, and the operator sees a table of refusals
            // rather than one cause.
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
    /// Off the actor loop, like [`Self::begin_action`] and for the same
    /// reason: each delivery is a round trip through a sheep task, and the
    /// actor must not park on one. Unlike an action there is nothing to wait
    /// out — a `kill(2)` either returns or does not — so the fan-out here is
    /// bounded by the syscall, not by a configured timeout.
    ///
    /// A sheep with no live task answers [`SignalOutcome::NotRunning`] without
    /// a round trip at all: `slot.signals` is `None` for exactly the states
    /// that have no process (`Stopped`, `Errored`, `WaitingRestart`).
    ///
    /// A reload drainee is signalled like any other live sheep — unlike
    /// [`Self::begin_action`], which skips one because an action expects a
    /// reply from a process on its way out. A signal expects nothing back, and
    /// the drainee is a live process the operator's selector matched; holding
    /// it back would be a silent refusal with no channel to explain itself in.
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
                // signals yet, which for a syscall-fast handler means it is
                // busy dying; a closed one means it already has. Both are
                // "there is no process here to signal", reported as the
                // refusal it is rather than as a delivery.
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

    /// Writes one line to every matched sheep's stdin.
    ///
    /// Off the actor loop, like [`Self::begin_signal`] and [`Self::begin_action`]
    /// and for the same reason: each write is a round trip through a sheep
    /// task, and the actor must not park on one.
    ///
    /// A sheep with no live task, or one running without `stdin = true`,
    /// answers [`LineOutcome::NoStdin`] without a round trip — read off
    /// [`SheepSlot::open_stdin`], which is the one fact that decides whether a
    /// pipe exists.
    ///
    /// **The enqueue is `try_send`, never an `.await`ed send.** A full queue
    /// means this sheep's writer task is blocked on a pipe the app is not
    /// draining — the exact condition [`LineOutcome::NotWritten`] names, and
    /// reporting it here costs no round trip at all. Awaiting into a full
    /// queue would park the actor loop itself on one wedged sheep's pipe,
    /// stopping every other sheep in the flock from being supervised — see
    /// [`SheepSlot::to_stdin`]'s own doc.
    ///
    /// A reload drainee gets the line, exactly as [`Self::begin_signal`]
    /// delivers a signal to one: the reply is not a conversation nobody can
    /// finish, and the drainee is a live process the selector matched.
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
                // The queue is full: this sheep's writer is blocked on a pipe
                // the app is not draining. That is precisely the condition
                // `NotWritten` names, and reporting it here costs no round
                // trip at all.
                //
                // The reason names no duration, and used to name
                // `STDIN_WRITE_TIMEOUT`. `try_send` measured nothing: it
                // looked at the queue once, on arrival, and found it full. An
                // elapsed time in this message is fiction — and the operator
                // reading it would take it for the bound the timeout path
                // reports, which is a different fact about a different line.
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
    /// reply, handing back the receiver that wait's outcome will arrive on.
    ///
    /// Infallible, and only reachable from [`Self::begin_action`]'s selector
    /// pass: every question an action can be refused over — is there a sheep,
    /// can it be reached, is it on its way out — is answered there, before
    /// this is called.
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
        // After the task is spawned, and safely: the task's own first act is
        // a send that has to reach a child, be read, and come back through
        // `run_sheep`, none of which can be observed before this handler
        // returns the actor to its loop.
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
    /// if it belongs to one. A reply with nowhere to go is dropped silently,
    /// exactly as an unwanted `Msg::Ready` is — see [`ActionWaits::answer`]
    /// for the shapes that takes, none of which is an error.
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
    /// on four things. The difference is what the two results decide. A
    /// readiness result drives a status transition, so it has to be refused
    /// by a slot that has moved on since; an action's result is a message to
    /// one caller who is still holding the phone, and answering it changes no
    /// flock state at all. So the only question worth asking is whether this
    /// wait is still one of the sheep's own, which the stamp answers and no
    /// respawn can make ambiguous.
    fn handle_action_result(&mut self, id: u32, stamp: u64, outcome: ActionOutcome) {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return;
        };
        if let Some(reply) = slot.actions.resolve(stamp) {
            let _ = reply.send(outcome);
        }
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
        to_info(&slot.entry, &self.smits)
    }

    /// Paints or clears one sheep's smit and answers with that sheep's
    /// instances as they now stand.
    ///
    /// Every instance of the name, not one row: the map is keyed by name, so
    /// every instance carries the mark and the reply says so rather than
    /// leaving the caller to guess how far it reached.
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
            // Refused rather than stored against a name nothing holds: a dog
            // painting a mark on a sheep somebody deleted gets a clean answer
            // instead of an orphan entry no listing would ever show it.
            return Err(SupervisorError::NotFound);
        }
        match smit {
            Some(smit) => {
                self.smits
                    .insert(sheep.to_string(), (conn, smit.to_string()));
            }
            // Only this connection's own — see `Command::SetSmit`'s doc.
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
    /// Sorted on `(name, instance, id)`, not id: sorting by id scatters a
    /// clustered app's instances across the table, and grouping by name is
    /// what makes a four-instance app read as one thing at a glance.
    /// `instance` keeps a clustered app's slots in their own order once
    /// grouped, and `id` breaks the tie a reload creates, where a
    /// replacement takes the drainee's slot number with a fresh id.
    ///
    /// Applied here once rather than once per verb: this is the single
    /// function every listing reply is built from — `ListFlock`, `Describe`,
    /// `Mustered`, and the muster roll's own `list_checked` — so sorting
    /// anywhere else would leave the metrics dog and bark reading a
    /// different order from the operator, or duplicate the rule per verb.
    fn snapshot_all(&self) -> Vec<ProcessInfo> {
        let mut entries: Vec<&ProcessEntry> = self.sheep.values().map(|slot| &slot.entry).collect();
        entries.sort_unstable_by(|a, b| {
            (a.spec.config().name.as_str(), a.instance, a.id).cmp(&(
                b.spec.config().name.as_str(),
                b.instance,
                b.id,
            ))
        });
        entries
            .into_iter()
            .map(|entry| to_info(entry, &self.smits))
            .collect()
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
///
/// Takes the smit map rather than hanging off `&self`, and that is not a
/// stylistic preference: several call sites hold a `&mut` borrow of
/// `self.sheep` when they reach this, so a method on `Actor` would borrow the
/// whole actor and refuse to compile. A free function taking one field lets
/// the borrow checker see the two fields as disjoint, which they are.
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
        // Lossy on purpose: `ProcessInfo` carries paths as strings, and a
        // non-UTF-8 log path must not be allowed to fail serialization of
        // the whole reply and blank the listing for every other sheep.
        .out_file(Some(entry.out_file.to_string_lossy().into_owned()))
        .err_file(Some(entry.err_file.to_string_lossy().into_owned()))
        // Left empty here, and filled in by the RPC layer for the two
        // verbs an operator reads resource usage from. Reading them is a
        // syscall walk over the host's whole process table, and the
        // actor must never block; every other caller of this function
        // answers a lifecycle verb, where the numbers would be paid for
        // and never read.
        .cpu_percent(None)
        .memory_bytes(None)
        .dog(entry.dog.clone())
        .last_exit(entry.last_exit)
        // By NAME: every instance of a sheep shows the same mark, including
        // one spawned after it was painted.
        .smit(
            smits
                .get(&entry.spec.config().name)
                .map(|(_, smit)| smit.clone()),
        )
        .build()
}

/// Converts the spawn-runner's own exit observation into the wire-facing
/// shape this crate's `ProcessEntry::last_exit` stores and its own `to_info`
/// reads back (both private to this crate, so named in code font rather
/// than linked).
///
/// A separate `From` rather than reusing [`ExitOutcome`] on the wire
/// directly: that type lives behind the [`ProcessRunner`] seam and is free
/// to grow with whatever the real runner needs to observe next (its own
/// module doc says so) without dragging a breaking wire change behind it.
/// The two share a shape today because the runner's own observation IS the
/// honest exit outcome — this is that fact stated once, at the one point it
/// crosses from the runner's vocabulary into the wire's.
impl From<ExitOutcome> for ExitInfo {
    fn from(outcome: ExitOutcome) -> Self {
        Self {
            code: outcome.code,
            signal: outcome.signal,
        }
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

/// Spawns the task that delivers one action to `id`'s child and waits for
/// the reply, returning the oneshot sender the actor stores
/// (`PendingAction::waiter`) so a later [`Msg::ActionReply`] can wake it. The
/// task reports its result back through `actor_tx` as a
/// [`Msg::ActionResult`], which `Actor::handle_action_result` drops if no
/// live wait carries `stamp` any more.
///
/// The send is in here rather than at the call site because both it and the
/// wait are awaits, and the actor loop may do neither — `Actor::begin_action`
/// gives the argument in full. `deadline` covers the two together, since a
/// child that has stopped reading its channel can stall either one.
///
/// The ways this can end map onto outcomes as follows:
///
/// - the reply arrives — [`ActionOutcome::Replied`], the only outcome that
///   proves the action landed;
/// - `deadline` elapses first — [`ActionOutcome::TimedOut`], which says the
///   app did not answer in time and nothing about whether it ever will;
/// - the send fails, or the waiter's other half is dropped —
///   [`ActionOutcome::NoChannel`]. The first means the writer task on the far
///   end is gone, so the message reached no child; the second means the
///   sheep's slot let go of this wait, which is what a process ending under
///   it does. Both leave no reply to come, which is what a sheep with no live
///   channel is told in the first place.
///
/// A send that SUCCEEDS proves nothing — the first one after a child exits is
/// accepted and discarded — so it is not reported at all.
///
/// Must be called from within a Tokio runtime context: it spawns the waiting
/// task immediately, the same way `spawn_readiness_task` already documents
/// for itself.
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
            // The send is inside the deadline rather than ahead of it. A
            // child that has stopped reading fd 3 backs its socket up, the
            // writer task stops draining this sender, and a send with no
            // bound on it would then park this task — and the caller waiting
            // on it — for as long as that child stays wedged.
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
/// folding the sheep already refused in [`Actor::begin_action`] in with the
/// ones that have a wait to report.
///
/// # Why awaiting them in a loop is not serial
///
/// Every wait in `waits` is already running its own task, its own deadline
/// started before this one existed — [`spawn_action_task`] spawns on the spot.
/// So the loop is a join and not a queue: it settles when the LAST wait does,
/// not when their timeouts add up, and a flock of ten unresponsive apps costs
/// whichever one of their `action_timeout`s is longest, not the sum of all
/// ten. Written as a plain loop for the reason
/// [`spawn_reopen_task`] gives for its own: one task with a `for` is a great
/// deal easier to follow than a join over the flock, and here it buys nothing
/// to give that up.
///
/// A wait whose sender is dropped rather than answered reports
/// [`ActionOutcome::NoChannel`] — the same thing [`ActionWaits::abandon_all`]
/// says, for the same event. The sender lives on the sheep's slot, so losing
/// it means that slot let go of the wait, which is what a process ending
/// under it does. The row is the honest one either way: no reply is coming.
///
/// Must be called from within a Tokio runtime context: it spawns immediately,
/// like `spawn_readiness_task` and `spawn_reopen_task`.
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
        // The refusals were collected in id order and the waits were armed in
        // it, but a wait's row is appended when it settles, so this is what
        // the answer's order actually rests on.
        rows.sort_unstable_by_key(|row| row.id);
        let _ = reply.send(Ok(rows));
    });
}

/// One matched sheep's pending signal delivery: its id, name, and the
/// receiver its outcome will arrive on. A type alias rather than a bare tuple
/// in [`spawn_signal_task`]'s signature — `clippy::type_complexity`'s
/// threshold, matching [`spawn_trigger_task`]'s own simpler tuple only by
/// element count and not by the `Result<(), RunnerError>` nested inside this
/// one's receiver.
type SignalWait = (u32, String, oneshot::Receiver<Result<(), RunnerError>>);

/// Spawns the task that collects one signal's rows and answers its caller,
/// folding the sheep already settled in [`Actor::begin_signal`] (no live
/// task, or a full/closed mailbox) in with the ones that have a delivery to
/// wait on. Mirrors [`spawn_trigger_task`], with one difference: a dropped
/// `done` sender — the sheep task ended between the send and the delivery —
/// reports [`SignalOutcome::NotRunning`] rather than the trigger tier's
/// `NoChannel`, since a process that stopped existing mid-delivery is exactly
/// what that outcome means here.
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
        rows.sort_unstable_by_key(|row| row.id);
        let _ = reply.send(Ok(rows));
    });
}

/// One matched sheep's arming: its id and name, and the receiver its write's
/// acknowledgement will arrive on.
type LineWait = (u32, String, oneshot::Receiver<Result<(), RunnerError>>);

/// Awaits every write's acknowledgement, each under its own
/// [`STDIN_WRITE_TIMEOUT`], and answers `reply` with `settled` and the results
/// in id order.
///
/// The waits run CONCURRENTLY — `join_all`, not a `for` loop. Unlike
/// `spawn_trigger_task`, whose per-row waits carry no shared bound, every wait
/// here is bounded by the same constant, so awaiting them one after another
/// would make the total `STDIN_WRITE_TIMEOUT * matched` and put a flock-wide
/// `sendline` over an RPC caller's default budget.
/// `a_flock_of_wedged_sheep_is_bounded_once_and_not_once_each` is the test
/// that says so.
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
                        // The shepherd stopped WAITING; it did not stop the
                        // write. The bytes may be part-written into a pipe
                        // the app is not draining and land in full when it
                        // drains, so the reason says so rather than letting
                        // an operator read `not_written` as "never sent" and
                        // retry into a double delivery. See `LineOutcome`'s
                        // own doc.
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
        rows.sort_unstable_by_key(|row| row.id);
        let _ = reply.send(Ok(rows));
    });
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
/// `pumps` is every writer to a path a sheep in `matched` writes to, which is
/// a wider set than `matched` whenever a selector names some but not all of
/// the sheep sharing a file — see [`Actor::handle_reopen`] for why the reach
/// is drawn around the file rather than around the selection, and why the
/// reply is not.
///
/// Must be called from within a Tokio runtime context: it spawns
/// immediately, like `spawn_readiness_task` and `schedule_restart`.
fn spawn_reopen_task(
    matched: Vec<ProcessInfo>,
    pumps: Vec<(ProcessInfo, mpsc::Sender<LogCtl>)>,
    reply: oneshot::Sender<Result<Vec<ProcessInfo>, SupervisorError>>,
) {
    tokio::spawn(async move {
        let mut failures = Vec::new();
        for (info, log_ctl) in &pumps {
            if let Err(error) = reopen_logs(log_ctl).await {
                // Named and id'd, because the reply that would have said
                // which sheep these are is the one being replaced — and
                // because a widened set can fail on a sheep the operator never
                // named, which is worth being told in those words rather than
                // as an unattributed path.
                failures.push(format!("{} (id {}): {error}", info.name, info.id));
            }
        }
        // Every pump is visited before anything is reported: one sheep
        // whose log directory is gone must not stop the rest of the flock
        // being reopened, and an operator wants every failing path in one
        // answer rather than one per rotation.
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

/// One signal delivery a sheep task is asked to perform, plus where the answer
/// goes.
///
/// A mailbox of its own rather than a [`SheepCtl`] variant — see
/// [`SheepSlot::signals`]'s own doc, which spells out why sharing
/// `SheepCtl`'s queue would let a burst of signals make [`Actor::claim_manual`]
/// drop a stop and report success for it.
#[derive(Debug)]
struct SignalRequest {
    /// What to deliver, to this sheep's own pid.
    sig: OperatorSignal,
    /// Fires with what the delivery came to. A dropped sender means the sheep
    /// task ended between the send and the delivery, which the caller reads as
    /// the sheep no longer running.
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
    events: broadcast::Sender<BusEvent>,
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
///
/// Eight parameters, one over clippy's default ceiling: `id`/`proc`/`io`/`app`
/// are this sheep's own identity and state, `ctl_rx`/`signal_rx` are its two
/// mailboxes (kept apart per [`SheepSlot::signals`]'s own doc), and
/// `events`/`actor_tx` are where its news goes. Private, one caller
/// ([`spawn_sheep_task`]), and every parameter independently threaded through
/// the `select!` below — bundling them into a struct would move the coupling
/// around rather than reduce it, the same call `serve_scripted`
/// (shep-client's own eleven-parameter precedent) makes.
#[allow(clippy::too_many_arguments)]
async fn run_sheep<P: RunningProcess>(
    id: u32,
    mut proc: P,
    io: ProcIo,
    app: ResolvedApp,
    mut ctl_rx: mpsc::Receiver<SheepCtl>,
    mut signal_rx: mpsc::Receiver<SignalRequest>,
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
        to_stdin: _to_stdin,
        //        ^ bound, not `_`: `to_stdin: _` drops the sender inside the
        // `let`, which closes the child's stdin at spawn and gives an
        // opted-in app immediate EOF. That survives today only because Task
        // 10 puts a clone on `SheepSlot` — i.e. the bug would be invisible
        // in the fast loop and would surface as "the app saw EOF" in an
        // e2e. Same `_log_ctl` precedent, one line up.
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
                        // Delivered from the task that OWNS the proc, never
                        // from the actor off a recorded pid. The owning task
                        // is the only place that knows the child has not been
                        // reaped, which is what closes the pid-reuse ABA race
                        // the same way `RunningProcess::signal` already does
                        // for the stop ladder.
                        let _ = done.send(proc.signal_process(sig));
                    }
                    None => signals_open = false,
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
                    Some(message) => {
                        // Forwarded BEFORE it is acted on, and unconditionally.
                        // A subscriber's view of fd 3 must not depend on
                        // whether this daemon happens to have a consumer for
                        // that kind: an `action-reply` nobody is waiting for
                        // is dropped a few lines below, and `deferred.md`
                        // names exactly that message as the traffic the bus
                        // exists to stop losing.
                        let _ = events.send(BusEvent::Channel {
                            id,
                            message: message.clone(),
                        });
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
                                // The child's `id` is the DISPATCH's, not the
                                // sheep's; `id` above is the sheep's. Renamed
                                // at the boundary so no line downstream has to
                                // hold both meanings.
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
    use shep_core::values::UpDuration;

    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    // the one crate-root fixture (IR-33)
    use crate::testing::{
        Harness, RecordingEnforcer, SharedRunner, app_with, armed_entry, harness, idle_stats,
        probe_config, test_paths,
    };
    // Test-only: the one case that drives a real `liveness_probe` has to
    // build the lifecycle extras the production wiring builds at boot, and
    // put the daemon's own reporter behind them.
    use crate::cron::{DEFAULT_MAX_CRON_SLEEP, SystemClock};
    use crate::extras::{ExtrasReports, spawn_extras_reporter};
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
        // and triggers an automatic respawn after the default 100ms
        // `exp_backoff_restart_delay`, well inside the seconds-wide margins
        // this test asserts on: status goes straight back to `Starting` for
        // the NEW process, epoch bumped. `Restart` fires here; `Online`
        // does not, proving the two emits stay separate on the respawn
        // path too.
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
        // backoff delays, which the `exp_backoff_restart_delay` above pins to
        // one fixed sequence rather than the default's. The budget check itself
        // (spec §4: reaching max_restarts=16 unstable exits errors) fires on
        // the 16th exit, using the script's 16th and final entry — the
        // script is exactly, not incidentally, exhausted at that point.
        await_event(&mut rx, 0, ProcessEventKind::Errored).await;
        let list = handle.list().await;
        assert_eq!(list[0].status, ProcStatus::Errored);
        assert_eq!(list[0].restarts, 15); // respawns performed, not exits
        // Task 49: an operator staring at `errored, restarts: 15` with no
        // way to tell a boot loop from a spawn failure is the exact gap
        // `last_exit` closes. The script's constant `code: 1` on its final,
        // budget-exhausting exit must still be readable here.
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
        // `CannotStart`, not `SpawnFailed`: passwd resolution happens in the
        // pass that runs BEFORE anything is registered, so nothing was
        // spawned and nothing was left behind. Saying "spawn failed" here
        // would point an operator at a spawn that never happened.
        assert!(matches!(err, SupervisorError::CannotStart(_)), "{err:?}");
        assert!(
            handle.list().await.is_empty(),
            "a refusal before the registering pass must leave nothing registered"
        );
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

    /// Waits until the flock's single process reads `restarts` restarts and
    /// `Online`, bounded rather than an unbounded `loop` (IR-46): the three
    /// budget-reset tests below all set `exp_backoff_restart_delay = None`
    /// so this state is ready work under their paused clock, but a future
    /// change that reintroduced a delay for that config shape would spin an
    /// unbounded wait for minutes at ~95% CPU with no failing assertion to
    /// notice. One bound here instead of three copies means one place to
    /// change it, and one place to get it wrong.
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
        // This test is about the budget, not the backoff: the sync loop
        // below is a busy `yield_now` poll under a paused clock, which never
        // lets the clock auto-advance, so a non-zero
        // `exp_backoff_restart_delay` (the default since defect 2's fix)
        // would spin it forever instead of failing.
        app.exp_backoff_restart_delay = None;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state, not on the repeated Online event: immediate restarts
        // mean restarts==2 once the never_exits proc is up.
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
        // This test is about the budget, not the backoff: the sync loop
        // below is a busy `yield_now` poll under a paused clock, which never
        // lets the clock auto-advance, so a non-zero
        // `exp_backoff_restart_delay` (the default since defect 2's fix)
        // would spin it forever instead of failing.
        app.exp_backoff_restart_delay = None;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state, not on the repeated Online event: immediate restarts
        // mean restarts==2 once the never_exits proc is up.
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
        // This test is about the budget, not the backoff: the sync loop
        // below is a busy `yield_now` poll under a paused clock, which never
        // lets the clock auto-advance, so a non-zero
        // `exp_backoff_restart_delay` (the default since defect 2's fix)
        // would spin it forever instead of failing.
        app.exp_backoff_restart_delay = None;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        // Sync on state, not on the repeated Online event: immediate restarts
        // mean restarts==2 once the never_exits proc is up.
        let info = wait_for_restarts_online(&handle, 2).await;
        assert_eq!(
            (info.status, info.restarts),
            (ProcStatus::Online, 2),
            "never reached the never_exits proc -- got {info:?}"
        );

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

    // Adversarial finding from a whole-branch review: a `Delete` that
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
    // anything. That is a real trap, not a hypothetical one: the single-sheep
    // version of this case had `handle.list()` after `shutter.await` always
    // hit `EngineStopped`, whether or not the fix was applied.
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

    // Regression for a reviewer's finding: a Delete racing a
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

    /// Task 49, Rin's own call on the open question in `handle_exited`'s own
    /// doc: an operator's `shep stop` still ends the process by a real
    /// signal, and `last_exit` must say so rather than going back to `None`
    /// because shep, not a crash, asked for it. `never_exits` obeys the
    /// ladder's first (`SIGTERM`) rung, so the wait resolves on that signal
    /// rather than on `kill_tree`'s `SIGKILL` -- the raw number this test
    /// pins is 15, the same constant `ExitOutcome`'s own doc names.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_still_shows_its_signal_as_the_last_exit() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
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

    /// A respawn that never spawns must not keep showing the PREVIOUS
    /// process's exit code. The two failures look identical in the status
    /// column -- both land in `Errored` -- and `last_exit` is the only thing
    /// that separates "your app crashed with 1" from "shep could not start
    /// your app at all", which is the entire question this field was added
    /// to answer.
    ///
    /// `ScriptedRunner` hands out `SpawnFailed` once its script list is
    /// exhausted, so one script that exits `1` gives exactly the sequence:
    /// a real exit that records a code, then a respawn that fails to spawn.
    #[tokio::test(start_paused = true)]
    async fn a_respawn_that_fails_to_spawn_clears_the_previous_exit_code() {
        let (events, _rx) = tokio::sync::broadcast::channel(64);
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

    // --- `Stopping`: reload's drainee, pinned against the guards it must
    // never pass ---
    //
    // These two cases build the actor directly — the same private-module
    // access `spawn`'s own struct literal uses — and call the guarded
    // handlers as plain functions rather than reaching them through a whole
    // swap. That is what makes a failure here name the guard that stopped
    // rejecting instead of some later consequence of it: nothing between the
    // handler and the assertion can absorb the difference. The reload cases
    // below drive the same guards through `SupervisorHandle::reload`, and
    // these sit alongside that coverage rather than in place of it.

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
            to_child: None,
            signals: None,
            to_stdin: None,
            manual: None,
            pending_delete: false,
            epoch,
            ready_tx: None,
            actions: ActionWaits::default(),
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

    /// One sheep marked as a reload's drainee — `Stopping` on `status` AND
    /// `ReloadState::Drainee` on `reload`, which is the pair a real swap
    /// writes — holding a live signal mailbox whose receiver the caller keeps.
    ///
    /// Both halves are load-bearing. `actor_with_stopping_drainee` sets only
    /// the status, which is what the guards it serves read; a signal has to be
    /// tested against the MARKER, because that is the field `begin_action`
    /// filters on and the field `begin_signal` must not.
    fn actor_with_a_drainee_holding_a_signal_mailbox(
        dir: &tempfile::TempDir,
    ) -> (Actor<ScriptedRunner>, mpsc::Receiver<SignalRequest>) {
        // No scripts: nothing here spawns, so an empty list turns a spawn that
        // should not have happened into a loud `SpawnFailed`.
        let (mut actor, _mailbox) = actor_with_one_online_sheep(dir, vec![]);
        let slot = actor.sheep.get_mut(&0).expect("the fixture registers id 0");
        slot.entry.status = ProcStatus::Stopping;
        slot.entry.reload = ReloadState::Drainee { new_id: 1 };
        // Wide enough that no case can fill it, so a `try_send` that comes
        // back `Full` means a bug rather than a fixture too small.
        let (signals, signal_rx) = mpsc::channel(16);
        slot.signals = Some(signals);
        (actor, signal_rx)
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
                to_child: None,
                signals: None,
                to_stdin: None,
                manual: None,
                pending_delete: false,
                epoch: 0,
                ready_tx: None,
                actions: ActionWaits::default(),
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
    /// The two entries are alike in everything a selector can read — same
    /// status, same fold, same registration, adjacent ids — so the marker is
    /// the only difference between them. That is what lets a case watching
    /// the dog drop out of a wildcard's answer conclude the MARKER did it,
    /// rather than a status or a fold the wildcard would have passed over
    /// anyway.
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
                },
            );
        }
        let (events, _events_rx) = broadcast::channel(64);
        let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
        let actor = Actor {
            runner: ScriptedRunner::new(Vec::new()),
            paths,
            events,
            tx,
            sheep,
            next_id: DOG_ID + 1,
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

    /// fails if a wildcard reaches a dog. Every assertion is load-bearing
    /// and none implies another: without the last two a helper that excluded
    /// dogs from EVERYTHING passes, and `shep disable bark` — which stops the
    /// dog by naming it — would silently match nothing.
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

    /// fails if `to_info` invents the marker rather than reading the entry's
    /// — the shape that puts a dog in a listing as an ordinary sheep, with
    /// nothing anywhere left to say which it is.
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
    /// Panics if `app` does not normalize, or if the actor refuses the
    /// start — both a fixture bug at the call site, not a condition under
    /// test. (Not `#[track_caller]`: it is a no-op on an async fn, and would
    /// only mislead a reader into thinking a panic here points back to the
    /// call site.)
    async fn start_app(h: &Harness, app: AppConfig) -> Vec<ProcessInfo> {
        h.ctx
            .supervisor
            .start(vec![normalize(app).unwrap()])
            .await
            .unwrap()
    }

    /// The instance slot of every registered instance of `name`, ascending.
    ///
    /// Read off `out_file` rather than off the entry, because it is the number
    /// an OPERATOR sees: `logs/web-2-out.log` is the file they tail and
    /// `SHEP_INSTANCE=2` is what the process reads. Asserting on the internal
    /// field would pass on a build that allocated the slot correctly and then
    /// derived the log path from something else.
    ///
    /// # Panics
    ///
    /// If a matched row carries no `out_file`, or one this fixture cannot
    /// parse — both mean the fixture stopped describing what it thinks it
    /// does.
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
    /// A scale-down's reply is the SURVIVORS and deliberately does not wait
    /// for the departures (`handle_scale`'s own doc says why), so a case that
    /// asserts on the flock afterwards has to wait for the kill ladders it
    /// started. Bounded (IR-46) because the only other failure mode is a poll
    /// loop that never ends.
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
    /// carrying the same normalized spec — the fixture the two stored-count
    /// cases need, because that field reaches no reply.
    ///
    /// Built on the same hand-written `SheepSlot` literal every other actor
    /// fixture in this module uses, so a field added to that struct breaks
    /// this in the same visible way it breaks the others.
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
                },
            );
        }
        let (events, _events_rx) = broadcast::channel(64);
        let (tx, _rx) = mpsc::channel(MAILBOX_CAPACITY);
        Actor {
            runner: ScriptedRunner::new(scripts),
            paths,
            events,
            tx,
            sheep,
            next_id: instances,
            next_action_stamp: 0,
            pending: Vec::new(),
            shutting_down: false,
            extras: None,
            registry: ExtrasRegistry::default(),
            reloads: HashMap::new(),
            smits: Smits::new(),
        }
    }

    /// Every registered slot's STORED instance count, ascending by id.
    fn stored_instance_counts(actor: &Actor<ScriptedRunner>) -> Vec<u32> {
        let mut ids: Vec<u32> = actor.sheep.keys().copied().collect();
        ids.sort_unstable();
        ids.iter()
            .map(|id| actor.sheep[id].entry.spec.config().instances)
            .collect()
    }

    /// A `ReloadJob` built the way `advance_reload` builds one (`:2435`), for
    /// a case that only needs `self.reloads` to hold an entry under `name` —
    /// no full swap is driven. `name` names the app only for the call site's
    /// readability: the map key the caller inserts under is what actually
    /// associates the job with `name`, and `ReloadJob` itself carries no
    /// name.
    fn reload_job_for(name: &str) -> ReloadJob {
        let _ = name;
        ReloadJob {
            queue: VecDeque::new(),
            swap: ReloadSwap {
                old_id: 0,
                new_id: 1,
                phase: ReloadPhase::AwaitReady,
            },
        }
    }

    /// fails if scaling up does not take the lowest free slots. Slot numbers are
    /// visible to the app (`SHEP_INSTANCE`) and to the filesystem
    /// (`web-2-out.log`), so which ones a scale hands out is a contract, not an
    /// implementation detail.
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

    /// fails if scaling down takes the LOWEST slots. Taking the highest is what
    /// makes 2 -> 4 -> 2 a round trip back to slots 0 and 1; taking the lowest
    /// would leave 2 and 3 — the same count, a different flock, different log
    /// files, and a different SHEP_INSTANCE for every survivor.
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
        // The reply is the survivors; the two removals run kill ladders this case
        // has to let finish before it can read the flock.
        settle_to(&h, "web", 2).await;
        assert_eq!(instance_slots_of(&h, "web").await, vec![0, 1]);
    }

    /// fails if scaling to the count an app already has does anything at all.
    /// Idempotence is the whole argument for an absolute count over a delta, and
    /// an operator re-running a provisioning script must not restart the flock.
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
        // Two scripts for two spawns: a scale that respawned would need a third
        // and would fail loudly rather than quietly, but the id check is what
        // says WHICH failure this is.
    }

    /// fails if `scale <name> 0` is accepted. `normalize` refuses `instances == 0`
    /// on every other path into the daemon, so accepting it here would put a config
    /// through the engine that the engine's own validator rejects.
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

    /// fails if an unregistered name is anything but NotFound. `shep stock typo 4`
    /// exiting 0 would be the worst answer available.
    #[tokio::test(start_paused = true)]
    async fn scaling_an_unregistered_app_is_not_found() {
        let h = harness(vec![]);
        assert_eq!(
            h.ctx.supervisor.scale("ghost", 2).await.unwrap_err(),
            SupervisorError::NotFound
        );
    }

    /// fails if a dog can be scaled. A dog is one process by contract (spec §8) —
    /// two metrics dogs would race for the same listen port, and two bark dogs
    /// would double every alert.
    ///
    /// Actor-tier, on the existing sheep-and-dog fixture: `DogSource` is written
    /// by `start_dog` and the two entries in that fixture differ in the marker and
    /// in nothing else, which is what makes a refusal here attributable to the
    /// marker rather than to a status or a fold.
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

    /// fails if an app mid-reload can be scaled. A reload holds two live processes
    /// in one instance slot; a scale-down picking that slot removes one of them and
    /// leaves the swap with nothing to finish.
    ///
    /// Actor-tier, and the guard it pins reads `Actor::reloads` — a map with no
    /// reply-side spelling at all, so this is the only tier that can put an entry
    /// in it without driving a whole swap.
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

    /// fails if a scale can be issued against an app whose EARLIER scale is still
    /// shutting instances down. The reply to a scale-down is the survivors and
    /// deliberately does not wait for the departures, so those slots stay
    /// registered — and a second scale counting them found four where one was
    /// going to be left, called itself a no-op, answered `Ok` with four instances
    /// and no shortfall, and let `rpc` record `instances = 4` into the muster
    /// roll. The flock then settled to one. Two `shep stock` calls in a
    /// provisioning script is the ordinary way to reach that.
    ///
    /// The doomed three `never_reports_its_exit`, which is what makes the case
    /// deterministic rather than a race against its own kill ladders: the marker
    /// under test is set the moment the departure is claimed and is independent
    /// of how the child eventually dies. An app that merely drains connections on
    /// `SIGTERM` — every app that drains connections on shutdown — sits in the
    /// same state for its whole `kill_timeout`.
    #[tokio::test(start_paused = true)]
    async fn a_scale_is_refused_while_an_earlier_ones_departures_are_still_leaving() {
        // Scripts go out in spawn order, so instance 0 — the survivor — gets the
        // first and the three a scale-down removes get the other three.
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

    /// fails if the departures guard outlives the departures. A refusal that
    /// never lifts would be worse than the divergence it replaces — the operator
    /// could not scale the app again at all without restarting it.
    ///
    /// `never_exits` rather than the case above's wedged scripts: these obey
    /// `SIGTERM`, so the ladders end with no clock to advance and `settle_to` is
    /// the forcing mechanism (IR-46).
    #[tokio::test(start_paused = true)]
    async fn a_scale_is_accepted_again_once_the_departures_have_left() {
        // Four for the first flock, three for the instances the scale back up
        // spawns.
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

    /// fails if a scale forgets to write the new count back onto the app. Without
    /// this, `shep stock web 4 && shep save` records `instances = 2` and the next
    /// reboot silently reverts the scale — the bug is invisible until the machine
    /// comes back.
    ///
    /// Actor-tier: the stored count is `SheepSlot::entry.spec`, which reaches no
    /// reply and no bus event. `Scaled::app` is the other half and is checked here
    /// too, because a build that returned the right config and stored the wrong
    /// one would pass either assertion alone.
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

    /// fails if a PARTIAL scale-up stores the count it asked for rather than the
    /// one it got. The instances that did spawn stay — unwinding them would turn
    /// one failed spawn into an outage of everything the call had already brought
    /// up — but every registered slot must then claim the number really running,
    /// or `shep describe` reads 4 while `shep save` writes 4 for a flock of 3 and
    /// the discrepancy surfaces at the next reboot.
    ///
    /// One script for two requested spawns: `ScriptedRunner` answers the first and
    /// fails the second with `script exhausted`, which is this module's standing
    /// way to make exactly one spawn fail.
    ///
    /// Three entries, not two: `spawn_fresh`'s own contract registers an
    /// `Errored` slot for the attempt that failed, exactly as it does for
    /// `Start` (`spawn_fresh`'s doc: "always inserts a `SheepSlot`... regardless
    /// of the outcome"). That third slot is still registered under `web`, so a
    /// FUTURE scale call counts it as part of `current` — it must not be left
    /// holding the count this call asked for either, or it is one more place
    /// the same discrepancy can surface from.
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

    /// fails if the listing comes back in id order. Built so id order and
    /// name order genuinely disagree — `web` is registered second and must
    /// still come first — because a fixture whose two orders coincide
    /// cannot tell the two implementations apart, and that is the shape of
    /// fixture this project has shipped before.
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

    /// fails if `start_dog` marks the entry and the marker is then lost on
    /// respawn — which is the shape a marker written by the START path
    /// rather than carried by the ENTRY takes, and it is invisible until a
    /// dog crashes once: the dog vanishes from the dogs table and reappears
    /// among the flock, with no error anywhere.
    ///
    /// Two scripts, of which a correct run uses both: the crash the entry
    /// earns its restart with, and the process that restart produces.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_restarts_is_still_a_dog() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = broadcast::channel(64);
        let runner =
            ScriptedRunner::new(vec![ProcScript::const_exit(1), ProcScript::never_exits()]);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);

        let dog = handle
            .start_dog(dog_app("bark"), DogSource::BuiltIn)
            .await
            .unwrap();
        assert_eq!(dog.dog, Some(DogSource::BuiltIn));

        // The scripted exit, then the automatic respawn it earns. Awaited on
        // the bus rather than polled for: `Restart` is emitted from inside
        // the respawn, after the entry has been rewritten, so a listing taken
        // once it lands cannot read the entry mid-flight.
        expect_event(&mut rx, dog.id, ProcessEventKind::Restart).await;
        let after = dog_row(&handle.list().await, dog.id);

        assert_eq!(
            after.restarts, 1,
            "the ordinary restart path, not a dog one"
        );
        assert_eq!(after.dog, Some(DogSource::BuiltIn));
    }

    /// fails if the marker is written only onto the entry a SUCCESSFUL spawn
    /// registers. A dog whose binary is not there is the case `adopt` with a
    /// bad path produces, and it has to be visible in the dogs table as
    /// `Errored` — an unmarked one is a sheep nobody started, sitting in the
    /// flock with a name the operator never chose.
    ///
    /// No scripts at all: [`ScriptedRunner`] fails a spawn by running out of
    /// them, which is the only way it can fail.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_cannot_be_spawned_is_still_a_dog() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = broadcast::channel(64);
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

    /// fails if a reload's replacement is built without the marker.
    /// `shep reload bark` names the dog exactly, so it reaches it (a
    /// wildcard would not), and an unmarked replacement turns the dog into a
    /// sheep at the one moment nothing is watching: the swap reports itself
    /// as a success either way.
    ///
    /// Three scripts, of which a correct run uses two — the original and its
    /// replacement. The third is for the spawn a broken run makes that a
    /// correct one does not, so it lands as a live entry rather than as the
    /// `SpawnFailed("script exhausted")` that reads like an unrelated
    /// failure.
    #[tokio::test(start_paused = true)]
    async fn a_reloaded_dog_is_still_a_dog() {
        let dir = tempfile::tempdir().unwrap();
        let (events, mut rx) = broadcast::channel(256);
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

        // The replacement is the next id the actor hands out, and `Reloaded`
        // on it is the swap being over — the drainee is deregistered by then,
        // so the listing below holds the replacement alone.
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

    /// fails if a dog can be started once a graceful shutdown has begun
    /// (CRITICAL-1), which is the rule `Start` already follows and for the
    /// same reason: the shutdown aggregation's `online` snapshot was fixed
    /// when it ran, so a child registered after it is one nothing will kill.
    ///
    /// The runner carries a script on purpose. With the guard deleted the
    /// spawn a broken run makes really does succeed, so the registration
    /// assertion below moves — against an exhausted runner it would fail for
    /// the unrelated reason that nothing could spawn at all.
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

    /// fails if `start_dog` is not idempotent by name. `shep enable` runs
    /// against a daemon that may already have the dog — from `enabled_dogs`
    /// at boot — and a second live process under one name would give the dog
    /// two connections, two metrics listeners on one port, and two copies of
    /// every bark.
    ///
    /// Two scripts, of which a correct run uses one. The second is for the
    /// spawn a non-idempotent `start_dog` makes into instance slot 1, so
    /// that the break shows up as the extra entry it is rather than as a
    /// spawn failure.
    #[tokio::test(start_paused = true)]
    async fn enabling_a_dog_twice_starts_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let (events, _rx) = broadcast::channel(64);
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
    // `Drainee` names the replacement and belongs on the instance being
    // replaced; `Replacement` belongs on the replacement and names nothing, since the only
    // caller that needs the other half holds the reload job that has it;
    // `Stopping` is the drainee's status and only the drainee's. Asserted
    // against the machine that sets them rather than a rehearsal of it —
    // `ProcessEntry::reload` never reaches the wire, so this is the only tier
    // that can read it back.
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
        assert_eq!(drainee.reload, ReloadState::Drainee { new_id });
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

    // fails if a reload is accepted once a graceful shutdown has begun
    // (CRITICAL-1): its replacement would be a child outside the shutdown
    // aggregation's `online` snapshot, fixed at the moment that ran, and so
    // orphaned when the actor exits.
    //
    // Two guards stand between a shutdown and that child, and the case has to
    // reach each separately, because the first stops anything getting to the
    // second. The REPLY is the only witness `Command::Reload`'s own guard
    // has: `advance_reload` refuses the spawn on its own re-check, so with
    // the command guard deleted the actor's state is identical either way and
    // no assertion on it can move. The direct `advance_reload` call below is
    // what puts that second, defence-in-depth guard under test — delete it
    // and the four state assertions all move, because the runner carries a
    // script and the spawn a broken guard performs really does register a
    // replacement in the drainee's slot.
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

        // `advance_reload` is the one door into `SpawnNew`, reached here as
        // a job that somehow survived into a shutdown would reach it.
        actor.advance_reload("web", VecDeque::from([0]));

        assert_eq!(actor.sheep.len(), 1, "nothing new was registered");
        assert_eq!(actor.sheep[&0].entry.status, ProcStatus::Online);
        assert_eq!(actor.sheep[&0].entry.reload, ReloadState::None);
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

    // fails if a second reload of an app already reloading is accepted.
    //
    // The app is clustered so that an acceptance has somewhere to go. With
    // instance 0 mid-swap, a second reload finds instance 1 still `Online`,
    // starts a swap on it, and `advance_reload`'s insert overwrites the first
    // job — whose drainee is then never reaped and whose queue is dropped,
    // on top of a third live entry in an app that asked for two. A
    // single-instance fixture shows none of that: its two entries are
    // `Stopping` and `Starting`, neither is reloadable, so the second reload
    // is accepted and then spawns nothing, and the reply is the only thing
    // that moves.
    #[tokio::test(start_paused = true)]
    async fn a_second_reload_of_an_app_already_reloading_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        // Four scripts, of which a correct run uses three: two originals and
        // the first swap's replacement. The fourth is sized for the spawn a
        // wrongly-accepted second reload performs, so it succeeds into a live
        // entry rather than being hidden behind an exhausted pool.
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

    // fails if a sheep whose kill ladder is already running counts as
    // replaceable. `claim_manual` sets the `manual` marker and sends the
    // `Kill` without touching the status, so an instance a memory breach — or
    // a cron occurrence, or an operator's own `restart` — claimed a moment ago
    // reads `Online` for the whole ladder, up to `kill_timeout`. A swap
    // started against one is doomed the instant it is accepted: that ladder's
    // exit lands inside `AwaitReady` carrying a marker, which abandons the
    // reload, kills the replacement it had just spawned, and warns that an
    // operator's command reached the drainee first when no operator issued
    // one. The caller was told `Ok` and gets the hard restart the overlap
    // exists to avoid.
    //
    // Skipping the instance is what the not-`Online` case already gets, and it
    // costs nothing that was not already lost: the instance is on its way out
    // under a restart that will bring it back on the same code a replacement
    // would have carried.
    #[tokio::test(start_paused = true)]
    async fn a_reload_skips_an_instance_whose_kill_ladder_is_already_running() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts, and a correct run uses both: the original, which defies
        // its signal so the breach's ladder is still running when the reload
        // lands, and the respawn that ladder ends in. A wrongly-spawned
        // replacement takes the second one and succeeds into a live entry
        // rather than hiding behind an exhausted pool, which is what the two
        // assertions below read.
        let (handle, runner, mut rx) = started(
            &dir,
            AppConfig::minimal("web", "./srv"),
            vec![ProcScript::ignores_signals(), ProcScript::never_exits()],
        )
        .await;
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        handle.extra_restart(0, pid).await;

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

        // The restart the breach asked for still lands, in the instance's own
        // slot and under its own id — no abandonment, nothing deregistered.
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

    // A liveness failure or memory breach raised against a drainee must ride
    // out to that drainee's own exit: claiming its marker kills the instance
    // shep is in the middle of replacing, before the replacement can serve,
    // which is the outage a reload exists to avoid.
    //
    // Two independent checks stop it, so this case fails only when BOTH are
    // gone, and the honest reading is that it pins the OUTCOME rather than
    // either mechanism. `handle_extra_restart`'s guard 4 rejects a status
    // that is not `Online`, and `begin_manual` drops an automatic restart
    // against either half of an uncommitted swap. Each has its own case that
    // reddens on a single line — `a_stopping_sheep_rejects_an_extra_restart`
    // for the guard, `an_automatic_restart_never_lands_on_either_half_of_a_swap`
    // for the drop — and neither of those drives a real report through a real
    // reload, which is what this one is for.
    #[tokio::test(start_paused = true)]
    async fn a_report_raised_against_a_drainee_never_takes_it_off_the_reload() {
        let dir = tempfile::tempdir().unwrap();
        // Three scripts, of which a correct run uses two. `kill_counts`
        // collects one entry per SUCCESSFUL spawn, so with a pool of two a
        // report that wrongly restarted the drainee would be invisible to it:
        // the respawn would find the pool empty, fail, and leave the drainee
        // `Errored` rather than the live process the bug really produces. The
        // third script lets that respawn succeed, so the count below reads
        // the extra spawn and the status reads what it left behind.
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
        handle.extra_restart(0, pid).await;

        // `Restart`, not `Delete`, and the difference is the whole assertion:
        // the bug kills the drainee and RESPAWNS it into a slot its
        // replacement already holds, which never emits a `Delete` for this id.
        // Watching for one would be watching for something the bug does not
        // do. The window is shorter than `listen_timeout`, so nothing but the
        // report can move the drainee inside it, and the drainee's real
        // deregistration is asserted below where it belongs.
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

    // `a_report_raised_against_a_drainee_never_takes_it_off_the_reload`'s
    // guarantee, observed end to end instead of off a report handed to the
    // actor by hand: a real `liveness_probe`, the real `OsProber` the engine
    // builds for it, the real loop that probes with it, the daemon's own
    // extras reporter, and a reload the supervisor performs. The drainee
    // stops answering while its replacement is still starting, and must be
    // reaped when the swap commits rather than restarted into the slot the
    // replacement already holds.
    //
    // What it pins is the OUTCOME, and TWO mechanisms stand behind that
    // outcome, so it reddens only when both are gone. Each is one line:
    // `handle_extra_restart`'s `status != Online` rejection, and
    // `begin_manual`'s `if held_off_by_a_swap { continue; }`. Removing either
    // one alone leaves this case green — checked both ways round — so a
    // comment naming a single mechanism here would be naming a bug this case
    // cannot catch. With both gone the drainee is killed and respawned, and
    // the `Restart` assertion below is what fails.
    //
    // The replacement's own restart at the end is a control: same run, same
    // dead target, same loop, and it says the chain really does carry a
    // failing probe all the way to a restart here. Delete the
    // `spawn_extras_reporter` call and it is what reddens (`no Restart for id
    // 1 within 30s`), where every assertion above would pass on a fixture
    // that reported nothing at all. What it does NOT do is prove the
    // DRAINEE's own probe failed — the mutation above proves that, since a
    // drainee nothing reported against cannot be respawned by deleting two
    // rejections. A change that armed no liveness loop on a drainee
    // specifically would leave this case green.
    //
    // The paused clock holds here where `readiness_probe_app_stays_starting_until_the_probe_passes`
    // needs real time, and the difference is that every probe in this case
    // must FAIL. A failure waits on nothing the frozen clock could hold up:
    // there is no listener to bind, no accept and no child to exit, and a
    // connection refused and an auto-advanced `timeout` are the same verdict.
    #[tokio::test(start_paused = true)]
    async fn a_drainee_whose_liveness_probe_fails_is_reaped_rather_than_restarted() {
        let dir = tempfile::tempdir().unwrap();
        // Reserve a port and release it: nothing ever listens there, so every
        // probe below fails, from the first one to the last.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);

        let mut app = AppConfig::minimal("web", "./srv");
        // Both halves go online by hand, so the liveness failure lands
        // squarely inside `AwaitReady` rather than racing a deadline...
        app.wait_ready = true;
        // ...and that window cannot elapse on its own while this case waits
        // out a probe interval inside it.
        app.listen_timeout = UpDuration::from_millis(60_000);
        app.liveness_probe = Some(ProbeConfig {
            // The floor `spawn_liveness_task` honours anyway, so the report
            // lands as early in the window as one can.
            interval: UpDuration::from_millis(1_000),
            timeout: UpDuration::from_millis(500),
            // One failed probe is a failure here: the threshold is another
            // case's subject.
            failure_threshold: 1,
            ..probe_config(ProbeKind::Tcp, &addr.to_string())
        });

        // Four scripts for the three spawns a correct run performs — the
        // original, the replacement, and the replacement's own liveness
        // restart at the end — plus one for the respawn a broken
        // implementation performs in between. `ScriptedRunner` answers
        // `SpawnFailed("script exhausted")` once it runs out, which would land
        // that respawn `Errored` rather than the live process the bug really
        // produces, and `Errored` is a state this case could mistake for the
        // failure it is looking for.
        let (events, mut rx) = tokio::sync::broadcast::channel(256);
        let runner = Arc::new(ScriptedRunner::new(vec![ProcScript::never_exits(); 4]));
        // Capacity past anything this case raises: one liveness failure per
        // armed instance, and no breaches at all.
        let (breaches_tx, breaches_rx) = mpsc::channel(8);
        let (liveness_tx, liveness_rx) = mpsc::channel(8);
        let handle =
            SupervisorBuilder::new(SharedRunner(Arc::clone(&runner)), test_paths(&dir), events)
                .extras(Extras {
                    // Neither seam is read: the app configures no
                    // `cron_restart` and no `max_memory`. The liveness half of
                    // `reports` is the whole reason the extras are wired here.
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
        // The daemon's own reporter, not a forwarding line written here: it
        // is the step that turns a `LivenessFailure` into the `extra_restart`
        // the two rejections then rule on.
        let _reporter = spawn_extras_reporter(breaches_rx, liveness_rx, handle.clone());
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();

        handle.tx.send(Msg::Ready { id: 0 }).await.unwrap();
        expect_event(&mut rx, 0, ProcessEventKind::Online).await;

        handle
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_event(&mut rx, 1, ProcessEventKind::Start).await;

        // Five times the probe interval, so the failure has long since been
        // raised, and far short of the 60s readiness deadline, so nothing but
        // that failure could have moved the drainee inside the window.
        // `Restart`, not `Delete`: the bug kills the drainee and respawns it,
        // which never emits a `Delete` for this id.
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

        // The replacement takes over, and the drainee goes the way a reload
        // ends one: reaped, with nothing put back in its place.
        handle.tx.send(Msg::Ready { id: 1 }).await.unwrap();
        expect_event(&mut rx, 1, ProcessEventKind::Online).await;
        expect_event(&mut rx, 0, ProcessEventKind::Delete).await;
        let after = handle.list().await;
        assert_eq!(after.len(), 1, "the drainee left no registration behind");
        assert_eq!(after[0].id, 1);

        // The control. The replacement is the app's live instance now, armed
        // against the same dead target, and its own probe failure DOES
        // restart it — so the chain this case rests on is delivering in this
        // run rather than reporting nothing at all.
        expect_event(&mut rx, 1, ProcessEventKind::Restart).await;
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

    // fails if abandoning a reload reports a drainee that is itself dying as
    // `Online`. The restore exists for the drainee that goes back to serving;
    // one an operator's `stop` already claimed is going nowhere but away, and
    // saying `Online` of it hands `shep flock` a live pid for a process on
    // its way out and re-opens `handle_extra_restart`'s `Online` guard for
    // the length of the ladder.
    //
    // The scripts are asymmetric the other way round from
    // `an_operators_stop_mid_reload_leaves_the_app_stopped_and_registered`,
    // and for the same reason: the `stop` claims both halves at once, and it
    // is the REPLACEMENT's exit landing first that routes the abandonment
    // through a drainee that is still alive. A drainee defying its signal is
    // what pins that order.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_reload_never_reports_a_dying_drainee_as_online() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        // Nobody signals the replacement, so the swap is still `AwaitReady`
        // when the stop lands and the abandonment is reachable at all.
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

        // Not awaited here: the drainee defies its signal, so the stop's own
        // reply is a whole `kill_timeout` away and the window under test is
        // before that.
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
    // `Replacement` marker cancels that event too (`handle_ready_result` routes
    // on the marker), and nothing is left that can reach `finish_swap`. The
    // job then refuses every later reload of the app for as long as the
    // daemon runs, and drops the rest of a clustered reload's queue after the
    // caller was already told `Ok`.
    //
    // Two separate defects live on this one path and the case checks both,
    // because they fail independently: the job outliving both of its ids
    // (which the second `reload` catches), and that ending reaching the bus as
    // nothing but a log line (which the `ReloadAbandoned` catches). Deleting
    // the emit leaves the reload verb working and the subscriber blind;
    // deleting the removal leaves the bus honest and the verb dead.
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

    /// Task 49's sibling claim to the restart-count test above, same
    /// fixture, same reasoning: `spawn_replacement` reads the drainee's
    /// `last_exit` before the drainee itself has exited again for the
    /// reload, so the replacement's answer to "why did this instance last
    /// stop" must still be the manual restart's kill, not `None` reset by
    /// the swap.
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

    /// One process event, flattened to what a reload's bus claims are made
    /// of: who it names, what happened, the status that went out with it,
    /// and whether it was reported as an operator's doing.
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
    /// [`expect_event`] skips past whatever it is not looking for, which is
    /// exactly wrong for an ordering claim — this keeps the run. Bounded by
    /// [`SWAP_WINDOW`] (rule 11: a bounded `timeout` + `recv`, never a bare
    /// `try_recv`), so an event that never arrives fails naming what was
    /// waited for rather than parking the suite. A `Lagged` is fatal rather
    /// than skipped: a hole in the stream is a hole in every claim read off
    /// it.
    async fn events_through(
        rx: &mut tokio::sync::broadcast::Receiver<BusEvent>,
        id: u32,
        kind: ProcessEventKind,
    ) -> Vec<Seen> {
        let collect = async {
            let mut seen = Vec::new();
            loop {
                match rx.recv().await {
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

    // fails if a completed swap goes unreported on the bus, or is reported in
    // an order a subscriber cannot read.
    //
    // A reload's reply is an ACCEPTANCE, so these frames are the whole of
    // what a client ever learns about how the reload actually went, and each
    // of the three claims here is one a subscriber has to be able to make:
    //
    // - `Reload` names the drainee BEFORE the replacement's `Start`. Without
    //   the ordering, the first thing a subscriber sees is a second `Start`
    //   in an instance slot that already had a live entry, which nothing
    //   explains. Deleting the emit, or moving it after the `Start`, fails
    //   here.
    // - `Reload` carries `Stopping`, which is what says the named instance is
    //   the one going rather than the one arriving.
    // - `Reloaded` lands only once the drainee's `Delete` has, so it means
    //   "the swap is over" and not merely "the replacement is up". An emit
    //   moved into `reload_ready_result`, where the replacement goes
    //   `Online`, is the plausible slip and this ordering is what catches it.
    //
    // Three scripts, of which a correct run uses two — the original and its
    // replacement. The third is for the spawn a broken run makes that a
    // correct one does not (a restart in place of a swap), so it lands as a
    // live entry rather than as the `SpawnFailed("script exhausted")` that
    // turns into `Errored` and reads like an unrelated failure.
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

    // fails if `finish_swap` announces a swap on the strength of the
    // replacement still being REGISTERED rather than still SERVING. The two
    // diverge for a whole class of exit: a replacement that goes down inside
    // the drain window keeps its row in the map — `Stopped` for an app that
    // does not autorestart, `Errored` for one whose budget ran out,
    // `WaitingRestart` for one still owed a respawn — so a registration test
    // passes for every one of them and `Reloaded`, the one event that means a
    // swap succeeded, goes out naming a process that is down.
    //
    // Two scripts, both used by a correct run. The instance being replaced
    // ignores its stop signal, so its drain runs the full `graceful_timeout`
    // (8000ms) and there is a window to die inside; the replacement exits
    // 5000ms in, which is after its 3000ms heuristic readiness wait — so the
    // swap is committed and this is not the abandon-before-ready case — and
    // well before the drain ends.
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

    // fails if nothing but a message from another task can end a reload.
    //
    // Every transition out of a `ReloadJob` is driven by a `Msg::Exited` from
    // a sheep task or a `Msg::ReadyResult` from a readiness task. Neither is
    // guaranteed to arrive: `kill_process`'s post-`SIGKILL` `wait` is
    // unbounded, so one instance wedged in uninterruptible sleep is enough,
    // and the job then sits at `DrainOld` for the life of the daemon. Nothing
    // recovers it from inside the process — `shep reload web` is refused on
    // the mere presence of the map key, `shep reload all` with it because the
    // refusal is whole-selector, `ProcessInfo` carries no reload marker to see
    // it by, and `shep delete web` resolves on the exit that is not coming.
    //
    // The drainee here is `never_reports_its_exit`, which delivers and counts
    // the `SIGKILL` and withholds only the exit — exactly the shape the
    // unbounded `wait` cannot see past. Deleting the deadline, or letting a
    // stale one be dropped when it is not stale, fails on the first assertion;
    // arming it per phase rather than per swap and forgetting to re-arm at the
    // commit fails on it too. Three scripts: the wedged original, its
    // replacement, and the replacement the SECOND reload spawns once the verb
    // is working again — a broken run never reaches that one.
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

        // The swap commits — the replacement is serving — and then stalls: the
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

        // The point of giving up: the verb works again without restarting the
        // daemon.
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

    // fails if a swap's deadline is allowed to act on a job that has moved on
    // — the staleness rule `RestartDue` established, read off the swap's
    // `new_id` here because ids are never reused.
    //
    // A clustered app is what puts the two in the same window. Each drainee
    // ignores its stop signal, so a swap runs its full `listen_timeout` +
    // `graceful_timeout` (3000 + 8000): the first ends at 11000, its deadline
    // is still pending and comes home at 16000, and by then the job under that
    // app's name is the SECOND swap, mid-drain. Dropping the id check turns
    // that arrival into an abandonment of a swap that was going fine, and the
    // second `Reloaded` never comes.
    //
    // Four spawns, two originals and two replacements, all of which a correct
    // run performs.
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

    // fails if a deadline that expires while the swap is still abandonable
    // takes the committed ending instead — dropping the job and leaving the
    // instance being replaced `Stopping` under a drain nothing started, with
    // the replacement that never proved itself still holding the slot.
    //
    // Driven directly, because the only way to reach this arm from outside is
    // a readiness result that never arrives, and every readiness task carries
    // its own `listen_timeout` and always sends. That is also why the arm is
    // worth a case at all: it is the half of the watchdog that no end-to-end
    // path can exercise, and untested protection is the kind that quietly
    // stops working.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_before_the_commit_puts_the_instance_being_replaced_back() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, _mailbox) =
            actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);
        // A live control sender is what says this instance's task is still
        // there to go back to; the fixture leaves it `None`.
        let (ctl_tx, _ctl_rx) = mpsc::channel(SHEEP_CTL_CAPACITY);
        actor.sheep.get_mut(&0).expect("the fixture's sheep").ctl = Some(ctl_tx);

        let new_id = actor
            .spawn_replacement(0)
            .expect("the fixture's one script covers this spawn");
        actor.reloads.insert(
            "web".to_string(),
            ReloadJob {
                queue: VecDeque::new(),
                swap: ReloadSwap {
                    old_id: 0,
                    new_id,
                    phase: ReloadPhase::AwaitReady,
                },
            },
        );

        actor.handle_reload_deadline("web", new_id);

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

    // fails if an abandoned reload ends in silence on the bus. The reply was
    // an acceptance, so a subscriber that hears a `Reload` and never hears
    // again cannot tell a reload still running from one that gave up — and
    // giving up is the case where knowing matters, because the app is still
    // on the old code.
    //
    // The abandonment here is the readiness one: `wait_ready` with nothing
    // ever signalling the replacement, so the wait elapses and
    // `abort_reload` runs. The `Online` in the event is the second half of
    // the claim — the instance named is the one still serving, which is what
    // makes the event actionable rather than merely a notice.
    //
    // Two scripts, both used by a correct run: the original and the
    // replacement that never becomes ready. A third would be spawned only by
    // an implementation that carried the reload on past the abandonment.
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

    // fails if a reload that cannot spawn its replacement ends in silence.
    // This is the other way a reload ends badly, and the one that reaches the
    // bus without going through `abort_reload`: no replacement was ever
    // registered, so no `Start`, no `Delete`, nothing at all unless
    // `advance_reload`'s own failure arm says so. A subscriber would see the
    // acceptance and then never hear about that app again.
    //
    // ONE script, and the exhausted pool IS the injected failure rather than
    // something hiding one: `ScriptedRunner` answers `SpawnFailed("script
    // exhausted")` for the replacement, which is `runner.spawn` failing —
    // exactly the arm under test. The assertions rule out the readings an
    // exhausted pool could otherwise be confused with: nothing extra is
    // registered, and the drainee is `Online` rather than `Errored`.
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
        let (_signal_tx, signal_rx) = mpsc::channel(8);
        let (actor_tx, _actor_rx) = mpsc::channel(8);
        let app = normalize(AppConfig::minimal("svc", "./svc")).unwrap();
        tokio::spawn(run_sheep(
            7, proc, io, app, ctl_rx, signal_rx, events, actor_tx,
        ));

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

    /// Fails if [`SheepSlot::to_child`] outlives the process it was cloned
    /// for — delete the clearing line in `handle_exited` and this case is the
    /// one that reddens.
    ///
    /// The far end of that sender is a writer task parked on `recv()`
    /// (`tokio_runner`'s `spawn_channel_pumps`, and the scripted fake's relay
    /// task has the same shape). Its only other way out is a write that
    /// fails, which needs traffic — and a stopped sheep produces none — so
    /// every sender being dropped is the only thing that can retire it. One
    /// left on a slot pins the task and the daemon's half of the socketpair
    /// for as long as the entry lives.
    ///
    /// What is asserted is the TASK ending and not the field being clear: the
    /// relay drops its own sender when it returns, so `to_child_rx` resolving
    /// `None` is that return observed from outside. A relay still parked
    /// keeps that sender, and the read below never resolves — which is the
    /// distinction the case exists to draw, since a slot whose entry is
    /// `Stopped` looks identical either way from the outside.
    #[tokio::test(start_paused = true)]
    async fn a_writer_task_is_reaped_when_its_sheep_exits() {
        let dir = tempfile::tempdir().unwrap();
        // `autorestart` off so the exit is terminal and the slot STAYS
        // registered: a respawn would hand out a second channel, and a
        // deregistration would drop the slot's clone as a side effect of
        // removing the row, which is not the clearing under test.
        let mut app = AppConfig::minimal("web", "./srv");
        app.autorestart = false;
        // Asked for explicitly: the writer task under test only exists for a
        // sheep that has a channel, and the runner wires one only when the
        // spec says so. Leaving it off gives this case nothing to reap.
        app.channel = true;
        // One script for one spawn, exiting on its own rather than under a
        // kill: a `Kill` can put a `Shutdown` on this very channel, and the
        // read below wants the channel's END, not its traffic.
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
        // stop, and `Exit` is the event that announces a pending restart.
        await_event(&mut rx, 0, ProcessEventKind::Stop).await;
        assert_eq!(
            handle.list().await.len(),
            1,
            "sanity: the sheep is still registered, so nothing but the \
             clearing can have let go of the slot's clone"
        );

        // Bounded rather than a bare `await`: the relay ends on its own
        // task's schedule, after the event this woke on, and a leak has to
        // fail the case instead of hanging it.
        let reaped = tokio::time::timeout(Duration::from_secs(5), io.to_child_rx.recv()).await;
        assert_eq!(
            reaped.ok(),
            Some(None),
            "the writer task outlived its sheep, parked on `recv()` and \
             holding the daemon's end of the shepherd channel"
        );
    }

    /// Fails if a spawn failure goes back to naming neither the sheep nor
    /// the path.
    ///
    /// The whole error an operator got was `error[spawn_failed]: the daemon
    /// reported SpawnFailed: process spawn failed: No such file or directory
    /// (os error 2)`. Starting an eleven-app Flockfile, that named neither
    /// which app had failed nor which path had been tried.
    ///
    /// An exact string rather than two `contains`: the message is the whole
    /// product here, and its shape is what a reader of a red run has to be
    /// able to compare against.
    ///
    /// The `cwd` half is left to the end-to-end tier, which has a real one.
    /// `AppConfig::minimal` sets none, and inventing one for this case would
    /// pin a branch against a fixture rather than against a spawn.
    #[tokio::test(start_paused = true)]
    async fn a_failed_spawn_names_the_sheep_and_the_path_it_tried() {
        let dir = tempfile::tempdir().unwrap();
        // An EMPTY script pool, so `ScriptedRunner` refuses the first spawn
        // it is asked for. What matters is that the refusal reaches the
        // reply unchanged apart from the two things being added to it.
        let (mut actor, _mailbox) = actor_with_one_online_sheep(&dir, Vec::new());

        let (reply, answer) = oneshot::channel();
        actor.handle_command(Command::Start {
            apps: vec![normalize(AppConfig::minimal("api", "./api")).unwrap()],
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

    /// Fails if `config_drift` stops naming an edited field, starts naming a
    /// field nobody edited, starts reporting an app the flock has never
    /// heard of, or lets a VALUE out.
    ///
    /// The defect in miniature: an operator edits `cwd` in a Flockfile and
    /// re-runs `shep start`, which adds instances rather than reconciling,
    /// so the edit is not applied. It was also not reported, and that is
    /// what this pins. The last assertion is the other half of the contract:
    /// asking must not APPLY the edit either, or the report would be a
    /// silent reconciliation with a message attached.
    #[tokio::test(start_paused = true)]
    async fn config_drift_names_an_edited_sheeps_fields_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (actor, _mailbox) = actor_with_one_online_sheep(&dir, vec![ProcScript::never_exits()]);

        // The fixture registered `AppConfig::minimal("web", "./srv")`. Two
        // fields edited, not one, so a comparator that stopped at the first
        // difference fails here; and `env` is one of them because reporting
        // it by NAME alone is the security half of this (IR-41).
        let mut edited = AppConfig::minimal("web", "./srv");
        edited.cwd = Some("/srv/new".to_string());
        edited
            .env
            .insert("DATABASE_URL".to_string(), "postgres://hunter2".to_string());
        // A name the flock does not have. `start` will register it, so there
        // is nothing to warn about and it must not appear in the answer.
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

    /// Fails if any of the three spawns stops putting its
    /// [`ProcIo::to_child`] clone on the slot — the handle the actor reaches a
    /// live child through, and the thing the case above would pass vacuously
    /// without, since a channel nobody cloned closes whether or not anything
    /// clears it.
    ///
    /// All three are walked rather than one taken as representative. There is
    /// nothing yet that READS the field, so a spawn that quietly stopped
    /// taking its clone changes no behaviour any other case can see — it
    /// would simply become an instance the actor cannot reach, discovered
    /// whenever something first tries to.
    ///
    /// Driven against the actor directly because the field is private and
    /// deliberately never on the wire. The exit is asserted here too, at the
    /// field, where the case above asserts it at the task: one says the
    /// handle is let go of, the other says letting go of it is enough.
    #[tokio::test(start_paused = true)]
    async fn every_spawn_leaves_the_daemons_end_of_the_channel_on_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        // Three scripts for the three spawns below, none of which exits: this
        // case tells the actor about the one exit it handles, and a proc that
        // went on its own would put a second, unasked-for `Msg::Exited` in
        // play.
        let (mut actor, _mailbox) = actor_with_one_online_sheep(
            &dir,
            vec![
                ProcScript::never_exits(),
                ProcScript::never_exits(),
                ProcScript::never_exits(),
            ],
        );

        // `spawn_fresh`, the door every sheep in the flock arrives by. The
        // fixture's own hand-built sheep is left alone for the reload below;
        // this registers id 1 next to it. `autorestart` off so the exit is a
        // clean stop and the entry stays registered to be read.
        let mut app = AppConfig::minimal("api", "./api");
        app.autorestart = false;
        let (reply, _answer) = oneshot::channel();
        actor.handle_command(Command::Start {
            apps: vec![normalize(app).unwrap()],
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
        // through. A new process under the same id needs a new handle, and a
        // respawn that skipped this would leave the id permanently
        // unreachable after its first exit.
        actor.respawn(1, false);
        assert!(
            actor.sheep[&1].to_child.is_some(),
            "a respawn hands the slot the new process's channel, not the dead \
             one's"
        );

        // `spawn_replacement`, the reload's door. The replacement takes a new
        // id in the drainee's instance slot, so it needs a handle of its own
        // — the drainee's says nothing about the process now serving.
        actor.advance_reload("web", VecDeque::from([0]));
        let new_id = actor.reloads["web"].swap.new_id;
        assert!(
            actor.sheep[&new_id].to_child.is_some(),
            "a reload's replacement holds the daemon's end of its own channel"
        );
    }

    // --- Custom actions: one action out, one answer back or none ---

    /// What an action gets to answer in. Virtual time, so a case that reaches
    /// it costs nothing; long enough that no scheduling order inside a case
    /// can reach it by accident, which matters because every "the app
    /// answered" assertion below would read as a timeout if it did.
    const ACTION_TIMEOUT: Duration = Duration::from_secs(20);

    /// A window generous enough for any action wait to report home, so a case
    /// whose result never arrives fails instead of parking the suite.
    const ACTION_WINDOW: Duration = Duration::from_secs(120);

    /// A bare actor holding one sheep whose shepherd channel is open, plus
    /// the two ends an action travels over: the mailbox every spawned wait
    /// reports to, and the child's end of the channel.
    ///
    /// Direct, like `actor_with_one_online_sheep` (which it builds on), and
    /// for one reason the reload cases do not have: driving the actor by hand
    /// is what lets a case put a reply on the channel at an exact point
    /// relative to a wait's deadline. Through `SupervisorHandle` the two are
    /// only orderable when they are far apart, and the cases that matter most
    /// here are the ones where they are not.
    fn actor_with_an_open_channel(
        dir: &tempfile::TempDir,
    ) -> (
        Actor<ScriptedRunner>,
        mpsc::Receiver<Msg>,
        mpsc::Receiver<ShepherdMessage>,
    ) {
        // No scripts: nothing in these cases spawns, so an empty list turns a
        // spawn that should not have happened into a loud failure.
        let (mut actor, mailbox) = actor_with_one_online_sheep(dir, vec![]);
        // Wide enough that no case can fill it, so a `send` that blocks means
        // a bug rather than a fixture too small for the traffic.
        let (to_child, child_rx) = mpsc::channel(16);
        actor
            .sheep
            .get_mut(&0)
            .expect("the fixture registers id 0")
            .to_child = Some(to_child);
        (actor, mailbox, child_rx)
    }

    /// Puts one action on the fixture's sheep and hands back the receiver its
    /// answer will arrive on.
    ///
    /// Arms the wait directly rather than going through `Command::Trigger`,
    /// because what these cases are about is one sheep's wait — a selector
    /// pass in front of it would only decide, again, the thing the fixture
    /// already decided by building the slot the way it did.
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

    /// Fails if a reply stops reaching the wait that asked for it — the whole
    /// path, from `SupervisorHandle::trigger` through the actor, the writer's
    /// end of the channel, `run_sheep`'s relay of a `ChildMessage` and back.
    ///
    /// Driven through the handle rather than the actor because it is the one
    /// case here whose subject IS the wiring: a `run_sheep` arm that dropped
    /// an `action-reply` on the floor again would leave every direct case
    /// below green.
    ///
    /// `params` are asserted on the wire, not just the name. They are passed
    /// to the app verbatim and read nowhere in the daemon, so nothing else
    /// would notice them being dropped between the command and the channel.
    #[tokio::test(start_paused = true)]
    async fn a_triggered_action_answers_with_the_apps_reply() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.channel = true;
        let (handle, runner, _events) = started(&dir, app, vec![ProcScript::never_exits()]).await;
        let mut io = runner.io_handles(0);

        // Spawned rather than awaited: the reply below is what ends this
        // wait, and it cannot be sent from a task already parked on it.
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
                // `next_action_stamp` starts at 0 and is read before it is
                // incremented, so a freshly-built actor's first dispatch —
                // this one — is always stamp 0. Written deliberately, not
                // discovered: this literal is the proof the daemon stamps at
                // all.
                id: 0,
            },
            "the action reaches the child's end of the channel as it was asked for"
        );

        io.from_child_tx
            .send(ChildMessage::ActionReply {
                action: "gc".to_string(),
                body: "swept 3".to_string(),
                // Echoing the dispatch's own stamp (0, asserted above) makes
                // this a real stamped round trip through the actor, not two
                // literals that happen to agree.
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

    /// fails if a `ready` on fd 3 reaches only the readiness machinery and never
    /// the bus. Readiness already had a consumer, which is exactly why it is the
    /// case worth pinning: forwarding must be a SECOND thing the arm does, not a
    /// replacement for the first.
    #[tokio::test(start_paused = true)]
    async fn a_ready_on_the_channel_reaches_both_the_bus_and_the_readiness_wait() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.channel = true;
        app.wait_ready = true;
        // `started` hands back the bus receiver as its third element — subscribed
        // BEFORE the start, so nothing this case cares about is missed.
        let (handle, runner, mut events) =
            started(&dir, app, vec![ProcScript::never_exits()]).await;
        let io = runner.io_handles(0);

        io.from_child_tx.send(ChildMessage::Ready).await.unwrap();

        // Bounded (IR-46): a bus that never receives would park this case rather
        // than fail it, and there is no other failure mode to give it.
        let seen = tokio::time::timeout(ACTION_WINDOW, async {
            loop {
                if let BusEvent::Channel { id, message } = events.recv().await.unwrap() {
                    break (id, message);
                }
            }
        })
        .await
        .expect("no channel event within the window");

        assert_eq!(seen, (0, ChildMessage::Ready));

        // The readiness half still works: the sheep goes Online off this same
        // message, which is what it did before the bus ever saw one.
        let listed = handle.list().await;
        assert_eq!(listed[0].status, ProcStatus::Online);
    }

    /// fails if a metric is still only a `tracing::debug!`. That log line was the
    /// whole of what a metric ever produced, and a subscriber could not read it —
    /// this is the case that says the topic exists for a reason.
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
                if let BusEvent::Channel { id, message } = events.recv().await.unwrap() {
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

    /// fails if an `action-reply` nobody is waiting for is dropped before the bus
    /// sees it. This is the case `deferred.md`'s `channel.*` entry names by name:
    /// an unprompted or late reply "stays just as invisible as before". No trigger
    /// is armed here, so `handle_action_reply` finds no wait and discards it — and
    /// the bus must still have carried it.
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
                if let BusEvent::Channel { message, .. } = events.recv().await.unwrap() {
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

    /// Fails if a wait for an app that never answers does not end on its own
    /// — the failure that makes a `PendingReply`-shaped trigger unusable,
    /// since nothing about a custom action guarantees a message ever comes
    /// back to resolve it.
    ///
    /// The action is read off the channel AFTER the answer, which is the
    /// half that makes the timeout mean anything: a build that never sent the
    /// action at all would time out too, and look identical here without it.
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
                // Same actor, same reasoning as the previous test: its first
                // and only dispatch carries stamp 0.
                id: 0,
            },
            "the timeout is an app that did not answer, not an action that was never sent"
        );
    }

    /// fails if a stamped reply is consumed as a debt payment instead of waking
    /// the live wait it names. This is wire.md #2, in one function: T1 times out
    /// and leaves a `gc` debt, T2 is triggered and is live, and the app's next
    /// `gc` reply — carrying T2's stamp — must reach T2.
    ///
    /// The alert that must not be missed: before this task, `answer` returned
    /// `None` here and the operator was told `timed_out` about a request the app
    /// had answered promptly and correctly.
    #[test]
    fn a_stamped_reply_wakes_its_own_wait_even_with_a_debt_outstanding() {
        let mut waits = ActionWaits::default();

        // T1: armed, then resolved without its reply — the timeout path.
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

    /// fails if an UNSTAMPED reply stops behaving the way it does today. An app
    /// that does not echo the stamp — every app written before this task — must
    /// see byte-identical behaviour: the debt is paid first, the live wait is
    /// left alone. Changing this is the regression a stamped path is most likely
    /// to cause.
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

    /// fails if a reply stamped for a wait that has ALREADY given up leaks into a
    /// live wait of the same name. The stamped path has to settle its own debt,
    /// not just skip the queue.
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

    /// Fails if a reply owed by a wait that already timed out is allowed to
    /// answer a later wait for the same action.
    ///
    /// This is the one failure that produces a WRONG answer rather than an
    /// error: an app's reply names the action and nothing else, so a `gc`
    /// reply written after the first `gc` gave up is byte-identical to one
    /// written for the second. Delete the `abandoned` bookkeeping in
    /// `ActionWaits::resolve` and the second trigger is answered `Replied`
    /// with the first trigger's body.
    ///
    /// The last two steps are not decoration. Without them the case would
    /// also pass on a build that swallowed every reply forever, which is the
    /// opposite bug and just as wrong: the debt has to be settled by one
    /// reply, not by all of them.
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
        // The app finally answers the FIRST `gc`, having no way to say so.
        actor.handle_action_reply(0, "gc", "swept 3".to_string(), None);
        assert_eq!(
            settle_action(&mut actor, &mut mailbox).await,
            ActionOutcome::TimedOut,
            "a reply the first `gc` was owed was handed to the second one"
        );
        assert_eq!(second.await.unwrap(), ActionOutcome::TimedOut);

        // One reply per debt, and no more. Two `gc` waits have now given up,
        // and the reply above settled the first of them — so exactly one
        // reply stands between a third trigger and an answer. Sending both
        // here is what separates a debt that is settled from one that is
        // permanent: a build that swallowed every `gc` reply for the life of
        // the process would leave the sheep unanswerable, and would pass
        // every assertion above.
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

    /// Fails if a second reply to an action that has already been answered is
    /// kept for anything.
    ///
    /// An app is free to write two replies to one action, and the daemon asked
    /// for one. The second is not an error and not a debt — nothing is owed,
    /// because the wait that asked got its answer — so the only correct thing
    /// to do with it is nothing at all.
    ///
    /// Proved by what happens NEXT rather than by the second reply itself,
    /// which produces no observable effect on its own: a wait armed after it
    /// must still time out. A build that parked the spare reply somewhere
    /// would answer that wait with `spent` instead.
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

    /// Fails if two waits for the SAME action on one sheep are not answered
    /// in the order they were asked — most sharply, if the second reply is
    /// handed back to the wait the first one already answered and the second
    /// wait is left to time out with its answer discarded.
    ///
    /// Two triggers of one action can be in flight at once because two
    /// operators can each run one, and neither reply says which is which.
    /// Order is the only thing that does, and it is a property of the channel
    /// rather than of anything the daemon records.
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

    /// Fails if the deadline stops covering the DELIVERY of an action and
    /// only covers the reply to it.
    ///
    /// A child that has stopped reading fd 3 backs its socket up, the writer
    /// task stops draining the daemon's end, and a send onto a full channel
    /// then waits for room that is not coming. Outside the deadline that
    /// parks the wait's task and its caller for good — the same permanent
    /// wait a custom action's whole timeout exists to rule out, one step
    /// earlier than the case that names it.
    ///
    /// The channel here is deliberately built full and left unread, which is
    /// that wedged child expressed at the one seam a paused-clock case can
    /// reach: a real child holding its socket unread is not reproducible
    /// without a real child.
    #[tokio::test(start_paused = true)]
    async fn an_action_that_cannot_even_be_delivered_still_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox) = actor_with_one_online_sheep(&dir, vec![]);

        // Held, never read: dropping it would make the send fail outright,
        // which is the OTHER outcome and would pass this case vacuously.
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
    /// its shepherd-channel sender, and hands back its id.
    ///
    /// Direct, like the fixtures it extends, because a flock whose sheep
    /// differ in exactly one respect — whether the channel is there, whether
    /// anything is reading it — is not reachable through `start`: the fake
    /// runner wires a live channel for every spawn regardless of what the
    /// app's `channel` says.
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
    /// never comes.
    ///
    /// Bounded for the reason every read of an action's answer here is: a
    /// request that armed a wait nothing will resolve never answers at all,
    /// so an unbounded read would park the suite where this reddens it.
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

    /// Fails if a trigger answers before every sheep it matched has been
    /// heard from, if it drops any of them, or if the rows come back in
    /// whatever order they settled in.
    ///
    /// The three sheep are the three shapes one selector routinely reaches at
    /// once: one that answers, one with nothing to answer over, and one that
    /// says nothing at all. The one that answers is asked FIRST and settles
    /// FIRST, so a build that replied as soon as it had an answer would pass
    /// every row assertion below — which is what the `try_recv` between the
    /// two settlements is for.
    ///
    /// The id order and the order the rows are produced in genuinely differ,
    /// so the final sort is load-bearing rather than incidental: the refused
    /// sheep is collected before either wait is even armed, and the two waits
    /// settle in the order the app and the clock decide. Delete the sort and
    /// the rows come back `[1, 0, 2]`.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_answers_every_sheep_it_matched_before_it_answers_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);
        register_sheep(&mut actor, &dir, "api", None);
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
        // Yielded first, and the assertion is worth nothing without it: the
        // send that would answer early happens in the task collecting the
        // rows, and `settle_action` returns without that task having been
        // polled — so an unyielded `try_recv` reads `Empty` on a build that
        // answers after every single row.
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
                row(1, "api", ActionOutcome::NoChannel),
                row(2, "worker", ActionOutcome::TimedOut),
            ])
        );
    }

    /// Fails if a sheep with no live channel is waited out instead of refused
    /// on the spot, or if that refusal is allowed to take the rest of the
    /// selector's matches with it.
    ///
    /// The refused sheep is asked alongside one that answers, because the
    /// mixed flock is the case the per-row shape exists for: a whole-request
    /// refusal would deny an operator the answer the reachable sheep gave.
    ///
    /// Refusing takes no wait at all — there is nothing to deliver to and no
    /// reply to expect — and the mailbox carrying exactly one result is what
    /// says so. A refusal that armed a wait anyway would leave one nothing
    /// ever resolves.
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
                row(
                    0,
                    "web",
                    ActionOutcome::Replied {
                        body: "swept 3".to_string()
                    }
                ),
                row(1, "api", ActionOutcome::NoChannel),
            ])
        );
        assert!(
            matches!(mailbox.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "a sheep with no channel armed a wait anyway"
        );
    }

    /// Fails if "can this sheep be triggered" is answered off the presence of
    /// a sender rather than off whether anything is still receiving on it.
    ///
    /// The two differ, and the difference is the whole of an app configured
    /// without a channel: the runner drops the receiving end at spawn rather
    /// than leaving it dangling, so its slot holds a sender whose far end is
    /// already gone. Read as "there is a sender, so deliver", that app's
    /// every action costs a full timeout to answer the same `NoChannel` this
    /// gives at once.
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

    /// Fails if a flock where NOTHING can be reached is answered as though
    /// the action had been delivered.
    ///
    /// It is a success — every match was found and every match was told about
    /// — and the rows are what stop it being a silent one: an operator reads
    /// `no channel` against every sheep rather than an empty-looking `Ok`.
    /// That is the whole difference from a reload that finds nothing to
    /// replace, whose reply is a flock listing indistinguishable from one
    /// that swapped every instance.
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

    /// Fails if a reload drainee is sent the action rather than skipped.
    ///
    /// Both halves of a swap answer to the app's name, and the drainee still
    /// holds a live channel of its own — so without the skip an operator
    /// asking `web` gets two rows for one instance, one of them from the
    /// process being replaced. Deleting the skip does not merely add a row:
    /// a drainee mid-kill-ladder has stopped reading its channel, so the row
    /// it adds costs a whole timeout to say nothing.
    ///
    /// Built by hand rather than through a real swap because what decides the
    /// skip is `ProcessEntry::reload`, which is crate-internal and never on
    /// the wire — the same reason `actor_with_one_online_sheep` exists.
    #[tokio::test(start_paused = true)]
    async fn a_reload_drainee_is_skipped_and_its_replacement_answers() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);
        let (replacement_tx, mut replacement_rx) = mpsc::channel(16);
        let new_id = register_sheep(&mut actor, &dir, "web", Some(replacement_tx));
        let drainee = actor.sheep.get_mut(&0).expect("the fixture registers id 0");
        drainee.entry.status = ProcStatus::Stopping;
        drainee.entry.reload = ReloadState::Drainee { new_id };
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
    /// an error. Every [`ActionOutcome`] is a statement about a sheep, and
    /// there is no sheep here to make one about — which is also what every
    /// other selector-in verb answers, so a trigger differing would be a
    /// second grammar for the same miss.
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

    /// Fails if a wait armed against a process that then exits is left for
    /// its own deadline to end, or dropped without an answer.
    ///
    /// Dropping it is the subtler half: the caller's receiver would resolve
    /// `Err`, which `SupervisorHandle::trigger` reports as the engine having
    /// gone away — a claim about the daemon, made because a single child
    /// exited.
    ///
    /// The debts go with the waits, and for a reason the exit itself does not
    /// make obvious: a replacement under this id has written none of the
    /// replies the dead process owed, so a debt left behind would have it
    /// swallow the first answer it gives.
    #[tokio::test(start_paused = true)]
    async fn a_sheep_exiting_answers_every_action_waiting_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut actor, mut mailbox, mut child_rx) = actor_with_an_open_channel(&dir);

        // One wait that will be left waiting, and one debt for a wait that
        // already gave up — the two halves the exit has to clear.
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
        // Bounded for the same reason the no-channel case above is: a wait
        // the exit failed to answer is a wait nothing else will, so an
        // unbounded read would park the suite instead of reddening here.
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

    // --- Signal: `shep signal`, one selector in, one row per matched sheep
    // out ---

    /// fails if `signal` reaches the group instead of the process, or reaches
    /// nothing. The group assertion is the load-bearing half — a supervisor
    /// that called `signal` rather than `signal_process` would look correct
    /// in every other respect and would deliver SIGHUP to every lamb.
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

    /// fails if a registered-but-dead sheep is reported as delivered. `Delivered`
    /// is the only outcome that claims the kernel took the signal, so a stopped
    /// sheep answering it would be the report lying about the one thing it says.
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

    /// fails if a selector matching nothing is answered with an empty success. It
    /// is `NotFound`, exactly as it is for every other selector-taking verb —
    /// `shep signal typo SIGHUP` exiting 0 would be the worst possible answer.
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

    /// fails if a reload drainee is skipped. `begin_action` skips one, because an
    /// action expects a reply from a process on its way out; a signal expects
    /// nothing back, and the drainee is a live process the operator's selector
    /// matched. Holding it back would be a silent refusal with no channel in which
    /// to explain itself.
    ///
    /// Actor-tier, and it has to be: a `ReloadState::Drainee` marker lives on
    /// `ProcessEntry::reload`, which is crate-internal and deliberately never on
    /// the wire, so there is no way to put one sheep in that state through the
    /// handle without driving a whole swap and losing the ability to say which
    /// half the signal reached.
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

        // The request really left the actor for the sheep task's mailbox — the
        // half that proves the drainee was not filtered out before the fan-out.
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
    /// it was handed, so this asserts the line and not merely that something
    /// happened.
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
    /// `no_stdin` — and especially if it is answered `Sent`, which would claim
    /// a line landed somewhere that has no pipe at all.
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
    /// pipe is the normal case under `all`, and a refusal that took the
    /// reachable half with it would leave the operator unable to tell which
    /// half was taken — the same rule `Reopen`, `Flush`, `Trigger` and
    /// `Signal` all follow.
    #[tokio::test(start_paused = true)]
    async fn a_mixed_flock_reports_per_sheep_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let mut piped = AppConfig::minimal("repl", "./repl");
        piped.stdin = true;
        // `started` starts one app; the second goes on through the handle,
        // which is what makes the two spawn indices 0 and 1 and the two ids
        // 0 and 1.
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
        // id-sorted, like every other row-shaped reply, so the answer's order
        // is the selector's and not the scheduler's.
        assert!(rows.windows(2).all(|w| w[0].id < w[1].id));
    }

    /// fails if a wait on an app that never reads its stdin has no bound
    /// (IR-46). This is the case that can only fail by hanging, so it is the
    /// one that has to carry an explicit deadline — and the outcome has to
    /// name the bound, because "the app is not reading" and "the pipe broke"
    /// have different fixes.
    #[tokio::test(start_paused = true)]
    async fn a_write_that_never_lands_times_out_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppConfig::minimal("stuck", "./stuck");
        app.stdin = true;
        // `never_reads_its_stdin` accepts the write and answers nothing —
        // exactly what a full pipe looks like from this side.
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

    /// fails if a flock of wedged sheep costs STDIN_WRITE_TIMEOUT **each**.
    ///
    /// This is the case the constant's own doc rests on: two seconds is
    /// chosen to sit under the 5s an RPC caller gets by default, and that
    /// argument is only true if the waits run CONCURRENTLY. Awaited in a
    /// `for` loop — which is what `spawn_trigger_task` does, and what this
    /// task was first written to copy — three wedged sheep cost six seconds,
    /// the caller's budget expires first, and `shep whisper all` answers
    /// `DeadlineExceeded` instead of the three honest `not_written` rows the
    /// outcome enum exists to deliver.
    ///
    /// Asserted against `Instant`, not against the row contents: the rows are
    /// the same either way, and the elapsed time is the whole difference.
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
        // Under the paused clock the auto-advance is exact, so this is a real
        // ceiling rather than a flaky one: sequential waits would read 6s.
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

    // --- the log plane mid-reload ------------------------------------
    //
    // A swap puts two entries in ONE instance slot, and `assemble` derives a
    // sheep's log paths from its name and its instance — so the drainee and
    // its replacement derive byte-identical paths and every app is a
    // shared-log-path app for as long as the swap lasts. What `merge_logs`
    // made reachable by configuration is reachable by running a verb, which
    // is why both log-plane verbs are pinned against this shape and not only
    // against `merge_logs`.
    //
    // Both cases below name the REPLACEMENT by id, the one selector form that
    // cannot reach the drainee by matching: `all` and `web` both name it
    // outright, and would pass against an implementation with no notion of a
    // shared path at all.

    /// Fails if the set of pumps a flush drains is narrowed back to the sheep
    /// the selector matched, which would leave a reload's drainee appending to
    /// a file being emptied under it.
    ///
    /// [`a_sibling_sharing_a_path_is_flushed_even_when_the_selector_skips_it`]
    /// proves the widening for two apps handed one explicit `out_file`. This
    /// is the same mechanism reached the way an operator meets it without
    /// having configured anything, and the two are worth keeping apart: that
    /// one would still pass if the shared-path case were narrowed to
    /// configurations that name a path outright.
    ///
    /// The equal paths are asserted rather than assumed. A swap that had
    /// quietly started taking a fresh instance slot would leave this case
    /// testing two unrelated sheep, where a narrowed pump set is invisible.
    #[tokio::test(start_paused = true)]
    async fn a_flush_naming_a_replacement_still_drains_the_drainee_sharing_its_path() {
        let dir = tempfile::tempdir().unwrap();
        // Two scripts, counted: the original spawn, and the one replacement a
        // reload of a one-instance app performs. A third would be answered
        // with `SpawnFailed("script exhausted")`, which abandons the reload
        // and leaves a single entry standing — no overlap at all, and nothing
        // here to see.
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
    /// drainee holding the inode an external rotator has just renamed.
    ///
    /// The consequence is the one `reopen` exists to prevent, and it is
    /// silent: the drainee goes on appending to the archive, which keeps
    /// growing after the rotation that was supposed to close it, while the
    /// recreated path takes only the replacement's lines. A `postrotate`
    /// stanza that waited for a zero exit was told the opposite of what
    /// happened.
    ///
    /// The counts are the whole case. The reply carries the named sheep
    /// either way, so an implementation that resolved the selector and pushed
    /// at one pump passes every other assertion here — see
    /// [`a_reopen_reaches_every_matched_sheep_and_no_others`], which pins that
    /// the reach does not go WIDER than the paths a selector reached.
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
            stdin: false,
            credentials: None,
        }
    }

    // ---------------------------------------------------------------
    // IR-37: supervisor proptest. A command script (what
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
        // 128, and the number is measured rather than picked: an injected-bug
        // trial (a Delete on an already-terminal sheep that forgets to
        // deregister) minimizes to the 3-step
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
