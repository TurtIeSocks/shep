//! Real [`crate::runner::ProcessRunner`] over actual OS processes.
//!
//! `TokioRunner`'s spawn starts a `tokio::process::Command` in its own
//! process group (so both stop rungs —
//! [`crate::runner::RunningProcess::signal`] and
//! [`crate::runner::RunningProcess::kill_tree`] — can reach the whole group
//! without touching the daemon's own), optionally wires an fd-3
//! socketpair as the shepherd channel, and spawns background pump tasks that
//! drain stdout/stderr into the `logs` channel (and append them to the
//! spec's log files) and shuttle shepherd-channel JSON both ways. The log
//! pump also takes [`LogCtl`](crate::runner::LogCtl) messages, one per thing
//! `shep` can ask of a live log file without restarting the sheep:
//! [`Reopen`](crate::runner::LogCtl::Reopen) drops both handles and opens the
//! paths again, which is how an externally rotated file gets picked up, and
//! [`Flush`](crate::runner::LogCtl::Flush) waits for the writes already in
//! flight to land, which is the barrier `shep flush` truncates behind.
//!
//! # Shepherd-channel fd lifecycle
//!
//! The child's end of the `UnixStream::pair()` is handed to the child via
//! [`command_fds::FdMapping`] as fd 3. That mapping captures the fd inside a
//! `pre_exec` closure owned by the `Command`, so the parent process's extra
//! reference to the same fd stays open until the `Command` itself is
//! dropped — done explicitly, immediately after `spawn()`, so the daemon's
//! side of the channel sees a clean EOF once the child closes or exits
//! rather than being kept artificially open by our own leftover reference.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use command_fds::{CommandFdExt, FdMapping};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWriteExt as _, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::boot::DIR_MODE;
use crate::channel::{ChildMessage, ShepherdMessage};
use crate::runner::{
    ExitOutcome, FlushError, LogCtl, LogLine, ProcIo, ProcessRunner, ReopenError, RunnerError,
    RunningProcess, SpawnSpec, StopSignal,
};

/// Capacity of every channel a spawn wires up — generous enough that a
/// bursty child doesn't back-pressure against a sheep task that's merely
/// slow to poll, without buffering unboundedly.
const CHANNEL_CAPACITY: usize = 32;

/// Real [`crate::runner::ProcessRunner`] over actual OS processes.
#[derive(Debug, Default)]
pub struct TokioRunner;

impl TokioRunner {
    /// Builds a runner that spawns real OS child processes.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// A live real OS child, produced by [`TokioRunner`]'s
/// [`crate::runner::ProcessRunner::spawn`].
///
/// Public because it is [`TokioRunner`]'s [`ProcessRunner::Proc`], and an
/// associated type cannot be less visible than the trait impl that names it.
#[derive(Debug)]
pub struct TokioProc {
    /// Captured once at spawn time — `tokio::process::Child::id` reports
    /// `None` after the child has been waited to completion, but callers
    /// (e.g. a kill ladder) may still need the pid after that point.
    pid: u32,
    child: Child,
}

impl RunningProcess for TokioProc {
    fn pid(&self) -> u32 {
        self.pid
    }

    async fn wait(&mut self) -> ExitOutcome {
        // tokio::process::Child::wait is documented cancel-safe (repeat
        // calls, or calls after a dropped-mid-flight future, replay the
        // cached result instead of restarting) — RunningProcess::wait's own
        // cancel-safety contract is inherited directly from it, no extra
        // latching needed on our side (contrast the scripted fake, which
        // has to hand-roll that latch itself).
        match self.child.wait().await {
            Ok(status) => ExitOutcome {
                code: status.code(),
                signal: status.signal(),
            },
            Err(error) => {
                // Only reachable if the wait4() syscall itself fails (e.g.
                // something outside our control reaped the pid first).
                // RunningProcess::wait has no error variant, so this reports
                // a degenerate terminal outcome instead of hanging forever.
                tracing::error!(pid = self.pid, %error, "process wait() failed");
                ExitOutcome {
                    code: None,
                    signal: None,
                }
            }
        }
    }

    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError> {
        signal_group(self.pid, to_nix_signal(sig))
    }

    fn kill_tree(&mut self) -> Result<(), RunnerError> {
        signal_group(self.pid, Signal::SIGKILL)
    }
}

// Both stop rungs go through one function because they differ only in which
// signal they send, never in who they reach: the graceful stop is group-wide
// for exactly the reason the escalated SIGKILL always was — a wrapper script
// that forks without exec'ing (`thing & wait`) leaves its child in the
// sheep's group, and a leader-only signal kills the wrapper while that child
// runs on, orphaned and no longer tracked by anything.
//
// The exec prober's timeout path (`probes/os.rs`) is the third caller, for
// the third instance of that same shape: a probe command that forks leaves
// the fork behind when `kill_on_drop` reaches only the `sh` above it. It is
// `pub(crate)` for that caller alone — a fourth copy of `kill(-pid)` is what
// this function exists to prevent.
//
// # What `-pid` assumes, and what breaks if it stops holding
//
// `command.process_group(0)` in `spawn` below makes each child the leader of
// a fresh group whose pgid equals its pid, which is what makes `-pid` name
// that group and nothing else. `tests/real_runner.rs` asserts the property
// against a real spawn rather than trusting the flag.
//
// The sheep's own leader cannot leave that group by accident: `setsid` fails
// `EPERM` for a process that is already a group leader, and `setpgid(0, 0)`
// is a no-op for one. A descendant can — the classic daemonize dance is
// fork-then-`setsid`, and a grandchild that does it lands in a session of its
// own that neither rung reaches. That is a limit of process groups, not of
// this choice: it was equally true of `kill_tree` before the graceful stop
// joined it here, and escaping supervision is what that dance is for.
//
// If a future runner spawns WITHOUT `process_group(0)`, the failure is a
// no-op rather than a misfire. `-pid` names the group led by `pid`; a live
// child that is not a group leader has no such group, since a pgid is only
// ever a live leader's own pid and pids are unique — so the call returns
// `ESRCH` and `kill_process`'s ladder logs it and falls through to the
// timeout. It can never reach the daemon's own group, whose pgid is the
// daemon's pid and therefore not this child's.
/// Sends `sig` to the whole process group led by `pid`.
///
/// # Errors
///
/// [`RunnerError::SignalFailed`] — `pid` is not a signallable process id, or
/// the `kill(2)` itself failed (typically `ESRCH`: no group led by `pid`).
pub(crate) fn signal_group(pid: u32, sig: Signal) -> Result<(), RunnerError> {
    // Rejecting 0 is not a range formality: `kill(0, ...)` means "every
    // process in the CALLER's group" — the daemon itself — and `-0` is `0`,
    // so a zero pid must never reach the syscall. `spawn` only ever records a
    // pid the OS reported for a live child, which is never 0, so this guards
    // a future refactor rather than a reachable state today.
    let pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            RunnerError::SignalFailed(format!("pid {pid} is not a signallable process id"))
        })?;
    signal::kill(Pid::from_raw(-pid), sig)
        .map_err(|error| RunnerError::SignalFailed(error.to_string()))
}

