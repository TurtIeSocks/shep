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
//! **No unsafe in this module.** [`BootOptions::ready_fd`] is
//! `Option<`[`std::fs::File`]`>`, not a raw descriptor: the CALLER adopts
//! the inherited readiness pipe (`unsafe fn` [`crate::sys::adopt_fd`],
//! IR-22's sole unsafe surface) before ever constructing a [`BootOptions`],
//! so [`boot`] only ever receives an already-owned handle and never
//! constructs one from a bare number itself. Every bind/probe/unlink/
//! signal-registration step in this module is plain safe std/tokio.
//!
//! (An earlier revision had `boot` perform that adoption inline, behind an
//! `unsafe` block at its own call site — see git history around commits
//! db02d9f/5c4f29b for that design, and f688ac2/9455d80 for the doc fallout
//! it caused. It moved here because `adopt_fd`'s ordering precondition is
//! process-wide — "call before THIS PROCESS opens any descriptor" — and
//! `boot` cannot discharge that on its own caller's behalf: `boot` is
//! `async`, so a tokio runtime with its own live poller fds already exists
//! by the time `boot` is ever called. The fix pushes adoption out to
//! somewhere that CAN discharge the precondition: the CLI's `main`, as its
//! literal first fd-touching statement, before a tokio runtime — or
//! anything else — exists (Phase 3). See [`crate::sys::adopt_fd`]'s own
//! `# Safety` section and rationale essay for the full contract.)

use core::fmt;
use core::time::Duration;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use shep_core::paths::ShepPaths;
use shep_core::protocol::BusEvent;

