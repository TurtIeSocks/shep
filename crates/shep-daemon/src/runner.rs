//! Spawn seam between the daemon engine and the OS
//!
//! [`ProcessRunner`] spawns a child; the [`RunningProcess`] it returns owns
//! that one live child. Spawn also hands back a [`ProcIo`] bundle of
//! channels, so the sheep task pumps stdout/stderr and shepherd-channel
//! messages without the runner blocking on delivery.
//!
//! Also owns the log plane's vocabulary ([`LogCtl`] and its two errors) and
//! this crate's only opener of a sheep's log file, `open_log_path`, with the
//! ancestry guard that runs ahead of it. The log pump and `shep flush` both
//! go through the pair, so neither can drift on what it will open. The
//! `#[cfg(unix)]` items are the handover's; Windows has no `execve`.

use core::fmt;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use shep_core::signals::OperatorSignal;

use crate::channel::{ChildMessage, ShepherdMessage};
use crate::privilege::Credentials;

/// Re-exported so [`AdoptSpec`]'s public signature can name it: the reaper
/// itself lives in the crate-private `handover` module.
#[cfg(unix)]
pub use crate::handover::reap::AdoptedReaper;

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
    /// `SIGTERM`: graceful stop request
    Term,
    /// `SIGINT`: interrupt
    Int,
    /// `SIGQUIT`: quit, core-dumping by default
    Quit,
    /// `SIGUSR2`: user-defined signal 2
    Usr2,
    /// `SIGKILL`: unblockable, immediate
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
/// The [`oneshot`] on each variant is what makes it usable as a barrier:
/// once it resolves, every live pump has done the thing. A logrotate
/// `postrotate` stanza needs that of [`Self::Reopen`] before it compresses
/// what it renamed; `shep flush` needs it of [`Self::Flush`] before it
/// truncates.
///
/// `#[non_exhaustive]`: this crate is published, and an out-of-tree
/// `ProcessRunner` matching exhaustively would break on the next variant.
#[derive(Debug)]
#[non_exhaustive]
pub enum LogCtl {
    /// Drop the current handle and open the path again, then acknowledge.
    /// Sent when an external rotator has renamed the file.
    Reopen {
        /// Fires once the pump has finished acting on this request.
        ///
        /// `Ok` says both old handles were flushed and closed and both paths
        /// were opened again. [`ReopenError`] says at least one path could
        /// not be opened: the old handle is closed either way, so the rename
        /// is safe to act on, but that stream's lines are dropped until
        /// something reopens it.
        ///
        /// The channel buffers, so a request that was accepted is not one
        /// that will be served: a pump that ends first drops this sender and
        /// the caller's `await` resolves [`Err`](oneshot::error::RecvError).
        /// Treat that as the stopped-sheep no-op a failed send means.
        done: oneshot::Sender<Result<(), ReopenError>>,
    },
    /// Write out whatever the pump has buffered, wait for it to reach the
    /// file, keep the handle, then acknowledge. Sent as the first half of
    /// `shep flush`, immediately before the recorded paths are truncated.
    Flush {
        /// Fires once both handles have nothing buffered and no write in
        /// flight.
        ///
        /// The barrier the truncate that follows is ordered against:
        /// `write_all` on a [`tokio::fs::File`] returns as soon as the real
        /// `write(2)` is queued, so a line already dispatched could otherwise
        /// land at offset 0 after the file was emptied.
        ///
        /// [`FlushError`] says at least one stream's owed bytes never reached
        /// its file. It does not hold up the truncate: `poll_flush` drives
        /// the in-flight write to completion either way. A pump that ends
        /// first drops this sender, as [`Self::Reopen`]'s does.
        done: oneshot::Sender<Result<(), FlushError>>,
    },
    /// Write out whatever the pump has buffered, wait for it to reach the
    /// files, then acknowledge with the descriptor numbers a blob is to
    /// name for this sheep. Sent while assembling a daemon handover.
    ///
    /// Flush and report are one request: bytes still buffered behind a
    /// descriptor the blob already claims die with the image at the `execve`,
    /// and the successor cannot repair a gap it never saw.
    ///
    /// Unix only: it answers with raw descriptor numbers, and Windows has no
    /// `execve` and no handover.
    #[cfg(unix)]
    ReportFds {
        /// Fires once both handles have nothing buffered and no write in
        /// flight, carrying the descriptors the blob is to name.
        ///
        /// `CarriedFds::none` is the honest answer for a pump holding
        /// nothing, never an error: a number that names nothing is what the
        /// successor must not be handed. A pump that ends first drops this
        /// sender, as [`Self::Reopen`]'s does.
        ///
        /// `CarriedFds` is crate-private, so an out-of-tree runner can drop
        /// this sender but cannot answer it.
        done: oneshot::Sender<crate::handover::CarriedFds>,
    },
    /// Read the sheep's streams again after a [`Self::ReportFds`] that no
    /// exec followed. Sent when a handover is abandoned.
    ///
    /// A report parks the pump, since a snapshot only stays true while
    /// nothing moves behind it, and normally the exec ends the park by
    /// replacing the image. Every other way out leaves this daemon with a
    /// pump that has stopped reading for the rest of its life, so an
    /// abandoned handover owes every pump it reported one of these.
    ///
    /// No acknowledgement: nobody is ordered against it, and sending one to
    /// a pump that was never parked is a no-op, so it is safe to send widely
    /// rather than exactly. Unix only, as [`Self::ReportFds`] is.
    #[cfg(unix)]
    Resume,
}

