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
//! [`crate::sys`]; [`boot`] calls into it as the very FIRST thing it does,
//! before opening any descriptor of its own — see [`sys::adopt_fd`]'s
//! `# Safety` section for why that ordering is load-bearing, not cosmetic
//! (an earlier version of this module got it wrong and adopted a recycled
//! fd in production-reachable conditions).

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
use tokio::task::JoinHandle;

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

/// Writes one newline-terminated JSON readiness line to `pipe` and closes
/// it — dropping `pipe` at the end of this call is the parent's own EOF
/// signal that there is nothing more to read.
///
/// Takes an already-[`sys::adopt_fd`]-ed [`std::fs::File`], never a raw
/// [`RawFd`]: adoption is a fd-inheritance concern (`sys.rs`'s rationale
/// essay) and must run before `boot` opens anything of its own; the write
/// itself is plain safe IO with no such ordering constraint, and keeping it
/// a separate step is what lets `boot` adopt first and write later without
/// this function re-deciding when adoption is safe.
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
        .map_err(BootError::ReadyWrite)
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

/// Brings the daemon up: readiness pipe, signal handlers, layout, roll
/// restore, bus, supervisor, socket.
///
/// Step order here is deliberate and load-bearing, not incidental:
/// 1. adopt the readiness descriptor (if any) — see [`sys::adopt_fd`]'s
///    `# Safety` section for why this must be the very first fd-touching
///    step, before anything below opens (or closes) a descriptor of its
///    own;
/// 2. install signal handlers — before the socket exists, so there is no
///    window where the socket is already live but an ordinary `kill -USR2`
///    (SIGUSR2's default disposition is to terminate) would still kill the
///    daemon instead of rotating logs;
/// 3. layout, socket bind, pidfile;
/// 4. report readiness, now that the socket is actually bound (spec §3) —
///    not once the whole flock is restored, so a slow muster can't make the
///    parent think boot failed;
/// 5. bus, supervisor, muster restore, snapshot writer, `RpcContext`.
///
/// # Errors
/// - [`BootError::Ready`] — `options.ready_fd` was set but the descriptor
///   could not be adopted.
/// - [`BootError::Io`] — a boot filesystem step failed, or the OS refused
///   to register a signal handler.
/// - [`BootError::AlreadyRunning`] — another daemon owns this `$SHEP_HOME`.
/// - [`BootError::ReadyWrite`] — the descriptor was adopted but the
///   readiness line could not be written.
/// - [`BootError::Snapshot`] — `options.restore` was set, a roll exists, but
///   it could not be read or parsed.
pub async fn boot<R: ProcessRunner>(
    runner: R,
    paths: ShepPaths,
    options: BootOptions,
) -> Result<RunningDaemon, BootError> {
    // 1. Adopt FIRST — before this process opens or closes anything of its
    //    own. See this fn's own doc and sys.rs's rationale essay.
    #[allow(unsafe_code)] // IR-24 escape hatch, exercised here — see sys.rs's essay.
    let ready_pipe = options
        .ready_fd
        .map(|fd| {
            // SAFETY: this is the first fd-touching statement in `boot` —
            // nothing in this process has opened or closed a descriptor of
            // its own yet, so `fd` cannot alias one. `adopt_fd`'s own
            // checks (>= 3, `F_GETFD`) rule out the remaining hazards (see
            // its `# Safety` section).
            unsafe { sys::adopt_fd(fd) }
        })
        .transpose()
        .map_err(BootError::Ready)?;

    // 2. Signal handlers next, before the socket (or anything else
    //    observable) exists — see this fn's own doc.
    let (shutdown, shutdown_rx) = watch::channel(false);
    let shutdown = Arc::new(shutdown);
    let signals = install_signals(Arc::clone(&shutdown), paths.clone())?;

    // 3. Layout, socket, pidfile.
    init_dirs(&paths)?;
    let socket = socket_path(&paths, options.socket.as_deref());
    let listener = bind_socket(&paths, &socket)?;
    let pid = std::process::id();
    write_pidfile(&paths, pid)?;

    // 4. Report readiness now that the socket is bound.
    if let Some(pipe) = ready_pipe {
        let ready = DaemonReady {
            pid,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_ready(pipe, &ready)?;
    }

    // 5. Bus, supervisor, muster restore, snapshot writer, context.
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

    let ctx = RpcContext {
        supervisor,
        events,
        registry,
        snapshot_path: paths.snapshot.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        pid,
        shutdown,
    };

    Ok(RunningDaemon {
        ctx,
        listener,
        writer,
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
    // Installed by `boot` (not `run` — see `boot`'s own doc for why: SIGUSR2
    // must be handled before the socket exists), kept alive through `run`'s
    // whole serving lifetime, and dropped only once teardown finishes —
    // `SignalTasks`'s own `Drop` is what actually stops these tasks.
    signals: SignalTasks,
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
    /// (`signal_ready`'s registration, the one thing that used to be able
    /// to `?`-exit `run` before any of this ran, now happens inside `boot`
    /// instead, before any of these are created — see `boot`'s own doc.)
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
            // Kept alive (not `_`) until this fn returns: `signals` must
            // outlive the whole serving lifetime below, and only its `Drop`
            // (at the end of this scope) stops its tasks. The underscore
            // prefix suppresses the "unused" warning for a binding that
            // exists purely for its drop side effect.
            signals: _signals,
        } = self;

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

        // 5. Unlink what boot created — both attempted regardless, the
        // first failure (if any) wins so a socket-unlink error can't hide
        // a pidfile that was never even attempted.
        let unlink_socket = unlink_if_present(&socket);
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
/// happened before this type existed (a bare `Arc<AtomicU64>` return value
/// with the actual `JoinHandle`s discarded at the spawn site).
#[derive(Debug)]
struct SignalTasks {
    // SIGUSR2 reopen requests observed since boot. Write-only today by
    // design, not by oversight: Phase 4's `flush`/`reopen` work is the
    // reader this is waiting for. `#[allow(dead_code)]` says so explicitly
    // rather than inventing an accessor nothing calls yet.
    #[allow(dead_code)]
    reopens: Arc<AtomicU64>,
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
) -> Result<SignalTasks, BootError> {
    let mut signals = SignalTasks {
        reopens: Arc::new(AtomicU64::new(0)),
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
            stream.recv().await; // first signal only — the daemon is exiting either way
            let _ = shutdown.send(true);
        }));
    }

    let mut usr2 = signal(SignalKind::user_defined2()).map_err(|source| BootError::Io {
        path: paths.home.clone(),
        source,
    })?;
    let task_reopens = Arc::clone(&signals.reopens);
    let logs = paths.logs.clone();
    signals.tasks.push(tokio::spawn(async move {
        while usr2.recv().await.is_some() {
            task_reopens.fetch_add(1, Ordering::SeqCst);
            if let Err(err) = create_dir_at_dir_mode(&logs) {
                tracing::warn!(%err, path = %logs.display(), "SIGUSR2: could not recreate log dir");
            }
            tracing::info!(
                "SIGUSR2 received: log reopen requested (full per-sheep reopening lands in Phase 4)"
            );
        }
    }));

    Ok(signals)
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
    /// The readiness descriptor could not be adopted (carries the reason —
    /// see [`SysError`]); the descriptor is untouched, nothing was written
    ///
    /// Kept distinct from [`Self::ReadyWrite`] on purpose: this is a
    /// `sys.rs`-layer failure (fd-adoption, `sys::adopt_fd`'s own concern),
    /// while `ReadyWrite` is a plain IO failure writing to an already-valid
    /// `File` — conflating the two into one `SysError::ReadyWrite` variant
    /// (an earlier version of this enum did) made `sys.rs` responsible for
    /// an error it never produces, since only `boot.rs`'s own `write_ready`
    /// ever constructs it.
    Ready(SysError),
    /// The readiness descriptor was adopted but writing the readiness line
    /// to it failed (carries the OS error)
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
            Self::Ready(err) => write!(f, "readiness descriptor could not be adopted: {err}"),
            Self::ReadyWrite(err) => write!(f, "writing the readiness line failed: {err}"),
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
            Self::ReadyWrite(err) => Some(err),
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

    /// Serializes every test in this module that calls `daemon.run()`
    /// against every OTHER such test, because `nix::sys::signal::raise` (and
    /// the real OS signal delivery it triggers) is NOT scoped to one test's
    /// own tokio runtime: every `Signal` stream registered anywhere in this
    /// test BINARY'S process receives it, regardless of which test spawned
    /// it or which runtime owns it.
    ///
    /// Proven load-bearing, not just theoretical (Opus review, 2026-08-08):
    /// reintroducing the fixed watch-receiver bug on purpose (dropping the
    /// initial `shutdown_rx` again) still made `boot::tests` PASS under the
    /// default PARALLEL test runner — `sigterm_triggers_the_same_graceful_shutdown`'s
    /// `raise(SIGTERM)` accidentally reached `boot_restores_a_saved_flock_and_tears_down_in_order`'s
    /// OWN daemon (hung on the reintroduced bug) too, on the SAME signal
    /// delivery, and rescued it — masking the regression. Only
    /// `--test-threads=1` (or, now, this lock) exposed it. Every test below
    /// that calls `daemon.run()` takes this for its own duration so the two
    /// can never overlap and one can never rescue (or corrupt) the other.
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
        #[allow(unsafe_code)] // IR-24 escape hatch, exercised here — see sys.rs's essay.
        // SAFETY: test-only socketpair fd, nothing else in this process has
        // opened or closed anything since it was created — the exact
        // invariant `boot()`'s own call site upholds structurally by being
        // its first fd-touching statement (see sys.rs's essay).
        let pipe = unsafe { sys::adopt_fd(child.into_raw_fd()) }.unwrap();
        write_ready(pipe, &ready).unwrap();
        let mut line = String::new();
        let mut parent = parent;
        parent.read_to_string(&mut line).unwrap();
        assert_eq!(line.trim_end(), serde_json::to_string(&ready).unwrap());
        assert!(line.ends_with('\n'), "the parent reads a line: {line:?}");
    }

    // Deliberately NOT tested with a real forced fd collision (tried, then
    // removed — Opus review follow-up, 2026-08-08): a test that frees a real
    // fd number and hands that same number to `boot()` a moment later has
    // the exact "close, then act again on that learned number" shape that
    // FD_REUSE_LOCK exists for (see its own doc), but locking THIS test
    // against sys.rs's only protects it from THOSE two tests — it does
    // nothing against the dozens of OTHER tests across this crate
    // (server.rs, supervisor.rs, boot.rs's own other socket tests, ...)
    // that independently open and close real descriptors on their own
    // threads at the same time. Empirically, adding this one extra
    // fd-churning test measurably raised the whole suite's collision odds
    // enough to crash an UNRELATED test
    // (`server::tests::a_garbage_frame_ends_the_connection_without_panicking`)
    // with the identical `SIGABRT` (`IO Safety violation: owned file
    // descriptor already closed`) within single-digit parallel runs — 40/40
    // clean runs on the commit before this test existed, a crash within the
    // first handful after. Locking the WHOLE crate's fd-touching tests
    // against each other to make this one test safe would be a wildly
    // disproportionate change for what it buys: the adoption-ordering fix
    // itself is verified by inspection (`sys::adopt_fd` is the literal first
    // statement in `boot`, see its own doc), by the rationale essay's
    // scenario (c), and by `adopt_fd` now being `unsafe fn` — a future
    // reorder needs its own fresh `unsafe` block and SAFETY justification
    // at the new call site, not a silent move. `sys::tests`' own two
    // fd-touching tests keep FD_REUSE_LOCK: that's a strict improvement
    // over the prior state (no lock at all) for a self-contained pair, not
    // an attempt to close this broader, pre-existing, whole-suite risk.

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
}
