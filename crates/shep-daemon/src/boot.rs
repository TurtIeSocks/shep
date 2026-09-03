//! Daemon boot: layout, pidfile, control-socket bind, and the run/teardown
//! sequence
//!
//! Everything a daemon needs before it can accept its first connection:
//! creating (and tightening) `$SHEP_HOME`'s directory layout, recording its
//! own pid, binding the control socket — including recovering from a socket
//! file a crashed daemon left behind — and, once bound, reporting readiness
//! on an inherited pipe if the CLI daemonized us (spec §3). This module owns
//! the 0700 guarantee `crate::server::RpcServer`'s doc names as the boot
//! path's responsibility: `init_dirs` creates `run/` (and every other
//! layout directory) `0700` and *tightens* it back down if it already
//! exists looser, so the guarantee holds whether this is a first boot or a
//! restart onto a directory some other process touched.
//!
//! [`boot`] assembles every piece earlier tasks built (bus, supervisor,
//! muster roll, RPC context) into one [`RunningDaemon`]; [`RunningDaemon::run`]
//! serves connections until a signal or `KillDaemon` arrives, then tears
//! down in a load-bearing order — see its own doc for why.
//!
//! **No unsafe in this module.** [`BootOptions::ready_fd`] is
//! `Option<`[`std::fs::File`]`>`, not a raw descriptor: the CALLER adopts
//! the inherited readiness pipe (`unsafe fn` `crate::sys::adopt_fd`,
//! IR-22's sole unsafe surface) before ever constructing a [`BootOptions`],
//! so [`boot`] only ever receives an already-owned handle and never
//! constructs one from a bare number itself. Every bind/probe/unlink/
//! signal-registration step in this module is plain safe std/tokio.
//!
//! (`boot` cannot perform that adoption itself: `adopt_fd`'s ordering
//! precondition is process-wide — "call before THIS PROCESS opens any
//! descriptor" — and `boot` is `async`, so a tokio runtime with its own live
//! poller fds already exists by the time `boot` is ever called. Only the
//! CLI's `main`, as its literal first fd-touching statement, can discharge
//! that precondition. See `crate::sys::adopt_fd`'s own `# Safety` section
//! and rationale essay for the full contract.)

use core::fmt;
use core::time::Duration;
use std::ffi::OsString;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use shep_core::transport::Listener;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use shep_core::paths::ShepPaths;
use shep_core::protocol::BusEvent;
#[cfg(unix)]
use shep_core::selector::ProcessSelector;

use crate::bus::{SharedEvent, new_bus};
use crate::cron::DEFAULT_MAX_CRON_SLEEP;
use crate::dogs::{DogSpec, spawn_dog_watch};
use crate::extras::{Extras, ExtrasReports, spawn_extras_reporter};
use crate::rpc::RpcContext;
use crate::runner::ProcessRunner;
use crate::server::RpcServer;
use crate::snapshot::{self, FlockRegistry, SnapshotError, SnapshotWriter, spawn_snapshot_writer};
use crate::supervisor::{SupervisorBuilder, SupervisorHandle};
// Read only by the SIGUSR2 log-reopen handler, which is unix-only — Windows
// has no user-defined console control event to hang one on. See
// `install_signals`' Windows arm.
#[cfg(unix)]
use crate::supervisor::SupervisorError;

/// Mode for every directory shep creates (spec §10: no other user, at all)
pub const DIR_MODE: u32 = 0o700;

/// Capacity of each of the two lifecycle-extra report channels.
///
/// Bounded rather than unbounded on purpose: a report producer that outruns
/// the reporting task should back-pressure (the enforcer's own send is
/// awaited) rather than grow a queue of restarts nobody has performed yet.
/// Generous enough that a whole flock breaching on one sampling pass fits.
const EXTRAS_REPORT_CAPACITY: usize = 64;

/// Creates `dir` (and any missing parents) at [`DIR_MODE`] directly, via
/// [`DirBuilderExt::mode`] rather than the std default (`0o777`, narrowed
/// only by whatever the process umask happens to strip).
///
/// This is the fix for a real TOCTOU: `create_dir_all` followed by a
/// separate `chmod` leaves a window where a *freshly created* directory
/// sits at its umask-derived mode — empirically `0o755` under the common
/// `umask 022`, and world-*writable* under `umask 0` (a misconfigured
/// systemd unit), which opens a pre-bind symlink race on the socket path
/// underneath it. Requesting `0o700` at creation has no group/other bits
/// for any ordinary umask to strip, so there is nothing left to narrow
/// after the fact — the directory is never wider than `DIR_MODE`, not even
/// for the instant between `mkdir` and a later `chmod`.
///
/// Does not touch a directory that already exists (the umask given to
/// `mkdir` only governs directories this call actually creates) — that
/// case is [`init_dirs`]'s `set_permissions` pass, not this function's job.
#[cfg(windows)]
fn create_dir_at_dir_mode(dir: &Path) -> std::io::Result<()> {
    // No mode to set. `DIR_MODE` is a POSIX permission word and Windows
    // access control is an ACL, not a scalar, so there is nothing here to
    // translate it into: a directory created this way inherits its parent's
    // ACL, which under a normal user profile means the user and the local
    // Administrators group.
    //
    // **This is a real difference in posture and is recorded rather than
    // papered over.** On unix the `0700` on `$SHEP_HOME` is the PRIMARY
    // access control — `server.rs`'s security writeup says so outright, and
    // the same-uid peer check is explicitly the second layer behind it. On
    // Windows the primary control moved to the control pipe's own ACL
    // instead (see `shep_core::transport`), which is what actually refuses a
    // foreign local user, and it does so before any byte reaches shep. What
    // this directory does NOT do is hide `flock.json` — which holds an app's
    // `env` verbatim so a muster restore can reproduce it — from another
    // account that already has read access to the profile it lives under.
    //
    // Closing that would mean building an explicit DACL with
    // `PROTECTED_DACL_SECURITY_INFORMATION` so it does not inherit, which is
    // raw FFI in a crate whose only sanctioned unsafe lives in `sys.rs`. It
    // is deliberately not smuggled in here, and it is named in the operator
    // docs rather than left for someone to discover.
    std::fs::DirBuilder::new().recursive(true).create(dir)
}

#[cfg(unix)]
fn create_dir_at_dir_mode(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .mode(DIR_MODE)
        .recursive(true)
        .create(dir)
}

/// Creates `$SHEP_HOME` and its subdirectories, tightening loose modes
///
/// Runs on every boot, not just the first: a restart onto a layout that
/// already exists still forces every directory back to `DIR_MODE`, so a
/// looser mode left by an external touch (or an older shep version) never
/// survives a restart. A directory this call actually creates lands at
/// `DIR_MODE` immediately (via the private `create_dir_at_dir_mode`
/// helper); the `set_permissions` call below is what tightens one that was
/// already there.
///
/// # Errors
/// - [`BootError::Io`] — a directory could not be created or chmod'ed.
pub(crate) fn init_dirs(paths: &ShepPaths) -> Result<(), BootError> {
    for dir in [&paths.home, &paths.logs, &paths.pids, &paths.run] {
        create_dir_at_dir_mode(dir).map_err(|source| BootError::Io {
            path: dir.clone(),
            source,
        })?;
        // Re-tightening an already-existing directory, which is the half
        // `create_dir_at_dir_mode` cannot do. Unix only, for the reason that
        // function's Windows arm gives: there is no scalar mode to force a
        // directory back to.
        #[cfg(unix)]
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(DIR_MODE)).map_err(
            |source| BootError::Io {
                path: dir.clone(),
                source,
            },
        )?;
    }
    Ok(())
}

/// The daemon's own pidfile: `$SHEP_HOME/pids/shepd.pid`
///
/// Public only for `tests/daemon_e2e.rs`, which reads the file back to check
/// what a booted daemon wrote there. `shep-cli` derives the same path from
/// `ShepPaths` itself rather than calling this.
#[must_use]
pub fn pidfile(paths: &ShepPaths) -> PathBuf {
    paths.pids.join("shepd.pid")
}

/// Writes the pidfile atomically (temp file in `pids/`, `fsync`, then
/// `rename`), matching [`crate::snapshot::write_atomic`]'s convention for
/// every other file this daemon writes. [`crate::snapshot::write_atomic`]
/// itself isn't reusable here — it's typed to serialize a
/// [`crate::snapshot::FlockSnapshot`] as JSON, not a bare pid — so this
/// inlines the same temp+rename shape rather than writing straight to the
/// final path with `std::fs::write` and risking a reader observing a
/// truncated file mid-write.
///
/// [`boot`] itself does NOT call this: it records its own pid through the
/// crate-private `PidfileLock::record` instead, writing in place into the
/// SAME open, locked file descriptor it already holds — a rename here
/// would swap in an unlocked inode and undo that lock's whole point (see
/// `PidfileLock`'s own doc, next to its definition in this file).
///
/// Test-only, and deliberately so. Its only remaining use is seeding a
/// fixture pidfile without contending for the boot-time lock. Exported, it
/// would be a footgun: a rename over the locked path by any outside caller
/// silently disarms `PidfileLock` for the rest of that daemon's life. The
/// Phase 3 CLI has no need for it either — it learns the daemon's pid from
/// the handshake's `HelloAck`, not from this file.
///
/// # Errors
/// - [`BootError::Io`] — the pidfile could not be written.
#[cfg(test)]
#[cfg_attr(windows, allow(dead_code))]
fn write_pidfile(paths: &ShepPaths, pid: u32) -> Result<(), BootError> {
    use std::io::Write;

    use tempfile::NamedTempFile;

    let path = pidfile(paths);
    let mut tmp = NamedTempFile::new_in(&paths.pids).map_err(|source| BootError::Io {
        path: paths.pids.clone(),
        source,
    })?;
    tmp.write_all(pid.to_string().as_bytes())
        .map_err(|source| BootError::Io {
            path: path.clone(),
            source,
        })?;
    tmp.as_file().sync_all().map_err(|source| BootError::Io {
        path: path.clone(),
        source,
    })?;
    tmp.persist(&path).map_err(|err| BootError::Io {
        path,
        source: err.error,
    })?;
    Ok(())
}

/// Reads the recorded daemon pid, if any
///
/// A missing pidfile reads as `None`, as does one whose contents are not a
/// valid pid (the daemon's own writes never produce that, so it only
/// happens to a file something else corrupted) — the pid is a best-effort
/// hint attached to [`BootError::AlreadyRunning`], not itself the source of
/// truth for whether a daemon is live.
///
/// # Errors
/// - [`BootError::Io`] — the pidfile exists but could not be read.
pub(crate) fn read_pidfile(paths: &ShepPaths) -> Result<Option<u32>, BootError> {
    let path = pidfile(paths);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BootError::Io { path, source }),
    }
}

/// This daemon's exclusive claim on `$SHEP_HOME`: an `flock(2)` held on the
/// pidfile for as long as this process is alive.
///
/// Exists to close a real race in [`bind_socket`]'s stale-socket recovery
/// (spec §6): two daemons racing to boot the same `$SHEP_HOME` can both hit
/// `EADDRINUSE` on a crashed predecessor's leftover socket file, both probe
/// it, both observe `ConnectionRefused` (correctly — the file IS stale),
/// and both proceed into the `remove_file` + rebind arm — B's `remove_file`
/// can then delete A's freshly-bound listener out from under it, and BOTH
/// end up with a live [`UnixListener`] on the same path, each unaware of
/// the other. That is two live daemons on one `$SHEP_HOME`, exactly the
/// case [`BootError::AlreadyRunning`] exists to prevent, defeated exactly
/// when the recovery path is what makes it matter.
///
/// `flock` closes it because the kernel serializes concurrent lockers
/// itself: [`Self::acquire`] uses `LOCK_EX | LOCK_NB`, so of any number of
/// processes racing to boot the same `$SHEP_HOME`, at most one ever holds
/// this lock at a time, and every other one fails IMMEDIATELY with
/// [`BootError::AlreadyRunning`] rather than proceeding into
/// [`bind_socket`]'s probe/recover logic at all — there is no window left
/// for two processes to be inside that logic concurrently, because only
/// the lock's single holder can be in there. [`boot`] acquires this BEFORE
/// calling [`bind_socket`] and keeps it for [`RunningDaemon`]'s whole
/// lifetime (dropped only at the end of [`RunningDaemon::run`]) — see this
/// type's own `acquire`/`record` docs for exactly what that does and does
/// not still leave [`bind_socket`]'s own probe responsible for.
///
/// A crashed daemon's lock needs no separate cleanup: `flock`'s locks are
/// owned by the OPEN FILE DESCRIPTION, which the kernel releases the
/// instant every fd referencing it closes — including on process death by
/// any signal, `SIGKILL` included, with no unlock call required. That is
/// exactly the property a pidfile-based "am I the only one" check needs
/// and a filesystem-existence check (`pidfile` alone) cannot give: a stale
/// pidfile from a crash still exists, but a stale LOCK on it does not.
/// # Windows
///
/// Every property above survives, by a different mechanism and on a
/// different file. The lock is an exclusive `share_mode(0)` open — the same
/// primitive [`shep_core::kv`] and [`shep_core::barks`] use — held on a
/// SIBLING `shepd.pid.lock` rather than on the pidfile itself.
///
/// The sibling is not incidental. `share_mode(0)` denies *every* other
/// handle, including a read-only one, so locking the pidfile directly would
/// make it unreadable — and reading it is exactly what the losing daemon
/// does to name the winner in its [`BootError::AlreadyRunning`]. Unix has no
/// such problem because `flock` is advisory and does not block an ordinary
/// `open`. Splitting the lock token from the data file restores the unix
/// behaviour: the loser is refused, and can still read who won.
///
/// The load-bearing crash property holds too, for the same underlying
/// reason. A `flock` lock is owned by the open file description and the
/// kernel drops it when the last descriptor closes, process death included;
/// a Windows share-mode reservation is owned by the HANDLE and the kernel
/// drops it when the last handle closes, process death included. Neither
/// needs an unlock call, and neither leaves a stale lock behind after a
/// `SIGKILL` or a `TerminateProcess`. A stale pidfile still exists on both
/// platforms; a stale lock does not.
///
/// One genuine difference: `LockExclusiveNonblock` fails with `EWOULDBLOCK`
/// and a contended `share_mode(0)` open fails with
/// `ERROR_SHARING_VIOLATION`. Both are immediate — this is the one lock in
/// the workspace that must NOT wait, unlike the kv and bark rings, whose
/// Windows arms retry precisely because their unix arms block.
///
/// # The adopted arm
///
/// A successor does not take this lock: it inherits the descriptor that
/// already holds it. See [`UnixLock`] for why it must not take it again.
#[derive(Debug)]
struct PidfileLock {
    #[cfg(unix)]
    flock: UnixLock,
    /// The sibling lock file, held open with every share flag cleared.
    /// Named with a leading underscore because it is held, never read —
    /// [`PidfileLock::record`] writes the pidfile itself.
    #[cfg(windows)]
    _handle: std::fs::File,
}

/// How this process came to hold the pidfile's `flock`, which is the one
/// thing the two ways of starting a shepherd differ on.
///
/// # Why an adopted descriptor is never re-locked
///
/// A `flock` lock belongs to the open file DESCRIPTION, not to the process
/// and not to the path, so it crosses an `execve` still held: a successor
/// inherits the descriptor and the lock on it in one act, before it runs a
/// line of its own code. Taking the lock again would mean releasing it
/// first, since `nix` offers no constructor for an already-locked file that
/// does not lock it, and that window is exactly long enough for a second
/// daemon to claim this `$SHEP_HOME` while the only process supervising its
/// flock is mid-boot. So the adopted arm holds the file and does nothing
/// else with it.
///
/// The release rules stay identical either way. The kernel drops the lock
/// when the last descriptor on the description closes, process death
/// included, so a successor that crashes leaves the home claimable exactly
/// as a predecessor that crashed did.
#[cfg(unix)]
#[derive(Debug)]
enum UnixLock {
    /// Taken here, by this process, with `flock(LOCK_EX | LOCK_NB)`.
    Taken(nix::fcntl::Flock<std::fs::File>),
    /// Inherited across a handover `execve`, still locked, never re-locked.
    Adopted(std::fs::File),
}

#[cfg(unix)]
impl UnixLock {
    /// The locked pidfile itself, which [`PidfileLock::record`] writes
    /// through.
    fn file(&mut self) -> &mut std::fs::File {
        match self {
            Self::Taken(flock) => flock,
            Self::Adopted(file) => file,
        }
    }

    /// The descriptor carrying the lock, for the blob a handover hands on.
    ///
    /// Borrowed, never owned. Closing this would release the `flock` that is
    /// the only thing keeping a second daemon out of this `$SHEP_HOME`.
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        match self {
            Self::Taken(flock) => flock.as_raw_fd(),
            Self::Adopted(file) => file.as_raw_fd(),
        }
    }
}

/// The sibling file the Windows arm locks: the pidfile with `.lock` appended.
///
/// Never renamed, never read, and left on disk between boots — an inode with
/// a stable identity whose only job is to be openable exclusively, exactly
/// as `kv.json.lock` and `barks.jsonl.lock` are.
#[cfg(windows)]
fn pidfile_lock_path(paths: &ShepPaths) -> PathBuf {
    paths.pids.join("shepd.pid.lock")
}

impl PidfileLock {
    /// Opens (creating if necessary) and takes an exclusive, non-blocking
    /// `flock` on `paths`'s pidfile.
    ///
    /// Deliberately does NOT truncate on open: a losing caller's
    /// [`BootError::AlreadyRunning`] still wants to read whatever pid a
    /// previous winner recorded (via [`Self::record`]) for its error's
    /// hint, and this call cannot yet know which one it will be.
    ///
    /// # Errors
    /// - [`BootError::AlreadyRunning`] — another process already holds this
    ///   lock (carries the pid recorded in the file, if any — the file
    ///   itself is left untouched either way).
    /// - [`BootError::Io`] — the pidfile could not be opened.
    #[cfg(unix)]
    fn acquire(paths: &ShepPaths) -> Result<Self, BootError> {
        let path = pidfile(paths);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false) // preserve any pid a previous winner recorded — see this fn's own doc
            .mode(0o600)
            .open(&path)
            .map_err(|source| BootError::Io {
                path: path.clone(),
                source,
            })?;
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
            Ok(flock) => Ok(Self {
                flock: UnixLock::Taken(flock),
            }),
            Err((_file, nix::errno::Errno::EWOULDBLOCK)) => Err(BootError::AlreadyRunning {
                pid: read_pidfile(paths)?,
            }),
            Err((_file, errno)) => Err(BootError::Io {
                path,
                source: errno.into(),
            }),
        }
    }

    /// Opens (creating if necessary) the sibling lock file with every share
    /// flag cleared, which no second process can then open at all.
    ///
    /// # Errors
    /// - [`BootError::AlreadyRunning`] — another process already holds this
    ///   lock (carries the pid the winner recorded in the pidfile, which
    ///   stays readable precisely because the lock is on a sibling).
    /// - [`BootError::Io`] — the lock file could not be opened.
    #[cfg(windows)]
    fn acquire(paths: &ShepPaths) -> Result<Self, BootError> {
        use std::os::windows::fs::OpenOptionsExt as _;

        /// Another handle already holds share access this open denies.
        const ERROR_SHARING_VIOLATION: i32 = 32;

        let path = pidfile_lock_path(paths);
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .share_mode(0)
            .open(&path)
        {
            Ok(handle) => Ok(Self { _handle: handle }),
            // Immediate, never retried: unlike the kv and bark rings, whose
            // unix arms block and whose Windows arms therefore poll, this
            // lock is non-blocking on unix too. A second daemon must be
            // refused now, not queued behind the first.
            Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                Err(BootError::AlreadyRunning {
                    pid: read_pidfile(paths)?,
                })
            }
            Err(source) => Err(BootError::Io { path, source }),
        }
    }

    /// Overwrites the locked pidfile's content with `pid`, in place —
    /// truncate then write at offset 0, never a temp-file-plus-`rename`
    /// (contrast `write_pidfile`, this module's test-only helper): renaming
    /// a fresh inode over this path
    /// would swap in a file nothing has locked, silently ending this
    /// type's whole reason to exist for as long as the daemon keeps
    /// running afterward. A `flock` lock lives on the OPEN FILE
    /// DESCRIPTION, not the path, so it does not follow a rename.
    ///
    /// # Errors
    /// - [`BootError::Io`] — the write failed.
    #[cfg(windows)]
    fn record(&mut self, paths: &ShepPaths, pid: u32) -> Result<(), BootError> {
        use std::io::Write as _;

        // An ordinary write, because this arm holds its lock on the sibling
        // `.lock` rather than on this file — so unlike the unix arm there is
        // no already-locked handle to write through, and nothing is lost by
        // not having one: holding the lock is itself proof that no second
        // daemon is writing here. Truncated in place rather than staged and
        // renamed, for exactly the reason the unix arm gives — a rename would
        // swap in an inode of its own.
        let path = pidfile(paths);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| BootError::Io {
                path: path.clone(),
                source,
            })?;
        file.write_all(pid.to_string().as_bytes())
            .map_err(|source| BootError::Io {
                path: path.clone(),
                source,
            })?;
        file.sync_all()
            .map_err(|source| BootError::Io { path, source })
    }

    /// Holds a pidfile descriptor this image inherited, already locked.
    ///
    /// Nothing here locks, unlocks, truncates or writes. `file` crossed an
    /// `execve` from the predecessor with its `flock` intact, and taking
    /// ownership of it is the whole job: it keeps the descriptor open for
    /// the rest of this process's life, which is what keeps the lock held.
    /// See [`UnixLock`] for why re-acquiring would be a bug rather than
    /// belt and braces.
    ///
    /// The pidfile's contents need no update either. An `execve` keeps the
    /// pid, so the number the predecessor recorded is this process's own.
    #[cfg(unix)]
    fn from_locked(file: std::fs::File) -> Self {
        Self {
            flock: UnixLock::Adopted(file),
        }
    }

    /// The descriptor the lock lives on, for a handover blob to name.
    ///
    /// See [`UnixLock::as_raw_fd`]: borrowed, and closing it would hand this
    /// `$SHEP_HOME` to whoever asks next.
    #[cfg(unix)]
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.flock.as_raw_fd()
    }

    #[cfg(unix)]
    fn record(&mut self, paths: &ShepPaths, pid: u32) -> Result<(), BootError> {
        use std::io::{Seek, SeekFrom, Write};

        let path = pidfile(paths);
        let file = self.flock.file();
        file.set_len(0).map_err(|source| BootError::Io {
            path: path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| BootError::Io {
                path: path.clone(),
                source,
            })?;
        file.write_all(pid.to_string().as_bytes())
            .map_err(|source| BootError::Io {
                path: path.clone(),
                source,
            })?;
        file.sync_all()
            .map_err(|source| BootError::Io { path, source })
    }
}

