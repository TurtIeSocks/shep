//! Whole-flock handover: whether this daemon's flock can be replaced in
//! place, the [`Handover`] blob that describes it, and (in a later task) the
//! exec that carries it.
//!
//! Phase 2a carries only the plainest sheep: no shepherd channel, no stdin,
//! no dog, one instance, no in-flight reload, and nothing an operator has
//! already asked to stop or delete. [`fitness`] is the gate: get it wrong in
//! the permissive direction and a half-built handover corrupts a live
//! flock; get it wrong in the strict direction and the caller merely falls
//! back to the stop-and-start arm that already works. That asymmetry is why
//! an unclear case refuses rather than guesses.

// Nothing in this crate calls `fitness` or writes a blob yet. Task 8 wires
// the gate into `boot.rs`'s SIGHUP arm and task 5 writes the blob; until
// then, an honestly-unreachable gate is better than a stub that pretends to
// decide something and always answers the same way.
#![expect(
    dead_code,
    reason = "tasks 5 and 8 wire the blob and the gate into boot.rs; nothing calls them yet"
)]

pub(crate) mod adopt;
mod fds;
pub(crate) mod reap;

use core::convert::Infallible;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;
use shep_core::protocol::ExitInfo;
use shep_core::status::ProcStatus;

use crate::entry::ProcessEntry;
use crate::privilege::SpawnIdentity;

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
/// Every variant is a feature phase 2a does not yet carry, not an error. The
/// caller falls back to the stop arm, which is correct behaviour rather
/// than a degraded one.
///
/// `#[non_exhaustive]`, unlike [`crate::boot::Shepherd`]: that enum is
/// closed by its mechanism (a pidfile lock is either free, held-with-pid or
/// held-without, and there is no fourth state). This one is closed by
/// nothing but how much of the handover has shipped. 2b and 2c each widen
/// what phase 2a refuses today into something a later phase carries, so a
/// match here must keep tolerating a variant this module has not named yet.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefusedReason {
    /// The sheep holds a shepherd channel: `channel`, `wait_ready` or
    /// `shutdown_with_message`, whose socketpair 2b carries.
    Channel {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep has `stdin = true`, whose pipe 2b carries.
    Stdin {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep is a dog, which 2b's descriptor inventory does not cover
    /// yet.
    Dog {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep's app runs more than one instance, which 2b carries.
    MultiInstance {
        /// The sheep's name.
        sheep: String,
    },
    /// The sheep is mid-reload, drainee or replacement, which 2c carries.
    ReloadInFlight {
        /// The sheep's name.
        sheep: String,
    },
    /// An operator's `stop` is waiting on this sheep's next exit, which 2c
    /// carries.
    PendingStop {
        /// The sheep's name.
        sheep: String,
    },
    /// An operator's `delete` targets this sheep, which 2c carries.
    PendingDelete {
        /// The sheep's name.
        sheep: String,
    },
}

impl core::fmt::Display for RefusedReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (sheep, feature) = match self {
            Self::Channel { sheep } => (sheep, "a shepherd channel"),
            Self::Stdin { sheep } => (sheep, "stdin"),
            Self::Dog { sheep } => (sheep, "being a dog"),
            Self::MultiInstance { sheep } => (sheep, "more than one instance"),
            Self::ReloadInFlight { sheep } => (sheep, "an in-flight reload"),
            Self::PendingStop { sheep } => (sheep, "a pending manual stop"),
            Self::PendingDelete { sheep } => (sheep, "a pending delete"),
        };
        write!(
            f,
            "sheep '{sheep}' has {feature}, which this daemon cannot yet hand \
             over; reload falls back to a stop-and-start instead"
        )
    }
}

