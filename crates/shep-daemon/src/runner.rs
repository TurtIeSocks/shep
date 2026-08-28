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
//! errors its requests answer with — and, with them, this crate's only
//! opener of a sheep's log file (`open_log_path`, crate-private) plus the
//! ancestry guard that runs ahead of it. The log pump and `shep flush` both
//! go through that pair, so neither half can drift from the other on what it
//! is willing to open.
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

use shep_core::signals::OperatorSignal;

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
#[cfg_attr(windows, allow(dead_code))]
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
/// [`check_log_ancestry`] is this function's other half and runs BEFORE it at
/// both call sites — before `open_append`'s `mkdir` in particular, so no
/// directory is created down an ancestry that is about to be refused. The two
/// are split rather than nested for exactly that ordering; neither is
/// meaningful without the other.
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
/// that in the open itself needs `openat2(RESOLVE_NO_SYMLINKS)`, which is
/// Linux-only — so it cannot be the only path here, though it could be a
/// Linux fast path beside this one. What that would cost, and why Phase 10
/// did not spend it, is on [`check_log_ancestry`] and in
/// `docs/specs/deferred.md`. [`check_log_ancestry`] covers that case from the
/// other side — by refusing an ancestry a privileged daemon should not be
/// writing below at all — but it checks rather than resolves, so a TOCTOU
/// window remains between the two. This stops the realistic attack; it does
/// not make the operation atomic.
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

/// Refuses — under a privileged shepherd — to open a log file whose ancestry
/// another local user could redirect, and warns about it under any other.
///
/// [`open_log_path`]'s other half, run before it (and before any `mkdir`) at
/// both of that function's call sites.
///
/// # Why the split by uid
///
/// The chain is an ESCALATION only when the daemon is privileged. A shepherd
/// running as an ordinary user that logs into a shared directory has handed
/// nobody anything they could not already do as themselves — it is a footgun,
/// not a vulnerability — and refusing it outright would break a developer
/// logging to `/tmp`, which is a legitimate thing to do. So root refuses and
/// everyone else is warned, once per path. The warning costs nothing at the
/// default level: the subscriber ships at `warn`.
///
/// # What counts as loose
///
/// See [`loose_ancestor`]. Two things, both about the components of the path
/// itself rather than about where it points: an ancestor owned by neither the
/// daemon's uid nor root, and a world-writable ancestor DIRECTORY. Ownership
/// is the load-bearing half — it is what catches an intermediate component
/// swapped for a symlink, which `O_NOFOLLOW` on the final component cannot
/// see, and it catches a plain `0755` directory owned by the app's own
/// dropped-privilege user, which the write bit alone would wave through.
///
/// # What remains
///
/// A TOCTOU window. This checks the ancestry and then opens the path with no
/// atomic tie between the two, so an attacker who can rearrange a directory
/// between the check and the open still wins that race. The bar is raised
/// substantially; the operation is not atomic.
///
/// The syscall that would close it on Linux is
/// `openat2(RESOLVE_NO_SYMLINKS)`, and it IS reachable — `nix 0.29` exposes
/// `fcntl::openat2` under the `fs` feature this crate already enables. What
/// stops it being a Linux fast path here is not availability but cost:
/// `openat2` hands back a `RawFd`, so adopting it into a `File` needs
/// `FromRawFd`, which is `unsafe` and belongs in `sys.rs` (IR-22), behind a
/// `cfg(target_os = "linux")` with an `ENOSYS`/`EPERM` fallback ladder for
/// pre-5.6 kernels and seccomp sandboxes — new unsafe on a Linux-only path
/// this project cannot execute a test for from a macOS development machine.
/// The design is written down in `docs/specs/deferred.md` rather than
/// half-built here.
///
/// # Errors
///
/// [`io::ErrorKind::PermissionDenied`], naming the offending ancestor and
/// why, when the daemon's effective uid is root and an ancestor is loose. The
/// message carries no path of its own — every caller prefixes the log path
/// already.
pub(crate) fn check_log_ancestry(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        check_log_ancestry_as(path, crate::server::daemon_uid())
    }
    // Windows has neither the uid model this reads nor the `shep flush`
    // surface that reaches it (spec §11's functional tier is unbuilt), so
    // there is nothing here to check and nothing to warn about yet.
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
/// directory. Narrower on purpose than `boot`'s socket-directory check, which
/// tests `0o022` (group OR world): a group-writable log directory names a
/// specific set of accounts an operator chose, while this bit names everyone.
#[cfg(unix)]
const WORLD_WRITABLE: u32 = 0o002;