/// Maps [`StopSignal`] to the nix [`Signal`] it names.
///
/// Kept as an explicit match (rather than `Signal::try_from(sig.as_raw())`)
/// so an unsupported raw number is a compile error, not a runtime one.
fn to_nix_signal(sig: StopSignal) -> Signal {
    match sig {
        StopSignal::Term => Signal::SIGTERM,
        StopSignal::Int => Signal::SIGINT,
        StopSignal::Quit => Signal::SIGQUIT,
        StopSignal::Usr2 => Signal::SIGUSR2,
        StopSignal::Kill => Signal::SIGKILL,
    }
}

impl ProcessRunner for TokioRunner {
    type Proc = TokioProc;

    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        // SpawnSpec::env is documented fully-resolved with "no daemon-env
        // leakage beyond this map" — env_clear() before envs() is what makes
        // that contract actually hold, since Command inherits the daemon's
        // ambient env by default otherwise.
        command.env_clear();
        command.envs(&spec.env);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        // New process group rooted at the child itself, so kill_tree's
        // negative-pid SIGKILL reaches it and its descendants without also
        // reaching the daemon's own group.
        command.process_group(0);

        if let Some(creds) = spec.credentials {
            // std sets the gid before the uid in the child (setgid must
            // happen while still privileged), which is the order privilege
            // drop requires.
            //
            // Supplementary groups: we never call `CommandExt::groups`
            // (still unstable), but that does NOT leave the daemon's own
            // supplementary groups inherited by the child. Verified against
            // std's own do_exec (sys/process/unix/unix.rs, this MSRV):
            // whenever `.uid()` is set and `.groups()` was never called,
            // std unconditionally calls `setgroups(0, NULL)` before
            // `setuid()`, clearing every supplementary group before the
            // child ever runs. The only gap `CommandExt::groups` stabilizing
            // would close is choosing a NON-EMPTY custom group set (e.g.
            // the target user's own supplementary groups from
            // `/etc/group`) — today's child always gets zero supplementary
            // groups plus whatever `gid` below sets as its single group,
            // which is the safe direction to be wrong in for a privilege
            // drop, not the dangerous one.
            if let Some(gid) = creds.gid {
                command.gid(gid);
            }
            command.uid(creds.uid);
        }

        let (from_child_tx, from_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (to_child_tx, to_child_rx) = mpsc::channel(CHANNEL_CAPACITY);

        if spec.channel {
            command.env("SHEP_CHANNEL_FD", "3");
            let (daemon_end, child_end) = UnixStream::pair().map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel socketpair: {error}"))
            })?;
            let std_child_end = child_end.into_std().map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel into_std: {error}"))
            })?;
            let child_fd = OwnedFd::from(std_child_end);
            command
                .as_std_mut()
                .fd_mappings(vec![FdMapping {
                    parent_fd: child_fd,
                    child_fd: 3,
                }])
                .map_err(|error| {
                    RunnerError::SpawnFailed(format!("shepherd channel fd mapping: {error}"))
                })?;
            spawn_channel_pumps(daemon_end, from_child_tx, to_child_rx);
        } else {
            // No channel requested: close both ends immediately rather than
            // leaving them dangling — a `from_child.recv()` reports closed
            // right away instead of hanging, and a stray `to_child.send()`
            // fails fast instead of silently buffering into a channel
            // nobody will ever drain.
            drop(from_child_tx);
            drop(to_child_rx);
        }

        let mut child = command
            .spawn()
            .map_err(|error| RunnerError::SpawnFailed(error.to_string()))?;
        // See module docs: drop the Command now so the parent's copy of the
        // fd-3 socketpair end (owned by its pre_exec closure) closes here,
        // not whenever `command` would otherwise have gone out of scope.
        drop(command);

        let pid = child.id().ok_or_else(|| {
            RunnerError::SpawnFailed("child exited before its pid could be read".to_string())
        })?;

        let (logs_tx, logs_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (log_ctl_tx, log_ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
        spawn_log_pump(
            child.stdout.take(),
            child.stderr.take(),
            spec.out_file.clone(),
            spec.err_file.clone(),
            logs_tx,
            log_ctl_rx,
        );

        let io = ProcIo {
            logs: logs_rx,
            from_child: from_child_rx,
            to_child: to_child_tx,
            log_ctl: log_ctl_tx,
        };
        Ok((TokioProc { pid, child }, io))
    }
}

/// One stream's log file: the path the spec named, plus the handle currently
/// open on it — `None` when the open failed (see [`open_append`]).
#[derive(Debug)]
struct LogFile {
    path: PathBuf,
    handle: Option<tokio::fs::File>,
}

impl LogFile {
    /// Opens `path` for appending, keeping the path for later reopens.
    ///
    /// A failed open is not fatal here — it is already logged, and the pump
    /// must still drain the child's streams whether or not it can write
    /// them anywhere. [`LogFile::reopen`] is the one that reports, because
    /// there a caller is waiting to hear.
    async fn open(path: PathBuf) -> Self {
        let handle = open_append(&path).await.ok();
        Self { path, handle }
    }

    /// Appends one line and its newline, logging (never propagating) a write
    /// failure — a log we cannot write to must not stop the pump draining
    /// the child's pipes.
    async fn append(&mut self, line: &str) {
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        if let Err(error) = handle.write_all(buf.as_bytes()).await {
            tracing::error!(path = ?self.path, %error, "log file append failed");
        }
    }

    /// Waits for every write already handed to the blocking pool to reach
    /// the file, keeping the handle open.
    ///
    /// The whole of [`LogCtl::Flush`], and the reason `shep flush` has two
    /// halves: `write_all` returns once the real `write(2)` is queued, so
    /// truncating the path without waiting here can empty the file a moment
    /// before a line that was already in flight lands at offset 0 of it.
    ///
    /// A stream whose open failed has no handle and nothing queued, so it
    /// has nothing to wait for and answers `Ok`.
    ///
    /// # Errors
    ///
    /// A write already dispatched failed — a full disk, an unlinked
    /// filesystem, an IO error the queued `write(2)` hit. Unlike
    /// [`Self::reopen`]'s own flush this is reported rather than logged:
    /// there no caller depends on the result (the handle is being replaced
    /// by a working one), while here the caller is about to truncate this
    /// exact path and the un-landed bytes are what it is racing.
    async fn flush(&mut self) -> Result<(), FlushError> {
        let Some(handle) = self.handle.as_mut() else {
            return Ok(());
        };
        handle.flush().await.map_err(|error| FlushError {
            message: format!("{}: {error}", self.path.display()),
        })
    }

