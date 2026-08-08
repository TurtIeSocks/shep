//! Daemon boot: layout, pidfile, control-socket bind, and the run/teardown
//! sequence
//!
//! Everything a daemon needs before it can accept its first connection:
//! creating (and tightening) `$SHEP_HOME`'s directory layout, recording its
//! own pid, binding the control socket — including recovering from a socket
//! file a crashed daemon left behind — and, once bound, reporting readiness
//! on an inherited pipe if the CLI daemonized us (spec §3). This module owns
//! the 0700 guarantee [`crate::server::RpcServer`]'s doc names as the boot
//! path's responsibility: [`init_dirs`] creates `run/` (and every other
//! layout directory) `0700` and *tightens* it back down if it already
//! exists looser, so the guarantee holds whether this is a first boot or a
//! restart onto a directory some other process touched.
//!
//! [`boot`] assembles every piece earlier tasks built (bus, supervisor,
//! muster roll, RPC context) into one [`RunningDaemon`]; [`RunningDaemon::run`]
//! serves connections until a signal or `KillDaemon` arrives, then tears
//! down in a load-bearing order — see its own doc for why.
//!
//! `unsafe`-free itself: bind/probe/unlink/signal-registration are all safe
//! std and tokio APIs. The one `unsafe` in this phase lives in
//! [`crate::sys`], which [`signal_ready`] calls into to adopt the readiness
//! pipe's inherited descriptor.

use core::fmt;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use shep_core::paths::ShepPaths;
use shep_core::protocol::BusEvent;

use crate::bus::new_bus;
use crate::rpc::RpcContext;
use crate::runner::ProcessRunner;
use crate::server::RpcServer;
use crate::snapshot::{self, FlockRegistry, SnapshotError, SnapshotWriter, spawn_snapshot_writer};
use crate::supervisor::{SupervisorHandle, spawn_supervisor};
use crate::sys::{self, SysError};

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

/// Environment variable naming the inherited readiness descriptor.
///
/// Set by the CLI on the child it re-execs detached (spec §3); read back
/// into [`BootOptions::ready_fd`] by that same CLI, not by anything in this
/// crate — shep-daemon only ever sees the already-parsed `RawFd`.
pub const READY_FD_ENV: &str = "SHEP_READY_FD";

/// What the daemonizing parent reads off the readiness pipe.
// wire format: shep-cli parses this line; changing it is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonReady {
    /// This daemon's OS pid.
    pub pid: u32,
    /// This daemon's crate version.
    pub version: String,
}

/// Reports readiness to the parent and closes the pipe.
///
/// Adopts `fd` (see [`sys::adopt_fd`]) and writes one newline-terminated
/// JSON line; dropping the adopted [`std::fs::File`] at the end of this
/// call closes the descriptor, which is the parent's own EOF signal that
/// there is nothing more to read.
///
/// # Errors
/// - [`BootError::Ready`] — the descriptor could not be adopted or written.
pub fn signal_ready(fd: RawFd, ready: &DaemonReady) -> Result<(), BootError> {
    use std::io::Write;

    let mut pipe = sys::adopt_fd(fd).map_err(BootError::Ready)?;
    // DaemonReady is a plain {u32, String} pair: serde_json::to_string only
    // fails on things neither field can ever be (non-string map keys, NaN
    // floats), so this can't error in practice. SysError::ReadyWrite below
    // is for a real IO failure, not this.
    let mut line = serde_json::to_string(ready).expect("DaemonReady always serializes");
    line.push('\n');
    pipe.write_all(line.as_bytes())
        .map_err(|source| BootError::Ready(SysError::ReadyWrite(source.to_string())))?;
    Ok(())
}

/// Options the CLI hands the daemon at boot.
#[derive(Debug, Default)]
pub struct BootOptions {
    /// Overrides the layout's default control-socket path.
    pub socket: Option<PathBuf>,
    /// The inherited readiness-pipe descriptor, if the CLI daemonized us
    /// (see [`READY_FD_ENV`]).
    pub ready_fd: Option<RawFd>,
    /// Restore the muster roll if one exists (spec §9's `shep muster`).
    pub restore: bool,
}