/// Log paths whose loose ancestry has already been reported, so an
/// unprivileged shepherd says it once rather than on every respawn, every
/// reopen and every flush of the same file.
///
/// Keyed by the LOG path, not by the offending ancestor: the warning names
/// both, and an operator asking "which of my apps is this about?" is asking
/// about the one this set is keyed on. Bounded by the number of distinct log
/// paths in the flock.
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
    /// Why it offends — reads as the predicate in `"<path> <reason>"`.
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

/// The nearest ancestor of `path` that another local user could use to
/// redirect it, or `None` when every one of them is already the daemon's to
/// trust.
///
/// Walks the path's own textual components from its parent upwards and stops
/// at the first offender, so the ancestor reported is the one closest to the
/// log file — the one an operator can actually do something about, and the
/// one a test can pin without knowing what `/tmp` looks like on the runner.
///
/// `symlink_metadata`, never `metadata`: a symlinked component must be read
/// as the link it is (owned by whoever planted it) rather than as whatever it
/// resolves to. That is the whole reason ownership is checked at all —
/// `O_NOFOLLOW` on the final component cannot see a redirect one level up.
///
/// An ancestor that does not exist is skipped rather than trusted or blamed:
/// `open_append` is about to create it, and every directory shep creates it
/// creates at `boot::DIR_MODE` (`0700`) as the daemon's own user, which is
/// exactly what this function would then wave through. An ancestor that
/// cannot be stat'd at all is skipped for the same reason it cannot be
/// judged.
///
/// # Cost
///
/// One `lstat(2)` per component of the path, once per log-file open — so
/// every spawn, every respawn, every reopen, and once per distinct path in a
/// flush. The syscalls hit the kernel's dentry cache after the first, and the
/// walk stops early on the first offender, which on a loose ancestry is
/// usually the first component it looks at.
///
/// Measured on macOS (release, warm cache, no offender so the walk runs to
/// the root): **7.8 µs** for a nine-component path — the shape
/// `$SHEP_HOME/logs/<name>-<n>-out.log` has under a home directory — and
/// **26 µs** for a twenty-four-component one, so about 1.1 µs per component.
/// That is against an `open(2)` this crate already dispatches to the blocking
/// pool, and against the process spawn a pump's first open belongs to, which
/// costs milliseconds. It is run inline rather than on the blocking pool
/// because a few microseconds is well inside what a runtime worker may do
/// between yields, and a `spawn_blocking` hop per open would cost more than
/// the walk.
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
    /// The shepherd's writing end of this sheep's stdin.
    ///
    /// Always present, and closed rather than absent when the app did not ask
    /// for a pipe: the runner drops the receiving end in that case, so
    /// `is_closed()` is the one question a caller has to ask — the same shape
    /// [`Self::to_child`] uses for a sheep configured without a shepherd
    /// channel.
    ///
    /// Hold it only for as long as the child is alive. The task on the far end
    /// parks on `recv()` and has no other way to finish, so a sender kept past
    /// the child's exit parks that task and holds the pipe's write end with it.
    pub to_stdin: mpsc::Sender<StdinWrite>,
}

/// One line to write to a sheep's stdin, and where the answer goes.
///
/// The acknowledgement is the point, exactly as it is on [`LogCtl`]: an
/// `mpsc::send` only proves the message was queued, and a caller told "sent"
/// on that basis would be told it about a line still sitting in a channel
/// behind a pipe the app has stopped reading. The `oneshot` fires after the
/// bytes are written AND flushed, which is the strongest claim this side of
/// the pipe can honestly make.
#[derive(Debug)]
pub struct StdinWrite {
    /// The line, without its terminator — the writer appends exactly one `\n`.
    pub line: String,
    /// Fires once the line has landed, or with why it could not.
    ///
    /// A dropped sender means the writer task ended before serving this
    /// request, which happens when the child's stdin closed — the caller reads
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

