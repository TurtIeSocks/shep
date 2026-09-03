//! Whole-flock handover: whether this daemon's flock can be replaced in
//! place, the [`Handover`] blob that describes it, and (in a later task) the
//! exec that carries it.
//!
//! One thing is still refused, and it is permanent rather than deferred: a
//! live sheep whose log pump did not report its descriptors in time is a
//! sheep whose descriptors this daemon does not know, and carrying it would
//! hand the successor a sheep it cannot read. Every refusal that named a
//! FEATURE has gone -- a dog was the last of them, and phase 3 carries one
//! by giving it a connection that re-establishes itself after the exec -- so
//! the gate no longer says anything about how much of the handover has
//! shipped. A
//! sheep's stdout, stderr, log files, stdin pipe and shepherd channel all
//! cross the exec, and every one of those is per SHEEP rather than per app,
//! so an app running several instances crosses as several sets and needs
//! nothing of its own. [`fitness`] is the gate: get it wrong in the permissive
//! direction and a half-built handover corrupts a live flock; get it wrong
//! in the strict direction and the caller merely falls back to the
//! stop-and-start arm that already works. That asymmetry is why an unclear
//! case refuses rather than guesses.

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
    /// Every sheep in the flock is carryable by phase 2a.
    Carryable,
    /// At least one sheep is not carryable, and why.
    Refused(RefusedReason),
}

/// Why a flock cannot be handed over in place, and what happens instead.
///
/// One variant, and it is a fault rather than a feature this daemon has not
/// shipped yet: a sheep's log pump did not answer in time, so nothing here
/// knows which descriptors that sheep holds. The caller still falls back to
/// the stop arm, which is correct behaviour rather than a degraded one.
///
/// **Every other variant this enum has ever had is gone**, each one turned
/// into something the daemon carries -- a shepherd channel, a stdin pipe, a
/// pending delete or stop, a swap in flight, more than one instance, and now
/// a dog. That is why the wording is written as a fault: an operator who
/// reads "cannot yet" about a stuck filesystem waits for a version that is
/// never coming.
///
/// `#[non_exhaustive]`, unlike [`crate::boot::Shepherd`]: that enum is
/// closed by its mechanism (a pidfile lock is either free, held-with-pid or
/// held-without, and there is no fourth state). This one is not closed by
/// anything. A second way for a live sheep's descriptors to be unknowable
/// would join it, and shep-daemon is a published library an out-of-tree
/// matcher should not break for (IR-20).
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
        // The tail is what `shep daemon reload`'s own end-to-end cases probe
        // for to tell a carried flock from a stopped-and-started one, so it
        // is load-bearing text rather than punctuation: see `cli_e2e`'s
        // assertions on "falls back to a stop-and-start".
        match self {
            Self::PumpUnresponsive { sheep } => write!(
                f,
                "sheep '{sheep}' has a log pump that did not report its descriptors in \
                 time; reload falls back to a stop-and-start instead"
            ),
        }
    }
}

/// One sheep's carryability-relevant facts.
///
/// Bundles a [`ProcessEntry`] with the one fact that does not live on it:
/// whether this sheep's log pump answered the snapshot's deadline, which
/// only the task that asked it knows. `fitness` stays a pure function over
/// data it is handed rather than reaching into the registry itself.
///
/// A pending delete, a pending manual stop and a swap in flight all used to
/// live here too. None of them is a refusal any more —
/// [`CarriedSheep::pending_delete`], [`CarriedSheep::manual`] and
/// [`CarriedSheep::reload`] carry them instead — so none of them is a
/// carryability-relevant fact and this view has nothing left to say about
/// any of them.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The sheep's lifecycle entry.
    pub entry: &'a ProcessEntry,
    /// Whether this sheep's log pump was asked for its descriptors and did
    /// not answer in time.
    ///
    /// A third answer rather than a `CarriedFds::none()`, and the
    /// distinction is the whole point: `none()` is what a STOPPED sheep
    /// reports, and a wedged live pump collapsed into it would be carried
    /// with its descriptors silently dropped. So it reaches the gate
    /// here, on the candidate, instead of being folded into the blob.
    pub pump_unresponsive: bool,
}

/// A [`Candidate`] that owns its entry.
///
/// The supervisor cannot lend one. Assembling a snapshot means asking every
/// log pump for its descriptors, and no `.await` on a pump may happen on the
/// actor loop (see `Actor::handle_reopen` for the cycle that rules it out),
/// so the assembly runs on a task of its own and the entries have to travel
/// there. A borrow cannot.
///
/// [`Self::as_candidate`] is how it reaches [`fitness`], which stays a pure
/// function over borrowed data.
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
/// Whole-flock, not per-sheep: the handover blob describes one process
/// image, so a flock is carried whole or refused whole. An empty flock is
/// carryable.
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
/// A successor is not necessarily the same build as the image that wrote the
/// blob, which is the entire point of a handover, so the two agree on a
/// number rather than on a struct layout. [`Handover::load_value`] refuses
/// anything else outright: an image that cannot understand the blob must
/// fail loudly so its caller falls back to the stop arm, never adopt a
/// partial picture of a live flock.
pub const VERSION: u32 = 1;

/// The file name the blob is written under, inside `$SHEP_HOME/run`.
const FILE_NAME: &str = "handover.json";

/// Everything the successor needs to keep supervising a flock it did not
/// spawn.
///
/// Written by the outgoing image just before it `execve`s, read once by the
/// incoming one, and unlinked by that reader (task 6's job, in
/// `handover::adopt`). It is the whole of what crosses the exec besides the
/// descriptors themselves, which it names by number.
///
/// **It carries each sheep's whole resolved spec, environment included.**
/// That is deliberate, and an earlier draft of this module said the
/// opposite, so here is the argument rather than only the conclusion.
///
/// The muster roll already persists every sheep's environment in cleartext,
/// permanently. [`SavedApp::app`](crate::snapshot::SavedApp::app) is a whole
/// `AppConfig`, `AppConfig::env` is a plain `BTreeMap<String, String>` with
/// no skip attribute, and `flock.json` is written at `0600`. That type's own
/// doc even notes that `Debug` redacts env, so the sensitivity was
/// understood and the value persisted anyway. A blob carrying the same
/// values, at the same mode, on a file the successor unlinks the moment it
/// has read it, is strictly less exposure than the file already sitting
/// there for the life of the flock.
///
/// Refusing to carry it bought nothing and cost a great deal. Without a spec
/// the successor has to rebuild one from the roll and bind carried sheep to
/// roll apps by name and instance, except the roll records a running COUNT
/// per app rather than which slots were up, and `muster` starts what it
/// restores. A second source of truth that can disagree with the blob, to
/// protect a value that is already on disk.
///
/// What protects it is what always did: mode `0600` set at creation rather
/// than by a later `chmod`, inside a `0700` directory, unlinked by the
/// successor as soon as it has read it. `Debug` stays derived and stays
/// safe, because `AppConfig`'s own `Debug` prints `env` as a count rather
/// than as pairs; an exact-string test pins that (IR-41), and a second one
/// pins that the serialized form does carry the values, since a successor
/// that silently lost them would respawn an app under an environment it was
/// never started with.
///
/// **`ProcessEntry::started_at` is deliberately absent**, and no serializer
/// for it would help. It is a `tokio::time::Instant`, which has no epoch and
/// means nothing outside the runtime that read it. The successor re-derives
/// each sheep's start time from the operating system, which is authoritative
/// about a pid it did not spawn in a way a carried value could never be.
/// [`uptime::started_at_of`] is that derivation.
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
    /// Carried so the successor can hold the descriptor open, not so it can
    /// re-acquire the lock. `flock` is a property of the open file
    /// description, so the lock crossed the exec already; releasing it to
    /// take it again would open a window for a second daemon to win it.
    pidfile_fd: RawFd,
    /// The supervisor's next entry id.
    ///
    /// The three counters below reset to zero in every constructor, so a
    /// successor that did not carry them would reissue an id, a watchdog
    /// stamp or an action stamp that a caller is still holding.
    next_id: u32,
    /// The supervisor's next reload-watchdog stamp.
    next_deadline: u64,
    /// The supervisor's next action-wait stamp.
    next_action_stamp: u64,
    /// Every app whose reload was still in flight at the exec.
    ///
    /// `Option` for the reason [`CarriedSheep::pending_delete`] gives at
    /// length: a predecessor from before this field existed refused to carry
    /// a flock with any reload in flight, so a blob it wrote never has the
    /// key, and an absent field loads as `None` rather than failing to
    /// parse. `None` is what that blob truthfully means — no app was
    /// mid-reload — so [`VERSION`] stays unmoved.
    ///
    /// Sorted by app name by its writer, since [`HashMap`](std::collections::HashMap)
    /// iteration order is arbitrary and the blob is a file an operator may
    /// read.
    reloads: Option<Vec<CarriedReload>>,
}

