//! Whole-flock handover: whether this daemon's flock can be replaced in
//! place, the [`Handover`] blob that describes it, and the exec that carries
//! it.
//!
//! [`fitness`] is the gate, and it refuses whole. One refusal: a live sheep
//! whose log pump did not report its descriptors in time. A sheep's stdout,
//! stderr, log files, stdin pipe and shepherd channel all cross the exec, per
//! sheep rather than per app.

pub(crate) mod adopt;
mod fds;
pub(crate) mod reap;
pub(crate) mod uptime;

use core::convert::Infallible;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{DogSource, ExitInfo};
use shep_core::status::ProcStatus;

use crate::entry::{ProcessEntry, ReloadState};
use crate::privilege::SpawnIdentity;
use crate::supervisor::{CarriedReload, PendingManual};

/// Whether a flock can be handed over in place, or must fall back to a
/// stop-and-start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fitness {
    /// Every sheep in the flock is carryable.
    Carryable,
    /// At least one sheep is not carryable, and why.
    Refused(RefusedReason),
}

/// Why a flock cannot be handed over in place. The caller falls back to the
/// stop arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefusedReason {
    /// The sheep's log pump did not report its descriptors before the
    /// snapshot's deadline, so nothing knows which descriptors it holds.
    PumpUnresponsive {
        /// The sheep's name.
        sheep: String,
    },
}

impl core::fmt::Display for RefusedReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Load-bearing text: `cli_e2e` probes for "falls back to a
        // stop-and-start" to tell a carried flock from a stopped one.
        match self {
            Self::PumpUnresponsive { sheep } => write!(
                f,
                "sheep '{sheep}' has a log pump that did not report its descriptors in \
                 time; reload falls back to a stop-and-start instead"
            ),
        }
    }
}

/// One sheep's carryability-relevant facts: a [`ProcessEntry`] plus the one
/// fact that does not live on it.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The sheep's lifecycle entry.
    pub entry: &'a ProcessEntry,
    /// Whether this sheep's log pump was asked for its descriptors and did
    /// not answer in time.
    ///
    /// Distinct from [`CarriedFds::none`], which is what a stopped sheep
    /// reports: a wedged live pump collapsed into it is carried with its
    /// descriptors silently dropped.
    pub pump_unresponsive: bool,
}

/// A [`Candidate`] that owns its entry.
///
/// Snapshot assembly awaits every log pump, which may not happen on the actor
/// loop, so it runs on a task of its own and the entries travel there.
#[derive(Debug, Clone)]
pub struct OwnedCandidate {
    /// The sheep's lifecycle entry, cloned off the supervisor's slot.
    pub entry: ProcessEntry,
    /// Whether this sheep's log pump missed the snapshot's deadline; see
    /// [`Candidate::pump_unresponsive`].
    pub pump_unresponsive: bool,
}

impl OwnedCandidate {
    /// Borrow this as the [`Candidate`] [`fitness`] takes.
    #[must_use]
    pub fn as_candidate(&self) -> Candidate<'_> {
        Candidate {
            entry: &self.entry,
            pump_unresponsive: self.pump_unresponsive,
        }
    }
}

/// Decide whether a flock can be handed over in place.
///
/// Whole-flock: the blob describes one process image, so a flock is carried
/// whole or refused whole. An empty flock is carryable.
#[must_use]
pub fn fitness(sheep: &[Candidate<'_>]) -> Fitness {
    for candidate in sheep {
        if let Some(reason) = refusal(candidate) {
            return Fitness::Refused(reason);
        }
    }
    Fitness::Carryable
}

/// Why `candidate` alone refuses the flock, if it does.
fn refusal(candidate: &Candidate<'_>) -> Option<RefusedReason> {
    let entry = candidate.entry;
    let config = entry.spec.config();
    let name = || config.name.clone();

    if candidate.pump_unresponsive {
        return Some(RefusedReason::PumpUnresponsive { sheep: name() });
    }
    None
}

/// The blob format this daemon writes, and the only one it can read.
///
/// [`Handover::load_value`] refuses any other number outright: an image that
/// cannot understand the blob must not adopt a partial picture of a live
/// flock.
pub const VERSION: u32 = 1;

/// The file name the blob is written under, inside `$SHEP_HOME/run`.
const FILE_NAME: &str = "handover.json";

/// Everything the successor needs to keep supervising a flock it did not
/// spawn.
///
/// Written just before the `execve`, read once by the incoming image, and
/// unlinked by that reader. Besides the descriptors it names by number, this
/// is the whole of what crosses. It carries each sheep's environment, so it
/// goes to disk at mode `0600` and `AppConfig`'s `Debug` prints `env` as a
/// count.
///
/// `ProcessEntry::started_at` is absent: a `tokio::time::Instant` has no
/// epoch outside the runtime that read it, so [`uptime::started_at_of`]
/// re-derives each sheep's start time from the operating system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    /// The format this blob was written in; see [`VERSION`].
    version: u32,
    /// Every sheep the successor is to adopt, in no particular order.
    sheep: Vec<CarriedSheep>,
    /// The control listener's descriptor number.
    listener_fd: RawFd,
    /// The pidfile lock's descriptor number.
    ///
    /// `flock` is a property of the open file description, so holding this
    /// descriptor open holds the lock. Re-acquiring it instead would open a
    /// window for a second daemon to win it.
    pidfile_fd: RawFd,
    /// The supervisor's next entry id.
    next_id: u32,
    /// The supervisor's next reload-watchdog stamp.
    next_deadline: u64,
    /// The supervisor's next action-wait stamp.
    next_action_stamp: u64,
    /// Every app whose reload was still in flight at the exec.
    ///
    /// An absent key loads as `None`, which means no reload was in flight.
    /// Sorted by app name by its writer, since the blob is a file an operator
    /// may read.
    reloads: Option<Vec<CarriedReload>>,
}

impl Handover {
    /// Describe a flock for the successor.
    #[must_use]
    pub fn new(
        sheep: Vec<CarriedSheep>,
        fds: DaemonFds,
        counters: Counters,
        reloads: Vec<CarriedReload>,
    ) -> Self {
        Self {
            version: VERSION,
            sheep,
            listener_fd: fds.listener,
            pidfile_fd: fds.pidfile,
            next_id: counters.next_id,
            next_deadline: counters.next_deadline,
            next_action_stamp: counters.next_action_stamp,
            reloads: Some(reloads),
        }
    }

    /// Every sheep this blob carries.
    #[must_use]
    #[allow(dead_code, reason = "read by this crate's own tests")]
    pub fn sheep(&self) -> &[CarriedSheep] {
        &self.sheep
    }

    /// The entry id the successor is to issue next.
    #[must_use]
    #[allow(dead_code, reason = "read by this crate's own tests")]
    pub const fn next_id(&self) -> u32 {
        self.next_id
    }

    /// The three counters the successor restores before installing any sheep.
    #[must_use]
    pub const fn counters(&self) -> Counters {
        Counters {
            next_id: self.next_id,
            next_deadline: self.next_deadline,
            next_action_stamp: self.next_action_stamp,
        }
    }

    /// Every app whose reload was still in flight at the exec.
    ///
    /// Empty both for a flock with nothing mid-reload and for a blob that
    /// carried none.
    #[must_use]
    pub fn reloads(&self) -> &[CarriedReload] {
        self.reloads.as_deref().unwrap_or_default()
    }

    /// Where the blob lives under `paths`: `$SHEP_HOME/run/handover.json`.
    #[must_use]
    pub fn path(paths: &ShepPaths) -> PathBuf {
        paths.run.join(FILE_NAME)
    }

    /// Every descriptor number this blob names, listener and pidfile first.
    ///
    /// The exact set [`hand_over`] clears `FD_CLOEXEC` on. A descriptor kept
    /// without being named leaks into the successor's image.
    fn named_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        [self.listener_fd, self.pidfile_fd].into_iter().chain(
            self.sheep
                .iter()
                .flat_map(|sheep| sheep.fds.all().into_iter().flatten()),
        )
    }

    /// Write the blob under `paths` at mode `0600`, returning where it went.
    ///
    /// The mode is set at creation, since a later `chmod` leaves a
    /// world-readable window. Any leftover blob is removed first, since
    /// [`OpenOptions::mode`](OpenOptionsExt::mode) is honoured only on
    /// create.
    ///
    /// # Errors
    ///
    /// The leftover blob could not be removed, the new one could not be
    /// created, or serializing to it failed.
    pub fn write(&self, paths: &ShepPaths) -> io::Result<PathBuf> {
        let path = Self::path(paths);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        serde_json::to_writer(&file, self).map_err(io::Error::other)?;
        Ok(path)
    }

    /// Read the blob at `path`.
    ///
    /// Does not unlink: the successor does that once it has adopted what the
    /// blob describes, so a failure here leaves the file for an operator.
    ///
    /// # Errors
    ///
    /// The file could not be read, its bytes are not a handover blob, or it
    /// names a format version this image does not implement.
    pub fn read(path: &Path) -> Result<Self, LoadError> {
        let text = fs::read_to_string(path).map_err(LoadError::Io)?;
        let value = serde_json::from_str(&text).map_err(LoadError::Malformed)?;
        Self::load_value(value)
    }

    /// Check `value`'s format version, then deserialize it.
    ///
    /// The version is read off the raw JSON first, so an unknown format is
    /// refused by number rather than by a field that failed to deserialize.
    ///
    /// # Errors
    ///
    /// `value` carries no `version`, carries one other than [`VERSION`], or
    /// is not a handover blob.
    pub fn load_value(value: serde_json::Value) -> Result<Self, LoadError> {
        match value.get("version").and_then(serde_json::Value::as_u64) {
            Some(found) if found == u64::from(VERSION) => {}
            Some(found) => return Err(LoadError::UnsupportedVersion { found }),
            None => return Err(LoadError::MissingVersion),
        }
        serde_json::from_value(value).map_err(LoadError::Malformed)
    }
}