/// The apps a successor records in its own registry, which is what the
/// muster roll on disk is written from.
///
/// Every carried sheep's, and **no dog's**. A dog is registered by
/// `dogs::spawn_enabled_dogs` at every boot, from `shep.toml`'s own
/// `enabled_dogs`/`adopted_dogs` lists, and it has never been in the roll:
/// nothing on the dog path ever touches [`FlockRegistry::record`]. Writing
/// one in here would put it there for the first time, and the roll outlives
/// the daemon -- so a later cold boot would restore `metrics` as an
/// ordinary sheep, with no marker, before `spawn_enabled_dogs` got to it,
/// and `shep disable metrics` would not be able to take it back out.
///
/// The filter belongs here rather than at the `record_config` call because
/// this is the one place that still holds the blob's own rows: the registry
/// takes bare [`AppConfig`](shep_core::config::AppConfig)s, which carry no
/// marker to filter on afterwards.
#[cfg(unix)]
fn apps_for_the_roll(
    flock: &[crate::handover::adopt::AdoptedSheep],
) -> Vec<shep_core::config::AppConfig> {
    flock
        .iter()
        .filter(|sheep| sheep.carried.dog().is_none())
        .map(|sheep| sheep.carried.app().clone())
        .collect()
}

/// The handover blob this process was handed, if it is a successor.
///
/// A successor is a shep image an outgoing daemon `execve`d in its own
/// place, handing it a live flock. Its only marker is `SHEP_HANDOVER` in the
/// environment, naming the blob to adopt; an image started any other way has
/// no variable, no blob, and boots normally.
///
/// # Why a refusal is not an error
///
/// By the time this runs the predecessor has already replaced itself, so
/// there is no image left to fall back to and no stop arm to take. That
/// leaves exactly two outcomes for a blob that cannot be used, and this
/// function chooses the second:
///
/// 1. refuse to boot at all, which leaves the operator no shepherd and a
///    flock nothing is watching;
/// 2. say so at `error` level and continue as an ordinary boot, which is
///    correct for the case this actually happens in.
///
/// The case it actually happens in is a STALE VARIABLE: something inherited
/// `SHEP_HANDOVER` from a process that has long since finished its handover,
/// and the blob it names was unlinked at the time. There is no live flock
/// behind it, and a fresh boot is exactly right.
///
/// A genuinely lost blob is the other case, and it is self-limiting rather
/// than dangerous. A real successor inherited the pidfile descriptor too,
/// with its `flock` still held, so the fresh boot this returns to cannot
/// take that lock and stops at [`BootError::AlreadyRunning`] before it
/// restores a thing. The one way a fresh boot proceeds is the one way it
/// should: no pidfile descriptor was inherited, so this was never a real
/// handover.
///
/// Silence is the outcome neither case may have, which is why every refusal
/// logs at `error` with the blob's path and what was wrong with it.
///
/// Unix only, as the whole handover is: Windows has no `execve`, so
/// `Arm::for_daemon` never chooses a handover there and no image can be a
/// successor.
#[cfg(unix)]
#[must_use]
pub(crate) fn successor_handover() -> Option<Successor> {
    let path = PathBuf::from(std::env::var_os(crate::handover::HANDOVER_ENV)?);
    let blob = successor_handover_at(&path)?;
    Some(Successor { path, blob })
}

/// Rebuild everything a successor was handed: the lock, the listener, and
/// every sheep's plumbing.
///
/// The blob is removed once its descriptors are adopted, and only then. One
/// left behind after a refusal is evidence an operator can read; one left
/// after a success is a picture of a handover that has already happened, and
/// the next boot would adopt it again.
///
/// # Errors
///
/// - [`BootError::Adopt`]: a descriptor the blob names is not open in this
///   process, or is not the kind of object it was named as.
///
/// There is no partial success and no fallback here. By the time this runs
/// the predecessor has already `execve`d itself away, so there is no image
/// left to hand the flock back to: a successor that cannot rehydrate refuses
/// to boot rather than serving a flock it only half holds, and the operator's
/// own `shep daemon reload` starts one in its place.
#[cfg(unix)]
fn rehydrate(carried: Successor, paths: &ShepPaths) -> Result<Rehydrated, BootError> {
    let Successor { path, blob } = carried;
    let counters = blob.counters();
    let adopted = crate::handover::adopt::adopt(&blob)
        .map_err(|source| BootError::Adopt(source.to_string()))?;
    crate::handover::adopt::discard_blob(&path);
    let _ = paths;
    let reloads = blob.reloads().to_vec();
    Ok((
        PidfileLock::from_locked(adopted.pidfile),
        Listener::from_unix_listener(adopted.listener),
        (adopted.sheep, counters, reloads),
    ))
}

/// A handover blob, and where it was read from.
///
/// The path is kept because the successor unlinks the blob once it has
/// adopted what it describes, and only then: a blob left behind after a
/// refusal is evidence, while one left behind after a success would be
/// adopted again by the next boot.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct Successor {
    /// Where the blob was read from.
    pub path: PathBuf,
    /// What it said.
    pub blob: crate::handover::Handover,
}

/// [`successor_handover`], against a caller-named path.
///
/// Split out so a test can drive every refusal without touching the
/// environment, which is process-global and, since edition 2024, unsafe to
/// write.
#[cfg(unix)]
fn successor_handover_at(path: &Path) -> Option<crate::handover::Handover> {
    match crate::handover::Handover::read(path) {
        Ok(blob) => Some(blob),
        Err(error) => {
            tracing::error!(
                path = %path.display(),
                %error,
                "this process was handed a handover blob it cannot use, and is booting as if \
                 it were fresh; if a flock was running, it is no longer supervised"
            );
            None
        }
    }
}

/// What, if anything, owns this home's pidfile lock.
///
/// Proof of life is the pidfile LOCK, never the pidfile's contents. A live
/// daemon holds that lock for its whole run and the kernel drops it on
/// process death, `SIGKILL` included, so a failure to acquire it is the only
/// evidence that cannot be faked by a stale file whose pid has since been
/// reused.
///
/// Answers a question; never claims the home. A lock this acquires is
/// released before the call returns.
///
/// Both platform arms of `PidfileLock` are reachable through this, and it
/// needs no `cfg` of its own: unix contends on the pidfile's `flock` and
/// Windows on a sibling `.lock` file's share mode, and both report a
/// contended lock as [`BootError::AlreadyRunning`].
///
/// # Errors
/// - [`BootError::Io`] — the pidfile could not be opened, created or read.
///   A contended lock is NOT an error here; it is [`Shepherd::Running`] or
///   [`Shepherd::Booting`].
pub fn daemon_liveness(paths: &ShepPaths) -> Result<Shepherd, BootError> {
    match PidfileLock::acquire(paths) {
        // We took it, so nobody else holds it. Released by this `drop`
        // rather than at the end of the scope, so that the window in which
        // a question-asker holds a claim on someone else's home is as
        // short as the type allows.
        Ok(lock) => {
            drop(lock);
            Ok(Shepherd::Absent)
        }
        Err(BootError::AlreadyRunning { pid: Some(pid) }) => Ok(Shepherd::Running(pid)),
        Err(BootError::AlreadyRunning { pid: None }) => Ok(Shepherd::Booting),
        // A home whose layout was never created cannot be holding a lock, so
        // this is an absence and not a failure. `init_dirs` makes `pids/` on
        // every boot, so its absence means no daemon has ever run here, which
        // is precisely what `Absent` says. Reported as an error instead, a
        // `shep kill` against a fresh `$SHEP_HOME` exits `Failure` rather than
        // `DaemonUnreachable`, which is a worse answer to a correct question.
        //
        // Narrow on purpose. Only `NotFound`, and only for a path under
        // `pids/`. A permissions error or a corrupt lock file is a real
        // failure and must still say so.
        Err(BootError::Io {
            ref path,
            ref source,
        }) if source.kind() == ErrorKind::NotFound && path.starts_with(&paths.pids) => {
            Ok(Shepherd::Absent)
        }
        Err(other) => Err(other),
    }
}

/// What [`daemon_liveness`] found holding a home's pidfile lock.
///
/// Three states, not two, because "nothing is running" and "something is
/// starting up" call for opposite actions and an `Option<u32>` cannot tell
/// them apart. Both would read as `None`: [`boot`] takes the lock at
/// `PidfileLock::acquire` and records its pid a few statements later, and
/// binding the socket happens in between, stale-socket recovery included.
/// A caller that read that window as an absence would refuse with the wrong
/// reason, or start a second daemon that then dies unable to take the lock.
///
/// Deliberately NOT `#[non_exhaustive]`, unlike [`BootError`]. The set is
/// closed by the mechanism rather than by today's implementation: the lock
/// is either free or held, and a holder either has written its pid or has
/// not. There is no fourth thing for a future boot step to add, and callers
/// get exhaustiveness checking on a decision where a missed arm means
/// signalling the wrong process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shepherd {
    /// Nothing holds this home's pidfile lock.
    ///
    /// A stale pidfile naming a long-dead pid still reads as this, which is
    /// the whole point of asking the lock rather than the file.
    Absent,
    /// A shepherd holds the lock and recorded this pid.
    Running(u32),
    /// A shepherd holds the lock but has not recorded a pid yet.
    ///
    /// It is alive and owns the home, so it must not be treated as absent,
    /// but there is no pid to signal. A caller should report that a
    /// shepherd is starting rather than guess at either.
    Booting,
}

/// The socket this daemon binds: the layout default, or a config override
#[must_use]
pub(crate) fn socket_path(paths: &ShepPaths, override_path: Option<&Path>) -> PathBuf {
    match override_path {
        Some(path) => path.to_path_buf(),
        None => paths.socket.clone(),
    }
}

/// Warns (does not refuse) when `socket`'s directory is reachable by anyone
/// but its owner. The default layout's `run/` is always `0700` by the time
/// [`bind_socket`] runs (via [`init_dirs`]); this only fires for a
/// `[daemon].socket` override the operator pointed somewhere looser, which
/// forfeits the 0700 guarantee the security model otherwise rests on. That
/// is the operator's call to make, not this function's to block.
#[cfg(unix)]
fn warn_if_socket_dir_is_loose(socket: &Path) {
    let Some(parent) = socket.parent() else {
        return;
    };
    let Ok(metadata) = std::fs::metadata(parent) else {
        return;
    };
    if metadata.permissions().mode() & 0o022 != 0 {
        tracing::warn!(
            path = %parent.display(),
            "control-socket directory is group- or world-writable; \
             the 0700 guarantee only covers the default $SHEP_HOME/run"
        );
    }
}

/// Binds the control socket, recovering from a crashed daemon's leftovers
///
/// # Errors
/// - [`BootError::AlreadyRunning`] — a live daemon answered on the socket.
/// - [`BootError::Io`] — bind, probe, or unlink failed.
// `needless_return` fires on the Windows arm's explicit `return`, which is
// load-bearing: the `cfg(unix)` block after it is the rest of the function,
// and the two cannot be an if/else over `cfg!` because the unix arm names
// types (`nix`'s errno, `std::os::unix::net`) that do not exist to name on
// Windows at all.
#[allow(clippy::needless_return)]
pub(crate) fn bind_socket(paths: &ShepPaths, socket: &Path) -> Result<Listener, BootError> {
    // Windows takes an entirely different route through this function, and
    // the reason is worth stating: almost everything below exists to cope
    // with a socket being a FILE. A named pipe is not one. It has no
    // `sun_path` length limit, no containing directory whose mode could be
    // loose, and — decisively — nothing left on disk when its owner dies, so
    // there is no stale artefact to probe for and no recovery to perform.
    //
    // The mutual exclusion the whole probe-and-recover dance is protecting
    // is instead enforced by the OS: `Listener::bind` passes
    // `first_pipe_instance`, so a second daemon on the same `$SHEP_HOME` is
    // refused by the kernel at create time rather than after a race this
    // code would have to adjudicate. `PidfileLock` still runs ahead of this
    // on both platforms and is still the primary guard; this is a second,
    // independent one that unix simply cannot have.
    #[cfg(windows)]
    {
        /// `ERROR_ACCESS_DENIED` — what `first_pipe_instance` reports when
        /// the pipe name already has an owner. The one error that means
        /// "another daemon", rather than a genuine I/O failure.
        const ERROR_ACCESS_DENIED: i32 = 5;

        return match Listener::bind(socket) {
            Ok(listener) => Ok(listener),
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                Err(BootError::AlreadyRunning {
                    pid: read_pidfile(paths)?,
                })
            }
            Err(source) => Err(BootError::Io {
                path: socket.to_path_buf(),
                source,
            }),
        };
    }

    #[cfg(unix)]
    {
        // Ahead of the bind, because the kernel's own refusal names neither the
        // limit nor `$SHEP_HOME`. `sun_path` is 104 bytes on macOS and 108 on
        // Linux, and it holds a NUL terminator, so the usable length is one less.
        const SUN_PATH_CAPACITY: usize = if cfg!(target_os = "linux") { 108 } else { 104 };
        let len = socket.as_os_str().as_encoded_bytes().len();
        if len >= SUN_PATH_CAPACITY {
            return Err(BootError::SocketPathTooLong {
                path: socket.to_path_buf(),
                len,
                limit: SUN_PATH_CAPACITY - 1,
            });
        }
        warn_if_socket_dir_is_loose(socket);
        match Listener::bind(socket) {
            Ok(listener) => Ok(listener),
            Err(err) if err.kind() == ErrorKind::AddrInUse => {
                // EADDRINUSE only says the path exists. Probe it: a live daemon's
                // listener accepts at the kernel level even mid-accept, while a
                // file left behind by a crash (or a reboot) refuses. This is the
                // load-bearing step for the reboot-resurrect scenario (§13.4).
                //
                // Only one direction of that is proof, and the asymmetry is
                // deliberate. A socket answers for as long as ANY descriptor for
                // it stays open, and `fork` copies every descriptor a process
                // holds: a child a dying daemon forked and has not yet exec'd
                // goes on answering on its behalf until close-on-exec clears the
                // copy. So a refusal proves staleness, while an answer is only
                // grounds to refuse this boot — never evidence that a healthy
                // peer is there. Refusing a boot that could have proceeded costs
                // an operator one retry; binding over a socket a daemon is still
                // serving on costs two daemons one flock.
                match std::os::unix::net::UnixStream::connect(socket) {
                    Ok(_) => Err(BootError::AlreadyRunning {
                        pid: read_pidfile(paths)?,
                    }),
                    Err(probe)
                        if matches!(
                            probe.kind(),
                            ErrorKind::ConnectionRefused | ErrorKind::NotFound
                        ) =>
                    {
                        std::fs::remove_file(socket).map_err(|source| BootError::Io {
                            path: socket.to_path_buf(),
                            source,
                        })?;
                        Listener::bind(socket).map_err(|source| BootError::Io {
                            path: socket.to_path_buf(),
                            source,
                        })
                    }
                    Err(source) => Err(BootError::Io {
                        path: socket.to_path_buf(),
                        source,
                    }),
                }
            }
            Err(source) => Err(BootError::Io {
                path: socket.to_path_buf(),
                source,
            }),
        }
    }
}

/// Environment variable naming the inherited readiness descriptor.
///
/// Set by the CLI on the child it re-execs detached (spec §3); read back and
/// adopted (`unsafe fn` `crate::sys::adopt_fd`) by that same CLI, not by
/// anything in this crate — shep-daemon never parses this variable or sees
/// a raw fd number itself, only the already-adopted [`std::fs::File`] that
/// lands in [`BootOptions::ready_fd`].
///
/// Public with no caller yet, for the same reason `crate::sys::adopt_fd`
/// is: both halves of this handshake belong to `shep-cli` and neither is
/// written. Crate-private it has no use at all, and "unused constant" is a
/// worse description of it than this paragraph.
pub const READY_FD_ENV: &str = "SHEP_READY_FD";

/// What the daemonizing parent reads off the readiness pipe.
///
/// Crate-private even though the paragraph above is not, because `write_ready`
/// does use this type — so narrowing it costs nothing today. What would
/// reopen it is the CLI-side reader deciding to deserialize into this exact
/// struct rather than its own; until that exists, the wire format below is
/// the contract, not the Rust type.
// wire format: shep-cli parses this line; changing it is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonReady {
    /// This daemon's OS pid.
    pub(crate) pid: u32,
    /// This daemon's crate version.
    pub(crate) version: String,
}

/// Writes one newline-terminated JSON readiness line to `pipe` and closes
/// it — dropping `pipe` at the end of this call is the parent's own EOF
/// signal that there is nothing more to read.
///
/// Takes an already-adopted [`std::fs::File`], never a raw fd: adoption
/// (`unsafe fn` `crate::sys::adopt_fd`) is a fd-inheritance concern
/// (`sys.rs`'s rationale essay) with a process-wide ordering precondition
/// that only the CLI's `main` can discharge — this function never touches a
/// bare descriptor, only the safe `File` handed to it.
///
/// # Errors
/// - [`BootError::ReadyWrite`] — the write failed (carries the OS error).
fn write_ready(mut pipe: std::fs::File, ready: &DaemonReady) -> Result<(), BootError> {
    use std::io::Write;

    // DaemonReady is a plain {u32, String} pair: serde_json::to_string only
    // fails on things neither field can ever be (non-string map keys, NaN
    // floats), so this can't error in practice.
    let mut line = serde_json::to_string(ready).expect("DaemonReady always serializes");
    line.push('\n');
    pipe.write_all(line.as_bytes())
        .map_err(BootError::ReadyWrite)?;
    Ok(())
}