impl Handover {
    /// Describe a flock for the successor.
    ///
    /// Every argument comes from a different owner, which is why none of
    /// them is read in here: `sheep` is assembled from the supervisor's
    /// slots and its pumps, `fds` from `boot`, and `counters` from the
    /// actor. Nothing in this crate can see all three.
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
    ///
    /// Read by this crate's own tests and by `adopt`, which reaches the
    /// field directly from inside the module; `allow` rather than `expect`
    /// because the expectation would go unfulfilled in a test build.
    #[must_use]
    #[allow(dead_code, reason = "read by this crate's own tests")]
    pub fn sheep(&self) -> &[CarriedSheep] {
        &self.sheep
    }

    /// The entry id the successor is to issue next.
    ///
    /// [`Self::counters`] is what a successor restores from; this is the one
    /// counter a test asserts on its own.
    #[must_use]
    #[allow(dead_code, reason = "read by this crate's own tests")]
    pub const fn next_id(&self) -> u32 {
        self.next_id
    }

    /// The three counters the successor restores before installing any
    /// sheep.
    ///
    /// All three together rather than one accessor each: they are restored
    /// in one act, and a successor that carried two of them would reissue a
    /// live stamp of the third.
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
    /// Empty both for a flock with nothing mid-reload and for a blob written
    /// before this daemon carried one at all, which say the same thing: see
    /// the field's own doc.
    #[must_use]
    pub fn reloads(&self) -> &[CarriedReload] {
        self.reloads.as_deref().unwrap_or_default()
    }

    /// Where the blob lives under `paths`: `$SHEP_HOME/run/handover.json`.
    #[must_use]
    pub fn path(paths: &ShepPaths) -> PathBuf {
        paths.run.join(FILE_NAME)
    }

    /// Every descriptor number this blob names, listener and pidfile
    /// first.
    ///
    /// This is the exact set [`hand_over`] clears `FD_CLOEXEC` on, and
    /// nothing else may be added to it: a descriptor kept without being
    /// named leaks into the successor's image, which is the mirror of the
    /// failure this module exists to avoid.
    fn named_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        [self.listener_fd, self.pidfile_fd].into_iter().chain(
            self.sheep
                .iter()
                .flat_map(|sheep| sheep.fds.all().into_iter().flatten()),
        )
    }

    /// Write the blob under `paths`, at mode `0600`, and return where it
    /// went.
    ///
    /// The mode is set at creation rather than with a `chmod` afterwards.
    /// A `chmod` leaves a window in which the file is world-readable, and it
    /// names this host's pids and descriptor numbers.
    ///
    /// Any leftover blob is removed first, because
    /// [`OpenOptions::mode`](OpenOptionsExt::mode) is honoured only when the
    /// open actually creates the file: reopening a stale one would inherit
    /// whatever mode it already carried.
    ///
    /// # Errors
    ///
    /// The leftover blob could not be removed, the new one could not be
    /// created (including because something else created it in between), or
    /// serializing to it failed.
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
    /// Reading does not unlink: the successor does that once it has adopted
    /// what the blob describes, so a failure here leaves the file for an
    /// operator to look at.
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
    /// The version is read off the raw JSON before anything else, so a blob
    /// from a format this image does not know is refused by number rather
    /// than by whichever field happens to fail to deserialize first.
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
/// picture of the flock, or only part of one, and supervising a flock it
/// half understands is worse than refusing.
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