    /// Flushes and closes the current handle, then opens the path again.
    ///
    /// Flushing first is what makes [`LogCtl::Reopen`]'s acknowledgement
    /// worth having: `write_all` returning only means the write was queued
    /// onto the blocking pool, while `flush` waits for the operation in
    /// flight — so every line read before the reopen has reached the OLD
    /// file (the renamed one, in the rotation this exists for) by the time
    /// the caller hears back.
    ///
    /// Reopening goes through [`open_append`] rather than opening the path
    /// here, so the new handle is an appending one exactly like the original
    /// — see that function for why `O_APPEND` is load-bearing.
    ///
    /// # Errors
    ///
    /// The path could not be opened again — a directory that no longer
    /// exists, a mode the daemon cannot write, a full disk. The old handle
    /// is closed regardless, so the rotator's rename is safe to act on; what
    /// is lost is this stream's file, and every line it would have taken
    /// until something reopens it successfully. A flush that fails does NOT
    /// error: it is logged, and the handle it belongs to is being replaced
    /// by a working one, so the sheep keeps logging.
    async fn reopen(&mut self) -> Result<(), ReopenError> {
        if let Some(handle) = self.handle.as_mut()
            && let Err(error) = handle.flush().await
        {
            tracing::error!(path = ?self.path, %error, "log file flush failed");
        }
        // Closed before the reopen, so the pump never holds two descriptors
        // on one log at the same time.
        drop(self.handle.take());
        match open_append(&self.path).await {
            Ok(handle) => {
                self.handle = Some(handle);
                Ok(())
            }
            Err(error) => Err(ReopenError {
                message: format!("{}: {error}", self.path.display()),
            }),
        }
    }
}

/// Both of a sheep's log files — the pair one [`LogCtl::Reopen`] swaps, and
/// the pair one [`LogCtl::Flush`] drains.
#[derive(Debug)]
struct LogFiles {
    out: LogFile,
    err: LogFile,
}

impl LogFiles {
    /// The file a line from this stream is appended to (`err` picks stderr).
    fn stream(&mut self, err: bool) -> &mut LogFile {
        if err { &mut self.err } else { &mut self.out }
    }

    /// Carries out one control request and then answers it.
    ///
    /// The acknowledgement is the last statement by construction, after BOTH
    /// streams have been dealt with: a caller that has heard back knows both
    /// handles were swapped, or that neither has a write left in flight,
    /// which is what a rotator that renamed both files — or a truncate about
    /// to empty both paths — is waiting on. One request, one answer —
    /// nothing here can send twice.
    ///
    /// stderr is served even when stdout's turn just failed. The two files
    /// are independent, and the stream that CAN be reopened is no less owed
    /// its handle because the other one cannot; short-circuiting would take
    /// a sheep's working half offline over the broken one, and would leave
    /// stderr's queued bytes racing a truncate that stdout's failure never
    /// stopped. Both failures then travel together, so an operator is told
    /// about both paths at once rather than one per rotation.
    ///
    /// Those two travel joined by `", "`, and the separator is chosen against
    /// the level above rather than for its own sake: the supervisor joins ONE
    /// OF THESE PER SHEEP (reopen) or PER PATH (flush) with `"; "`, so one
    /// separator at both levels would punctuate a single sheep that failed on
    /// both streams exactly like two that failed on one each.
    async fn serve(&mut self, ctl: LogCtl) {
        match ctl {
            LogCtl::Reopen { done } => {
                let mut failures = Vec::new();
                if let Err(error) = self.out.reopen().await {
                    failures.push(error.message);
                }
                if let Err(error) = self.err.reopen().await {
                    failures.push(error.message);
                }
                let result = if failures.is_empty() {
                    Ok(())
                } else {
                    Err(ReopenError {
                        message: failures.join(", "),
                    })
                };
                // A caller that stopped waiting is not a failure: the reopen
                // happened either way.
                let _ = done.send(result);
            }
            LogCtl::Flush { done } => {
                let mut failures = Vec::new();
                if let Err(error) = self.out.flush().await {
                    failures.push(error.message);
                }
                if let Err(error) = self.err.flush().await {
                    failures.push(error.message);
                }
                let result = if failures.is_empty() {
                    Ok(())
                } else {
                    Err(FlushError {
                        message: failures.join(", "),
                    })
                };
                // Same as above: the flush happened either way. A caller that
                // stopped waiting is one whose deadline expired, and it will
                // not truncate anything on the strength of an answer it never
                // read.
                let _ = done.send(result);
            }
        }
    }
}

/// What the pump does after handling one line result.
enum AfterLine {
    /// The stream is live; keep reading it.
    KeepReading,
    /// The stream reached EOF or failed; stop reading THIS stream.
    StreamEnded,
    /// The owning sheep task dropped its `logs` receiver; stop entirely.
    LogsClosed,
}

/// Handles one line read from a stream: appends it to that stream's file,
/// forwards it on `logs_tx`, and reports what the pump should do next.
///
/// The wait for room on `logs_tx` keeps serving `ctl_rx` — see
/// [`reserve_slot`] for the cycle that would otherwise close.
async fn deliver_line(
    result: io::Result<Option<String>>,
    err: bool,
    files: &mut LogFiles,
    logs_tx: &mpsc::Sender<LogLine>,
    ctl_rx: &mut mpsc::Receiver<LogCtl>,
) -> AfterLine {
    match result {
        Ok(Some(line)) => {
            files.stream(err).append(&line).await;
            let Some(slot) = reserve_slot(logs_tx, files, ctl_rx).await else {
                return AfterLine::LogsClosed;
            };
            slot.send(LogLine { err, line });
            AfterLine::KeepReading
        }
        Ok(None) => AfterLine::StreamEnded, // normally the child exiting
        Err(error) => {
            tracing::error!(path = ?files.stream(err).path, %error, "log stream read failed");
            AfterLine::StreamEnded
        }
    }
}

/// Waits for room on `logs_tx`, serving control requests while it waits.
///
/// Returns `None` once the `logs` receiver is gone.
///
/// # Why not a bare `send().await`
///
/// A `select!` handler is not cancellable, so anything awaited inside one
/// stops the pump polling its control channel for as long as the await
/// lasts. A wait for room on `logs` is unbounded, and the party that makes
/// that room is the sheep task — the same party a reopen's acknowledgement
/// travels back to. Sending from inside the handler therefore lets a full
/// `logs` channel close a cycle: nothing drains `logs` until the sheep task
/// runs, the sheep task waits on the acknowledgement, and the pump cannot
/// look at the request that would produce it. Serving control requests from
/// inside the wait breaks that cycle by construction rather than by timing.
async fn reserve_slot<'tx>(
    logs_tx: &'tx mpsc::Sender<LogLine>,
    files: &mut LogFiles,
    ctl_rx: &mut mpsc::Receiver<LogCtl>,
) -> Option<mpsc::Permit<'tx, LogLine>> {
    loop {
        // Both branches are documented cancel-safe, as `select!` requires: a
        // `reserve` that loses the race has taken no slot, and a `recv` that
        // loses it has taken no message.
        tokio::select! {
            slot = logs_tx.reserve() => return slot.ok(),
            ctl = ctl_rx.recv() => match ctl {
                Some(ctl) => files.serve(ctl).await,
                // Nothing can reach the pump any more, but the line in hand
                // is still owed to the receiver. Waiting for it outside the
                // `select!` rather than looping avoids spinning on a closed
                // receiver, which is ready on every poll; the pump's own
                // loop then sees the same closed channel and ends.
                None => return logs_tx.reserve().await.ok(),
            },
        }
    }
}