/// Why a handover blob could not be loaded.
///
/// Every variant means the successor must not adopt anything: it has no
/// picture of the flock, or only part of one.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// The blob could not be read off disk.
    Io(io::Error),
    /// The blob names no format version at all, so it is not one this image
    /// wrote.
    MissingVersion,
    /// The blob names a format version this image does not implement.
    UnsupportedVersion {
        /// The version the blob claims.
        found: u64,
    },
    /// The blob names a version this image implements, but its contents do
    /// not deserialize into one.
    Malformed(serde_json::Error),
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "the handover blob could not be read: {err}"),
            Self::MissingVersion => f.write_str("the handover blob names no format version"),
            Self::UnsupportedVersion { found } => write!(
                f,
                "the handover blob is format version {found}, and this shep implements \
                 version {VERSION}"
            ),
            Self::Malformed(err) => write!(f, "the handover blob is not readable: {err}"),
        }
    }
}

impl core::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Malformed(err) => Some(err),
            Self::MissingVersion | Self::UnsupportedVersion { .. } => None,
        }
    }
}

/// The daemon's own two descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonFds {
    /// The control listener's descriptor number.
    pub listener: RawFd,
    /// The pidfile lock's descriptor number.
    pub pidfile: RawFd,
}

/// The three supervisor counters a successor must not reissue.
///
/// They reset to zero in every constructor, so a successor that did not
/// carry them would hand out an entry id, a reload-watchdog stamp or an
/// action stamp a caller still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// The next entry id.
    pub next_id: u32,
    /// The next reload-watchdog stamp.
    pub next_deadline: u64,
    /// The next action-wait stamp.
    pub next_action_stamp: u64,
}

/// One sheep, as the successor will find it.
///
/// [`Self::app`] is what the sheep is, carried whole so the successor can
/// respawn this exact instance without asking the muster roll. Every other
/// field is what this instance is currently doing.
///
/// An absent `Option` key loads as `None`, so a blob an older image wrote
/// still loads and [`VERSION`] stays unmoved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedSheep {
    /// The supervisor's entry id, which callers already hold and selectors
    /// already name.
    id: u32,
    /// The sheep's name, which is how an operator names it back.
    name: String,
    /// The instance slot within its app.
    instance: u32,
    /// The running process, or `None` for an instance that is registered and
    /// not running.
    pid: Option<u32>,
    /// Respawns performed so far, which the restart budget counts against.
    restarts: u32,
    /// The supervisor slot's respawn epoch, so a timer armed before the exec
    /// is still recognised as stale afterwards.
    epoch: u64,
    /// The instance's lifecycle status.
    status: ProcStatus,
    /// How this instance most recently stopped existing, if it has.
    last_exit: Option<ExitInfo>,
    /// The identity this instance's next spawn runs under.
    ///
    /// Carried resolved, never re-derived: the value is pinned at the first
    /// spawn, so a later change to the passwd database cannot move a running
    /// app's identity underneath it.
    credentials: SpawnIdentity,
    /// The descriptor numbers this instance's output travels on.
    fds: CarriedFds,
    /// Whether an operator's `delete` targeted this instance before the
    /// exec. An absent key means "no".
    pending_delete: Option<bool>,
    /// The manual command that owned this instance's next exit before the
    /// exec, and who asked for it. `None` is already this field's own word
    /// for "no command owns this exit", so an absent key needs no second
    /// state.
    manual: Option<PendingManual>,
    /// Which half of a reload's swap this instance is, if either. An absent
    /// key means [`ReloadState::None`].
    ///
    /// It routes this instance's next exit: without it a drainee's exit goes
    /// to `decide_on_exit`, and an `autorestart` app respawns the old code
    /// into the replacement's slot.
    reload: Option<ReloadState>,
    /// Whether a reload's readiness verification has already failed against
    /// this instance. An absent key reads as `false`.
    ///
    /// It keeps a failed reload's leftovers reachable: an abandoned instance
    /// is left `Starting`, and `reload_eligible` reads this flag beside the
    /// status so a rollback can still reach it.
    ready_failed: Option<bool>,
    /// When this instance's owed respawn falls due, in wall-clock terms, or
    /// `None` for an instance that is not owed one at all.
    ///
    /// Wall-clock and not monotonic: a [`tokio::time::Instant`] has no epoch
    /// across the `execve`, while an absolute moment lets the successor re-arm
    /// for what is left. A clock that moves under it is clamped by
    /// [`adopted_restart_delay`](crate::backoff::adopted_restart_delay). An
    /// absent key re-arms the whole delay, and only a `WaitingRestart` row
    /// carries one.
    restart_due: Option<SystemTime>,
    /// Where this instance's binary came from, for an instance that is a
    /// dog, or `None` for an ordinary sheep.
    ///
    /// An absent key means "not a dog". The whole [`DogSource`] and not a
    /// boolean, because `shep dogs` reports where each dog's binary came from.
    ///
    /// Losing the marker is invisible to a pid check: `Actor::matching_ids`,
    /// `dogs::spawn_dog_watch` and `rpc::dog_staleness` all read it.
    dog: Option<DogSource>,
    /// The config a load parked for this instance's next spawn, or `None`
    /// for an instance nothing is owed.
    ///
    /// An `AppConfig` and not a `ResolvedApp` for the reason [`Self::app`]
    /// gives. Losing it is silent config erasure: the parked change vanishes
    /// and the next load compares against a spec that already matched.
    pending: Option<AppConfig>,
    /// Whether promoting [`Self::pending`] must re-resolve
    /// [`Self::credentials`].
    ///
    /// `None` reads as `false`, and covers both an absent key and a sheep with
    /// nothing parked.
    ///
    /// Carried with [`Self::pending`] and never without it: the load that
    /// parks the config decides this, and a later diff cannot recompute it.
    pending_reidentifies: Option<bool>,
    /// The resolved config this instance runs under, environment included.
    ///
    /// The `AppConfig` beneath [`ProcessEntry::spec`]'s `ResolvedApp`, not the
    /// `ResolvedApp` itself: that type is a proof token minted only by
    /// `normalize`, so the successor rebuilds it by normalizing again. A
    /// successor whose `normalize` has tightened refuses a config its
    /// predecessor accepted, with no stop arm left.
    app: AppConfig,
}

impl CarriedSheep {
    /// Describe `entry` for the successor.
    ///
    /// The arguments beyond `entry` do not live on it. `restart_due` is the
    /// one not carried verbatim: it is gated on the entry's status.
    #[must_use]
    pub fn from_entry(
        entry: &ProcessEntry,
        epoch: u64,
        fds: CarriedFds,
        pending_delete: bool,
        manual: Option<PendingManual>,
        ready_failed: bool,
        restart_due: Option<SystemTime>,
    ) -> Self {
        Self {
            id: entry.id,
            name: entry.spec.config().name.clone(),
            instance: entry.instance,
            pid: entry.pid,
            restarts: entry.restarts,
            epoch,
            status: entry.status,
            last_exit: entry.last_exit,
            credentials: entry.credentials,
            fds,
            pending_delete: Some(pending_delete),
            manual,
            reload: Some(entry.reload),
            dog: entry.dog.clone(),
            // Written as a pair with the flag below: a config without its
            // flag promotes on the wrong identity.
            pending: entry.pending.as_ref().map(|parked| parked.config().clone()),
            // Same `Option` as the line above, so the two keys are absent
            // together.
            pending_reidentifies: entry.pending.as_ref().map(|_| entry.pending_reidentifies),
            ready_failed: Some(ready_failed),
            // The slot's own field is written on the one transition into
            // `WaitingRestart` and never cleared, so gating here stops an
            // expired moment riding out on an `Online` row.
            restart_due: (entry.status == ProcStatus::WaitingRestart)
                .then_some(restart_due)
                .flatten(),
            app: entry.spec.config().clone(),
        }
    }