/// The daemon's own two descriptors, as the party that opened them knows
/// them.
///
/// A pair rather than two arguments because both are a bare `RawFd`: passing
/// them positionally lets a caller swap them silently, and the swap is not
/// detectable afterwards — the successor would `flock` its control socket
/// and listen on its pidfile. This is the same argument [`CarriedFds`] makes
/// for grouping its own.
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
/// action stamp that a caller is still holding.
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
/// Two halves. [`Self::app`] is what the sheep IS, carried whole so the
/// successor can respawn this exact instance without asking the muster roll
/// what it was. Every other field is what this instance is currently DOING,
/// none of which any config could answer.
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
    /// Carried resolved, never re-derived. The value is pinned at the first
    /// spawn precisely so a later change to the passwd database cannot move
    /// a running app's identity underneath it; looking the name up again in
    /// the successor would reintroduce exactly the re-lookup the pinning
    /// exists to prevent.
    credentials: SpawnIdentity,
    /// The descriptor numbers this instance's output travels on.
    fds: CarriedFds,
    /// Whether an operator's `delete` targeted this instance before the
    /// exec.
    ///
    /// `Option` rather than `bool` for the reason [`CarriedFds::stdin`]
    /// gives at length: a predecessor from before this field existed
    /// refused to carry a sheep with a delete pending at all, so a blob it
    /// wrote never has the key, and an absent field loads as `None` rather
    /// than failing to parse. `None` is what that blob truthfully means —
    /// "no" — not "unknown", so [`VERSION`] stays unmoved: nothing an older
    /// reader must understand has changed.
    pending_delete: Option<bool>,
    /// The manual command that owned this instance's next exit before the
    /// exec, and who asked for it.
    ///
    /// One `Option`, not the two [`Self::pending_delete`] needs, and the
    /// difference is what "absent" means rather than a style choice. A
    /// missing key loads as `None`, and `None` is already this field's own
    /// word for "no command owns this exit" — the same thing a predecessor
    /// that refused to carry a marker at all was saying. There is no third
    /// state to distinguish, so [`VERSION`] stays unmoved here for the same
    /// reason it did there.
    ///
    /// Nothing on it is sensitive: two closed enums naming which verb and
    /// whether a person or the daemon raised it (IR-41). The blob's
    /// `AppConfig` already carries the app's whole environment, and this
    /// adds nothing to what a reader of that file can see.
    manual: Option<PendingManual>,
    /// Which half of a reload's swap this instance is, if either.
    ///
    /// `Option` for the reason [`Self::pending_delete`] gives: a predecessor
    /// from before this field existed refused to carry a sheep mid-swap at
    /// all, so a blob it wrote never has the key, and `None` is what that
    /// blob truthfully means — [`ReloadState::None`] — rather than
    /// "unknown". [`VERSION`] stays unmoved.
    ///
    /// It is the marker that ROUTES this instance's next exit. A successor
    /// that dropped it would send a drainee's exit to `decide_on_exit`
    /// instead of to the reload machinery, which for an `autorestart` app
    /// respawns the old code into a slot the replacement owns.
    reload: Option<ReloadState>,
    /// Whether a reload's readiness verification has already failed against
    /// this instance.
    ///
    /// `Option` for the same reason the three fields above are, but NOT for
    /// the same argument, and the difference is worth stating rather than
    /// borrowing. Each of those was a gate refusal before it was a field, so
    /// an absent key proves the fact was false. This was never a refusal: a
    /// predecessor from before this field existed carried such an instance
    /// happily and simply dropped the flag, so an absent key is that
    /// predecessor staying silent rather than saying "no". `false` is still
    /// the right reading of the silence — it is exactly what a successor
    /// assumed before the field existed, so an older blob adopts the way it
    /// always did — and it is the only reading available, since a hard parse
    /// failure would leave a successor refusing to boot after its
    /// predecessor had exec'd itself away. [`VERSION`] stays unmoved.
    ///
    /// It is what keeps a failed reload's leftovers REACHABLE. A reload
    /// replaces `Online` instances, and an abandoned one is deliberately
    /// left `Starting` so the daemon does not report a release that never
    /// served as serving; `reload_eligible` reads this flag beside the
    /// status, so the reload that rolls the release back can still reach the
    /// instance. A successor that dropped it would answer that rollback with
    /// `Ok` and replace nothing.
    ready_failed: Option<bool>,
    /// When this instance's owed respawn falls due, in wall-clock terms, or
    /// `None` for an instance that is not owed one at all.
    ///
    /// The one thing on this row that is deliberately NOT a monotonic value,
    /// and the reason is the `execve`. A [`tokio::time::Instant`] has no
    /// epoch and means nothing outside the runtime that read it -- which is
    /// exactly why `started_at` cannot be carried and is re-derived from the
    /// process table instead (see [`uptime`](super::uptime)). A wall-clock
    /// moment has an epoch, so it crosses the exec intact, and the successor
    /// can re-arm for what is LEFT of the delay rather than for the whole of
    /// it. An app with `restart_delay = "1h"` reloaded at minute 59 waits the
    /// remaining minute; a flock reloaded four times inside that hour still
    /// comes back at the original moment, because an absolute deadline
    /// absorbs each handover's own duration where a carried remainder would
    /// add it back on every hop.
    ///
    /// The cost is a clock that can move under it, where a monotonic one
    /// cannot: an NTP correction or a suspend changes when a down sheep
    /// comes back. That is bounded rather than open-ended --
    /// [`adopted_restart_delay`](crate::backoff::adopted_restart_delay)
    /// clamps the remainder to the app's own configured delay, so a backward
    /// jump can at worst give back the behaviour that shipped in v0.1.20 --
    /// and it corrupts nothing: a restart delay is pacing, not state.
    ///
    /// `Option` for the reason [`Self::ready_failed`] gives rather than the
    /// one the three fields above it give, and it is the same distinction.
    /// Each of those was a gate refusal before it was a field, so an absent
    /// key proves the fact was false. This was never a refusal: a
    /// predecessor from before this field existed carried a
    /// [`ProcStatus::WaitingRestart`] sheep happily and re-armed the whole
    /// delay on the far side. So an absent key is that predecessor staying
    /// silent, and the successor falls back to exactly what it did.
    /// [`VERSION`] stays unmoved: nothing an older reader must understand
    /// has changed, and a hard parse failure would leave a successor
    /// refusing to boot after its predecessor had exec'd itself away.
    ///
    /// Written only for a row this blob reports as `WaitingRestart` -- see
    /// [`Self::from_entry`], which gates it -- so it never appears on a row
    /// an operator reading the blob would be surprised to find it on.
    ///
    /// Nothing on it is sensitive: one timestamp (IR-41).
    restart_due: Option<SystemTime>,
    /// Where this instance's binary came from, for an instance that is a
    /// dog, or `None` for an ordinary sheep.
    ///
    /// `Option` for the reason [`Self::pending_delete`] gives: a
    /// predecessor from before this field existed refused to carry a dog at
    /// all, so a blob it wrote never has the key, and `None` is what that
    /// blob truthfully means -- "not a dog" -- rather than "unknown".
    /// [`VERSION`] stays unmoved: nothing an older reader must understand
    /// has changed. Serde's derive already lets an `Option` field be
    /// absent, so there is no `#[serde(default)]`, for the reason
    /// [`CarriedFds::stdin`] gives at length.
    ///
    /// **The whole [`DogSource`], not a boolean**, because `shep dogs`
    /// reports where each dog's binary came from and a successor holding
    /// only "yes" would have to guess that column.
    ///
    /// Losing it is not cosmetic, and none of what it costs is visible to a
    /// pid check. `Actor::matching_ids` keeps a dog out of every selector
    /// but an exact one, so an unmarked dog joins `shep flock` beside the
    /// operator's own apps, leaves `shep dogs`, and starts answering to
    /// `shep restart all`. `dogs::spawn_dog_watch` records an exhausted
    /// restart budget only for a marked entry, so an unmarked dog that
    /// burned its whole budget writes nothing to `barks.jsonl` -- the alert
    /// an operator reads after an outage. And `rpc::dog_staleness` builds
    /// its roster from the marker, so a reload could not tell a dog that had
    /// not dialled back yet from one that had.
    ///
    /// Nothing on it is sensitive: a closed enum naming this binary or a
    /// path the operator typed into `shep adopt`, which the blob's own
    /// `AppConfig` already carries as the program to run (IR-41).
    dog: Option<DogSource>,
    /// The config a load parked for this instance's next spawn, or `None`
    /// for an instance nothing is owed.
    ///
    /// An `AppConfig` and not a `ResolvedApp`, for the reason [`Self::app`]
    /// gives at length: that type is a proof token with no `Deserialize`, so
    /// the successor rebuilds it by normalizing this again. The two travel
    /// as the same shape because they are the same kind of thing, one config
    /// the instance is running and one it is owed.
    ///
    /// One `Option`, not the two [`Self::pending_delete`] needs, for the
    /// reason [`Self::manual`] gives: a missing key loads as `None`, and
    /// `None` is already this field's own word for "nothing is owed" -- the
    /// same thing a predecessor from before the parking slot existed was
    /// saying, since it had no way to park anything. There is no third state
    /// to distinguish, so [`VERSION`] stays unmoved.
    ///
    /// Losing it is silent config erasure, which is why it is carried at
    /// all. An operator loads a Flockfile, sees fields reported pending,
    /// then runs `shep daemon reload` to pick up a new binary: the `Live`
    /// fields are on [`Self::app`] and survive, and every parked change
    /// would vanish with nothing anywhere recording that it was ever owed.
    /// The next load would compare against a spec that already matched and
    /// report nothing to do.
    ///
    /// It carries the app's whole environment, exactly as [`Self::app`]
    /// does, and the blob is written with the same permissions for the same
    /// reason (IR-41). It adds no class of secret that file did not already
    /// hold.
    pending: Option<AppConfig>,
    /// Whether promoting [`Self::pending`] must re-resolve
    /// [`Self::credentials`], or `None` for a blob written before this field
    /// existed.
    ///
    /// `Option` for the reason [`Self::ready_failed`] gives rather than the
    /// one [`Self::pending_delete`] gives, and it is the same distinction: a
    /// predecessor from before the field existed was not refusing anything,
    /// it simply had nothing to say. `false` is the right reading of that
    /// silence, because such a predecessor also had no [`Self::pending`] to
    /// go with it. [`VERSION`] stays unmoved.
    ///
    /// **Carried with [`Self::pending`] and never without it.** The decision
    /// is made by the load that parks the config, against the spec that
    /// entry held at that moment, precisely because it cannot be recomputed
    /// later from a diff -- a load derives one spec from an app's first
    /// instance and writes it onto every sibling, so a `user` change a
    /// sibling has not applied stops being visible as a difference. A parked
    /// config arriving without its flag would promote on the identity it was
    /// supposed to replace, which is that same defect reintroduced through
    /// this file. See `ProcessEntry::pending_reidentifies` (crate-internal,
    /// hence code font) for the full argument.
    ///
    /// Nothing on it is sensitive: one boolean saying whether a name is due
    /// a passwd lookup, never the name (IR-41).
    pending_reidentifies: Option<bool>,
    /// The resolved config this instance runs under, environment included.
    ///
    /// This is the `AppConfig` beneath [`ProcessEntry::spec`]'s
    /// `ResolvedApp`, not the `ResolvedApp` itself. That type is a proof
    /// token, obtainable only by passing a config through `normalize`, so
    /// deriving `Deserialize` for it would mint the token out of arbitrary
    /// JSON for every consumer of shep-core, which is a far wider change
    /// than a handover needs. The value here has already been through
    /// `normalize`, and `normalize` is pure over one of its own outputs: it
    /// reads no filesystem, and a `~/` it expanded no longer begins with
    /// `~`. So the successor rebuilds the token by normalizing this again.
    ///
    /// The roll persists an app the same way for the same reason, and doing
    /// it differently here would be a second shape for one job:
    /// [`SavedApp::app`](crate::snapshot::SavedApp::app) is an `AppConfig`
    /// and `snapshot.rs` re-normalizes it on restore.
    ///
    /// One residual, recorded because it is real and not introduced here. A
    /// successor whose `normalize` has tightened can refuse a config its
    /// predecessor accepted, and after the exec there is no stop arm left to
    /// fall back to. Carrying the resolved token instead would not buy the
    /// escape it looks like it would: the alternative to carrying a spec at
    /// all is the roll, which re-normalizes this identical `AppConfig` and
    /// meets the identical refusal, with a second source of truth on top.
    ///
    /// Its serialized shape is part of this blob's version 1 format, as
    /// [`SpawnIdentity`]'s is.
    app: AppConfig,
}