/// A [`LogCtl::Reopen`] that could not open one or both of a sheep's log
/// files again.
///
/// Carries a rendered message rather than the `io::Error`s behind it, as
/// [`RunnerError`] does: it crosses a channel and every layer between only
/// prints it, and a `String` is what keeps this `Clone`/`Eq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReopenError {
    /// Every log file the reopen could not open again, as
    /// `"<path>: <what the open reported>"`, joined by `", "` when both
    /// streams failed. Never empty: a reopen that opened both answers `Ok`.
    ///
    /// `", "` and not `"; "` because this list nests:
    /// [`SupervisorError::ReopenFailed`] joins one of these per sheep with
    /// `"; "`, and one separator at both levels would punctuate one sheep
    /// that failed on both streams like two sheep that failed on one each.
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

/// A `shep flush` that could not empty a log file, from either half of that
/// verb: a pump whose pending writes would not reach the file
/// ([`LogCtl::Flush`]), or a path that could not be truncated once they had.
///
/// What the two leave behind differs: a truncate that failed leaves its file
/// as it was, while a failed flush does not hold up the truncate, so that
/// file ends up empty with the lines it held gone unwritten. Carries a
/// rendered message for the reasons [`ReopenError`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushError {
    /// Every log file the flush could not empty, as
    /// `"<path>: <what the failing call reported>"`, joined by `", "` when
    /// both of a pump's streams did. Never empty: a flush that emptied every
    /// file answers `Ok`.
    ///
    /// `", "` for the reason [`ReopenError::message`] gives about its own
    /// separator.
    ///
    /// Keyed by path and never by sheep: several sheep can share one log
    /// path (`merge_logs`, or an explicit `out_file` on a multi-instance
    /// app), so naming one of them would be arbitrary.
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
/// One owner for the sentence, cited by [`open_log_path`] and by both
/// openers' tests, so the operator reads a remedy rather than a bare
/// `ELOOP`. The path is not in here: every caller already prefixes one.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) const SYMLINK_REFUSED: &str = "refusing to follow a symlink at this log path; shep \
     opens log files with O_NOFOLLOW, so point out_file/err_file at the real file";

