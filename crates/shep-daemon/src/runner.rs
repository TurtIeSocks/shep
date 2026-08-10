//! Spawn seam between the daemon engine and the OS
//!
//! [`ProcessRunner`] spawns a child; the [`RunningProcess`] it returns owns
//! that one live child for its whole lifetime (pid, wait, signal, kill).
//! Spawn also hands back a [`ProcIo`] bundle of channels so the owning sheep
//! task can pump stdout/stderr lines and shepherd-channel messages without
//! the runner itself blocking on delivery.
//!
//! Two implementations exist: a deterministic scripted fake (this crate's
//! test-only `fake` module, `ScriptedRunner`) for engine tests, and a real
//! runner over OS processes (a later task) for production.
//!
//! It also owns the log plane's shared vocabulary — [`LogCtl`] and the two
//! errors its requests answer with — and, with it, [`open_log_path`]: the
//! one function in this crate that opens a sheep's log file, for the log
//! pump and for `shep flush` alike, so neither half can drift from the other
//! on what it is willing to open.
//!
//! This whole module is public and stays that way: [`ProcessRunner`] is the
//! bound on [`boot`](crate::boot::boot), which `shep-cli` calls, so a caller
//! outside this crate has to be able to name it — and naming it drags in every
//! type in its signature. `tests/real_runner.rs` drives the same surface
//! directly against real children.

use core::fmt;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use crate::channel::{ChildMessage, ShepherdMessage};
use crate::privilege::Credentials;

/// One exit observation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitOutcome {
    /// Exit code on normal exit
    pub code: Option<i32>,
    /// Raw unix signal number when killed (`SIGTERM`=15, `SIGKILL`=9, ...)
    pub signal: Option<i32>,
}

/// Typed stop signal
///
/// [`StopSignal::as_raw`] gives the unix number so fake and real runners
/// record identical [`ExitOutcome`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// `SIGTERM` — graceful stop request
    Term,
    /// `SIGINT` — interrupt
    Int,
    /// `SIGQUIT` — quit, core-dumping by default
    Quit,
    /// `SIGUSR2` — user-defined signal 2
    Usr2,
    /// `SIGKILL` — unblockable, immediate
    Kill,
}

impl StopSignal {
    /// The raw unix signal number
    #[must_use]
    pub fn as_raw(self) -> i32 {
        match self {
            Self::Term => 15,
            Self::Int => 2,
            Self::Quit => 3,
            Self::Usr2 => 12,
            Self::Kill => 9,
        }
    }
}

/// One stdout/stderr line from a child
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// True = stderr, false = stdout
    pub err: bool,
    /// The line, no trailing newline
    pub line: String,
}

