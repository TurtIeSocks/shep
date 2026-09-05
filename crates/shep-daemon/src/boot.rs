//! Daemon boot: layout, pidfile, control-socket bind, and the run/teardown
//! sequence
//!
//! `init_dirs` creates every layout directory `0700` and tightens one that
//! already exists looser, the guarantee `crate::server::RpcServer`'s doc names
//! as boot's. [`boot`] assembles bus, supervisor, muster roll and RPC context
//! into one [`RunningDaemon`]; [`RunningDaemon::run`] serves until a signal or
//! `KillDaemon`, then tears down in a load-bearing order.
//!
//! [`BootOptions::ready_fd`] arrives as an owned [`std::fs::File`]:
//! `crate::sys::adopt_fd`'s ordering precondition is process-wide and `boot`
//! is `async`, so only the CLI's `main` can discharge it.

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
// unix only: read by the SIGUSR2 log-reopen handler, which Windows has none of.
#[cfg(unix)]
use crate::supervisor::SupervisorError;

/// Mode for every directory shep creates (spec §10: no other user, at all)
pub const DIR_MODE: u32 = 0o700;

/// Capacity of each of the two lifecycle-extra report channels.
///
/// Bounded, so a report producer that outruns the reporting task
/// back-pressures instead of queueing restarts nobody has performed. 64 fits
/// a whole flock breaching on one sampling pass.
const EXTRAS_REPORT_CAPACITY: usize = 64;