/// Opens `path` through `options`, refusing a symlink at the path itself
///
/// The one opener of a log file in this crate: the pump's append handle and
/// `shep flush`'s truncating one both come through here.
/// [`check_log_ancestry`] runs before it at both call sites, ahead of
/// `open_append`'s `mkdir`. `O_NOFOLLOW` guards only the final component, so
/// a symlinked parent still resolves; only that check covers it.
///
/// # Errors
///
/// Whatever the open reported, with `ELOOP` relabelled to
/// [`SYMLINK_REFUSED`]: `NotFound` passes through untouched.
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
/// One errno covers both supported platforms: POSIX specifies `ELOOP` and
/// Darwin's `open(2)` matches. The kind is carried over; only the message
/// changes.
#[cfg(unix)]
fn name_the_symlink(error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(nix::libc::ELOOP) {
        io::Error::new(error.kind(), SYMLINK_REFUSED)
    } else {
        error
    }
}

/// The non-unix arm of [`name_the_symlink`]: no `O_NOFOLLOW`, so no refusal
/// to relabel.
#[cfg(not(unix))]
fn name_the_symlink(error: io::Error) -> io::Error {
    error
}

/// Refuses a log path another local user could redirect, under root only
///
/// [`open_log_path`]'s other half, run before it (and before any `mkdir`)
/// at both call sites. A loose ancestry escalates only under a privileged
/// daemon, so root refuses and everyone else is warned once per path.
/// [`loose_ancestor`] defines loose, and the window between the check and
/// the open stays open (`docs/specs/deferred.md`).
///
/// # Errors
///
/// [`io::ErrorKind::PermissionDenied`] naming the loose ancestor, when the
/// daemon's effective uid is root. The message carries no path of its own.
pub(crate) fn check_log_ancestry(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        check_log_ancestry_as(path, crate::server::daemon_uid())
    }
    // Windows has neither the uid model this reads nor the `shep flush`
    // surface that reaches it, so there is nothing to check.
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// The effective uid a shepherd has to be running as for a loose ancestor to
/// be an escalation rather than a footgun.
#[cfg(unix)]
const ROOT_UID: u32 = 0;

/// The permission bit that lets every local user create entries in a
/// directory. Narrower than `boot`'s socket-directory check (`0o022`, group
/// or world): a group-writable log directory names accounts an operator
/// chose, while this bit names everyone.
#[cfg(unix)]
const WORLD_WRITABLE: u32 = 0o002;

/// Log paths whose loose ancestry has already been reported, so an
/// unprivileged shepherd says it once rather than on every open.
///
/// Keyed by the log path, not by the offending ancestor: that is what an
/// operator asking which of their apps this is about is asking. Bounded by
/// the number of distinct log paths in the flock.
#[cfg(unix)]
static WARNED_LOOSE_LOG_PATHS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// [`check_log_ancestry`] with the daemon's effective uid supplied, so the
/// privileged arm is reachable from a test that is not running as root.
///
/// # Errors
///
/// [`check_log_ancestry`]'s.
#[cfg(unix)]
fn check_log_ancestry_as(path: &Path, daemon_uid: u32) -> io::Result<()> {
    let Some(loose) = loose_ancestor(path, daemon_uid) else {
        return Ok(());
    };
    if daemon_uid == ROOT_UID {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to open a log file below {}, which {}; a shepherd running as root \
                 writes only below directories its own user owns",
                loose.path.display(),
                loose.reason,
            ),
        ));
    }
    let first_time = WARNED_LOOSE_LOG_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_path_buf());
    if first_time {
        tracing::warn!(
            path = %path.display(),
            ancestor = %loose.path.display(),
            reason = %loose.reason,
            "log path sits below a directory another local user could redirect; a shepherd \
             running as root would refuse to open it"
        );
    }
    Ok(())
}

