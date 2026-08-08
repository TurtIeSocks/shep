//! Daemon boot: layout, pidfile, and control-socket bind
//!
//! Everything a daemon needs before it can accept its first connection:
//! creating (and tightening) `$SHEP_HOME`'s directory layout, recording its
//! own pid, and binding the control socket — including recovering from a
//! socket file a crashed daemon left behind. This module owns the 0700
//! guarantee [`crate::server::RpcServer`]'s doc names as the boot path's
//! responsibility: [`init_dirs`] creates `run/` (and every other layout
//! directory) `0700` and *tightens* it back down if it already exists
//! looser, so the guarantee holds whether this is a first boot or a restart
//! onto a directory some other process touched.
//!
//! `unsafe`-free: bind/probe/unlink are all safe std and tokio APIs. The
//! readiness pipe (the phase's one `unsafe`, adopting an inherited
//! descriptor) is a later addition to this module, in its own `sys.rs`.

use core::fmt;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use tokio::net::UnixListener;

use shep_core::paths::ShepPaths;

/// Mode for every directory shep creates (spec §10: no other user, at all)
pub const DIR_MODE: u32 = 0o700;

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
pub fn init_dirs(paths: &ShepPaths) -> Result<(), BootError> {
    for dir in [&paths.home, &paths.logs, &paths.pids, &paths.run] {
        create_dir_at_dir_mode(dir).map_err(|source| BootError::Io {
            path: dir.clone(),
            source,
        })?;
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

/// Writes the pidfile atomically (temp file in `pids/`, `fsync`, then
/// `rename`), matching [`crate::snapshot::write_atomic`]'s convention for
/// every other file this daemon writes. [`crate::snapshot::write_atomic`]
/// itself isn't reusable here — it's typed to serialize a
/// [`crate::snapshot::FlockSnapshot`] as JSON, not a bare pid — so this
/// inlines the same temp+rename shape rather than writing straight to the
/// final path with `std::fs::write` and risking a reader observing a
/// truncated file mid-write.
///
/// # Errors
/// - [`BootError::Io`] — the pidfile could not be written.
pub fn write_pidfile(paths: &ShepPaths, pid: u32) -> Result<(), BootError> {
    use std::io::Write;

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
pub fn read_pidfile(paths: &ShepPaths) -> Result<Option<u32>, BootError> {
    let path = pidfile(paths);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BootError::Io { path, source }),
    }
}

/// The socket this daemon binds: the layout default, or a config override
#[must_use]
pub fn socket_path(paths: &ShepPaths, override_path: Option<&Path>) -> PathBuf {
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
pub fn bind_socket(paths: &ShepPaths, socket: &Path) -> Result<UnixListener, BootError> {
    warn_if_socket_dir_is_loose(socket);
    match UnixListener::bind(socket) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            // EADDRINUSE only says the path exists. Probe it: a live daemon's
            // listener accepts at the kernel level even mid-accept, while a
            // file left behind by a crash (or a reboot) refuses. This is the
            // load-bearing step for the reboot-resurrect scenario (§13.4).
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
                    UnixListener::bind(socket).map_err(|source| BootError::Io {
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

/// Error type returned from this module's boot steps
///
/// Wraps `io::Error` directly rather than stringifying it (contrast
/// [`shep_core::protocol::WireError`]) so callers keep the underlying OS
/// diagnostic via [`core::error::Error::source`]; that costs this enum
/// `Clone`/`PartialEq`/`Eq` (IR-19's documented exception for variants
/// wrapping `io::Error`).
///
/// Grows two more variants (`Snapshot`, `Ready`) once the readiness pipe and
/// the assembled `boot()` entry point land — this task only wires the steps
/// that can fail with an I/O error or find a live daemon already listening.
#[derive(Debug)]
pub enum BootError {
    /// A filesystem step failed (carries the path and the OS error)
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
        }
    }
}

impl core::error::Error for BootError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyRunning { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_paths; // the one crate-root fixture (IR-33)
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

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
    fn socket_path_honors_a_config_override() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        assert_eq!(socket_path(&paths, None), paths.socket);
        let custom = dir.path().join("custom.sock");
        assert_eq!(socket_path(&paths, Some(&custom)), custom);
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

    #[tokio::test]
    async fn a_socket_left_by_a_crash_is_unlinked_and_rebound() {
        // Neither std nor tokio unlinks a UnixListener's path on drop, so this
        // is exactly the file a killed daemon leaves behind.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        init_dirs(&paths).unwrap();
        drop(UnixListener::bind(&paths.socket).unwrap());
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
        let live = UnixListener::bind(&paths.socket).unwrap();
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
}