impl CarriedSheep {
    /// Describe `entry` for the successor.
    ///
    /// `epoch`, `fds`, `pending_delete`, `manual`, `ready_failed` and
    /// `restart_due` are arguments rather than reads off `entry` because
    /// none of them lives there: the respawn epoch, the pending-delete
    /// marker, the manual-command marker, the failed-readiness verdict and
    /// the respawn deadline are on the supervisor's private slot type, and
    /// the descriptor numbers are known only to whichever code holds the
    /// open descriptors. This is the same split [`Candidate`] makes, for the
    /// same reason. [`Self::reload`] and [`Self::dog`] are the two that do
    /// live on `entry`, so they are read rather than passed.
    ///
    /// `restart_due` is the one argument this does not carry verbatim: it is
    /// gated on the entry's status, for the reason
    /// [`Self::restart_due`]'s own doc gives.
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
            // Read off the entry rather than passed in, unlike the markers
            // either side of it: these two do live on `ProcessEntry`.
            reload: Some(entry.reload),
            dog: entry.dog.clone(),
            // Read off the entry for the reason `reload` and `dog` above
            // are, and written as a pair for the reason
            // `Self::pending_reidentifies` gives: a config without its flag
            // promotes on the wrong identity.
            pending: entry.pending.as_ref().map(|parked| parked.config().clone()),
            pending_reidentifies: Some(entry.pending_reidentifies),
            ready_failed: Some(ready_failed),
            // Gated on the status rather than carried verbatim, unlike every
            // marker above. The slot's own field is written on the one
            // transition INTO `WaitingRestart` and never cleared -- the two
            // ways out of that status (a respawn, and an operator's `stop`
            // on a waiting sheep) both land somewhere this value has no
            // meaning -- so gating here is what stops a moment that expired
            // three restarts ago riding out on an `Online` row, where the
            // successor would ignore it and an operator reading the blob
            // would not.
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
    /// exec, or `None` for a blob written before this field existed — which
    /// truthfully means "no", since that predecessor refused to carry a
    /// pending delete at all. See the field's own doc.
    #[must_use]
    pub const fn pending_delete(&self) -> Option<bool> {
        self.pending_delete
    }

    /// The manual command that owned this instance's next exit before the
    /// exec, or `None` for an instance no command was waiting on — which is
    /// also what a blob written before this field existed says, and
    /// truthfully, since that predecessor refused to carry a marker at all.
    /// See the field's own doc.
    #[must_use]
    pub const fn manual(&self) -> Option<PendingManual> {
        self.manual
    }

    /// Which half of a reload's swap this instance is, or `None` for a blob
    /// written before this field existed — which truthfully means
    /// [`ReloadState::None`], since that predecessor refused to carry a
    /// sheep mid-swap at all. See the field's own doc.
    #[must_use]
    pub const fn reload(&self) -> Option<ReloadState> {
        self.reload
    }

    /// Where this instance's binary came from if it is a dog, or `None` for
    /// an ordinary sheep — which is also what a blob written before this
    /// field existed says, and truthfully, since that predecessor refused to
    /// carry a dog at all. See the field's own doc for what losing it costs.
    ///
    /// Borrowed rather than cloned: [`DogSource::Adopted`] owns a path, and
    /// every caller either reads it or clones it for an entry of its own.
    #[must_use]
    pub const fn dog(&self) -> Option<&DogSource> {
        self.dog.as_ref()
    }

    /// The config a load parked for this instance's next spawn, or `None`
    /// for an instance nothing is owed -- which is also what a blob written
    /// before this field existed says, and truthfully, since its predecessor
    /// had no parking slot to fill. See the field's own doc.
    ///
    /// Borrowed rather than cloned: the one caller normalizes it into a
    /// [`ResolvedApp`] of its own.
    #[must_use]
    pub const fn pending(&self) -> Option<&AppConfig> {
        self.pending.as_ref()
    }

    /// Whether promoting [`Self::pending`] must re-resolve the identity, or
    /// `None` for a blob written before this field existed. That `None`
    /// reads as `false`, which is not the claim [`Self::pending`]'s own
    /// getter makes about its own; the field's doc says why, and why the two
    /// are never carried apart.
    #[must_use]
    pub const fn pending_reidentifies(&self) -> Option<bool> {
        self.pending_reidentifies
    }

    /// Whether a reload's readiness verification has already failed against
    /// this instance, or `None` for a blob written before this field
    /// existed. That `None` reads as `false` — which is not the same claim
    /// the three getters above make about their own, and the field's own doc
    /// says why.
    #[must_use]
    pub const fn ready_failed(&self) -> Option<bool> {
        self.ready_failed
    }

    /// When this instance's owed respawn falls due, or `None` for one that
    /// is not owed a respawn -- and for a blob written before this field
    /// existed, whose predecessor re-armed the whole delay instead. See the
    /// field's own doc for why those two read the same and what the
    /// successor does about it.
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
    /// The one question a successor asks before adopting anything: there is
    /// no process to take over for an instance that has none, and its
    /// [`Self::fds`] are [`CarriedFds::none`] for the same reason.
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
    /// the predecessor and never looked up again.
    #[must_use]
    pub const fn credentials(&self) -> SpawnIdentity {
        self.credentials
    }

    /// The config this instance runs under, as its predecessor normalized it.
    ///
    /// Not a [`ResolvedApp`](shep_core::config::ResolvedApp): see the field's
    /// own doc for why the proof token is rebuilt by re-normalizing this
    /// rather than carried, and why re-normalizing is a no-op that hands the
    /// token back.
    #[must_use]
    pub const fn app(&self) -> &AppConfig {
        &self.app
    }
}

/// The descriptor numbers one sheep's output travels on, the one its input
/// travels back through, and the one that carries both directions of its
/// shepherd channel.
///
/// Grouped rather than spelled as six fields on [`CarriedSheep`] for two
/// reasons: they are all `Option<RawFd>`, so a constructor taking them
/// positionally would let a caller swap two of them silently, and four of
/// them share one fact, which is that a running instance has all four and a
/// registered one that is not running has none.
///
/// `None` on those four is that second case and only that case. A blob
/// naming a descriptor that is not open in the successor is a failure to
/// refuse, not a `None` to tolerate: losing a sheep's stdout read end does
/// not lose its output, it blocks the child on `write()` once the 64KiB
/// pipe buffer fills, which reads as an application hang rather than as a
/// shep bug.
///
/// [`Self::stdin`] and [`Self::channel`] are the exceptions on both counts.
/// Each is present only for a running sheep whose app asked for it, and
/// neither is a read end the daemon drains one way: stdin is a WRITE end,
/// and the channel is one socket the daemon reads and writes at once.
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
    /// `/dev/null` on fd 0 and nothing for the daemon to hold, as well as
    /// for a sheep that is not running.
    ///
    /// # Why an absent field is `None` rather than a refusal to load
    ///
    /// The predecessor in a handover is by definition a different build,
    /// and one from before this field existed refused to carry a sheep with
    /// a stdin pipe at all. So its blobs describe sheep that genuinely had
    /// no such descriptor, which is what `None` says. Parsing them as an
    /// error instead would leave the successor refusing to boot after the
    /// predecessor had already exec'd itself away, and a whole flock
    /// unsupervised is a steep price for one added field. [`VERSION`] is
    /// unmoved for the same reason: nothing an older reader must understand
    /// has changed.
    ///
    /// Serde's derive already lets an `Option` field be absent, so there is
    /// no `#[serde(default)]` here: adding one changes nothing, and a
    /// reader would reasonably take it as meaning that without it the field
    /// is required. What holds the behaviour still is
    /// `a_blob_written_before_stdin_was_carried_still_loads`, which loads a
    /// blob with the key removed.
    pub stdin: Option<RawFd>,
    /// The daemon's end of the sheep's shepherd-channel socketpair, whose
    /// other end is the child's fd 3.
    ///
    /// One number for both directions, unlike every other field here: a
    /// socketpair is one descriptor that is read and written at the same
    /// time, so `spawn_channel_pumps` splits it into two tasks over one
    /// open file description rather than into two descriptors.
    ///
    /// `None` for a sheep whose app set none of `channel`, `wait_ready` or
    /// `shutdown_with_message` (`assemble` folds all three into one flag),
    /// for a sheep that is not running, and for one whose child has closed
    /// its fd 3 and left the daemon's writer nothing to write to. The last
    /// of those is why the snapshot masks this field rather than reporting
    /// whatever number a pump last saw: see `SheepSlot::open_channel`.
    ///
    /// An absent field loads as `None` for the reason [`Self::stdin`] gives
    /// at length, and [`VERSION`] is unmoved for the same reason. A
    /// predecessor from before this field existed refused to carry a sheep
    /// with a channel at all, so `None` is what its blobs truthfully mean.
    pub channel: Option<RawFd>,
}