/// An ancestor of a log path that another local user could use to redirect
/// where that path lands.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct LooseAncestor {
    /// The offending component, as it appears in the log path.
    path: PathBuf,
    /// Why it offends: reads as the predicate in `"<path> <reason>"`.
    reason: LooseReason,
}

/// Why an ancestor of a log path counts as loose.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LooseReason {
    /// Owned by a uid that is neither the daemon's own nor root's, so its
    /// owner can replace or redirect it under the daemon (carries that uid).
    /// Also how a symlinked component is caught: the link's own owner is the
    /// user who planted it.
    ForeignOwner(u32),
    /// A directory every local user can create entries in, so anyone can put
    /// a symlink where the next component is about to be resolved.
    WorldWritable,
}

#[cfg(unix)]
impl fmt::Display for LooseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignOwner(uid) => write!(f, "is owned by uid {uid}"),
            Self::WorldWritable => f.write_str("is world-writable"),
        }
    }
}

/// The nearest ancestor of `path` another local user could redirect it
/// through, or `None` when every one is the daemon's to trust.
///
/// Walks the path's own textual components upwards from its parent and stops
/// at the first offender. `symlink_metadata`, never `metadata`: a symlinked
/// component must be read as the link it is, owned by whoever planted it. An
/// ancestor that does not exist, or cannot be stat'd, is skipped, since
/// `open_append` is about to create it at `boot::DIR_MODE` as the daemon.
///
/// One `lstat(2)` per component per log-file open, 7.8 µs for a
/// nine-component path on macOS, so it runs inline rather than paying a
/// `spawn_blocking` hop.
#[cfg(unix)]
fn loose_ancestor(path: &Path, daemon_uid: u32) -> Option<LooseAncestor> {
    use std::os::unix::fs::MetadataExt as _;

    path.parent()?
        .ancestors()
        .filter_map(|ancestor| Some((ancestor, std::fs::symlink_metadata(ancestor).ok()?)))
        .find_map(|(ancestor, meta)| {
            let reason = if meta.uid() != daemon_uid && meta.uid() != ROOT_UID {
                LooseReason::ForeignOwner(meta.uid())
            } else if meta.is_dir() && meta.mode() & WORLD_WRITABLE != 0 {
                LooseReason::WorldWritable
            } else {
                return None;
            };
            Some(LooseAncestor {
                path: ancestor.to_path_buf(),
                reason,
            })
        })
}

/// IO endpoints handed back by spawn; the runner pumps internally.
///
/// The sheep task owns this and must drain every receiver: an undrained
/// `from_child` back-pressures a metric-emitting child until it stalls on
/// its own fd-3 write.
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
    /// ends when the last of these senders drops, so hold this while the
    /// child is alive: ending the pump drops the read ends of both pipes and
    /// the child's next write gets `EPIPE`/`SIGPIPE`. The supervisor clones
    /// it (`SheepSlot::log_ctl`); what keeps a clone from stretching a
    /// pump's life is the pump's own exit on the `logs` receiver going away.
    ///
    /// A send that fails means the pump is already gone, which makes a
    /// reopen a no-op rather than an error.
    pub log_ctl: mpsc::Sender<LogCtl>,
    /// The shepherd's writing end of this sheep's stdin.
    ///
    /// Always present, and closed rather than absent when the app asked for
    /// no pipe: the runner drops the receiving end, so `is_closed()` is the
    /// one question a caller asks, the same shape [`Self::to_child`] uses.
    ///
    /// Hold it only for as long as the child is alive: the task on the far
    /// end parks on `recv()`, so a sender kept past the child's exit parks
    /// that task and holds the pipe's write end with it.
    pub to_stdin: mpsc::Sender<StdinWrite>,
}