/// Creates `dir` and any missing parents at [`DIR_MODE`] via
/// [`DirBuilderExt::mode`], closing the TOCTOU a `create_dir_all` plus a
/// separate `chmod` leaves open: the umask-derived mode in between is wide
/// enough to race a symlink onto the socket path underneath.
///
/// Does not touch a directory that already exists; that is [`init_dirs`]'s
/// `set_permissions` pass.
#[cfg(windows)]
fn create_dir_at_dir_mode(dir: &Path) -> std::io::Result<()> {
    // No mode to set: `DIR_MODE` is a POSIX word, Windows access control is an
    // inherited ACL. The control pipe's ACL guards the socket instead, so
    // `flock.json`'s `env` stays readable to another account with access to
    // the profile; the operator docs name that gap.
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
/// Idempotent: a restart onto an existing layout forces every directory back
/// to [`DIR_MODE`].
///
/// # Errors
/// - [`BootError::Io`] if a directory could not be created or chmod'ed.
pub(crate) fn init_dirs(paths: &ShepPaths) -> Result<(), BootError> {
    for dir in [&paths.home, &paths.logs, &paths.pids, &paths.run] {
        create_dir_at_dir_mode(dir).map_err(|source| BootError::Io {
            path: dir.clone(),
            source,
        })?;
        // Unix only: Windows has no scalar mode to force a directory back to.
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
#[must_use]
pub fn pidfile(paths: &ShepPaths) -> PathBuf {
    paths.pids.join("shepd.pid")
}

/// Writes the pidfile atomically: temp file in `pids/`, `fsync`, `rename`.
///
/// Fixture seeding only. [`boot`] records its pid through
/// `PidfileLock::record` instead: a rename over the locked path swaps in an
/// unlocked inode and disarms the lock for the daemon's life.
///
/// # Errors
/// - [`BootError::Io`] if the pidfile could not be written.
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
/// valid pid. A best-effort hint for [`BootError::AlreadyRunning`], never
/// proof that a daemon is live; the lock is that.
///
/// # Errors
/// - [`BootError::Io`] if the pidfile exists but could not be read.
pub(crate) fn read_pidfile(paths: &ShepPaths) -> Result<Option<u32>, BootError> {
    let path = pidfile(paths);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BootError::Io { path, source }),
    }
}

/// This daemon's exclusive claim on `$SHEP_HOME`: an `flock(2)` held on the
/// pidfile from before [`bind_socket`] to the end of [`RunningDaemon::run`].
///
/// Serializes [`bind_socket`]'s stale-socket recovery: unserialized, two
/// daemons both see `ConnectionRefused` on a crashed predecessor's leftover
/// and the loser's `remove_file` deletes the winner's fresh listener. A crash
/// needs no cleanup: the kernel releases the lock with the last descriptor on
/// the open file description.
///
/// Windows locks a sibling `shepd.pid.lock`, since `share_mode(0)` would deny
/// the read-only open the loser needs to name the winner. A successor inherits
/// the descriptor already holding the lock; see [`UnixLock`].
#[derive(Debug)]
struct PidfileLock {
    #[cfg(unix)]
    flock: UnixLock,
    /// The sibling lock file, held open with every share flag cleared. Held,
    /// never read: [`PidfileLock::record`] writes the pidfile itself.
    #[cfg(windows)]
    _handle: std::fs::File,
}

/// How this process came to hold the pidfile's `flock`.
///
/// An adopted descriptor is never re-locked: `nix` has no constructor for an
/// already-locked file that leaves it locked, and the release-then-relock
/// window is long enough for a second daemon to claim this `$SHEP_HOME`. The
/// lock crosses an `execve` with the descriptor, so there is nothing to redo.
/// Either arm releases the same way, when the last descriptor on the open file
/// description closes.
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
    /// The locked pidfile, which [`PidfileLock::record`] writes through.
    fn file(&mut self) -> &mut std::fs::File {
        match self {
            Self::Taken(flock) => flock,
            Self::Adopted(file) => file,
        }
    }

    /// The descriptor carrying the lock, for the blob a handover hands on.
    ///
    /// Borrowed, never owned: closing it releases the `flock`.
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
/// Never renamed, never read, left on disk between boots, as `kv.json.lock`
/// and `barks.jsonl.lock` are.
#[cfg(windows)]
fn pidfile_lock_path(paths: &ShepPaths) -> PathBuf {
    paths.pids.join("shepd.pid.lock")
}

impl PidfileLock {
    /// Opens (creating if necessary) and takes an exclusive, non-blocking
    /// `flock` on `paths`'s pidfile.
    ///
    /// Does not truncate on open: a loser's [`BootError::AlreadyRunning`]
    /// reads the pid the winner recorded through [`Self::record`].
    ///
    /// # Errors
    /// - [`BootError::AlreadyRunning`] if another process holds this lock,
    ///   carrying the pid recorded in the file if any.
    /// - [`BootError::Io`] if the pidfile could not be opened.
    #[cfg(unix)]
    fn acquire(paths: &ShepPaths) -> Result<Self, BootError> {
        let path = pidfile(paths);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false) // preserve any pid a previous winner recorded
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
    /// - [`BootError::AlreadyRunning`] if another process holds this lock,
    ///   carrying the pid the winner recorded in the pidfile, which stays
    ///   readable because the lock is on a sibling.
    /// - [`BootError::Io`] if the lock file could not be opened.
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
            // Immediate, never retried: a second daemon is refused now rather
            // than queued behind the first.
            Err(err) if err.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                Err(BootError::AlreadyRunning {
                    pid: read_pidfile(paths)?,
                })
            }
            Err(source) => Err(BootError::Io { path, source }),
        }
    }

    /// Overwrites the locked pidfile's content with `pid` in place: truncate,
    /// then write at offset 0. Never a temp file plus `rename`, which would
    /// swap in an inode nothing has locked.
    ///
    /// # Errors
    /// - [`BootError::Io`] if the write failed.
    #[cfg(windows)]
    fn record(&mut self, paths: &ShepPaths, pid: u32) -> Result<(), BootError> {
        use std::io::Write as _;

        // An ordinary write: this arm's lock is on the sibling `.lock`, so
        // there is no already-locked handle to write through.
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
    /// Nothing here locks, unlocks, truncates or writes: `file` crossed an
    /// `execve` with its `flock` intact, and an `execve` keeps the pid, so the
    /// number the predecessor recorded is this process's own.
    #[cfg(unix)]
    fn from_locked(file: std::fs::File) -> Self {
        Self {
            flock: UnixLock::Adopted(file),
        }
    }

    /// The descriptor the lock lives on, for a handover blob to name.
    ///
    /// Borrowed: closing it frees this `$SHEP_HOME` for the next claimant.
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

/// The apps a successor records in its own registry, which is what the muster
/// roll on disk is written from.
///
/// Every carried sheep's, and no dog's: the roll outlives the daemon, so a dog
/// in it would come back on a later cold boot as an unmarked sheep, ahead of
/// `spawn_enabled_dogs`, and `shep disable metrics` could not take it out.
/// Filtered here because `record_config` takes bare
/// [`AppConfig`](shep_core::config::AppConfig)s with no marker to filter on.
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
/// A successor is a shep image an outgoing daemon `execve`d in its own place.
/// Its only marker is `SHEP_HANDOVER`, naming the blob to adopt.
///
/// An unusable blob logs at `error` and boots as if fresh: the predecessor has
/// already replaced itself, so refusing leaves the operator no shepherd. A
/// genuinely lost blob is self-limiting, since a real successor also inherited
/// the locked pidfile descriptor and the fresh boot stops at
/// [`BootError::AlreadyRunning`] before restoring anything.
///
/// Unix only: Windows has no `execve`, so no image can be a successor.
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
/// The blob is removed once its descriptors are adopted, and only then: one
/// left after a refusal is evidence, one left after a success would be adopted
/// again by the next boot. No partial success, since the predecessor has
/// already `execve`d itself away.
///
/// # Errors
///
/// - [`BootError::Adopt`] if a descriptor the blob names is not open in this
///   process, or is not the kind of object it was named as.
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
/// The path is kept so [`rehydrate`] can unlink the blob after adopting it.
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
/// Split out so a test can drive every refusal without writing the
/// environment, which is process-global and `unsafe` in edition 2024.
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

/// What, if anything, owns this home's pidfile lock
///
/// Proof of life is the lock, not the pidfile's contents: a stale file with a
/// reused pid can fake those, and the kernel drops the lock on process death,
/// `SIGKILL` included. Any lock this takes is released before it returns.
///
/// # Errors
/// - [`BootError::Io`] if the pidfile could not be opened, created or read.
///   A contended lock is not an error; it is [`Shepherd::Running`] or
///   [`Shepherd::Booting`].
pub fn daemon_liveness(paths: &ShepPaths) -> Result<Shepherd, BootError> {
    match PidfileLock::acquire(paths) {
        // Dropped here rather than at the end of the scope: a question-asker
        // holds someone else's home for as short a window as the type allows.
        Ok(lock) => {
            drop(lock);
            Ok(Shepherd::Absent)
        }
        Err(BootError::AlreadyRunning { pid: Some(pid) }) => Ok(Shepherd::Running(pid)),
        Err(BootError::AlreadyRunning { pid: None }) => Ok(Shepherd::Booting),
        // `init_dirs` makes `pids/` on every boot, so a missing one means no
        // daemon has ever run here: an absence, not a failure. Narrow to
        // `NotFound` under `pids/`; a permissions error is a real failure.
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
/// Three states, not two: [`boot`] takes the lock and records its pid a few
/// statements later, with the socket bind in between, and a caller that read
/// that window as an absence would start a second daemon that then dies unable
/// to take the lock.
///
/// Not `#[non_exhaustive]`, unlike [`BootError`]: the lock is free or held,
/// and a holder has written its pid or has not, so there is no fourth state to
/// add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shepherd {
    /// Nothing holds this home's pidfile lock.
    ///
    /// A stale pidfile naming a long-dead pid reads as this.
    Absent,
    /// A shepherd holds the lock and recorded this pid.
    Running(u32),
    /// A shepherd holds the lock but has not recorded a pid yet.
    ///
    /// It owns the home, so it is not absent, but there is no pid to signal.
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

/// Warns, and does not refuse, when `socket`'s directory is reachable by
/// anyone but its owner. [`init_dirs`] leaves the default layout's `run/` at
/// `0700`, so this fires only for a `[daemon].socket` override pointed
/// somewhere looser.
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
/// - [`BootError::AlreadyRunning`] if a live daemon answered on the socket.
/// - [`BootError::Io`] if bind, probe, or unlink failed.
// The Windows arm's `return` is load-bearing: the `cfg(unix)` block after it
// is the rest of the function, and the unix arm names types Windows lacks.
#[allow(clippy::needless_return)]
pub(crate) fn bind_socket(paths: &ShepPaths, socket: &Path) -> Result<Listener, BootError> {
    // A named pipe is not a file: no `sun_path` limit, no containing directory
    // mode, and nothing left on disk to probe when its owner dies. The kernel
    // enforces the exclusion instead, through `Listener::bind`'s
    // `first_pipe_instance`.
    #[cfg(windows)]
    {
        /// What `first_pipe_instance` reports when the pipe name already has
        /// an owner: another daemon rather than a genuine I/O failure.
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
        // Ahead of the bind, because the kernel's refusal names neither the
        // limit nor `$SHEP_HOME`. `sun_path` holds a NUL terminator, so the
        // usable length is one less.
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
                // EADDRINUSE only says the path exists. Only a refusal is
                // proof of absence: a dying daemon's forked child keeps the
                // socket answering until its close-on-exec clears, so an
                // answer refuses this boot rather than proving a live peer.
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
/// Set by the CLI on the child it re-execs detached, and adopted by that same
/// CLI through `crate::sys::adopt_fd`. shep-daemon never parses it or sees a
/// raw fd, only the adopted [`std::fs::File`] in [`BootOptions::ready_fd`].
pub const READY_FD_ENV: &str = "SHEP_READY_FD";

/// What the daemonizing parent reads off the readiness pipe.
///
/// Crate-private, unlike [`READY_FD_ENV`]: the CLI-side reader deserializes
/// into a struct of its own, so the wire format is the contract rather than
/// this type.
// wire format: shep-cli parses this line; changing it is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonReady {
    /// This daemon's OS pid.
    pub(crate) pid: u32,
    /// This daemon's crate version.
    pub(crate) version: String,
}

/// Writes one newline-terminated JSON readiness line to `pipe` and closes it.
/// Dropping `pipe` here is the parent's EOF.
///
/// # Errors
/// - [`BootError::ReadyWrite`] if the write failed, carrying the OS error.
fn write_ready(mut pipe: std::fs::File, ready: &DaemonReady) -> Result<(), BootError> {
    use std::io::Write;

    // `DaemonReady` is a plain {u32, String} pair, and `to_string` fails only
    // on non-string map keys and NaN floats.
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
    /// The inherited readiness pipe (see [`READY_FD_ENV`]), adopted into an
    /// owned [`std::fs::File`] by the caller.
    ///
    /// Adoption is the caller's job: `crate::sys::adopt_fd`'s precondition,
    /// "call before the process opens any descriptor of its own", is
    /// process-wide, and [`boot`] already runs inside a tokio runtime with
    /// poller fds of its own.
    pub ready_fd: Option<std::fs::File>,
    /// Restore the muster roll if one exists.
    pub restore: bool,
    /// Longest a cron worker parks before re-reading the wall clock, from
    /// `[daemon] max_cron_sleep`. Unset means `DEFAULT_MAX_CRON_SLEEP`,
    /// which [`boot`] applies and nothing else does.
    pub max_cron_sleep: Option<Duration>,
    /// Where to report readiness once the muster restore has finished, for an
    /// init system supervising this process directly. `None` reports nothing.
    ///
    /// The resolved address rather than a bool: `std::env::set_var` is
    /// `unsafe` in edition 2024 and this crate is `#![deny(unsafe_code)]`, so
    /// a boot test could not establish an ambient `$NOTIFY_SOCKET`.
    ///
    /// Distinct from [`Self::ready_fd`], which answers a parent shep process
    /// the moment the socket binds. This one is written last, so a unit goes
    /// green only once the flock is back.
    pub notify_socket: Option<OsString>,
    /// Dogs to start once the flock is back, in the order given.
    ///
    /// Assembled by the caller from `[daemon] enabled_dogs` and
    /// `[daemon] adopted_dogs`, so shep-daemon never reads `shep.toml` itself.
    pub dogs: Vec<DogSpec>,
    /// Every dog name this shepherd may hold a section for, running or
    /// not: the built-in dogs plus every name `[daemon] adopted_dogs`
    /// records, plus whatever `[daemon] enabled_dogs` names.
    ///
    /// Assembled by the caller from the same file [`Self::dogs`] comes out
    /// of, and for the same reason: shep-daemon never reads `shep.toml`
    /// itself.
    ///
    /// A SUPERSET of [`Self::dogs`], and the difference is the whole point
    /// of carrying both. That one is the spawn list, so it holds only the
    /// dogs an operator has switched on; this one holds the dogs that
    /// EXIST. `Request::SetDogConfig` is guarded on this one, because the
    /// dog most in need of configuring is the one that is disabled or has
    /// never started (the dog-config design, decision 4) and a guard on
    /// the running set refuses exactly that dog.
    pub known_dogs: Vec<String>,
    /// Wipe the in-memory flock registry before [`RunningDaemon::run`]'s
    /// teardown writes the final muster roll, so that roll describes an empty
    /// flock however the session ended.
    ///
    /// `true` only for `shep dev`'s isolated session; a real boot needs the
    /// roll to carry the flock's running state for `shep muster`.
    pub delete_flock_on_shutdown: bool,
    /// Let SIGHUP replace this process's image with a successor holding the
    /// same flock, rather than stopping gracefully.
    ///
    /// A handover `execve`s the file this process was launched from, so a
    /// caller that opts in is asserting it is the shep binary; a test harness
    /// would re-run itself from the top forever. Defaults to `false`, and a
    /// boot that has not opted in answers SIGHUP with the graceful stop, as
    /// does a handover that cannot proceed. Unix only in effect.
    pub handover: bool,
}

/// Brings the daemon up: layout, lock, socket, restore, dogs, readiness
///
/// The order is load-bearing: handlers before the socket (SIGUSR2 otherwise
/// terminates), the pidfile lock before the bind it makes race-free,
/// `ready_fd` on the bind not the restore, dogs after the restore so a metrics
/// dog does not answer for an empty flock, [`BootOptions::notify_socket`] last.
///
/// # Errors
/// - [`BootError::Io`] if a boot filesystem or signal-handler step failed.
/// - [`BootError::AlreadyRunning`] if another daemon holds the lock or answered.
/// - [`BootError::ReadyWrite`] if the readiness line could not be written.
/// - [`BootError::Snapshot`] if a roll exists and could not be read or parsed.
pub async fn boot<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    mut options: BootOptions,
) -> Result<RunningDaemon, BootError> {
    // Before anything else: it reads the current directory, which is the
    // startup directory only until something moves it.
    #[cfg(unix)]
    crate::handover::record_launch_path();

    let delete_flock_on_shutdown = options.delete_flock_on_shutdown;

    // 1. Signal handlers, before the socket or anything else observable
    //    exists.
    let (shutdown, shutdown_rx) = watch::channel(false);
    let shutdown = Arc::new(shutdown);
    #[cfg(unix)]
    let (signals, connect_supervisor, connect_handover) =
        install_signals(Arc::clone(&shutdown), paths.clone())?;
    #[cfg(windows)]
    let (signals, connect_supervisor) = install_signals(Arc::clone(&shutdown), paths.clone())?;

    // 2. Layout, then claim $SHEP_HOME before touching the socket: that is
    //    what closes the concurrent-boot race a bare probe-then-recover
    //    sequence cannot. Held for the rest of this daemon's life.
    init_dirs(&paths)?;
    let socket = socket_path(&paths, options.socket.as_deref());
    // A successor takes neither the lock nor the address: it inherited both,
    // still held. Rebinding would race the predecessor's own socket file, and
    // re-locking would mean releasing first.
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

    // 3. Readiness, now that the socket is bound. Taken rather than moved out:
    //    a partial move leaves `options` unborrowable, and step 4 hands it
    //    whole to `max_cron_sleep`.
    if let Some(pipe) = options.ready_fd.take() {
        let ready = DaemonReady {
            pid,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_ready(pipe, &ready)?;
    }

    // 4. Bus, supervisor, muster restore, snapshot writer, context.
    let events = new_bus();
    // Subscribed before the supervisor that emits onto the bus, so it cannot
    // miss an `Errored` a dog reaches during the restore step.
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
    // One `StatsState`, two owners: the extras record the periodic CPU
    // baseline and the RPC layer reads a live sample against it, so a second
    // state would leave one of them on an empty watch set.
    let stats = Arc::clone(&extras.stats);
    let builder = SupervisorBuilder::new(runner, paths.clone(), events.clone()).extras(extras);
    // A successor installs the flock it inherited rather than spawning one:
    // every sheep keeps its pid, id, epoch and history, and nothing here
    // signals, spawns or reopens.
    #[cfg(unix)]
    let mut carried_apps = Vec::new();
    // Not derived from `carried_apps` below: a successor that inherited an
    // empty flock is still a successor.
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
    // The detached reporter and the actor hold each other alive: the reporter
    // holds a `SupervisorHandle`, and its own report senders live as long as
    // the actor's registry. Only `SupervisorHandle::shutdown` (teardown step
    // 4) ends either, so a teardown waiting on sender counts would hang.
    spawn_extras_reporter(breach_rx, live_rx, supervisor.clone());

    // The other half of step 1's SIGUSR2 listener, parked on this since before
    // the socket existed.
    let _ = connect_supervisor.send(supervisor.clone());

    // The other half of step 1's SIGHUP task. It carries the two descriptors a
    // handover blob has to name, which only this function knows: an fd number
    // means nothing outside the owning process.
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

    // A successor rebuilds the registry from the blob and skips the restore.
    // An empty registry would overwrite a good roll within seconds, and a
    // restore would give a flock that never stopped a second copy of every
    // sheep the roll records as running.
    #[cfg(unix)]
    for app in &carried_apps {
        registry.record_config(app);
    }
    #[cfg(windows)]
    let inherited_flock = false;

    if options.restore && !inherited_flock {
        restore_flock(&paths, &registry, &supervisor).await?;
    }

    // Never fails the boot: a dog that cannot be spawned is a monitoring gap
    // rather than an outage, so `spawn_enabled_dogs` warns and carries on.
    crate::dogs::spawn_enabled_dogs(&options.dogs, &paths, &supervisor, &events).await;

    // Starts empty and is not carried across a handover: a successor has
    // refused nobody.
    let dog_refusals = crate::dogs::DogRefusals::new();
    // Also not carried, so a successor must not claim a pid it has never seen
    // never called. `PEER_CONTACT_WARMUP` is what makes starting empty safe:
    // until the map has listened long enough for an absence to mean
    // something it answers `Contact::Unknown` rather than `Contact::None`.
    let peer_contacts = crate::dogs::PeerContacts::new();
    // Spawned at every boot, a successor's included, rather than anchored to a
    // dog's own spawn. It restarts a dog that has been running without ever
    // answering this shepherd; the tradeoff is argued at `record_silent_dog`.
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
        dogs_config: paths.dogs_config.clone(),
        known_dogs: crate::rpc::KnownDogs::new(options.known_dogs.iter().cloned().collect()),
        paths: paths.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        dog_refusals,
        peer_contacts,
        pid,
        shutdown,
        stats,
    };

    // 5. A failure is a `warn!` and the boot continues: only systemd's view
    //    is wrong. Unix only, since `$NOTIFY_SOCKET` is a unix datagram
    //    socket; the field stays on `BootOptions` for both platforms.
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
        // `watch::Sender::send` is a silent no-op at zero receivers, and
        // `ctx.shutdown()` is callable the instant a caller has
        // `Self::context`, ahead of `run` ever being polled.
        shutdown_rx,
        signals,
        pidfile_lock,
        delete_flock_on_shutdown,
    })
}

/// The cron sleep bound this boot runs with: [`BootOptions::max_cron_sleep`],
/// or [`DEFAULT_MAX_CRON_SLEEP`] when `shep.toml` named none.
///
/// The one place that constant is applied: `shep-core` carries the floor and
/// never the default, the daemon carries the default and never the floor.
/// Named, and reading the whole [`BootOptions`], so a test has a seam to stand
/// on; the only behavioural trace is how often a cron worker wakes.
fn max_cron_sleep(options: &BootOptions) -> Duration {
    options.max_cron_sleep.unwrap_or(DEFAULT_MAX_CRON_SLEEP)
}

/// Reads the muster roll (if one exists) and starts every app it restores.
///
/// One line over [`snapshot::muster`], which holds the whole restore rule and
/// also serves an operator's `Muster` request. The names it returns are
/// discarded: nobody is waiting on them here.
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
    // Held rather than detached: `run`'s teardown step 1 aborts both, so
    // nothing rewrites the roll or asks for a dog's restart once serving ends.
    dog_watch: JoinHandle<()>,
    silent_dog_watch: JoinHandle<()>,
    paths: ShepPaths,
    socket: PathBuf,
    // Held from `boot`, not resubscribed in `run`: `watch::Sender::send` is a
    // silent no-op at zero receivers, and `ctx.shutdown()` is callable the
    // moment `boot` returns, so a gap here loses that signal forever.
    shutdown_rx: watch::Receiver<bool>,
    // Kept alive through `run`'s whole serving lifetime; `SignalTasks`'s
    // `Drop` is what stops these tasks.
    signals: SignalTasks,
    // Dropping this `flock` is what lets the next daemon's own
    // `PidfileLock::acquire` succeed.
    pidfile_lock: PidfileLock,
    delete_flock_on_shutdown: bool,
}