/// What the supervisor can ask a log pump to do mid-flight.
///
/// # Why a pushed message rather than a generation counter
///
/// A counter the pump re-read before each write would only ever promise
/// "before the next line", and **a quiet sheep never writes a next line** —
/// so a caller could ask for a rotation, or for the pending writes to land,
/// and be told nothing about when or whether the daemon caught up. The
/// [`oneshot`] on each variant below is the whole point of the shape: once it
/// resolves, every live pump has provably done the thing, which is what makes
/// either variant usable as a barrier. A logrotate `postrotate` stanza needs
/// that of [`Self::Reopen`] before it compresses or deletes the file it
/// renamed; `shep flush` needs it of [`Self::Flush`] before it truncates.
#[derive(Debug)]
pub enum LogCtl {
    /// Drop the current handle and open the path again, then acknowledge.
    /// Sent when an external rotator has renamed the file.
    Reopen {
        /// Fires once the pump has finished acting on this request, carrying
        /// what came of it.
        ///
        /// `Ok` says both old handles were flushed and closed AND both paths
        /// were opened again — everything a rotator needs before it
        /// compresses or deletes what it renamed, and everything the sheep
        /// needs to keep logging. [`ReopenError`] says at least one path
        /// could not be opened: the old handle is closed either way, so the
        /// rename is safe to act on, but that stream now has no file at all
        /// and its lines are dropped until something reopens it. Answering
        /// `Ok` there would leave a sheep logging into nothing with nobody
        /// told, which is the failure a reopen exists to end.
        ///
        /// # When it never fires
        ///
        /// A pump that ends between accepting a request and serving it drops
        /// this sender, and the caller's `await` resolves
        /// [`Err`](oneshot::error::RecvError). The channel buffers several
        /// requests, so a send that succeeded is not a request that will be
        /// served: every way a pump ends — both streams reaching EOF, the
        /// `logs` receiver going away, or the last control sender dropping —
        /// retires it with whatever is still queued. Treat that error as the
        /// same stopped-sheep no-op a failed send means (see
        /// [`ProcIo::log_ctl`]); the two describe one situation observed a
        /// moment apart, and neither is a reopen that failed.
        done: oneshot::Sender<Result<(), ReopenError>>,
    },
    /// Wait for every write already handed to the blocking pool to reach the
    /// file, keeping the handle, then acknowledge. Sent as the first half of
    /// `shep flush`, immediately before the recorded paths are truncated.
    Flush {
        /// Fires once both handles have no write left in flight, carrying
        /// what came of it.
        ///
        /// The acknowledgement is the barrier the truncate that follows is
        /// ordered against: `write_all` on a [`tokio::fs::File`] returns as
        /// soon as the real `write(2)` is queued, so without waiting here a
        /// line already dispatched can land at offset 0 of the file *after*
        /// it was emptied — the one line that survives a flush, in the file
        /// the operator was told is now empty.
        ///
        /// [`FlushError`] says at least one stream's owed bytes never
        /// reached its file. It does not hold up the truncate that follows,
        /// and cannot: `poll_flush` drives the write already in flight to
        /// completion either way, so bytes reported here are bytes that
        /// errored rather than bytes still racing anything. What it changes
        /// is the answer the operator gets — that sheep could not write its
        /// log, which is worth a non-zero exit even though the file does end
        /// up empty. `LogFile::reopen` logs its own flush failure and moves
        /// on instead, because the handle it belongs to is about to be
        /// replaced by a working one and the sheep keeps logging either way.
        ///
        /// # When it never fires
        ///
        /// Exactly as [`Self::Reopen`]'s: a pump that ends between accepting
        /// a request and serving it drops this sender, and the caller's
        /// `await` resolves [`Err`](oneshot::error::RecvError). Treat that as
        /// the same stopped-sheep no-op a failed send means — a pump that is
        /// gone owes no bytes to anything.
        done: oneshot::Sender<Result<(), FlushError>>,
    },
}

/// A [`LogCtl::Reopen`] that could not open one or both of a sheep's log
/// files again.
///
/// Carries a rendered message rather than the `io::Error`s behind it, the
/// same way [`RunnerError`] does: it crosses a channel, ends up in an RPC
/// error message, and every layer between only ever prints it. Keeping it a
/// `String` is also what keeps this `Clone`/`Eq`, which `io::Error` is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReopenError {
    /// Every log file the reopen could not open again, as
    /// `"<path>: <what the open reported>"`, joined by `", "` when both
    /// streams failed. Never empty: a reopen that opened both files answers
    /// `Ok`.
    ///
    /// `", "` and not `"; "` because this list gets nested inside another
    /// one: [`SupervisorError::ReopenFailed`] joins one of these per sheep
    /// with `"; "`, and a single separator at both levels would punctuate one
    /// sheep that failed on both streams exactly like two sheep that failed
    /// on one each.
    ///
    /// [`SupervisorError::ReopenFailed`]: crate::supervisor::SupervisorError::ReopenFailed
    pub message: String,
}

impl fmt::Display for ReopenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not reopen {}", self.message)
    }
}

impl core::error::Error for ReopenError {}