/// One line to write to a sheep's stdin, and where the answer goes.
///
/// The acknowledgement is the point, as on [`LogCtl`]: an `mpsc::send` only
/// proves the message was queued, and the line may still be sitting behind a
/// pipe the app has stopped reading. The `oneshot` fires after the bytes are
/// written and flushed.
#[derive(Debug)]
pub struct StdinWrite {
    /// The line, without its terminator: the writer appends one `\n`.
    pub line: String,
    /// Fires once the line has landed, or with why it could not.
    ///
    /// A dropped sender means the writer task ended before serving this
    /// request, which happens when the child's stdin closed; the caller reads
    /// that as the pipe being gone.
    pub done: oneshot::Sender<Result<(), RunnerError>>,
}

/// A live child.
pub trait RunningProcess: Send + 'static {
    /// The OS process id
    fn pid(&self) -> u32;

    /// Resolves exactly once with the exit outcome
    ///
    /// # Cancellation safety
    ///
    /// Dropping the returned future and calling `wait` again neither
    /// restarts the wait nor loses progress toward it, as
    /// [`tokio::process::Child::wait`] guarantees; the scripted fake mirrors
    /// it by fixing its exit deadline once, at spawn.
    ///
    /// The future is `Send` (RPITIT) because the sheep task that owns the
    /// proc is `tokio::spawn`'ed.
    fn wait(&mut self) -> impl core::future::Future<Output = ExitOutcome> + Send;

    /// Sends a signal to the sheep's whole process group
    ///
    /// Group-wide, not leader-only: a `thing & wait` wrapper's forked child
    /// stays in its own group, and a leader-only signal would leave it
    /// running and untracked. Implementors must spawn each child as the
    /// leader of a fresh group; this and [`Self::kill_tree`] address it by
    /// [`Self::pid`], so a child that escapes with `setsid` is beyond both.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] if delivery failed (already reaped,
    ///   `EPERM`).
    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError>;

    /// Sends `sig` to this sheep's own process, never its process group.
    ///
    /// Not group-wide, unlike [`Self::signal`]: this exists for a
    /// conversation between an operator and one application, and a `SIGHUP`
    /// broadcast to the group would reach whatever `sh` and runtime children
    /// are in it. The default refuses rather than widening to the group.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] if delivery failed (`ESRCH`, `EPERM`)
    ///   or this implementation has no per-process delivery at all.
    fn signal_process(&mut self, sig: OperatorSignal) -> Result<(), RunnerError> {
        let _ = sig;
        Err(RunnerError::SignalFailed(
            "this runner cannot signal a single process".to_string(),
        ))
    }

    /// SIGKILLs the whole process group/tree
    ///
    /// The escalation rung above [`Self::signal`]: same group, same
    /// process-group assumption, but a signal nothing can catch or ignore.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] if delivery failed (already reaped,
    ///   `EPERM`).
    fn kill_tree(&mut self) -> Result<(), RunnerError>;
}

/// Spawn seam between engine and OS
pub trait ProcessRunner: Send + Sync + 'static {
    /// The live-child type this runner produces
    type Proc: RunningProcess;

    /// Spawns per the spec, returning the proc + its IO bundle
    ///
    /// Must be called from within a Tokio runtime context: both
    /// implementations spawn background tasks internally to pump IO.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SpawnFailed`] on an exec failure, bad permissions or
    ///   a missing binary.
    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError>;

    /// What is knowable about `spec` before anything is spawned
    ///
    /// Lets a caller refuse a whole batch before registering any of it,
    /// rather than registering the apps ahead of the one that cannot spawn.
    /// See [`Preflight`] for which verdicts a caller may refuse a batch over.
    /// The default answers [`Preflight::Unknown`], which is also the honest
    /// answer for a runner that never touches the filesystem.
    #[must_use]
    fn preflight(&self, spec: &SpawnSpec) -> Preflight {
        let _ = spec;
        Preflight::Unknown
    }

    /// Rebuilds a proc around a sheep this process inherited, not started
    ///
    /// The successor half of a handover, unix only as the handover is. Every
    /// handle in `spec` crossed an `execve` with its `FD_CLOEXEC` cleared, so
    /// the sheep never noticed: same pid, same pipes, same open file
    /// description on each log. Nothing here spawns or signals.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::AdoptFailed`] if this runner cannot take a process it
    ///   did not spawn (what the default answers), or if the carried handles
    ///   could not be wired to a pump.
    #[cfg(unix)]
    fn adopt(&self, spec: AdoptSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        let _ = spec;
        Err(RunnerError::AdoptFailed(
            "this runner cannot adopt a process it did not spawn".to_string(),
        ))
    }
}