/// Which of a sheep's six descriptors a number is.
///
/// Not a description of the object — that is what the adoption itself
/// discovers — but of the SLOT, which is what decides which adoption runs.
/// A stdout pipe and a stdin pipe are both pipes and are refused by opposite
/// checks, so the slot is the part a caller has to carry with the number.
///
/// `Debug` is derived and carries nothing: six unit variants, no descriptor
/// number, no path, no environment (IR-41).
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
    /// The WRITE end of its stdin pipe ([`CarriedFds::stdin`]).
    Stdin,
    /// The daemon's end of its shepherd channel ([`CarriedFds::channel`]).
    Channel,
}

impl SheepFd {
    /// What this slot is called in a refusal, in the wording the adoption
    /// functions already use.
    ///
    /// Kept beside the variants rather than at the one call site so the two
    /// cannot drift: `adopt_pipe` says "stdout pipe" and this has to say the
    /// same, or an operator reads one name from a rehearsal and a different
    /// one from the successor for the identical descriptor.
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
    /// The six numbers, listener-order irrelevant but fixed: stdout's pipe,
    /// stderr's pipe, stdout's log, stderr's log, stdin's pipe, the
    /// shepherd channel.
    ///
    /// One array rather than six field reads, so a caller that walks all of
    /// them — clearing `FD_CLOEXEC`, checking each is open — cannot walk
    /// five by mistake.
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

    /// [`Self::all`], with each number labelled by which of the six it is,
    /// and so by the adoption a successor will attempt on it.
    ///
    /// The pairing lives here rather than at either caller because there are
    /// two callers and they must not disagree. [`adopt::adopt`] runs these
    /// adoptions for real in the successor; [`adopt::dry_run`] runs the same
    /// ones in the PREDECESSOR, against duplicates, so that a blob the
    /// successor would refuse never reaches an `execve`. A number rehearsed
    /// as the wrong kind is a rehearsal that passes and a boot that still
    /// fails, which is worse than not rehearsing at all.
    ///
    /// Three things hold the two in step, and none of them is a promise to
    /// remember. This array is the only pairing either side reads;
    /// `every_carried_number_is_kinded_in_the_same_order` pins it against
    /// [`Self::all`], which is what the `FD_CLOEXEC` sweep already walks; and
    /// a seventh descriptor changes that function's return type, so this one
    /// stops compiling until it grows a seventh entry too.
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
    ///
    /// Not a failure, and [`fitness`] does not refuse it. A stopped sheep
    /// has no pump, so it has nothing to carry and nothing to lose.
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
/// against the startup directory. Set once by [`record_launch_path`], and
/// read only by [`exec_target`], whose doc carries the argument for why a
/// recorded path beats asking the kernel later.
///
/// The inner `Option` is the "recorded, and there was nothing usable to
/// record" case, which must not be confused with "never recorded": both
/// fall through to the same fallback, but only the second is a bug in
/// whoever forgot the call.
static LAUNCH_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Record how this process was invoked, for a later [`exec_target`].
///
/// Call this once, as early in the daemon's life as there is anywhere to
/// call it from: it reads `argv[0]` and the current directory, and the
/// second of those is only the startup directory for as long as nothing has
/// moved it. The first call wins; a later one is ignored, so a test or a
/// second boot in the same process cannot overwrite the real launch path.
///
/// This is process-global state rather than a field on [`RunningDaemon`]
/// because [`exec_target`] takes no arguments and is called from the exec
/// path, which holds no daemon context by then.
///
/// [`RunningDaemon`]: crate::boot::RunningDaemon
pub fn record_launch_path() {
    let _ = LAUNCH_PATH.set(launch_path_from_argv());
}

/// The binary to `execv` for a handover, and never the running image.
///
/// # Why this does not use [`std::env::current_exe`]
///
/// Everywhere else in this workspace resolves its own binary that way, and
/// here it is wrong. `current_exe` answers "which image am I running?",
/// while a handover needs "which file holds the version an operator just
/// installed?". Those are the same path only until somebody upgrades, which
/// is the one moment this function exists for.
///
/// On Linux `current_exe` reads `/proc/self/exe`, a symlink to the *inode*
/// this process was executed from rather than to a path. `cargo install`
/// and every package manager replace a binary by renaming a new file over
/// it, which leaves the old inode unlinked and still open, so the readlink
/// comes back as `"<path> (deleted)"`. Exec'ing that string fails, and
/// stripping the suffix is a guess about text the kernel does not promise:
/// a path may legitimately end that way. So a handover using `current_exe`
/// on Linux cannot upgrade, which is the whole feature.
///
/// On macOS the same sequence returns a clean path that holds the NEW
/// image, so the naive version passes every local test. That is worse than
/// failing, not better, and it is why this function is written the way it
/// is. Do not simplify it back.
///
/// So: prefer the path this process was launched from, recorded by
/// [`record_launch_path`] before anything could move the current directory,
/// and fall back to `current_exe` only when that is unusable. Both arms go
/// through [`check_target`], because a fallback that skips validation is
/// the same bug with an extra step.
///
/// A bare `argv[0]` with no separator in it (a `PATH` lookup, so `shep
/// daemon` typed by hand rather than the CLI's own spawn, which passes an
/// absolute path) is not resolvable from `argv[0]` alone and is left to the
/// `current_exe` arm.
///
/// # Errors
/// - [`io::ErrorKind::NotFound`] if neither candidate is a file on disk
///   that is safe to exec. The message names what each one was, and what
///   was wrong with it. The caller falls back to the stop-and-start arm, which
///   restarts the flock but does reach the new binary.
pub fn exec_target() -> io::Result<PathBuf> {
    let recorded = LAUNCH_PATH.get().cloned().flatten();
    let current = std::env::current_exe();
    resolve_target(
        [recorded, current.as_deref().ok().map(Path::to_path_buf)],
        current.as_ref().err(),
    )
}

/// The validated-candidate loop [`exec_target`] runs, pulled out so a test
/// can hand it a synthetic `" (deleted)"` path directly rather than needing
/// a real Linux inode-unlink to produce one — `current_exe` cannot be made
/// to return that string on any other platform, or by any safe means at
/// all.
///
/// `pub(crate)` rather than private: [`crate::dogs::dog_app`] resolves a
/// built-in dog's own program through [`exec_target`] for the same reason a
/// handover does (see that module's `builtin_program` for the argument),
/// and its tests use this seam the same way `exec_target`'s own tests do.
///
/// Returns the first candidate in `candidates` that [`check_target`]
/// accepts, in order; a `None` entry is skipped (the "nothing recorded
/// yet" case). `current_exe_error` is folded into the diagnostic only —
/// a `current_exe()` failure is not itself a path to validate.
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
/// does exist. A file genuinely named that way is vanishingly rare, a
/// handover of it merely falls back to the stop arm, and the alternative is
/// exec'ing an old image while reporting an upgrade.
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
/// `argv[0]`, which is what `launch_command` gives the daemon it spawns,
/// passes through the join unchanged.
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
/// Its presence is also the successor's only marker that it is one: an
/// image started any other way has no blob to read and boots normally.
/// Naming a descriptor-carrying thing in the environment follows the
/// `SHEP_CHANNEL_FD` precedent this daemon already sets for a sheep's
/// shepherd channel.
pub const HANDOVER_ENV: &str = "SHEP_HANDOVER";

/// Replace this process with a fresh copy of the shep binary, handing it
/// `blob`'s flock.
///
/// Returns [`Infallible`] rather than `()` so that a call site reads as
/// what it is: on success there is no successor statement, because there is
/// no successor image running this code. Only the error arm returns.
///
/// The order is the whole of this function, and getting it wrong loses a
/// flock:
///
/// 1. write the blob, so the successor has something to read before there
///    is any chance of it existing;
/// 2. clear `FD_CLOEXEC` on every descriptor the blob names, and only
///    those, since a descriptor kept without being named leaks into the new
///    image;
/// 3. `execv` the binary [`exec_target`] resolves, which is deliberately
///    not this running image.
///
/// If the exec fails, the blob on disk is a lie: it describes a handover
/// that never happened, and the next boot would adopt a picture of a
/// process image that does not exist. It is removed before the error is
/// returned, and the caller falls back to the stop-and-start arm.
///
/// The target is resolved before anything is written, so the one failure
/// that needs no cleanup does not get any.
///
/// Nothing this process installed on a signal survives. `execve` resets
/// every disposition that names a handler back to `SIG_DFL`, so tokio's
/// `SIGCHLD` handling and this daemon's own installer both go, and the
/// successor installs them again. That is the design, not a loss to work
/// around.
///
/// # Errors
///
/// No binary is safe to exec (see [`exec_target`]), the blob could not be
/// written, a descriptor it names is not open, or the exec itself failed.
/// Every one of those returns with no blob left on disk, and with
/// `FD_CLOEXEC` back on every descriptor the attempt cleared it from, so the
/// caller's fallback to a graceful stop leaves nothing exec-inheritable
/// behind it.
pub fn hand_over(blob: &Handover, paths: &ShepPaths) -> io::Result<Infallible> {
    exec_into(&exec_target()?, blob, paths)
}

/// [`hand_over`], against a caller-chosen binary.
///
/// Split out so a test can point the exec at something that cannot run and
/// watch the blob be cleaned up; production has exactly one target and
/// [`exec_target`] chooses it.
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
/// holds an interior NUL, or the exec failed. On any of them this process
/// is still itself and `written` is still on disk, which is what
/// [`exec_into`] cleans up.
fn exec_with_blob(target: &Path, blob: &Handover, written: &Path) -> io::Result<Infallible> {
    let mut cleared = Vec::new();
    let failure = match keep_and_exec(target, blob, written, &mut cleared) {
        Ok(never) => match never {},
        Err(err) => err,
    };
    // Every descriptor put back the way it was found. Without this the
    // daemon returns to the graceful-stop fallback with the listener, the
    // pidfile and every carried log descriptor still exec-inheritable, and
    // the supervisor is still running: a restart or a queued `Start` in the
    // window before teardown spawns a sheep that inherits them, and a child
    // holding the pidfile keeps this home claimed after the daemon it
    // belonged to has gone.
    //
    // Failures ignored, one descriptor at a time, because there is nothing
    // better to do with them: the error being carried out of here is the one
    // that matters, and a descriptor whose `fcntl` fails now is one that was
    // already not what the blob said it was.
    for fd in cleared {
        let _ = fds::close_raw_after_exec(fd);
    }
    Err(failure)
}

/// [`exec_with_blob`]'s body, recording what it cleared as it goes.
///
/// Split out so the caller can restore on every failure path with one piece
/// of cleanup rather than one per `?`. `cleared` is pushed to only after a
/// clear succeeds, so it never names a descriptor this process did not
/// change.
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