/// A `shep flush` that could not empty a log file, from either of the two
/// halves that verb is made of: a pump whose pending writes would not reach
/// the file ([`LogCtl::Flush`]), or a path that could not be truncated once
/// they had.
///
/// One type for both halves because an operator is owed one answer about one
/// file, and because either half failing costs them the same non-zero exit.
/// What it leaves behind differs, and this type says nothing about which: a
/// truncate that failed leaves its file as it was, while a flush that failed
/// does not hold up the truncate — the bytes it reports are bytes that
/// errored, not bytes still in flight — so that file does end up empty and
/// the lines it held are gone unwritten. Carries a rendered message for the
/// reasons [`ReopenError`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushError {
    /// Every log file the flush could not empty, as
    /// `"<path>: <what the failing call reported>"`, joined by `", "` when
    /// both of a pump's streams did. Never empty: a flush that emptied every
    /// file answers `Ok`.
    ///
    /// `", "` for the reason [`ReopenError::message`] gives about its own
    /// separator: [`SupervisorError::FlushFailed`] joins one of these per
    /// failing path with `"; "`, and one separator at both levels would make
    /// the nesting unreadable.
    ///
    /// Keyed by path and never by sheep, unlike [`SupervisorError`]'s
    /// reopen failures: several sheep can share one log path (`merge_logs`,
    /// or an explicit `out_file` on a multi-instance app), so naming one of
    /// them would be arbitrary and naming all of them would repeat the path
    /// the reader actually needs.
    ///
    /// [`SupervisorError`]: crate::supervisor::SupervisorError
    /// [`SupervisorError::FlushFailed`]: crate::supervisor::SupervisorError::FlushFailed
    pub message: String,
}

impl fmt::Display for FlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not flush {}", self.message)
    }
}

impl core::error::Error for FlushError {}

/// What an operator is told when a log path turns out to be a symlink.
///
/// One owner for the sentence, cited by [`open_log_path`]'s doc and by both
/// openers' tests: an operator who legitimately keeps `/var/log/app` as a
/// symlink to another filesystem reads this, not a bare `ELOOP`, and the
/// remedy is in the sentence itself. The path is NOT in here — every caller
/// already prefixes one (`"<path>: <what the open reported>"`, see
/// [`ReopenError::message`] and [`FlushError::message`]), and repeating it
/// would print it twice.
pub(crate) const SYMLINK_REFUSED: &str = "refusing to follow a symlink at this log path; shep \
     opens log files with O_NOFOLLOW, so point out_file/err_file at the real file";

/// Opens `path` through `options`, refusing to follow a symlink standing at
/// the path itself.
///
/// The one opener of a log file in this crate, in both directions: the log
/// pump's append handle (`tokio_runner`'s `open_append`) and `shep flush`'s
/// truncating one (`supervisor`'s `truncate_log`) both come through here, so
/// the two halves of the log plane cannot drift on what they will open.
///
/// # Security
///
/// An app's `out_file`/`err_file` are free-form config the assembler takes
/// verbatim, so a log path can name anywhere the daemon can write. Under a
/// root daemon that turns a pre-existing loose directory into a
/// write-and-truncate primitive: another local user plants a symlink where
/// the log file will be, the pump appends through it, and `shep flush`
/// empties whatever it points at. `O_NOFOLLOW` closes that: the open fails
/// with `ELOOP` instead, the symlink is left alone, and its target is
/// untouched.
///
/// What it does NOT cover, stated plainly because the gap is real:
/// `O_NOFOLLOW` guards only the FINAL path component. A symlinked *parent*
/// directory still resolves, so `logs -> /elsewhere` followed by
/// `logs/app.log` reaches `/elsewhere/app.log` exactly as before. Closing
/// that needs `openat2(RESOLVE_NO_SYMLINKS)`, which is Linux-only and so out
/// of scope for a project with macOS as a tier-1 platform (spec §11). This
/// stops the realistic attack — a symlink planted at the log path — not
/// every theoretical one.
///
/// `custom_flags` is safe, so none of this is the exception to
/// `shep-daemon/src/sys.rs` owning every `unsafe` block in this crate
/// (IR-22).
///
/// # Errors
///
/// Whatever the open reported. An `ELOOP` — the refusal above — is relabelled
/// to [`SYMLINK_REFUSED`] on the way out, because `ELOOP`'s own strerror
/// ("too many levels of symbolic links") reads as a symlink *loop* and tells
/// an operator with one perfectly ordinary symlink neither what shep did nor
/// what to change. Every other error is passed through untouched, `NotFound`
/// included — `truncate_log` still recognises it.
pub(crate) async fn open_log_path(
    options: &mut tokio::fs::OpenOptions,
    path: &Path,
) -> io::Result<tokio::fs::File> {
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW);
    options.open(path).await.map_err(name_the_symlink)
}