    /// The descriptor numbers this instance's output travels on.
    #[must_use]
    #[allow(dead_code, reason = "read by this crate's own tests")]
    pub const fn fds(&self) -> CarriedFds {
        self.fds
    }

    /// Whether an operator's `delete` targeted this instance before the
    /// exec, or `None`, which means "no".
    #[must_use]
    pub const fn pending_delete(&self) -> Option<bool> {
        self.pending_delete
    }

    /// The manual command that owned this instance's next exit before the
    /// exec, or `None` for an instance no command was waiting on.
    #[must_use]
    pub const fn manual(&self) -> Option<PendingManual> {
        self.manual
    }

    /// Which half of a reload's swap this instance is, or `None`, which
    /// means [`ReloadState::None`].
    #[must_use]
    pub const fn reload(&self) -> Option<ReloadState> {
        self.reload
    }

    /// Where this instance's binary came from if it is a dog, or `None` for
    /// an ordinary sheep.
    ///
    /// Borrowed rather than cloned: [`DogSource::Adopted`] owns a path.
    #[must_use]
    pub const fn dog(&self) -> Option<&DogSource> {
        self.dog.as_ref()
    }

    /// The config a load parked for this instance's next spawn, or `None`
    /// for an instance nothing is owed.
    #[must_use]
    pub const fn pending(&self) -> Option<&AppConfig> {
        self.pending.as_ref()
    }

    /// Whether promoting [`Self::pending`] must re-resolve the identity, or
    /// `None`, which reads as `false`.
    #[must_use]
    pub const fn pending_reidentifies(&self) -> Option<bool> {
        self.pending_reidentifies
    }

    /// Whether a reload's readiness verification has already failed against
    /// this instance, or `None`, which reads as `false`.
    #[must_use]
    pub const fn ready_failed(&self) -> Option<bool> {
        self.ready_failed
    }

    /// When this instance's owed respawn falls due, or `None` for one that is
    /// not owed a respawn.
    #[must_use]
    pub const fn restart_due(&self) -> Option<SystemTime> {
        self.restart_due
    }

    /// The supervisor slot's respawn epoch at the moment of the handover.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The entry id this instance keeps across the handover.
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    /// The name an operator reaches this instance by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The instance slot within its app.
    #[must_use]
    pub const fn instance(&self) -> u32 {
        self.instance
    }

    /// The pid this instance is running under, or `None` for one that is
    /// registered and not running.
    ///
    /// An instance with no pid has [`CarriedFds::none`] and nothing to adopt.
    #[must_use]
    pub const fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Respawns performed so far, which the restart budget counts against.
    #[must_use]
    pub const fn restarts(&self) -> u32 {
        self.restarts
    }

    /// The instance's lifecycle status.
    #[must_use]
    pub const fn status(&self) -> ProcStatus {
        self.status
    }

    /// How this instance most recently stopped existing, if it has.
    #[must_use]
    pub const fn last_exit(&self) -> Option<ExitInfo> {
        self.last_exit
    }

    /// The identity this instance's next spawn runs under, resolved once by
    /// the predecessor.
    #[must_use]
    pub const fn credentials(&self) -> SpawnIdentity {
        self.credentials
    }

    /// The config this instance runs under, as its predecessor normalized it.
    ///
    /// Not a [`ResolvedApp`](shep_core::config::ResolvedApp): see the field's
    /// own doc.
    #[must_use]
    pub const fn app(&self) -> &AppConfig {
        &self.app
    }
}

/// The descriptor numbers one sheep's output travels on, the one its input
/// travels back through, and the one carrying both directions of its
/// shepherd channel.
///
/// `None` on the four output fields means the instance is registered and not
/// running; losing a sheep's stdout read end blocks the child on `write()`
/// once the 64KiB pipe buffer fills. [`Self::stdin`] and [`Self::channel`] are
/// present only for a running sheep whose app asked for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarriedFds {
    /// The read end of the sheep's stdout pipe.
    pub out_pipe: Option<RawFd>,
    /// The read end of the sheep's stderr pipe.
    pub err_pipe: Option<RawFd>,
    /// The appending handle on the sheep's stdout log file.
    pub out_log: Option<RawFd>,
    /// The appending handle on the sheep's stderr log file.
    pub err_log: Option<RawFd>,
    /// The write end of the sheep's stdin pipe, which `shep whisper` writes
    /// a line into.
    ///
    /// `None` for a sheep whose app did not set `stdin = true`, which has
    /// `/dev/null` on fd 0, and for a sheep that is not running. An absent
    /// field loads as `None`, so [`VERSION`] is unmoved.
    pub stdin: Option<RawFd>,
    /// The daemon's end of the sheep's shepherd-channel socketpair, whose
    /// other end is the child's fd 3. One number for both directions:
    /// `spawn_channel_pumps` splits it into two tasks over one open file
    /// description.
    ///
    /// `None` for a sheep whose app set none of `channel`, `wait_ready` or
    /// `shutdown_with_message`, for a sheep that is not running, and for one
    /// whose child has closed its fd 3. An absent field loads as `None`.
    pub channel: Option<RawFd>,
}

/// Which of a sheep's six descriptors a number is. A stdout pipe and a stdin
/// pipe are both pipes and are refused by opposite checks, so the slot has to
/// travel with the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SheepFd {
    /// The read end of the sheep's stdout pipe ([`CarriedFds::out_pipe`]).
    OutPipe,
    /// The read end of the sheep's stderr pipe ([`CarriedFds::err_pipe`]).
    ErrPipe,
    /// The appending handle on its stdout log ([`CarriedFds::out_log`]).
    OutLog,
    /// The appending handle on its stderr log ([`CarriedFds::err_log`]).
    ErrLog,
    /// The write end of its stdin pipe ([`CarriedFds::stdin`]).
    Stdin,
    /// The daemon's end of its shepherd channel ([`CarriedFds::channel`]).
    Channel,
}

impl SheepFd {
    /// What this slot is called in a refusal. Must match the wording the
    /// adoption functions use.
    pub(crate) const fn describe(self) -> &'static str {
        match self {
            Self::OutPipe => "stdout pipe",
            Self::ErrPipe => "stderr pipe",
            Self::OutLog => "stdout log",
            Self::ErrLog => "stderr log",
            Self::Stdin => "stdin pipe",
            Self::Channel => "shepherd channel",
        }
    }
}

impl CarriedFds {
    /// The six numbers in a fixed order: stdout's pipe, stderr's pipe,
    /// stdout's log, stderr's log, stdin's pipe, the shepherd channel.
    #[must_use]
    pub const fn all(&self) -> [Option<RawFd>; 6] {
        [
            self.out_pipe,
            self.err_pipe,
            self.out_log,
            self.err_log,
            self.stdin,
            self.channel,
        ]
    }

    /// [`Self::all`], with each number labelled by which of the six it is.
    ///
    /// [`adopt::adopt`] and [`adopt::dry_run`] must agree on the kinds: a
    /// number rehearsed as the wrong kind is a rehearsal that passes and a
    /// boot that still fails.
    pub(crate) const fn all_kinded(&self) -> [(Option<RawFd>, SheepFd); 6] {
        [
            (self.out_pipe, SheepFd::OutPipe),
            (self.err_pipe, SheepFd::ErrPipe),
            (self.out_log, SheepFd::OutLog),
            (self.err_log, SheepFd::ErrLog),
            (self.stdin, SheepFd::Stdin),
            (self.channel, SheepFd::Channel),
        ]
    }

    /// The no-descriptors case: a sheep that is registered and not running.
    /// [`fitness`] does not refuse it.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            out_pipe: None,
            err_pipe: None,
            out_log: None,
            err_log: None,
            stdin: None,
            channel: None,
        }
    }
}

/// Where this process's binary was when it started, as `argv[0]` resolved
/// against the startup directory. Set once by [`record_launch_path`], read
/// only by [`exec_target`].
///
/// The inner `Option` is "recorded, and there was nothing usable to record",
/// distinct from "never recorded", which is a missing call.
static LAUNCH_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Record how this process was invoked, for a later [`exec_target`].
///
/// Call once, before anything can move the current directory: it resolves
/// `argv[0]` against it. The first call wins.
pub fn record_launch_path() {
    let _ = LAUNCH_PATH.set(launch_path_from_argv());
}

/// The binary to `execv` for a handover, and never the running image.
///
/// Prefers the path [`record_launch_path`] recorded, falling back to
/// [`std::env::current_exe`], both through [`check_target`]. On Linux
/// `current_exe` reads `/proc/self/exe`, which after an upgrade renames a new
/// binary over the old one comes back as `"<path> (deleted)"` and cannot be
/// exec'd.
///
/// # Errors
/// - [`io::ErrorKind::NotFound`] if neither candidate is a file safe to exec.
pub fn exec_target() -> io::Result<PathBuf> {
    let recorded = LAUNCH_PATH.get().cloned().flatten();
    let current = std::env::current_exe();
    resolve_target(
        [recorded, current.as_deref().ok().map(Path::to_path_buf)],
        current.as_ref().err(),
    )
}