use crate::bus::new_bus;
use crate::cron::DEFAULT_MAX_CRON_SLEEP;
use crate::extras::{Extras, ExtrasReports, spawn_extras_reporter};
use crate::rpc::RpcContext;
use crate::runner::ProcessRunner;
use crate::server::RpcServer;
use crate::snapshot::{self, FlockRegistry, SnapshotError, SnapshotWriter, spawn_snapshot_writer};
use crate::supervisor::{SupervisorBuilder, SupervisorHandle};

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
pub fn read_pidfile(paths: &ShepPaths) -> Result<Option<u32>, BootError> {
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
#[derive(Debug)]
struct PidfileLock(nix::fcntl::Flock<std::fs::File>);

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
            Ok(lock) => Ok(Self(lock)),
            Err((_file, nix::errno::Errno::EWOULDBLOCK)) => Err(BootError::AlreadyRunning {
                pid: read_pidfile(paths)?,
            }),
            Err((_file, errno)) => Err(BootError::Io {
                path,
                source: errno.into(),
            }),
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
    fn record(&mut self, paths: &ShepPaths, pid: u32) -> Result<(), BootError> {
        use std::io::{Seek, SeekFrom, Write};

        let path = pidfile(paths);
        let file = &mut *self.0;
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
/// Set by the CLI on the child it re-execs detached (spec §3); read back and
/// adopted (`unsafe fn` [`crate::sys::adopt_fd`]) by that same CLI, not by
/// anything in this crate — shep-daemon never parses this variable or sees
/// a raw fd number itself, only the already-adopted [`std::fs::File`] that
/// lands in [`BootOptions::ready_fd`].
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
/// Takes an already-adopted [`std::fs::File`], never a raw fd: adoption
/// (`unsafe fn` [`crate::sys::adopt_fd`]) is a fd-inheritance concern
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
        .map_err(BootError::ReadyWrite)
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
    /// Adoption (`unsafe fn` [`crate::sys::adopt_fd`]) is deliberately not
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
///    [`bind_socket`] ever runs, and it is what makes that call race-free
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
/// 4. bus, supervisor, muster restore, snapshot writer, `RpcContext`.
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
    options: BootOptions,
) -> Result<RunningDaemon, BootError> {
    // 1. Install signal handlers before the socket (or anything else
    //    observable) exists — see this fn's own doc.
    let (shutdown, shutdown_rx) = watch::channel(false);
    let shutdown = Arc::new(shutdown);
    let signals = install_signals(Arc::clone(&shutdown), paths.clone())?;

    // 2. Layout, then claim exclusive ownership of $SHEP_HOME BEFORE
    //    touching the socket at all — see `PidfileLock`'s own doc for why
    //    this is what actually closes the concurrent-boot race a bare
    //    probe-then-recover sequence can't. Held across the whole
    //    bind-and-recover sequence, and for the rest of this daemon's life
    //    (kept in `RunningDaemon`, dropped only at the end of `run`).
    init_dirs(&paths)?;
    let mut pidfile_lock = PidfileLock::acquire(&paths)?;
    let socket = socket_path(&paths, options.socket.as_deref());
    let listener = bind_socket(&paths, &socket)?;
    let pid = std::process::id();
    pidfile_lock.record(&paths, pid)?;

    // 3. Report readiness now that the socket is bound. `options.ready_fd`
    //    is already an owned File adopted by the caller — see this fn's
    //    own doc and `BootOptions::ready_fd`'s doc — so this is nothing
    //    more than a write.
    if let Some(pipe) = options.ready_fd {
        let ready = DaemonReady {
            pid,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_ready(pipe, &ready)?;
    }

    // 4. Bus, supervisor, muster restore, snapshot writer, context.
    let events = new_bus();
    let (breach_tx, breach_rx) = mpsc::channel(EXTRAS_REPORT_CAPACITY);
    let (live_tx, live_rx) = mpsc::channel(EXTRAS_REPORT_CAPACITY);
    let extras = Extras::real(
        ExtrasReports {
            breaches: breach_tx,
            liveness: live_tx,
        },
        // The one place `DEFAULT_MAX_CRON_SLEEP` is applied — see its own doc
        // for why a second application anywhere else is how two supposedly
        // identical constants drift apart.
        options.max_cron_sleep.unwrap_or(DEFAULT_MAX_CRON_SLEEP),
    );
    let supervisor = SupervisorBuilder::new(runner, paths.clone(), events.clone())
        .extras(extras)
        .spawn();
    // Ordered, not stylistic: the reporter needs the handle the builder
    // returns, and the actor must never own a receiver a subsystem feeds.
    spawn_extras_reporter(breach_rx, live_rx, supervisor.clone());
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
        // Held from here into `RunningDaemon` and through the whole of
        // `run`: this is what keeps `$SHEP_HOME` exclusively claimed for
        // this daemon's entire life, not just its boot — dropping it (at
        // the end of `run`, or on an early `?`-return from a LATER step in
        // THIS function) is the only thing that releases the `flock`. See
        // `PidfileLock`'s own doc.
        pidfile_lock,
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
    // Acquired by `boot` before the socket was ever bound, kept alive
    // through `run`'s whole serving lifetime, and dropped only once
    // teardown finishes — releasing this `flock` is what lets a NEXT
    // daemon's own `PidfileLock::acquire` succeed. See that type's own doc.
    pidfile_lock: PidfileLock,
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
    /// (`install_signals`'s registration used to run here, in `run`, where
    /// its own failure could `?`-exit before any of this teardown had a
    /// chance to run at all; it now happens inside `boot` instead, before
    /// any of the state teardown depends on is even created — see `boot`'s
    /// own doc.)
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
            // Same reasoning as `signals` above: this `flock` must stay
            // held for the whole serving lifetime below, released only by
            // its `Drop` at the end of this scope — that release is what
            // lets a future daemon's own boot succeed.
            pidfile_lock: _pidfile_lock,
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

/// Installs SIGTERM/SIGINT/SIGQUIT (graceful shutdown, first signal starts
/// it — see below for what a repeat does) and SIGUSR2 (`shep reopen`'s
/// out-of-band form, spec §9).
///
/// SIGUSR2's DEFAULT disposition is to terminate the process, so installing
/// this handler is load-bearing on its own: without it, an operator's
/// `kill -USR2` (or `shep reopen`) kills the daemon instead of rotating
/// logs. Full per-sheep handle reopening lands with `flush`/`reopen` (Phase
/// 4); today this only re-creates a missing log dir, counts the request,
/// and logs it.
///
/// **Each listener stays armed for the rest of the process's life
/// (Decision 3, 2026-08-08).** A signal handler, once installed, is
/// installed for good — `tokio` never uninstalls the underlying libc
/// disposition just because the [`tokio::signal::unix::Signal`] stream
/// polling it happens to stop. An earlier version of this function's
/// SIGTERM/SIGINT/SIGQUIT loop awaited exactly one signal and returned,
/// which left a real gap: a SECOND SIGTERM arriving during a slow
/// [`RunningDaemon::run`] teardown (the kill ladder waiting out
/// `kill_timeout` on a stuck sheep, say) had nowhere left to go — not
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
    /// Writing the readiness line to the caller-adopted readiness pipe
    /// failed (carries the OS error)
    ///
    /// Adoption itself (`unsafe fn` [`crate::sys::adopt_fd`]) is the
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
    /// Proven concretely, not just reasoned about (Opus review follow-up,
    /// 2026-08-08): a `boot`-only test with no lock passed 10/10 runs in
    /// isolation and FAILED 10/10 runs alongside
    /// `sigterm_triggers_the_same_graceful_shutdown` — the exact same
    /// process-wide `raise(SIGTERM)` hazard, just reached through `boot`
    /// alone rather than through `run`. Task 10's e2e `Fixture` is the next
    /// `boot()` caller this crate will grow, so this is a real, standing
    /// tripwire, not a one-off.
    ///
    /// Proven load-bearing on the shutdown-signal front too (Opus review,
    /// 2026-08-08): reintroducing the fixed watch-receiver bug on purpose
    /// (dropping the initial `shutdown_rx` again) still made `boot::tests`
    /// PASS under the default PARALLEL test runner —
    /// `sigterm_triggers_the_same_graceful_shutdown`'s `raise(SIGTERM)`
    /// accidentally reached `boot_restores_a_saved_flock_and_tears_down_in_order`'s
    /// OWN daemon (hung on the reintroduced bug) too, on the SAME signal
    /// delivery, and rescued it — masking the regression. Only
    /// `--test-threads=1` (or, now, this lock) exposed it. Every test below
    /// that calls `boot()` takes this for its own duration so no two such
    /// tests can ever overlap and one can never rescue (or corrupt) another.
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
    /// lying fixture, and it is what made
    /// [`two_concurrent_boots_on_a_stale_socket_exactly_one_wins`] fail as
    /// `[AlreadyRunning, AlreadyRunning]`, both racers refused, roughly 1 run
    /// in 28 of this crate's suite under a saturated machine: the flock's
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
        // Decision 1 (2026-08-08) replaced `boot_refuses_a_stale_ready_fd_
        // before_touching_anything_else`, which lived here and drove a bad
        // fd number straight through `BootOptions` to pin that adoption was
        // refused before `bind_socket` ran. That guard is no longer
        // expressible through `boot`'s public API at all: `BootOptions::
        // ready_fd` is `Option<std::fs::File>` now, and there is no safe
        // way to hand this test a `File` that names a bad descriptor — the
        // type itself is the proof the fd was valid at construction time.
        // The BadFd-refusal behavior itself still exists and is still
        // tested, just one layer down: see `sys::tests::
        // a_fd_this_process_never_owned_is_refused`, which calls
        // `sys::adopt_fd` directly (that module's own job now). What THIS
        // test covers instead — real coverage that would otherwise be lost
        // entirely, since no other test drives a `Some` `ready_fd` through
        // `boot` at all — is the happy path: a caller-adopted pipe really
        // does receive the readiness line, and only after the socket is
        // genuinely bound (spec §3), exactly as `boot`'s own doc claims.
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
        let signals = install_signals(shutdown, paths).unwrap();

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
}