/// Relabels the `ELOOP` an `O_NOFOLLOW` open answers with, leaving every
/// other error exactly as the OS reported it.
///
/// Both platforms shep supports report the refusal the same way — POSIX
/// specifies `ELOOP`, and Darwin's `open(2)` matches it (measured, not
/// assumed) — so one errno covers the tier rather than a per-platform list.
/// The kind is carried over rather than invented: only the message changes.
#[cfg(unix)]
fn name_the_symlink(error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(nix::libc::ELOOP) {
        io::Error::new(error.kind(), SYMLINK_REFUSED)
    } else {
        error
    }
}

/// The non-unix arm of [`name_the_symlink`]: there is no `O_NOFOLLOW` to
/// apply on this tier, so there is no refusal to relabel either.
#[cfg(not(unix))]
fn name_the_symlink(error: io::Error) -> io::Error {
    error
}

/// IO endpoints handed back by spawn — the runner pumps internally.
///
/// The sheep task owns this and MUST drain every receiver: an undrained
/// `from_child` back-pressures a metric-emitting child until it stalls on
/// its own fd-3 write (see the supervisor's `select!` model, a later task).
#[derive(Debug)]
pub struct ProcIo {
    /// stdout+stderr lines
    pub logs: mpsc::Receiver<LogLine>,
    /// Parsed child→daemon shepherd-channel messages
    pub from_child: mpsc::Receiver<ChildMessage>,
    /// daemon→child shepherd-channel sender
    pub to_child: mpsc::Sender<ShepherdMessage>,
    /// Control channel into this sheep's log pump
    ///
    /// The pump is the only reader of the child's stdout and stderr, and it
    /// ends when the last of these senders drops — so hold this for as long
    /// as the child is alive. Ending the pump drops the read ends of those
    /// two pipes along with it, and the child's next write to either then
    /// gets `EPIPE`/`SIGPIPE`: the child typically dies on the spot rather
    /// than stalling on a pipe nobody drains.
    ///
    /// Cloning it is therefore not free of consequence, and the supervisor
    /// does clone it (`SheepSlot::log_ctl`, so a `Reopen` or a `Flush` can
    /// reach a pump without going through the sheep task). What keeps a clone
    /// from
    /// stretching a pump's life is the pump's own exit on the `logs`
    /// receiver going away — see `tokio_runner`'s `spawn_log_pump` — which
    /// the owner of this bundle drops when it drops the bundle.
    ///
    /// A send that fails means the pump is already gone (its `logs` receiver
    /// dropped, or both streams reached EOF — normally the child exiting),
    /// which makes a reopen a no-op rather than an error.
    /// [`LogCtl::Reopen`]'s acknowledgement resolving `Err` says the same
    /// thing about a request that was accepted and then never served.
    pub log_ctl: mpsc::Sender<LogCtl>,
}