/// One sheep's carryability-relevant facts.
///
/// Bundles a [`ProcessEntry`] with the two facts that do not live on it: a
/// pending manual stop and a pending delete both live on the supervisor's
/// private slot type, not on the entry it wraps, so `fitness` cannot reach
/// them through `entry` alone. The caller, the supervisor, which owns both
/// of them, builds this view; `fitness` stays a pure function over data it is
/// handed rather than reaching into the registry itself.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The sheep's lifecycle entry.
    pub entry: &'a ProcessEntry,
    /// Whether an operator's `stop` is waiting on this sheep's next exit.
    pub pending_stop: bool,
    /// Whether an operator's `delete` targets this sheep.
    pub pending_delete: bool,
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

    if config.channel || config.wait_ready || config.shutdown_with_message {
        return Some(RefusedReason::Channel { sheep: name() });
    }
    if config.stdin {
        return Some(RefusedReason::Stdin { sheep: name() });
    }
    if entry.dog.is_some() {
        return Some(RefusedReason::Dog { sheep: name() });
    }
    if config.instances > 1 {
        return Some(RefusedReason::MultiInstance { sheep: name() });
    }
    if !matches!(entry.reload, crate::entry::ReloadState::None) {
        return Some(RefusedReason::ReloadInFlight { sheep: name() });
    }
    if candidate.pending_delete {
        return Some(RefusedReason::PendingDelete { sheep: name() });
    }
    if candidate.pending_stop {
        return Some(RefusedReason::PendingStop { sheep: name() });
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
}

impl Handover {
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
        [self.listener_fd, self.pidfile_fd]
            .into_iter()
            .chain(self.sheep.iter().flat_map(|sheep| {
                let fds = sheep.fds;
                [fds.out_pipe, fds.err_pipe, fds.out_log, fds.err_log]
                    .into_iter()
                    .flatten()
            }))
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
    /// `epoch` and `fds` are arguments rather than reads off `entry`
    /// because neither lives there: the respawn epoch is on the
    /// supervisor's private slot, and the descriptor numbers are known only
    /// to whichever code holds the open descriptors. This is the same split
    /// [`Candidate`] makes, for the same reason.
    #[must_use]
    pub fn from_entry(entry: &ProcessEntry, epoch: u64, fds: CarriedFds) -> Self {
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
            app: entry.spec.config().clone(),
        }
    }
}