/// The next line from an optional stream, or a future that never resolves
/// once there is no stream left to read.
///
/// The pump's `select!` needs a branch it can leave in place after a stream
/// ends: a ready `None` would be re-selected on every poll and spin the
/// loop, while pending forever drops the branch out of contention and lets
/// the loop's own condition end the task once both streams are gone.
///
/// Cancel-safe, as a `select!` branch must be:
/// [`tokio::io::Lines::next_line`] documents itself so — a partially read
/// line stays in the `Lines` buffer instead of being lost when another
/// branch wins the race.
async fn next_line<R>(lines: &mut Option<Lines<BufReader<R>>>) -> io::Result<Option<String>>
where
    R: AsyncRead + Unpin,
{
    match lines {
        Some(lines) => lines.next_line().await,
        None => core::future::pending().await,
    }
}

/// Pumps a sheep's stdout and stderr to completion, and stays reachable the
/// whole time it does.
///
/// Every line is appended to its stream's file (parent directories created
/// as needed) and then forwarded on `logs_tx`. A [`LogCtl`] message is
/// served between lines; while no line is flowing at all, which is the point
/// of the `select!`, since a sheep that has been quiet for hours has no next
/// line for a request to ride along with; and while the pump is waiting for
/// room on `logs_tx`, which is [`reserve_slot`]'s reason for existing.
///
/// # What still bounds a reopen
///
/// Not the party draining `logs`, deliberately — that is the cycle
/// [`reserve_slot`] exists to rule out. What does bound it is the pump's own
/// file I/O, because a request is only looked at between awaits: a reopen
/// waits behind the line append in flight, and behind any earlier reopen's
/// `create_dir_all` and `open`. Those are the same syscalls every spawn
/// already trusts, but a reopen repeats them mid-flight and on demand, and
/// it runs them while the pump is reading neither pipe. On a wedged
/// filesystem that stalls the acknowledgement, and the child's stdout and
/// stderr with it, for as long as the kernel takes; neither side has a
/// timeout.
///
/// # Why the reopen never disturbs the child
///
/// The child never sees the log file. It is spawned with `Stdio::piped()`
/// and this task does the file I/O on the far side of that pipe, so swapping
/// the handle here is invisible across the process boundary: no signal to
/// the child, no fd surgery, no restart, and no gap in the pipe. Nothing
/// child-side is needed to rotate a sheep's logs.
///
/// # Why one task for both streams
///
/// One [`LogCtl::Reopen`] swaps BOTH files and answers once, which is what a
/// rotator that renamed both of them is waiting on. The alternatives cost
/// more for less: an `mpsc::Receiver` cannot be split across two tasks, so
/// one channel feeding two pumps would hand each request to whichever pump
/// woke first and strand the other on the old inode, and a channel per pump
/// would need a third task to fan one request out and join two
/// acknowledgements. The two streams already share one `logs_tx`, so the
/// only isolation merging them costs is a slow write on one stream delaying
/// the other's read.
///
/// # Ordering
///
/// The file write is ISSUED before the line is forwarded, but
/// `tokio::fs::File::write_all` returning means the write was queued onto
/// the blocking pool, not that it reached the file. A receiver that observes
/// a line on `logs_tx` therefore cannot conclude the file already holds it.
/// The barrier that can be relied on is [`LogCtl::Reopen`]'s
/// acknowledgement, which flushes before swapping handles.
///
/// # When a pump ends
///
/// At both streams reaching EOF (normally the child exiting), when the
/// owning sheep task drops its `logs` receiver, or when the last `log_ctl`
/// sender anywhere drops. Each is a branch of its own, so none of them needs
/// a line to arrive before it is noticed.
///
/// The `logs` receiver is the one that reaps a pump whose child has forked a
/// lamb that inherited the pipe: neither stream reaches EOF while the lamb
/// lives, and the supervisor holds a `log_ctl` clone for as long as the
/// sheep is registered (`SheepSlot::log_ctl`), so the other two conditions
/// can wait indefinitely.
fn spawn_log_pump<O, E>(
    stdout: Option<O>,
    stderr: Option<E>,
    out_path: PathBuf,
    err_path: PathBuf,
    logs_tx: mpsc::Sender<LogLine>,
    mut ctl_rx: mpsc::Receiver<LogCtl>,
) where
    O: AsyncRead + Unpin + Send + 'static,
    E: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut files = LogFiles {
            out: LogFile::open(out_path).await,
            err: LogFile::open(err_path).await,
        };
        let mut out_lines = stdout.map(|reader| BufReader::new(reader).lines());
        let mut err_lines = stderr.map(|reader| BufReader::new(reader).lines());

        while out_lines.is_some() || err_lines.is_some() {
            tokio::select! {
                result = next_line(&mut out_lines) => {
                    match deliver_line(result, false, &mut files, &logs_tx, &mut ctl_rx).await {
                        AfterLine::KeepReading => {}
                        AfterLine::StreamEnded => out_lines = None,
                        AfterLine::LogsClosed => break,
                    }
                }
                result = next_line(&mut err_lines) => {
                    match deliver_line(result, true, &mut files, &logs_tx, &mut ctl_rx).await {
                        AfterLine::KeepReading => {}
                        AfterLine::StreamEnded => err_lines = None,
                        AfterLine::LogsClosed => break,
                    }
                }
                ctl = ctl_rx.recv() => {
                    match ctl {
                        Some(ctl) => files.serve(ctl).await,
                        None => break, // nothing holds a `log_ctl` sender
                    }
                }
                // The owning sheep task dropped its `logs` receiver. Its own
                // branch rather than a check folded into the line handling,
                // because a pump whose child writes nothing has no next line
                // to notice it on — a forked lamb holding the pipe open past
                // the child's exit is exactly that pump, and without this it
                // would keep two files and two pipe read ends open until the
                // sheep is deleted or the daemon exits.
                //
                // Documented cancel-safe, as a `select!` branch must be: a
                // closed channel stays closed, so losing the race loses
                // nothing.
                () = logs_tx.closed() => break,
            }
        }
    });
}