/// A live child.
pub trait RunningProcess: Send + 'static {
    /// The OS process id
    fn pid(&self) -> u32;

    /// Resolves exactly once with the exit outcome
    ///
    /// # Cancellation safety
    ///
    /// Dropping the returned future and calling `wait` again is safe: it
    /// neither restarts the wait nor loses whatever progress was already
    /// made toward it. [`tokio::process::Child::wait`] documents this
    /// guarantee for real children; the scripted fake mirrors it by fixing
    /// its exit deadline once, at spawn time, rather than recomputing it on
    /// each `wait` call.
    ///
    /// The future is also explicitly `Send` (RPITIT) because the sheep task
    /// that owns the proc is `tokio::spawn`'ed.
    fn wait(&mut self) -> impl core::future::Future<Output = ExitOutcome> + Send;

    /// Sends a signal to the sheep's whole process group
    ///
    /// Group-wide, not leader-only. A sheep that forks a child without
    /// `exec`ing it — the shape every `thing & wait` wrapper script produces —
    /// keeps that child in its own process group, so a leader-only signal
    /// stops the wrapper and leaves the child running, orphaned and untracked.
    /// Signalling the group instead delivers the graceful stop to the lambs
    /// too, and gives them the same chance to shut down cleanly that the
    /// sheep gets, rather than only ever meeting [`Self::kill_tree`]'s
    /// `SIGKILL`.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] — delivery failed (already reaped, `EPERM`).
    ///
    /// # Process-group assumption
    ///
    /// Implementors must spawn each child as the leader of a fresh process
    /// group of its own; this method and [`Self::kill_tree`] both address
    /// that group by the pid [`Self::pid`] reports. A child that escapes its
    /// group after the fact (the `setsid`-in-a-fork daemonize dance) is
    /// beyond the reach of either. The real runner's own `signal_group`
    /// (`tokio_runner.rs`, unix-only, so deliberately not linked from this
    /// portable tier) documents how it establishes the property and what an
    /// implementation that dropped it would do instead.
    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError>;

    /// SIGKILLs the whole process group/tree
    ///
    /// The escalation rung above [`Self::signal`]: same group, same
    /// process-group assumption (documented there), but a signal nothing can
    /// catch or ignore.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] — delivery failed (already reaped, `EPERM`).
    fn kill_tree(&mut self) -> Result<(), RunnerError>;
}

/// Spawn seam between engine and OS
pub trait ProcessRunner: Send + Sync + 'static {
    /// The live-child type this runner produces
    type Proc: RunningProcess;

    /// Spawns per the spec, returning the proc + its IO bundle
    ///
    /// Must be called from within a Tokio runtime context: both
    /// implementations (the scripted fake and the real runner) spawn
    /// background tasks internally to pump IO.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SpawnFailed`] — exec failure, permissions, missing binary.
    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError>;
}

/// Everything a spawn needs, pre-assembled by the assembler (a later task)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// Sheep name (for logging/tracing, not passed to the child)
    pub name: String,
    /// Executable path or name (resolved via `PATH` if bare)
    pub program: String,
    /// Argument vector, `argv[1..]`
    pub args: Vec<String>,
    /// Working directory; `None` inherits the daemon's
    pub cwd: Option<PathBuf>,
    /// Environment variables, fully resolved (no daemon-env leakage beyond this map)
    pub env: BTreeMap<String, String>,
    /// File stdout is appended to
    pub out_file: PathBuf,
    /// File stderr is appended to
    pub err_file: PathBuf,
    /// Open the shepherd channel (fd 3 socketpair)
    pub channel: bool,
    /// Unix uid/gid to drop to before exec (`None` inherits the daemon's own
    /// identity). Resolved once per `Start` by `crate::privilege::resolve`;
    /// see that module for how `user`/`group` config names become this.
    pub credentials: Option<Credentials>,
}

/// Error type returned from spawn and process control
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerError {
    /// The OS refused the spawn (exec failure, permissions, missing binary)
    SpawnFailed(String),
    /// Signal delivery failed (already reaped, `EPERM`)
    SignalFailed(String),
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(msg) => write!(f, "process spawn failed: {msg}"),
            Self::SignalFailed(msg) => write!(f, "signal delivery failed: {msg}"),
        }
    }
}

impl core::error::Error for RunnerError {}