/// The four descriptor numbers one sheep's output travels on.
///
/// Grouped rather than spelled as four fields on [`CarriedSheep`] for two
/// reasons: they are all `Option<RawFd>`, so a constructor taking them
/// positionally would let a caller swap two of them silently, and they share
/// one fact, which is that a running instance has all four and a registered
/// one that is not running has none.
///
/// `None` is that second case and only that case. A blob naming a
/// descriptor that is not open in the successor is a failure to refuse, not
/// a `None` to tolerate: losing a sheep's stdout read end does not lose its
/// output, it blocks the child on `write()` once the 64KiB pipe buffer
/// fills, which reads as an application hang rather than as a shep bug.
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

    let mut refusals = Vec::new();
    for candidate in [recorded, current.as_deref().ok().map(Path::to_path_buf)] {
        let Some(candidate) = candidate else { continue };
        match check_target(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(problem) => refusals.push(format!("{} ({problem})", candidate.display())),
        }
    }

    if let Err(e) = &current {
        refusals.push(format!("this process's own image ({e})"));
    }
    if refusals.is_empty() {
        refusals.push("no candidate at all".to_owned());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no binary to hand over to: {}", refusals.join("; ")),
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
/// Every one of those returns with no blob left on disk.
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
    for fd in blob.named_fds() {
        fds::keep_raw_across_exec(fd)?;
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
    use crate::testing::{app_with, test_paths};

    /// A plain, `Online` entry: no channel, no stdin, not a dog, one
    /// instance, no in-flight reload. Every field a real spawn would set is
    /// present so a future field this gate should read cannot be silently
    /// left at a `Default` that hides a bug.
    fn entry_fixture(mutate: impl FnOnce(&mut AppConfig)) -> ProcessEntry {
        let spec = app_with("web", mutate);
        ProcessEntry {
            id: 1,
            spec,
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
            pending_stop: false,
            pending_delete: false,
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
        let plain_entry = entry_fixture(|_| {});
        let channelled = entry_fixture(|app| app.channel = true);
        assert!(matches!(
            fitness(&[plain(&plain_entry), plain(&channelled)]),
            Fitness::Refused(_)
        ));
    }

    #[test]
    fn the_refusal_names_which_sheep_and_why() {
        // The operator sees this in `shep daemon reload`'s output, so it has
        // to say what to do about it, not just that it declined.
        let channelled = entry_fixture(|app| app.channel = true);
        let Fitness::Refused(r) = fitness(&[plain(&channelled)]) else {
            panic!("expected a refusal")
        };
        let text = r.to_string();
        assert!(text.contains("shepherd channel"), "{text}");
    }

    #[test]
    fn an_empty_flock_is_carryable() {
        assert_eq!(fitness(&[]), Fitness::Carryable);
    }

    #[test]
    fn wait_ready_alone_refuses_as_a_channel() {
        let e = entry_fixture(|app| app.wait_ready = true);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Channel { .. })
        ));
    }

    #[test]
    fn shutdown_with_message_alone_refuses_as_a_channel() {
        let e = entry_fixture(|app| app.shutdown_with_message = true);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Channel { .. })
        ));
    }

    #[test]
    fn stdin_refuses() {
        let e = entry_fixture(|app| app.stdin = true);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Stdin { .. })
        ));
    }

    #[test]
    fn a_dog_refuses() {
        let mut e = entry_fixture(|_| {});
        e.dog = Some(shep_core::protocol::DogSource::BuiltIn);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::Dog { .. })
        ));
    }

    #[test]
    fn more_than_one_instance_refuses() {
        let e = entry_fixture(|app| app.instances = 2);
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::MultiInstance { .. })
        ));
    }

    #[test]
    fn an_in_flight_reload_refuses() {
        let mut e = entry_fixture(|_| {});
        e.reload = ReloadState::Replacement;
        assert!(matches!(
            fitness(&[plain(&e)]),
            Fitness::Refused(RefusedReason::ReloadInFlight { .. })
        ));
    }

    #[test]
    fn a_pending_manual_stop_refuses() {
        let e = entry_fixture(|_| {});
        let candidate = Candidate {
            entry: &e,
            pending_stop: true,
            pending_delete: false,
        };
        assert!(matches!(
            fitness(&[candidate]),
            Fitness::Refused(RefusedReason::PendingStop { .. })
        ));
    }

    /// One carried sheep off `entry`, with the descriptor numbers a
    /// running sheep would have.
    fn carried(entry: &ProcessEntry) -> CarriedSheep {
        CarriedSheep::from_entry(
            entry,
            7,
            CarriedFds {
                out_pipe: Some(11),
                err_pipe: Some(12),
                out_log: Some(13),
                err_log: Some(14),
            },
        )
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
        }
    }

    fn sample_handover() -> Handover {
        handover_over(&entry_fixture(|_| {}))
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

    #[test]
    fn a_pending_delete_refuses() {
        let e = entry_fixture(|_| {});
        let candidate = Candidate {
            entry: &e,
            pending_stop: false,
            pending_delete: true,
        };
        assert!(matches!(
            fitness(&[candidate]),
            Fitness::Refused(RefusedReason::PendingDelete { .. })
        ));
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
            sheep: vec![CarriedSheep::from_entry(entry, 7, fds)],
            listener_fd,
            pidfile_fd,
            next_id: 9,
            next_deadline: 5,
            next_action_stamp: 2,
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
            },
        );

        let err = exec_into(&target, &blob, &paths).unwrap_err();
        assert!(
            !Handover::path(&paths).exists(),
            "a failed exec left a blob behind: {err}"
        );
    }
}