/// Options the CLI hands the daemon at boot.
#[derive(Debug, Default)]
pub struct BootOptions {
    /// Overrides the layout's default control-socket path.
    pub socket: Option<PathBuf>,
    /// The inherited readiness pipe, if the CLI daemonized us (see
    /// [`READY_FD_ENV`]) — already adopted into an owned
    /// [`std::fs::File`] by the CALLER before this struct is ever
    /// constructed.
    ///
    /// Adoption (`unsafe fn` `crate::sys::adopt_fd`) is deliberately not
    /// this crate's job: its ordering precondition ("call this before the
    /// process opens any descriptor of its own") is process-wide, and
    /// [`boot`] — already running inside a tokio runtime with its own live
    /// poller fds by the time it is called — cannot discharge that on its
    /// own caller's behalf. The intended caller is the CLI's `main`, as its
    /// literal first fd-touching statement, before a tokio runtime even
    /// exists (Phase 3). Because this field already carries a safe owned
    /// handle, [`boot`] never constructs a `File` from a raw number and
    /// this crate's unsafe stays confined to `sys.rs` (IR-22).
    pub ready_fd: Option<std::fs::File>,
    /// Restore the muster roll if one exists (spec §9's `shep muster`).
    pub restore: bool,
    /// Longest a cron worker parks before re-reading the wall clock, from
    /// `[daemon] max_cron_sleep`. Unset means the default (`cron`'s
    /// crate-private `DEFAULT_MAX_CRON_SLEEP`, applied by [`boot`] and
    /// nowhere else) — the same `Option` shape [`Self::socket`] uses, so
    /// nothing between `shep.toml` and here invents a value.
    pub max_cron_sleep: Option<Duration>,
    /// Where to report readiness once the muster restore has finished, for
    /// an init system supervising this process directly. `None` — the
    /// ordinary case — reports nothing.
    ///
    /// The resolved address rather than a bool, and not read from the
    /// environment inside this crate: `std::env::set_var` is `unsafe` in
    /// edition 2024 and this crate is `#![deny(unsafe_code)]`, so a boot
    /// test could not establish an ambient `$NOTIFY_SOCKET` to observe the
    /// ordering against. The CLI reads the variable
    /// (`crate::notify::NOTIFY_SOCKET_ENV`) once, where it already reads
    /// every other `SHEP_*` override.
    ///
    /// Distinct from [`Self::ready_fd`], which the two share nothing with
    /// but a name: that one answers a *parent shep process* that daemonized
    /// this one and is waiting to exit, and it is written the moment the
    /// socket binds so a slow muster cannot make that parent think the boot
    /// failed. This one answers an init system that is supervising this
    /// process itself, and is written last, so the unit goes green only
    /// once the flock is actually back. Both may be set, but in practice
    /// never are: whichever one is supervising, the other is not.
    pub notify_socket: Option<OsString>,
    /// Dogs to start once the flock is back, in the order given.
    ///
    /// Assembled by the caller from `[daemon] enabled_dogs` and
    /// `[daemon] adopted_dogs`, so shep-daemon never reads `shep.toml`
    /// itself — the same division [`Self::socket`] and
    /// [`Self::max_cron_sleep`] already follow.
    pub dogs: Vec<DogSpec>,
    /// Wipe the in-memory flock registry before [`RunningDaemon::run`]'s
    /// teardown writes the final muster roll, so that roll always describes
    /// an empty flock — regardless of whether the session ended through an
    /// explicit `Stop`/`Delete`/`KillDaemon` sequence or by a signal caught
    /// inside `run` itself, which no caller-level request can precede.
    ///
    /// `false` for every real `runtime`/`daemon` boot: the roll surviving
    /// with the flock's true running state is what lets `shep muster`
    /// restore it after a reboot. `true` only for `shep dev`'s isolated
    /// session, where nothing here should ever be worth mustering — see
    /// `crate::snapshot::FlockRegistry::clear`'s own doc for the shutdown
    /// gap this closes.
    pub delete_flock_on_shutdown: bool,
    /// Let SIGHUP replace this process's image with a successor holding the
    /// same flock, rather than stopping gracefully.
    ///
    /// `true` for every boot the `shep` binary performs, which is what makes
    /// `shep daemon reload` a handover rather than a stop-and-start.
    ///
    /// **Defaults to `false`, and the default is the safe direction rather
    /// than the polite one.** A handover `execve`s the file this process was
    /// launched from, so it is only ever correct where that file IS the shep
    /// binary. A test harness, or any program embedding this crate, is
    /// launched from something else entirely, and a SIGHUP there would
    /// replace the whole program with a fresh copy of itself, which is not a
    /// subtle failure but a process that re-runs from the top forever.
    /// A caller that opts in is asserting it is the shepherd binary.
    ///
    /// The graceful stop is what a boot that has not opted in does with
    /// SIGHUP, which is also what a handover that cannot proceed falls back
    /// to. Unix only in effect: Windows has no `execve` and every arm of the
    /// reload there is a stop-and-start (spec H5).
    pub handover: bool,
}