    /// Sends `sig` to this sheep's OWN process, never its process group.
    ///
    /// The counterpart to [`Self::signal`], and the difference between them is
    /// the whole design of `shep signal`. That one is group-wide because a
    /// stop has to reach a `thing & wait` wrapper's child too. This one is not,
    /// because it exists for a conversation between an operator and one
    /// application: a `SIGHUP` broadcast to every process in the group reaches
    /// whatever `sh` and whatever runtime child happen to be in it, none of
    /// which the operator addressed and several of which have their own
    /// meaning for the signal.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] — delivery failed (`ESRCH` for a
    ///   process reaped between the lookup and the syscall, `EPERM` for one
    ///   this daemon may not signal), or this implementation has no per-process
    ///   delivery at all.
    ///
    /// # Default implementation
    ///
    /// Refuses. A defaulted method rather than a required one so that adding
    /// it did not break an out-of-tree implementor of this trait, which is a
    /// `pub` trait in a published library — the same courtesy `#[non_exhaustive]`
    /// buys an enum (IR-20). An implementation that can deliver a signal to one
    /// process overrides it; one that cannot says so honestly instead of
    /// silently widening to the group.
    fn signal_process(&mut self, sig: OperatorSignal) -> Result<(), RunnerError> {
        let _ = sig;
        Err(RunnerError::SignalFailed(
            "this runner cannot signal a single process".to_string(),
        ))
    }

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

    /// What is knowable about `spec` before anything is spawned
    ///
    /// Lets a caller refuse a whole batch before registering any of it. The
    /// defect this exists for: an eleven-app Flockfile whose third app
    /// pointed at an unbuilt binary registered and started apps one and two,
    /// failed on three, and never reached four through eleven, leaving a
    /// flock that matched neither the file nor its previous state.
    ///
    /// See [`Preflight`] for what separates a verdict a caller may refuse a
    /// batch over from one it may only report. That split is the whole
    /// design: a check that refuses too much is worse than the bug it
    /// addresses, because it takes a flock down that would have come up.
    ///
    /// # Default implementation
    ///
    /// [`Preflight::Unknown`], always. A defaulted method rather than a
    /// required one for the reason [`RunningProcess::signal_process`] gives:
    /// this is a `pub` trait in a published library, and adding a required
    /// method to one is a break for an out-of-tree implementor (IR-20). The
    /// default is also the honest answer for a runner that never touches the
    /// filesystem, which is what the crate's own fakes are.
    #[must_use]
    fn preflight(&self, spec: &SpawnSpec) -> Preflight {
        let _ = spec;
        Preflight::Unknown
    }
}

/// What a [`ProcessRunner`] can tell about a [`SpawnSpec`] before anything is
/// spawned
///
/// Three variants because a caller registering a BATCH has three different
/// things to do with the answer, and collapsing any two of them costs a
/// flock. The line that matters runs between [`Self::Impossible`] and
/// [`Self::Doubtful`], and it is a line between two kinds of claim rather
/// than between two confidence levels:
///
/// - a path with a `/` in it is a claim about the FILESYSTEM, which the
///   daemon can check with certainty and an operator can fix with a typo
///   correction.
/// - a bare command is a claim about an ENVIRONMENT, and the environment
///   that matters is the daemon's, not the shell the operator tested in. A
///   `shep startup` unit gives the shepherd whatever `PATH` launchd or
///   systemd hands it, so `node` from homebrew on Apple Silicon
///   (`/opt/homebrew/bin`) and `node` from nvm (under `$HOME`) both resolve
///   in a terminal and neither resolves under the unit.
///
/// Refusing a batch on the second kind would mean one app's interpreter
/// keeping the other twelve down at boot, which is strictly worse than the
/// partial registration the check exists to prevent.
// `#[non_exhaustive]`: shep-daemon is a published library, an out-of-tree
// consumer can match this exhaustively today, and a fourth verdict would
// break them with no version bump to say so (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// Nothing is knowable in advance.
    ///
    /// NOT "this will work". Every form an implementation declines to decide
    /// arrives here, alongside every form it decided is fine, and the two are
    /// deliberately not distinguished: a caller may only ever act on the
    /// other two variants.
    Unknown,
    /// The spawn cannot succeed, and that is a certainty rather than a
    /// suspicion. Carries one reason, no trailing punctuation, ready to be
    /// printed after a sheep's name.
    ///
    /// A caller registering a batch should refuse the WHOLE batch on this and
    /// register none of it.
    Impossible(String),
    /// The spawn looks like it will fail, and a caller must not refuse a
    /// batch over it. Carries a reason on the same terms as
    /// [`Self::Impossible`].
    ///
    /// Report it and carry on: the spawn then fails for that one sheep
    /// exactly as it would have anyway, and every other app in the batch
    /// comes up. See this enum's own doc for why the two are not the same
    /// question.
    Doubtful(String),
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
    /// Pipe the child's stdin, so `shep whisper` can write to it. `false`
    /// gives the child `/dev/null` on fd 0, which is what every sheep gets
    /// unless its config sets `stdin = true`.
    pub stdin: bool,
    /// Unix uid/gid to drop to before exec (`None` inherits the daemon's own
    /// identity). Resolved once per `Start` by `crate::privilege::resolve`;
    /// see that module for how `user`/`group` config names become this.
    pub credentials: Option<Credentials>,
}

