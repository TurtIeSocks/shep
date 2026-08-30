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

mod fds;

use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::RawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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
/// **No environment values, ever.** A sheep's env can hold secrets, and the
/// successor re-reads them from config, so nothing here is derived from
/// `AppConfig::env`. That is asserted by an exact-string test over the
/// serialized form rather than by a field check, because the risk is a
/// future field that carries env by accident (IR-41). `Debug` is derived
/// for the same reason: ids, pids, a name and a handful of descriptor
/// numbers are all an operator could read out of `ps`.
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
/// Everything here is a fact about an instance that is already running, or
/// already registered and not running. Nothing is re-derivable from config,
/// which is why each field is here: config says what an app *is*, and this
/// says what this instance currently *is doing*.
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

#[cfg(test)]
mod tests {
    use shep_core::config::AppConfig;
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
    fn the_blob_carries_no_environment_values() {
        // A sheep's env can hold secrets. This is an exact-string assertion
        // rather than a field check, because the risk is a future field that
        // serializes env by accident (IR-41).
        let text = serde_json::to_string(&sample_handover_with_secret_env()).unwrap();
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
}