impl RunningDaemon {
    /// Handles for driving this daemon from outside its run loop.
    #[must_use]
    pub fn context(&self) -> RpcContext {
        self.ctx.clone()
    }

    /// The control socket this daemon is bound to.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Serves until a signal or `KillDaemon`, then tears down in order
    ///
    /// Every teardown step runs unconditionally: stop the snapshot writer and
    /// both dog watches, write the final muster roll, broadcast
    /// [`BusEvent::DaemonShutdown`] before subscribers' sockets close, run
    /// [`SupervisorHandle::shutdown`]'s kill ladder, then unlink the socket and
    /// the pidfile best-effort. The roll goes before the ladder, and the writer
    /// is stopped before the roll, or the ladder's `Exit`/`Stop` events leave a
    /// roll of stopped sheep for `shep muster` to restore nothing from.
    ///
    /// # Errors
    /// - [`BootError::Io`] if a teardown filesystem step failed.
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
            // Bound, not dropped with `_`: both must outlive the serving
            // lifetime below, and only their `Drop` at the end of this scope
            // stops the signal tasks and releases the home.
            signals: _signals,
            pidfile_lock: _pidfile_lock,
            delete_flock_on_shutdown,
        } = self;

        // The receiver `boot` kept alive, reused rather than a fresh
        // `ctx.shutdown.subscribe()`: no window with zero receivers between
        // `boot` returning and this line running.
        RpcServer::new(listener, ctx.clone())
            .serve(shutdown_rx)
            .await;

        // 1. Nothing may rewrite the roll or ask for a dog's restart from here
        //    on.
        writer.stop().await;
        dog_watch.abort();
        silent_dog_watch.abort();

        // 2. The final roll, written while every sheep is still online, unless
        //    `delete_flock_on_shutdown` (`shep dev`'s case) wiped the registry
        //    first. Runs however serving ended, a caught signal included.
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

        // 5. Both are attempted regardless and the first failure wins, so a
        // socket-unlink error cannot hide a pidfile nothing tried to remove.
        // Unix only: `remove_file` on a `\.\pipe\...` name fails with
        // `ERROR_INVALID_PARAMETER` and would fail every Windows shutdown.
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