/// One inherited sheep, and everything a runner needs to supervise it again.
///
/// Produced by the successor's `handover::adopt` and consumed by
/// [`ProcessRunner::adopt`]. The handles are owned, since adopting is taking
/// ownership of them; `None` on a pair means the predecessor had no handle
/// to carry.
///
/// `Debug` is derived: descriptor numbers, a pid and two log paths are all in
/// `shep flock` already, and no env value reaches this type.
#[cfg(unix)]
#[derive(Debug)]
pub struct AdoptSpec {
    /// The pid the sheep has been running under all along, unchanged by the
    /// handover.
    pub pid: u32,
    /// Where its stdout is logged, kept so a later rotation can reopen it.
    pub out_file: PathBuf,
    /// Where its stderr is logged, kept for the same reason.
    pub err_file: PathBuf,
    /// The read end of its stdout pipe, still the one the child writes into.
    pub out_pipe: Option<tokio::net::unix::pipe::Receiver>,
    /// The read end of its stderr pipe, likewise.
    pub err_pipe: Option<tokio::net::unix::pipe::Receiver>,
    /// The appending handle on its stdout log, written through rather than
    /// reopened so `O_APPEND` survives.
    pub out_log: Option<tokio::fs::File>,
    /// The appending handle on its stderr log, likewise.
    pub err_log: Option<tokio::fs::File>,
    /// The write end of its stdin pipe, still the one the child reads from,
    /// for a sheep whose app asked for one.
    ///
    /// The one handle here the daemon writes to. `None` is the commoner
    /// sheep, which has `/dev/null` on fd 0.
    pub stdin_pipe: Option<tokio::net::unix::pipe::Sender>,
    /// The daemon's end of its shepherd-channel socketpair, still the one
    /// whose other end is the child's fd 3.
    ///
    /// Goes both ways, so an adoption puts both pumps back on it. `None` for
    /// a sheep whose app asked for no channel, and for one whose child has
    /// closed fd 3.
    pub channel: Option<tokio::net::UnixStream>,
    /// The one reaper this successor waits every adopted pid through.
    ///
    /// Shared rather than owned per sheep: a status can be collected once,
    /// so two reapers racing on one pid would leave one meeting `ECHILD`.
    pub reaper: std::sync::Arc<AdoptedReaper>,
}

/// What a [`ProcessRunner`] can tell about a [`SpawnSpec`] before anything is
/// spawned
///
/// The line that matters runs between [`Self::Impossible`] and
/// [`Self::Doubtful`], and it separates two kinds of claim. A path with a
/// `/` is a claim about the filesystem, which the daemon can check. A bare
/// command is a claim about the daemon's own environment, which is not the
/// shell the operator tested in: `node` from homebrew or nvm resolves in a
/// terminal and under no `shep startup` unit. Refusing a batch on the second
/// would keep twelve apps down over one interpreter.
// `#[non_exhaustive]`: an out-of-tree consumer can match this exhaustively,
// and a fourth verdict would break them with no version bump.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// Nothing is knowable in advance.
    ///
    /// Not "this will work": every form an implementation declines to decide
    /// arrives here alongside every form it decided is fine. A caller may
    /// only act on the other two variants.
    Unknown,
    /// The spawn cannot succeed, as a certainty. Carries one reason, no
    /// trailing punctuation, ready to be printed after a sheep's name.
    ///
    /// A caller registering a batch should refuse the whole batch and
    /// register none of it.
    Impossible(String),
    /// The spawn looks like it will fail, and a caller must not refuse a
    /// batch over it. Carries a reason on the same terms as
    /// [`Self::Impossible`]. Report it and carry on: the spawn then fails for
    /// that one sheep as it would have anyway.
    Doubtful(String),
}