/// Error type returned from spawn and process control
///
/// `#[non_exhaustive]`: today's three variants cover spawn, signal delivery
/// and a stdin write, and a future process-control primitive — a cgroup
/// freeze, or a Windows job-object failure — would need its own variant
/// rather than stretching one of these to mean something it does not cover,
/// and shep-daemon is a published library an out-of-tree matcher should not
/// break for (IR-20).
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
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(msg) => write!(f, "process spawn failed: {msg}"),
            Self::SignalFailed(msg) => write!(f, "signal delivery failed: {msg}"),
            Self::WriteFailed(msg) => write!(f, "stdin write failed: {msg}"),
        }
    }
}

impl core::error::Error for RunnerError {}

// Every case here is `#[cfg(unix)]`, as is everything they exercise: the uid
// model `loose_ancestor` reads, the mode bits it tests, and
// `std::os::unix::fs::symlink`. On Windows this module compiles to nothing,
// which matches `check_log_ancestry`'s own non-unix arm having nothing to do.
#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::testing::capture_logs;

    /// A uid no fixture in this module creates anything as. Only reached on a
    /// root test runner, where `chown` is available and everything the test
    /// creates would otherwise be root-owned — and root is exempt by
    /// construction, so a self-owned fixture could not be foreign there.
    const FOREIGN_UID: u32 = 65_432;

    /// This process's effective uid — what every fixture directory below is
    /// owned by, and what the cases move the *daemon's* uid relative to
    /// rather than trying to chown their way around.
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

    /// Fails if the walk stops reporting the NEAREST offender — an
    /// implementation that walked to the filesystem root and kept the last
    /// hit, or that returned an arbitrary one, would blame `/tmp` on a Linux
    /// runner (mode `1777`) instead of the directory the operator configured
    /// and can actually fix. Also the only case pinning the world-writable
    /// arm on its own: drop that arm and this reddens with `None` while the
    /// ownership cases below stay green.
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

    /// Fails if the check tests only the write bit — the shape the guard was
    /// first specified as. This parent is `0700`, so a world-writable test
    /// waves it through; its OWNER can still replace it under a root
    /// shepherd, which is the whole point of widening the predicate. A `0755`
    /// directory owned by an app's own dropped-privilege `user` is the same
    /// case wearing ordinary clothes.
    #[test]
    fn an_ancestor_owned_by_another_user_is_loose_however_tight_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (parent, log) = log_path_under(&dir, 0o700);

        // The predicate is symmetric in the two uids, so an unprivileged
        // runner moves the daemon's rather than the directory's — it cannot
        // chown. A root runner must move the directory's instead: everything
        // it creates is root-owned, and root is exempt whatever the daemon's
        // uid is.
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

    /// Fails if the walk reads each component with `metadata` instead of
    /// `symlink_metadata`. The link below is owned by this user and points at
    /// a root-owned, non-world-writable directory, so following it reports a
    /// component that is NOT loose and the walk moves on to blame the
    /// tempdir; reading the link itself blames the link. Only the path in the
    /// answer tells the two apart, which is why this asserts on it.
    ///
    /// This is the case `O_NOFOLLOW` structurally cannot cover: it guards the
    /// final component, and the redirect here is one level up.
    #[test]
    fn a_symlinked_component_is_judged_as_the_link_not_as_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("logs");
        // `/usr` exists and is root-owned `0755` on both tier-1 platforms —
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

    /// Fails if the two uid arms are collapsed either way round.
    ///
    /// Refusing everywhere would break a developer logging to `/tmp` as
    /// themselves — a footgun, not an escalation, since they have handed
    /// nobody anything they could not already do. Warning everywhere would
    /// leave the root case, the one that IS an escalation, exiting zero.
    ///
    /// The warn-once half is asserted in the same case because it is the same
    /// call: a count of two means the dedup set is gone, and a pump that
    /// reopens on every respawn would then repeat the line forever.
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

        // Never `me()`: a root test runner would take the arm above instead.
        // `me() + 1` is non-root by construction and owns nothing here, so
        // the fixture is loose to it whichever uid this process has.
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
}