/// Brings the daemon up: layout, roll restore, bus, supervisor, socket.
///
/// # Errors
/// - [`BootError::AlreadyRunning`] — another daemon owns this `$SHEP_HOME`.
/// - [`BootError::Io`] — a boot filesystem step failed.
/// - [`BootError::Ready`] — `options.ready_fd` was set but the descriptor
///   could not be adopted or written.
/// - [`BootError::Snapshot`] — `options.restore` was set, a roll exists, but
///   it could not be read or parsed.
pub async fn boot<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    options: BootOptions,
) -> Result<RunningDaemon, BootError> {
    init_dirs(&paths)?;
    let socket = socket_path(&paths, options.socket.as_deref());
    let listener = bind_socket(&paths, &socket)?;
    let pid = std::process::id();
    write_pidfile(&paths, pid)?;

    // The spec's contract (sys.rs's rationale essay): report readiness once
    // the socket is bound, not once the whole flock is restored — a slow
    // muster must not make the parent think the daemon failed to start.
    if let Some(fd) = options.ready_fd {
        let ready = DaemonReady {
            pid,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        signal_ready(fd, &ready)?;
    }

    let events = new_bus();
    let supervisor = spawn_supervisor(runner, paths.clone(), events.clone());
    let registry = FlockRegistry::new();

    if options.restore {
        restore_flock(&paths, &registry, &supervisor).await?;
    }

    let writer = spawn_snapshot_writer(
        paths.snapshot.clone(),
        supervisor.clone(),
        registry.clone(),
        events.subscribe(),
    );

    // `shutdown_rx` is kept (not dropped) all the way into `RunningDaemon` —
    // see that field's own doc for why letting the receiver count hit zero
    // here would make `ctx.shutdown()` a silent no-op.
    let (shutdown, shutdown_rx) = watch::channel(false);
    let ctx = RpcContext {
        supervisor,
        events,
        registry,
        snapshot_path: paths.snapshot.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        pid,
        shutdown: Arc::new(shutdown),
    };

    Ok(RunningDaemon {
        ctx,
        listener,
        writer,
        paths,
        socket,
        shutdown_rx,
    })
}

/// Reads the muster roll (if one exists) and starts every app it restores.
///
/// A missing roll is not restore's problem to report — a fresh `$SHEP_HOME`
/// has none, and that's just a first boot, not a [`BootError`]. A roll that
/// exists but fails to parse IS reported: something already corrupted or
/// hand-edited it, and silently booting an empty flock instead would hide
/// that from the operator.
async fn restore_flock(
    paths: &ShepPaths,
    registry: &FlockRegistry,
    supervisor: &SupervisorHandle,
) -> Result<(), BootError> {
    let saved = match snapshot::read(&paths.snapshot) {
        Ok(saved) => saved,
        Err(SnapshotError::Io(err)) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(BootError::Snapshot(err)),
    };
    let restorable = snapshot::restorable(saved);
    for (name, err) in &restorable.rejected {
        tracing::warn!(name, %err, "muster roll entry rejected on restore");
    }
    if restorable.apps.is_empty() {
        return Ok(());
    }
    // Recorded regardless of whether `start` below fully succeeds, matching
    // `rpc::run`'s own Start handler: already-registered entries must
    // persist even when a later spawn in the same batch fails.
    registry.record(&restorable.apps);
    if let Err(err) = supervisor.start(restorable.apps).await {
        // A restore spawn failure must not sink the whole boot — the same
        // "one bad entry doesn't sink the muster" policy `restorable`
        // already applies at validation time. The sheep that failed to
        // spawn is already recorded `Errored` by the supervisor itself.
        tracing::warn!(%err, "muster roll restore failed to spawn one or more apps");
    }
    Ok(())
}

/// A booted daemon, not yet serving: everything [`boot`] assembled, handed
/// back so the caller can read [`Self::context`] before driving [`Self::run`].
#[derive(Debug)]
pub struct RunningDaemon {
    ctx: RpcContext,
    listener: UnixListener,
    writer: SnapshotWriter,
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

    /// Serves until a signal or `KillDaemon`, then tears down in order.
    ///
    /// TEARDOWN ORDER IS LOAD-BEARING:
    /// 1. stop the snapshot writer — nothing may rewrite the roll from here on;
    /// 2. write the final muster roll — records the flock AS IT WAS, still running;
    /// 3. broadcast [`BusEvent::DaemonShutdown`] — subscribers learn before their sockets close;
    /// 4. [`SupervisorHandle::shutdown`] — the kill ladder on every online sheep;
    /// 5. unlink the socket, remove the pidfile.
    ///
    /// Steps 1-2 before 4 are the whole point: run them the other way round
    /// and the roll records a flock of stopped sheep, and `shep muster`
    /// after a reboot restores nothing — silently breaking spec §13.4, the
    /// flagship migration scenario. Step 1 specifically must precede step 4
    /// (not just step 2): the writer would otherwise still be alive to
    /// observe the kill ladder's own `Exit`/`Stop` events and overwrite the
    /// roll step 2 just wrote.
    ///
    /// # Errors
    /// - [`BootError::Io`] — a teardown filesystem step failed.
    pub async fn run(self) -> Result<(), BootError> {
        let RunningDaemon {
            ctx,
            listener,
            writer,
            paths,
            socket,
            shutdown_rx,
        } = self;

        // Installed here, not in `boot`, so its tasks start landing right
        // around when this future is first polled — see
        // `sigterm_triggers_the_same_graceful_shutdown`'s own comment for
        // why that timing matters to that test.
        let _reopens = install_signals(Arc::clone(&ctx.shutdown), paths.clone())?;

        // `shutdown_rx` is the receiver `boot` has kept alive since the
        // watch channel was created (see the field's own doc) — reused
        // here rather than a fresh `ctx.shutdown.subscribe()` precisely so
        // there is never a window with zero receivers between `boot`
        // returning and this line running.
        RpcServer::new(listener, ctx.clone())
            .serve(shutdown_rx)
            .await;

        // 1. Stop the snapshot writer FIRST — see this fn's doc.
        writer.stop().await;

        // 2. Write the final roll while every sheep is still online.
        if let Err(err) = ctx.snapshot_now().await {
            tracing::warn!(%err, "final muster roll write failed");
        }

        // 3. Tell subscribers before their sockets close underneath them.
        let _ = ctx.events.send(BusEvent::DaemonShutdown);

        // 4. Kill ladder on every online sheep.
        ctx.supervisor.shutdown().await;

        // 5. Unlink what boot created.
        unlink_if_present(&socket)?;
        unlink_if_present(&pidfile(&paths))?;

        Ok(())
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

/// Installs SIGTERM/SIGINT/SIGQUIT (graceful shutdown, first signal wins)
/// and SIGUSR2 (`shep reopen`'s out-of-band form, spec §9).
///
/// SIGUSR2's DEFAULT disposition is to terminate the process, so installing
/// this handler is load-bearing on its own: without it, an operator's
/// `kill -USR2` (or `shep reopen`) kills the daemon instead of rotating
/// logs. Full per-sheep handle reopening lands with `flush`/`reopen` (Phase
/// 4); today this only re-creates a missing log dir, counts the request,
/// and logs it.
///
/// # Errors
/// - [`BootError::Io`] — the OS refused to register a signal handler.
fn install_signals(
    shutdown: Arc<watch::Sender<bool>>,
    paths: ShepPaths,
) -> Result<Arc<AtomicU64>, BootError> {
    for kind in [
        SignalKind::terminate(),
        SignalKind::interrupt(),
        SignalKind::quit(),
    ] {
        let mut stream = signal(kind).map_err(|source| BootError::Io {
            path: paths.home.clone(),
            source,
        })?;
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            stream.recv().await; // first signal only — the daemon is exiting either way
            let _ = shutdown.send(true);
        });
    }

    let reopens = Arc::new(AtomicU64::new(0));
    let mut usr2 = signal(SignalKind::user_defined2()).map_err(|source| BootError::Io {
        path: paths.home.clone(),
        source,
    })?;
    let task_reopens = Arc::clone(&reopens);
    let logs = paths.logs.clone();
    tokio::spawn(async move {
        while usr2.recv().await.is_some() {
            task_reopens.fetch_add(1, Ordering::SeqCst);
            if let Err(err) = create_dir_at_dir_mode(&logs) {
                tracing::warn!(%err, path = %logs.display(), "SIGUSR2: could not recreate log dir");
            }
            tracing::info!(
                "SIGUSR2 received: log reopen requested (full per-sheep reopening lands in Phase 4)"
            );
        }
    });

    Ok(reopens)
}

/// Error type returned from this module's boot steps
///
/// Wraps `io::Error` directly rather than stringifying it (contrast
/// [`shep_core::protocol::WireError`]) so callers keep the underlying OS
/// diagnostic via [`core::error::Error::source`]; that costs this enum
/// `Clone`/`PartialEq`/`Eq` (IR-19's documented exception for variants
/// wrapping `io::Error`).
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
    /// The muster roll exists but could not be read or parsed on restore
    Snapshot(SnapshotError),
    /// The readiness descriptor could not be adopted or written
    Ready(SysError),
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
            Self::Ready(err) => write!(f, "readiness signal failed: {err}"),
        }
    }
}