/// Opens `path` for appending, creating its parent directory at
/// [`DIR_MODE`] first.
///
/// Every failure is logged here, where the two causes can still be told
/// apart, and returned as well: [`LogFile::open`] discards it (a log file we
/// cannot create must not stop us draining the child's stdout/stderr —
/// leaving that unread risks the child stalling on a full pipe once its own
/// buffer backs up), while [`LogFile::reopen`] has a caller waiting to hear
/// whether the rotated path came back.
///
/// # Errors
///
/// The parent directory could not be created, or the file itself could not
/// be opened.
///
/// # Why the mode is asked for at creation
///
/// [`DIR_MODE`] is the mode every directory shep creates gets, and this is
/// the only place the daemon creates one once its boot layout is already
/// there. A rotator that moved or removed the log DIRECTORY rather than the
/// files leaves the next open to put it back, and a plain `create_dir_all`
/// would put it back at `0o777` narrowed by whatever the process umask
/// happens to strip — `0o755` under the common `umask 022`, world-writable
/// under `umask 0`.
/// Asking for the mode at `mkdir` time rather than chmod'ing afterwards is
/// `crate::boot::init_dirs`' discipline for the same reason it holds there:
/// `0o700` has no group or other bits for a umask to strip, so the directory
/// is never wider than `DIR_MODE`, not even for the instant between the two
/// calls.
///
/// This is the whole of that guarantee for a log directory, and it holds
/// however the reopen was asked for — `Request::Reopen` over the socket,
/// `SIGUSR2`, or the first open of a freshly spawned sheep. A directory that
/// already exists is left alone: the mode given to `mkdir` governs only the
/// directories a call actually creates, and re-tightening one that was
/// already there is `init_dirs`' boot-time pass, not this function's job.
///
/// An app whose `out_file` points outside the log directory gets the same
/// treatment for any parent shep has to create on its behalf, which is the
/// intended reading of `DIR_MODE` rather than an accident of where the call
/// sits.
///
/// `.append(true)` is load-bearing rather than a convenience. `O_APPEND`
/// makes every write seek to end atomically, which is what lets a
/// `copytruncate` rotator truncate the file under a live handle and have the
/// next line land at offset 0. A handle tracking its own offset instead
/// would write past the truncation point, leaving a sparse hole the size of
/// everything rotated away — and it would do so after every rotation, so the
/// files would grow without bound. This holds for a reopened handle as much
/// as for the first one, which is why [`LogFile::reopen`] comes back through
/// here.
async fn open_append(path: &Path) -> io::Result<tokio::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = tokio::fs::DirBuilder::new()
            .mode(DIR_MODE)
            .recursive(true)
            .create(parent)
            .await
    {
        tracing::error!(?path, %error, "log directory create failed");
        return Err(error);
    }

    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .inspect_err(|error| tracing::error!(?path, %error, "log file open failed"))
}