/// Returns the first entry of `candidates` that [`check_target`] accepts,
/// skipping a `None`. `current_exe_error` is folded into the diagnostic only.
///
/// [`crate::dogs::dog_app`] resolves a built-in dog's program through it too.
///
/// # Errors
/// [`io::ErrorKind::NotFound`] if every candidate is `None` or refused.
/// The message names each candidate tried and what was wrong with it.
pub(crate) fn resolve_target(
    candidates: [Option<PathBuf>; 2],
    current_exe_error: Option<&io::Error>,
) -> io::Result<PathBuf> {
    let mut refusals = Vec::new();
    for candidate in candidates {
        let Some(candidate) = candidate else { continue };
        match check_target(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(problem) => refusals.push(format!("{} ({problem})", candidate.display())),
        }
    }

    if let Some(e) = current_exe_error {
        refusals.push(format!("this process's own image ({e})"));
    }
    if refusals.is_empty() {
        refusals.push("no candidate at all".to_owned());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no binary to exec: {}", refusals.join("; ")),
    ))
}

/// Why a candidate path is not safe to `execv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetProblem {
    /// The path carries Linux's `" (deleted)"` suffix, so it names an
    /// unlinked inode rather than a file.
    DeletedInode,
    /// Nothing is at the path, or it could not be read.
    Missing,
    /// Something is at the path, but it is a directory or a device.
    NotAFile,
}

impl core::fmt::Display for TargetProblem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::DeletedInode => "names a deleted inode, not a file",
            Self::Missing => "is not on disk",
            Self::NotAFile => "is not a file",
        };
        f.write_str(text)
    }
}

/// Whether `candidate` is a file this daemon may replace itself with.
///
/// The `" (deleted)"` check runs first and refuses even a path that really
/// does exist.
fn check_target(candidate: &Path) -> Result<(), TargetProblem> {
    if candidate.to_string_lossy().contains(" (deleted)") {
        return Err(TargetProblem::DeletedInode);
    }
    match std::fs::metadata(candidate) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(_) => Err(TargetProblem::NotAFile),
        Err(_) => Err(TargetProblem::Missing),
    }
}

/// This process's `argv[0]`, resolved against the current directory.
///
/// `None` when there is no `argv[0]`, when it is empty, or when it holds no
/// separator and so came from a `PATH` lookup this cannot undo. An absolute
/// `argv[0]` passes through the join unchanged.
fn launch_path_from_argv() -> Option<PathBuf> {
    let argv0 = PathBuf::from(std::env::args_os().next()?);
    if argv0.as_os_str().is_empty() {
        return None;
    }
    if argv0.is_absolute() {
        return Some(argv0);
    }
    let has_separator = argv0
        .parent()
        .is_some_and(|dir| !dir.as_os_str().is_empty());
    has_separator.then(|| std::env::current_dir().ok().map(|cwd| cwd.join(&argv0)))?
}

/// The environment variable a handover leaves for its successor, holding
/// the path of the blob it is to adopt.
///
/// Its presence is also the successor's only marker that it is one: an image
/// started any other way has no blob to read and boots normally.
pub const HANDOVER_ENV: &str = "SHEP_HANDOVER";

/// Replace this process with a fresh copy of the shep binary, handing it
/// `blob`'s flock.
///
/// Ordered: resolve the target, write the blob, clear `FD_CLOEXEC` on the
/// descriptors it names and only those, `execv`. A failed exec removes the
/// blob, which would describe a handover that never happened.
///
/// # Errors
/// No binary is safe to exec, the blob could not be written, a descriptor it
/// names is not open, or the exec failed. Each returns with no blob on disk
/// and `FD_CLOEXEC` back on every descriptor it cleared.
pub fn hand_over(blob: &Handover, paths: &ShepPaths) -> io::Result<Infallible> {
    exec_into(&exec_target()?, blob, paths)
}

/// [`hand_over`], against a caller-chosen binary.
///
/// # Errors
///
/// As [`hand_over`], minus the target resolution.
fn exec_into(target: &Path, blob: &Handover, paths: &ShepPaths) -> io::Result<Infallible> {
    let written = blob.write(paths)?;
    let failure = match exec_with_blob(target, blob, &written) {
        Ok(never) => match never {},
        Err(err) => err,
    };
    match fs::remove_file(&written) {
        Ok(()) | Err(_) => Err(failure),
    }
}

/// Clear `FD_CLOEXEC` on what `blob` names, then become `target`.
///
/// # Errors
///
/// A descriptor the blob names is not open, a path or an environment entry
/// holds an interior NUL, or the exec failed. On any of them this process is
/// still itself and `written` is still on disk, which [`exec_into`] cleans up.
fn exec_with_blob(target: &Path, blob: &Handover, written: &Path) -> io::Result<Infallible> {
    let mut cleared = Vec::new();
    let failure = match keep_and_exec(target, blob, written, &mut cleared) {
        Ok(never) => match never {},
        Err(err) => err,
    };
    // Put every descriptor back: the graceful-stop fallback leaves the
    // supervisor running, so a later spawn would inherit the listener, the
    // pidfile and every carried log descriptor.
    for fd in cleared {
        let _ = fds::close_raw_after_exec(fd);
    }
    Err(failure)
}

/// [`exec_with_blob`]'s body, recording what it cleared as it goes.
///
/// `cleared` is pushed to only after a clear succeeds, so it never names a
/// descriptor this process did not change.
///
/// # Errors
///
/// As [`exec_with_blob`].
fn keep_and_exec(
    target: &Path,
    blob: &Handover,
    written: &Path,
    cleared: &mut Vec<RawFd>,
) -> io::Result<Infallible> {
    for fd in blob.named_fds() {
        fds::keep_raw_across_exec(fd)?;
        cleared.push(fd);
    }

    let path = c_string(target.as_os_str().as_bytes())?;
    let argv = std::env::args_os()
        .map(|arg| c_string(arg.as_bytes()))
        .collect::<io::Result<Vec<_>>>()?;
    let env = successor_env(written)?;

    // `execve` rather than `execv`: `execv` inherits this process's `environ`,
    // so pointing the successor at the blob would need `std::env::set_var`,
    // unsafe in edition 2024 and unsound with this many threads.
    nix::unistd::execve(&path, &argv, &env).map_err(io::Error::from)
}

/// This process's environment, with [`HANDOVER_ENV`] set to `written`.
///
/// Any inherited value of that variable is dropped: a stale entry from an
/// earlier handover names a file that has already been read and unlinked.
///
/// # Errors
///
/// A name or value holds an interior NUL.
fn successor_env(written: &Path) -> io::Result<Vec<CString>> {
    let mut env = std::env::vars_os()
        .filter(|(name, _)| name != HANDOVER_ENV)
        .map(|(name, value)| {
            let mut entry = name.into_vec();
            entry.push(b'=');
            entry.extend(value.into_vec());
            c_string(&entry)
        })
        .collect::<io::Result<Vec<_>>>()?;

    let mut marker = HANDOVER_ENV.as_bytes().to_vec();
    marker.push(b'=');
    marker.extend(written.as_os_str().as_bytes());
    env.push(c_string(&marker)?);
    Ok(env)
}