impl core::error::Error for BootError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyRunning { .. } => None,
            Self::Snapshot(err) => Some(err),
            Self::Ready(err) => Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::snapshot::{FlockSnapshot, SNAPSHOT_VERSION, SavedApp};
    use crate::testing::test_paths; // the one crate-root fixture (IR-33)
    use shep_core::config::AppConfig;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

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

    #[test]
    fn readiness_reports_pid_and_version_then_closes_the_pipe() {
        use std::io::Read;
        use std::os::unix::io::IntoRawFd;
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        let ready = DaemonReady {
            pid: 4242,
            version: "0.1.0".to_string(),
        };
        signal_ready(child.into_raw_fd(), &ready).unwrap();
        let mut line = String::new();
        let mut parent = parent;
        parent.read_to_string(&mut line).unwrap();
        assert_eq!(line.trim_end(), serde_json::to_string(&ready).unwrap());
        assert!(line.ends_with('\n'), "the parent reads a line: {line:?}");
    }

    #[tokio::test]
    async fn boot_restores_a_saved_flock_and_tears_down_in_order() {
        // Real time: binds a real socket.
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

    #[tokio::test]
    async fn sigterm_triggers_the_same_graceful_shutdown() {
        // Real time + a real signal. Safe to raise here only because the
        // handler is installed first: SIGTERM's default action would kill
        // the test binary. tokio never uninstalls it, which is harmless.
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
        tokio::time::sleep(Duration::from_millis(50)).await; // let install_signals land
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!paths.socket.exists());
    }
}