/// Wires the daemon side of the shepherd channel: a reader task decodes
/// newline-JSON [`ChildMessage`]s onto `from_child_tx`; a writer task encodes
/// [`ShepherdMessage`]s taken from `to_child_rx` back onto the socket.
fn spawn_channel_pumps(
    daemon_end: UnixStream,
    from_child_tx: mpsc::Sender<ChildMessage>,
    mut to_child_rx: mpsc::Receiver<ShepherdMessage>,
) {
    let (read_half, mut write_half) = daemon_end.into_split();

    tokio::spawn(async move {
        let mut lines = BufReader::new(read_half).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<ChildMessage>(&line) {
                    Ok(msg) => {
                        if from_child_tx.send(msg).await.is_err() {
                            break; // owning sheep task dropped from_child
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%line, %error, "malformed shepherd-channel frame");
                    }
                },
                Ok(None) => break, // child closed its end, normally at exit
                Err(error) => {
                    tracing::error!(%error, "shepherd-channel read failed");
                    break;
                }
            }
        }
    });

    tokio::spawn(async move {
        while let Some(msg) = to_child_rx.recv().await {
            let mut line = match serde_json::to_string(&msg) {
                Ok(json) => json,
                Err(error) => {
                    tracing::error!(%error, "shepherd message encode failed");
                    continue;
                }
            };
            line.push('\n');
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break; // child closed its end
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use tokio::io::DuplexStream;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;

    // What needs a real OS child lives in `tests/real_runner.rs`; what is
    // reachable with no process at all belongs here (IR-38). The log pump is
    // the second kind: it reads an `AsyncRead`, and a `tokio::io::duplex`
    // half is one, so both `LogCtl` variants can be driven from this tier
    // against real files and with no child to reap.
    //
    // One thing cannot: a write that fails. The pump opens its own handles,
    // so making one unwritable means reaching past it — which is why
    // `a_flush_reports_the_write_its_file_never_took` drives a `LogFile`
    // directly instead of a whole pump.
    //
    // These cases run on the REAL clock rather than this crate's usual paused
    // one (IR-33): they wait on actual file I/O dispatched to the blocking
    // pool, which no amount of virtual-time advance brings any closer.

    /// How long a pump gets to answer before a test calls it hung. A pump
    /// that is working answers in microseconds; this is slack for a loaded
    /// runner, not an expected duration.
    const PUMP_DEADLINE: Duration = Duration::from_secs(5);

    /// Room in each in-memory pipe standing in for a child's stdout/stderr.
    ///
    /// Sized so a case can hand the pump more lines than `logs` will hold
    /// without the writing side parking first — with a buffer that small,
    /// "the pump stopped reading" and "the test stopped writing" become the
    /// same observation.
    const STREAM_BUFFER: usize = 4096;

    /// One pump over two in-memory streams and two real files — everything
    /// [`spawn_log_pump`] takes, with no child process involved.
    struct PumpHarness {
        dir: tempfile::TempDir,
        out_path: PathBuf,
        err_path: PathBuf,
        out_writer: DuplexStream,
        err_writer: DuplexStream,
        logs: mpsc::Receiver<LogLine>,
        ctl: mpsc::Sender<LogCtl>,
    }

    impl PumpHarness {
        fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let out_path = dir.path().join("out.log");
            let err_path = dir.path().join("err.log");
            let (out_writer, out_reader) = tokio::io::duplex(STREAM_BUFFER);
            let (err_writer, err_reader) = tokio::io::duplex(STREAM_BUFFER);
            let (logs_tx, logs) = mpsc::channel(CHANNEL_CAPACITY);
            let (ctl, ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
            spawn_log_pump(
                Some(out_reader),
                Some(err_reader),
                out_path.clone(),
                err_path.clone(),
                logs_tx,
                ctl_rx,
            );
            Self {
                dir,
                out_path,
                err_path,
                out_writer,
                err_writer,
                logs,
                ctl,
            }
        }

        /// Writes one line into the chosen stream and waits for the pump to
        /// hand it back on `logs` — which is proof the pump read it and
        /// issued its file write, and orders the two streams against each
        /// other so a test never has to guess which line arrives first.
        async fn feed(&mut self, err: bool, line: &str) {
            let writer = if err {
                &mut self.err_writer
            } else {
                &mut self.out_writer
            };
            writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
            let observed = timeout(PUMP_DEADLINE, self.logs.recv())
                .await
                .expect("the pump must forward a line it has read")
                .expect("the pump must not end while its streams are open");
            assert_eq!(
                observed,
                LogLine {
                    err,
                    line: line.to_string()
                }
            );
        }

        /// Sends a [`LogCtl::Reopen`], waits for its acknowledgement, and
        /// requires that it reports success — every caller of this reopens
        /// paths the pump can open.
        async fn reopen(&self) {
            let outcome = self.reopen_for_answer().await;
            assert_eq!(outcome, Ok(()), "this reopen must have worked");
        }

        /// [`PumpHarness::reopen`] for the case where the answer itself is
        /// the assertion.
        async fn reopen_for_answer(&self) -> Result<(), ReopenError> {
            let (done, ack) = oneshot::channel();
            self.ctl
                .send(LogCtl::Reopen { done })
                .await
                .expect("the pump must still be reading its control channel");
            timeout(PUMP_DEADLINE, ack)
                .await
                .expect("a reopen must be acknowledged")
                .expect("the pump must answer rather than drop the acknowledgement")
        }

        /// Sends a [`LogCtl::Flush`], waits for its acknowledgement, and
        /// requires that it reports success — every caller of this flushes
        /// handles the pump can write.
        async fn flush(&self) {
            let (done, ack) = oneshot::channel();
            self.ctl
                .send(LogCtl::Flush { done })
                .await
                .expect("the pump must still be reading its control channel");
            let outcome = timeout(PUMP_DEADLINE, ack)
                .await
                .expect("a flush must be acknowledged")
                .expect("the pump must answer rather than drop the acknowledgement");
            assert_eq!(outcome, Ok(()), "this flush must have worked");
        }
    }

    /// Waits for `path` to hold exactly `expected`.
    ///
    /// A line observed on `logs` has had its file write ISSUED, not
    /// necessarily completed (see [`spawn_log_pump`]'s ordering note), so
    /// this polls for the write to land instead of asserting it already has.
    /// The barrier that would make polling unnecessary — a reopen
    /// acknowledgement — is not usable where the point is what the CURRENT
    /// handle does, since a reopen replaces it.
    async fn assert_file_settles(path: &Path, expected: &str) {
        let settled = timeout(PUMP_DEADLINE, async {
            while fs::read_to_string(path).unwrap_or_default() != expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "{}: expected {expected:?}, found {:?}",
            path.display(),
            fs::read_to_string(path)
        );
    }

    /// Fails if the `Reopen` arm acknowledges without opening the paths
    /// again: the renamed inodes keep receiving every later line and the
    /// live paths never come back — which is `create`-mode rotation silently
    /// producing an empty log forever.
    #[tokio::test]
    async fn a_reopen_moves_both_streams_onto_the_recreated_paths() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "before-out").await;
        pump.feed(true, "before-err").await;

        // The rotator's rename: the pump's handles now point at inodes that
        // answer to a different name, and the paths it was given are gone.
        let rotated_out = pump.dir.path().join("out.log.1");
        let rotated_err = pump.dir.path().join("err.log.1");
        fs::rename(&pump.out_path, &rotated_out).unwrap();
        fs::rename(&pump.err_path, &rotated_err).unwrap();
        assert!(
            !pump.out_path.exists(),
            "sanity: the rename really moved it"
        );

        pump.reopen().await;

        // No polling here: the acknowledgement is a real barrier, because
        // the reopen flushes the old handle before dropping it.
        assert_eq!(fs::read_to_string(&rotated_out).unwrap(), "before-out\n");
        assert_eq!(fs::read_to_string(&rotated_err).unwrap(), "before-err\n");
        assert_eq!(fs::read_to_string(&pump.out_path).unwrap(), "");
        assert_eq!(fs::read_to_string(&pump.err_path).unwrap(), "");

        pump.feed(false, "after-out").await;
        pump.feed(true, "after-err").await;
        pump.reopen().await; // second reopen, wanted here only as the flush

        assert_eq!(fs::read_to_string(&pump.out_path).unwrap(), "after-out\n");
        assert_eq!(fs::read_to_string(&pump.err_path).unwrap(), "after-err\n");
        // Both archives stopped growing the moment the handles were swapped.
        assert_eq!(fs::read_to_string(&rotated_out).unwrap(), "before-out\n");
        assert_eq!(fs::read_to_string(&rotated_err).unwrap(), "before-err\n");
    }

    /// Fails if the reopen opens the path without `.append(true)`: the
    /// handle would then carry its own offset across an external truncation
    /// and write the next line past a sparse hole, instead of at offset 0.
    #[tokio::test]
    async fn a_reopened_handle_still_appends_so_a_truncation_leaves_no_hole() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "first").await;
        fs::rename(&pump.out_path, pump.dir.path().join("out.log.1")).unwrap();
        pump.reopen().await;

        // Push the REOPENED handle's offset off zero. Without this the case
        // proves nothing: in a file nobody has written to, offset zero and
        // end-of-file are the same place, so an appending handle and a
        // positional one behave identically.
        pump.feed(false, "second").await;
        assert_file_settles(&pump.out_path, "second\n").await;

        // The copytruncate rotator: it copies the file elsewhere and
        // truncates this one in place, leaving the pump's handle open on the
        // same inode at size zero.
        fs::File::create(&pump.out_path).unwrap();
        assert_eq!(fs::metadata(&pump.out_path).unwrap().len(), 0);

        pump.feed(false, "third").await;
        assert_file_settles(&pump.out_path, "third\n").await;
    }

    /// Fails if the control channel is only consulted after a line arrives
    /// (a sequential `next_line().await` and then a check, rather than a
    /// `select!`): a sheep that has gone quiet would never reopen at all,
    /// which is the failure a pushed message exists to rule out.
    #[tokio::test]
    async fn a_reopen_is_answered_while_both_streams_are_idle() {
        let pump = PumpHarness::start();
        // The pump opens both paths as it starts, on its own task. This
        // first acknowledgement is only a barrier proving it got that far,
        // so the removal below cannot race the initial open.
        pump.reopen().await;

        // Deleted rather than renamed, so the reopen under test is the only
        // thing that could put these paths back. Not one byte has been
        // written to either stream, and none is written before the
        // acknowledgement.
        fs::remove_file(&pump.out_path).unwrap();
        fs::remove_file(&pump.err_path).unwrap();

        pump.reopen().await;

        assert!(pump.out_path.exists(), "stdout's path must be back");
        assert!(pump.err_path.exists(), "stderr's path must be back");
    }

    /// Fails if the pump waits for room on `logs` with a bare
    /// `logs_tx.send(...).await` inside the `select!` handler: a handler is
    /// not cancellable, so the control channel goes unpolled for as long as
    /// that wait lasts, and with nothing draining `logs` it lasts forever.
    ///
    /// One layer up this is the cycle `Actor::claim_manual` documents: the
    /// party that drains `logs` is the sheep task, so anything that makes
    /// the sheep task wait on an acknowledgement closes the loop — actor
    /// waiting on the ack, sheep task waiting on the actor, pump waiting on
    /// the sheep task.
    #[tokio::test]
    async fn a_reopen_is_answered_while_the_logs_channel_is_full() {
        let mut pump = PumpHarness::start();

        // One line more than `logs` can hold, and nothing here drains it:
        // the pump appends every line to the file, hands CHANNEL_CAPACITY of
        // them to the channel, and is left waiting for room for the last.
        let flooded = CHANNEL_CAPACITY + 1;
        let mut written = String::new();
        for n in 0..flooded {
            let line = format!("line-{n}\n");
            pump.out_writer.write_all(line.as_bytes()).await.unwrap();
            written.push_str(&line);
        }

        // The append comes before the send, so a file holding every line is
        // proof the pump has read the last one and is parked on its send —
        // which is what makes the reopen below land on a pump that is
        // already waiting rather than one that merely might.
        assert_file_settles(&pump.out_path, &written).await;

        // Deleted rather than renamed, so the reopen under test is the only
        // thing that could put these paths back.
        fs::remove_file(&pump.out_path).unwrap();
        fs::remove_file(&pump.err_path).unwrap();

        pump.reopen().await;

        assert!(pump.out_path.exists(), "stdout's path must be back");
        assert!(pump.err_path.exists(), "stderr's path must be back");

        // The line the pump was holding is owed to the receiver, not
        // dropped: serving the reopen must not have cost the send its place.
        for n in 0..flooded {
            let observed = timeout(PUMP_DEADLINE, pump.logs.recv())
                .await
                .expect("the pump must resume once `logs` has room")
                .expect("the pump must not end while its streams are open");
            assert_eq!(observed.line, format!("line-{n}"));
        }
    }

    /// Hands the runtime to the pump task and back.
    ///
    /// `#[tokio::test]` runs on a current-thread runtime, so yielding is
    /// what lets the pump run at all: dropping a duplex writer wakes its
    /// read with EOF, and retiring a stream from there has no await of its
    /// own, so the pump reaches its next park before this returns. The
    /// repeats are slack for a pump that wakes with other work already
    /// queued, not a race the count papers over.
    async fn let_the_pump_settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// Fails if the pump lets go of a stream's [`LogFile`] once that stream
    /// ends — the "free the file, nothing can be read into it now" tidy-up
    /// that looks like an obvious win. A sheep whose stdout closes while
    /// stderr runs on would then never get its stdout log back from a
    /// rotation, and nothing else here would notice: every other case
    /// reopens with both streams still live.
    #[tokio::test]
    async fn a_stream_that_has_ended_is_still_reopened() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "before-out").await;
        pump.feed(true, "before-err").await;

        // stdout at EOF with stderr still live: what a child that closes one
        // stream early leaves behind, and what the pump holds in between the
        // two EOFs of an ordinary exit.
        drop(pump.out_writer);
        let_the_pump_settle().await;

        // Deleted rather than renamed, so the reopen is the only thing that
        // could put either path back.
        fs::remove_file(&pump.out_path).unwrap();
        fs::remove_file(&pump.err_path).unwrap();

        let (done, ack) = oneshot::channel();
        pump.ctl
            .send(LogCtl::Reopen { done })
            .await
            .expect("a pump with one live stream still reads its control channel");
        let outcome = timeout(PUMP_DEADLINE, ack)
            .await
            .expect("a reopen must be acknowledged")
            .expect("the pump must answer rather than drop the acknowledgement");
        assert_eq!(
            outcome,
            Ok(()),
            "an ended stream's path is as openable as a live one's"
        );

        assert!(
            pump.out_path.exists(),
            "the ended stream's path must come back too"
        );
        assert!(
            pump.err_path.exists(),
            "the live stream's path must be back"
        );
    }

    /// Fails if a pump that has ended leaves its control channel reachable.
    /// The failed send is what [`ProcIo::log_ctl`] promises callers as the
    /// signal that the pump is already gone, and it is what lets a reopen
    /// aimed at a stopped sheep be a no-op rather than an error worth
    /// reporting to whoever asked for it.
    #[tokio::test]
    async fn a_send_fails_once_the_pump_has_ended() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "before-out").await;
        pump.feed(true, "before-err").await;

        // Both writers gone = both streams at EOF, which is what a child
        // exiting looks like from inside the pump.
        drop(pump.out_writer);
        drop(pump.err_writer);

        // The pump's `logs` sender drops with the task, so this `None` is a
        // bounded wait for the task to have finished rather than a guess
        // that it already has.
        let ended = timeout(PUMP_DEADLINE, pump.logs.recv())
            .await
            .expect("the pump must end once both streams reach EOF");
        assert!(ended.is_none(), "a pump that has ended sends nothing more");

        let (done, ack) = oneshot::channel();
        assert!(
            pump.ctl.send(LogCtl::Reopen { done }).await.is_err(),
            "a reopen aimed at an ended pump must fail to send"
        );
        // The rejected request takes its acknowledgement down with it, so a
        // caller that had already started awaiting one is told the same
        // thing rather than left pending.
        assert!(ack.await.is_err());
    }

    /// Fails if the pump acknowledges a reopen it could not carry out.
    ///
    /// A path the pump cannot open leaves that stream with no file at all
    /// and every later line dropped, so answering `Ok` tells a rotator its
    /// rotation worked while the sheep logs into nothing — the same silent
    /// failure a reopen exists to end, moved one layer up.
    ///
    /// A directory in the log's place is the failure with no permission
    /// games in it: `open(2)` on a directory fails for every uid, root
    /// included, so this cannot pass for the wrong reason on a privileged
    /// runner.
    #[tokio::test]
    async fn a_reopen_that_cannot_open_a_path_again_answers_with_the_failure() {
        let pump = PumpHarness::start();
        // A barrier proving both initial opens are done, so what follows
        // cannot race them.
        pump.reopen().await;

        // The rotator's rename, and then something in stdout's way. stderr
        // is merely deleted, so its own reopen is the only thing that can
        // put it back.
        fs::rename(&pump.out_path, pump.dir.path().join("out.log.1")).unwrap();
        fs::create_dir(&pump.out_path).unwrap();
        fs::remove_file(&pump.err_path).unwrap();

        let error = pump
            .reopen_for_answer()
            .await
            .expect_err("a reopen that could not open stdout's path must say so");
        assert!(
            error.message.contains(pump.out_path.to_str().unwrap()),
            "the failure must name the path it could not open: {error}"
        );
        assert!(
            !error.message.contains(pump.err_path.to_str().unwrap()),
            "stderr's path opened fine and must not be reported: {error}"
        );

        // The other half of the answer: a failed open on one stream must not
        // cost the other its handle. Without this the case would pass
        // against a `serve` that gave up at the first failure, taking a
        // sheep's working stream offline over its broken one.
        assert!(
            pump.err_path.exists(),
            "stderr must be reopened even though stdout's open failed"
        );
    }

    /// Fails if the `Flush` arm never reaches the files, never answers, or
    /// lets go of a handle the way the `Reopen` arm deliberately does.
    ///
    /// Nothing polls for the content: a flush that has answered is a flush
    /// whose files hold every line already handed to them — the same barrier
    /// the reopen cases lean on, from the variant that keeps its handle
    /// instead of replacing it. A `Flush` arm that dropped a handle would
    /// leave the last feed with nowhere to go: the pump would go on reading
    /// the stream and forwarding it while writing it nowhere, which is the
    /// quietest way this module can be wrong.
    ///
    /// What this case does NOT catch is a [`LogFile::flush`] that answers
    /// without asking its file. Nothing here would fail — it would only leave
    /// the content assertion racing the blocking pool, which on a write this
    /// small it wins. That leg needs a write that fails, which needs a handle
    /// the pump would never open;
    /// [`a_flush_reports_the_write_its_file_never_took`] is where it is
    /// pinned, and it is pinned deterministically.
    #[tokio::test]
    async fn a_flush_lands_both_streams_and_keeps_writing_afterwards() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "out-before").await;
        pump.feed(true, "err-before").await;

        pump.flush().await;

        // No polling: the acknowledgement is the barrier this half of `shep
        // flush` exists to provide, and a test that polled for the content
        // would pass against a pump that never provided it.
        assert_eq!(fs::read_to_string(&pump.out_path).unwrap(), "out-before\n");
        assert_eq!(fs::read_to_string(&pump.err_path).unwrap(), "err-before\n");

        // A flush keeps the handle, where a reopen replaces it — so the next
        // line appends to what is already there rather than starting a file.
        pump.feed(false, "out-after").await;
        assert_file_settles(&pump.out_path, "out-before\nout-after\n").await;
    }

    /// Fails if [`LogFile::flush`] stops asking the file anything — an early
    /// `return Ok(())`, or a `map_err` traded for `.ok()`.
    ///
    /// A `tokio::fs::File` reports a failed write on the NEXT operation
    /// rather than the one that failed: `write_all` returns as soon as the
    /// real `write(2)` is queued on the blocking pool, so that write's error
    /// is owed to whoever asks next. `flush` is who asks. This is the whole
    /// reason the flush half of `shep flush` reports where
    /// [`LogFile::reopen`]'s own flush logs — and a flush that answered
    /// without asking would swallow the only signal there is that a sheep's
    /// log went unwritten.
    ///
    /// Driven against a [`LogFile`] rather than through [`PumpHarness`]
    /// because the pump opens its own handles: a read-only one is the
    /// deterministic way to make a write fail, since `write(2)` on an
    /// `O_RDONLY` descriptor is `EBADF` for every uid, with no disk to fill
    /// and no mode to race.
    #[tokio::test]
    async fn a_flush_reports_the_write_its_file_never_took() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        fs::write(&path, "").unwrap();
        let mut log = LogFile {
            path: path.clone(),
            handle: Some(tokio::fs::File::open(&path).await.unwrap()),
        };

        // Swallowed by design — the pump must keep draining a child whose
        // log it cannot write — so the failure is still owed at the flush.
        log.append("never-lands").await;

        let error = log
            .flush()
            .await
            .expect_err("a write that never reached the file must fail the flush");
        assert!(
            error.message.starts_with(&format!("{}: ", path.display())),
            "the failure must name the file it belongs to: {error}"
        );
    }

    /// Fails if the pump can only notice a departed `logs` receiver from
    /// inside `deliver_line` — that is, if it has no `select!` branch of its
    /// own for it.
    ///
    /// Without that branch a pump ends on a line it cannot deliver, on both
    /// streams reaching EOF, or on its last control sender dropping. A child
    /// that forked a lamb holding its pipes open satisfies none of the
    /// three: the lamb keeps both streams from EOF whether or not it ever
    /// writes, and the supervisor keeps a control sender for as long as the
    /// sheep stays registered (`SheepSlot::log_ctl`). The pump task, both
    /// `LogFile` handles and both pipe read ends would then live until that
    /// sheep was deleted or the daemon exited.
    #[tokio::test]
    async fn dropping_the_logs_receiver_ends_a_pump_with_no_line_to_carry_the_news() {
        let pump = PumpHarness::start();
        // A barrier proving the pump is up and serving control requests, so
        // the drop below cannot race its initial open.
        pump.reopen().await;

        drop(pump.logs); // the sheep task returning

        // Both stream writers are still held, so neither stream is at EOF,
        // and `pump.ctl` is still alive, so the control channel is still
        // open from this side: the receiver above is the only thing that can
        // end this pump. `closed()` resolves when the pump's own `ctl_rx`
        // drops with its task, so this is a bounded wait for that rather
        // than a guess that it already happened.
        timeout(PUMP_DEADLINE, pump.ctl.closed())
            .await
            .expect("a pump whose `logs` receiver is gone must end");
    }

    /// Fails if the pump treats a closed control channel as nothing to do:
    /// `ProcIo::log_ctl` documents that dropping the sender ends the pump,
    /// and an arm that ignored the `None` would spin on it forever — a
    /// closed `mpsc::Receiver` is ready on every poll.
    #[tokio::test]
    async fn dropping_the_control_sender_ends_the_pump() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "still-here").await;

        drop(pump.ctl);

        let after = timeout(PUMP_DEADLINE, pump.logs.recv())
            .await
            .expect("the pump must end once nothing can control it");
        assert!(after.is_none(), "a pump that has ended sends nothing more");
    }

    // Everything else in this module needs a real OS child and lives in
    // `tests/real_runner.rs`; this one case is reachable with no process at
    // all, so it belongs here (IR-38).
    #[test]
    fn a_zero_pid_is_refused_before_it_can_reach_the_daemons_own_group() {
        // `SIGCONT`, not a lethal signal, deliberately: if `signal_group`'s
        // zero guard is ever deleted, this assertion must go RED rather than
        // take the test harness's own process group down with it.
        let err = signal_group(0, Signal::SIGCONT).unwrap_err();
        assert_eq!(
            err.to_string(),
            "signal delivery failed: pid 0 is not a signallable process id"
        );
    }
}