/// Removes `path`, treating "already gone" as success: teardown's job is to
/// make sure it is gone, not to prove it was there.
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

/// Live signal-listener tasks [`install_signals`] spawned, held so its
/// [`Drop`] stops them rather than detaching them. Covers an early `?`-return
/// from a later step inside [`boot`], which must not leak a task per boot
/// attempt.
#[derive(Debug)]
struct SignalTasks {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for SignalTasks {
    fn drop(&mut self) {
        // `JoinHandle::drop` detaches rather than stopping.
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Installs SIGTERM/SIGINT/SIGQUIT (graceful shutdown) and SIGUSR2 (reopen)
///
/// SIGUSR2's default disposition is to terminate, so this handler is what
/// keeps a logrotate `postrotate` stanza from killing the daemon; carrying no
/// selector, it reopens [`ProcessSelector::All`]. The returned sender hands
/// that listener its [`SupervisorHandle`], absent until [`boot`]'s step 4;
/// tokio coalesces anything raised in that window into the first `recv()`.
/// Each listener then loops for life, or a second SIGTERM during a slow
/// teardown would have nowhere to go and no default disposition left.
///
/// # Errors
/// - [`BootError::Io`] if the OS refused to register a signal handler.
#[cfg(windows)]
fn install_signals(
    shutdown: Arc<watch::Sender<bool>>,
    paths: ShepPaths,
) -> Result<(SignalTasks, oneshot::Sender<SupervisorHandle>), BootError> {
    use tokio::signal::windows;

    let mut signals = SignalTasks {
        tasks: Vec::with_capacity(4),
    };

    // A macro rather than the unix arm's loop: each console control event is
    // its own tokio type with its own `recv`, so there is nothing to iterate.
    macro_rules! listen {
        ($ctor:path, $name:literal) => {{
            let mut stream = $ctor().map_err(|source| BootError::Io {
                path: paths.home.clone(),
                source,
            })?;
            let shutdown = Arc::clone(&shutdown);
            signals.tasks.push(tokio::spawn(async move {
                // Looped: a single await leaves a second event during a
                // slow teardown with nowhere to go.
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

    // CTRL_CLOSE and CTRL_SHUTDOWN carry a hard OS deadline shorter than any
    // teardown shep can promise: Windows terminates the process about five
    // seconds after the handler returns, so a flock slower than that loses the
    // tail of its kill ladder. Only an SCM service can negotiate longer.
    listen!(windows::ctrl_c, "CTRL_C");
    listen!(windows::ctrl_break, "CTRL_BREAK");
    listen!(windows::ctrl_close, "CTRL_CLOSE");
    listen!(windows::ctrl_shutdown, "CTRL_SHUTDOWN");

    // No SIGUSR2 counterpart: Windows has no user-defined console control
    // event, so the signal-driven log reopen has no trigger. Rotation works
    // anyway through `tokio_runner`'s `open_append`. The channel is created
    // and dropped so the caller wiring is one shape on both platforms.
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
        // An early return drops `signals`, whose `Drop` aborts every task
        // already pushed.
        let mut stream = signal(kind).map_err(|source| BootError::Io {
            path: paths.home.clone(),
            source,
        })?;
        let shutdown = Arc::clone(&shutdown);
        signals.tasks.push(tokio::spawn(async move {
            // `None` means the stream itself closed, leaving this task
            // nothing to listen for.
            let mut already_shutting_down = false;
            while stream.recv().await.is_some() {
                if already_shutting_down {
                    // Observable, but teardown is already unconditional and
                    // already running. `SIGKILL` is the only faster exit,
                    // and no handler here can intercept it.
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

    // SIGHUP is the handover trigger, a signal rather than a request because
    // the case that most needs a reload is a daemon refusing the client at the
    // handshake. Its own task, since it replaces this daemon where the loop
    // above stops it; a refused handover falls back to the graceful stop.
    let mut hup = signal(SignalKind::hangup()).map_err(|source| BootError::Io {
        path: paths.home.clone(),
        source,
    })?;
    let (connect_handover, handover_rx) = oneshot::channel::<Option<HandoverSeam>>();
    let hup_shutdown = Arc::clone(&shutdown);
    signals.tasks.push(tokio::spawn(async move {
        // Parked until `boot` reaches step 4: the descriptors and supervisor a
        // handover needs do not exist yet, and the stream registered above
        // buffers anything raised meanwhile. `None` (not armed) and `Err`
        // (boot never got that far) still answer SIGHUP, whose default kills.
        let seam = handover_rx.await.ok().flatten();
        // `if`, not `while`: at most one SIGHUP. On the success arm there is
        // no image left to loop in, and every other arm is now stopping.
        if hup.recv().await.is_some() {
            let refusal = match &seam {
                Some(seam) => match hand_over_now(seam).await {
                    // No successor image runs this code.
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
        // Parked until `boot` reaches step 4; the wait loses no signal.
        let Ok(supervisor) = supervisor_rx.await else {
            return;
        };
        while usr2.recv().await.is_some() {
            // A rotator that moved the whole log directory gets it back at
            // `DIR_MODE` from the pump's own open (see `open_append`).
            // Recreating it here would be a second owner of that guarantee.
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
                // A signal carries no reply channel, so this log is the
                // whole report.
                Err(err) => tracing::warn!(%err, "SIGUSR2: log reopen failed"),
            }
        }
    }));

    Ok((signals, connect_supervisor, connect_handover))
}

/// What [`install_signals`] hands back: the live listener tasks, and the two
/// senders that connect them to state `boot` has not built yet.
///
/// The SIGUSR2 task needs a [`SupervisorHandle`], the SIGHUP task a
/// [`HandoverSeam`] or the `None` saying this boot did not arm one.
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
/// Handed over a channel rather than as arguments, because none of it exists
/// when [`install_signals`] runs. `Debug` carries nothing sensitive: two
/// descriptor numbers, a mailbox and the home's paths. The blob they end up in
/// does carry each sheep's environment; see [`crate::handover::Handover`].
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
/// The gate runs here as well as in the client that asked before signalling:
/// anyone can send a signal, and the flock can change between the question and
/// the signal.
///
/// # Errors
///
/// The sentence to log, when the flock cannot be carried, when the actor is
/// gone, or when the exec failed. Each leaves this process still itself with
/// no blob on disk, and the caller falls back to a graceful stop.
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
    // Taking the snapshot stopped every pump it reached and nothing else ever
    // sends a resume, so without this the flock logs nothing through the
    // graceful stop the caller falls back to. Here rather than in
    // `exec_into`'s error path: this is where every way out meets.
    parked.resume().await;
    Err(refusal)
}

/// [`hand_over_now`]'s body, split out so a single resume can cover every
/// way it refuses.
///
/// Two gates, in order: whether this flock is a shape a handover carries, then
/// whether the blob describing it is one a successor could adopt, run against
/// duplicates while this image still exists to fall back to.
///
/// # Errors
///
/// The flock cannot be carried, a successor could not have adopted the blob,
/// or the exec failed. All three leave this process still itself, with no blob
/// on disk.
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
    // The gate with no way back if it is skipped: past the `execve` there is
    // no image to refuse to, and the flock runs on unsupervised. Not inside
    // `handover::hand_over`, because the rehearsal registers objects with the
    // tokio reactor and that fn's own self-test runs from a plain `#[test]`.
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
/// Wraps `io::Error` directly rather than stringifying it, so callers keep the
/// OS diagnostic via [`core::error::Error::source`]; that costs this enum
/// `Clone`/`PartialEq`/`Eq`.
///
/// `#[non_exhaustive]`: a future boot step adds a variant rather than
/// overloading [`Self::Io`], whose `path`/`source` shape is specific to the
/// steps that already exist.
#[non_exhaustive]
#[derive(Debug)]
pub enum BootError {
    /// A flock this image inherited across a handover could not be installed.
    ///
    /// A `String` because the two underlying sources are private types in
    /// different modules; what a caller needs is the sentence naming which
    /// sheep is now unsupervised.
    Adopt(String),
    /// A filesystem step failed (carries the path and the OS error)
    ///
    /// No `From<std::io::Error>`, here or on any sibling: `ReadyWrite` wraps
    /// the same type, so one would make a bare `?` in this module pick a
    /// variant rather than report one.
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
    /// `ENAMETOOLONG` names neither the limit nor `$SHEP_HOME`.
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
    /// Only the write. Adoption is the caller's job (see
    /// [`BootOptions::ready_fd`]), so `boot` has no variant for a failed one.
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
            // Refused before any syscall was attempted.
            Self::SocketPathTooLong { .. } => None,
            Self::Snapshot(err) => Some(err),
            Self::ReadyWrite(err) => Some(err),
            // Both underlying types are module-private.
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
/// The `mod tests` below is `#[cfg(unix)]` because almost every case in it
/// asserts something only unix has: a `0700` mode, a `raise(SIGTERM)`, a
/// socket file left behind by a crash. What survives translation is asserted
/// here instead.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn paths_in(dir: &Path) -> ShepPaths {
        ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| dir.to_string_lossy().into_owned()),
            Path::new(""),
        )
    }

    /// No mode to assert on this platform, only that the directories every
    /// later step writes into exist.
    #[test]
    fn init_dirs_creates_the_whole_layout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        init_dirs(&paths).unwrap();
        for expected in [&paths.home, &paths.logs, &paths.pids, &paths.run] {
            assert!(expected.is_dir(), "{} was not created", expected.display());
        }
    }

    /// Also pins why the lock lives on a sibling `.lock`: a `share_mode(0)`
    /// open of the pidfile itself would make `read_pidfile` fail with a
    /// sharing violation, and `AlreadyRunning` would lose its pid.
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

    /// The crash property: dropping stands in for the process dying, which is
    /// what closes the handle the share-mode reservation is owned by.
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

    /// `#[tokio::test]`, unlike its three siblings: creating a named pipe
    /// instance registers it with the tokio reactor, so
    /// `ServerOptions::create` panics outside a runtime context.
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

    /// `flock` conflicts between separate open file descriptions even inside
    /// one process, so this process can ask for the lock it holds and be
    /// refused.
    ///
    /// `mem::forget` plus `sys::adopt_handover_fd` stands in for the `execve`:
    /// forgetting skips the `flock(fd, LOCK_UN)` a drop would run, and
    /// adopting the number gives the descriptor one new owner. Duplicating
    /// instead would leave a second descriptor holding the same lock, and both
    /// assertions would hold whatever the adopted arm did.
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

        // Retried rather than demanded on the first attempt: a `fork` copies
        // the whole descriptor table, so a child another test in this binary
        // spawns concurrently holds a duplicate of this descriptor, and its
        // `flock` with it, until its own `exec` runs.
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

    /// Serializes every test here whose `boot()` call succeeds
    ///
    /// `raise(SIGTERM)` reaches every `Signal` stream in the test binary,
    /// whichever runtime registered it, so two overlapping tests can rescue or
    /// corrupt each other's daemon. A successful `boot()` is the line, not a
    /// `run()`: `install_signals` runs inside `boot`.
    ///
    /// `tokio::sync::Mutex`, since the guard is held across `.await`.
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
        // No `set_permissions` here, so this passes only if `DirBuilder`'s
        // `.mode(DIR_MODE)` lands the mode at creation. `init_dirs`' own tests
        // observe the mode after its chmod pass and cannot see that window.
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
        // The file names a pid; nothing holds the lock, so nothing is live.
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
        // `boot` records its pid a few statements after taking the lock; a
        // caller reading that window as an absence starts a second daemon.
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

    /// The kernel's `ENAMETOOLONG` names neither the limit nor `$SHEP_HOME`,
    /// and the limit is 104 here, 108 on Linux.
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

    /// Fabricates what a crashed daemon leaves behind: a socket file at
    /// `socket` that nothing is listening on. Bind-then-drop is the shape,
    /// since neither std nor tokio unlinks the path, but macOS marks the
    /// descriptor close-on-exec just after `socket(2)` returns, so a child
    /// another test forks in that window holds a duplicate and the path keeps
    /// answering. So this waits until the path refuses a connection; only a
    /// fresh bind can undo that. Real sleeps, not the module's paused clock:
    /// another process's descriptor is on no clock tokio can advance.
    ///
    /// # Panics
    /// If the leftover never goes stale, or the probe fails for any reason
    /// other than nobody listening.
    #[track_caller]
    fn stale_socket_leftover(socket: &Path) {
        // Two loops for two holders: the inner waits out a child that copied
        // the descriptor mid-spawn and drops it on `exec`; the outer
        // re-fabricates for one that forked inside the close-on-exec window
        // and keeps it for life, since unlinking detaches that socket for good.
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
        // The raw unix type, not `shep_core::transport::Listener`: what this
        // proves is that a real socket someone else listens on reads as
        // `AlreadyRunning`, not anything about shep's own wrapper.
        let live = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        write_pidfile(&paths, 4242).unwrap();
        assert!(matches!(
            bind_socket(&paths, &paths.socket),
            Err(BootError::AlreadyRunning { pid: Some(4242) })
        ));
        // A pure probe: the live daemon's socket file is untouched and still
        // answers, so `bind_socket` never reached the remove_file/rebind arm.
        assert!(
            paths.socket.exists(),
            "a live daemon's socket must never be unlinked"
        );
        std::os::unix::net::UnixStream::connect(&paths.socket)
            .expect("the live listener must still be accepting after a refused bind");
        drop(live);
    }

    /// What one racer thread observed in
    /// [`two_concurrent_boots_on_a_stale_socket_exactly_one_wins`]
    ///
    /// Small and `'static` so it crosses the thread boundary without carrying
    /// a `RunningDaemon`, and the tokio resources in it, out of the runtime
    /// that created it.
    #[derive(Debug)]
    enum RaceOutcome {
        Won { socket_still_accepts: bool },
        AlreadyRunning,
        Other(String),
    }

    #[test]
    fn two_concurrent_boots_on_a_stale_socket_exactly_one_wins() {
        // Two daemons racing on a crashed predecessor's leftover both see
        // `ConnectionRefused` and enter `bind_socket`'s recovery arm, where the
        // loser's `remove_file` can delete the winner's fresh listener.
        // Looped: the bad interleaving does not land on every attempt.
        for _ in 0..25 {
            // `blocking_lock` per SIGNAL_TEST_LOCK's rule: this fn is sync.
            let _guard = SIGNAL_TEST_LOCK.blocking_lock();
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(&dir);
            init_dirs(&paths).unwrap();
            // Must really be leftover before the racers start: a socket kept
            // briefly alive by another test's child would make the winner
            // refuse too, for a reason unrelated to this race.
            stale_socket_leftover(&paths.socket);

            // Real OS threads on a barrier, not tokio tasks: `boot`'s
            // synchronous prefix never awaits, so two tasks on one runtime
            // would run one body to completion before the other was scheduled.
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
                                    // Checked inside this racer's own
                                    // runtime: nothing tokio-shaped crosses
                                    // the thread boundary.
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
        // No `unsafe`: `std::io::pipe` hands back an owned `PipeWriter`, which
        // converts into `File` through the standard `OwnedFd` bridge.
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
        // Real time: binds a real socket. Locked per SIGNAL_TEST_LOCK's rule.
        // The only test driving a `Some` `ready_fd` through `boot`. A bad
        // descriptor cannot reach `BootOptions::ready_fd`, whose type is
        // `Option<std::fs::File>`, so `sys::tests` covers that refusal.
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

        // `write_ready` closes its `File`, so this read sees the line and then
        // EOF rather than blocking on a live writer.
        let mut line = String::new();
        reader.read_to_string(&mut line).unwrap();
        let ready: DaemonReady = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(ready.pid, std::process::id());
        assert!(line.ends_with('\n'), "the parent reads a line: {line:?}");

        drop(daemon); // no run() needed; SignalTasks::drop stops the listeners
    }

    #[tokio::test]
    async fn boot_restores_a_saved_flock_and_tears_down_in_order() {
        // Real time: binds a real socket. Locked per SIGNAL_TEST_LOCK's rule.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // A wrong `instances_running` on purpose: restore reads
        // `app.instances`, not this count, so 99 changes nothing about the
        // boot and survives untouched if teardown's roll write is skipped. A
        // seeded 1 would have matched the right value by coincidence.
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

    /// Pins the shared shutdown watch: `ctx.shutdown()` flips the same watch
    /// `install_signals` flips on a caught SIGTERM, with no caller-level
    /// `Stop`/`Delete` first, which is the gap a CLI-side `tidy_up` flag
    /// cannot close. It raises no real signal, so it says nothing about the
    /// listener wiring.
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
        // `install_signals` handler flips this same watch on a real `SIGTERM`.
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

    /// A metrics dog that starts first answers for an empty flock for the
    /// whole restore window, and a bark dog alerts on every restored sheep.
    /// [`ScriptedRunner`] hands out pids as `FIRST_SCRIPTED_PID + index`, so
    /// the spawn order is observable only as a pid order.
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

    /// The dog gets no script, so `ScriptedRunner` answers
    /// `SpawnFailed("script exhausted")` on its spawn and the flock must still
    /// come up.
    ///
    /// `#[test]` with a `block_on` of its own, not `#[tokio::test]`:
    /// `capture_logs` scopes its subscriber to a synchronous closure.
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

    /// `start_dog` is idempotent by name: enabling a dog under a name a sheep
    /// already holds comes back `Ok` over the sheep, not a started dog. The
    /// RPC arm inspects that reply for the missing `dog` marker, and this pins
    /// that `spawn_enabled_dogs` does the same.
    ///
    /// `#[test]` plus `capture_logs` for the reason
    /// `a_dog_that_will_not_start_does_not_fail_the_boot` gives.
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

    /// The ordering `Type=notify` was chosen for: a unit that goes green at
    /// exec time reports a flock that is not up yet, and a hung restore reads
    /// as a healthy service supervising nothing.
    ///
    /// The restore announces its own spawn on the same socket, so what is
    /// asserted is the queue order of two datagrams. Reading only `READY=1`
    /// after `boot` returns would pass on a notify moved to the top of `boot`,
    /// since the kernel keeps that datagram queued however early it was sent.
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

        // The marker sent from inside the restore's own spawn. AF_UNIX
        // SOCK_DGRAM enqueues synchronously and the two sends are sequential,
        // so the queue order is the program order.
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

    /// Nothing is bound at the address, so the send errors and the boot must
    /// still succeed: what failed is the init system's knowledge of a daemon
    /// that is otherwise up, which systemd reports through its own
    /// `TimeoutStartSec`.
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
        // Real time and a real signal, safe to raise only because the handler
        // is installed first: SIGTERM's default action would kill the test
        // binary. The raise is process-wide, hence SIGNAL_TEST_LOCK.
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
        // No sleep: the handlers are installed inside `boot`, which this call
        // already awaited, so they are live before `run()` is ever polled.
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
        // SIGHUP's default disposition is to terminate, and this handler
        // replaces it. SIGHUP is the handover trigger, but a boot that has not
        // set `BootOptions::handover` has no successor to become, and must
        // still walk SIGTERM's graceful path rather than drop the flock's pipes.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // `handover` left `false`, or `exec_target` would replace the test
        // binary with a fresh copy of itself.
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

    /// The daemon-side gate: anyone can send a signal, and the flock can
    /// change between a client's question and the delivery, so the SIGHUP path
    /// runs [`crate::handover::fitness`] again and refuses on its own.
    ///
    /// The descriptors are invalid on purpose: if the gate stopped refusing,
    /// `hand_over` would meet `EBADF` and return rather than exec this test
    /// binary into a re-run of the suite. The assertion on the message is what
    /// tells the two failures apart.
    #[tokio::test(start_paused = true)]
    async fn a_sighup_over_a_flock_it_cannot_carry_refuses_before_it_execs() {
        // A successful `boot()` installs signal listeners for this test's
        // whole duration, and a `raise()` elsewhere reaches them, hence the
        // lock. The paused clock is separate: it keeps the refusal this needs,
        // a pump missing `REPORT_DEADLINE`, from costing two real seconds.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // A wedged log pump: the one thing the gate still refuses. The case is
        // about the gate firing at all, not about what fires it.
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
        // Bounded: the lock is held across this await, so a hung teardown
        // would stop every other signal test rather than failing this one.
        tokio::time::timeout(Duration::from_secs(5), daemon.run())
            .await
            .unwrap()
            .unwrap();
    }

    /// The second gate: a flock the fitness check passes, described by a blob
    /// no successor could have adopted, refuses here rather than after the
    /// `execve`. Past the exec there is no predecessor to refuse to, so
    /// `rehydrate` returns [`BootError::Adopt`], the successor exits without
    /// serving, and the flock runs on unsupervised.
    ///
    /// The assertion is on which refusal, not on there being one: the
    /// `FD_CLOEXEC` sweep meeting `EBADF` refused these descriptors before the
    /// gate existed. They stay invalid on purpose, since a blob past both
    /// gates would exec this test binary into a re-run of the suite.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sighup_over_a_blob_no_successor_could_adopt_refuses_before_it_execs() {
        // A successful `boot()` installs real signal listeners for this
        // test's whole duration.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        // No dog and no sheep, so the first gate passes: an empty flock is
        // carryable, and this case is about the second gate.
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

    /// A handover that reports and then refuses has to leave every pump
    /// reading again.
    ///
    /// Taking the snapshot stops each pump where it stands, and nothing else
    /// in the daemon ever sends a resume, so a missing one is a flock that
    /// logs nothing more for the rest of the daemon's life.
    ///
    /// Two sheep, since the resume has to reach every pump that was reported
    /// to rather than the one the refusal named.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_handover_starts_every_pump_reading_again() {
        // A successful `boot()` installs real signal listeners for this test's
        // whole duration, and the paused clock keeps the missed
        // `REPORT_DEADLINE` this refusal needs from costing real seconds.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        let runner = Arc::new(
            ScriptedRunner::new(vec![ProcScript::never_exits(); 3])
                .with_a_pump_that_never_reports(&["wedged"]),
        );
        // The refusal: a wedged log pump, read after every pump that answered
        // has been reported to and parked, which is what makes a resume owed.
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
        // Polled: `LogCtl::Resume` carries no acknowledgement, so a send that
        // returned was queued rather than served. Bounded, so a pump that is
        // never told fails here instead of hanging.
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
        // A resume that reached only the first sheep reported to would satisfy
        // the single-pump sibling and fail here.
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

    /// A snapshot abandoned because a pump went quiet still owes a resume to
    /// every pump that answered. A missed deadline is a different way into the
    /// refusal than the sibling's config gate, and it arrives with one pump
    /// parked and one that never was.
    ///
    /// No `boot()` and no signal: this is about what `hand_over_now` does with
    /// a snapshot, and building the supervisor directly is what lets the clock
    /// be paused so the deadline costs nothing to wait out.
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

        // Invalid on purpose: if the gate stopped refusing, `hand_over` would
        // meet `EBADF` and return rather than exec this test binary into a
        // re-run of the suite.
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
        // Polled: `LogCtl::Resume` carries no acknowledgement, so a send that
        // returned was queued rather than served. Bounded, so a pump that is
        // never told fails here instead of hanging.
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
        // Each listener `install_signals` spawns stays armed for the process's
        // life instead of returning after one `recv()`. Drives
        // `install_signals` directly: the loop is the whole subject. Real time
        // and real signals, and the raise is process-wide, hence the lock.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let shutdown = Arc::new(shutdown);
        // The SIGUSR2 and SIGHUP senders are dropped unused: that ends the
        // task parked on each receiver without disturbing the three below.
        let (signals, _connect_supervisor, _connect_handover) =
            install_signals(shutdown, paths).unwrap();

        // First SIGTERM: starts shutdown, exactly as before this decision.
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), shutdown_rx.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*shutdown_rx.borrow());

        // Looped, the listener never finishes on its own; only
        // `SignalTasks::drop`'s `abort()` stops it.
        assert!(
            !signals.tasks[0].is_finished(),
            "the SIGTERM listener must still be polling after its first signal, not have exited"
        );

        // A second SIGTERM into the already-shutting-down state a slow
        // teardown would be in. `watch::Sender::send` marks its channel
        // changed on every call whether or not the value differs, so a second
        // `changed()` on a value already `true` proves the loop delivered it.
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

    // `boot` is the one place `DEFAULT_MAX_CRON_SLEEP` is applied: the CLI
    // keeps the knob an `Option` all the way down, so nothing else here would
    // notice a different fallback. Whole `BootOptions` values rather than bare
    // `Option`s, since that is what `boot` reads.
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

    // The only case driving `boot`'s own spawn of the extras reporter, over
    // the whole production chain: the actor arms the liveness loop at Online,
    // the loop reports over `Extras::real`'s sender, the reporter reads it,
    // and `extra_restart` lets it through. Real time, and a real `OsProber`.
    #[tokio::test]
    async fn a_booted_daemon_restarts_a_sheep_whose_liveness_probe_fails() {
        // Real time: binds a real socket, so it takes SIGNAL_TEST_LOCK.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        // Reserve a port, then release it: nothing listens there, so every
        // probe fails with a connection refusal and there is no race.
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

    // The only case driving the seam where `boot` hands the SIGUSR2 listener
    // its supervisor; `shep reopen` reaches the same supervisor over the
    // socket, so the RPC tier stays green with the signal path dead. Both
    // instances are asserted, since `ProcessSelector::All` is the claim.
    #[tokio::test]
    async fn sigusr2_reopens_every_sheeps_log_files() {
        // Real time and a real signal, raised process-wide, hence the lock.
        // Safe only because `boot` below has already replaced SIGUSR2's
        // default disposition, which would kill the test binary.
        let _guard = SIGNAL_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        // Two scripts for two instances: `ScriptedRunner` answers
        // `SpawnFailed("script exhausted")` once it runs out, landing that
        // sheep `Errored` with no pump, which this case could not tell apart
        // from a pump nobody reopened. `log_ctl_live` below is the other half.
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

        // Polled: a signal has no reply channel, so the counters are the only
        // place a reopen becomes visible. Bounded, so a listener that never
        // reaches a pump fails here instead of hanging.
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
            pending: None,
            pending_reidentifies: false,
            overridden: Vec::new(),
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

    /// A successor rebuilds its registry from the blob, and the roll on disk
    /// is written from that registry within seconds. `spawn_enabled_dogs`
    /// never touches `FlockRegistry`, so a successor that recorded a dog would
    /// put one in the roll permanently: a later cold boot restores `metrics`
    /// as an unmarked sheep ahead of `spawn_enabled_dogs`, and `shep disable
    /// metrics` cannot take it out.
    ///
    /// Both rows are asserted, since a filter that dropped everything would
    /// satisfy the negative half and overwrite a good roll with an empty one.
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