/// Everything a spawn needs, pre-assembled by the assembler (a later task)
#[derive(Clone, PartialEq, Eq)]
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
    /// Pipe the child's stdin, so `shep whisper` can write to it. `false`
    /// gives the child `/dev/null` on fd 0, which is what every sheep gets
    /// unless its config sets `stdin = true`.
    pub stdin: bool,
    /// Unix uid/gid to drop to before exec (`None` inherits the daemon's own
    /// identity). Resolved once per `Start` by `crate::privilege::resolve`;
    /// see that module for how `user`/`group` config names become this.
    pub credentials: Option<Credentials>,
}

/// Redacted: `env` carries whatever the operator configured, and this type is
/// the one handed to `Command::envs` at exec.
impl fmt::Debug for SpawnSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnSpec")
            .field("name", &self.name)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .finish_non_exhaustive()
    }
}

/// Error type returned from spawn and process control
///
/// `#[non_exhaustive]`: a future process-control primitive, a cgroup freeze
/// or a Windows job-object failure, would need its own variant rather than
/// stretching one of these, and an out-of-tree matcher should not break.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerError {
    /// The OS refused the spawn (exec failure, permissions, missing binary)
    SpawnFailed(String),
    /// Signal delivery failed (already reaped, `EPERM`)
    SignalFailed(String),
    /// A write to a child's stdin failed (carries the OS message, or the
    /// shepherd's own bound when the app was not reading).
    WriteFailed(String),
    /// A sheep inherited across a handover could not be taken back under
    /// supervision: this runner does not adopt at all, or the carried
    /// handles could not be wired to a pump.
    AdoptFailed(String),
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(msg) => write!(f, "process spawn failed: {msg}"),
            Self::SignalFailed(msg) => write!(f, "signal delivery failed: {msg}"),
            Self::WriteFailed(msg) => write!(f, "stdin write failed: {msg}"),
            Self::AdoptFailed(msg) => write!(f, "process adoption failed: {msg}"),
        }
    }
}

impl core::error::Error for RunnerError {}

