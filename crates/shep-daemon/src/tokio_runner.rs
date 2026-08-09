//! Real [`crate::runner::ProcessRunner`] over actual OS processes.
//!
//! `TokioRunner`'s spawn starts a `tokio::process::Command` in its own
//! process group (so both stop rungs —
//! [`crate::runner::RunningProcess::signal`] and
//! [`crate::runner::RunningProcess::kill_tree`] — can reach the whole group
//! without touching the daemon's own), optionally wires an fd-3
//! socketpair as the shepherd channel, and spawns background pump tasks that
//! drain stdout/stderr into the `logs` channel (and append them to the
//! spec's log files) and shuttle shepherd-channel JSON both ways.
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

use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use command_fds::{CommandFdExt, FdMapping};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::channel::{ChildMessage, ShepherdMessage};
use crate::runner::{
    ExitOutcome, LogLine, ProcIo, ProcessRunner, RunnerError, RunningProcess, SpawnSpec, StopSignal,
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
        if let Some(stdout) = child.stdout.take() {
            spawn_log_pump(stdout, false, spec.out_file.clone(), logs_tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_pump(stderr, true, spec.err_file.clone(), logs_tx);
        }

        let io = ProcIo {
            logs: logs_rx,
            from_child: from_child_rx,
            to_child: to_child_tx,
        };
        Ok((TokioProc { pid, child }, io))
    }
}

/// Pumps one stdout/stderr stream to completion: every line is appended to
/// `file_path` (parent directories created as needed) and then forwarded on
/// `logs_tx` — in that order, so a receiver that observes a line on the
/// channel can rely on that line's file write having already landed.
///
/// Runs until the stream hits EOF (normally the child exiting) or the owning
/// sheep task drops its `logs` receiver.
fn spawn_log_pump<R>(reader: R, err: bool, file_path: PathBuf, logs_tx: mpsc::Sender<LogLine>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut file = open_append(&file_path).await;
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Some(file) = file.as_mut() {
                        let mut buf = line.clone();
                        buf.push('\n');
                        if let Err(error) = file.write_all(buf.as_bytes()).await {
                            tracing::error!(?file_path, %error, "log file append failed");
                        }
                    }
                    if logs_tx.send(LogLine { err, line }).await.is_err() {
                        break; // owning sheep task dropped its logs receiver
                    }
                }
                Ok(None) => break, // stream closed, normally the child exiting
                Err(error) => {
                    tracing::error!(?file_path, %error, "log stream read failed");
                    break;
                }
            }
        }
    });
}

/// Opens `path` for appending, creating its parent directory first.
///
/// Returns `None` on any I/O failure instead of erroring the whole pump: a
/// log file we can't create shouldn't stop us draining the child's
/// stdout/stderr — leaving that unread risks the child stalling on a full
/// pipe once its own stdout buffer backs up.
async fn open_append(path: &Path) -> Option<tokio::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = tokio::fs::create_dir_all(parent).await
    {
        tracing::error!(?path, %error, "log directory create failed");
        return None;
    }

    match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        Ok(file) => Some(file),
        Err(error) => {
            tracing::error!(?path, %error, "log file open failed");
            None
        }
    }
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
    use super::{Signal, signal_group};

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