/// `bytes` as a C string, with an interior NUL reported as an `io::Error`
/// rather than as a `NulError` nothing else in this module speaks.
fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use shep_core::status::ProcStatus;
    use std::path::PathBuf;

    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::entry::{ReloadState, RestartBudget};
    use crate::privilege::SpawnIdentity;
    use crate::supervisor::{CommandOrigin, ManualKind, ReloadMode, ReloadPhase, ReloadSwap};
    use crate::testing::{app_with, test_paths};

    /// A plain, `Online` entry: no channel, not a dog, one instance, no
    /// in-flight reload. Every field a real spawn would set is present.
    fn entry_fixture(mutate: impl FnOnce(&mut AppConfig)) -> ProcessEntry {
        let spec = app_with("web", mutate);
        ProcessEntry {
            id: 1,
            spec,
            pending: None,
            pending_reidentifies: false,
            overridden: Vec::new(),
            instance: 0,
            status: ProcStatus::Online,
            pid: Some(100),
            restarts: 0,
            started_at: None,
            budget: RestartBudget::default(),
            reload: ReloadState::None,
            credentials: SpawnIdentity::Resolved(None),
            out_file: PathBuf::from("/tmp/shep-handover-test-out.log"),
            err_file: PathBuf::from("/tmp/shep-handover-test-err.log"),
            dog: None,
            last_exit: None,
        }
    }

    fn plain(entry: &ProcessEntry) -> Candidate<'_> {
        Candidate {
            entry,
            pump_unresponsive: false,
        }
    }

    /// A candidate whose log pump missed the snapshot's deadline, which is the
    /// one thing that refuses a flock.
    fn wedged(entry: &ProcessEntry) -> Candidate<'_> {
        Candidate {
            entry,
            pump_unresponsive: true,
        }
    }

    #[test]
    fn a_plain_sheep_is_carryable() {
        let e = entry_fixture(|_| {});
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    #[test]
    fn one_unsupported_sheep_refuses_the_whole_flock() {
        let carryable = entry_fixture(|_| {});
        let unsupported = entry_fixture(|_| {});
        assert!(matches!(
            fitness(&[plain(&carryable), wedged(&unsupported)]),
            Fitness::Refused(_)
        ));
    }

    #[test]
    fn the_refusal_names_which_sheep_and_why() {
        let unsupported = entry_fixture(|_| {});
        let Fitness::Refused(r) = fitness(&[wedged(&unsupported)]) else {
            panic!("expected a refusal")
        };
        let text = r.to_string();
        assert!(text.contains("did not report its descriptors"), "{text}");
        assert!(text.contains("web"), "{text}");
        assert!(
            text.contains("falls back to a stop-and-start"),
            "the refusal must say what happens instead, not only that it declined: {text}"
        );
    }

    #[test]
    fn a_pump_that_did_not_report_in_time_refuses_as_a_fault_not_a_feature() {
        let e = entry_fixture(|_| {});
        let candidate = Candidate {
            entry: &e,
            pump_unresponsive: true,
        };
        let Fitness::Refused(r) = fitness(&[candidate]) else {
            panic!("a sheep whose descriptors are unknown cannot be carried")
        };
        let text = r.to_string();
        assert!(
            text.contains("did not report its descriptors in time"),
            "{text}"
        );
        assert!(
            !text.contains("cannot yet"),
            "a wedged pump is not a feature a later phase ships: {text}"
        );
    }

    #[test]
    fn an_empty_flock_is_carryable() {
        assert_eq!(fitness(&[]), Fitness::Carryable);
    }

    #[test]
    fn a_sheep_with_a_channel_is_carried() {
        let e = entry_fixture(|app| app.channel = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    #[test]
    fn wait_ready_alone_is_carried() {
        let e = entry_fixture(|app| app.wait_ready = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    /// `shutdown_with_message` is the one of the three whose channel traffic
    /// runs from the shepherd to the child, so the writer half has to work.
    #[test]
    fn shutdown_with_message_alone_is_carried() {
        let e = entry_fixture(|app| app.shutdown_with_message = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    #[test]
    fn a_sheep_with_stdin_is_carried() {
        let e = entry_fixture(|app| app.stdin = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    /// Both sources, since a gate reading only `DogSource::BuiltIn` would pass
    /// on one of them.
    #[test]
    fn a_dog_is_carried_rather_than_refused() {
        let mut built_in = entry_fixture(|_| {});
        built_in.dog = Some(DogSource::BuiltIn);
        assert_eq!(fitness(&[plain(&built_in)]), Fitness::Carryable);

        let mut adopted = entry_fixture(|_| {});
        adopted.dog = Some(DogSource::Adopted {
            path: "/opt/bin/shep-log-rotate".to_string(),
        });
        assert_eq!(fitness(&[plain(&adopted)]), Fitness::Carryable);
    }

    /// Both slots, since a version that stopped reading `instances` on slot 0
    /// alone would pass a one-candidate case.
    #[test]
    fn an_app_with_more_than_one_instance_is_carried() {
        let mut zero = entry_fixture(|app| app.instances = 2);
        let mut one = entry_fixture(|app| app.instances = 2);
        one.id = 2;
        one.instance = 1;
        one.pid = Some(101);
        assert_eq!(fitness(&[plain(&zero), plain(&one)]), Fitness::Carryable);
        // A gate that read only the first candidate would pass the assertion
        // above too.
        zero.instance = 1;
        one.instance = 0;
        assert_eq!(fitness(&[plain(&one), plain(&zero)]), Fitness::Carryable);
    }

    /// [`CarriedSheep::instance`] is carried per row rather than re-derived
    /// from a count: without that, two live pids write into each other's log
    /// files under each other's `SHEP_INSTANCE`.
    #[test]
    fn each_instance_carries_its_own_slot_and_descriptors() {
        let mut zero = entry_fixture(|app| app.instances = 2);
        zero.instance = 0;
        let mut one = entry_fixture(|app| app.instances = 2);
        one.id = 2;
        one.instance = 1;
        one.pid = Some(101);

        let blob = Handover::new(
            vec![
                CarriedSheep::from_entry(&zero, 7, fds_at(11), false, None, false, None),
                CarriedSheep::from_entry(&one, 8, fds_at(21), false, None, false, None),
            ],
            DaemonFds {
                listener: 3,
                pidfile: 4,
            },
            Counters {
                next_id: 9,
                next_deadline: 5,
                next_action_stamp: 2,
            },
            Vec::new(),
        );
        let back: Handover = serde_json::from_str(&serde_json::to_string(&blob).unwrap()).unwrap();

        let carried = back.sheep();
        assert_eq!(carried.len(), 2, "one row per instance, never per app");
        // Bound by slot rather than by position, so a reordered blob still
        // has to put each pid with its own slot.
        let slot_zero = carried
            .iter()
            .find(|sheep| sheep.instance() == 0)
            .expect("slot 0 must be carried");
        let slot_one = carried
            .iter()
            .find(|sheep| sheep.instance() == 1)
            .expect("slot 1 must be carried");
        assert_eq!(slot_zero.id(), 1);
        assert_eq!(slot_zero.pid(), Some(100));
        assert_eq!(slot_zero.epoch(), 7);
        assert_eq!(slot_zero.fds(), fds_at(11));
        assert_eq!(slot_one.id(), 2);
        assert_eq!(slot_one.pid(), Some(101));
        assert_eq!(slot_one.epoch(), 8);
        assert_eq!(slot_one.fds(), fds_at(21));
        assert_eq!(
            slot_zero.name(),
            slot_one.name(),
            "both slots are the same app, which is what makes the slot the \
             only thing telling them apart"
        );
    }

    /// Both halves of the swap, and the drainee's linked id with them.
    #[test]
    fn a_swap_in_flight_no_longer_refuses_and_reaches_the_blob() {
        let mut drainee = entry_fixture(|_| {});
        drainee.reload = ReloadState::Drainee { new_id: Some(9) };
        let mut replacement = entry_fixture(|_| {});
        replacement.id = 9;
        replacement.reload = ReloadState::Replacement;
        assert_eq!(
            fitness(&[plain(&drainee), plain(&replacement)]),
            Fitness::Carryable
        );

        assert_eq!(
            carried(&drainee).reload(),
            Some(ReloadState::Drainee { new_id: Some(9) }),
            "the id linking the two halves must survive the blob, not just the role"
        );
        assert_eq!(
            carried(&replacement).reload(),
            Some(ReloadState::Replacement)
        );
    }

    /// `Candidate` has nothing to say about a pending command, so this
    /// asserts through the blob rather than through the gate.
    #[test]
    fn a_pending_manual_command_no_longer_refuses_and_reaches_the_blob() {
        let e = entry_fixture(|_| {});
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);

        let marked = CarriedSheep::from_entry(
            &e,
            7,
            fds_at(11),
            false,
            Some(PendingManual {
                kind: ManualKind::Delete,
                origin: CommandOrigin::Automatic,
            }),
            false,
            None,
        );
        assert_eq!(
            marked.manual(),
            Some(PendingManual {
                kind: ManualKind::Delete,
                origin: CommandOrigin::Automatic,
            }),
            "both halves of the marker must survive the blob, not just that one exists"
        );
    }

    /// The six descriptor numbers a running sheep would have, counting up
    /// from `base`, so two instances can be given disjoint sets.
    const fn fds_at(base: RawFd) -> CarriedFds {
        CarriedFds {
            out_pipe: Some(base),
            err_pipe: Some(base + 1),
            out_log: Some(base + 2),
            err_log: Some(base + 3),
            stdin: Some(base + 4),
            channel: Some(base + 5),
        }
    }

    /// One carried sheep off `entry`, with the descriptor numbers a
    /// running sheep would have.
    fn carried(entry: &ProcessEntry) -> CarriedSheep {
        CarriedSheep::from_entry(entry, 7, fds_at(11), false, None, false, None)
    }

    fn handover_over(entry: &ProcessEntry) -> Handover {
        Handover {
            version: VERSION,
            sheep: vec![carried(entry)],
            listener_fd: 3,
            pidfile_fd: 4,
            next_id: 9,
            next_deadline: 5,
            next_action_stamp: 2,
            reloads: Some(Vec::new()),
        }
    }

    fn sample_handover() -> Handover {
        handover_over(&entry_fixture(|_| {}))
    }

    /// Six distinct numbers, so the equality below is about the pairing
    /// rather than the length.
    #[test]
    fn every_carried_number_is_kinded_in_the_same_order() {
        let fds = CarriedFds {
            out_pipe: Some(10),
            err_pipe: Some(11),
            out_log: Some(12),
            err_log: Some(13),
            stdin: Some(14),
            channel: Some(15),
        };

        assert_eq!(
            fds.all_kinded().map(|(fd, _)| fd),
            fds.all(),
            "the kinded walk and the `FD_CLOEXEC` walk must see the same numbers"
        );

        let slots: std::collections::HashSet<SheepFd> =
            fds.all_kinded().iter().map(|(_, slot)| *slot).collect();
        assert_eq!(slots.len(), 6, "each slot must appear exactly once");
    }

    fn sample_handover_with_secret_env() -> Handover {
        let entry = entry_fixture(|app| {
            app.env.insert("TOKEN".to_owned(), "hunter2".to_owned());
        });
        assert!(
            entry.spec.config().env.values().any(|v| v == "hunter2"),
            "the fixture must really carry the secret it is testing for"
        );
        handover_over(&entry)
    }

    #[test]
    fn a_blob_round_trips() {
        let h = sample_handover();
        let back: Handover = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn a_blob_round_trips_a_sheeps_environment_intact() {
        let text = serde_json::to_string(&sample_handover_with_secret_env()).unwrap();
        let back: Handover = serde_json::from_str(&text).unwrap();
        assert_eq!(
            back.sheep[0].app.env.get("TOKEN").map(String::as_str),
            Some("hunter2"),
            "{text}"
        );
    }

    #[test]
    fn debug_redacts_a_carried_sheeps_environment() {
        // An exact string, not a `contains`: a field added later that prints
        // env cannot be named in a substring check.
        let entry = entry_fixture(|app| {
            app.env.insert("TOKEN".to_owned(), "hunter2".to_owned());
        });
        assert_eq!(
            format!("{:?}", carried(&entry)),
            "CarriedSheep { id: 1, name: \"web\", instance: 0, pid: Some(100), restarts: 0, \
             epoch: 7, status: Online, last_exit: None, credentials: Resolved(None), fds: \
             CarriedFds { out_pipe: Some(11), err_pipe: Some(12), out_log: Some(13), err_log: \
             Some(14), stdin: Some(15), channel: Some(16) }, pending_delete: Some(false), \
             manual: None, reload: Some(None), ready_failed: Some(false), restart_due: None, \
             dog: None, pending: None, pending_reidentifies: None, app: AppConfig { \
             name: \"web\", script: \"./srv\", env: <1 vars>, .. } }"
        );
        // The whole blob too, which holds fields of its own.
        let text = format!("{:?}", sample_handover_with_secret_env());
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("TOKEN"), "{text}");
    }

    #[test]
    fn debug_redacts_the_environment_on_a_process_entry() {
        // `ProcessEntry` derives `Debug`, so its safety rests on
        // `AppConfig`'s redacted rendering reaching it through `spec`.
        let entry = entry_fixture(|app| {
            app.env.insert("TOKEN".to_owned(), "hunter2".to_owned());
        });
        assert_eq!(
            format!("{entry:?}"),
            "ProcessEntry { id: 1, spec: ResolvedApp { config: AppConfig { name: \"web\", \
             script: \"./srv\", env: <1 vars>, .. } }, pending: None, pending_reidentifies: \
             false, overridden: [], instance: 0, status: Online, pid: Some(100), restarts: 0, \
             started_at: None, budget: RestartBudget { unstable_count: 0 }, reload: None, \
             credentials: Resolved(None), out_file: \"/tmp/shep-handover-test-out.log\", \
             err_file: \"/tmp/shep-handover-test-err.log\", dog: None, last_exit: None }"
        );
    }

    #[test]
    fn a_written_blob_is_readable_only_by_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.run).unwrap();

        let written = sample_handover().write(&paths).unwrap();

        assert_eq!(written, Handover::path(&paths));
        let mode = std::fs::metadata(&written).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        assert_eq!(Handover::read(&written).unwrap(), sample_handover());
    }

    #[test]
    fn a_stale_blob_does_not_lend_its_mode_to_the_next_one() {
        // `OpenOptions::mode` applies only when the open creates the file,
        // so a leftover blob left in place would keep whatever mode it had.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.run).unwrap();
        let path = Handover::path(&paths);
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        sample_handover().write(&paths).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn a_blob_from_a_future_version_is_refused_not_guessed_at() {
        let mut v = serde_json::to_value(sample_handover()).unwrap();
        v["version"] = serde_json::json!(u32::MAX);
        assert!(Handover::load_value(v).is_err());
    }

    #[test]
    fn a_blob_written_before_stdin_was_carried_still_loads() {
        let mut value = serde_json::to_value(sample_handover()).unwrap();
        let fds = value["sheep"][0]["fds"]
            .as_object_mut()
            .expect("a carried sheep names its descriptors");
        assert!(
            fds.remove("stdin").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(value).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].fds.stdin, None);
        assert_eq!(
            loaded.sheep[0].fds.out_pipe,
            sample_handover().sheep[0].fds.out_pipe,
            "the other five are unchanged by the one that was absent"
        );
    }

    #[test]
    fn a_blob_written_before_the_channel_was_carried_still_loads() {
        let mut value = serde_json::to_value(sample_handover()).unwrap();
        let fds = value["sheep"][0]["fds"]
            .as_object_mut()
            .expect("a carried sheep names its descriptors");
        assert!(
            fds.remove("channel").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(value).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].fds.channel, None);
        assert_eq!(
            loaded.sheep[0].fds.stdin,
            sample_handover().sheep[0].fds.stdin,
            "the other five are unchanged by the one that was absent"
        );
    }

    #[test]
    fn a_blob_written_before_pending_delete_was_carried_still_loads() {
        let mut value = serde_json::to_value(sample_handover()).unwrap();
        let sheep = value["sheep"][0]
            .as_object_mut()
            .expect("a carried sheep is an object");
        assert!(
            sheep.remove("pending_delete").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(value).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].pending_delete(), None);
        assert_eq!(
            loaded.sheep[0].id(),
            sample_handover().sheep[0].id(),
            "the rest of the row is unchanged by the one field that was absent"
        );
    }

    #[test]
    fn a_sheep_with_nothing_parked_carries_neither_parking_key() {
        let entry = entry_fixture(|_| {});
        assert!(
            entry.pending.is_none(),
            "fixture check: this case is about a sheep with nothing parked"
        );
        let blob = handover_over(&entry);

        let round_tripped = Handover::load_value(serde_json::to_value(&blob).unwrap())
            .expect("a blob this daemon wrote must load");
        assert_eq!(round_tripped.sheep[0].pending(), None);
        assert_eq!(
            round_tripped.sheep[0].pending_reidentifies(),
            None,
            "a reset flag with no parked config to apply it to is a key that means nothing"
        );
    }

    #[test]
    fn a_blob_written_before_pending_was_carried_still_loads() {
        let mut entry = entry_fixture(|_| {});
        entry.pending = Some(app_with("web", |app| {
            app.env.insert("MODE".to_owned(), "blue".to_owned());
        }));
        entry.pending_reidentifies = true;
        let blob = handover_over(&entry);

        // A blob that has the two keys carries both.
        let round_tripped = Handover::load_value(serde_json::to_value(&blob).unwrap())
            .expect("a blob this daemon wrote must load");
        assert_eq!(
            round_tripped.sheep[0]
                .pending()
                .expect("the parked config crosses the exec")
                .env
                .get("MODE")
                .map(String::as_str),
            Some("blue")
        );
        assert_eq!(
            round_tripped.sheep[0].pending_reidentifies(),
            Some(true),
            "and its reset flag crosses with it: a config that arrived without one would \
             promote on the identity the flag exists to replace"
        );

        // An older blob, which has neither key.
        let mut value = serde_json::to_value(&blob).unwrap();
        let sheep = value["sheep"][0]
            .as_object_mut()
            .expect("a carried sheep is an object");
        for key in ["pending", "pending_reidentifies"] {
            assert!(
                sheep.remove(key).is_some(),
                "the field this case removes must be there to remove: {key}"
            );
        }

        let loaded = Handover::load_value(value).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].pending(), None);
        assert_eq!(loaded.sheep[0].pending_reidentifies(), None);
        assert_eq!(
            loaded.sheep[0].id(),
            blob.sheep[0].id(),
            "the rest of the row is unchanged by the two fields that were absent"
        );
    }

    #[test]
    fn a_blob_written_before_a_manual_marker_was_carried_still_loads() {
        let marker = PendingManual {
            kind: ManualKind::Restart,
            origin: CommandOrigin::Automatic,
        };
        let mut blob = sample_handover();
        blob.sheep[0] = CarriedSheep::from_entry(
            &entry_fixture(|_| {}),
            7,
            fds_at(11),
            false,
            Some(marker),
            false,
            None,
        );
        let value = serde_json::to_value(&blob).unwrap();

        assert_eq!(
            Handover::load_value(value.clone())
                .expect("a current blob loads")
                .sheep[0]
                .manual(),
            Some(marker),
            "a marker on the wire must come back whole, kind and origin both"
        );

        let mut older = value;
        let sheep = older["sheep"][0]
            .as_object_mut()
            .expect("a carried sheep is an object");
        assert!(
            sheep.remove("manual").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(older).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].manual(), None);
        assert_eq!(
            loaded.sheep[0].id(),
            blob.sheep[0].id(),
            "the rest of the row is unchanged by the one field that was absent"
        );
    }

    /// Both keys an older blob lacks: `sheep[].reload` and the top-level
    /// `reloads`. The job is asserted as well as the marker.
    #[test]
    fn a_blob_written_before_a_swap_was_carried_still_loads() {
        let job = CarriedReload {
            app: "web".to_owned(),
            queue: vec![11, 12],
            mode: ReloadMode::Serial,
            swap: ReloadSwap {
                old_id: 1,
                new_id: Some(9),
                phase: ReloadPhase::AwaitReady,
            },
        };
        let mut blob = sample_handover();
        let mut drainee = entry_fixture(|_| {});
        drainee.reload = ReloadState::Drainee { new_id: Some(9) };
        blob.sheep[0] = carried(&drainee);
        blob.reloads = Some(vec![job.clone()]);
        let value = serde_json::to_value(&blob).unwrap();

        let current = Handover::load_value(value.clone()).expect("a current blob loads");
        assert_eq!(
            current.sheep[0].reload(),
            Some(ReloadState::Drainee { new_id: Some(9) }),
            "the marker on the wire must come back whole, role and linked id both"
        );
        assert_eq!(
            current.reloads(),
            &[job],
            "the job must come back whole: queue, mode and every field of the swap"
        );

        let mut older = value;
        let object = older.as_object_mut().expect("a blob is an object");
        assert!(
            object.remove("reloads").is_some(),
            "the field this case removes must be there to remove"
        );
        let sheep = older["sheep"][0]
            .as_object_mut()
            .expect("a carried sheep is an object");
        assert!(
            sheep.remove("reload").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(older).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].reload(), None);
        assert!(loaded.reloads().is_empty());
        assert_eq!(
            loaded.sheep[0].id(),
            blob.sheep[0].id(),
            "the rest of the row is unchanged by the two fields that were absent"
        );
    }

    #[test]
    fn a_blob_written_before_ready_failed_was_carried_still_loads() {
        let mut blob = sample_handover();
        blob.sheep[0] = CarriedSheep::from_entry(
            &entry_fixture(|_| {}),
            7,
            fds_at(11),
            false,
            None,
            true,
            None,
        );
        let value = serde_json::to_value(&blob).unwrap();

        assert_eq!(
            Handover::load_value(value.clone())
                .expect("a current blob loads")
                .sheep[0]
                .ready_failed(),
            Some(true),
            "a verdict on the wire must come back as one, or the rollback it keeps reachable \
             cannot reach anything"
        );

        let mut older = value;
        let sheep = older["sheep"][0]
            .as_object_mut()
            .expect("a carried sheep is an object");
        assert!(
            sheep.remove("ready_failed").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(older).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].ready_failed(), None);
        assert_eq!(
            loaded.sheep[0].id(),
            blob.sheep[0].id(),
            "the rest of the row is unchanged by the one field that was absent"
        );
    }

    /// The whole `DogSource`, not a boolean: [`CarriedSheep::dog`] says what
    /// the marker is read for.
    #[test]
    fn a_dogs_marker_crosses_the_blob() {
        let mut entry = entry_fixture(|_| {});
        entry.dog = Some(DogSource::Adopted {
            path: "/opt/bin/shep-log-rotate".to_string(),
        });
        let mut blob = sample_handover();
        blob.sheep[0] = CarriedSheep::from_entry(&entry, 7, fds_at(11), false, None, false, None);

        let loaded = Handover::load_value(serde_json::to_value(&blob).unwrap())
            .expect("a current blob loads");

        assert_eq!(
            loaded.sheep[0].dog(),
            Some(&DogSource::Adopted {
                path: "/opt/bin/shep-log-rotate".to_string(),
            }),
            "a dog that crossed the exec as an ordinary sheep is one `shep dogs` has lost"
        );
    }

    /// Not implied by the case above: a `from_entry` that hardcoded a source
    /// would pass that one and turn every carried app into a dog.
    #[test]
    fn a_plain_sheep_crosses_the_blob_without_one() {
        let mut blob = sample_handover();
        blob.sheep[0] = CarriedSheep::from_entry(
            &entry_fixture(|_| {}),
            7,
            fds_at(11),
            false,
            None,
            false,
            None,
        );

        let loaded = Handover::load_value(serde_json::to_value(&blob).unwrap())
            .expect("a current blob loads");

        assert_eq!(loaded.sheep[0].dog(), None);
    }

    #[test]
    fn a_blob_written_before_a_dog_was_carried_still_loads() {
        let mut entry = entry_fixture(|_| {});
        entry.dog = Some(DogSource::BuiltIn);
        let mut blob = sample_handover();
        blob.sheep[0] = CarriedSheep::from_entry(&entry, 7, fds_at(11), false, None, false, None);
        let value = serde_json::to_value(&blob).unwrap();

        assert_eq!(
            Handover::load_value(value.clone())
                .expect("a current blob loads")
                .sheep[0]
                .dog(),
            Some(&DogSource::BuiltIn),
            "a marker on the wire must come back as one"
        );

        let mut older = value;
        let sheep = older["sheep"][0]
            .as_object_mut()
            .expect("a carried sheep is an object");
        assert!(
            sheep.remove("dog").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(older).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].dog(), None);
        assert_eq!(
            loaded.sheep[0].id(),
            blob.sheep[0].id(),
            "the rest of the row is unchanged by the one field that was absent"
        );
    }

    /// One entry owed a respawn, which is the only status the deadline is
    /// carried for.
    fn owed_a_restart() -> ProcessEntry {
        let mut entry = entry_fixture(|_| {});
        entry.status = ProcStatus::WaitingRestart;
        entry.pid = None;
        entry
    }

    /// A whole second of slack on the round trip: this pins the value rather
    /// than the precision.
    #[test]
    fn a_blob_written_before_a_restart_deadline_was_carried_still_loads() {
        let due = SystemTime::now() + core::time::Duration::from_secs(600);
        let mut blob = sample_handover();
        blob.sheep[0] = CarriedSheep::from_entry(
            &owed_a_restart(),
            7,
            fds_at(11),
            false,
            None,
            false,
            Some(due),
        );
        let value = serde_json::to_value(&blob).unwrap();

        let carried = Handover::load_value(value.clone())
            .expect("a current blob loads")
            .sheep[0]
            .restart_due()
            .expect("a deadline on the wire must come back as one");
        let drift = carried
            .duration_since(due)
            .or_else(|_| due.duration_since(carried))
            .unwrap();
        assert!(
            drift < core::time::Duration::from_secs(1),
            "a moment that does not survive the wire is a moment the successor cannot re-arm \
             from: {drift:?} of drift"
        );

        let mut older = value;
        let sheep = older["sheep"][0]
            .as_object_mut()
            .expect("a carried sheep is an object");
        assert!(
            sheep.remove("restart_due").is_some(),
            "the field this case removes must be there to remove"
        );

        let loaded = Handover::load_value(older).expect("an older blob must still load");

        assert_eq!(loaded.sheep[0].restart_due(), None);
        assert_eq!(
            loaded.sheep[0].id(),
            blob.sheep[0].id(),
            "the rest of the row is unchanged by the one field that was absent"
        );
    }

    /// Both halves, since a gate that dropped the field unconditionally would
    /// pass the second assertion on its own.
    #[test]
    fn a_deadline_is_carried_only_for_a_sheep_owed_a_respawn() {
        let due = SystemTime::now() + core::time::Duration::from_secs(600);

        let waiting = CarriedSheep::from_entry(
            &owed_a_restart(),
            7,
            fds_at(11),
            false,
            None,
            false,
            Some(due),
        );
        assert!(
            waiting.restart_due().is_some(),
            "a sheep that IS owed a respawn must carry its deadline, or there is nothing to gate"
        );

        let online = CarriedSheep::from_entry(
            &entry_fixture(|_| {}),
            7,
            fds_at(11),
            false,
            None,
            false,
            Some(due),
        );
        assert_eq!(
            online.restart_due(),
            None,
            "a running sheep must not carry a deadline left over from an earlier exit"
        );
    }

    #[test]
    fn the_exec_target_exists_and_is_a_file() {
        let p = exec_target().unwrap();
        assert!(p.is_file(), "{}", p.display());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_deleted_inode_path_is_never_returned() {
        let p = exec_target().unwrap();
        assert!(
            !p.to_string_lossy().contains("(deleted)"),
            "exec target resolved to a deleted inode: {}",
            p.display()
        );
    }

    #[test]
    fn resolve_target_refuses_a_synthetic_deleted_inode_candidate() {
        // `current_exe` cannot be made to return a `" (deleted)"` string on
        // this platform, so this hands `resolve_target` one directly.
        let deleted = PathBuf::from("/opt/shep/shep (deleted)");
        let err = resolve_target([None, Some(deleted)], None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no binary to exec: /opt/shep/shep (deleted) (names a deleted inode, not a file)"
        );
    }

    #[test]
    fn a_deleted_inode_candidate_is_refused_on_every_platform() {
        // The portable half of the Linux-only case, which a macOS run never
        // compiles.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep (deleted)");
        std::fs::write(&path, "an exec target that really is on disk").unwrap();

        assert_eq!(
            check_target(&path),
            Err(TargetProblem::DeletedInode),
            "existing on disk must not excuse the suffix"
        );
    }

    #[test]
    fn a_candidate_that_is_not_on_disk_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            check_target(&dir.path().join("never-written")),
            Err(TargetProblem::Missing)
        );
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(check_target(dir.path()), Err(TargetProblem::NotAFile));
    }

    #[test]
    fn a_real_binary_passes_the_check() {
        assert_eq!(check_target(&std::env::current_exe().unwrap()), Ok(()));
    }

    #[test]
    fn argv0_resolves_against_the_startup_directory() {
        // The harness is invoked by an absolute path, so this proves the
        // argv[0] arm reaches a real file rather than that the join is right.
        let p = launch_path_from_argv().expect("argv[0] names a path");
        assert!(p.is_file(), "{}", p.display());
    }

    /// Names the directory the exec self-test's middle stage works in, and
    /// tells that stage it is not the ordinary run of the test.
    const SELFTEST_HOME: &str = "SHEP_HANDOVER_SELFTEST";

    /// The full path of the self-test, as libtest's `--exact` wants it.
    const SELFTEST_NAME: &str =
        "handover::tests::an_exec_replaces_the_image_and_keeps_a_descriptor";

    /// A blob naming real descriptors: `entry`'s sheep carries `fds`, and
    /// the listener and pidfile numbers are the caller's own open files.
    fn handover_with_fds(
        entry: &ProcessEntry,
        listener_fd: RawFd,
        pidfile_fd: RawFd,
        fds: CarriedFds,
    ) -> Handover {
        Handover {
            version: VERSION,
            sheep: vec![CarriedSheep::from_entry(
                entry, 7, fds, false, None, false, None,
            )],
            listener_fd,
            pidfile_fd,
            next_id: 9,
            next_deadline: 5,
            next_action_stamp: 2,
            reloads: Some(Vec::new()),
        }
    }

    fn selftest_paths(home: &Path) -> ShepPaths {
        let home = home.display().to_string();
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.clone()),
            Path::new("/nonexistent"),
        );
        std::fs::create_dir_all(&paths.run).unwrap();
        paths
    }

    #[test]
    fn an_exec_replaces_the_image_and_keeps_a_descriptor() {
        // Three stages of the same test binary: the ordinary run re-runs this
        // one test in a child, which writes into a pipe and hands over, and
        // the image `hand_over` execs into reads that pipe back by number.
        if let Some(blob) = std::env::var_os(HANDOVER_ENV) {
            successor_stage(Path::new(&blob));
        }
        if let Some(home) = std::env::var_os(SELFTEST_HOME) {
            exec_stage(Path::new(&home));
        }

        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .arg(SELFTEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(SELFTEST_HOME, dir.path())
            .env_remove(HANDOVER_ENV)
            .output()
            .unwrap();

        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // `--exact` against a stale name matches nothing: the child would run
        // zero tests, exit successfully and print no marker.
        assert!(
            stdout.contains("running 1 test"),
            "the child ran no test, so `{SELFTEST_NAME}` is not where this test lives any more;              `--exact` needs the full path and nothing updates it automatically: {stdout}"
        );
        // A pipe written before the exec is readable after it, on the same fd
        // number: the image changed and the descriptor crossed.
        assert!(stdout.contains("adopted: hello"), "{stdout}");
    }

    /// The middle stage: fill a pipe, name its read end in a blob, and hand
    /// over. Returns only if the exec failed, which is a test failure.
    fn exec_stage(home: &Path) -> ! {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;

        let paths = selftest_paths(home);
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"hello").unwrap();
        drop(writer);

        let listener = tempfile::tempfile().unwrap();
        let pidfile = tempfile::tempfile().unwrap();
        let blob = handover_with_fds(
            &entry_fixture(|_| {}),
            listener.as_raw_fd(),
            pidfile.as_raw_fd(),
            CarriedFds {
                out_pipe: Some(reader.as_raw_fd()),
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: None,
                channel: None,
            },
        );

        let err = hand_over(&blob, &paths).unwrap_err();
        panic!("the exec should not have returned: {err}");
    }

    /// The stage after the exec: read the blob this process was pointed at,
    /// and read the descriptor it names.
    fn successor_stage(blob_path: &Path) -> ! {
        let blob = Handover::read(blob_path).expect("the successor's blob");
        let fd = blob.sheep[0].fds.out_pipe.expect("a carried stdout pipe");
        let mut buf = [0_u8; 16];
        let read = nix::unistd::read(fd, &mut buf).expect("the carried descriptor is open");
        println!("adopted: {}", String::from_utf8_lossy(&buf[..read]));
        std::process::exit(0);
    }

    #[test]
    fn a_failed_exec_leaves_no_blob_behind() {
        use std::os::fd::AsRawFd as _;

        let dir = tempfile::tempdir().unwrap();
        let paths = selftest_paths(dir.path());
        let target = dir.path().join("not-a-binary");
        std::fs::write(&target, "this will never execute").unwrap();

        let listener = tempfile::tempfile().unwrap();
        let pidfile = tempfile::tempfile().unwrap();
        let blob = handover_with_fds(
            &entry_fixture(|_| {}),
            listener.as_raw_fd(),
            pidfile.as_raw_fd(),
            CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: None,
                channel: None,
            },
        );

        let err = exec_into(&target, &blob, &paths).unwrap_err();
        assert!(
            !Handover::path(&paths).exists(),
            "a failed exec left a blob behind: {err}"
        );
    }

    /// Both the daemon's own two descriptors and a carried log handle:
    /// `named_fds` yields them from two different places.
    #[test]
    fn a_failed_exec_makes_every_descriptor_close_on_exec_again() {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        let dir = tempfile::tempdir().unwrap();
        let paths = selftest_paths(dir.path());
        let target = dir.path().join("not-a-binary");
        std::fs::write(&target, "this will never execute").unwrap();

        let listener = tempfile::tempfile().unwrap();
        let pidfile = tempfile::tempfile().unwrap();
        let out_log = tempfile::tempfile().unwrap();
        // The stdin write end and the channel's daemon end ride along: a
        // `named_fds` yielding four or five would leave them close-on-exec.
        let (_child_end, stdin) = std::io::pipe().unwrap();
        let (channel, _child_channel) = std::os::unix::net::UnixStream::pair().unwrap();
        let blob = handover_with_fds(
            &entry_fixture(|_| {}),
            listener.as_raw_fd(),
            pidfile.as_raw_fd(),
            CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: Some(out_log.as_raw_fd()),
                err_log: None,
                stdin: Some(stdin.as_raw_fd()),
                channel: Some(channel.as_raw_fd()),
            },
        );

        // The precondition, so the assertion below cannot pass on a
        // descriptor that was never cleared in the first place.
        for fd in [
            listener.as_fd(),
            pidfile.as_fd(),
            out_log.as_fd(),
            stdin.as_fd(),
            channel.as_fd(),
        ] {
            assert!(
                !fds::is_kept(fd).unwrap(),
                "the daemon opens everything close-on-exec, so this starts set"
            );
        }

        // Drives `keep_and_exec` directly so the clear is observable: with
        // `named_fds` yielding nothing, both assertions pass over untouched
        // flags.
        let mut cleared = Vec::new();
        let written = Handover::path(&paths);
        let _ = keep_and_exec(&target, &blob, &written, &mut cleared);
        cleared.sort_unstable();
        let mut expected = vec![
            listener.as_raw_fd(),
            pidfile.as_raw_fd(),
            out_log.as_raw_fd(),
            stdin.as_raw_fd(),
            channel.as_raw_fd(),
        ];
        expected.sort_unstable();
        assert_eq!(
            cleared, expected,
            "every named descriptor must be cleared, from both of the two \
             places `named_fds` draws them"
        );

        let err = exec_into(&target, &blob, &paths).unwrap_err();

        for fd in [
            listener.as_fd(),
            pidfile.as_fd(),
            out_log.as_fd(),
            stdin.as_fd(),
            channel.as_fd(),
        ] {
            assert!(
                !fds::is_kept(fd).unwrap(),
                "a failed exec left a descriptor exec-inheritable: {err}"
            );
        }
    }
}