/// Brings the daemon up: signal handlers, layout, roll restore, bus,
/// supervisor, socket, readiness report.
///
/// Step order here is deliberate and load-bearing, not incidental:
/// 1. install signal handlers — before the socket exists, so there is no
///    window where the socket is already live but an ordinary `kill -USR2`
///    (SIGUSR2's default disposition is to terminate) would still kill the
///    daemon instead of rotating logs;
/// 2. layout, then the crate-private `PidfileLock::acquire` — this is the
///    FIRST thing that can fail with [`BootError::AlreadyRunning`], before
///    `bind_socket` ever runs, and it is what makes that call race-free
///    against another process booting the same `$SHEP_HOME` concurrently
///    (see `PidfileLock`'s own doc, next to its definition in this file) —
///    then socket bind (with stale-socket recovery, spec §6), then
///    `PidfileLock::record` into the now-held-for-this-process's-whole-life
///    lock;
/// 3. report readiness, now that the socket is actually bound (spec §3) —
///    not once the whole flock is restored, so a slow muster can't make the
///    parent think boot failed. `options.ready_fd`, if set, already names
///    an owned [`std::fs::File`] the CALLER adopted before ever
///    constructing [`BootOptions`] (see that field's own doc) — this step
///    is a plain write, no fd adoption happens inside `boot` itself;
/// 4. bus, supervisor, muster restore, [`BootOptions::dogs`], snapshot
///    writer, `RpcContext` — and the point where step 1's SIGUSR2 listener
///    is handed the supervisor it reopens through, this being the first
///    moment one exists. See `install_signals`'s own doc, next to its
///    definition in this file, for why that seam is a channel rather than
///    an argument, and why the gap between the two steps drops no signal.
///    The dogs come up strictly between the restore and the snapshot
///    writer, and both halves of that placement are load-bearing: after
///    the restore, because a metrics dog that started first would answer
///    for an empty flock for the whole restore window, and a bark dog
///    would raise a `process.start` alert for every sheep the roll brings
///    back; before step 5's readiness report, because `Type=notify` going
///    green is meant to mean the whole daemon — flock and dogs alike — is
///    up, the same reasoning that put the restore itself inside that
///    promise;
/// 5. report readiness to an init system supervising this process directly
///    ([`BootOptions::notify_socket`], `crate::notify`) — last of all,
///    which is the opposite of step 3 and deliberately so: that one answers
///    a parent shep waiting to exit, this one decides when a unit goes
///    green, and a unit that goes green at exec time describes a flock that
///    is not up yet.
///
/// # Errors
/// - [`BootError::Io`] — a boot filesystem step failed, or the OS refused
///   to register a signal handler.
/// - [`BootError::AlreadyRunning`] — another daemon already holds the
///   pidfile lock, or (belt-and-suspenders, for a peer not participating in
///   that lock) answered on the socket.
/// - [`BootError::ReadyWrite`] — the readiness line could not be written to
///   `options.ready_fd`.
/// - [`BootError::Snapshot`] — `options.restore` was set, a roll exists, but
///   it could not be read or parsed.
pub async fn boot<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    mut options: BootOptions,
) -> Result<RunningDaemon, BootError> {
    // Before anything else, because it reads the current directory and
    // that is only the startup directory until something moves it. A
    // handover execs this path rather than `current_exe()`; see
    // `handover::exec_target` for why those differ exactly when an
    // operator has upgraded. `#[cfg(unix)]` because the whole handover
    // module is: Windows has no `execve` and takes the stop arm.
    #[cfg(unix)]
    crate::handover::record_launch_path();

    // Copied out up front, purely a `bool`, so nothing later in this fn has
    // to remember to read it off `options` before that struct is consumed.
    let delete_flock_on_shutdown = options.delete_flock_on_shutdown;

    // 1. Install signal handlers before the socket (or anything else
    //    observable) exists — see this fn's own doc.
    let (shutdown, shutdown_rx) = watch::channel(false);
    let shutdown = Arc::new(shutdown);
    #[cfg(unix)]
    let (signals, connect_supervisor, connect_handover) =
        install_signals(Arc::clone(&shutdown), paths.clone())?;
    #[cfg(windows)]
    let (signals, connect_supervisor) = install_signals(Arc::clone(&shutdown), paths.clone())?;

    // 2. Layout, then claim exclusive ownership of $SHEP_HOME BEFORE
    //    touching the socket at all — see `PidfileLock`'s own doc for why
    //    this is what actually closes the concurrent-boot race a bare
    //    probe-then-recover sequence can't. Held across the whole
    //    bind-and-recover sequence, and for the rest of this daemon's life
    //    (kept in `RunningDaemon`, dropped only at the end of `run`).
    init_dirs(&paths)?;
    let socket = socket_path(&paths, options.socket.as_deref());
    // A successor takes neither the lock nor the address: it inherited both,
    // still held, in the same act that made it this process. Rebinding would
    // race the predecessor's own socket file and lose whatever connection a
    // client had already made, and re-locking would mean releasing first.
    #[cfg(unix)]
    let (mut pidfile_lock, listener, inherited) = match successor_handover() {
        Some(carried) => {
            let (lock, listener, flock) = rehydrate(carried, &paths)?;
            (lock, listener, Some(flock))
        }
        None => (
            PidfileLock::acquire(&paths)?,
            bind_socket(&paths, &socket)?,
            None,
        ),
    };
    #[cfg(windows)]
    let (mut pidfile_lock, listener) =
        (PidfileLock::acquire(&paths)?, bind_socket(&paths, &socket)?);
    let pid = std::process::id();
    pidfile_lock.record(&paths, pid)?;

    // 3. Report readiness now that the socket is bound. `options.ready_fd`
    //    is already an owned File adopted by the caller — see this fn's
    //    own doc and `BootOptions::ready_fd`'s doc — so this is nothing
    //    more than a write. TAKEN out of `options` rather than moved out of
    //    it: a partial move leaves the struct unborrowable, and step 4 hands
    //    it whole to `max_cron_sleep` — see that fn's own doc for why the
    //    field is not picked out here.
    if let Some(pipe) = options.ready_fd.take() {
        let ready = DaemonReady {
            pid,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_ready(pipe, &ready)?;
    }

    // 4. Bus, supervisor, muster restore, snapshot writer, context.
    let events = new_bus();
    // Spawned the moment the bus exists, ahead of the supervisor that will
    // ever emit anything onto it: this is a subscriber like the snapshot
    // writer, not a branch inside the engine (see `spawn_dog_watch`'s own
    // doc), and giving it a receiver early means it can never miss an
    // `Errored` a dog reaches during boot's own restore step.
    let dog_watch = spawn_dog_watch(events.subscribe(), events.clone(), paths.barks.clone());
    let (breach_tx, breach_rx) = mpsc::channel(EXTRAS_REPORT_CAPACITY);
    let (live_tx, live_rx) = mpsc::channel(EXTRAS_REPORT_CAPACITY);
    let extras = Extras::real(
        ExtrasReports {
            breaches: breach_tx,
            liveness: live_tx,
        },
        max_cron_sleep(&options),
    );
    // Taken before `extras` is moved into the builder. One `StatsState`, two
    // owners, for the reason `Extras::enforcer` is shared the same way: the
    // extras decide which sheep is watched and record the periodic CPU
    // baseline, the RPC layer reads a live sample against it, and a second
    // state would leave one of the two reading an empty watch set.
    let stats = Arc::clone(&extras.stats);
    let builder = SupervisorBuilder::new(runner, paths.clone(), events.clone()).extras(extras);
    // A successor installs the flock it inherited rather than spawning one:
    // every sheep keeps the pid, the id, the epoch and the history it had a
    // moment ago, and nothing here signals, spawns or reopens anything. From
    // each sheep's own side the shepherd was never away.
    #[cfg(unix)]
    let mut carried_apps = Vec::new();
    // Read before the match consumes `inherited`, and deliberately not
    // derived from `carried_apps` afterwards: a successor that inherited an
    // EMPTY flock is still a successor, and the two are only the same
    // question when the predecessor had at least one sheep. See the restore
    // guard below for what the difference costs.
    #[cfg(unix)]
    let inherited_flock = inherited.is_some();
    #[cfg(unix)]
    let supervisor = match inherited {
        Some((flock, counters, reloads)) => {
            // Read before the flock is moved: the registry below is rebuilt
            // from these, and the roll would otherwise be written empty.
            carried_apps.extend(apps_for_the_roll(&flock));
            builder
                .spawn_adopted(flock, counters, reloads)
                .map_err(|source| BootError::Adopt(source.to_string()))?
        }
        None => builder.spawn(),
    };
    #[cfg(windows)]
    let supervisor = builder.spawn();
    // Ordered, not stylistic: the reporter needs the handle the builder
    // returns, and the actor must never own a receiver a subsystem feeds.
    //
    // Its `JoinHandle` is discarded, which DETACHES the task rather than
    // stopping it — and that closes a cycle worth naming, because nothing
    // here breaks it. The reporter holds a `SupervisorHandle`, so the actor's
    // mailbox can never reach zero senders while the reporter lives; the
    // reporter itself only ends once BOTH report senders have dropped, and the
    // enforcer holding one of them lives as long as the actor's registry does.
    // What actually ends both is `RunningDaemon::run`'s explicit
    // `SupervisorHandle::shutdown` (teardown step 4), which stops the actor by
    // command instead of by sender count and drops the registry with it. That
    // call is load-bearing for this reason as well as for the kill ladder it
    // is named after; a future teardown that relied on senders going away
    // would hang here instead.
    spawn_extras_reporter(breach_rx, live_rx, supervisor.clone());

    // The other half of step 1's SIGUSR2 listener, which has been parked on
    // this since before the socket existed, waiting for the handle it reopens
    // through — see `install_signals`'s own doc for why the wait is what the
    // step order forces and why it drops no signal. An `Err` would mean that
    // listener is already gone, which cannot happen while `signals` — moved
    // into the `RunningDaemon` below, and the only thing that aborts it — is
    // still alive.
    let _ = connect_supervisor.send(supervisor.clone());

    // The other half of step 1's SIGHUP task, parked on this since before
    // the socket existed. It carries the two descriptors a handover blob has
    // to name, which only this function knows: an fd number means nothing
    // outside the process that owns it, and the supervisor has never seen
    // either of them.
    #[cfg(unix)]
    let _ = connect_handover.send(options.handover.then(|| HandoverSeam {
        supervisor: supervisor.clone(),
        fds: crate::handover::DaemonFds {
            listener: listener.as_raw_fd(),
            pidfile: pidfile_lock.as_raw_fd(),
        },
        paths: paths.clone(),
    }));

    let registry = FlockRegistry::new();

    // A successor rebuilds the registry from the blob rather than from the
    // roll, and skips the restore entirely. Both halves matter. The registry
    // is what the snapshot writer builds the muster roll from, so a
    // successor that left it empty would overwrite a good roll with an empty
    // one within seconds of taking over. And a restore would START whatever
    // the roll records as running, which for a flock that never stopped
    // means a second copy of every sheep that happens to be down.
    //
    // An EMPTY inherited flock is the case that makes `inherited_flock` a
    // fact about the boot rather than a count of the sheep. `shep daemon
    // reload` against an idle shepherd carries nothing, and a handover skips
    // the predecessor's teardown, so the roll on disk is whatever the last
    // periodic write left. Deriving the flag from `carried_apps` would run
    // the restore there and start sheep that had just been deleted.
    #[cfg(unix)]
    for app in &carried_apps {
        registry.record_config(app);
    }
    #[cfg(windows)]
    let inherited_flock = false;

    if options.restore && !inherited_flock {
        restore_flock(&paths, &registry, &supervisor).await?;
    }

    // After the restore, before step 5's readiness report — see this fn's
    // own doc, step 4, for why both halves of that placement are
    // load-bearing. Never fails the boot: a dog that cannot be spawned is a
    // monitoring gap, not an outage, and `spawn_enabled_dogs` warns and
    // carries on rather than propagating anything here.
    crate::dogs::spawn_enabled_dogs(&options.dogs, &paths, &supervisor, &events).await;

    // Built here rather than inside the `RpcContext` below because the watch
    // on the next line shares it. Still empty, and still deliberately not
    // carried across a handover: a successor has refused nobody yet, and a
    // dog it can talk to is not stale by any definition it could apply.
    let dog_refusals = crate::dogs::DogRefusals::new();
    // Built beside the refusals and for the same reason: the watch on the
    // next line reads it, and the connection layer writes it. Not carried
    // across a handover either — a successor has been connected to by
    // nobody, and a pid it has never seen is one it must not claim has
    // never called.
    //
    // That last clause was a wish rather than a description until
    // `PEER_CONTACT_WARMUP` existed. An empty map answered `Contact::None`
    // for every pid, which routes to `Silence::Unreachable` and prints the
    // reinstall verdict, so for its first seconds a successor told every dog
    // carried across the reload that the binary on disk could not reach shep.
    // The warm-up is what makes starting empty safe: until this map has been
    // listening long enough for an absence to mean something, it answers
    // `Contact::Unknown` and the ladder names both candidates instead.
    let peer_contacts = crate::dogs::PeerContacts::new();
    // Spawned at every boot, INCLUDING a successor's after an `execve` --
    // that is why it is anchored here and not to a dog's own spawn (see
    // `spawn_silent_dog_watch`'s doc). It restarts a dog that has been
    // running without ever answering this shepherd, which costs a merely
    // slow dog one restart it did not need; the tradeoff is argued at
    // `record_silent_dog`.
    let silent_dog_watch = crate::dogs::spawn_silent_dog_watch(
        supervisor.clone(),
        dog_refusals.clone(),
        peer_contacts.clone(),
        events.clone(),
    );

    let writer = spawn_snapshot_writer(
        paths.snapshot.clone(),
        supervisor.clone(),
        registry.clone(),
        events.subscribe(),
    );

    let ctx = RpcContext {
        supervisor,
        events,
        registry,
        snapshot_path: paths.snapshot.clone(),
        daemon_config: paths.daemon_config.clone(),
        paths: paths.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        dog_refusals,
        peer_contacts,
        pid,
        shutdown,
        stats,
    };

    // 5. The flock is back and the plane is assembled, so this daemon is
    //    now what a unit ordered `After=` it expects to find. A failure is
    //    a `warn!` and the boot continues: the daemon is fully functional
    //    and only systemd's knowledge of it is wrong, which systemd's own
    //    `TimeoutStartSec` reports honestly — killing a working daemon over
    //    an undeliverable datagram would be the worse outcome.
    //
    //    Unix only, because `$NOTIFY_SOCKET` is systemd's protocol over a
    //    unix datagram socket and there is nothing on Windows for it to
    //    address. The field stays on `BootOptions` on both platforms rather
    //    than being gated out of the struct: `shep-cli` reads the variable
    //    in one place for every target, and a config type whose SHAPE
    //    changes per platform makes every caller carry the gate instead of
    //    one call site doing so. A Windows daemon simply never has anything
    //    to report readiness to — the equivalent, once a real Windows
    //    service exists, is `SetServiceStatus`, which is Tier B work and
    //    deliberately not faked here.
    #[cfg(unix)]
    if let Some(target) = options.notify_socket.as_deref()
        && let Err(err) = crate::notify::notify(target)
    {
        tracing::warn!(
            %err,
            "readiness could not be reported to $NOTIFY_SOCKET; the flock is up regardless"
        );
    }

    Ok(RunningDaemon {
        ctx,
        listener,
        writer,
        dog_watch,
        silent_dog_watch,
        paths,
        socket,
        // Held from here into `RunningDaemon` — `watch::Sender::send` is a
        // silent no-op whenever the receiver count is zero (confirmed
        // against tokio's own source), and `ctx.shutdown()` is callable the
        // instant a caller has `Self::context`, racing ahead of `run` ever
        // being polled. See `RunningDaemon::shutdown_rx`'s own doc.
        shutdown_rx,
        // Held from here into `RunningDaemon` too, and through the whole of
        // `run` — its `Drop` aborts every signal-listener task on every
        // exit from this point on, including an early `?` return from a
        // later step in THIS function. See `SignalTasks`'s own doc.
        signals,
        // Held from here into `RunningDaemon` and through the whole of
        // `run`: this is what keeps `$SHEP_HOME` exclusively claimed for
        // this daemon's entire life, not just its boot — dropping it (at
        // the end of `run`, or on an early `?`-return from a LATER step in
        // THIS function) is the only thing that releases the `flock`. See
        // `PidfileLock`'s own doc.
        pidfile_lock,
        delete_flock_on_shutdown,
    })
}

/// The cron sleep bound this boot runs with: [`BootOptions::max_cron_sleep`],
/// or [`DEFAULT_MAX_CRON_SLEEP`] when `shep.toml` named none.
///
/// A named function rather than an `unwrap_or` inline in [`boot`] only so the
/// application has a seam a test can stand on. It is still the ONE place that
/// constant is applied, and a second application anywhere else is how two
/// supposedly identical constants drift apart: `shep-core` carries the floor
/// and never the default, the daemon carries the default and never the floor.
///
/// It reads the whole [`BootOptions`] rather than the one field because the
/// field is what [`boot`] would otherwise have to pick out, and picking the
/// wrong one there is a mistake no test in this crate could catch: the only
/// behavioural trace `max_cron_sleep` leaves is how often a cron worker wakes,
/// and a wakeup is observable only through the [`Clock`](crate::cron::Clock)
/// seam that [`Extras::real`] fixes to the system clock. Reading the struct
/// here leaves nothing at the call site to get wrong.
fn max_cron_sleep(options: &BootOptions) -> Duration {
    options.max_cron_sleep.unwrap_or(DEFAULT_MAX_CRON_SLEEP)
}

/// Reads the muster roll (if one exists) and starts every app it restores.
///
/// One line over [`snapshot::muster`], which holds the whole restore rule and
/// its rationale — a missing roll, a corrupt one, a rejected entry, an app
/// the flock already has. The `Muster` request an operator sends runs that
/// same function, so the restore that happens unattended after a reboot is
/// the one an operator exercises by hand.
///
/// What boot supplies that the operator's call does not is an empty flock:
/// nothing here can already be running, so every restorable app is started
/// and the names come back describing exactly what boot just did. It discards
/// them because there is no one at this end of a boot to report them to.
async fn restore_flock(
    paths: &ShepPaths,
    registry: &FlockRegistry,
    supervisor: &SupervisorHandle,
) -> Result<(), BootError> {
    snapshot::muster(&paths.snapshot, registry, supervisor).await?;
    Ok(())
}

/// A booted daemon, not yet serving: everything [`boot`] assembled, handed
/// back so the caller can read [`Self::context`] before driving [`Self::run`].
#[derive(Debug)]
pub struct RunningDaemon {
    ctx: RpcContext,
    listener: Listener,
    writer: SnapshotWriter,
    // Parks on a broadcast receiver until this handle aborts it (see
    // `spawn_dog_watch`'s own doc for why holding the handle, not sender
    // count, is what makes that deterministic). Stopped in `run`'s teardown
    // step 1 alongside `writer`: both are bus subscribers with no further
    // reason to run once serving ends.
    dog_watch: JoinHandle<()>,
    // Parks on a timer rather than on the bus, and is stopped the same way
    // and at the same moment: nothing may ask for a dog's restart once
    // serving has ended. See `spawn_silent_dog_watch`'s own doc for why the
    // task is anchored to boot at all.
    silent_dog_watch: JoinHandle<()>,
    paths: ShepPaths,
    socket: PathBuf,
    // Held from `boot` onward, not created fresh in `run`: `watch::Sender::send`
    // is a silent no-op (`Err`, value left unchanged — confirmed against
    // tokio's own source) whenever the receiver count is zero. `ctx.shutdown()`
    // is callable the moment `boot` returns [`Self::context`], which can race
    // ahead of `run` ever being polled; without a receiver alive for that
    // whole window, an early `ctx.shutdown()` is silently lost and `run` hangs
    // forever waiting on a signal that already fired. (Caught by
    // `boot_restores_a_saved_flock_and_tears_down_in_order`, which calls
    // `ctx.shutdown()` immediately after `tokio::spawn(daemon.run())` with no
    // guaranteed ordering between the two.)
    shutdown_rx: watch::Receiver<bool>,
    // Installed by `boot` (not `run` — see `boot`'s own doc for why: SIGUSR2
    // must be handled before the socket exists), kept alive through `run`'s
    // whole serving lifetime, and dropped only once teardown finishes —
    // `SignalTasks`'s own `Drop` is what actually stops these tasks.
    signals: SignalTasks,
    // Acquired by `boot` before the socket was ever bound, kept alive
    // through `run`'s whole serving lifetime, and dropped only once
    // teardown finishes — releasing this `flock` is what lets a NEXT
    // daemon's own `PidfileLock::acquire` succeed. See that type's own doc.
    pidfile_lock: PidfileLock,
    // Copied from `BootOptions::delete_flock_on_shutdown` at boot time; see
    // that field's own doc. Consulted only in `run`'s teardown step 2.
    delete_flock_on_shutdown: bool,
}

impl RunningDaemon {
    /// Handles for driving this daemon from outside its run loop.
    ///
    /// Public only for `tests/daemon_e2e.rs`, which needs to shut a booted
    /// daemon down and force a roll write without a socket round-trip. The
    /// CLI boots and calls [`Self::run`]; it never reaches inside.
    #[must_use]
    pub fn context(&self) -> RpcContext {
        self.ctx.clone()
    }

    /// The control socket this daemon is bound to.
    ///
    /// Public only for the crate-root doc example, which connects a raw
    /// client to it; the CLI already knows the path it asked to bind.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Serves until a signal or `KillDaemon`, then tears down in order.
    ///
    /// TEARDOWN ORDER IS LOAD-BEARING:
    /// 1. stop the snapshot writer, and the dog watch alongside it — nothing
    ///    may rewrite the roll from here on, and no bus subscriber has a
    ///    reason left to watch once serving ends;
    /// 2. write the final muster roll — records the flock AS IT WAS, still
    ///    running, unless [`BootOptions::delete_flock_on_shutdown`] asked
    ///    for the registry to be wiped first, in which case it records
    ///    nothing;
    /// 3. broadcast [`BusEvent::DaemonShutdown`] — subscribers learn before their sockets close;
    /// 4. [`SupervisorHandle::shutdown`] — the kill ladder on every online sheep;
    /// 5. unlink the socket, remove the pidfile (best-effort on both: a
    ///    failure removing one must not skip attempting the other).
    ///
    /// Steps 1-2 before 4 are the whole point: run them the other way round
    /// and the roll records a flock of stopped sheep, and `shep muster`
    /// after a reboot restores nothing — silently breaking spec §13.4, the
    /// flagship migration scenario. Step 1 specifically must precede step 4
    /// (not just step 2): the writer would otherwise still be alive to
    /// observe the kill ladder's own `Exit`/`Stop` events and overwrite the
    /// roll step 2 just wrote.
    ///
    /// Every one of these steps runs unconditionally once this fn starts —
    /// `boot` succeeding is what commits the daemon to owning the flock,
    /// the roll, the socket, and the pidfile, so nothing short of a panic
    /// may return from here without having attempted every step above.
    /// `install_signals`'s registration runs inside `boot`, before any of
    /// the state this teardown depends on is even created — a failure there
    /// can `?`-exit without skipping teardown of state that doesn't exist
    /// yet. See `boot`'s own doc.
    ///
    /// # Errors
    /// - [`BootError::Io`] — a teardown filesystem step failed.
    pub async fn run(self) -> Result<(), BootError> {
        let RunningDaemon {
            ctx,
            listener,
            writer,
            dog_watch,
            silent_dog_watch,
            paths,
            socket,
            shutdown_rx,
            // Kept alive (not `_`) until this fn returns: `signals` must
            // outlive the whole serving lifetime below, and only its `Drop`
            // (at the end of this scope) stops its tasks. The underscore
            // prefix suppresses the "unused" warning for a binding that
            // exists purely for its drop side effect.
            signals: _signals,
            // Same reasoning as `signals` above: this `flock` must stay
            // held for the whole serving lifetime below, released only by
            // its `Drop` at the end of this scope — that release is what
            // lets a future daemon's own boot succeed.
            pidfile_lock: _pidfile_lock,
            delete_flock_on_shutdown,
        } = self;

        // `shutdown_rx` is the receiver `boot` has kept alive since the
        // watch channel was created (see the field's own doc) — reused
        // here rather than a fresh `ctx.shutdown.subscribe()` precisely so
        // there is never a window with zero receivers between `boot`
        // returning and this line running.
        RpcServer::new(listener, ctx.clone())
            .serve(shutdown_rx)
            .await;

        // 1. Stop the snapshot writer FIRST — see this fn's doc. Both dog
        //    watches stop alongside it: one is a bus subscriber with nothing
        //    left to watch for once serving ends, and the other is a timer
        //    that must not ask for a dog's restart while the flock is being
        //    torn down.
        writer.stop().await;
        dog_watch.abort();
        silent_dog_watch.abort();

        // 2. Write the final roll while every sheep is still online — UNLESS
        //    this boot asked for nothing to survive here at all
        //    (`delete_flock_on_shutdown`, `shep dev`'s own case): then the
        //    registry is wiped first, so this write already agrees with
        //    step 4's kill ladder that nothing here should come back. This
        //    runs regardless of how serving just ended — a signal caught
        //    inside `install_signals` ends things exactly the same way a
        //    `KillDaemon` request does, and no caller-level `Stop`/`Delete`
        //    pair can run ahead of that path (see
        //    `crate::snapshot::FlockRegistry::clear`'s own doc).
        if delete_flock_on_shutdown {
            ctx.registry.clear();
        }
        if let Err(err) = ctx.snapshot_now().await {
            tracing::warn!(%err, "final muster roll write failed");
        }

        // 3. Tell subscribers before their sockets close underneath them.
        let _ = ctx.events.send(SharedEvent::new(BusEvent::DaemonShutdown));

        // 4. Kill ladder on every online sheep.
        ctx.supervisor.shutdown().await;

        // 5. Unlink what boot created — both attempted regardless, the
        // first failure (if any) wins so a socket-unlink error can't hide
        // a pidfile that was never even attempted.
        //
        // The SOCKET half is unix-only, and skipping it on Windows is not an
        // omission: there is no file there to remove. The control address is
        // a named pipe, which stops existing when this process's last handle
        // closes — the listener is dropped moments from here — so teardown
        // has nothing to do that the kernel is not already doing.
        //
        // Attempting it anyway is not harmlessly redundant, which is how
        // this was found: `remove_file` on a `\.\pipe\...` name fails with
        // `ERROR_INVALID_PARAMETER`, so every Windows daemon shutdown
        // returned `Err` from `run()` — invisible through `shep kill`, which
        // does not read that value, and caught by `daemon_e2e`'s fixture,
        // which unwraps it.
        #[cfg(unix)]
        let unlink_socket = unlink_if_present(&socket);
        #[cfg(windows)]
        let unlink_socket = {
            let _ = &socket;
            Ok(())
        };
        let unlink_pidfile = unlink_if_present(&pidfile(&paths));
        unlink_socket.and(unlink_pidfile)
    }
}

/// Removes `path`, treating "already gone" as success rather than an error —
/// teardown's job is to make sure it's gone, not to prove it was there.
fn unlink_if_present(path: &Path) -> Result<(), BootError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BootError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Live signal-listener tasks [`install_signals`] spawned, held so they are
/// properly stopped — not merely detached, see this type's own [`Drop`] impl
/// — on every exit from [`boot`] or [`RunningDaemon::run`] that follows a
/// successful `install_signals` call. That includes an early `?`-return from
/// a LATER step inside `boot` itself (a failed `bind_socket` after signals
/// were already installed, say): dropping the partially-built value in that
/// path must not leak a task per boot attempt, which is exactly what
/// happened before this type existed (a bare counter returned by value, with
/// the actual `JoinHandle`s discarded at the spawn site).
#[derive(Debug)]
struct SignalTasks {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for SignalTasks {
    fn drop(&mut self) {
        // `JoinHandle::drop` alone DETACHES a task rather than stopping it
        // (the same footgun `server.rs`'s `converse` doc calls out for the
        // bus forwarder) — every task this struct owns is explicitly
        // aborted here, which is the whole reason this type exists instead
        // of a bare `Vec<JoinHandle<()>>`.
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Installs SIGTERM/SIGINT/SIGQUIT (graceful shutdown, first signal starts
/// it — see below for what a repeat does) and SIGUSR2 (`shep reopen`'s
/// out-of-band form, spec §9).
///
/// SIGUSR2's DEFAULT disposition is to terminate the process, so installing
/// this handler is load-bearing on its own: without it, an operator's
/// `kill -USR2` — or a logrotate `postrotate` stanza — kills the daemon
/// instead of rotating logs. A signal carries no selector, so it can only
/// mean [`ProcessSelector::All`], and that is exactly what it asks the
/// supervisor for: every sheep's log pump swaps both of its file handles,
/// the same work `shep reopen all` does. There is no reply channel either,
/// so the outcome is logged rather than reported to anyone.
///
/// # The supervisor arrives after the handler does
///
/// Returned alongside the tasks: the sender that hands the SIGUSR2 listener
/// the [`SupervisorHandle`] it reopens through. This function runs at
/// [`boot`]'s step 1, deliberately before the socket — or anything else —
/// exists, and the supervisor is not built until step 4, so no handle can be
/// passed in here. The listener parks on the matching receiver instead, and
/// `boot` connects the two once it has a handle to give.
///
/// That wait costs no delivery. The `signal()` call below is what replaces
/// SIGUSR2's disposition, and it has already done so by the time this
/// function returns; tokio coalesces every notification arriving before the
/// first `poll` into one item that the first `recv()` then yields. A SIGUSR2
/// raced into the window between step 1 and step 4 is therefore served late,
/// never dropped. A `boot` that FAILS before step 4 drops the sender instead
/// and the listener ends — with the libc disposition still replaced, which
/// is the half that matters for a process on its way out.
///
/// **Each listener stays armed for the rest of the process's life
/// (Decision 3, 2026-08-08).** A signal handler, once installed, is
/// installed for good — `tokio` never uninstalls the underlying libc
/// disposition just because the [`tokio::signal::unix::Signal`] stream
/// polling it happens to stop. A loop that awaited only one signal and
/// returned would leave a real gap: a SECOND SIGTERM arriving during a slow
/// [`RunningDaemon::run`] teardown (the kill ladder waiting out
/// `kill_timeout` on a stuck sheep, say) would have nowhere left to go — not
/// re-delivered to the now-finished task, and not killing the process
/// either, since installing ANY handler for a signal already replaced its
/// default terminate disposition. The daemon would sit there, unresponsive
/// to a second graceful request, with `SIGKILL` as the only remaining way
/// out. Looping keeps every listener polling for as long as the process
/// runs, so no delivery is ever silently dropped; a repeat is logged (see
/// the loop below) but does not otherwise change teardown, which is
/// already unconditional and already running (see
/// [`RunningDaemon::run`]'s own doc) — this crate does not invent an
/// escalation policy beyond "stay armed and observable" that nothing in
/// the spec or plan asks for.
///
/// # Errors
/// - [`BootError::Io`] — the OS refused to register a signal handler.
#[cfg(windows)]
fn install_signals(
    shutdown: Arc<watch::Sender<bool>>,
    paths: ShepPaths,
) -> Result<(SignalTasks, oneshot::Sender<SupervisorHandle>), BootError> {
    use tokio::signal::windows;

    let mut signals = SignalTasks {
        tasks: Vec::with_capacity(4),
    };

    // A macro rather than the unix arm loop because each console control
    // event is its OWN tokio type (`CtrlC`, `CtrlBreak`, `CtrlClose`,
    // `CtrlShutdown`) with its own `recv`, where unix has one `Signal` type
    // parameterised by a `SignalKind` value. There is nothing to iterate.
    macro_rules! listen {
        ($ctor:path, $name:literal) => {{
            let mut stream = $ctor().map_err(|source| BootError::Io {
                path: paths.home.clone(),
                source,
            })?;
            let shutdown = Arc::clone(&shutdown);
            signals.tasks.push(tokio::spawn(async move {
                // Looped for the same reason the unix arm loops — see this
                // function unix twin doc: a single await leaves a second
                // event during a slow teardown with nowhere to go.
                let mut already_shutting_down = false;
                while stream.recv().await.is_some() {
                    if already_shutting_down {
                        tracing::warn!(
                            signal = $name,
                            "received a repeat shutdown event while teardown is already \
                             underway; teardown continues unchanged"
                        );
                    } else {
                        already_shutting_down = true;
                    }
                    let _ = shutdown.send(true);
                }
            }));
        }};
    }

    // CTRL_C and CTRL_BREAK are the console interrupts an operator sends by
    // hand. CTRL_CLOSE (the console window closing), CTRL_SHUTDOWN (the
    // machine going down) and their siblings are what a graceful reboot
    // delivers, and handling them is what keeps a reboot from looking like a
    // crash to every sheep in the flock.
    //
    // **CTRL_CLOSE and CTRL_SHUTDOWN carry a hard OS deadline**, and it is
    // shorter than the daemon own teardown can promise: Windows terminates
    // the process about five seconds after the handler returns, regardless
    // of what shep is still doing. A flock whose apps take longer than that
    // to stop gracefully will lose the tail of its kill ladder to the OS.
    // There is no way to extend it from inside the process, and it is the
    // reason a production Windows deployment wants a real service (which
    // negotiates its own longer timeout with the SCM) rather than a console
    // daemon. Recorded here because it is invisible otherwise, and because
    // it is the sharpest edge in the Windows tier.
    listen!(windows::ctrl_c, "CTRL_C");
    listen!(windows::ctrl_break, "CTRL_BREAK");
    listen!(windows::ctrl_close, "CTRL_CLOSE");
    listen!(windows::ctrl_shutdown, "CTRL_SHUTDOWN");

    // No SIGUSR2 counterpart, and no Windows mechanism to build one from:
    // there is no user-defined console control event, and no way to deliver
    // an arbitrary one to another process. So the signal-driven log reopen
    // has no trigger on this platform.
    //
    // It costs less than it appears to. That signal exists for an external
    // rotator that has just renamed the log files underneath a running
    // daemon, and the shape of rotation itself differs here — see
    // `tokio_runner`'s `open_append`, whose Windows arm opens with
    // `FILE_SHARE_DELETE` precisely so a rotator can rename an open file at
    // all. The receiver is created and returned so the caller wiring is one
    // shape on both platforms; dropping the receiving end simply makes
    // `boot`'s own `let _ = connect_supervisor.send(..)` a no-op.
    let (connect_supervisor, _supervisor_rx) = oneshot::channel::<SupervisorHandle>();
    Ok((signals, connect_supervisor))
}

#[cfg(unix)]
fn install_signals(
    shutdown: Arc<watch::Sender<bool>>,
    paths: ShepPaths,
) -> Result<InstalledSignals, BootError> {
    let mut signals = SignalTasks {
        tasks: Vec::with_capacity(4),
    };

    for kind in [
        SignalKind::terminate(),
        SignalKind::interrupt(),
        SignalKind::quit(),
    ] {
        // An early return here drops `signals`, whose own `Drop` aborts
        // every task already pushed — registering the 2nd or 3rd kind
        // failing must not leak the 1st's already-spawned listener.
        let mut stream = signal(kind).map_err(|source| BootError::Io {
            path: paths.home.clone(),
            source,
        })?;
        let shutdown = Arc::clone(&shutdown);
        signals.tasks.push(tokio::spawn(async move {
            // Looped, not `stream.recv().await` once — see this fn's own
            // doc for why a single await left a real gap. `recv` returning
            // `None` would mean the stream itself closed (never observed
            // in practice — the same reasoning the SIGUSR2 loop below
            // already relies on), at which point this task has nothing
            // left to listen for and ending it is correct.
            let mut already_shutting_down = false;
            while stream.recv().await.is_some() {
                if already_shutting_down {
                    // Not escalated further on purpose: the brief asks
                    // only that a repeat signal during teardown be
                    // observable, not that it change teardown's own
                    // behavior (already unconditional, see
                    // `RunningDaemon::run`'s doc) — an operator who needs
                    // the daemon gone RIGHT NOW still has `SIGKILL`, which
                    // no handler installed here can intercept.
                    tracing::warn!(
                        ?kind,
                        "received a repeat shutdown signal while teardown is already \
                         underway; teardown continues unchanged (SIGKILL forces an \
                         immediate exit)"
                    );
                } else {
                    already_shutting_down = true;
                }
                let _ = shutdown.send(true);
            }
        }));
    }

    // SIGHUP is the handover trigger, and it is a signal rather than a
    // request for the reason spec H3 gives: the case that most needs a
    // reload is the one where the daemon refuses the client at the
    // handshake, and a remedy delivered over the channel it is meant to
    // repair is not a remedy. It has its own task rather than riding in the
    // loop above, because the two dispositions are different: the shutdown
    // signals stop this daemon, and this one replaces it.
    //
    // The graceful stop stays as the arm taken when a handover cannot
    // proceed. SIGHUP's kernel default is an unhandled terminate that would
    // drop the flock's pipes rather than walk the ladder, so a stray or
    // mistaken one, or one whose handover is refused, must still end the way
    // every other shutdown signal does.
    let mut hup = signal(SignalKind::hangup()).map_err(|source| BootError::Io {
        path: paths.home.clone(),
        source,
    })?;
    let (connect_handover, handover_rx) = oneshot::channel::<Option<HandoverSeam>>();
    let hup_shutdown = Arc::clone(&shutdown);
    signals.tasks.push(tokio::spawn(async move {
        // Parked until `boot` reaches step 4, exactly as the SIGUSR2 task
        // below is, and for the same reason: the descriptors and the
        // supervisor a handover needs do not exist when signals are
        // installed. A signal that arrives before then is buffered by the
        // stream, which was registered above.
        //
        // `None` is a boot that did not arm the handover
        // ([`BootOptions::handover`]), and an `Err` is a boot that never got
        // that far. Both mean this task has no image to hand anything to,
        // and both still have to answer SIGHUP: its kernel default is an
        // unhandled terminate, so a task that simply returned here would
        // leave a stray signal killing the daemon outright and dropping
        // every sheep's pipes.
        let seam = handover_rx.await.ok().flatten();
        // `if`, not `while`: this task handles at most one SIGHUP, unlike
        // the shutdown listeners above, which stay armed for the process's
        // life. On the success arm there is no image left to loop in, and on
        // every other arm this daemon is now stopping, and a second SIGHUP
        // during that teardown would find the same graceful stop already
        // underway.
        if hup.recv().await.is_some() {
            let refusal = match &seam {
                Some(seam) => match hand_over_now(seam).await {
                    // No successor statement on the success arm, because
                    // there is no successor image running this code.
                    Ok(never) => match never {},
                    Err(refusal) => refusal,
                },
                None => "this shepherd was not booted with the handover armed".to_string(),
            };
            tracing::warn!(
                %refusal,
                "SIGHUP: this flock could not be handed to a successor; stopping gracefully \
                 instead. This line may be the only record of the reason: a signal carries no \
                 sender, and the case this gate exists for is a flock that changed between a \
                 client's question and the signal, where that client was told nothing"
            );
            let _ = hup_shutdown.send(true);
        }
    }));

    let mut usr2 = signal(SignalKind::user_defined2()).map_err(|source| BootError::Io {
        path: paths.home.clone(),
        source,
    })?;
    let (connect_supervisor, supervisor_rx) = oneshot::channel::<SupervisorHandle>();
    signals.tasks.push(tokio::spawn(async move {
        // Parked until `boot` reaches step 4 — see this fn's own doc for why
        // the wait loses no signal, and for what an `Err` here means.

        let Ok(supervisor) = supervisor_rx.await else {
            return;
        };
        while usr2.recv().await.is_some() {
            // A rotator that moved the whole log DIRECTORY needs it back at
            // `DIR_MODE`, and the pump this reaches is what puts it there —
            // its own open asks `mkdir` for the mode (see `open_append`).
            // Recreating it here as well would be a second owner of the same
            // guarantee, differing from the pump's for any sheep logging
            // outside the layout.
            match supervisor.reopen(ProcessSelector::All).await {
                Ok(reopened) => tracing::info!(
                    reopened = reopened.len(),
                    "SIGUSR2: every sheep's log files reopened"
                ),
                // An empty flock is an idle daemon's ordinary state, not
                // something a nightly `postrotate` should warn about.
                Err(SupervisorError::NotFound) => {
                    tracing::info!("SIGUSR2: no sheep to reopen");
                }
                // A signal carries no reply channel, so this log is the whole
                // report — nobody is waiting to be told.
                Err(err) => tracing::warn!(%err, "SIGUSR2: log reopen failed"),
            }
        }
    }));

    Ok((signals, connect_supervisor, connect_handover))
}

/// What [`install_signals`] hands back: the live listener tasks, and the two
/// senders that connect them to state `boot` has not built yet.
///
/// The SIGUSR2 task needs a [`SupervisorHandle`] to reopen through, and the
/// SIGHUP task needs a whole [`HandoverSeam`] or the `None` that says this
/// boot did not arm one. Both are parked on their receivers from the moment
/// the handlers are installed, which is before the socket exists.
#[cfg(unix)]
type InstalledSignals = (
    SignalTasks,
    oneshot::Sender<SupervisorHandle>,
    oneshot::Sender<Option<HandoverSeam>>,
);

/// What [`rehydrate`] rebuilds from a blob: the home's lock, the control
/// listener, and the flock to install with the counters and the in-flight
/// reloads it ran under.
#[cfg(unix)]
type Rehydrated = (
    PidfileLock,
    Listener,
    (
        Vec<crate::handover::adopt::AdoptedSheep>,
        crate::handover::Counters,
        Vec<crate::supervisor::CarriedReload>,
    ),
);

/// Everything the SIGHUP task needs to replace this daemon's image.
///
/// A struct handed over a channel rather than three arguments, because none
/// of it exists when [`install_signals`] runs: the descriptors are opened
/// two steps later and the supervisor a step after that.
///
/// `Debug` is derived and carries nothing sensitive: two descriptor numbers,
/// a mailbox and the home's own paths. The blob those descriptors end up in
/// is a different matter and does carry each sheep's environment; see
/// [`crate::handover::Handover`]'s own doc for what protects it.
#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct HandoverSeam {
    /// The flock to carry.
    supervisor: SupervisorHandle,
    /// The daemon's own two descriptors, which the actor never sees.
    fds: crate::handover::DaemonFds,
    /// The home, for the blob's path.
    paths: ShepPaths,
}

/// Replace this process with a successor holding `seam`'s flock.
///
/// The gate runs HERE as well as in the client that asked before signalling,
/// and both are load-bearing. The client asks so that a refusal reaches the
/// operator and the flock is stopped-and-started rather than left down (spec
/// H3a); this one asks because a signal is a signal: anyone can send one,
/// peer input is untrusted, and the flock can change between the question
/// and the signal.
///
/// # Errors
///
/// The sentence to log, when the flock cannot be carried, when the actor is
/// gone, or when the exec itself failed. Every one of them leaves this
/// process still itself, with no blob on disk, and the caller falls back to
/// a graceful stop.
#[cfg(unix)]
async fn hand_over_now(seam: &HandoverSeam) -> Result<core::convert::Infallible, String> {
    let (candidates, blob, parked) = seam
        .supervisor
        .handover_snapshot(seam.fds)
        .await
        .map_err(|err| format!("the supervisor could not describe its flock: {err}"))?;
    let refusal = match hand_over_carrying(&candidates, &blob, seam) {
        Ok(never) => match never {},
        Err(refusal) => refusal,
    };
    // Taking the snapshot stopped every pump it reached, and this process
    // is the one that owes them a resume: there is no successor image to do
    // it, and nothing else in the daemon ever sends one. The caller falls
    // back to a graceful stop, which is a stretch of seconds during which
    // the flock is being told to exit and is writing exactly the lines an
    // operator will reach for. A refusal that changes nothing else must not
    // silently cost them.
    //
    // Here rather than inside `exec_into`'s own error path, which restores
    // `FD_CLOEXEC` on the same failure: the gate below refuses BEFORE
    // anything is exec'd, so a resume that lived with the descriptors would
    // miss the abort that happens most often. This is where every way out
    // meets.
    parked.resume().await;
    Err(refusal)
}

/// [`hand_over_now`]'s body, split out so a single resume can cover every
/// way it refuses.
///
/// Two gates, in this order, and they ask different questions. The first
/// asks whether this FLOCK is a shape a handover carries. The second asks
/// whether the BLOB describing it is one a successor could actually adopt,
/// by running the successor's own adoption here, against duplicates, while
/// this image still exists to fall back to.
///
/// # Errors
///
/// The flock cannot be carried, a successor could not have adopted the blob,
/// or the exec itself failed. All three leave this process still itself,
/// with no blob on disk.
#[cfg(unix)]
fn hand_over_carrying(
    candidates: &[crate::handover::OwnedCandidate],
    blob: &crate::handover::Handover,
    seam: &HandoverSeam,
) -> Result<core::convert::Infallible, String> {
    let borrowed: Vec<crate::handover::Candidate<'_>> = candidates
        .iter()
        .map(crate::handover::OwnedCandidate::as_candidate)
        .collect();
    if let crate::handover::Fitness::Refused(reason) = crate::handover::fitness(&borrowed) {
        return Err(reason.to_string());
    }
    // The gate with no way back if it is skipped. After the `execve` there
    // is no image to refuse to: `rehydrate` returns `BootError::Adopt`, this
    // daemon's replacement exits without ever serving, and the flock runs on
    // with nothing supervising it — which is what that variant's own message
    // tells the operator, pointing them at `shep muster`.
    //
    // Here rather than inside `handover::hand_over`, which would be harder
    // for a future caller to bypass. The rehearsal registers objects with
    // the tokio reactor and so needs a runtime, and `hand_over`'s own exec
    // self-test runs from a plain `#[test]` with a tempfile standing in for
    // the listener. This is the production seam and its only caller.
    crate::handover::adopt::dry_run(blob).map_err(|err| {
        format!(
            "a successor could not have adopted this flock, so none was started: {err}. This is \
             a shep bug worth reporting: the descriptors are ones this shepherd opened itself, \
             and the check that refused them is the successor's own. The flock is stopped and \
             started instead, which is the reload an operator had before handovers existed"
        )
    })?;
    crate::handover::hand_over(blob, &seam.paths).map_err(|err| err.to_string())
}

/// Error type returned from this module's boot steps
///
/// Wraps `io::Error` directly rather than stringifying it (contrast
/// [`shep_core::protocol::WireError`]) so callers keep the underlying OS
/// diagnostic via [`core::error::Error::source`]; that costs this enum
/// `Clone`/`PartialEq`/`Eq` (IR-19's documented exception for variants
/// wrapping `io::Error`).
///
/// `#[non_exhaustive]`: today's four variants cover filesystem setup, socket
/// claim, roll restore, and readiness reporting, and a future boot step —
/// socket-activation handoff, or cgroup setup — would add a fifth rather
/// than overloading [`Self::Io`], whose `path`/`source` shape is specific to
/// the steps that already exist, and shep-daemon is a published library an
/// out-of-tree matcher should not break for (IR-20).
#[non_exhaustive]
#[derive(Debug)]
pub enum BootError {
    /// A flock this image inherited across a handover could not be
    /// installed.
    ///
    /// The descriptor a blob named is not open here, is not the kind of
    /// object it was named as, or the supervisor would not take it. A
    /// `String` rather than the underlying error, because the two sources
    /// are different types in different modules and neither is part of this
    /// crate's public surface; what a caller needs is the sentence naming
    /// which sheep is now unsupervised.
    Adopt(String),
    /// A filesystem step failed (carries the path and the OS error)
    ///
    /// Deliberately has no `From<std::io::Error>`, and neither does any
    /// sibling: `ReadyWrite` wraps the same type, so one would make a bare
    /// `?` in this module pick a variant rather than report one. A caller
    /// that cannot name the path it was working on has not finished
    /// thinking about the error yet.
    Io {
        /// The path the failing step operated on
        path: PathBuf,
        /// The underlying OS error
        source: std::io::Error,
    },
    /// Another daemon already answers on this socket (carries its pid if recorded)
    AlreadyRunning {
        /// The pid recorded in the pidfile, if one was readable
        pid: Option<u32>,
    },
    /// The muster roll exists but could not be read or parsed on restore
    Snapshot(SnapshotError),
    /// `$SHEP_HOME` puts the control socket past the platform's `sun_path`
    /// limit, so no bind could ever succeed (carries the path and the limit)
    ///
    /// Checked before the bind rather than translated after it: the kernel's
    /// own `ENAMETOOLONG` names neither the limit nor the variable
    /// responsible, and an operator reading it has no way to know that the
    /// number is 104 here and 108 on Linux, nor that `$SHEP_HOME` is what
    /// feeds it.
    SocketPathTooLong {
        /// The socket path that would not fit
        path: PathBuf,
        /// Its length in bytes
        len: usize,
        /// This platform's `sun_path` capacity in bytes
        limit: usize,
    },
    /// Writing the readiness line to the caller-adopted readiness pipe
    /// failed (carries the OS error)
    ///
    /// Adoption itself (`unsafe fn` `crate::sys::adopt_fd`) is the
    /// caller's job, not `boot`'s — see [`BootOptions::ready_fd`]'s own
    /// doc — so `boot` has no error variant for a failed adoption; by the
    /// time `options.ready_fd` reaches here it is already a valid, owned
    /// [`std::fs::File`], and this variant covers only the plain IO write
    /// to it.
    ReadyWrite(std::io::Error),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "boot step failed for `{}`: {source}", path.display())
            }
            Self::AlreadyRunning { pid: Some(pid) } => {
                write!(f, "a shep daemon is already running (pid {pid})")
            }
            Self::AlreadyRunning { pid: None } => write!(f, "a shep daemon is already running"),
            Self::Snapshot(err) => write!(f, "muster roll restore failed: {err}"),
            Self::SocketPathTooLong { path, len, limit } => write!(
                f,
                "the control socket path is {len} bytes and this platform allows {limit}: `{}`. \
                 A unix socket path is bounded by the kernel, not by shep, so a shorter \
                 $SHEP_HOME is the only fix.",
                path.display()
            ),
            Self::ReadyWrite(err) => write!(f, "writing the readiness line failed: {err}"),
            Self::Adopt(reason) => write!(
                f,
                "this shepherd was handed a flock it could not take over: {reason}. The flock is \
                 still running and nothing is supervising it. `shep daemon reload` is not the \
                 way back: it needs a live shepherd to ask and to signal, and this process is \
                 about to exit without ever serving. It holds the pidfile until it does, so the \
                 home is claimable straight afterwards and `shep muster` starts a shepherd from \
                 the roll"
            ),
        }
    }
}