    // `execve` rather than `execv`: `execv` inherits this process's
    // `environ`, so pointing the successor at the blob would mean
    // `std::env::set_var`, which is unsafe in edition 2024 and unsound in a
    // process with as many threads as this one. Handing the environment
    // over explicitly needs neither.
    nix::unistd::execve(&path, &argv, &env).map_err(io::Error::from)
}

/// This process's environment, with [`HANDOVER_ENV`] set to `written`.
///
/// Any inherited value of that variable is dropped rather than kept: a
/// successor that adopts a blob must adopt the one its predecessor just
/// wrote, and a stale entry from an earlier handover would name a file that
/// has already been read and unlinked.
///
/// # Errors
///
/// A name or value holds an interior NUL, which no environment this process
/// was given can.
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
    /// in-flight reload. Every field a real spawn would set is
    /// present so a future field this gate should read cannot be silently
    /// left at a `Default` that hides a bug.
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

    /// The same entry as a candidate whose log pump missed the snapshot's
    /// deadline, which is the one thing left that refuses a flock.
    ///
    /// Every case below that needs A refusal rather than a particular one
    /// goes through here. Three of them used to reach for a feature the
    /// daemon had not shipped yet -- a shepherd channel, then two instances,
    /// then a dog -- and moved each time that feature landed. There is
    /// nothing left for them to move to, which is the point: a wedged pump
    /// is a fault about this daemon's own knowledge and will still refuse
    /// when every feature has shipped.
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
        // Not per-sheep. The blob describes one process image, so a flock is
        // carried whole or not at all.
        let carryable = entry_fixture(|_| {});
        let unsupported = entry_fixture(|_| {});
        assert!(matches!(
            fitness(&[plain(&carryable), wedged(&unsupported)]),
            Fitness::Refused(_)
        ));
    }

    #[test]
    fn the_refusal_names_which_sheep_and_why() {
        // The operator sees this in `shep daemon reload`'s output, so it has
        // to say what to do about it, not just that it declined.
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

    /// A wedged pump refuses, and says so as a fault rather than as a
    /// feature this daemon has not shipped yet.
    ///
    /// The wording matters more than it looks: every other refusal here is
    /// "wait for a later version", and an operator who reads that about a
    /// stuck filesystem will wait for something that is never coming.
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

    /// fails if the gate still refuses a sheep that asked for a shepherd
    /// channel by its own name.
    ///
    /// One socketpair, carried by number in [`CarriedFds::channel`]. What
    /// the successor rebuilds on it is the same pair of pumps a spawn
    /// wires, so the child's fd 3 is the fd 3 it has had all along.
    #[test]
    fn a_sheep_with_a_channel_is_carried() {
        let e = entry_fixture(|app| app.channel = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    /// fails if the gate still refuses a sheep gated on `{"kind":"ready"}`.
    ///
    /// Its own case rather than a variant of the one above, because
    /// `wait_ready` reaches machinery `channel` alone does not: the sheep
    /// may be `Starting` at the exec, and a successor that adopts it with
    /// nothing waiting leaves it `Starting` outside any timeout. See
    /// `Actor::install_adopted`, which re-arms the wait.
    #[test]
    fn wait_ready_alone_is_carried() {
        let e = entry_fixture(|app| app.wait_ready = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    /// fails if the gate still refuses a sheep that is told before it is
    /// killed.
    ///
    /// Its own case for the same reason: `shutdown_with_message` is the one
    /// of the three whose channel traffic runs the other way, from the
    /// shepherd to the child, so it is the writer half of the rebuilt pair
    /// that has to work.
    #[test]
    fn shutdown_with_message_alone_is_carried() {
        let e = entry_fixture(|app| app.shutdown_with_message = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    /// fails if the gate still refuses a sheep whose app asked for a stdin
    /// pipe.
    ///
    /// The write end is one more descriptor through the machinery that
    /// already carries four, and [`CarriedFds::stdin`] is where it travels.
    #[test]
    fn a_sheep_with_stdin_is_carried() {
        let e = entry_fixture(|app| app.stdin = true);
        assert_eq!(fitness(&[plain(&e)]), Fitness::Carryable);
    }

    /// fails if the gate still refuses a flock because one of its sheep is
    /// a dog.
    ///
    /// The last FEATURE refusal, and the one this phase exists to remove. A
    /// dog's process crosses the exec for free, exactly as every sheep's
    /// does; what it needed was a connection that re-establishes itself
    /// afterwards (`shep_client::ReconnectingClient`, task 1) and a daemon
    /// that knows what to do when a successor refuses that handshake
    /// (`dogs::record_refused_dog`, task 2).
    ///
    /// Both sources, because a dog is the MARKER rather than the binary: a
    /// gate that read `DogSource::BuiltIn` alone would pass this half and go
    /// on refusing every flock an operator had adopted a dog into.
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

    /// fails if the gate still refuses an app running more than one
    /// instance.
    ///
    /// Nothing about the descriptor inventory changes: a slot IS a sheep
    /// here, with its own supervisor slot, its own log pump and its own set
    /// of descriptors, so a two-instance app is two entries the gate reads
    /// one at a time exactly as it reads two apps.
    ///
    /// Both slots, not one. The gate is whole-flock, so refusing either of
    /// them would refuse the app, and a version that only stopped looking
    /// at `instances` on slot 0 would pass a one-candidate case.
    #[test]
    fn an_app_with_more_than_one_instance_is_carried() {
        let mut zero = entry_fixture(|app| app.instances = 2);
        let mut one = entry_fixture(|app| app.instances = 2);
        one.id = 2;
        one.instance = 1;
        one.pid = Some(101);
        assert_eq!(fitness(&[plain(&zero), plain(&one)]), Fitness::Carryable);
        // Order is not the fact being tested, but a gate that read only the
        // first candidate would pass the assertion above too.
        zero.instance = 1;
        one.instance = 0;
        assert_eq!(fitness(&[plain(&one), plain(&zero)]), Fitness::Carryable);
    }

    /// fails if a blob folds two instances of one app together, or lets one
    /// slot's descriptors reach the other's row.
    ///
    /// The defect this exists for leaves a flock that looks entirely
    /// healthy: two live pids, both adopted, both `Online`, and each one
    /// writing into the other's log file under the other's
    /// `SHEP_INSTANCE`. Nothing a pid check can see. What stops it is that
    /// [`CarriedSheep::instance`] is carried per row rather than re-derived
    /// from a count, and this pins that.
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
        // Bound by slot rather than by position, so a blob that reordered
        // the rows still has to put each pid with its own slot.
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

    /// Fails if a sheep mid-swap refuses the flock again, or if the marker
    /// that routes its next exit is lost on the way into the blob.
    ///
    /// The inverse of the case this replaces, in the shape
    /// [`a_pending_manual_command_no_longer_refuses_and_reaches_the_blob`]
    /// set. Both halves of a swap are asserted, and the drainee's linked id
    /// with them: `Drainee { new_id }` is what tells the successor which
    /// entry is coming to take this one's place, and a marker that arrived
    /// as a bare "drainee" would leave the successor unable to finish the
    /// swap it inherited.
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

    /// Fails if a pending manual command refuses the flock again.
    ///
    /// The inverse of the case this replaces. A `stop`, `restart` or
    /// `delete` already claimed against a sheep used to turn the whole flock
    /// away; [`CarriedSheep::manual`] carries the marker now, and
    /// `Actor::install_adopted` re-arms the ladder that carries it out, so
    /// the fact is no longer carryability-relevant at all — which is why
    /// `Candidate` has nothing left to say about it and this case asserts
    /// through the blob instead of through the gate.
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
    /// from `base`.
    ///
    /// Takes a base so two instances of one app can be given two disjoint
    /// sets, which is what a merged-log app really has: one inode, two
    /// `open`s, two numbers.
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

    /// [`CarriedFds::all`] and [`CarriedFds::all_kinded`] must name the same
    /// six numbers, in the same order, one slot each.
    ///
    /// `all` is what the `FD_CLOEXEC` sweep walks, and `all_kinded` is what
    /// the pre-exec rehearsal walks. A slot paired with the wrong field
    /// there would have the rehearsal check a stdout pipe against the stdin
    /// check, which passes for the wrong reason on a fixture where both are
    /// pipes and fails on a real flock. Nothing else would notice: both
    /// arrays are six long and both are full of plausible numbers.
    ///
    /// Six DISTINCT numbers, so the equality below is about the pairing
    /// rather than about the length, and a second pass over the slots, so a
    /// copy-paste that repeated one cannot ride along behind numbers that
    /// happen to line up.
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
        // Without this the secret test could pass on a fixture that never
        // carried a secret in the first place.
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
        // The successor respawns this sheep from what the blob carries, so
        // an env value the blob drops is an app that comes back with a
        // different environment than the one it was started with. Silent,
        // and visible only as the app misbehaving.
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
        // The blob carries env; a log line naming the daemon's own state
        // must not. An exact-string assertion rather than a field check,
        // because the risk is a future field printing env by accident
        // (IR-41).
        let text = format!("{:?}", sample_handover_with_secret_env());
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn a_written_blob_is_readable_only_by_its_owner() {
        // It names this host's pids and descriptor numbers, and the mode is
        // set at creation so there is never a window in which it is not.
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

    /// fails if a blob written before this daemon carried stdin stops
    /// loading.
    ///
    /// The predecessor in a handover is by definition a different build,
    /// and one old enough not to know about [`CarriedFds::stdin`] refused
    /// to carry a sheep that had one. So an absent field is not a gap to
    /// guess at: it says this sheep had no stdin pipe, which is what
    /// `None` means. Making it a hard parse failure instead would leave the
    /// successor refusing to boot after the predecessor had already exec'd
    /// itself away, which is a whole flock unsupervised for one added
    /// field.
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

    /// fails if a blob written before this daemon carried a shepherd
    /// channel stops loading.
    ///
    /// The same argument the case above makes for stdin, and it has to be
    /// made again rather than assumed: `channel` is a second `Option` field
    /// added after [`VERSION`] was fixed at 1, and a reader that refused an
    /// absent one would leave a successor unable to boot after its
    /// predecessor had exec'd itself away. A predecessor from before this
    /// field existed refused to carry a channelled sheep at all, so `None`
    /// is not a guess: it is what that blob means.
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

    /// fails if a blob written before this daemon carried a pending delete
    /// stops loading.
    ///
    /// A predecessor from before [`CarriedSheep::pending_delete`] existed
    /// refused to hand over a sheep with a delete pending at all, so its
    /// blobs never have the key and `None` is what that truthfully means:
    /// "no", not "unknown". Failing to parse instead would leave the
    /// successor refusing to boot after the predecessor had already exec'd
    /// itself away, which is a whole flock unsupervised for one added
    /// field.
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

    /// fails if a blob written before this daemon carried a parked config
    /// stops loading, or if the config and its reset flag do not survive one
    /// that has them.
    ///
    /// Same shape and same stakes as the pending-delete case above. A
    /// predecessor from before [`CarriedSheep::pending`] existed had no
    /// parking slot at all, so its blobs never have the key and `None` is
    /// the truthful reading of that: this sheep is owed nothing. A hard
    /// parse failure instead would leave the successor refusing to boot
    /// after the predecessor had already exec'd itself away.
    ///
    /// The sample is given a parked config first, so the removal below
    /// removes something: a blob whose `pending` was `None` all along could
    /// not tell an absent key from a present `null`.
    #[test]
    fn a_blob_written_before_pending_was_carried_still_loads() {
        let mut entry = entry_fixture(|_| {});
        entry.pending = Some(app_with("web", |app| {
            app.env.insert("MODE".to_owned(), "blue".to_owned());
        }));
        entry.pending_reidentifies = true;
        let blob = handover_over(&entry);

        // First: a blob that HAS the two keys carries both, which is the
        // half the eight siblings' shape does not assert for its own field.
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

        // Then: an older blob, which has neither key.
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

    /// fails if a blob written before this daemon carried a manual marker
    /// stops loading, or if the marker does not survive one that has it.
    ///
    /// Same shape and same stakes as the pending-delete case above. A
    /// predecessor from before [`CarriedSheep::manual`] existed refused to
    /// hand over a sheep with a `stop`, `restart` or `delete` already
    /// claimed against it, so its blobs never have the key, and `None` is
    /// the truthful reading of that: no command owns this exit. A hard
    /// parse failure instead would leave the successor refusing to boot
    /// after the predecessor had already exec'd itself away.
    ///
    /// The sample is given a marker first, so the removal below removes
    /// something: a blob whose `manual` was `None` all along could not tell
    /// an absent key from a present `null`.
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

    /// fails if a blob written before this daemon carried a swap in flight
    /// stops loading, or if a swap that IS carried does not survive the
    /// wire whole.
    ///
    /// Same shape and same stakes as the two cases above, in both halves.
    /// A predecessor from before this daemon carried a reload refused the
    /// whole flock over one sheep mid-swap, so its blobs have neither
    /// `sheep[].reload` nor the top-level `reloads` array — and `None` is
    /// the truthful reading of both: nothing was mid-reload. A hard parse
    /// failure instead would leave the successor refusing to boot after the
    /// predecessor had already exec'd itself away.
    ///
    /// The current-blob half is asserted first so the removals below remove
    /// something, and it asserts the JOB as well as the marker: the marker
    /// alone tells a successor an instance is half of a swap without
    /// telling it which swap, and a job restored without its phase or its
    /// mode would be continued down the wrong ordering.
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

    /// fails if a blob written before this daemon carried a failed
    /// readiness verdict stops loading, or if a verdict that IS carried does
    /// not survive the wire.
    ///
    /// Same stakes as the three cases above and a different argument, which
    /// is why it is asserted rather than assumed. Each of those was a gate
    /// refusal before it was a field, so an absent key proves the fact was
    /// false; this was never a refusal, and a predecessor from before the
    /// field existed carried such an instance while silently dropping the
    /// flag. `None` is therefore that predecessor saying nothing rather than
    /// saying "no" — and `false` is still the only reading available, since
    /// it is what a successor of that blob assumed anyway and a hard parse
    /// failure would leave this one refusing to boot after its predecessor
    /// had exec'd itself away.
    ///
    /// The current-blob half is asserted first so the removal below removes
    /// something: a blob whose `ready_failed` was `false` all along could
    /// not tell an absent key from a present one.
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

    /// fails if a dog crosses the blob as an ordinary sheep.
    ///
    /// The marker is not decoration. `Actor::matching_ids` keeps a dog out
    /// of every selector but an exact one, so a carried dog that lost it
    /// leaves `shep dogs` and turns up in `shep flock` beside the
    /// operator's own apps -- and `shep restart all` reaches it.
    /// `dogs::spawn_dog_watch` records an exhausted restart budget only for
    /// an entry carrying it, so a dog that burned its whole budget after a
    /// reload would write nothing to `barks.jsonl`. And `rpc::dog_staleness`
    /// builds its roster from it, so a reload could not tell a dog that had
    /// not dialled back yet from one that had.
    ///
    /// The whole `DogSource`, not a boolean: `shep dogs` reports where a
    /// dog's binary came from, and a successor that knew only THAT a sheep
    /// was a dog would answer that column with a guess.
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

    /// fails if a plain sheep picks a dog marker up on the way across.
    ///
    /// The other direction, and it is not implied by the case above: a
    /// `from_entry` that hardcoded a source would pass that one and turn
    /// every carried app into a dog, which takes the whole flock out of
    /// `shep flock` and out of `shep restart all` at once.
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

    /// fails if a blob written before this daemon carried a dog stops
    /// loading.
    ///
    /// Same stakes and the same argument as the four fields above it: a
    /// predecessor from before this field existed refused to carry a dog at
    /// ALL, so a blob it wrote never has the key and never describes one.
    /// `None` is what that blob truthfully means rather than "unknown", so
    /// [`VERSION`] stays unmoved -- and a hard parse failure would leave a
    /// successor refusing to boot after its predecessor had exec'd itself
    /// away, over a field whose absence has a correct reading.
    ///
    /// The current-blob half is asserted first so the removal below removes
    /// something: a blob whose sheep was never a dog could not tell an
    /// absent key from a present one.
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

    /// fails if a blob written before this daemon carried a respawn deadline
    /// stops loading, or if a deadline that IS carried does not survive the
    /// wire.
    ///
    /// Same shape as `ready_failed` above and the same argument: this was
    /// never a gate refusal, so an absent key is a predecessor staying
    /// silent rather than saying "no". What it does about that silence is
    /// different, though, and is what makes the case worth having — it falls
    /// back to re-arming the whole delay, which is exactly what v0.1.20 did
    /// for every sheep. A hard parse failure instead would leave a successor
    /// refusing to boot after its predecessor had exec'd itself away, over a
    /// field whose absence has a correct reading.
    ///
    /// The current-blob half is asserted first so the removal below removes
    /// something.
    ///
    /// A whole second of slack on the round trip: serde carries a
    /// `SystemTime` as seconds plus nanoseconds, so this pins the value
    /// rather than the precision, and an exact equality would be asserting
    /// something about serde rather than about the blob.
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

    /// fails if a deadline reaches the blob on a row that is not owed a
    /// respawn.
    ///
    /// The slot's own field is written on the transition into
    /// `WaitingRestart` and never cleared, so a sheep that has since been
    /// respawned still holds the moment its previous exit was due back at.
    /// Nothing would act on it — `install_adopted` re-arms only for a
    /// `WaitingRestart` row — but the blob is a file an operator may read,
    /// and a stale deadline on an `Online` row is a claim about the flock
    /// that is not true.
    ///
    /// Both halves, because a gate that dropped the field unconditionally
    /// would pass the second assertion on its own.
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
        // /proc/self/exe resolves to the inode, so a binary replaced by a
        // `cargo install` rename gives `"<path> (deleted)"`. Exec'ing that
        // fails, which would make a handover silently unable to upgrade,
        // which is the whole point of the feature.
        let p = exec_target().unwrap();
        assert!(
            !p.to_string_lossy().contains("(deleted)"),
            "exec target resolved to a deleted inode: {}",
            p.display()
        );
    }

    #[test]
    fn resolve_target_refuses_a_synthetic_deleted_inode_candidate() {
        // The seam `dogs::builtin_program` depends on through
        // `exec_target`. `current_exe` cannot be made to return a
        // `" (deleted)"` string on this platform, so this hands
        // `resolve_target` one directly — proving the refusal without
        // needing a real Linux inode-unlink to produce the string.
        let deleted = PathBuf::from("/opt/shep/shep (deleted)");
        let err = resolve_target([None, Some(deleted)], None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no binary to exec: /opt/shep/shep (deleted) (names a deleted inode, not a file)"
        );
    }

    #[test]
    fn a_deleted_inode_candidate_is_refused_on_every_platform() {
        // The portable half of the test above. The Linux one asserts the
        // whole resolution never yields such a path, but a macOS run never
        // compiles it, so the rule it protects would go unexercised here.
        // This one drives the same check directly.
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
        // The test harness is invoked by an absolute path, so this proves
        // the argv[0] arm reaches a real file rather than that the join
        // itself is correct; the join is exercised by `Path::join`'s own
        // contract that an absolute right-hand side replaces the left.
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
        // Three stages of the same test binary. The ordinary run is the
        // parent: it re-runs this one test in a child, which writes into a
        // pipe and hands over, and the image that `hand_over` execs into
        // reads that pipe back by number. There is no helper binary to
        // borrow here, shep-daemon being a library, and a test that stopped
        // short of a real `execve` would prove none of what this one does.
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
        // Checked before the real assertion, because the two failures look
        // the same from the outside and only one of them is about the
        // handover. `--exact` against a name that no longer resolves matches
        // nothing: the child runs zero tests, exits successfully, and prints
        // no marker, which would otherwise be reported as a descriptor that
        // did not cross.
        assert!(
            stdout.contains("running 1 test"),
            "the child ran no test, so `{SELFTEST_NAME}` is not where this test lives any more;              `--exact` needs the full path and nothing updates it automatically: {stdout}"
        );
        // The whole mechanism in one assertion: a pipe written before the
        // exec is readable by the process after it, on the same fd number,
        // proving both that the image changed and that the descriptor
        // crossed.
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
        // A blob that outlives a failed exec describes a handover that never
        // happened, and the next boot would adopt a picture of a process
        // image that does not exist.
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

    /// A failed exec puts `FD_CLOEXEC` back on everything it cleared.
    ///
    /// The clearing is what makes a descriptor cross an `execve`, and when
    /// the exec does not happen the daemon returns to the graceful-stop
    /// fallback with its supervisor still running. A restart or a queued
    /// `Start` in the window before teardown then spawns a sheep that
    /// inherits whatever is still exec-inheritable, and a child holding the
    /// pidfile keeps this home claimed after the daemon that owned it has
    /// gone.
    ///
    /// Asserted on both the daemon's own two descriptors and a carried log
    /// handle, because `named_fds` yields them from two different places and
    /// a restore that walked only the first pair would pass on a shorter
    /// case.
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
        // The stdin write end and the channel's daemon end ride along,
        // because they are the two newest and are what a `named_fds` that
        // still yielded four, or five, would silently leave close-on-exec.
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

        // Drives `keep_and_exec` directly so the clear itself is observable.
        // Without this the case cannot fail on the regression it exists to
        // catch: if `named_fds` yielded nothing, no descriptor would be
        // cleared and none restored, and the before and after assertions
        // would both still pass over untouched flags.
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