// Every case here is `#[cfg(unix)]`, as is everything they exercise: the uid
// model `loose_ancestor` reads, the mode bits it tests, and
// `std::os::unix::fs::symlink`.
#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::testing::capture_logs;

    /// A uid no fixture in this module creates anything as. Only reached on
    /// a root test runner, where everything created is root-owned and root is
    /// exempt by construction.
    const FOREIGN_UID: u32 = 65_432;

    /// This process's effective uid: what every fixture directory is owned
    /// by, and what the cases move the daemon's uid relative to.
    fn me() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    /// A log path two components below `dir`, with its parent created and
    /// left at `mode`.
    fn log_path_under(dir: &tempfile::TempDir, mode: u32) -> (PathBuf, PathBuf) {
        let parent = dir.path().join("logs");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
        let log = parent.join("web-0-out.log");
        (parent, log)
    }

    /// Also the only case pinning the world-writable arm on its own: drop
    /// that arm and this reddens with `None` while the ownership cases below
    /// stay green.
    #[test]
    fn the_nearest_loose_ancestor_is_the_one_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (parent, log) = log_path_under(&dir, 0o777);

        assert_eq!(
            loose_ancestor(&log, me()),
            Some(LooseAncestor {
                path: parent,
                reason: LooseReason::WorldWritable,
            })
        );
    }

    /// The parent is `0700`, so a write-bit-only check waves it through,
    /// while its owner can still replace it under a root shepherd.
    #[test]
    fn an_ancestor_owned_by_another_user_is_loose_however_tight_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (parent, log) = log_path_under(&dir, 0o700);

        // The predicate is symmetric in the two uids, so an unprivileged
        // runner moves the daemon's rather than the directory's; it cannot
        // chown. A root runner must move the directory's: root is exempt
        // whatever the daemon's uid is.
        let (daemon_uid, owner) = if me() == ROOT_UID {
            std::os::unix::fs::chown(&parent, Some(FOREIGN_UID), None).unwrap();
            (ROOT_UID, FOREIGN_UID)
        } else {
            (me() + 1, me())
        };

        assert_eq!(
            loose_ancestor(&log, daemon_uid),
            Some(LooseAncestor {
                path: parent,
                reason: LooseReason::ForeignOwner(owner),
            })
        );
    }

    /// The link is owned by this user and points at a root-owned, tight
    /// directory, so following it blames the tempdir instead. Only the path
    /// in the answer tells the two apart. This is the case `O_NOFOLLOW`
    /// cannot cover: the redirect is one level up.
    #[test]
    fn a_symlinked_component_is_judged_as_the_link_not_as_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("logs");
        // `/usr` exists and is root-owned `0755` on both tier-1 platforms,
        // an ancestor the walk would wave through if it followed the link.
        std::os::unix::fs::symlink("/usr", &link).unwrap();
        let log = link.join("web-0-out.log");

        let loose = loose_ancestor(&log, me() + 1).expect("a foreign-owned component is loose");
        assert_eq!(
            loose.path,
            link,
            "the link itself must be judged, not what it resolves to: blaming {} means the \
             walk followed it",
            loose.path.display()
        );
    }

    /// Refusing everywhere would break a developer logging to `/tmp` as
    /// themselves; warning everywhere would leave the root case exiting zero.
    ///
    /// The warn-once half rides along because it is the same call: a count of
    /// two means the dedup set is gone.
    #[test]
    fn a_root_shepherd_refuses_where_an_unprivileged_one_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        let (parent, log) = log_path_under(&dir, 0o777);

        let refused = check_log_ancestry_as(&log, ROOT_UID)
            .expect_err("a root shepherd must not open a log below a loose ancestor");
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            refused.to_string().contains(&parent.display().to_string()),
            "the refusal must name the ancestor an operator has to fix: {refused}"
        );

        // Never `me()`: a root test runner would take the arm above.
        // `me() + 1` is non-root by construction and owns nothing here.
        let unprivileged = me() + 1;
        let rendered = capture_logs(|| {
            assert_eq!(check_log_ancestry_as(&log, unprivileged).ok(), Some(()));
            assert_eq!(check_log_ancestry_as(&log, unprivileged).ok(), Some(()));
        });
        assert_eq!(
            rendered.matches("log path sits below").count(),
            1,
            "an unprivileged shepherd warns once per path, not once per open: {rendered}"
        );
    }

    /// This type sits on the exec boundary, so a `tracing` call that
    /// formatted it would put every configured secret in the daemon's log.
    /// Exact string pinned so a `derive(Debug)` refactor fails here.
    #[test]
    fn debug_redacts_env_values() {
        let mut spec = SpawnSpec {
            name: "web".to_string(),
            program: "./srv".to_string(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            out_file: PathBuf::from("/tmp/web-out.log"),
            err_file: PathBuf::from("/tmp/web-err.log"),
            channel: false,
            stdin: false,
            credentials: None,
        };
        spec.env.insert(
            "DATABASE_URL".to_string(),
            "postgres://user:hunter2@db".to_string(),
        );
        spec.env
            .insert("API_KEY".to_string(), "sk-live-abc".to_string());

        assert_eq!(
            format!("{spec:?}"),
            "SpawnSpec { name: \"web\", program: \"./srv\", args: [], cwd: None, env: <2 vars>, \
             .. }"
        );
    }
}