impl core::error::Error for BootError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyRunning { .. } => None,
            // No source: nothing failed underneath, the path was refused
            // before any syscall was attempted.
            Self::SocketPathTooLong { .. } => None,
            Self::Snapshot(err) => Some(err),
            Self::ReadyWrite(err) => Some(err),
            // No source: both underlying types are module-private, so the
            // sentence is the whole report (see the variant's own doc).
            Self::Adopt(_) => None,
        }
    }
}

impl From<SnapshotError> for BootError {
    fn from(source: SnapshotError) -> Self {
        Self::Snapshot(source)
    }
}

/// Boot behaviour that is specific to the Windows tier.
///
/// The big `mod tests` below is `#[cfg(unix)]` because almost every case in
/// it asserts something that only exists on unix — a `0700` mode read back
/// off a directory, a `raise(SIGTERM)`, a socket FILE left behind by a crash
/// and then probed. Those are not skipped here out of convenience; they
/// assert guarantees the Windows tier deliberately makes differently, and
/// each difference is argued at its own call site above.
///
/// What survives translation is asserted here instead, on the three
/// properties that must hold on both platforms or the daemon is unsound.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn paths_in(dir: &Path) -> ShepPaths {
        ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| dir.to_string_lossy().into_owned()),
            Path::new(""),
        )
    }

    /// fails if the layout is not created. No mode to assert — see
    /// `create_dir_at_dir_mode`'s Windows arm — but the directories
    /// themselves are what every later step writes into.
    #[test]
    fn init_dirs_creates_the_whole_layout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        init_dirs(&paths).unwrap();
        for expected in [&paths.home, &paths.logs, &paths.pids, &paths.run] {
            assert!(expected.is_dir(), "{} was not created", expected.display());
        }
    }

    /// fails if two daemons can both claim one `$SHEP_HOME`.
    ///
    /// The Windows arm holds its lock on a SIBLING `.lock` file rather than
    /// on the pidfile, so this also pins the property that motivated that
    /// split: the loser must still be able to READ the pidfile to name the
    /// winner. A `share_mode(0)` open of the pidfile itself would make
    /// `read_pidfile` fail with a sharing violation instead, and the
    /// `AlreadyRunning` error would lose its pid.
    #[test]
    fn a_second_pidfile_lock_is_refused_and_can_still_read_the_winners_pid() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        init_dirs(&paths).unwrap();

        let mut first = PidfileLock::acquire(&paths).expect("the first daemon must win");
        first.record(&paths, 4242).unwrap();

        let refusal = PidfileLock::acquire(&paths).expect_err("a second daemon must be refused");
        let BootError::AlreadyRunning { pid } = refusal else {
            panic!("a contended lock must report AlreadyRunning, got {refusal:?}");
        };
        assert_eq!(
            pid,
            Some(4242),
            "the loser must be able to read the winner's pid off the pidfile"
        );
    }

    /// fails if the lock outlives the process that held it.
    ///
    /// The crash property: a Windows share-mode reservation is owned by the
    /// HANDLE and released by the kernel when the last one closes, exactly
    /// as `flock`'s is released with the last descriptor. Dropping stands in
    /// for the process dying, which is what closes the handle either way.
    #[test]
    fn dropping_the_lock_releases_it_for_the_next_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        init_dirs(&paths).unwrap();

        let first = PidfileLock::acquire(&paths).unwrap();
        drop(first);

        PidfileLock::acquire(&paths)
            .expect("a released lock must be re-acquirable, or a crash would wedge $SHEP_HOME");
    }

    /// fails if two daemons can both bind one control address.
    ///
    /// `first_pipe_instance` is what refuses the second, and this asserts
    /// the refusal arrives as `AlreadyRunning` rather than as a raw
    /// `ERROR_ACCESS_DENIED` an operator could not act on.
    /// `#[tokio::test]`, unlike its three siblings: creating a named pipe
    /// instance registers it with the tokio reactor, so `ServerOptions::create`
    /// panics outside a runtime context. `boot` always has one; a bare
    /// `#[test]` here does not.
    #[tokio::test]
    async fn a_second_bind_on_a_live_control_address_reports_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        init_dirs(&paths).unwrap();

        let _live = bind_socket(&paths, &paths.socket).expect("the first bind must succeed");

        let refusal = bind_socket(&paths, &paths.socket)
            .expect_err("a second daemon must not bind the same pipe");
        assert!(
            matches!(refusal, BootError::AlreadyRunning { .. }),
            "a taken pipe name must read as AlreadyRunning, got {refusal:?}"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::dogs::DogSpec;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::snapshot::{FlockSnapshot, SNAPSHOT_VERSION, SavedApp};
    // the one crate-root fixture (IR-33)
    use crate::testing::{AnnouncingRunner, SharedRunner, capture_logs, test_paths};
    use shep_core::config::{AppConfig, ProbeConfig, ProbeKind, normalize};
    use shep_core::protocol::{DogSource, ProcessEventKind};
    use shep_core::status::ProcStatus;
    use shep_core::values::UpDuration;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// fails if adopting the pidfile descriptor ever leaves the lock free.
    ///
    /// The failure it prevents: an arm that re-locked would have to release
    /// first, and that window is exactly long enough for a second daemon to
    /// win this home while the only one supervising its flock is mid-boot.
    ///
    /// What makes the assertion possible at all is that `flock` conflicts
    /// between separate open file DESCRIPTIONS even inside one process, so
    /// this process can ask for the lock it already holds and be refused.
    ///
    /// `mem::forget` plus `sys::adopt_handover_fd` stands in for the
    /// `execve` a successor arrives through, and is as close to it as one
    /// process can get: forgetting the lock skips the `flock(fd, LOCK_UN)`
    /// its drop would run, exactly as an exec does, and adopting the number
    /// back gives the descriptor a single new owner, exactly as the
    /// successor gives it one. Duplicating it instead would leave a second
    /// descriptor holding the same lock, and both assertions below would
    /// then hold whatever the adopted arm did with its own.
    #[test]
    fn the_adopted_pidfile_arm_does_not_release_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();

        let mut held = PidfileLock::acquire(&paths).expect("the predecessor must win");
        let fd = std::os::fd::AsRawFd::as_raw_fd(held.flock.file());
        core::mem::forget(held);
        let inherited = crate::sys::adopt_handover_fd(fd)
            .expect("the successor adopts the number the blob named");

        let adopted = PidfileLock::from_locked(inherited);
        let refusal = PidfileLock::acquire(&paths)
            .expect_err("the lock must never be free while a successor holds it");
        assert!(
            matches!(refusal, BootError::AlreadyRunning { .. }),
            "a contended lock must report AlreadyRunning, got {refusal:?}"
        );

        // And it is a lock rather than a descriptor nobody can ever release:
        // a successor that exits closes the only handle left, and the kernel
        // frees the home for the next daemon.
        //
        // Retried rather than demanded on the first attempt, for the reason
        // `stale_socket_leftover` spells out at length a few tests below: a
        // `fork` copies the whole descriptor table, so any child another test
        // in this binary spawns concurrently holds a duplicate of this
        // descriptor until its own `exec` runs — and a duplicate holds the
        // `flock` with it. That is a lying fixture rather than a lock this
        // arm failed to release, and it was reproducible the moment a
        // supervisor test began spawning a real child. The assertion is
        // unweakened: the home must become claimable, and a lock genuinely
        // still held fails this loop just as flatly as it failed the single
        // attempt.
        drop(adopted);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let claimed = loop {
            match PidfileLock::acquire(&paths) {
                Ok(claimed) => break claimed,
                Err(error) => assert!(
                    std::time::Instant::now() < deadline,
                    "a successor that exits must leave the home claimable: {error:?}"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(claimed);
    }

    /// Serializes every test in this module that calls `boot()` and expects
    /// it to succeed — NOT just the ones that go on to call `daemon.run()`
    /// — against every OTHER such test, because `nix::sys::signal::raise`
    /// (and the real OS signal delivery it triggers) is NOT scoped to one
    /// test's own tokio runtime: every `Signal` stream registered anywhere
    /// in this test BINARY'S process receives it, regardless of which test
    /// spawned it or which runtime owns it.
    ///
    /// The rule is "calls `boot()` successfully", not "calls `run()`",
    /// because `install_signals` moved INTO `boot` (see `boot`'s own doc):
    /// a test whose `boot()` call succeeds has live signal listeners
    /// running from that point on, whether or not it ever calls `run()`.
    ///
    /// Without this lock, two overlapping `boot()`-successful tests can
    /// rescue or corrupt each other: `raise(SIGTERM)` in one test's signal
    /// path can reach a second test's own hung daemon on the same delivery,
    /// masking a real regression in that second test. Every test below that
    /// calls `boot()` takes this for its own duration so no two such tests
    /// can ever overlap.
    ///
    /// `tokio::sync::Mutex`, not `std::sync::Mutex`: the guard is held
    /// across this fn's own `.await` points (`boot`, `run`, ...), and
    /// clippy's `await_holding_lock` correctly denies a blocking guard held
    /// there — an async-aware mutex has no such restriction.
    static SIGNAL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn init_dirs_creates_the_whole_layout_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        for path in [&paths.home, &paths.logs, &paths.pids, &paths.run] {
            assert!(path.is_dir(), "{} was not created", path.display());
            assert_eq!(mode_of(path), DIR_MODE, "{}", path.display());
        }
        init_dirs(&paths).unwrap(); // idempotent: a restart must not fail here
    }

    #[test]
    fn a_fresh_dir_lands_at_dir_mode_with_no_separate_chmod() {
        // TOCTOU regression guard: this calls the raw creation primitive
        // ALONE, with no follow-up set_permissions in this test, so it can
        // only pass if DirBuilder's `.mode(DIR_MODE)` really lands the mode
        // at creation. A regression back to `create_dir_all` (0o777, narrowed
        // only by whatever the ambient umask strips) would still slip past
        // init_dirs_creates_the_whole_layout_owner_only above, because that
        // test only observes the mode after init_dirs' own chmod pass has
        // already run — it can't see the window this test targets.
        let dir = tempfile::tempdir().unwrap();
        let never_existed = dir.path().join("nested").join("run");
        create_dir_at_dir_mode(&never_existed).unwrap();
        assert_eq!(
            mode_of(&never_existed),
            DIR_MODE,
            "a freshly created dir must be DIR_MODE at creation, not after a later chmod"
        );
    }

    #[test]
    fn init_dirs_tightens_a_world_readable_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.run).unwrap();
        std::fs::set_permissions(&paths.run, std::fs::Permissions::from_mode(0o755)).unwrap();
        init_dirs(&paths).unwrap();
        assert_eq!(
            mode_of(&paths.run),
            DIR_MODE,
            "a loose run dir must be tightened, not accepted"
        );
    }

    #[test]
    fn pidfile_round_trips_and_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        assert_eq!(read_pidfile(&paths).unwrap(), None);
        write_pidfile(&paths, 4242).unwrap();
        assert_eq!(read_pidfile(&paths).unwrap(), Some(4242));
        assert_eq!(pidfile(&paths), paths.pids.join("shepd.pid"));
    }

    #[test]
    fn liveness_reports_none_when_no_daemon_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        assert_eq!(daemon_liveness(&paths).unwrap(), Shepherd::Absent);
    }

    #[test]
    fn liveness_reports_none_for_a_stale_pidfile_nobody_holds() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        std::fs::write(pidfile(&paths), "999999").unwrap();
        // The file exists and names a pid. Nothing holds the lock, so this is
        // NOT a live daemon and must not be reported as one.
        assert_eq!(daemon_liveness(&paths).unwrap(), Shepherd::Absent);
    }

    #[test]
    fn liveness_reports_the_pid_a_lock_holder_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let mut held = PidfileLock::acquire(&paths).unwrap();
        held.record(&paths, 4242).unwrap();
        assert_eq!(daemon_liveness(&paths).unwrap(), Shepherd::Running(4242));
        drop(held);
        assert_eq!(
            daemon_liveness(&paths).unwrap(),
            Shepherd::Absent,
            "a released lock is not a live daemon, whatever the file still says"
        );
    }

    #[test]
    fn liveness_reports_booting_for_a_holder_that_has_not_recorded_a_pid_yet() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // `boot` takes the lock and records its pid a few statements later,
        // binding the socket in between. A caller that read this window as
        // an absence would start a second daemon that then dies unable to
        // take the lock.
        let held = PidfileLock::acquire(&paths).unwrap();
        assert_eq!(daemon_liveness(&paths).unwrap(), Shepherd::Booting);
        drop(held);
        assert_eq!(daemon_liveness(&paths).unwrap(), Shepherd::Absent);
    }

    #[test]
    fn socket_path_honors_a_config_override() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        assert_eq!(socket_path(&paths, None), paths.socket);
        let custom = dir.path().join("custom.sock");
        assert_eq!(socket_path(&paths, Some(&custom)), custom);
    }

    /// The kernel's own refusal is `ENAMETOOLONG` and names neither the
    /// limit nor the variable that produced it. An operator whose
    /// `$SHEP_HOME` is a few directories too deep has no way to guess that
    /// the number is 104 here and 108 on Linux.
    #[tokio::test]
    async fn an_over_length_socket_path_names_the_limit_and_the_variable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();

        // Comfortably past both platforms' capacity, without depending on
        // which one this is.
        let long = dir.path().join("x".repeat(200));

        let err = bind_socket(&paths, &long).expect_err("a path this long cannot bind");
        assert!(
            matches!(err, BootError::SocketPathTooLong { .. }),
            "refused before the syscall, not translated after it: {err:?}"
        );

        let rendered = err.to_string();
        assert!(
            rendered.contains("$SHEP_HOME"),
            "the message names what to shorten: {rendered}"
        );
        assert!(
            rendered.contains("bytes"),
            "and the limit it is measured against: {rendered}"
        );
        assert!(
            !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
            "no em or en dash in copy a user reads: {rendered}"
        );
    }

    #[tokio::test]
    async fn bind_socket_binds_a_fresh_path() {
        // Real time: real socket IO (see the paused-clock rule).
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let listener = bind_socket(&paths, &paths.socket).unwrap();
        assert!(paths.socket.exists());
        drop(listener);
    }

    /// Fabricates exactly what a crashed daemon leaves behind: a socket file
    /// at `socket` that nothing is listening on. Neither std nor tokio
    /// unlinks a `UnixListener`'s path on drop, so binding and dropping is
    /// the right shape — but it is NOT, on its own, enough to guarantee the
    /// second half of that sentence inside this test binary.
    ///
    /// macOS has no atomic `SOCK_CLOEXEC` (the descriptor is marked
    /// close-on-exec a moment AFTER `socket(2)` returns), and a `fork` copies
    /// the whole descriptor table, so any child another test spawns
    /// concurrently — the exec probes, the runner tests — can end up holding
    /// a duplicate of the listener below. For as long as that duplicate lives
    /// (until the child's `exec` or exit; measured at up to ~25ms) the socket
    /// object is NOT destroyed, `connect` to the path SUCCEEDS, and any
    /// prober is looking at a live socket. That is not a daemon bug — it is a
    /// lying fixture: it can make
    /// [`two_concurrent_boots_on_a_stale_socket_exactly_one_wins`] fail as
    /// `[AlreadyRunning, AlreadyRunning]`, both racers refused — the flock's
    /// loser refused correctly, and the flock's WINNER then found this
    /// leftover answering and refused too, exactly as `bind_socket` should.
    ///
    /// So establish the precondition instead of assuming it: don't return
    /// until the path actually refuses a connection. That verdict is stable
    /// once it lands — a refused socket is a destroyed socket, and only a
    /// fresh `bind` can make the path answer again, which nothing but the
    /// code under test does.
    ///
    /// Real sleeps, in a module whose default is a paused clock (IR-33): a
    /// descriptor's lifetime in ANOTHER process is not on any clock tokio can
    /// advance, and every caller of this helper is already a real-time test
    /// for the same reason (real socket IO).
    ///
    /// # Panics
    /// If the leftover never goes stale, or the probe fails for any reason
    /// other than nobody listening.
    #[track_caller]
    fn stale_socket_leftover(socket: &Path) {
        // Two nested loops for two different holders. The inner one waits out
        // the common case, a child that copied the descriptor mid-spawn and
        // drops it on `exec`. The outer one re-fabricates for the rarer one,
        // a child that forked inside the socket(2)-to-close-on-exec window
        // and therefore keeps the descriptor for its whole life: unlinking
        // detaches that socket from this path for good, and the next bind
        // starts clean.
        for _ in 0..20 {
            let _ = std::fs::remove_file(socket);
            drop(std::os::unix::net::UnixListener::bind(socket).unwrap());
            for _ in 0..40 {
                match std::os::unix::net::UnixStream::connect(socket) {
                    Err(refused)
                        if matches!(
                            refused.kind(),
                            ErrorKind::ConnectionRefused | ErrorKind::NotFound
                        ) =>
                    {
                        return;
                    }
                    Ok(_) => std::thread::sleep(Duration::from_millis(5)),
                    Err(other) => {
                        panic!("probing the fabricated leftover socket failed: {other}")
                    }
                }
            }
        }
        panic!(
            "{} never went stale: something kept answering on it",
            socket.display()
        );
    }

    #[tokio::test]
    async fn a_socket_left_by_a_crash_is_unlinked_and_rebound() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        stale_socket_leftover(&paths.socket);
        assert!(paths.socket.exists(), "the stale file must still be there");
        let listener = bind_socket(&paths, &paths.socket).unwrap();
        assert!(paths.socket.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn a_live_socket_is_reported_as_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // `tokio::net::UnixListener` by its full path: this module no longer
        // imports it, because production code reaches the transport through
        // `shep_core::transport::Listener` on both platforms now. This case
        // is `cfg(unix)` and deliberately binds the RAW unix type, so that
        // what it proves is "a real socket someone else is listening on is
        // reported as AlreadyRunning" rather than anything about shep's own
        // wrapper.
        let live = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        write_pidfile(&paths, 4242).unwrap();
        assert!(matches!(
            bind_socket(&paths, &paths.socket),
            Err(BootError::AlreadyRunning { pid: Some(4242) })
        ));
        // The refused bind must be a pure probe: the live daemon's socket
        // file is untouched (not unlinked) and still answers, proving
        // bind_socket never reached the remove_file/rebind arm here.
        assert!(
            paths.socket.exists(),
            "a live daemon's socket must never be unlinked"
        );
        std::os::unix::net::UnixStream::connect(&paths.socket)
            .expect("the live listener must still be accepting after a refused bind");
        drop(live);
    }

    /// What one racer thread observed in
    /// [`two_concurrent_boots_on_a_stale_socket_exactly_one_wins`] — kept
    /// deliberately small and `'static` so it can cross the thread boundary
    /// without carrying a `RunningDaemon` (and the tokio resources inside
    /// it) out of the runtime that created it. See that test's own doc for
    /// why.
    #[derive(Debug)]
    enum RaceOutcome {
        Won { socket_still_accepts: bool },
        AlreadyRunning,
        Other(String),
    }

    #[test]
    fn two_concurrent_boots_on_a_stale_socket_exactly_one_wins() {
        // Pins Decision 2 (2026-08-08), closing double-bind race #3: two
        // daemons racing to boot the same $SHEP_HOME could both hit
        // `bind_socket`'s EADDRINUSE -> probe -> remove_file -> rebind arm
        // over a CRASHED predecessor's leftover socket file, both observe
        // `ConnectionRefused` (correctly — nobody's listening), and both
        // proceed into the recovery arm — loser's `remove_file` can delete
        // winner's freshly-bound listener out from under it, leaving BOTH
        // convinced they hold the sole live daemon. See `PidfileLock`'s own
        // doc for the full mechanism and why `flock` closes it.
        //
        // NOT `#[tokio::test]`, and each racer gets its OWN
        // `new_current_thread` runtime on its OWN `std::thread`: `boot`'s
        // own synchronous prefix (signals, dirs, the pidfile lock, socket
        // bind) never actually awaits a not-yet-ready future, so two
        // `boot()` calls driven as plain tokio TASKS on one runtime would
        // never really interleave — the executor would run the first one's
        // entire synchronous body to completion in a single poll before the
        // second ever got scheduled, proving nothing about real contention.
        // Real OS threads, synchronized to start together via a `Barrier`,
        // give the two racers genuine kernel-level concurrency over the
        // same `open`/`flock`/`bind`/`connect`/`remove_file` calls — the
        // actual shape of the bug this test pins.
        //
        // Looped: a race this timing-dependent isn't guaranteed to land on
        // the exact bad interleaving every single attempt (the fd-reuse
        // double-close that `sys.rs`'s
        // `a_closed_descriptor_is_refused_instead_of_adopted` now pins
        // structurally took 25 saturated workspace runs to show itself even
        // once) — running many trials inside one test call, and failing on
        // the first bad one, is what makes the revert-and-confirm-it-fails
        // check below actually reliable rather than a coin flip.
        //
        // Locked per SIGNAL_TEST_LOCK's rule for every trial that has a
        // winner: `blocking_lock` because this outer fn is plain sync, not
        // `#[tokio::test]`, so there is no surrounding runtime to `.await`
        // on when acquiring it.
        for _ in 0..25 {
            let _guard = SIGNAL_TEST_LOCK.blocking_lock();
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(&dir);
            init_dirs(&paths).unwrap();
            // The crashed predecessor's leftover, and it must really be
            // leftover before the racers start: a socket another test's
            // mid-spawn child is briefly keeping alive would make the flock's
            // WINNER refuse too, for a reason that has nothing to do with the
            // race under test. See `stale_socket_leftover`'s own doc — that
            // exact false failure is why it exists.
            stale_socket_leftover(&paths.socket);

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let paths = paths.clone();
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait(); // both racers cross together
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        rt.block_on(async {
                            match boot(ScriptedRunner::new(vec![]), paths, BootOptions::default())
                                .await
                            {
                                Ok(daemon) => {
                                    // Checked (and `daemon` dropped) INSIDE
                                    // this racer's own runtime — see
                                    // `RaceOutcome`'s own doc for why
                                    // nothing tokio-shaped crosses the
                                    // thread boundary.
                                    let reachable =
                                        std::os::unix::net::UnixStream::connect(daemon.socket())
                                            .is_ok();
                                    RaceOutcome::Won {
                                        socket_still_accepts: reachable,
                                    }
                                }
                                Err(BootError::AlreadyRunning { .. }) => {
                                    RaceOutcome::AlreadyRunning
                                }
                                Err(other) => RaceOutcome::Other(other.to_string()),
                            }
                        })
                    })
                })
                .collect();
            let outcomes: Vec<RaceOutcome> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();

            for outcome in &outcomes {
                if let RaceOutcome::Other(msg) = outcome {
                    panic!("a racer hit neither Ok nor AlreadyRunning: {msg}");
                }
            }

            let wins = outcomes
                .iter()
                .filter(|o| matches!(o, RaceOutcome::Won { .. }))
                .count();
            let already_running = outcomes
                .iter()
                .filter(|o| matches!(o, RaceOutcome::AlreadyRunning))
                .count();
            assert_eq!(
                wins, 1,
                "exactly one racer must win a boot on the same $SHEP_HOME: {outcomes:?}"
            );
            assert_eq!(
                already_running, 1,
                "the loser must be refused as AlreadyRunning, not silently succeed or hit some other error: {outcomes:?}"
            );
            assert!(
                matches!(
                    outcomes
                        .iter()
                        .find(|o| matches!(o, RaceOutcome::Won { .. }))
                        .unwrap(),
                    RaceOutcome::Won {
                        socket_still_accepts: true
                    }
                ),
                "the winner's own socket must still accept a connection, proving its bind \
                 wasn't the one the loser's remove_file clobbered: {outcomes:?}"
            );
        }
    }

    #[test]
    fn readiness_reports_pid_and_version_then_closes_the_pipe() {
        use std::io::Read;
        // Safe end-to-end, unlike the pre-Decision-1 version of this test:
        // `std::io::pipe` (stable 1.87, below this workspace's 1.88 floor)
        // hands back an owned `PipeWriter`, which converts into `File`
        // through the standard `OwnedFd` bridge — no `unsafe`, because this
        // test created the pipe itself and knows exactly what it is. There
        // is nothing left in this module for `sys::adopt_fd` to be called
        // on; see this file's own module doc.
        let (mut reader, writer) = std::io::pipe().unwrap();
        let pipe = std::fs::File::from(std::os::fd::OwnedFd::from(writer));
        let ready = DaemonReady {
            pid: 4242,
            version: "0.1.0".to_string(),
        };
        write_ready(pipe, &ready).unwrap();
        let mut line = String::new();
        reader.read_to_string(&mut line).unwrap();
        assert_eq!(line.trim_end(), serde_json::to_string(&ready).unwrap());
        assert!(line.ends_with('\n'), "the parent reads a line: {line:?}");
    }

    #[tokio::test]
    async fn boot_writes_readiness_to_the_callers_pipe_after_the_socket_is_bound() {
        // Real time: binds a real socket. Locked per SIGNAL_TEST_LOCK's rule
        // — any test whose `boot()` call succeeds has live signal listeners
        // from that point on.
        //
        // `BootOptions::ready_fd` is `Option<std::fs::File>`, so there is no
        // safe way to hand this test a `File` naming a bad descriptor — the
        // type itself is the proof the fd was valid at construction time.
        // BadFd refusal is tested one layer down instead: see
        // `sys::tests::a_fd_this_process_never_owned_is_refused`, which
        // calls `sys::adopt_fd` directly. This test covers the happy path
        // instead — no other test drives a `Some` `ready_fd` through `boot`
        // at all — a caller-adopted pipe really does receive the readiness
        // line, and only after the socket is genuinely bound (spec §3),
        // exactly as `boot`'s own doc claims.
        use std::io::Read;
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        let (mut reader, writer) = std::io::pipe().unwrap();
        let pipe = std::fs::File::from(std::os::fd::OwnedFd::from(writer));

        let daemon = boot(
            ScriptedRunner::new(vec![]),
            paths.clone(),
            BootOptions {
                ready_fd: Some(pipe),
                ..BootOptions::default()
            },
        )
        .await
        .unwrap();
        assert!(
            paths.socket.exists(),
            "boot must bind the socket before it returns"
        );

        // `write_ready` closes its `File` at the end of `boot`'s own call
        // to it, so this read observes EOF (the readiness line, then
        // nothing) without blocking on a live writer.
        let mut line = String::new();
        reader.read_to_string(&mut line).unwrap();
        let ready: DaemonReady = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(ready.pid, std::process::id());
        assert!(line.ends_with('\n'), "the parent reads a line: {line:?}");

        drop(daemon); // no run() needed; SignalTasks::drop stops the listeners
    }

    #[tokio::test]
    async fn boot_restores_a_saved_flock_and_tears_down_in_order() {
        // Real time: binds a real socket. Locked against
        // `sigterm_triggers_the_same_graceful_shutdown` — see
        // SIGNAL_TEST_LOCK's own doc.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // instances_running is a deliberately WRONG sentinel (99, not the 1
        // instance actually restored below): restore reads `app.instances`
        // to decide how many to start, never this historical count, so it
        // has no effect on the boot itself. Its only job is to make the
        // final-roll assertion below load-bearing: if teardown's roll write
        // is ever skipped (e.g. because a reordering runs it after the
        // supervisor has already stopped, when `snapshot_now` silently
        // no-ops rather than querying a dead engine), this stale 99 survives
        // untouched and the assertion catches it. A seeded `1` would have
        // matched the correct post-teardown value by coincidence and let a
        // skipped write pass unnoticed.
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp {
                app: AppConfig::minimal("web", "./srv"),
                instances_running: 99,
            }],
        };
        crate::snapshot::write_atomic(&paths.snapshot, &roll).unwrap();

        let daemon = boot(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            BootOptions {
                restore: true,
                ..BootOptions::default()
            },
        )
        .await
        .unwrap();
        let ctx = daemon.context();
        let flock = ctx.supervisor.list_checked().await.unwrap();
        assert_eq!(flock.len(), 1, "the muster roll must be back on its feet");
        assert_eq!(flock[0].name, "web");

        let run = tokio::spawn(daemon.run());
        ctx.shutdown();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // The roll written during teardown records the flock as it WAS.
        let final_roll = crate::snapshot::read(&paths.snapshot).unwrap();
        assert_eq!(
            final_roll.apps[0].instances_running, 1,
            "the roll must be written before the flock is killed, or muster restores nothing"
        );
        assert!(
            !paths.socket.exists(),
            "the socket is unlinked on a clean exit"
        );
        assert_eq!(read_pidfile(&paths).unwrap(), None);
    }

    /// fails if `delete_flock_on_shutdown` leaves anything in the final
    /// roll. What this pins is the SHARED shutdown watch: `ctx.shutdown()`
    /// flips the same watch `run`'s `install_signals` handler flips on a
    /// caught `SIGTERM`, WITHOUT going through any caller-level
    /// `Stop`/`Delete` request first, which is precisely the gap a CLI-side
    /// `tidy_up` flag alone cannot close. It does not raise a real `SIGTERM`
    /// and proves nothing about the signal-listener wiring itself — only
    /// about the shutdown path both routes share. If `delete_flock_on_shutdown`
    /// is ever ignored — or is threaded through as `false` by mistake — this
    /// app survives into the roll exactly as
    /// `boot_restores_a_saved_flock_and_tears_down_in_order` above expects
    /// for the ordinary (non-`dev`) case, and this assertion catches it.
    #[tokio::test]
    async fn delete_flock_on_shutdown_clears_the_roll_even_on_a_signalled_exit() {
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();

        let daemon = boot(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            BootOptions {
                delete_flock_on_shutdown: true,
                ..BootOptions::default()
            },
        )
        .await
        .unwrap();
        let ctx = daemon.context();
        let held = shep_core::config::normalize(AppConfig::minimal("held", "./held")).unwrap();
        ctx.registry.record(std::slice::from_ref(&held));
        ctx.supervisor.start(vec![held]).await.unwrap();
        let flock = ctx.supervisor.list_checked().await.unwrap();
        assert_eq!(flock.len(), 1, "the held app must actually be up");

        let run = tokio::spawn(daemon.run());
        // The signal path, not a caller-level `Stop`/`Delete` pair: `run`'s
        // own `install_signals` handler flips this same watch on a real
        // `SIGTERM`, and never runs anything this test hasn't run either.
        ctx.shutdown();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let final_roll = crate::snapshot::read(&paths.snapshot).unwrap();
        assert!(
            final_roll.apps.is_empty(),
            "delete_flock_on_shutdown must leave the roll empty, not {:?}",
            final_roll.apps
        );
    }

    /// fails if the dogs come up before the muster restore, or not at all.
    /// The order half is the point: a metrics dog that starts first answers
    /// for an empty flock for the whole restore window, and a bark dog
    /// raises a start alert for every sheep the roll brought back. The
    /// assertion reads the ORDER the scripted runner was asked to spawn in
    /// — [`ScriptedRunner`] hands out pids as `FIRST_SCRIPTED_PID + index`,
    /// so a lower pid is an earlier spawn — which is the only place the
    /// sequence is observable.
    #[tokio::test]
    async fn boot_restores_the_flock_before_it_lets_the_dogs_out() {
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp {
                app: AppConfig::minimal("web", "./srv"),
                instances_running: 1,
            }],
        };
        crate::snapshot::write_atomic(&paths.snapshot, &roll).unwrap();

        let daemon = boot(
            // Two scripts: the restored sheep's spawn, then the dog's.
            ScriptedRunner::new(vec![ProcScript::never_exits(), ProcScript::never_exits()]),
            paths.clone(),
            BootOptions {
                restore: true,
                dogs: vec![DogSpec {
                    name: "metrics".to_string(),
                    source: DogSource::BuiltIn,
                }],
                ..BootOptions::default()
            },
        )
        .await
        .unwrap();

        let ctx = daemon.context();
        let flock = ctx.supervisor.list_checked().await.unwrap();
        assert_eq!(flock.len(), 2, "the sheep and the dog must both be up");

        let sheep = flock
            .iter()
            .find(|p| p.name == "web")
            .expect("the restored sheep must be present");
        let dog = flock
            .iter()
            .find(|p| p.name == "metrics")
            .expect("the dog must be present");
        assert!(
            sheep.dog.is_none(),
            "the restored app must carry no dog marker"
        );
        assert_eq!(
            dog.dog,
            Some(DogSource::BuiltIn),
            "the dog entry must carry its source"
        );
        assert!(
            sheep.pid < dog.pid,
            "the sheep must be spawned before the dog: sheep={:?} dog={:?}",
            sheep.pid,
            dog.pid
        );

        drop(daemon); // no run() needed; SignalTasks::drop stops the listeners
    }

    /// fails if a dog that will not start takes the boot down with it. The
    /// dog is given no script to spawn against — `ScriptedRunner` answers
    /// `SpawnFailed("script exhausted")` for the first unscripted spawn — so
    /// the flock must still come up, and the daemon must still serve.
    ///
    /// NOT `#[tokio::test]`: `capture_logs` needs a synchronous closure to
    /// scope its subscriber to (see its own doc), so this drives `boot`
    /// through a `block_on` of its own inside that closure instead —
    /// `two_concurrent_boots_on_a_stale_socket_exactly_one_wins`, above,
    /// establishes the same `new_current_thread` pattern for the same
    /// reason.
    #[test]
    fn a_dog_that_will_not_start_does_not_fail_the_boot() {
        let _guard = SIGNAL_TEST_LOCK.blocking_lock();
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let mut boot_result = None;
        let logs = capture_logs(|| {
            boot_result = Some(rt.block_on(boot(
                // No scripts queued: the dog's spawn is the first (and
                // only) one attempted, and finds nothing to pop.
                ScriptedRunner::new(vec![]),
                paths.clone(),
                BootOptions {
                    dogs: vec![DogSpec {
                        name: "metrics".to_string(),
                        source: DogSource::BuiltIn,
                    }],
                    ..BootOptions::default()
                },
            )));
        });
        let daemon = boot_result
            .unwrap()
            .expect("a dog that will not start must not fail the boot");

        let flock = rt
            .block_on(daemon.context().supervisor.list_checked())
            .unwrap();
        let dog = flock
            .iter()
            .find(|p| p.name == "metrics")
            .expect("the dog's entry must still be registered");
        assert_eq!(
            dog.status,
            ProcStatus::Errored,
            "a dog that could not spawn is errored, not silently absent"
        );
        assert!(
            logs.contains("metrics"),
            "the warning must name the dog that did not start: {logs:?}"
        );
        assert!(
            logs.contains("WARN"),
            "a dog failing to start is a warning, not silence: {logs:?}"
        );

        drop(daemon); // no run() needed; SignalTasks::drop stops the listeners
    }

    /// fails if this boot path answers a name collision the way
    /// `Request::EnableDog`'s own handler refuses to. `start_dog` is
    /// idempotent by NAME: enabling a dog under a name a sheep already
    /// holds comes back `Ok` over the SHEEP, not a started dog, and the RPC
    /// arm inspects that reply for the missing `dog` marker so it can
    /// refuse rather than report a success that never happened. Task 6
    /// flagged this exact gap on the boot path when it built the guard into
    /// the RPC arm alone — this pins that `spawn_enabled_dogs` closes it
    /// too, rather than logging the sheep's own info as though a dog had
    /// started.
    ///
    /// `#[test]` + `capture_logs`, not `#[tokio::test]`, for the same
    /// reason as `a_dog_that_will_not_start_does_not_fail_the_boot` above.
    #[test]
    fn a_dog_enabled_under_a_sheeps_name_does_not_start_and_does_not_fail_the_boot() {
        let _guard = SIGNAL_TEST_LOCK.blocking_lock();
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp {
                app: AppConfig::minimal("metrics", "./srv"),
                instances_running: 1,
            }],
        };
        crate::snapshot::write_atomic(&paths.snapshot, &roll).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let mut boot_result = None;
        let logs = capture_logs(|| {
            boot_result = Some(rt.block_on(boot(
                // One script: the restored sheep's own spawn. `start_dog`
                // finds the name already registered and returns early
                // without ever touching the runner, so a second script
                // here would go unconsumed if that held.
                ScriptedRunner::new(vec![ProcScript::never_exits()]),
                paths.clone(),
                BootOptions {
                    restore: true,
                    dogs: vec![DogSpec {
                        name: "metrics".to_string(),
                        source: DogSource::BuiltIn,
                    }],
                    ..BootOptions::default()
                },
            )));
        });
        let daemon = boot_result
            .unwrap()
            .expect("a name collision must not fail the boot");

        let flock = rt
            .block_on(daemon.context().supervisor.list_checked())
            .unwrap();
        assert_eq!(
            flock.len(),
            1,
            "the collision must not register a second entry: {flock:?}"
        );
        assert!(
            flock[0].dog.is_none(),
            "the sheep must not be relabeled as a dog by a same-named enable: {:?}",
            flock[0]
        );
        assert!(
            logs.contains("metrics"),
            "the warning must name the collision: {logs:?}"
        );

        drop(daemon); // no run() needed; SignalTasks::drop stops the listeners
    }

    /// fails if the notification is sent before the muster restore finishes.
    /// That ordering is the entire reason `Type=notify` was chosen over
    /// `Type=simple`: a unit that goes green at exec time reports a flock
    /// that is not up yet, and a restore that hangs reads as a healthy
    /// service supervising nothing.
    ///
    /// **How it tells the two orders apart**, since a test that only checks
    /// the notification arrived would prove nothing about when: the restore
    /// announces its own spawn on the SAME socket, so what is asserted is
    /// the queue order of two datagrams rather than the presence of one.
    /// Reading only `READY=1` after `boot` returns would pass on a notify
    /// moved to the very top of `boot`, because the kernel keeps that
    /// datagram queued however early it was sent — confirmed by moving the
    /// send above step 4 and watching this case fail on the first `recv`.
    /// The flock is asserted afterwards too, so a `SPAWNED` from something
    /// other than a completed restore could not carry the case.
    #[tokio::test]
    async fn readiness_is_reported_only_once_the_roll_is_restored() {
        // Real time: binds a real socket, so this obeys SIGNAL_TEST_LOCK's
        // rule like every other successful `boot()` in this module.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        crate::snapshot::write_atomic(
            &paths.snapshot,
            &FlockSnapshot {
                version: SNAPSHOT_VERSION,
                saved_at_ms: 0,
                apps: vec![SavedApp {
                    app: AppConfig::minimal("web", "./srv"),
                    instances_running: 1,
                }],
            },
        )
        .unwrap();

        // Inside the TempDir and short: macOS caps a unix socket path near
        // 97 characters, which `test_paths` already keeps this under.
        let notify_path = dir.path().join("n.sock");
        let listener = std::os::unix::net::UnixDatagram::bind(&notify_path).unwrap();
        // Bounded: a datagram that never arrives must fail this case, not
        // park it.
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // Both events land on the SAME socket, so the assertion is on their
        // ORDER rather than on their presence. This is the whole design of
        // the case: reading only READY=1 after `boot` returns would pass on
        // a notify moved to the TOP of `boot`, because the datagram is
        // queued by the kernel and is still there whenever the test looks.
        // A marker sent from inside the restore's own spawn is the only
        // thing that distinguishes the two orders. AF_UNIX SOCK_DGRAM
        // enqueues synchronously, and these two sends are strictly
        // sequential in program order, so the queue order is the program
        // order.
        let runner = AnnouncingRunner::new(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            &notify_path,
        );

        let daemon = boot(
            runner,
            paths.clone(),
            BootOptions {
                restore: true,
                notify_socket: Some(notify_path.clone().into_os_string()),
                ..BootOptions::default()
            },
        )
        .await
        .unwrap();

        let mut buf = [0u8; 64];
        let read = listener.recv(&mut buf).unwrap();
        assert_eq!(
            &buf[..read],
            b"SPAWNED\n",
            "READY=1 arrived before the roll was restored: a unit that goes \
             green at exec time reports a flock that is not up yet, and a \
             restore that hangs reads as a healthy service supervising nothing"
        );
        let read = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..read], b"READY=1\n");

        let ctx = daemon.context();
        let flock = ctx.supervisor.list_checked().await.unwrap();
        assert_eq!(flock.len(), 1, "the roll was actually restored");
        assert_eq!(flock[0].name, "web");

        ctx.shutdown();
        daemon.run().await.unwrap();
    }

    /// fails if an undeliverable readiness datagram takes the daemon down
    /// with it. Nothing is bound at the address, so the send errors — and
    /// the boot must still succeed, because what failed is the init
    /// system's *knowledge* of a daemon that is otherwise fully up. Systemd
    /// reports that honestly through its own `TimeoutStartSec`; killing a
    /// working supervisor over one datagram would be the worse outcome, and
    /// on a `?` here a reboot would leave the flock down instead of merely
    /// unannounced.
    #[tokio::test]
    async fn a_readiness_datagram_that_cannot_be_delivered_does_not_fail_the_boot() {
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        let daemon = boot(
            ScriptedRunner::new(vec![]),
            paths.clone(),
            BootOptions {
                // Bound by nothing, and never created by anything: the send
                // is an error, which is the whole premise of the case.
                notify_socket: Some(dir.path().join("nobody.sock").into_os_string()),
                ..BootOptions::default()
            },
        )
        .await
        .expect("a daemon nobody could be told about is still a daemon");

        // Up enough to serve, not merely constructed.
        assert!(daemon.context().supervisor.list_checked().await.is_ok());

        daemon.context().shutdown();
        daemon.run().await.unwrap();
    }

    #[tokio::test]
    async fn sigterm_triggers_the_same_graceful_shutdown() {
        // Real time + a real signal. Safe to raise here only because the
        // handler is installed first: SIGTERM's default action would kill
        // the test binary. tokio never uninstalls it, which is harmless.
        // Locked against `boot_restores_a_saved_flock_and_tears_down_in_order`
        // — see SIGNAL_TEST_LOCK's own doc: this raise() is process-wide and
        // would otherwise be observed by that test's daemon too.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let daemon = boot(
            ScriptedRunner::new(vec![]),
            paths.clone(),
            BootOptions::default(),
        )
        .await
        .unwrap();
        // No sleep needed here (there was one; see git history): signal
        // handlers are installed inside `boot`, which this call already
        // `.await`ed to completion, so they are provably live the moment
        // `boot` returns — well before `run()`'s own task is ever polled.
        let run = tokio::spawn(daemon.run());
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!paths.socket.exists());
    }

    #[tokio::test]
    async fn sighup_triggers_the_same_graceful_shutdown() {
        // SIGHUP's default disposition is to terminate the process, and
        // this handler is what replaces that default. SIGHUP is the
        // handover trigger now, but a boot that has not opted in
        // (`BootOptions::handover`, left `false` by the default below) has
        // no successor to become, and a stray or mistaken SIGHUP must still
        // walk the same graceful path SIGTERM does rather than drop the
        // flock's pipes.
        //
        // That default is also what keeps this test from replacing the test
        // binary with a fresh copy of itself, which is what an opted-in boot
        // would do here: `exec_target` resolves the file this process was
        // launched from, and in a test that file is the harness. See
        // `BootOptions::handover`'s own doc.
        //
        // Mirrors `sigterm_triggers_the_same_graceful_shutdown` above —
        // see that test's own comments for why raising a real signal here
        // is safe only because the handler is installed first, and for
        // why `SIGNAL_TEST_LOCK` is required.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let daemon = boot(
            ScriptedRunner::new(vec![]),
            paths.clone(),
            BootOptions::default(),
        )
        .await
        .unwrap();
        let run = tokio::spawn(daemon.run());
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGHUP).unwrap();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!paths.socket.exists());
    }

    /// The daemon-side gate, and why a client asking first is not enough.
    ///
    /// A signal is a signal: anyone can send one, and the flock can change
    /// between the client's question and the signal. So the SIGHUP path runs
    /// [`crate::handover::fitness`] again and refuses on its own, which is
    /// what makes the fallback to a graceful stop reachable at all.
    ///
    /// **The descriptors are deliberately invalid.** If the gate ever
    /// stopped refusing, `hand_over` would go on to clear `FD_CLOEXEC` on
    /// them, meet `EBADF`, and return, rather than exec'ing this test binary
    /// into an endless re-run of the whole suite. The assertion on
    /// the message is what tells the two failures apart.
    #[tokio::test(start_paused = true)]
    async fn a_sighup_over_a_flock_it_cannot_carry_refuses_before_it_execs() {
        // Real signal listeners, and a PAUSED clock. This case raises
        // nothing itself, but `SIGNAL_TEST_LOCK`'s rule is "calls `boot()`
        // successfully", not "calls `raise()`": a successful `boot()`
        // installs SIGTERM, SIGHUP and SIGUSR2 listeners that run for this
        // test's whole duration, and a concurrent `raise()` in one of the
        // shutdown cases reaches them too. This daemon would then shut down
        // ahead of its own `ctx.shutdown()` while absorbing a delivery the
        // other test needed, which is the way a real regression there gets
        // masked. The lock is about the signals; the clock is a separate
        // question, and pausing it is what stops the refusal this case
        // needs -- a log pump missing `REPORT_DEADLINE` -- costing the
        // suite two real seconds. Measured: 2.0s awake, 0.03s paused.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // A wedged log pump: the one thing the gate still refuses, and the
        // only one it ever will. The case is about the gate firing at all
        // rather than about what fires it, so its fixture has moved every
        // time a phase carried the feature it was reaching for -- a
        // shepherd channel in 2b task 5, two instances in task 6, a dog in
        // phase 3 task 4. There is nothing left for it to move to.
        let daemon = boot(
            ScriptedRunner::new(vec![ProcScript::never_exits()])
                .with_a_pump_that_never_reports(&["wedged"]),
            paths.clone(),
            BootOptions::default(),
        )
        .await
        .unwrap();
        let ctx = daemon.context();
        ctx.supervisor
            .start(vec![
                normalize(AppConfig::minimal("wedged", "./srv")).unwrap(),
            ])
            .await
            .unwrap();

        let seam = HandoverSeam {
            supervisor: ctx.supervisor.clone(),
            fds: crate::handover::DaemonFds {
                listener: -1,
                pidfile: -1,
            },
            paths: paths.clone(),
        };
        let refusal = hand_over_now(&seam)
            .await
            .expect_err("a flock with a wedged log pump cannot be carried");
        assert!(
            refusal.contains("did not report its descriptors in time"),
            "the gate must refuse before anything is exec'd: {refusal}"
        );
        assert!(
            refusal.contains("wedged"),
            "the refusal must name the sheep that held the flock back: {refusal}"
        );

        ctx.shutdown();
        // Bounded, like the sibling above. The lock this case now holds is
        // held across this await, so a teardown that hung would stop every
        // other signal test in the module rather than failing this one.
        tokio::time::timeout(Duration::from_secs(5), daemon.run())
            .await
            .unwrap()
            .unwrap();
    }

    /// The second gate: a flock the fitness check passes, described by a
    /// blob no successor could have adopted, refuses HERE rather than after
    /// the `execve`.
    ///
    /// This is the failure with no way back. Past the exec there is no
    /// predecessor to refuse to: `rehydrate` returns `BootError::Adopt`, the
    /// successor exits without ever serving, and the flock keeps running
    /// with nothing supervising it. So the rehearsal runs while this image
    /// still exists, and a refusal takes the stop-and-start arm the operator
    /// would have had before handovers existed.
    ///
    /// **Read the assertion, not just the `expect_err`.** These descriptors
    /// were already refused before this gate existed, by the `FD_CLOEXEC`
    /// sweep meeting `EBADF`, so a case that only checked for A refusal
    /// would pass with the gate deleted. What is new is WHICH refusal: the
    /// successor's own wording, reached before anything was written or
    /// cleared. The wording is the whole test.
    ///
    /// **The descriptors stay deliberately invalid**, for the reason both
    /// siblings above give. A blob that got past both gates would exec this
    /// test binary into a re-run of the entire suite, so no case in this
    /// module may offer one that could.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sighup_over_a_blob_no_successor_could_adopt_refuses_before_it_execs() {
        // As every sibling: a successful `boot()` installs real signal
        // listeners for this test's whole duration.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // No dog and no sheep, so the FIRST gate passes and this case is
        // about the second one. An empty flock is carryable — see
        // `handover::fitness`'s own doc.
        let daemon = boot(
            ScriptedRunner::new(Vec::new()),
            paths.clone(),
            BootOptions::default(),
        )
        .await
        .unwrap();
        let ctx = daemon.context();

        let seam = HandoverSeam {
            supervisor: ctx.supervisor.clone(),
            fds: crate::handover::DaemonFds {
                listener: -1,
                pidfile: -2,
            },
            paths: paths.clone(),
        };
        let refusal = hand_over_now(&seam)
            .await
            .expect_err("a blob naming no real listener cannot be adopted");
        assert!(
            refusal.contains("a successor could not have adopted this flock"),
            "the rehearsal must be what refuses, not the `FD_CLOEXEC` sweep further on: {refusal}"
        );
        assert!(
            refusal.contains("-1"),
            "the refusal must name the descriptor it refused: {refusal}"
        );
        assert!(
            !crate::handover::Handover::path(&paths).exists(),
            "a refusal before the exec must leave no blob on disk"
        );

        ctx.shutdown();
        tokio::time::timeout(Duration::from_secs(5), daemon.run())
            .await
            .unwrap()
            .unwrap();
    }

    /// The sharp edge of parking a pump: a handover that reports and then
    /// refuses has to leave every pump reading again.
    ///
    /// Taking the snapshot stops each pump where it stands, which is what
    /// makes the report still true at the exec. There is no exec here, and
    /// nothing else in the daemon ever sends a resume, so a missing one is
    /// not a slow log or a lost line: it is a flock that logs nothing more,
    /// through the graceful stop this refusal falls back to and for as long
    /// as the daemon lives after it, having reported no failure beyond the
    /// refusal itself.
    ///
    /// Two sheep, because the resume has to reach every pump that was
    /// reported to rather than the one the refusal named.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_handover_starts_every_pump_reading_again() {
        // As the sibling above: a successful `boot()` installs real signal
        // listeners for this test's whole duration, and the clock is paused
        // so the missed `REPORT_DEADLINE` this refusal needs costs the
        // suite nothing to wait out.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let runner = Arc::new(
            ScriptedRunner::new(vec![ProcScript::never_exits(); 3])
                .with_a_pump_that_never_reports(&["wedged"]),
        );
        // The refusal: a wedged log pump, read AFTER every pump that
        // answered has already been reported to and parked, which is what
        // makes the resume owed. It was a dog until phase 3 task 4 carried
        // one, a shepherd channel until 2b task 5 and two instances until
        // task 6; what the case needs is any refusal at all.
        let daemon = boot(
            SharedRunner(Arc::clone(&runner)),
            paths.clone(),
            BootOptions::default(),
        )
        .await
        .unwrap();
        let ctx = daemon.context();
        ctx.supervisor
            .start(vec![
                normalize(AppConfig::minimal("quiet", "./srv")).unwrap(),
                normalize(AppConfig::minimal("chatty", "./srv")).unwrap(),
                normalize(AppConfig::minimal("wedged", "./srv")).unwrap(),
            ])
            .await
            .unwrap();
        for sheep in 0..3 {
            assert!(
                runner.log_ctl_live(sheep),
                "sheep {sheep} must have a live log pump before the report, or this case                  proves nothing"
            );
        }

        let seam = HandoverSeam {
            supervisor: ctx.supervisor.clone(),
            fds: crate::handover::DaemonFds {
                listener: -1,
                pidfile: -1,
            },
            paths: paths.clone(),
        };
        let refusal = hand_over_now(&seam)
            .await
            .expect_err("a flock with a wedged log pump cannot be carried");
        assert!(
            refusal.contains("did not report its descriptors in time"),
            "the gate must refuse before anything is exec'd: {refusal}"
        );

        let answered: Vec<usize> = ["quiet", "chatty"]
            .iter()
            .map(|name| runner.spawn_index_of(name).expect("started above"))
            .collect();
        let wedged = runner.spawn_index_of("wedged").expect("started above");
        // Polled rather than read once: a resume carries no acknowledgement
        // (see `LogCtl::Resume`), so a send that has returned has been
        // queued rather than served. Bounded, so a pump that is never told
        // fails here instead of hanging.
        let all_resumed = async {
            while answered.iter().any(|sheep| runner.resumes(*sheep) == 0) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), all_resumed)
            .await
            .expect("every pump a refused handover reported to must be reading again");
        for sheep in &answered {
            assert_eq!(
                runner.resumes(*sheep),
                1,
                "sheep {sheep} must be resumed once rather than repeatedly"
            );
        }
        // The plural is what this case adds over its sibling, which has one
        // answering pump: a resume that reached only the first sheep
        // reported to would satisfy that one and fail here.
        assert_eq!(answered.len(), 2, "two pumps must have answered the report");
        assert_eq!(
            runner.resumes(wedged),
            0,
            "a pump that never answered never parked, so nothing may resume it"
        );

        ctx.shutdown();
        tokio::time::timeout(Duration::from_secs(5), daemon.run())
            .await
            .unwrap()
            .unwrap();
    }

    /// The second refusal path, and the one nothing else covers: a snapshot
    /// abandoned because a pump went quiet still owes a resume to every pump
    /// that DID answer.
    ///
    /// Its sibling above proves the resume for a refusal the gate reads off
    /// a sheep's config. A missed deadline is a different way in, and it
    /// arrives with one pump already parked and one that never was. Miss it
    /// and a handover abandoned on a wedged pump leaves the REST of the
    /// flock parked, which is a silent logging stop for the life of the
    /// daemon and strictly worse than the stall this deadline exists to end.
    ///
    /// No `boot()` and no signal here, unlike its sibling: this is about
    /// what `hand_over_now` does with a snapshot, and building the
    /// supervisor directly is what lets the clock be paused, so the deadline
    /// costs the suite nothing to wait out.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_handover_abandoned_on_a_wedged_pump_resumes_the_pumps_that_parked() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let (events, _rx) = crate::bus::test_bus(64);
        let runner = Arc::new(
            ScriptedRunner::new(vec![ProcScript::never_exits(); 2])
                .with_a_pump_that_never_reports(&["wedged"]),
        );
        let supervisor = crate::supervisor::spawn_supervisor(
            SharedRunner(Arc::clone(&runner)),
            paths.clone(),
            events,
        );
        supervisor
            .start(vec![
                normalize(AppConfig::minimal("answering", "./srv")).unwrap(),
                normalize(AppConfig::minimal("wedged", "./srv")).unwrap(),
            ])
            .await
            .unwrap();

        // Deliberately invalid, as its sibling above explains: if the gate
        // ever stopped refusing, `hand_over` would meet `EBADF` and return
        // rather than exec this test binary into a re-run of the whole
        // suite. The assertion on the message is what tells those apart.
        let seam = HandoverSeam {
            supervisor: supervisor.clone(),
            fds: crate::handover::DaemonFds {
                listener: -1,
                pidfile: -1,
            },
            paths: paths.clone(),
        };
        let refusal = hand_over_now(&seam)
            .await
            .expect_err("a flock with a pump that never reported cannot be carried");
        assert!(
            refusal.contains("did not report its descriptors in time"),
            "the refusal must say the pump went quiet, not something else: {refusal}"
        );
        assert!(
            refusal.contains("wedged"),
            "the refusal must name the sheep whose pump went quiet: {refusal}"
        );

        let answering = runner.spawn_index_of("answering").expect("started above");
        let wedged = runner.spawn_index_of("wedged").expect("started above");
        // Polled rather than read once: a resume carries no acknowledgement
        // (see `LogCtl::Resume`), so a send that has returned has been
        // queued rather than served. Instant under the paused clock, and
        // bounded so a pump that is never told fails here instead of
        // hanging.
        let delivered = async {
            while runner.resumes(answering) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), delivered)
            .await
            .expect("the pump that answered was parked, and a refusal owes it a resume");
        assert_eq!(
            runner.resumes(wedged),
            0,
            "a pump that never answered never parked, so nothing may resume it"
        );
    }

    #[tokio::test]
    async fn a_repeat_sigterm_is_observed_not_swallowed() {
        // Pins Decision 3 (2026-08-08): each shutdown-signal listener
        // `install_signals` spawns stays armed for the rest of the
        // process's life instead of returning after one `recv()`. Tests
        // `install_signals` directly rather than the whole `boot()`/`run()`
        // teardown — the bug and its fix live entirely in this one loop,
        // and driving a genuinely slow teardown here would only add
        // unrelated timing noise to what is otherwise a fully
        // deterministic check.
        //
        // Real time + real signals — see SIGNAL_TEST_LOCK's own doc: this
        // raise() is process-wide.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let shutdown = Arc::new(shutdown);
        // The SIGUSR2 and SIGHUP halves of the return are dropped unused:
        // this case drives the shutdown listeners only, and a dropped sender
        // simply ends the task parked on its receiver (see
        // `install_signals`'s own doc) without disturbing the three below.
        let (signals, _connect_supervisor, _connect_handover) =
            install_signals(shutdown, paths).unwrap();

        // First SIGTERM: starts shutdown, exactly as before this decision.
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), shutdown_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*shutdown_rx.borrow());

        // The regression this test pins: on the pre-fix code, the SIGTERM
        // listener task RETURNED after that one signal (see
        // `install_signals`'s own doc for the full history) — `is_finished`
        // would already read `true` here. Looped, it never finishes on its
        // own; only `SignalTasks::drop`'s `abort()` stops it.
        assert!(
            !signals.tasks[0].is_finished(),
            "the SIGTERM listener must still be polling after its first signal, not have exited"
        );

        // A second SIGTERM, into the same "already shutting down" state a
        // real slow teardown would still be in when a repeat arrives (the
        // watch channel is already `true`, exactly as `run()` would leave
        // it while its own teardown steps run). The pre-fix version would
        // have dropped this on the floor: no live task left to receive it,
        // and no process kill either, since installing this handler already
        // replaced SIGTERM's default terminate disposition.
        // `watch::Sender::send` marks its channel changed on every call
        // regardless of whether the value differs (confirmed against
        // tokio's own source, matching the precedent `RunningDaemon::
        // shutdown_rx`'s own doc already relies on for a different
        // scenario) — so a SECOND `changed()` resolving here, for a value
        // that was already `true`, is airtight proof the loop delivered
        // this signal all the way through to another `shutdown.send()`
        // call, not a timing coincidence.
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), shutdown_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !signals.tasks[0].is_finished(),
            "the SIGTERM listener must still be armed after a SECOND signal too"
        );

        drop(signals); // aborts the listener tasks (SignalTasks::drop)
    }

    // `boot` is the ONE place `DEFAULT_MAX_CRON_SLEEP` is applied — the CLI
    // half of the plumbing keeps the knob an `Option` all the way down, and
    // its own test pins that — so nothing else in this workspace would notice
    // a different fallback landing here. fails if the default is replaced (a
    // stray `Duration::from_secs(1)` would have every cron worker in the
    // daemon waking a minute more often than the constant says), and fails if
    // the configured value is dropped for one of `max_cron_sleep`'s own
    // invention.
    //
    // Whole `BootOptions` values rather than bare `Option`s, because that is
    // what `boot` hands over: the field this reads is the field the daemon
    // runs with, with no projection in between for a call site to get wrong.
    #[test]
    fn an_unset_max_cron_sleep_falls_back_to_the_daemons_own_default() {
        assert_eq!(
            max_cron_sleep(&BootOptions::default()),
            DEFAULT_MAX_CRON_SLEEP,
            "unset means the default"
        );
        assert_eq!(
            max_cron_sleep(&BootOptions {
                max_cron_sleep: Some(Duration::from_secs(300)),
                ..BootOptions::default()
            }),
            Duration::from_secs(300),
            "a configured value must reach the workers unchanged"
        );
    }

    // fails if `boot` never spawns the extras reporter. Nothing else in this
    // crate drives that call — every other reporter case constructs one by
    // hand — so dropping it here would leave a real daemon in which no memory
    // breach and no liveness failure ever restarts anything, with every unit
    // test still green.
    //
    // The whole production chain is what makes the claim: the actor arms a
    // liveness loop at the Online transition, the loop reports over the sender
    // `Extras::real` was built with, the reporter reads it, and
    // `extra_restart`'s guards let it through. Real time and a real
    // `OsProber` — a paused clock does not move a real TCP connect.
    #[tokio::test]
    async fn a_booted_daemon_restarts_a_sheep_whose_liveness_probe_fails() {
        // Real time: binds a real socket, so it takes the signal lock like
        // every other successful `boot()` here — see SIGNAL_TEST_LOCK's doc.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        // Reserve a port, then release it: nothing ever listens there, so
        // every probe fails with a connection refusal, with no listener to
        // race and no port to reserve for real.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);

        let daemon = boot(
            ScriptedRunner::new(vec![ProcScript::never_exits(); 4]),
            paths.clone(),
            BootOptions::default(),
        )
        .await
        .unwrap();
        let ctx = daemon.context();
        let mut events = ctx.events.subscribe();
        let run = tokio::spawn(daemon.run());

        let mut app = AppConfig::minimal("web", "./srv");
        app.liveness_probe = Some(ProbeConfig {
            kind: ProbeKind::Tcp,
            target: addr.to_string(),
            // The loop floors anything shorter at one second, so a smaller
            // number here would be a lie about what this test waits for.
            interval: UpDuration::from_millis(1_000),
            timeout: UpDuration::from_millis(500),
            failure_threshold: 1,
        });
        ctx.supervisor
            .start(vec![normalize(app).unwrap()])
            .await
            .unwrap();

        let restarted = async {
            loop {
                match events.recv().await.map(|event| event.to_event()) {
                    Ok(BusEvent::Process {
                        event: ProcessEventKind::Restart,
                        info,
                        ..
                    }) => return info,
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(err) => panic!("the event stream closed before a restart: {err}"),
                }
            }
        };
        let info = tokio::time::timeout(Duration::from_secs(20), restarted)
            .await
            .expect("a failing liveness probe must restart its sheep");
        assert_eq!(info.id, 0);
        assert_eq!(info.restarts, 1);

        ctx.shutdown();
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    // fails if SIGUSR2 stops meaning `reopen all`: a listener that logs the
    // signal and reaches no pump, one wired to a narrower selector, or a
    // `boot` that never hands the listener the supervisor it reopens through
    // (the step-1/step-4 seam). Nothing else in this workspace drives that
    // seam — `shep reopen` reaches the same supervisor over the socket
    // instead, so every RPC-tier reopen test stays green with the signal path
    // dead.
    //
    // Both instances are asserted, not just one: `All` is the whole claim,
    // and a listener that reopened the first sheep it found would satisfy
    // half of it.
    #[tokio::test]
    async fn sigusr2_reopens_every_sheeps_log_files() {
        // Real time + a real signal, so it takes the lock like every other
        // successful `boot()` here — see SIGNAL_TEST_LOCK's own doc: this
        // raise() is process-wide. Raising SIGUSR2 is safe only because
        // `boot` below has already returned, having replaced a default
        // disposition that would otherwise kill the test binary outright.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        // TWO scripts for two instances, counted against what a BROKEN
        // implementation demands rather than a working one: `ScriptedRunner`
        // answers `SpawnFailed("script exhausted")` once it runs out, which
        // lands that sheep `Errored` with no log pump at all — a state this
        // case could not tell apart from the pump nobody asked to reopen.
        // The `log_ctl_live` assertions below are the other half of that
        // guard: they say a pump is there to be reached before the signal is
        // ever raised.
        let runner = Arc::new(ScriptedRunner::new(vec![ProcScript::never_exits(); 2]));
        let daemon = boot(
            SharedRunner(Arc::clone(&runner)),
            paths.clone(),
            BootOptions::default(),
        )
        .await
        .unwrap();
        let ctx = daemon.context();
        let run = tokio::spawn(daemon.run());

        let mut web = AppConfig::minimal("web", "./srv");
        web.instances = 2;
        ctx.supervisor
            .start(vec![normalize(web).unwrap()])
            .await
            .unwrap();
        for instance in 0..2 {
            assert!(
                runner.log_ctl_live(instance),
                "instance {instance} must have a live log pump before the signal, or this \
                 case proves nothing"
            );
            assert_eq!(
                runner.reopens(instance),
                0,
                "instance {instance} must not have been reopened before the signal"
            );
        }

        nix::sys::signal::raise(nix::sys::signal::Signal::SIGUSR2).unwrap();

        // Polled rather than awaited: a signal has no reply channel, so
        // there is nothing to await on — the counters are the only place the
        // reopen becomes visible. Bounded, so a listener that never reaches a
        // pump fails here instead of hanging.
        let both_reopened = async {
            while runner.reopens(0) == 0 || runner.reopens(1) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), both_reopened)
            .await
            .expect("SIGUSR2 must reopen every sheep's log files");

        ctx.shutdown();
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// One carried sheep, named, with `dog` set or not.
    #[cfg(unix)]
    fn carried_for_the_roll(
        name: &str,
        dog: Option<DogSource>,
    ) -> crate::handover::adopt::AdoptedSheep {
        let mut entry = crate::entry::ProcessEntry {
            id: 1,
            spec: normalize(AppConfig::minimal(name, "./srv")).unwrap(),
            instance: 0,
            status: shep_core::status::ProcStatus::Online,
            pid: Some(4242),
            restarts: 0,
            started_at: None,
            budget: crate::entry::RestartBudget::default(),
            reload: crate::entry::ReloadState::None,
            credentials: crate::privilege::SpawnIdentity::Resolved(None),
            out_file: PathBuf::new(),
            err_file: PathBuf::new(),
            dog: None,
            last_exit: None,
        };
        entry.dog = dog;
        crate::handover::adopt::AdoptedSheep {
            carried: crate::handover::CarriedSheep::from_entry(
                &entry,
                0,
                crate::handover::CarriedFds::none(),
                false,
                None,
                false,
                None,
            ),
            out_pipe: None,
            err_pipe: None,
            out_log: None,
            err_log: None,
            stdin_pipe: None,
            channel: None,
        }
    }

    /// fails if a carried dog reaches the muster roll.
    ///
    /// A successor rebuilds its registry from the blob rather than from the
    /// roll, and the registry is what the roll on disk is written from
    /// within seconds. A dog has never been in it -- `spawn_enabled_dogs`
    /// registers dogs straight through the supervisor and never touches
    /// `FlockRegistry` -- so a successor that recorded one would put it
    /// there for the first time, and permanently: the roll outlives the
    /// daemon, so a later cold boot would restore `metrics` as an ordinary
    /// unmarked sheep BEFORE `spawn_enabled_dogs` ran, and `shep disable
    /// metrics` could not take it back out.
    ///
    /// Both rows, because a filter that dropped everything would satisfy
    /// the negative half on its own -- and an empty registry is the failure
    /// this whole `record_config` path exists to prevent, since it would
    /// overwrite a good roll with an empty one.
    #[cfg(unix)]
    #[test]
    fn a_carried_dog_does_not_reach_the_muster_roll() {
        let flock = vec![
            carried_for_the_roll("web", None),
            carried_for_the_roll("metrics", Some(DogSource::BuiltIn)),
            carried_for_the_roll(
                "log-rotate",
                Some(DogSource::Adopted {
                    path: "/opt/bin/shep-log-rotate".to_string(),
                }),
            ),
        ];

        let names: Vec<String> = apps_for_the_roll(&flock)
            .into_iter()
            .map(|app| app.name)
            .collect();

        assert_eq!(
            names,
            vec!["web".to_string()],
            "the roll is the operator's flock; a dog belongs to `shep.toml`"
        );
    }

    /// A blob written by hand rather than by `Handover::write`, so this
    /// module's tests pin the on-disk shape a successor has to read rather
    /// than round-tripping whatever the writer happens to emit.
    fn write_blob(path: &Path, version: u32) {
        std::fs::write(
            path,
            format!(
                r#"{{"version":{version},"sheep":[],"listener_fd":3,"pidfile_fd":4,"next_id":0,"next_deadline":0,"next_action_stamp":0}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_blob_on_disk_makes_this_process_a_successor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handover.json");
        write_blob(&path, 1);

        assert!(successor_handover_at(&path).is_some());
    }

    #[test]
    fn a_missing_blob_is_refused_out_loud_rather_than_silently() {
        // A stale inherited variable and a lost blob look the same from
        // here, and neither may pass for a fresh boot without a word.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-written.json");

        let logs = capture_logs(|| assert!(successor_handover_at(&path).is_none()));

        assert!(logs.contains("never-written.json"), "{logs}");
    }

    #[test]
    fn a_blob_of_an_unknown_version_is_refused_out_loud() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handover.json");
        write_blob(&path, u32::MAX);

        let logs = capture_logs(|| assert!(successor_handover_at(&path).is_none()));

        assert!(logs.contains("version"), "{logs}");
    }

    #[test]
    fn a_refused_blob_is_left_on_disk_for_an_operator_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handover.json");
        write_blob(&path, u32::MAX);

        capture_logs(|| assert!(successor_handover_at(&path).is_none()));

        assert!(path.exists(), "a refused blob is evidence, not litter");
    }
}
