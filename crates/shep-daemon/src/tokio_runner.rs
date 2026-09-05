//! Real [`crate::runner::ProcessRunner`] over actual OS processes.
//!
//! A spawn puts the child in its own process group, so
//! [`crate::runner::RunningProcess::signal`] and
//! [`crate::runner::RunningProcess::kill_tree`] reach the whole group without
//! touching the daemon's own. It optionally wires an fd-3 socketpair as the
//! shepherd channel, and spawns pumps that drain stdout/stderr, shuttle
//! channel JSON both ways, and serve [`LogCtl`](crate::runner::LogCtl).
//!
//! `command_fds::FdMapping` holds the parent's copy of the child's fd 3
//! inside the `Command`, so dropping the `Command` right after `spawn()`
//! gives the daemon's end a clean EOF at the child's exit.

use core::time::Duration;
use std::collections::HashMap;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

#[cfg(unix)]
use command_fds::{CommandFdExt, FdMapping};
#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;
use shep_core::signals::OperatorSignal;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, BufWriter, Lines,
};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};

#[cfg(unix)]
use crate::boot::DIR_MODE;
use crate::channel::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
#[cfg(unix)]
use crate::runner::{AdoptSpec, AdoptedReaper};
use crate::runner::{
    ExitOutcome, FlushError, LogCtl, LogLine, Preflight, ProcIo, ProcessRunner, ReopenError,
    RunnerError, RunningProcess, SpawnSpec, StdinWrite, StopSignal, check_log_ancestry,
    open_log_path,
};

/// Capacity of every channel a spawn wires up: enough that a bursty child
/// does not back-pressure a sheep task that is merely slow to poll, without
/// buffering unboundedly.
const CHANNEL_CAPACITY: usize = 32;

/// Bytes a log file buffers before the pump writes them through.
///
/// `tokio::fs::File` hands every `write` to the blocking pool: 32.8 us of
/// daemon CPU per line against 0.99 us for the `write(2)` under it. Batching
/// amortises one dispatch over a whole buffer.
const LOG_BUFFER: usize = 8 * 1024;

/// How long a line may sit in that buffer before the pump writes it out
/// anyway.
///
/// Timed from the oldest unflushed line, so a steady trickle cannot push the
/// deadline out indefinitely. It exists for the sheep that logs one line and
/// then goes quiet.
const IDLE_FLUSH: Duration = Duration::from_millis(50);

use shep_core::logstamp::stamp_into;

/// Bytes each stream's reader may hold ahead of the lines it has emitted.
///
/// Two bounds rest on it: the most a handover can strand in userspace, since
/// bytes taken off the pipe die with the image at the `execve`, and the most
/// [`drain_ready`] can write.
#[cfg(unix)]
const READ_BUFFER: usize = 8 * 1024;

/// How long the pump keeps reading after its sheep task has let go.
///
/// Without it [`spawn_log_pump`]'s `select!` can take the `logs_tx.closed()`
/// branch with the child's last line still in the pipe: `tokio::select!`
/// picks between ready branches at random. A reaped child's write ends are
/// closed, so the common case answers EOF at once. The budget is spent only
/// when a lamb still holds a write end, and it does not bound the lamb.
///
/// 100ms against a worst case of 7 to 12ms for two full pipes;
/// [`both_pipes_filled_to_capacity_drain_inside_the_budget`] pins it. Not
/// wider: a draining pump does not poll `ctl_rx`, and a handover gives each
/// pump one `REPORT_DEADLINE` (2s) to answer.
const FINAL_DRAIN: Duration = Duration::from_millis(100);

/// Real [`crate::runner::ProcessRunner`] over actual OS processes.
#[derive(Debug, Default)]
pub struct TokioRunner;

/// The exit code [`TokioProc::kill_tree`] terminates a job with.
///
/// Windows exits carry no signal number, so this is all a reader of
/// `ProcessInfo::last_exit` sees for a sheep the daemon killed. `137` is
/// `128 + 9`, what `commands::reap::classify` reads on unix for "killed by
/// SIGKILL".
#[cfg(windows)]
const KILL_TREE_EXIT_CODE: u32 = 137;

/// Distinguishes one spawn's shepherd-channel pipe from every other's.
///
/// The pipe namespace is machine-global, so a name must be unique across this
/// daemon's flock and any other daemon on the host: process id plus this
/// counter, never the sheep's name, which two `$SHEP_HOME`s could share.
/// Monotonic, so a restarted sheep never inherits a dying predecessor's name.
#[cfg(windows)]
static NEXT_CHANNEL_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

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
    /// Captured at spawn: `Child::id` reports `None` once the child has been
    /// waited, and a kill ladder may still need the pid then. It is also the
    /// whole of an adopted sheep's identity.
    pid: u32,
    proc: Supervised,
    /// The job object this sheep and everything it spawns belong to: Windows'
    /// stand-in for the unix process group, and what
    /// [`RunningProcess::kill_tree`] terminates.
    ///
    /// Held for the proc's whole life: the handle is the group, so dropping it
    /// leaves nothing to address the tree by.
    #[cfg(windows)]
    job: crate::sys_windows::Job,
}

/// Where this proc's exit comes from: tokio, or a targeted `waitpid`.
///
/// An adopted sheep crossed an `execve` into a successor that has no `Child`
/// for it and no way to make one, so [`AdoptedReaper`] collects its exit.
/// Only the wait differs: `signal`, `signal_process` and `kill_tree` all
/// address the pid.
#[derive(Debug)]
enum Supervised {
    /// Started by this daemon, and waited by tokio.
    Spawned(Child),
    /// Inherited across a handover, and waited by the successor's reaper.
    #[cfg(unix)]
    Adopted(Arc<AdoptedReaper>),
}

impl RunningProcess for TokioProc {
    fn pid(&self) -> u32 {
        self.pid
    }

    async fn wait(&mut self) -> ExitOutcome {
        let child = match &mut self.proc {
            Supervised::Spawned(child) => child,
            // Cancel-safe: the reaper remembers and replays a status it has
            // taken. Its error means something else reaped the pid, which
            // lands on the same degenerate outcome as the arm below.
            #[cfg(unix)]
            Supervised::Adopted(reaper) => {
                return match reaper.wait(self.pid).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        tracing::error!(pid = self.pid, %error, "adopted process wait failed");
                        ExitOutcome {
                            code: None,
                            signal: None,
                        }
                    }
                };
            }
        };
        // Cancel-safe: `Child::wait` replays its cached result rather than
        // restarting.
        match child.wait().await {
            Ok(status) => ExitOutcome {
                code: status.code(),
                // Nothing kills a Windows process by signal, so there is no
                // number for an exit to carry; see `KILL_TREE_EXIT_CODE`.
                #[cfg(windows)]
                signal: None,
                #[cfg(unix)]
                signal: status.signal(),
            },
            Err(error) => {
                // The `wait4()` itself failed, e.g. something else reaped the
                // pid. `wait` has no error variant, so report a terminal one.
                tracing::error!(pid = self.pid, %error, "process wait() failed");
                ExitOutcome {
                    code: None,
                    signal: None,
                }
            }
        }
    }

    #[cfg(unix)]
    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError> {
        signal_group(self.pid, to_nix_signal(sig))
    }

    /// Refuses every signal: Windows has no way to deliver anything
    /// SIGTERM-shaped to an arbitrary process.
    ///
    /// `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group)` reaches only
    /// console processes sharing a console with the caller, and a detached
    /// shepherd shares a console with nothing. An `Ok(())` would tell the
    /// ladder a polite stop was delivered and turn every `shep stop` into a
    /// silent hang and kill.
    ///
    /// An app on the shepherd channel never reaches this: `kill::kill_process`
    /// sends `ShepherdMessage::Shutdown` instead. Any other app costs its full
    /// `kill_timeout` and ends in [`Self::kill_tree`].
    #[cfg(windows)]
    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError> {
        Err(RunnerError::SignalFailed(format!(
            "Windows cannot deliver {sig:?} to another process; \
             an app that needs a graceful stop must opt into the shepherd \
             channel (shutdown_with_message), otherwise the stop escalates \
             to a forced termination after kill_timeout"
        )))
    }

    #[cfg(unix)]
    fn kill_tree(&mut self) -> Result<(), RunnerError> {
        signal_group(self.pid, Signal::SIGKILL)
    }

    /// Terminates the sheep's whole job: every process it spawned, however
    /// deeply nested.
    ///
    /// Stronger than the unix rung: a grandchild that calls `setsid` escapes
    /// its process group, while a job member cannot leave its job or spawn
    /// outside it, since `sys_windows::Job::create` grants no breakaway.
    #[cfg(windows)]
    fn kill_tree(&mut self) -> Result<(), RunnerError> {
        self.job
            .terminate(KILL_TREE_EXIT_CODE)
            .map_err(|error| RunnerError::SignalFailed(error.to_string()))
    }

    /// `SIGKILL` is delivered; the other eight names are refused by name.
    ///
    /// Per-signal rather than per-verb, so `shep signal <sheep> SIGKILL` keeps
    /// working while `SIGHUP` says what it cannot do. Seven of the nine have
    /// no delivery mechanism to a foreign Windows process at all. `Int` is
    /// refused as a judgement: `GenerateConsoleCtrlEvent(CTRL_C_EVENT, ..)`
    /// exists, but Ctrl+C is disabled by default under
    /// `CREATE_NEW_PROCESS_GROUP`, which is how every sheep is spawned.
    ///
    /// Per-process, matching the unix arm's positive-pid `kill`: this leaves
    /// the sheep's lambs running, unlike [`Self::kill_tree`].
    #[cfg(windows)]
    fn signal_process(&mut self, sig: OperatorSignal) -> Result<(), RunnerError> {
        let Supervised::Spawned(child) = &mut self.proc;
        match sig {
            OperatorSignal::Kill => child
                .start_kill()
                .map_err(|error| RunnerError::SignalFailed(error.to_string())),
            other => Err(RunnerError::SignalFailed(format!(
                "Windows has no way to deliver {other:?} to another process; \
                 only SIGKILL is available here"
            ))),
        }
    }

    #[cfg(unix)]
    fn signal_process(&mut self, sig: OperatorSignal) -> Result<(), RunnerError> {
        // Positive pid, unlike `signal_group`'s negative one: this reaches the
        // sheep alone, that one reaches its whole group.
        let pid = i32::try_from(self.pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| {
                RunnerError::SignalFailed(format!(
                    "pid {} is not a signallable process id",
                    self.pid
                ))
            })?;
        signal::kill(Pid::from_raw(pid), to_nix_operator_signal(sig))
            .map_err(|error| RunnerError::SignalFailed(error.to_string()))
    }
}

// Group-wide for both stop rungs and for the exec prober's timeout path
// (`probes/os.rs`, the reason this is `pub(crate)`): a wrapper script that
// forks without exec'ing leaves its child in the sheep's group, and a
// leader-only signal would leave that child running orphaned.
/// Sends `sig` to the whole process group led by `pid`.
///
/// `-pid` names the group `spawn`'s `process_group(0)` establishes. A
/// descendant that forks and then calls `setsid` lands in its own session,
/// which neither stop rung reaches.
///
/// # Errors
///
/// [`RunnerError::SignalFailed`] if `pid` is not a signallable process id, or
/// the `kill(2)` itself failed (typically `ESRCH`: no group led by `pid`).
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, sig: Signal) -> Result<(), RunnerError> {
    // `-0` is `0`, and `kill(0, ..)` means the daemon's own group: a zero pid
    // must never reach the syscall.
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
/// An explicit match, not `Signal::try_from(sig.as_raw())`, so an unmapped
/// variant is a compile error rather than a runtime one.
#[cfg(unix)]
fn to_nix_signal(sig: StopSignal) -> Signal {
    match sig {
        StopSignal::Term => Signal::SIGTERM,
        StopSignal::Int => Signal::SIGINT,
        StopSignal::Quit => Signal::SIGQUIT,
        StopSignal::Usr2 => Signal::SIGUSR2,
        StopSignal::Kill => Signal::SIGKILL,
    }
}

/// Maps [`OperatorSignal`] to the nix [`Signal`] it names.
///
/// shep-core holds no raw signal numbers, since they differ by platform
/// (`SIGUSR1` is 10 on Linux and 30 on macOS), so the two vocabularies meet
/// here.
#[cfg(unix)]
fn to_nix_operator_signal(sig: OperatorSignal) -> Signal {
    match sig {
        OperatorSignal::Hup => Signal::SIGHUP,
        OperatorSignal::Int => Signal::SIGINT,
        OperatorSignal::Quit => Signal::SIGQUIT,
        OperatorSignal::Term => Signal::SIGTERM,
        OperatorSignal::Usr1 => Signal::SIGUSR1,
        OperatorSignal::Usr2 => Signal::SIGUSR2,
        OperatorSignal::Winch => Signal::SIGWINCH,
        OperatorSignal::Cont => Signal::SIGCONT,
        OperatorSignal::Kill => Signal::SIGKILL,
    }
}

/// What exec will make of `spec.program`, before anything is spawned.
///
/// A `/` makes it a path, absolute or relative to `spec.cwd`, whose absence
/// is [`Preflight::Impossible`] and refuses the caller's whole batch. Without
/// one it is a bare command resolved through `spec.env`'s own `PATH`, at most
/// [`Preflight::Doubtful`]: a `shep startup` unit's `PATH` is not the
/// operator's shell's, so refusing would keep a flock down over one app's
/// interpreter.
///
/// [`Preflight::Unknown`] for everything else, and existence only. Read
/// through [`definitely_absent`] rather than [`Path::exists`], which
/// collapses a permission error into "absent".
fn what_exec_will_find(spec: &SpawnSpec) -> Preflight {
    if spec.program.is_empty() {
        return Preflight::Unknown;
    }
    let program = Path::new(&spec.program);
    // A `/` is the claim that this is a path; Windows spells it `\`. Missing
    // that claim costs only the clear refusal: the fall-through looks the
    // program up on PATH, misses, and the spawn proceeds anyway.
    if spec.program.contains('/') || spec.program.contains(std::path::MAIN_SEPARATOR) {
        let full = if program.is_absolute() {
            program.to_path_buf()
        } else {
            match &spec.cwd {
                Some(cwd) => cwd.join(program),
                None => return Preflight::Unknown,
            }
        };
        if !definitely_absent(&full) {
            return Preflight::Unknown;
        }
        return Preflight::Impossible(format!("no such file: {}", full.display()));
    }
    let Some(path) = spec.env.get("PATH").filter(|value| !value.is_empty()) else {
        return Preflight::Unknown;
    };
    // Absent only if every entry answers a plain `NotFound`: one unreadable
    // directory means exec may still find the program there. `split_paths`
    // rather than a separator of our own, since a Windows entry carries a `:`
    // of its own and may be quoted.
    for dir in std::env::split_paths(path).filter(|dir| !dir.as_os_str().is_empty()) {
        match fs::metadata(dir.join(program)) {
            Ok(_) => return Preflight::Unknown,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Preflight::Unknown,
        }
    }
    Preflight::Doubtful(format!(
        "`{}` is not on the shepherd's PATH ({})",
        spec.program,
        summarise_path(path)
    ))
}

/// How many `PATH` entries a preflight message names before summarising the
/// rest.
///
/// Four: [`base_env`](crate::assemble)'s fallback for a startup unit with no
/// `PATH` is three entries, so the case an operator hits prints in full with
/// one to spare. What gets cut off is an interactive shell's `PATH`, which is
/// unreadable in a terminal error.
const PATH_ENTRIES_IN_MESSAGE: usize = 4;

/// What separates one `PATH` entry from the next.
///
/// Display only: the lookup in `what_exec_will_find` goes through
/// `std::env::split_paths`, which also understands quoting.
const PATH_LIST_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

/// `path` as a message should print it: in full when short, and otherwise its
/// first [`PATH_ENTRIES_IN_MESSAGE`] entries with a count of the rest.
fn summarise_path(path: &str) -> String {
    let entries: Vec<&str> = path
        .split(PATH_LIST_SEPARATOR)
        .filter(|dir| !dir.is_empty())
        .collect();
    if entries.len() <= PATH_ENTRIES_IN_MESSAGE {
        return path.to_string();
    }
    format!(
        "{} and {} more entries",
        entries[..PATH_ENTRIES_IN_MESSAGE].join(&PATH_LIST_SEPARATOR.to_string()),
        entries.len() - PATH_ENTRIES_IN_MESSAGE,
    )
}

/// Whether the filesystem says, without qualification, that `path` is not
/// there.
///
/// `NotFound` and nothing else. [`Path::exists`] returns `false` on any
/// [`fs::metadata`] error, so a permission error, an unsettled mount and a
/// race would all read as absent and have `what_exec_will_find` refuse a
/// whole batch over a filesystem that was unavailable for a moment.
///
/// Follows symlinks, as exec does. A directory and a file with no execute bit
/// both answer `Ok` and so are not absent.
fn definitely_absent(path: &Path) -> bool {
    matches!(fs::metadata(path), Err(err) if err.kind() == io::ErrorKind::NotFound)
}

impl ProcessRunner for TokioRunner {
    type Proc = TokioProc;

    /// Reports a `program` that provably is not there, and nothing else.
    ///
    /// `program` is what `assemble` resolved `script` and `interpreter` down
    /// to, so an app running `npx next start` is checked at `npx` and `next`
    /// is npx's business. See `what_exec_will_find`.
    fn preflight(&self, spec: &SpawnSpec) -> Preflight {
        what_exec_will_find(spec)
    }

    /// Takes a sheep this image inherited rather than started.
    ///
    /// Nothing is spawned, opened or signalled. The carried pipe read ends go
    /// to the same log pump a spawn feeds, the carried log handles are written
    /// through rather than reopened, and the pid is the one the sheep has been
    /// running under all along.
    ///
    /// Stdin and the shepherd channel are both carried, so `shep whisper`
    /// reaches the same fd 0 and the child's fd 3 is undisturbed. What a blob
    /// named neither of is closed here rather than left dangling, so a
    /// caller's `is_closed()` says so at once.
    #[cfg(unix)]
    fn adopt(&self, spec: AdoptSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        let AdoptSpec {
            pid,
            out_file,
            err_file,
            out_pipe,
            err_pipe,
            out_log,
            err_log,
            stdin_pipe,
            channel,
            reaper,
        } = spec;

        let (logs_tx, logs_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (log_ctl_tx, log_ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
        // Read before the handles move into their pumps: these are the
        // numbers the next handover carries.
        let pipes = PipeFds {
            out: out_pipe.as_ref().map(AsRawFd::as_raw_fd),
            err: err_pipe.as_ref().map(AsRawFd::as_raw_fd),
            stdin: stdin_pipe.as_ref().map(AsRawFd::as_raw_fd),
            channel: channel.as_ref().map(AsRawFd::as_raw_fd),
        };
        spawn_log_pump(
            out_pipe,
            err_pipe,
            carried_sink(out_file, out_log),
            carried_sink(err_file, err_log),
            logs_tx,
            log_ctl_rx,
            pipes,
        );

        let (from_child_tx, from_child) = mpsc::channel(CHANNEL_CAPACITY);
        let (to_child, to_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        if let Some(channel) = channel {
            spawn_channel_pumps(channel, from_child_tx, to_child_rx);
        } else {
            drop(from_child_tx);
            drop(to_child_rx);
        }
        let (to_stdin, to_stdin_rx) = mpsc::channel(CHANNEL_CAPACITY);
        if let Some(stdin_pipe) = stdin_pipe {
            spawn_stdin_pump(Some(stdin_pipe), to_stdin_rx);
        } else {
            drop(to_stdin_rx);
        }

        Ok((
            TokioProc {
                pid,
                proc: Supervised::Adopted(reaper),
            },
            ProcIo {
                logs: logs_rx,
                from_child,
                to_child,
                log_ctl: log_ctl_tx,
                to_stdin,
            },
        ))
    }

    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        // `SpawnSpec::env` promises no daemon-env leakage beyond this map, and
        // `Command` inherits the daemon's ambient env without the clear.
        command.env_clear();
        command.envs(&spec.env);
        // `/dev/null` unless the app asked for a pipe: many programs decide
        // they are non-interactive from a closed fd 0. See `AppConfig::stdin`.
        command.stdin(if spec.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        // New process group rooted at the child, so `kill_tree`'s
        // negative-pid `SIGKILL` cannot reach the daemon's own group.
        #[cfg(unix)]
        command.process_group(0);

        // Containment itself happens after `spawn()`: a process joins a job
        // only once it exists. These flags make that assignment meaningful by
        // keeping a Ctrl+C in the shepherd's console off the flock, and a
        // console child from flashing up a window nobody can draw.
        #[cfg(windows)]
        {
            /// Roots a new console process group at the child.
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            /// Runs a console application without allocating a console window.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }

        #[cfg(unix)]
        if let Some(creds) = spec.credentials {
            // std sets the gid before the uid, which is the order a privilege
            // drop requires. `CommandExt::groups` is unstable and unused, and
            // std still calls `setgroups(0, NULL)` before `setuid()` whenever
            // `.uid()` is set, so the child gets no supplementary groups.
            if let Some(gid) = creds.gid {
                command.gid(gid);
            }
            command.uid(creds.uid);
        }

        // `privilege::resolve` refuses `user`/`group` on Windows long before
        // a spawn, so this is an assertion rather than an error path: real
        // privilege drop there needs a plaintext password or an LSA logon
        // session, and a partial version would be worse than the refusal.
        #[cfg(windows)]
        debug_assert!(
            spec.credentials.is_none(),
            "privilege::resolve must refuse user/group on Windows before a spawn is reached"
        );

        let (from_child_tx, from_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (to_child_tx, to_child_rx) = mpsc::channel(CHANNEL_CAPACITY);

        // A named pipe the child opens by name, since `command-fds` is
        // unix-only and `cmd.exe` has no fd-3 redirection. Same wire format;
        // only the handle moves. `SHEP_CHANNEL_PIPE` is exported and
        // `SHEP_CHANNEL_FD` is not, so an app branches on the variable.
        #[cfg(windows)]
        if spec.channel {
            use shep_core::transport;

            // Unique per spawn, so two instances cannot share a channel. The
            // nonce closes prediction, not observation: the pipe namespace
            // lists to any local user and the `accept` below authenticates
            // nobody. A restrictive DACL needs unsafe; see deferred.md.
            let mut nonce = [0_u8; 16];
            getrandom::fill(&mut nonce).map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel pipe name: {error}"))
            })?;
            let pipe = std::path::PathBuf::from(format!(
                r"\\.\pipe\shep-channel-{}-{}-{:032x}",
                std::process::id(),
                NEXT_CHANNEL_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
                u128::from_ne_bytes(nonce)
            ));
            let mut listener = transport::Listener::bind(&pipe).map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel pipe: {error}"))
            })?;
            command.env("SHEP_CHANNEL_PIPE", &pipe);
            command.env("SHEP_CHANNEL_VERSION", CHANNEL_VERSION);

            // On a task, since `spawn` is synchronous and the child cannot
            // connect until it is started below. The `closed()` arm bounds
            // it: an app that never opens the pipe would otherwise hold the
            // listener and a sender for the daemon's life. Both are cancel-safe.
            let from_child_tx = from_child_tx.clone();
            tokio::spawn(async move {
                let watcher = from_child_tx.clone();
                tokio::select! {
                    accepted = listener.accept() => match accepted {
                        Ok(daemon_end) => {
                            spawn_channel_pumps(daemon_end, from_child_tx, to_child_rx);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "shepherd channel accept failed");
                        }
                    },
                    () = watcher.closed() => {}
                }
            });
        } else {
            drop(to_child_rx);
        }

        // The block below moves `daemon_end` into its pumps, and `PipeFds` is
        // assembled after the spawn: the number is in reach only here.
        #[cfg(unix)]
        let mut channel_fd: Option<RawFd> = None;
        #[cfg(unix)]
        if spec.channel {
            command.env("SHEP_CHANNEL_FD", "3");
            // Not negotiation: an app cannot be asked what it speaks, but one
            // that wants to be defensive can check what it is given.
            command.env("SHEP_CHANNEL_VERSION", CHANNEL_VERSION);
            let (daemon_end, child_end) = UnixStream::pair().map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel socketpair: {error}"))
            })?;
            let std_child_end = child_end.into_std().map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel into_std: {error}"))
            })?;
            // `UnixStream::pair()` sets `O_NONBLOCK` on both ends for tokio's
            // half, and the child inherits it across the exec: a plain
            // `read <&3` would get `EAGAIN` rather than parking. The daemon's
            // own end is a separate descriptor and stays non-blocking.
            std_child_end.set_nonblocking(false).map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel set_nonblocking: {error}"))
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
            channel_fd = Some(daemon_end.as_raw_fd());
            spawn_channel_pumps(daemon_end, from_child_tx, to_child_rx);
        } else {
            // No channel: closed rather than dangling, so `from_child.recv()`
            // reports closed at once and a stray send fails fast.
            drop(from_child_tx);
            drop(to_child_rx);
        }

        let mut child = command
            .spawn()
            .map_err(|error| RunnerError::SpawnFailed(error.to_string()))?;
        // Closes the parent's copy of the fd-3 socketpair end here rather
        // than at the end of the scope, so the daemon's end sees a clean EOF.
        drop(command);

        let pid = child.id().ok_or_else(|| {
            RunnerError::SpawnFailed("child exited before its pid could be read".to_string())
        })?;

        // As early as containment can happen: the child exists and everything
        // it spawns from here inherits the job. Fatal on failure, because a
        // sheep outside its job is one `kill_tree` cannot reach and `shep
        // stop` would report success over a running process.
        #[cfg(windows)]
        let job = {
            let job = crate::sys_windows::Job::create().map_err(|error| {
                RunnerError::SpawnFailed(format!("job object for {}: {error}", spec.name))
            })?;
            let handle = child.raw_handle().ok_or_else(|| {
                RunnerError::SpawnFailed("child exited before it could be contained".to_string())
            })?;
            if let Err(error) = job.assign(handle) {
                // Running and in no job: nothing could stop its descendants
                // afterwards, so it must not be left behind.
                let _ = child.start_kill();
                return Err(RunnerError::SpawnFailed(format!(
                    "could not contain {} in a job object: {error}",
                    spec.name
                )));
            }
            job
        };

        let (logs_tx, logs_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (log_ctl_tx, log_ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
        // Before the `take`s below move the handles: the only place the
        // numbers a handover carries are known.
        #[cfg(unix)]
        let pipes = PipeFds {
            out: child.stdout.as_ref().map(AsRawFd::as_raw_fd),
            err: child.stderr.as_ref().map(AsRawFd::as_raw_fd),
            stdin: child.stdin.as_ref().map(AsRawFd::as_raw_fd),
            // Read further up, before the daemon end moved into its pumps.
            // Nothing on `child` names it: the child's side is fd 3.
            channel: channel_fd,
        };
        #[cfg(not(unix))]
        let pipes = PipeFds;
        spawn_log_pump(
            child.stdout.take(),
            child.stderr.take(),
            LogSink::Path(spec.out_file.clone()),
            LogSink::Path(spec.err_file.clone()),
            logs_tx,
            log_ctl_rx,
            pipes,
        );

        let (to_stdin_tx, to_stdin_rx) = mpsc::channel(CHANNEL_CAPACITY);
        if spec.stdin {
            spawn_stdin_pump(child.stdin.take(), to_stdin_rx);
        } else {
            // Dropped rather than dangling, so a caller's `is_closed()` says
            // "no pipe here" at once.
            drop(to_stdin_rx);
        }

        let io = ProcIo {
            logs: logs_rx,
            from_child: from_child_rx,
            to_child: to_child_tx,
            log_ctl: log_ctl_tx,
            to_stdin: to_stdin_tx,
        };
        Ok((
            TokioProc {
                pid,
                proc: Supervised::Spawned(child),
                #[cfg(windows)]
                job,
            },
            io,
        ))
    }
}

/// The sink for one carried stream: its handle when the blob had one, and
/// its path when it did not.
///
/// A sheep whose log open had failed before the handover carries no handle
/// for that stream, which is a `None` rather than a refusal (see
/// `handover::adopt`). Opening the path here is the right recovery: it is
/// what the predecessor's pump would have done at its next reopen, and it
/// costs the successor nothing when the open fails again.
#[cfg(unix)]
fn carried_sink(path: PathBuf, handle: Option<tokio::fs::File>) -> LogSink {
    match handle {
        Some(file) => LogSink::Carried(path, file),
        None => LogSink::Path(path),
    }
}

/// One stream's log file: the path the spec named, plus the buffered handle
/// open on it, `None` when the open failed.
///
/// Generic over the sink only so a test can count the writes that reach it;
/// production only ever builds the default.
#[derive(Debug)]
struct LogFile<W = tokio::fs::File> {
    path: PathBuf,
    handle: Option<BufWriter<W>>,
    /// Serializes a whole record against the other writer on this path.
    ///
    /// From [`record_lock`]. Taken at open and kept rather than looked up
    /// per line: the path never changes and a reopen keeps it.
    record: Arc<tokio::sync::Mutex<()>>,
    /// When the oldest line the pump has not tried to flush yet was
    /// appended; the pump reads it as an [`IDLE_FLUSH`] deadline.
    ///
    /// Cleared by every flush attempt, successful or not: a file that cannot
    /// be written must not turn the idle flush into a retry loop.
    buffered_since: Option<Instant>,
    /// Scratch the line's timestamp is formatted into, cleared and refilled
    /// per line rather than reallocated.
    ///
    /// Nothing outside [`LogFile::append`] may read it.
    stamp: String,
}

/// Where one stream's log handle comes from when a pump starts.
///
/// A successor's pump is handed a handle already open on the file; opening
/// the path again would lose `O_APPEND` (see [`open_append`]). The path
/// travels with the handle either way, so a later [`LogCtl::Reopen`] behaves
/// identically for both.
enum LogSink {
    /// Open this path for appending, which is what every spawn does.
    Path(PathBuf),
    /// Write through this already-open appending handle, on this path.
    #[cfg(unix)]
    Carried(PathBuf, tokio::fs::File),
}

impl LogFile<tokio::fs::File> {
    /// Builds the log file a [`LogSink`] describes.
    async fn from_sink(sink: LogSink) -> Self {
        match sink {
            LogSink::Path(path) => Self::open(path).await,
            #[cfg(unix)]
            LogSink::Carried(path, file) => Self::from_file(path, file),
        }
    }

    /// Takes an already-open appending handle on `path`, opening nothing.
    ///
    /// `O_APPEND` is a file status flag on the open file description, so it
    /// crossed the `execve` with the descriptor; reopening `path` here would
    /// give a handle that writes at its own tracked offset, the sparse hole
    /// [`open_append`] documents. [`Self::reopen`] still goes by path, so a
    /// rotation works on a carried handle as on an opened one.
    #[cfg(unix)]
    fn from_file(path: PathBuf, file: tokio::fs::File) -> Self {
        Self {
            // Same path, therefore the same lock as any other handle on it.
            // A carried descriptor is still one of two writers.
            record: record_lock(&path),
            path,
            handle: Some(BufWriter::with_capacity(LOG_BUFFER, file)),
            buffered_since: None,
            stamp: String::new(),
        }
    }

    /// Opens `path` for appending, keeping the path for later reopens.
    ///
    /// A failed open is not fatal: the pump must still drain the child's
    /// streams whether or not it can write them anywhere.
    /// [`LogFile::reopen`] is the one that reports, since there a caller is
    /// waiting.
    async fn open(path: PathBuf) -> Self {
        let handle = open_append(&path)
            .await
            .ok()
            .map(|file| BufWriter::with_capacity(LOG_BUFFER, file));
        Self {
            record: record_lock(&path),
            path,
            handle,
            buffered_since: None,
            stamp: String::new(),
        }
    }

    /// Flushes and closes the current handle, then opens the path again.
    ///
    /// Flushing first is what makes [`LogCtl::Reopen`]'s acknowledgement
    /// worth having: the buffer travels with the handle being dropped, so a
    /// reopen that skipped it would discard those lines rather than delay
    /// them. Reopening goes through [`open_append`].
    ///
    /// # Errors
    ///
    /// The path could not be opened again. The old handle is closed
    /// regardless, so the rotator's rename is safe to act on. A failed flush
    /// is logged rather than returned.
    async fn reopen(&mut self) -> Result<(), ReopenError> {
        self.buffered_since = None;
        if let Some(handle) = self.handle.as_mut() {
            // A flush of a full buffer is the at-or-over-capacity case
            // `record_lock` documents as spanning several `poll_write`
            // calls.
            let _record = self.record.lock().await;
            if let Err(error) = handle.flush().await {
                tracing::error!(path = ?self.path, %error, "log file flush failed");
            }
        }
        // Closed before the reopen, so the pump never holds two descriptors
        // on one log at the same time.
        drop(self.handle.take());
        match open_append(&self.path).await {
            Ok(handle) => {
                self.handle = Some(BufWriter::with_capacity(LOG_BUFFER, handle));
                Ok(())
            }
            Err(error) => Err(ReopenError {
                message: format!("{}: {error}", self.path.display()),
            }),
        }
    }

    /// This stream's open log-file descriptor, `None` when there is no
    /// handle.
    ///
    /// Read off the live handle rather than remembered, so a
    /// [`Self::reopen`] cannot leave a stale number for a handover to carry.
    #[cfg(unix)]
    fn raw_fd(&self) -> Option<RawFd> {
        self.handle
            .as_ref()
            .map(|handle| handle.get_ref().as_raw_fd())
    }
}

/// The lock that serializes one whole record written to one log path.
///
/// Two things write a sheep's log file: [`LogFile::append`] through a
/// [`BufWriter`] owned by the pump, and [`crate::dogs::narrate`] through its
/// own handle. One `write_all` is not enough: a `BufWriter` writes through
/// directly at or over capacity, and a short write is several `poll_write`
/// calls the other handle can land between, tearing a line into one half
/// with two stamps and one with none.
///
/// Keyed by path, since narration reaches a log through a path and never
/// sees the `LogFile`. The map grows one entry per log path opened and is
/// never freed.
pub(crate) fn record_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(locks.entry(path.to_path_buf()).or_default())
}

impl<W: AsyncWrite + Unpin> LogFile<W> {
    /// Appends one timestamped line and its newline to the buffer, logging
    /// rather than propagating a write failure: a log we cannot write to
    /// must not stop the pump draining the child's pipes.
    ///
    /// The stamp, the line and the newline are joined in `self.stamp` and
    /// handed to one `write_all`. [`crate::dogs::narrate`] writes through a
    /// second handle on this same path, so three separate writes let a
    /// narration line land between a stamp and the body it belongs to,
    /// breaking the one-stamp-per-line contract
    /// [`shep_core::logstamp::strip`] reads on.
    ///
    /// The line forwarded on `logs_tx` is the sheep's own bytes, unstamped.
    async fn append(&mut self, line: &str) {
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        self.stamp.clear();
        stamp_into(&mut self.stamp);
        self.stamp.push_str(line);
        self.stamp.push('\n');
        // Held across the write, not merely around the buffer copy: one
        // `write_all` is still several syscalls when the line is at or over
        // the buffer's capacity. See `record_lock`.
        let written = {
            let _record = self.record.lock().await;
            handle.write_all(self.stamp.as_bytes()).await
        };
        self.buffered_since.get_or_insert_with(Instant::now);
        if let Err(error) = written {
            tracing::error!(path = ?self.path, %error, "log file append failed");
        }
    }

    /// Writes out the buffer and waits for it to reach the file, keeping the
    /// handle open.
    ///
    /// [`Self::append`] only reaches the buffer, so truncating this path
    /// without waiting here can empty the file before already-accepted lines
    /// land at offset 0. A stream with no handle answers `Ok`.
    ///
    /// # Errors
    ///
    /// A buffered or already-dispatched write failed.
    async fn flush(&mut self) -> Result<(), FlushError> {
        self.buffered_since = None;
        let Some(handle) = self.handle.as_mut() else {
            return Ok(());
        };
        // A flush can land inside the other writer's record as readily as an
        // append; see `record_lock`.
        let _record = self.record.lock().await;
        handle.flush().await.map_err(|error| FlushError {
            message: format!("{}: {error}", self.path.display()),
        })
    }
}

/// The descriptor numbers a handover carries for one sheep's pipes.
///
/// Told to the pump rather than read off its readers: the pump is generic
/// over them, and an in-memory stream has no descriptor at all.
///
/// Empty on Windows, which has no handover to carry them: `Arm::for_daemon`
/// returns the stop-and-start arm there.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default)]
struct PipeFds {
    /// The read end of the child's stdout, while the pump still holds it.
    out: Option<RawFd>,
    /// The read end of the child's stderr, while the pump still holds it.
    err: Option<RawFd>,
    /// The write end of the child's stdin, held by the stdin pump rather
    /// than by this one.
    ///
    /// Reported here because this pump is the only party a snapshot asks.
    /// The stdin pump ends when the last `to_stdin` sender drops, and the
    /// supervisor's slot holds one while the sheep is registered, so the
    /// write end cannot be closed and its number reissued between the report
    /// and the exec.
    ///
    /// Never cleared as a stream ends: it has no EOF to reach.
    stdin: Option<RawFd>,
    /// The daemon's end of the child's shepherd-channel socketpair, held by
    /// that channel's two pump tasks rather than by this one.
    ///
    /// Reported here for the reason [`Self::stdin`] gives, but the ownership
    /// argument is weaker: the writer task also ends on a write that fails,
    /// which a child that has closed its fd 3 produces, and this pump never
    /// hears about it. `Actor::handle_handover_snapshot` masks this field
    /// when `SheepSlot::open_channel` says the channel is gone.
    channel: Option<RawFd>,
}

/// See the unix definition above; there is nothing to carry here.
#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, Default)]
struct PipeFds;

/// A sheep's two log files and the two stream descriptors alongside them,
/// which is the set a handover carries.
#[derive(Debug)]
struct LogFiles {
    out: LogFile,
    err: LogFile,
    /// The descriptors a blob names for this sheep. Each stream's read end
    /// is cleared as that stream ends: the reader is dropped at the same
    /// moment, and a closed number must never reach a handover blob.
    pipes: PipeFds,
    /// Whether this pump has answered a [`LogCtl::ReportFds`] that no
    /// [`LogCtl::Resume`] has ended, and so has stopped reading its
    /// sheep's streams.
    ///
    /// See [`LogFiles::reading`] for why a report parks a pump at all.
    #[cfg(unix)]
    parked: bool,
}

impl LogFiles {
    /// The file a line from this stream is appended to (`err` picks stderr).
    fn stream(&mut self, err: bool) -> &mut LogFile {
        if err { &mut self.err } else { &mut self.out }
    }

    /// Whether the pump should still be reading its sheep's streams.
    ///
    /// False between a [`LogCtl::ReportFds`] and the exec that consumes it:
    /// the report hands the successor descriptor numbers and a flush behind
    /// them, so a pump still reading would append lines the `execve` erases
    /// and strand pipe bytes the successor cannot see.
    ///
    /// Also pins both stream numbers: a paused pump cannot reach EOF, drop a
    /// reader, or free its number. The residual is one line per reload, the
    /// one the sheep was mid-write on at the exec.
    fn reading(&self) -> bool {
        #[cfg(unix)]
        {
            !self.parked
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    /// When the pump owes the older of the two buffers a flush, or `None`
    /// when neither holds anything.
    ///
    /// Derived rather than stored, so an explicit [`LogCtl::Flush`] or
    /// [`LogCtl::Reopen`] retires the deadline by the same act that empties
    /// the buffer; nothing here can be left armed for a buffer that is
    /// already on disk.
    fn flush_deadline(&self) -> Option<Instant> {
        let oldest = match (self.out.buffered_since, self.err.buffered_since) {
            (Some(out), Some(err)) => out.min(err),
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => return None,
        };
        Some(oldest + IDLE_FLUSH)
    }

    /// Flushes both buffers, logging rather than reporting a failure: no
    /// caller is waiting on an idle flush.
    async fn flush_idle(&mut self) {
        for file in [&mut self.out, &mut self.err] {
            if let Err(error) = file.flush().await {
                tracing::error!(%error, "log file idle flush failed");
            }
        }
    }

    /// Carries out one control request and then answers it.
    ///
    /// The acknowledgement is the last statement, after both streams have
    /// been dealt with: a caller that has heard back knows both handles were
    /// swapped, or that neither has a write left in flight.
    ///
    /// stderr is served even when stdout's turn just failed, since
    /// short-circuiting would take a sheep's working half offline over the
    /// broken one. Both failures then travel joined by `", "`, where the
    /// supervisor joins one of these per sheep with `"; "`.
    #[cfg_attr(
        not(unix),
        allow(
            unused_variables,
            reason = "`streams` is read only by `ReportFds`, and that variant is unix-only"
        )
    )]
    async fn serve<O, E>(&mut self, ctl: LogCtl, streams: &mut Streams<O, E>)
    where
        O: AsyncRead + Unpin,
        E: AsyncRead + Unpin,
    {
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
            // Park, drain, flush, then answer. What a reader has taken off
            // its pipe is on the far side of the descriptor the blob
            // carries, so those bytes reach a file only if this image writes
            // them. A failed flush is logged; the answer has no room for it.
            #[cfg(unix)]
            LogCtl::ReportFds { done } => {
                // Nothing between here and the answer could read a stream
                // anyway: `serve` runs to completion inside the pump's own
                // task, the only reader either stream has.
                self.parked = true;
                drain_ready(&mut streams.out, &mut self.out).await;
                drain_ready(&mut streams.err, &mut self.err).await;
                for file in [&mut self.out, &mut self.err] {
                    if let Err(error) = file.flush().await {
                        tracing::error!(%error, "log flush before a handover report failed");
                    }
                }
                let _ = done.send(crate::handover::CarriedFds {
                    out_pipe: self.pipes.out,
                    err_pipe: self.pipes.err,
                    out_log: self.out.raw_fd(),
                    err_log: self.err.raw_fd(),
                    stdin: self.pipes.stdin,
                    channel: self.pipes.channel,
                });
            }
            // No acknowledgement to send, and nothing to undo but the flag:
            // the drain above emptied the reader rather than copying it, so
            // a resumed pump picks its sheep up wherever the pipe left off.
            #[cfg(unix)]
            LogCtl::Resume => self.parked = false,
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
/// The wait for room on `logs_tx` keeps serving `ctl_rx`; see
/// [`reserve_slot`] for the cycle that would otherwise close.
async fn deliver_line<O, E>(
    result: io::Result<Option<String>>,
    err: bool,
    files: &mut LogFiles,
    logs_tx: &mpsc::Sender<LogLine>,
    ctl_rx: &mut mpsc::Receiver<LogCtl>,
    streams: &mut Streams<O, E>,
) -> AfterLine
where
    O: AsyncRead + Unpin,
    E: AsyncRead + Unpin,
{
    match result {
        Ok(Some(line)) => {
            files.stream(err).append(&line).await;
            let Some(slot) = reserve_slot(logs_tx, files, ctl_rx, streams).await else {
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

/// Waits for room on `logs_tx`, serving control requests and idle flushes
/// while it waits. `None` once the `logs` receiver is gone.
///
/// A pump parked on a full `logs` channel is parked for as long as the sheep
/// task takes, which is unbounded, so without the idle-flush branch the line
/// just appended would sit in the buffer for exactly that long.
///
/// A `select!` handler is not cancellable, so a bare `send().await` inside
/// one stops the pump polling `ctl_rx` for as long as the wait lasts. The
/// party that makes room on `logs` is the sheep task, the same party a
/// reopen's acknowledgement travels back to, so that would close a cycle.
async fn reserve_slot<'tx, O, E>(
    logs_tx: &'tx mpsc::Sender<LogLine>,
    files: &mut LogFiles,
    ctl_rx: &mut mpsc::Receiver<LogCtl>,
    streams: &mut Streams<O, E>,
) -> Option<mpsc::Permit<'tx, LogLine>>
where
    O: AsyncRead + Unpin,
    E: AsyncRead + Unpin,
{
    loop {
        // Recomputed every iteration from the stored mark, so losing the
        // race never extends the window.
        let flush_at = files.flush_deadline();
        // Every branch is documented cancel-safe, as `select!` requires: a
        // `reserve` that loses the race has taken no slot, a `recv` that
        // loses it has taken no message, and a `sleep_until` that loses it
        // is rebuilt against the same absolute deadline.
        tokio::select! {
            slot = logs_tx.reserve() => return slot.ok(),
            ctl = ctl_rx.recv() => match ctl {
                Some(ctl) => files.serve(ctl, streams).await,
                // The line in hand is still owed to the receiver. Awaited
                // outside the `select!` rather than looping, since a closed
                // receiver is ready on every poll.
                None => return logs_tx.reserve().await.ok(),
            },
            () = sleep_until(flush_at.unwrap_or_else(Instant::now)), if flush_at.is_some() => {
                files.flush_idle().await;
            }
        }
    }
}

/// The next line from an optional stream, or a future that never resolves
/// once there is no stream left to read.
///
/// The pump's `select!` needs a branch it can leave in place after a stream
/// ends: a ready `None` would be re-selected on every poll and spin the
/// loop, while pending forever drops the branch out of contention.
///
/// Cancel-safe, as a `select!` branch must be: a partially read line stays
/// in the `Lines` buffer instead of being lost to another branch.
async fn next_line<R>(lines: &mut Option<Lines<BufReader<R>>>) -> io::Result<Option<String>>
where
    R: AsyncRead + Unpin,
{
    match lines {
        Some(lines) => lines.next_line().await,
        None => core::future::pending().await,
    }
}

/// One stream's reader, at the capacity a handover reasons about.
///
/// A free function rather than an inline `BufReader::with_capacity` at each
/// call site, so [`drain_ready`]'s bound and the reader's real capacity
/// cannot drift apart.
#[cfg(unix)]
fn with_read_buffer<R: AsyncRead>(reader: R) -> BufReader<R> {
    BufReader::with_capacity(READ_BUFFER, reader)
}

/// See the unix definition above; nothing here reasons about the capacity.
#[cfg(not(unix))]
fn with_read_buffer<R: AsyncRead>(reader: R) -> BufReader<R> {
    BufReader::new(reader)
}

/// A pump's two line readers, held in one place so a control request can
/// reach them.
///
/// [`LogCtl::ReportFds`] is served from two places, the pump's own `select!`
/// and [`reserve_slot`]'s, and a local would be visible to only one.
///
/// `None` per stream is one that has reached EOF or failed. The pump drops
/// the reader at that moment, which closes the descriptor, so a stream with
/// no reader has neither a number nor bytes left to write.
struct Streams<O, E> {
    /// The stdout reader, until stdout ends.
    out: Option<Lines<BufReader<O>>>,
    /// The stderr reader, until stderr ends.
    err: Option<Lines<BufReader<E>>>,
}

/// Writes out the whole lines one stream's reader is already holding, and
/// touches the pipe behind it not at all.
///
/// This is what a descriptor report owes the successor: the reader has taken
/// up to [`READ_BUFFER`] off the pipe the successor will never see there,
/// and the `execve` destroys it. Reading the pipe here would refill the
/// reader as fast as it empties it; stopping at "no whole line is buffered"
/// makes the buffer strictly shrink, since a `read_line` that finds its
/// delimiter already buffered returns without a syscall.
///
/// The partial line at the end of the buffer is left behind. Nothing drained
/// here goes on `logs_tx`: those bus subscribers go with the image.
#[cfg(unix)]
async fn drain_ready<R>(lines: &mut Option<Lines<BufReader<R>>>, file: &mut LogFile)
where
    R: AsyncRead + Unpin,
{
    let Some(reader) = lines.as_mut() else {
        return;
    };
    while reader.get_ref().buffer().contains(&b'\n') {
        // A delimiter already in the buffer means no `read(2)`, so nothing
        // new arrives to replace what is written.
        let Ok(Some(line)) = reader.next_line().await else {
            return;
        };
        file.append(&line).await;
    }
}

/// Writes out what the streams still hold once the sheep task has let go,
/// and stops after [`FINAL_DRAIN`] however much is left.
///
/// Files only: `logs_tx` is closed, which is what brought us here.
///
/// Each stream is retired on EOF or a read failure, so a child that has
/// exited ends this in one poll per stream. The budget covers the other
/// case: a lamb that inherited a write end keeps the pipe open, and without
/// a bound the pump would follow it for as long as it cared to talk.
///
/// A retired stream does not clear its number from `files.pipes` and does
/// not need to: the caller `break`s as soon as this returns.
async fn final_drain<O, E>(files: &mut LogFiles, streams: &mut Streams<O, E>)
where
    O: AsyncRead + Unpin,
    E: AsyncRead + Unpin,
{
    let drained = timeout(FINAL_DRAIN, async {
        while streams.out.is_some() || streams.err.is_some() {
            // Bound before the `match`: the future borrows `streams`, and a
            // scrutinee's temporaries outlive the arms.
            tokio::select! {
                result = next_line(&mut streams.out) => {
                    match result {
                        Ok(Some(line)) => files.stream(false).append(&line).await,
                        Ok(None) | Err(_) => streams.out = None,
                    }
                }
                result = next_line(&mut streams.err) => {
                    match result {
                        Ok(Some(line)) => files.stream(true).append(&line).await,
                        Ok(None) | Err(_) => streams.err = None,
                    }
                }
            }
        }
    })
    .await;
    if drained.is_err() {
        tracing::debug!("a pump left the pipe still open at its sheep task's exit");
    }
}

/// Pumps a sheep's stdout and stderr to completion, and stays reachable the
/// whole time it does.
///
/// Every line is appended to its stream's file and then forwarded on
/// `logs_tx`. A [`LogCtl`] is served between lines, while no line is flowing
/// at all, and while the pump waits for room on `logs_tx`. One task for both
/// streams, so one [`LogCtl::Reopen`] swaps both files and answers once.
///
/// The file write is issued before the line is forwarded but lands in a
/// [`BufWriter`], so a line seen on `logs_tx` need not be in the file yet.
/// [`LogCtl::Reopen`], [`LogCtl::Flush`] and [`IDLE_FLUSH`] are the
/// barriers. A reopen waits behind the pump's own file I/O, with no timeout.
fn spawn_log_pump<O, E>(
    stdout: Option<O>,
    stderr: Option<E>,
    out_sink: LogSink,
    err_sink: LogSink,
    logs_tx: mpsc::Sender<LogLine>,
    mut ctl_rx: mpsc::Receiver<LogCtl>,
    pipes: PipeFds,
) where
    O: AsyncRead + Unpin + Send + 'static,
    E: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut files = LogFiles {
            out: LogFile::from_sink(out_sink).await,
            err: LogFile::from_sink(err_sink).await,
            pipes,
            #[cfg(unix)]
            parked: false,
        };
        let mut streams = Streams {
            out: stdout.map(|reader| with_read_buffer(reader).lines()),
            err: stderr.map(|reader| with_read_buffer(reader).lines()),
        };

        while streams.out.is_some() || streams.err.is_some() {
            // Recomputed every iteration from the stored mark;
            // `reserve_slot` carries the same branch.
            let flush_at = files.flush_deadline();
            tokio::select! {
                result = next_line(&mut streams.out), if files.reading() => {
                    // Bound before the `match`: the future borrows `streams`,
                    // and a scrutinee's temporaries outlive the arms, so an
                    // arm could not clear the reader it was reading.
                    let after =
                        deliver_line(result, false, &mut files, &logs_tx, &mut ctl_rx, &mut streams)
                            .await;
                    match after {
                        AfterLine::KeepReading => {}
                        // Dropping the reader closes the descriptor, so the
                        // number stops being ours in the same statement it
                        // stops being readable.
                        AfterLine::StreamEnded => {
                            streams.out = None;
                            #[cfg(unix)]
                            {
                                files.pipes.out = None;
                            }
                        }
                        AfterLine::LogsClosed => break,
                    }
                }
                result = next_line(&mut streams.err), if files.reading() => {
                    let after =
                        deliver_line(result, true, &mut files, &logs_tx, &mut ctl_rx, &mut streams)
                            .await;
                    match after {
                        AfterLine::KeepReading => {}
                        // As above: the number goes with the reader.
                        AfterLine::StreamEnded => {
                            streams.err = None;
                            #[cfg(unix)]
                            {
                                files.pipes.err = None;
                            }
                        }
                        AfterLine::LogsClosed => break,
                    }
                }
                ctl = ctl_rx.recv() => {
                    match ctl {
                        Some(ctl) => files.serve(ctl, &mut streams).await,
                        None => break, // nothing holds a `log_ctl` sender
                    }
                }
                // The owning sheep task dropped its `logs` receiver. Its own
                // branch, because a pump whose child forked a lamb holding
                // the pipe reaches no EOF and has no next line to notice it
                // on. Cancel-safe: a closed channel stays closed.
                () = logs_tx.closed() => break,

                // Cancel-safe: rebuilt against the same absolute deadline
                // every iteration, so losing the race costs nothing.
                () = sleep_until(flush_at.unwrap_or_else(Instant::now)), if flush_at.is_some() => {
                    files.flush_idle().await;
                }
            }
        }
        // On the way out rather than in the branch that prompted it: four
        // exits reach this line and each can leave a line unread. Not while
        // parked, since a parked pump is holding the pipe for a successor to
        // adopt.
        if files.reading() {
            final_drain(&mut files, &mut streams).await;
        }
        // A `BufWriter` cannot flush itself as it drops, and every way out
        // of the loop above drops both of them.
        files.flush_idle().await;
    });
}

/// Opens `path` for appending, creating its parent directory at
/// [`DIR_MODE`], asked for at `mkdir` time so it is never wider.
///
/// `.append(true)` is load-bearing: `O_APPEND` makes every write seek to end
/// atomically, so a `copytruncate` rotator can truncate under a live handle
/// and the next line still lands at offset 0. [`check_log_ancestry`] runs
/// before the `mkdir`, since `create_dir_all` walks straight through a
/// symlink to a directory, and [`open_log_path`] adds `O_NOFOLLOW`.
///
/// # Errors
///
/// The parent could not be created, or the file could not be opened.
pub(crate) async fn open_append(path: &Path) -> io::Result<tokio::fs::File> {
    check_log_ancestry(path)
        .inspect_err(|error| tracing::error!(?path, %error, "log ancestry check failed"))?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let mut builder = tokio::fs::DirBuilder::new();
        builder.recursive(true);
        // No `DIR_MODE` on Windows: no scalar mode to set. See
        // `boot::create_dir_at_dir_mode`'s Windows arm.
        #[cfg(unix)]
        builder.mode(DIR_MODE);
        if let Err(error) = builder.create(parent).await {
            tracing::error!(?path, %error, "log directory create failed");
            return Err(error);
        }
    }

    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    open_log_path(&mut options, path)
        .await
        .inspect_err(|error| tracing::error!(?path, %error, "log file open failed"))
}

/// Writes lines to one child's stdin, one at a time, acknowledging each.
///
/// Serial on purpose: two concurrent writers to one pipe can interleave
/// mid-line, and a REPL reading the result would see a command neither
/// caller sent. A line queued behind one the app is not reading waits, so
/// the caller bounds its own wait; abandoning a write halfway would leave a
/// partial line in the pipe.
///
/// A request whose caller has stopped listening is dropped rather than
/// written: delivering later would send a line the operator was told was not
/// written. The line already inside `write_all` is past that point. Ends
/// when the last sender drops, giving the app EOF on stdin.
fn spawn_stdin_pump<W>(stdin: Option<W>, mut rx: mpsc::Receiver<StdinWrite>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut stdin) = stdin else {
            // `Stdio::piped()` was set and `child.stdin` was still `None`.
            // Answering nothing would hang every caller.
            while let Some(StdinWrite { done, .. }) = rx.recv().await {
                let _ = done.send(Err(RunnerError::WriteFailed(
                    "this child has no stdin pipe".to_string(),
                )));
            }
            return;
        };
        while let Some(StdinWrite { line, done }) = rx.recv().await {
            if done.is_closed() {
                // The supervisor's `STDIN_WRITE_TIMEOUT` expired and it
                // dropped the receiver. Writing now would deliver a line the
                // operator was already told was not written.
                continue;
            }
            let mut bytes = line.into_bytes();
            // Exactly one terminator, appended here and nowhere else: the
            // wire carries the line without one (`Request::SendLine::line`).
            bytes.push(b'\n');
            let result = match stdin.write_all(&bytes).await {
                Ok(()) => stdin.flush().await,
                Err(error) => Err(error),
            };
            let _ = done.send(result.map_err(|error| RunnerError::WriteFailed(error.to_string())));
        }
    });
}

/// Wires the daemon side of the shepherd channel: a reader task decodes
/// newline-JSON [`ChildMessage`]s onto `from_child_tx`; a writer task encodes
/// [`ShepherdMessage`]s taken from `to_child_rx` back onto the socket.
///
/// Generic over the transport: the daemon's end is a `UnixStream` half of a
/// socketpair on unix and an accepted named-pipe server instance on Windows.
/// `tokio::io::split` rather than `UnixStream::into_split`, since
/// `NamedPipeServer` has no `into_split` of its own.
fn spawn_channel_pumps<S>(
    daemon_end: S,
    from_child_tx: mpsc::Sender<ChildMessage>,
    mut to_child_rx: mpsc::Receiver<ShepherdMessage>,
) where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(daemon_end);

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

    /// Lines longer than `LOG_BUFFER` force the multi-`poll_write` case
    /// `O_APPEND` alone cannot make atomic, which is what this guards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn narration_cannot_tear_a_line_the_pump_is_writing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("torn.log");

        let long = "x".repeat(LOG_BUFFER + 512);
        let pump = {
            let path = path.clone();
            let long = long.clone();
            tokio::spawn(async move {
                let mut log = LogFile::open(path).await;
                for i in 0..1200 {
                    log.append(&format!("{i} {long}")).await;
                    if i % 4 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                log.flush().await.expect("the pump's own flush");
            })
        };
        let narrator = {
            let path = path.clone();
            tokio::spawn(async move {
                for i in 0..600 {
                    let mut written = String::new();
                    stamp_into(&mut written);
                    written.push_str(&format!("[shep] narration {i}\n"));
                    let file = open_append(&path).await.expect("the narration handle");
                    let _record = record_lock(&path).lock_owned().await;
                    let mut file = file;
                    use tokio::io::AsyncWriteExt as _;
                    file.write_all(written.as_bytes()).await.expect("write");
                    file.flush().await.expect("flush");
                }
            })
        };
        pump.await.expect("the pump task");
        narrator.await.expect("the narration task");

        let text = std::fs::read_to_string(&path).expect("the log is readable");
        let mut lines = 0_usize;
        for line in text.lines() {
            lines += 1;
            let once = shep_core::logstamp::strip(line);
            assert_ne!(
                once, line,
                "a line reached the file with no stamp: {line:.80}"
            );
            assert_eq!(
                shep_core::logstamp::strip(once),
                once,
                "a line carries two stamps, so a record was torn: {line:.80}"
            );
        }
        assert_eq!(lines, 1800, "every record reached the file exactly once");
    }
    use core::pin::Pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll};
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::collections::BTreeSet;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::DuplexStream;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::*;
    #[cfg(unix)]
    use crate::handover::CarriedFds;

    // Tests needing a real child live in `tests/real_runner.rs`; a
    // `tokio::io::duplex` half stands in for the pump's `AsyncRead` here.
    // `a_flush_reports_the_write_its_file_never_took` drives a `LogFile`
    // directly, the only way to force a write failure. Real clock, not paused.

    /// How long a pump gets to answer before a test calls it hung. A pump
    /// that is working answers in microseconds; this is slack for a loaded
    /// runner, not an expected duration.
    const PUMP_DEADLINE: Duration = Duration::from_secs(5);

    /// Room in each in-memory pipe standing in for a child's stdout/stderr.
    ///
    /// Sized so a case can hand the pump more lines than `logs` will hold
    /// without the writing side parking first: with a buffer that small,
    /// "the pump stopped reading" and "the test stopped writing" become the
    /// same observation.
    const STREAM_BUFFER: usize = 4096;

    /// One pump over two streams and two real files: everything
    /// [`spawn_log_pump`] takes, with no child process involved.
    ///
    /// Generic over the writing side only, so the descriptor cases can swap
    /// the in-memory pair for a real one: an in-memory pipe has no
    /// descriptor, so the pump is told its stream numbers rather than
    /// reading them off its own readers. Every case about bytes rather
    /// than descriptors uses the cheaper [`PumpHarness::start`].
    struct PumpHarness<W = DuplexStream> {
        dir: tempfile::TempDir,
        out_path: PathBuf,
        err_path: PathBuf,
        out_writer: W,
        err_writer: W,
        logs: mpsc::Receiver<LogLine>,
        ctl: mpsc::Sender<LogCtl>,
        /// The stream descriptor numbers this harness handed the pump, which
        /// is what a descriptor report has to answer with.
        pipes: PipeFds,
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
                LogSink::Path(out_path.clone()),
                LogSink::Path(err_path.clone()),
                logs_tx,
                ctl_rx,
                PipeFds::default(),
            );
            Self {
                dir,
                out_path,
                err_path,
                out_writer,
                err_writer,
                logs,
                ctl,
                pipes: PipeFds::default(),
            }
        }
    }

    /// A pump reading two real pipes, for the cases that are about
    /// descriptor numbers rather than about bytes.
    ///
    /// `tokio::net::unix::pipe` is what a child's stdout actually is, so the
    /// numbers this hands the pump are the same kind of thing a spawn hands
    /// it, and the test can hold the writing ends open for as long as it
    /// needs the reading ends to stay valid.
    #[cfg(unix)]
    impl PumpHarness<tokio::net::unix::pipe::Sender> {
        fn start_over_pipes() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let out_path = dir.path().join("out.log");
            let err_path = dir.path().join("err.log");
            let (out_writer, out_reader) = tokio::net::unix::pipe::pipe().unwrap();
            let (err_writer, err_reader) = tokio::net::unix::pipe::pipe().unwrap();
            let pipes = PipeFds {
                out: Some(out_reader.as_raw_fd()),
                err: Some(err_reader.as_raw_fd()),
                stdin: None,
                channel: None,
            };
            let (logs_tx, logs) = mpsc::channel(CHANNEL_CAPACITY);
            let (ctl, ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
            spawn_log_pump(
                Some(out_reader),
                Some(err_reader),
                LogSink::Path(out_path.clone()),
                LogSink::Path(err_path.clone()),
                logs_tx,
                ctl_rx,
                pipes,
            );
            Self {
                dir,
                out_path,
                err_path,
                out_writer,
                err_writer,
                logs,
                ctl,
                pipes,
            }
        }
    }

    impl<W: AsyncWrite + Unpin> PumpHarness<W> {
        /// Sends a [`LogCtl::ReportFds`] and waits for the answer.
        ///
        /// Generic over the writing side rather than living with the
        /// descriptor cases, because what a report does to the pump is not
        /// about descriptors at all: it flushes, it drains, and it parks.
        /// The cases about those three run over the cheaper in-memory pair.
        #[cfg(unix)]
        async fn report_fds(&self) -> CarriedFds {
            let (done, ack) = oneshot::channel();
            self.ctl
                .send(LogCtl::ReportFds { done })
                .await
                .expect("the pump must still be reading its control channel");
            timeout(PUMP_DEADLINE, ack)
                .await
                .expect("a descriptor report must be acknowledged")
                .expect("the pump must answer rather than drop the acknowledgement")
        }

        /// Sends a [`LogCtl::Resume`], which carries no acknowledgement.
        ///
        /// Nothing to wait for by design (see the variant): every case that
        /// resumes a pump then waits on what the pump does next, which is a
        /// stronger barrier than an answer would be.
        #[cfg(unix)]
        async fn resume(&self) {
            self.ctl
                .send(LogCtl::Resume)
                .await
                .expect("a parked pump must still be reading its control channel");
        }

        /// Writes one line into the chosen stream and waits for the pump to
        /// hand it back on `logs`, proof it read the line and issued the
        /// file write. Also orders the two streams: a test never has to
        /// guess which line arrives first.
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
        /// requires success: every caller reopens paths the pump can open.
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
        /// requires success: every caller flushes handles the pump can write.
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

    use shep_core::logstamp::LOG_STAMP_BYTES;

    /// One log file's contents with [`LogFile::append`]'s per-line stamp
    /// taken back off, so a test can assert on the sheep's own bytes.
    ///
    /// The stamp is the daemon's and moves every run, so pinning it would
    /// assert on the clock. [`a_line_carries_the_time_it_was_written`] checks
    /// the stamp itself; this checks it is present and well formed as a
    /// side effect of parsing (see [`unstamped`]).
    fn log_text(path: &Path) -> String {
        unstamped(&fs::read_to_string(path).unwrap())
    }

    /// Drops the per-line stamp from every line of `text` that carries one,
    /// leaving the rest untouched.
    ///
    /// Uses [`shep_core::logstamp::strip`], the same call `bleats` makes, so
    /// tests compare against what an operator sees. Recognises a stamp
    /// rather than assuming one: some tests write a line straight to the log
    /// file, ahead of any pump.
    fn unstamped(text: &str) -> String {
        let mut out = String::new();
        for line in text.lines() {
            out.push_str(shep_core::logstamp::strip(line));
            out.push('\n');
        }
        out
    }

    /// Waits for `path` to hold exactly `expected`, ignoring the per-line
    /// stamps (see [`log_text`]).
    ///
    /// A line on `logs` has reached the stream's buffer, not necessarily the
    /// file (see [`spawn_log_pump`]'s ordering note), so this polls for
    /// [`IDLE_FLUSH`] to write it through. A reopen acknowledgement would be
    /// a cleaner barrier, but the point here is what the current handle
    /// does, and a reopen replaces it.
    async fn assert_file_settles(path: &Path, expected: &str) {
        let settled = timeout(PUMP_DEADLINE, async {
            while unstamped(&fs::read_to_string(path).unwrap_or_default()) != expected {
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

    /// Whether `fd` names something open in this process.
    ///
    /// `F_GETFD` is the cheapest question the kernel answers about a
    /// descriptor, and it is the one `handover::fds` already asks.
    #[cfg(unix)]
    fn is_open(fd: RawFd) -> bool {
        nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).is_ok()
    }

    /// Fails if a descriptor report answers with anything but the four
    /// numbers the pump is really holding.
    ///
    /// Checks exact equality against the harness's own numbers, not merely
    /// that something is open, which a wrong-but-open descriptor would pass.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pump_reports_the_descriptors_it_holds() {
        let pump = PumpHarness::start_over_pipes();
        let fds = pump.report_fds().await;

        assert_eq!(fds.out_pipe, pump.pipes.out, "stdout's read end");
        assert_eq!(fds.err_pipe, pump.pipes.err, "stderr's read end");
        assert!(
            fds.out_log.is_some() && fds.err_log.is_some(),
            "a pump that opened both log files must name both handles: {fds:?}"
        );

        let named: Vec<RawFd> = fds.all().into_iter().flatten().collect();
        assert_eq!(named.len(), 4, "a running sheep has all four: {fds:?}");
        for fd in &named {
            assert!(
                is_open(*fd),
                "the blob would name a closed descriptor: {fd}"
            );
        }
        let distinct: BTreeSet<RawFd> = named.iter().copied().collect();
        assert_eq!(distinct.len(), 4, "four descriptors, four numbers: {fds:?}");
    }

    /// Fails if both pipes filled to capacity cannot be drained.
    ///
    /// The bound [`FINAL_DRAIN`] rests on: a reaped child cannot leave more
    /// than its pipes' capacity behind, so two full pipes is the worst case.
    /// Both streams, since `final_drain` selects between them. Runs over
    /// real pipes, since the kernel's capacity is the one that matters; the
    /// line count is read off the fill rather than assumed.
    ///
    /// Waits [`PUMP_DEADLINE`], not [`FINAL_DRAIN`]: binding to the latter
    /// would assert this machine's scheduling speed rather than the drain's
    /// completeness.
    #[cfg(unix)]
    #[tokio::test]
    async fn both_pipes_filled_to_capacity_drain_inside_the_budget() {
        let pump = PumpHarness::start_over_pipes();
        // 64 bytes a line: short enough to be a round fraction of a pipe,
        // long enough that filling one does not take many thousands of
        // syscalls.
        let line = format!("{}\n", "x".repeat(63));
        let out_lines = fill_pipe(&pump.out_writer, &line).await;
        let err_lines = fill_pipe(&pump.err_writer, &line).await;
        assert!(
            out_lines > 0 && err_lines > 0,
            "neither pipe took a whole line: stdout={out_lines} stderr={err_lines}"
        );

        drop(pump.out_writer);
        drop(pump.err_writer);
        drop(pump.logs);

        for (path, want, stream) in [
            (&pump.out_path, out_lines, "stdout"),
            (&pump.err_path, err_lines, "stderr"),
        ] {
            let settled = timeout(PUMP_DEADLINE, async {
                while fs::read_to_string(path).unwrap_or_default().lines().count() < want {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await;
            assert!(
                settled.is_ok(),
                "a full {stream} pipe did not drain inside {FINAL_DRAIN:?}: {want} lines \
                 written, {} landed",
                fs::read_to_string(path).unwrap_or_default().lines().count()
            );
        }
    }

    /// How many times [`a_last_line_written_before_the_sheep_task_lets_go_reaches_the_file`]
    /// reconstructs the race.
    ///
    /// The two ready branches are picked at random, so a broken pump passes
    /// one attempt about two times in five. Thirty-two takes that to
    /// roughly 2e-13, costing nothing extra once the pump is right.
    const RACE_ATTEMPTS: usize = 32;

    /// Fills one pipe with whole lines until the kernel refuses another, and
    /// answers with how many went in.
    ///
    /// Uses `try_write`, not a timed `write_all`: saturation must be
    /// something the kernel reports, since a full pipe and a merely slow
    /// pump look identical to a wait. A short write's partial line is left
    /// uncounted, so the answer is an exact count of whole lines.
    ///
    /// `cfg(unix)`: `tokio::net::unix` does not exist on Windows.
    #[cfg(unix)]
    async fn fill_pipe(writer: &tokio::net::unix::pipe::Sender, line: &str) -> usize {
        // `try_write` reports `WouldBlock` for a not-yet-writable pipe as
        // readily as for a full one, so this establishes writability first,
        // before the loop starts counting refusals as saturation.
        timeout(PUMP_DEADLINE, writer.writable())
            .await
            .expect("an empty pipe is writable")
            .expect("the pipe is open");

        let mut whole = 0usize;
        loop {
            match writer.try_write(line.as_bytes()) {
                Ok(written) if written == line.len() => whole += 1,
                Ok(_) => return whole,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return whole,
                Err(error) => {
                    panic!("the fill failed for a reason that is not a full pipe: {error}")
                }
            }
        }
    }

    /// Fails if a line the child wrote before its sheep task let go never
    /// reaches the log file.
    ///
    /// The pump's `select!` has a branch for the sheep task dropping its
    /// `logs` receiver, for a lamb that holds the pipe open past the
    /// child's own exit. That branch competes with the read branches rather
    /// than following them, so a child that writes and exits can leave both
    /// ready in the same poll.
    ///
    /// No child here: the harness writes the line itself and then does what
    /// `run_sheep` does on return, isolating the race from the process
    /// lifecycle that normally hides it.
    #[tokio::test]
    async fn a_last_line_written_before_the_sheep_task_lets_go_reaches_the_file() {
        for attempt in 0..RACE_ATTEMPTS {
            let mut pump = PumpHarness::start();
            pump.out_writer.write_all(b"last-words\n").await.unwrap();
            // The child exiting, then `run_sheep` breaking out of its loop:
            // the write end closes and the receiver goes, in that order,
            // with the line still unread in between.
            drop(pump.out_writer);
            drop(pump.err_writer);
            drop(pump.logs);

            let settled = timeout(PUMP_DEADLINE, async {
                while !fs::read_to_string(&pump.out_path)
                    .unwrap_or_default()
                    .contains("last-words")
                {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await;
            assert!(
                settled.is_ok(),
                "attempt {attempt}: the pump dropped the line it had not read yet, \
                 leaving {:?}",
                fs::read_to_string(&pump.out_path)
            );
        }
    }

    /// The same line, lost the other way: through the control channel
    /// rather than through `logs`.
    ///
    /// `logs` and `log_ctl` do not close together ordinarily: the sheep task
    /// drops `logs` when its child exits, while the slot's `log_ctl` sender
    /// outlives it until a delete or shutdown. Those are the cases where
    /// both close at once, giving the control arm's own `None` a fourth way
    /// out of the loop that can win the same random pick.
    ///
    /// The drain runs once, on the way out, not inside whichever branch
    /// triggered it, so it cannot matter which exit was taken.
    #[tokio::test]
    async fn a_last_line_survives_the_control_channel_closing_with_the_logs() {
        for attempt in 0..RACE_ATTEMPTS {
            let mut pump = PumpHarness::start();
            pump.out_writer.write_all(b"last-words\n").await.unwrap();
            drop(pump.out_writer);
            drop(pump.err_writer);
            // A delete: the slot's sender and the sheep task's receiver go
            // in the same moment, so all four exits are live at once.
            drop(pump.ctl);
            drop(pump.logs);

            let settled = timeout(PUMP_DEADLINE, async {
                while !fs::read_to_string(&pump.out_path)
                    .unwrap_or_default()
                    .contains("last-words")
                {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await;
            assert!(
                settled.is_ok(),
                "attempt {attempt}: the pump took an exit that skips the drain, \
                 leaving {:?}",
                fs::read_to_string(&pump.out_path)
            );
        }
    }

    /// Fails if two instances of one `merge_logs` app end up naming one
    /// descriptor number for the file they share.
    ///
    /// `handover::adopt::refuse_repeated_fds` rejects the entire handover on
    /// any repeated number, so a shared descriptor here would fail every
    /// reload of every flock containing the app, not just this one.
    ///
    /// One inode, two `open`s, two numbers: each instance runs its own
    /// [`LogFile::open`] rather than sharing a handle. `merge_logs` makes
    /// both instances resolve to the same two paths, since `assemble` drops
    /// the `-<instance>` suffix.
    #[cfg(unix)]
    #[tokio::test]
    async fn two_pumps_on_one_log_path_report_different_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("merged-out.log");
        let err_path = dir.path().join("merged-err.log");

        // Held for the whole case: a reading end is only a valid number
        // while its writing end is alive, and a pump that reached EOF clears
        // the number it would otherwise report.
        let mut writers = Vec::new();
        let mut reports = Vec::new();
        for _ in 0..2 {
            let (out_writer, out_reader) = tokio::net::unix::pipe::pipe().unwrap();
            let (err_writer, err_reader) = tokio::net::unix::pipe::pipe().unwrap();
            let pipes = PipeFds {
                out: Some(out_reader.as_raw_fd()),
                err: Some(err_reader.as_raw_fd()),
                stdin: None,
                channel: None,
            };
            let (logs_tx, _logs) = mpsc::channel(CHANNEL_CAPACITY);
            let (ctl, ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
            spawn_log_pump(
                Some(out_reader),
                Some(err_reader),
                LogSink::Path(out_path.clone()),
                LogSink::Path(err_path.clone()),
                logs_tx,
                ctl_rx,
                pipes,
            );
            let (done, ack) = oneshot::channel();
            ctl.send(LogCtl::ReportFds { done }).await.unwrap();
            let fds = timeout(PUMP_DEADLINE, ack)
                .await
                .expect("a descriptor report must be acknowledged")
                .expect("the pump must answer rather than drop the acknowledgement");
            // `_logs` and `ctl` are kept alive alongside the writers: a pump
            // whose control channel closed would end and close the very
            // handles whose numbers are being compared.
            writers.push((out_writer, err_writer, ctl, _logs));
            reports.push(fds);
        }

        let named: Vec<RawFd> = reports
            .iter()
            .flat_map(|fds| fds.all().into_iter().flatten())
            .collect();
        assert_eq!(
            named.len(),
            8,
            "two running instances, four descriptors each: {reports:?}"
        );
        let distinct: BTreeSet<RawFd> = named.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            8,
            "a repeat here refuses the whole handover, not just this app: {reports:?}"
        );
        // The pointed half of the assertion above: the two log handles are
        // the pair that could plausibly have been shared, since they are the
        // only two of the eight opened on the same path.
        assert_ne!(
            reports[0].out_log, reports[1].out_log,
            "two instances sharing one log file must still hold two handles"
        );
        assert_ne!(reports[0].err_log, reports[1].err_log);
        drop(writers);
    }

    /// Fails if a report still names a stream whose descriptor the pump has
    /// already let go of.
    ///
    /// Exercises the `files.pipes.out = None` clearing in the `StreamEnded`
    /// arm: [`a_pump_reports_the_descriptors_it_holds`] reports while both
    /// streams are live, so it passes with or without that clearing.
    ///
    /// A closed number is free for the next `open` in this process, so a
    /// stale report risks the successor adopting an unrelated file as this
    /// sheep's stdout. `err_pipe` staying at its own number is what keeps
    /// this a case about the ended stream, not a dead pump.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_report_after_a_stream_ends_names_no_descriptor_for_it() {
        let mut pump = PumpHarness::start_over_pipes();
        pump.feed(false, "before-the-eof").await;

        drop(pump.out_writer);
        let_the_pump_settle().await;

        // Sent through the field, not `report_fds`: dropping the writer
        // above moves out of the harness, so `&self` cannot be called.
        let (done, ack) = oneshot::channel();
        pump.ctl
            .send(LogCtl::ReportFds { done })
            .await
            .expect("a pump with one live stream still reads its control channel");
        let fds = timeout(PUMP_DEADLINE, ack)
            .await
            .expect("a descriptor report must be acknowledged")
            .expect("the pump must answer rather than drop the acknowledgement");

        assert_eq!(
            fds.out_pipe, None,
            "stdout is at EOF and its descriptor is gone: {fds:?}"
        );
        assert_eq!(
            fds.err_pipe, pump.pipes.err,
            "stderr is still live and keeps its own number: {fds:?}"
        );
        assert!(
            fds.out_log.is_some() && fds.err_log.is_some(),
            "both log handles are held whichever stream ended: {fds:?}"
        );
    }

    /// Fails if a report is answered before what the pump is holding has
    /// reached the file.
    ///
    /// Written before the report, readable on disk after it, with no
    /// settling in between: not [`assert_file_settles`], since polling
    /// would let [`IDLE_FLUSH`] pass a case the report's own flush should
    /// have covered. A blob whose descriptors are ready but whose bytes are
    /// not is a log gap the successor cannot repair, since the bytes died
    /// with the image at the exec.
    #[cfg(unix)]
    #[tokio::test]
    async fn reporting_flushes_first() {
        let mut pump = PumpHarness::start_over_pipes();
        pump.feed(false, "before-the-blob").await;
        pump.feed(true, "and-on-stderr").await;

        let _ = pump.report_fds().await;

        assert_eq!(log_text(&pump.out_path), "before-the-blob\n");
        assert_eq!(log_text(&pump.err_path), "and-on-stderr\n");
    }

    /// How long a case watches a parked pump before believing it.
    ///
    /// Several [`IDLE_FLUSH`] windows, because that is what turns a pump
    /// that read a line into a file that shows one: a pump still reading
    /// appends within microseconds and the idle flush writes it through
    /// 50ms later, so a file unchanged across six of those windows is a
    /// pump that never read.
    #[cfg(unix)]
    const STILL_WINDOWS: u32 = 6;

    /// Lines a case writes straight into a stream, bypassing
    /// [`PumpHarness::feed`], when the point is a pump that has fallen
    /// behind rather than one keeping up.
    ///
    /// More than `CHANNEL_CAPACITY`, so the pump fills `logs`, parks in
    /// [`reserve_slot`], and holds the rest in its reader.
    #[cfg(unix)]
    const BURST: u32 = 60;

    /// Fails if `path` changes at all over the next few flush windows.
    ///
    /// The negative half of the parking case, and the reason it is a window
    /// rather than a single read: a pump that is still reading loses this
    /// on its first poll, while a parked one cannot lose it at any length.
    #[cfg(unix)]
    async fn assert_file_holds_still(path: &Path, expected: &str) {
        for window in 1..=STILL_WINDOWS {
            tokio::time::sleep(IDLE_FLUSH).await;
            assert_eq!(
                log_text(path),
                expected,
                "{}: a parked pump wrote during flush window {window}",
                path.display()
            );
        }
    }

    /// Waits until nothing more will fit on `logs`, which is the state a
    /// handover finds a chatty sheep's pump in.
    ///
    /// Forces both cases below: with the channel full, the pump parks
    /// inside [`reserve_slot`], so everything read past that point sits in
    /// its reader until a report drains it.
    #[cfg(unix)]
    async fn wait_for_a_full_logs_channel(logs: &mpsc::Receiver<LogLine>) {
        let filled = timeout(PUMP_DEADLINE, async {
            while logs.len() < CHANNEL_CAPACITY {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            filled.is_ok(),
            "the pump must fall behind a burst it cannot forward"
        );
    }

    /// Fails if a pump goes on reading its sheep's streams after it has
    /// reported.
    ///
    /// A report is a snapshot: descriptors, and a flush that empties the
    /// write buffer behind them, taken before the exec that consumes it.
    /// Anything read afterward is written by an image about to be replaced,
    /// landing in neither the pipe the successor inherits nor the buffer it
    /// could have been handed. Parking keeps the snapshot true until used.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pump_that_has_reported_stops_reading_until_it_is_resumed() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "before-the-report").await;

        let _ = pump.report_fds().await;

        // The sheep does not stop writing because its shepherd is being
        // replaced. Every one of these has to still be in the pipe at the
        // exec, which is the same claim as the file not growing.
        pump.out_writer
            .write_all(b"after-1\nafter-2\n")
            .await
            .unwrap();
        assert_file_holds_still(&pump.out_path, "before-the-report\n").await;

        pump.resume().await;
        assert_file_settles(&pump.out_path, "before-the-report\nafter-1\nafter-2\n").await;
    }

    /// Fails if a report leaves lines stranded in the pump's reader.
    ///
    /// A quiet sheep leaves the reader empty at every instant a report
    /// could land, so this needs a busy one: filling `logs` first leaves
    /// everything past that sitting in a userspace buffer the exec destroys
    /// and no descriptor carries.
    ///
    /// Asserted with no settling: the report's own flush is the barrier.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_report_lands_what_the_reader_was_holding() {
        let mut pump = PumpHarness::start();
        let burst: String = (1..=BURST).map(|n| format!("{n}\n")).collect();
        pump.out_writer.write_all(burst.as_bytes()).await.unwrap();
        wait_for_a_full_logs_channel(&pump.logs).await;

        let _ = pump.report_fds().await;

        assert_eq!(
            log_text(&pump.out_path),
            burst,
            "every line the reader held must be on disk once, in order, \
             before the report is answered"
        );
    }

    /// Fails if a report takes bytes off a pipe that it does not write.
    ///
    /// Every byte the sheep wrote must be in the log file or still in the
    /// pipe when the report answers, since the successor inherits the pipe
    /// by descriptor number. A byte in neither is destroyed at the `execve`.
    ///
    /// The inherited handle is a `try_clone` sharing one open file
    /// description with the reader, so it sees what the successor would.
    ///
    /// [`drain_ready`]'s documented residual is the one line of slack: a
    /// part-way line splits between the reader's buffer and `Lines`'
    /// accumulator, unreachable here. Fails at two lines lost, not one.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_report_leaves_in_the_pipe_everything_it_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("out.log");
        let (reader, mut writer) = std::io::pipe().unwrap();
        // The successor's view, taken before the pump owns the reader.
        let inherited = reader.try_clone().unwrap();
        let reader =
            tokio::net::unix::pipe::Receiver::from_owned_fd(OwnedFd::from(reader)).unwrap();
        let (logs_tx, logs) = mpsc::channel(CHANNEL_CAPACITY);
        let (ctl, ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let pipes = PipeFds {
            out: Some(reader.as_raw_fd()),
            err: None,
            stdin: None,
            channel: None,
        };
        spawn_log_pump(
            Some(reader),
            None::<DuplexStream>,
            LogSink::Path(out_path.clone()),
            LogSink::Path(dir.path().join("err.log")),
            logs_tx,
            ctl_rx,
            pipes,
        );

        // More than one `READ_BUFFER`, so the reader is full and the pipe
        // still holds the rest; under `SMALLEST_PIPE`, so this write does
        // not park against a pump that has stopped draining.
        let lines = u32::try_from(SMALLEST_PIPE * 3 / 4 / RUN_LINE).unwrap();
        let run: String = (1..=lines).map(|n| format!("{n:07}\n")).collect();
        assert!(run.len() > READ_BUFFER, "the reader must fill");
        std::io::Write::write_all(&mut writer, run.as_bytes()).unwrap();
        wait_for_a_full_logs_channel(&logs).await;

        let (done, ack) = oneshot::channel();
        ctl.send(LogCtl::ReportFds { done }).await.unwrap();
        timeout(PUMP_DEADLINE, ack)
            .await
            .expect("a descriptor report must be acknowledged")
            .expect("the pump must answer rather than drop the acknowledgement");

        // The sheep goes quiet, so the inherited handle reaches EOF rather
        // than waiting for a writer that outlives the test.
        drop(writer);
        let rest = read_to_eof(inherited).await;
        let written = log_text(&out_path);
        assert!(
            run.starts_with(&written),
            "the pump must write the run's own bytes, in order"
        );
        assert!(
            run.ends_with(&rest),
            "what is left in the pipe must be the run's own tail"
        );
        assert!(
            written.len() + rest.len() >= run.len() - RUN_LINE,
            "{} of {} bytes reached neither the file ({}) nor the pipe ({}): the report read \
             them out of the pipe and then dropped them",
            run.len() - written.len() - rest.len(),
            run.len(),
            written.len(),
            rest.len()
        );
    }

    /// Everything still readable from a pipe handle, to EOF.
    ///
    /// `WouldBlock` is retried rather than treated as the end: the handle
    /// shares its open file description with a `tokio` reader, which put
    /// the description in non-blocking mode, so an empty moment reads as an
    /// error rather than as a wait.
    #[cfg(unix)]
    async fn read_to_eof(handle: std::io::PipeReader) -> String {
        let mut out = Vec::new();
        let mut buf = [0_u8; 4096];
        let drained = timeout(PUMP_DEADLINE, async {
            loop {
                match std::io::Read::read(&mut &handle, &mut buf) {
                    Ok(0) => return,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(error) => panic!("the successor's handle must be readable: {error}"),
                }
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "the pipe must reach EOF once the sheep is gone"
        );
        String::from_utf8(out).expect("a pipe of ASCII lines")
    }

    /// The smallest buffer a pipe starts with on any host these cases run
    /// on: macOS opens one at 16 KiB, Linux at 64.
    ///
    /// The case below writes its whole run in one go into a pipe whose
    /// reader has stopped draining it, so the run has to fit under this or
    /// the writing side parks and the test deadlocks instead of failing.
    #[cfg(unix)]
    const SMALLEST_PIPE: usize = 16 * 1024;

    /// One line of the run below, sized so the arithmetic in it is exact.
    #[cfg(unix)]
    const RUN_LINE: usize = 8;

    /// Fails if a report follows a sheep that is still writing instead of
    /// rescuing what its reader already held.
    ///
    /// A reader can strand at most one [`READ_BUFFER`], and the drain
    /// writes whole lines out of that buffer without reading the pipe
    /// behind it, so it cannot write more than the buffer held however fast
    /// the sheep is writing. The slack is one line: [`tokio::io::Lines`]
    /// may have been part-way through one before the buffer was filled.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_report_drains_at_most_one_bufferful() {
        let mut pump = PumpHarness::start_over_pipes();
        // Three quarters of the smallest pipe: over `MAX_DRAIN`, so the
        // bound is really reached, and under the pipe, so the writer never
        // parks.
        let lines = u32::try_from(SMALLEST_PIPE * 3 / 4 / RUN_LINE).unwrap();
        let run: String = (1..=lines).map(|n| format!("{n:07}\n")).collect();
        assert!(run.len() > READ_BUFFER, "the run must reach the bound");
        pump.out_writer.write_all(run.as_bytes()).await.unwrap();
        wait_for_a_full_logs_channel(&pump.logs).await;
        pump.flush().await;
        let before = fs::metadata(&pump.out_path).unwrap().len();

        let _ = pump.report_fds().await;

        let drained = fs::metadata(&pump.out_path).unwrap().len() - before;
        assert!(
            drained > 0,
            "the report must rescue what the reader was holding"
        );
        // Bound is on the sheep's bytes, one bufferful plus a part-way
        // line, but measured on the file where each line carries a
        // `LOG_STAMP_BYTES` stamp. Counting the stamp in keeps this a bound
        // on the drain, not on stamp width.
        const RESCUED: usize = READ_BUFFER + RUN_LINE;
        let ceiling = u64::try_from(RESCUED + RESCUED / RUN_LINE * LOG_STAMP_BYTES).unwrap();
        assert!(
            drained <= ceiling,
            "the report drained {drained} bytes against a ceiling of {ceiling}, which is \
             more than one bufferful: it is following the sheep rather than catching up"
        );
    }

    /// A sink standing in for the [`tokio::fs::File`] a real [`LogFile`]
    /// holds, counting the writes that reach it.
    ///
    /// Bytes on disk come out the same whether or not appends are batched;
    /// only the write count shows it, so a counter is what pins
    /// [`LOG_BUFFER`] against a future append that writes straight through.
    #[derive(Clone, Debug, Default)]
    struct WriteCounter {
        writes: Arc<AtomicUsize>,
        bytes: Arc<AtomicUsize>,
    }

    impl WriteCounter {
        fn writes(&self) -> usize {
            self.writes.load(Ordering::Relaxed)
        }

        fn bytes(&self) -> usize {
            self.bytes.load(Ordering::Relaxed)
        }
    }

    impl AsyncWrite for WriteCounter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(buf.len(), Ordering::Relaxed);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Fails if an appended line still costs one write to the file.
    ///
    /// `tokio::fs::File` hands each write to the blocking pool: 32.8 us of
    /// daemon CPU per line against 0.99 us for the `write(2)` underneath.
    /// A buffer turns N writes into one per bufferful, and the write count
    /// is the only observable that shows it.
    ///
    /// Paused clock: no time passes, so [`IDLE_FLUSH`] never fires, and
    /// every write counted is the buffer spilling or the closing flush.
    #[tokio::test(start_paused = true)]
    async fn a_run_of_lines_costs_one_write_per_bufferful_not_one_per_line() {
        // 69 characters: `append`'s newline makes a 70-byte line, the size
        // `LOG_BUFFER` was measured against. The stamp is counted
        // separately so 70 stays that number.
        const LINE: &str = "012345678901234567890123456789012345678901234567890123456789012345678";
        const LINE_BYTES: usize = LOG_STAMP_BYTES + LINE.len() + 1;
        let lines = 3 * LOG_BUFFER / LINE_BYTES;

        let sink = WriteCounter::default();
        let mut log = LogFile {
            record: record_lock(std::path::Path::new("test")),
            path: PathBuf::from("counted.log"),
            handle: Some(BufWriter::with_capacity(LOG_BUFFER, sink.clone())),
            buffered_since: None,
            stamp: String::new(),
        };
        for _ in 0..lines {
            log.append(LINE).await;
        }
        log.flush().await.unwrap();

        let total = lines * LINE_BYTES;
        let ceiling = total.div_ceil(LOG_BUFFER) + 1; // + the closing flush's partial buffer
        assert_eq!(
            sink.bytes(),
            total,
            "buffering must not lose or repeat a byte"
        );
        assert!(
            sink.writes() <= ceiling,
            "{lines} lines cost {} writes; one per bufferful plus the closing flush is {ceiling}",
            sink.writes()
        );
        assert!(
            sink.writes() < lines,
            "a write per line is the regression this exists to catch: \
             {lines} lines, {} writes",
            sink.writes()
        );
    }

    /// Fails if a line only leaves the buffer when the buffer fills or when
    /// something asks: that is, if [`IDLE_FLUSH`] bounds nothing.
    ///
    /// A sheep that logs once and goes quiet, with no `Flush`, no reopen,
    /// and no second line to push the first out. One line is nowhere near
    /// [`LOG_BUFFER`], so only the idle flush can write it through.
    ///
    /// Paused clock: the wait is on [`IDLE_FLUSH`], and a real 50ms would be
    /// a claim about the machine. Counting sink: a file would only confirm
    /// the bytes left once the blocking pool caught up, the same claim
    /// through the filesystem.
    #[tokio::test(start_paused = true)]
    async fn a_line_from_a_sheep_that_then_goes_quiet_still_reaches_its_file() {
        const LINE: &str = "the-only-line";
        let sink = WriteCounter::default();
        let mut log = LogFile {
            record: record_lock(std::path::Path::new("test")),
            path: PathBuf::from("quiet.log"),
            handle: Some(BufWriter::with_capacity(LOG_BUFFER, sink.clone())),
            buffered_since: None,
            stamp: String::new(),
        };

        log.append(LINE).await;
        assert_eq!(
            sink.bytes(),
            0,
            "one short line must sit in the buffer, or there is nothing for the idle flush to do"
        );

        let armed = log
            .buffered_since
            .expect("an appended line must arm the flush deadline");
        tokio::time::sleep_until(armed + IDLE_FLUSH).await;
        log.flush().await.unwrap();

        assert_eq!(
            sink.bytes(),
            LOG_STAMP_BYTES + LINE.len() + 1,
            "the stamp, the line and its newline must reach the file once the window closes"
        );
        assert_eq!(
            log.buffered_since, None,
            "a flush must retire the deadline, or the pump re-arms it every window"
        );
    }

    /// Fails if a line reaches its file without the time it was written.
    ///
    /// `mtime` answers for the whole file and only until something touches
    /// it again, so a per-line stamp is what an operator can trust after a
    /// rotation.
    ///
    /// Asserts the shape and width, not a value: the value is the wall
    /// clock, and pinning it would test the clock instead. The width is
    /// what every reader stripping the prefix depends on, and what
    /// [`LOG_STAMP_BYTES`] claims.
    #[tokio::test]
    async fn a_line_carries_the_time_it_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stamped.log");
        let mut log = LogFile::open(path.clone()).await;

        log.append("the-sheep-said-this").await;
        log.flush().await.expect("the file must be writable");

        let written = fs::read_to_string(&path).unwrap();
        let line = written.strip_suffix('\n').expect("one whole line");
        let (stamp, rest) = line.split_at(LOG_STAMP_BYTES);
        assert_eq!(
            rest, "the-sheep-said-this",
            "the sheep's own bytes must survive the prefix, whole and unaltered"
        );
        let stamp = stamp
            .strip_suffix(' ')
            .expect("one space must separate the stamp from the line");
        let parsed = chrono::DateTime::parse_from_rfc3339(stamp)
            .unwrap_or_else(|err| panic!("{stamp:?} must parse as RFC 3339: {err}"));
        // A minute of slack, against a line written a moment ago: wide
        // enough that a loaded runner cannot fail it, narrow enough that a
        // stamp reading the epoch, the wrong unit, or the wrong offset does.
        let drift = (chrono::Utc::now() - parsed.to_utc()).num_seconds().abs();
        assert!(
            drift < 60,
            "{stamp:?} is {drift}s from now; the stamp must be the moment the line was written"
        );
    }

    /// Fails if the idle-flush window is measured from the newest buffered
    /// line rather than the oldest: a stream logging just inside the window
    /// would then keep pushing the deadline out, and "buffered but not on
    /// disk" would have no bound at all.
    ///
    /// Paused clock: the question is arithmetic over `Instant`s, and an
    /// append that fits in the buffer touches no file to wait on.
    #[tokio::test(start_paused = true)]
    async fn the_idle_flush_window_is_measured_from_the_oldest_buffered_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = LogFiles {
            out: LogFile::open(dir.path().join("out.log")).await,
            err: LogFile::open(dir.path().join("err.log")).await,
            pipes: PipeFds::default(),
            #[cfg(unix)]
            parked: false,
        };
        assert_eq!(
            files.flush_deadline(),
            None,
            "an untouched pair owes no flush"
        );

        let oldest = Instant::now();
        files.stream(false).append("out-first").await;
        assert_eq!(files.flush_deadline(), Some(oldest + IDLE_FLUSH));

        // Both streams, so the deadline is answering for the pair rather
        // than for whichever one happened to be written last.
        tokio::time::advance(IDLE_FLUSH / 2).await;
        files.stream(false).append("out-second").await;
        files.stream(true).append("err-first").await;
        assert_eq!(
            files.flush_deadline(),
            Some(oldest + IDLE_FLUSH),
            "a later line must not push the window out"
        );

        files.flush_idle().await;
        assert_eq!(
            files.flush_deadline(),
            None,
            "a flush must retire the deadline it satisfied"
        );
    }

    /// Fails if a rotation loses or duplicates what the buffers were
    /// holding.
    ///
    /// Every line below fits in [`LOG_BUFFER`], so at the rename the file may
    /// hold none of them: a reopen that dropped its handle without flushing
    /// would lose the lot, and one that flushed after swapping handles would
    /// write them into the fresh file, leaving a gap in the archive and
    /// stale lines in the live log. Exact equality on both paths tells
    /// those apart from a reopen that got it right.
    #[tokio::test]
    async fn a_rotation_lands_every_buffered_line_exactly_once() {
        let mut pump = PumpHarness::start();
        let mut before = String::new();
        for n in 0..40 {
            let line = format!("before-{n}");
            pump.feed(false, &line).await;
            before.push_str(&line);
            before.push('\n');
        }
        assert!(
            before.len() < LOG_BUFFER,
            "the case needs lines small enough that the buffer may still hold them"
        );

        let rotated = pump.dir.path().join("out.log.1");
        fs::rename(&pump.out_path, &rotated).unwrap();
        pump.reopen().await;

        assert_eq!(log_text(&rotated), before);
        assert_eq!(log_text(&pump.out_path), "");

        let mut after = String::new();
        for n in 0..40 {
            let line = format!("after-{n}");
            pump.feed(false, &line).await;
            after.push_str(&line);
            after.push('\n');
        }
        pump.flush().await;

        assert_eq!(log_text(&pump.out_path), after);
        assert_eq!(
            log_text(&rotated),
            before,
            "the archive must have stopped growing at the swap"
        );
    }

    /// Fails if the `Reopen` arm acknowledges without opening the paths
    /// again: the renamed inodes keep receiving every later line and the
    /// live paths never come back, `create`-mode rotation silently
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
        assert_eq!(log_text(&rotated_out), "before-out\n");
        assert_eq!(log_text(&rotated_err), "before-err\n");
        assert_eq!(log_text(&pump.out_path), "");
        assert_eq!(log_text(&pump.err_path), "");

        pump.feed(false, "after-out").await;
        pump.feed(true, "after-err").await;
        pump.reopen().await; // second reopen, wanted here only as the flush

        assert_eq!(log_text(&pump.out_path), "after-out\n");
        assert_eq!(log_text(&pump.err_path), "after-err\n");
        // Both archives stopped growing the moment the handles were swapped.
        assert_eq!(log_text(&rotated_out), "before-out\n");
        assert_eq!(log_text(&rotated_err), "before-err\n");
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

        // Pushes the reopened handle's offset off zero: at offset zero,
        // appending and writing at a fixed position look identical.
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

    /// Fails if [`open_append`] stops opening through [`open_log_path`]:
    /// without its `O_NOFOLLOW`, a symlink planted at a sheep's `out_file`
    /// would be followed, and every line the sheep writes would land in
    /// whatever it points at. No other case here can see this, since every
    /// other one opens a real file.
    ///
    /// Asserts the target's bytes are unchanged, the discriminator from a
    /// followed link, and asserts the error message, which
    /// [`LogFile::reopen`] hands to an operator.
    ///
    /// `cfg(unix)`: needs `std::os::unix::fs::symlink`. `O_NOFOLLOW` has no
    /// Windows counterpart wired up yet, a gap named in the operator docs.
    #[cfg(unix)]
    #[tokio::test]
    async fn opening_a_symlinked_log_path_is_refused_rather_than_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("someone-elses.conf");
        let link = dir.path().join("web-out.log");
        fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let error = open_append(&link)
            .await
            .expect_err("a symlinked log path must not be opened for appending");

        assert_eq!(
            fs::read(&target).unwrap(),
            b"original",
            "a refused open must not have reached the symlink's target at all"
        );
        assert_eq!(
            error.to_string(),
            crate::runner::SYMLINK_REFUSED,
            "the operator who reads this needs the word symlink, not ELOOP's \
             own wording about levels of them"
        );
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
    /// the sheep task wait on an acknowledgement closes the loop: actor
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
        // proof the pump has read the last one and is parked on its send,
        // so the reopen below lands on a pump already waiting, not one
        // that merely might be.
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
    /// ends. A sheep whose stdout closes while stderr runs on would then
    /// never get its stdout log back from a rotation, and no other case
    /// here would notice, since every other one reopens with both streams
    /// still live.
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
    /// rotation worked while the sheep logs into nothing, the same silent
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
    /// drops a handle the way `Reopen` does.
    ///
    /// No polling: an answered flush means the files hold every line
    /// already handed to them, and a `Flush` that dropped a handle would
    /// leave the pump writing the stream nowhere from then on.
    ///
    /// Does not catch a [`LogFile::flush`] that answers without asking its
    /// file: on a write this small the content assertion would win that
    /// race anyway. [`a_flush_reports_the_write_its_file_never_took`] pins
    /// that leg deterministically instead.
    #[tokio::test]
    async fn a_flush_lands_both_streams_and_keeps_writing_afterwards() {
        let mut pump = PumpHarness::start();
        pump.feed(false, "out-before").await;
        pump.feed(true, "err-before").await;

        pump.flush().await;

        // No polling: the acknowledgement is the barrier this half of `shep
        // flush` exists to provide, and a test that polled for the content
        // would pass against a pump that never provided it.
        assert_eq!(log_text(&pump.out_path), "out-before\n");
        assert_eq!(log_text(&pump.err_path), "err-before\n");

        // A flush keeps the handle; a reopen replaces it. The next line
        // appends to what is already there rather than starting a file.
        pump.feed(false, "out-after").await;
        assert_file_settles(&pump.out_path, "out-before\nout-after\n").await;
    }

    /// Fails if [`LogFile::flush`] stops asking the file anything: an early
    /// `return Ok(())`, or a `map_err` traded for `.ok()`.
    ///
    /// `tokio::fs::File` reports a failed write on the next operation
    /// rather than the one that failed, since `write_all` returns once the
    /// real `write(2)` is queued. `flush` is the one that asks, so a flush
    /// that answered without asking would swallow the only signal a
    /// sheep's log went unwritten.
    ///
    /// Driven against a [`LogFile`] directly, not [`PumpHarness`]: a
    /// read-only handle makes the write fail deterministically, since
    /// `write(2)` on `O_RDONLY` is `EBADF` for every uid.
    #[tokio::test]
    async fn a_flush_reports_the_write_its_file_never_took() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        fs::write(&path, "").unwrap();
        let mut log = LogFile {
            record: record_lock(std::path::Path::new("test")),
            path: path.clone(),
            handle: Some(BufWriter::with_capacity(
                LOG_BUFFER,
                tokio::fs::File::open(&path).await.unwrap(),
            )),
            buffered_since: None,
            stamp: String::new(),
        };

        // Swallowed by design: the pump keeps draining a child whose log
        // it cannot write. The failure is owed at the flush instead.
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

    /// Fails if a log handle carried across a handover writes anywhere but
    /// the end of the file.
    ///
    /// `O_APPEND` is a file status flag on the open file description, so it
    /// crosses an exec with the descriptor. A handle that lost it writes at
    /// its own tracked offset instead, overwriting the first line here, or
    /// leaving a sparse hole after a `copytruncate` rotation. Reading both
    /// lines is what tells the two apart, not just checking non-empty.
    ///
    /// `cfg(unix)` alongside the constructor it drives, and the whole
    /// handover with it: Windows has no `execve`, so no image there is ever
    /// handed a log handle it did not open.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_log_file_from_an_open_handle_still_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.log");
        fs::write(&path, "first\n").unwrap();

        // Opened exactly as a predecessor's pump had it, and handed over
        // rather than reopened by path.
        let handle = open_append(&path).await.unwrap();
        let mut log = LogFile::from_file(path.clone(), handle);

        log.append("second").await;
        log.flush().await.expect("the carried handle must be live");

        assert_eq!(
            log_text(&path),
            "first\nsecond\n",
            "a carried handle must append, not write at its own offset"
        );
    }

    /// Fails if the pump can only notice a departed `logs` receiver from
    /// inside `deliver_line`, that is, if it has no `select!` branch of its
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

        // Both writers held (no stream at EOF) and `pump.ctl` alive
        // (control channel open), so dropping `logs` is the only thing
        // that can end this pump. `closed()` resolves when the pump's own
        // `ctl_rx` drops with its task: a bounded wait, not a guess.
        timeout(PUMP_DEADLINE, pump.ctl.closed())
            .await
            .expect("a pump whose `logs` receiver is gone must end");
    }

    /// Fails if the pump treats a closed control channel as nothing to do:
    /// `ProcIo::log_ctl` documents that dropping the sender ends the pump,
    /// and an arm that ignored the `None` would spin on it forever, since a
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

    /// Room in the in-memory pipe standing in for a child's stdin.
    ///
    /// Four bytes, far under the first line the stdin case writes, so the
    /// pump parks inside `write_all` on that first request and cannot reach
    /// the next one until the test starts reading. That parking is what
    /// makes the case's ordering a fact rather than a hope.
    const STDIN_BUFFER: usize = 4;

    // Fails if the pump writes a line whose caller has stopped waiting: the
    // supervisor abandons the `oneshot` at `STDIN_WRITE_TIMEOUT`, risking a
    // late write arriving after `not_written` was already returned. Real
    // clock: the forcing mechanism is the pipe draining and closing.
    #[tokio::test]
    async fn a_line_whose_caller_stopped_waiting_is_dropped_rather_than_written_later() {
        use tokio::io::AsyncReadExt as _;

        let (mut child_end, daemon_end) = tokio::io::duplex(STDIN_BUFFER);
        let (to_stdin, rx) = mpsc::channel(CHANNEL_CAPACITY);
        spawn_stdin_pump(Some(daemon_end), rx);

        // Wedges the pump: eight bytes into a four-byte pipe nobody is
        // reading yet.
        let (first_done, first_ack) = oneshot::channel();
        to_stdin
            .send(StdinWrite {
                line: "AAAAAAAA".to_string(),
                done: first_done,
            })
            .await
            .unwrap();

        // The abandoned one. Sent while the pump is parked on the first, so
        // it is still in the queue when its caller gives up.
        let (second_done, second_ack) = oneshot::channel();
        to_stdin
            .send(StdinWrite {
                line: "BBBB".to_string(),
                done: second_done,
            })
            .await
            .unwrap();
        drop(second_ack);

        // A live caller behind it, so the case can tell "dropped the
        // abandoned line" from "stopped writing altogether".
        let (third_done, third_ack) = oneshot::channel();
        to_stdin
            .send(StdinWrite {
                line: "CCCC".to_string(),
                done: third_done,
            })
            .await
            .unwrap();
        drop(to_stdin);

        let mut written = Vec::new();
        timeout(PUMP_DEADLINE, child_end.read_to_end(&mut written))
            .await
            .expect("the pump must drain and close the pipe")
            .expect("reading the pipe must succeed");

        assert_eq!(
            String::from_utf8(written).unwrap(),
            "AAAAAAAA\nCCCC\n",
            "the abandoned line must not reach the app, and the live one must"
        );
        assert!(
            timeout(PUMP_DEADLINE, first_ack)
                .await
                .expect("the first write must be acknowledged")
                .expect("its sender must outlive the write")
                .is_ok()
        );
        assert!(
            timeout(PUMP_DEADLINE, third_ack)
                .await
                .expect("the third write must be acknowledged")
                .expect("its sender must outlive the write")
                .is_ok()
        );
    }

    /// fails if a descriptor report leaves out the sheep's stdin write end.
    ///
    /// The number belongs to the stdin pump, but the log pump is told it,
    /// exactly as it is told its two reader numbers. A report that dropped
    /// it would hand the successor a sheep whose `shep whisper` writes into
    /// a descriptor the exec closed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_report_names_the_stdin_write_end_it_was_told_about() {
        let dir = tempfile::tempdir().unwrap();
        // Held for the length of the case, as the stdin pump holds it in
        // production: a number whose owner has dropped it is a number the
        // next `open` may be handed.
        let (_child_end, daemon_end) = std::io::pipe().unwrap();
        // One live stream, so the pump has something to be reading. A pump
        // handed no streams at all ends before it can answer anything.
        let (_out_writer, out_reader) = tokio::io::duplex(STREAM_BUFFER);
        let (logs_tx, _logs) = mpsc::channel(CHANNEL_CAPACITY);
        let (ctl, ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let pipes = PipeFds {
            out: None,
            err: None,
            stdin: Some(daemon_end.as_raw_fd()),
            channel: None,
        };
        spawn_log_pump(
            Some(out_reader),
            None::<DuplexStream>,
            LogSink::Path(dir.path().join("out.log")),
            LogSink::Path(dir.path().join("err.log")),
            logs_tx,
            ctl_rx,
            pipes,
        );

        let (done, ack) = oneshot::channel();
        ctl.send(LogCtl::ReportFds { done }).await.unwrap();
        let fds = timeout(PUMP_DEADLINE, ack)
            .await
            .expect("a descriptor report must be acknowledged")
            .expect("the pump must answer rather than drop the acknowledgement");

        assert_eq!(fds.stdin, Some(daemon_end.as_raw_fd()));
    }

    /// fails if an adopted sheep's stdin write end is not wired back to
    /// `to_stdin`.
    ///
    /// The read half of this feature. Carrying the descriptor is only half
    /// of it: the successor has to put a pump back on the daemon's end, or
    /// every `shep whisper` after a handover answers on a channel with
    /// nothing behind it.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_adopted_stdin_pipe_carries_a_line_to_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let (mut child_end, daemon_end) = std::io::pipe().unwrap();
        let daemon_end = tokio::net::unix::pipe::Sender::from_file(std::fs::File::from(
            std::os::fd::OwnedFd::from(daemon_end),
        ))
        .expect("the daemon's end of a stdin pipe is writable");

        let (_proc, io) = TokioRunner::new()
            .adopt(adopt_spec(&dir, Some(daemon_end), None))
            .expect("the real runner must be able to adopt");

        let (done, ack) = oneshot::channel();
        io.to_stdin
            .send(StdinWrite {
                line: "whisper".to_string(),
                done,
            })
            .await
            .expect("an adopted sheep must still have a stdin pump");
        timeout(PUMP_DEADLINE, ack)
            .await
            .expect("the write must be acknowledged")
            .expect("the pump must outlive the write")
            .expect("the write must succeed");

        let mut buf = [0_u8; 8];
        std::io::Read::read_exact(&mut child_end, &mut buf).expect("the child end must read");
        assert_eq!(&buf, b"whisper\n");
    }

    /// fails if an adopted sheep that never had a stdin pipe is given a
    /// channel nothing drains.
    ///
    /// `is_closed()` is the one question a caller asks about `to_stdin`
    /// (see [`ProcIo::to_stdin`]), so a dangling receiver would have the
    /// supervisor wait out its whole `STDIN_WRITE_TIMEOUT` on a sheep that
    /// has no fd 0 at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_adopted_sheep_without_a_stdin_pipe_has_a_closed_channel() {
        let dir = tempfile::tempdir().unwrap();

        let (_proc, io) = TokioRunner::new()
            .adopt(adopt_spec(&dir, None, None))
            .expect("the real runner must be able to adopt");

        assert!(io.to_stdin.is_closed());
    }

    /// fails if a descriptor report leaves out the sheep's shepherd
    /// channel.
    ///
    /// The number belongs to that channel's two pump tasks rather than to
    /// this one, exactly as the stdin number belongs to the stdin pump, and
    /// the log pump is told it for the same reason: it is the only party a
    /// snapshot asks. A report that dropped it would hand the successor a
    /// sheep whose fd 3 the exec closed, so a `shep trigger` reaches
    /// nothing and the app sees its channel end for no reason it can
    /// observe.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_report_names_the_shepherd_channel_it_was_told_about() {
        let dir = tempfile::tempdir().unwrap();
        // A real socketpair, held for the length of the case exactly as the
        // channel's own pumps hold it in production.
        let (daemon_end, _child_end) = std::os::unix::net::UnixStream::pair().unwrap();
        let (_out_writer, out_reader) = tokio::io::duplex(STREAM_BUFFER);
        let (logs_tx, _logs) = mpsc::channel(CHANNEL_CAPACITY);
        let (ctl, ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let pipes = PipeFds {
            out: None,
            err: None,
            stdin: None,
            channel: Some(daemon_end.as_raw_fd()),
        };
        spawn_log_pump(
            Some(out_reader),
            None::<DuplexStream>,
            LogSink::Path(dir.path().join("out.log")),
            LogSink::Path(dir.path().join("err.log")),
            logs_tx,
            ctl_rx,
            pipes,
        );

        let (done, ack) = oneshot::channel();
        ctl.send(LogCtl::ReportFds { done }).await.unwrap();
        let fds = timeout(PUMP_DEADLINE, ack)
            .await
            .expect("a descriptor report must be acknowledged")
            .expect("the pump must answer rather than drop the acknowledgement");

        assert_eq!(fds.channel, Some(daemon_end.as_raw_fd()));
    }

    /// fails if an adopted shepherd channel is not wired back to both
    /// `to_child` and `from_child`.
    ///
    /// Both directions in one case, because the failure to catch is a
    /// successor that rebuilt one pump and not the other, and either half
    /// alone looks healthy from the other side. Writing proves
    /// `shutdown_with_message` and `shep trigger` still land; reading proves
    /// `{"kind":"ready"}` and every action reply still come back, which is
    /// what a `wait_ready` sheep's whole lifecycle turns on.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_adopted_shepherd_channel_carries_both_directions() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

        let dir = tempfile::tempdir().unwrap();
        // `child_end` is what an app holds on fd 3, unmoved by the handover.
        // Both ends are async here rather than one of each: the write below
        // is served by a task the adoption spawned, so a blocking read on
        // this thread would park the runtime before that task ever ran.
        let (daemon_end, child_end) = tokio::net::UnixStream::pair().unwrap();
        let (child_read, mut child_write) = tokio::io::split(child_end);
        let mut child = tokio::io::BufReader::new(child_read);

        let (_proc, mut io) = TokioRunner::new()
            .adopt(adopt_spec(&dir, None, Some(daemon_end)))
            .expect("the real runner must be able to adopt");

        io.to_child
            .send(ShepherdMessage::Shutdown)
            .await
            .expect("an adopted sheep must still have a channel writer");
        let mut line = String::new();
        timeout(PUMP_DEADLINE, child.read_line(&mut line))
            .await
            .expect("the shepherd's message must reach the child's end")
            .expect("the child end must read");
        assert_eq!(line.trim_end(), r#"{"kind":"shutdown"}"#);

        child_write
            .write_all(b"{\"kind\":\"ready\"}\n")
            .await
            .unwrap();
        let back = timeout(PUMP_DEADLINE, io.from_child.recv())
            .await
            .expect("an adopted sheep must still have a channel reader")
            .expect("the reader must forward what the child said");
        assert_eq!(back, ChildMessage::Ready);
    }

    /// fails if an adopted sheep that never had a channel is given ends
    /// nothing drains.
    ///
    /// `is_closed()` is the one question the supervisor asks about
    /// `to_child` (see `SheepSlot::open_channel`), so a dangling receiver
    /// would have a `shep trigger` against a sheep with no fd 3 wait out its
    /// whole `action_timeout` instead of answering `NoChannel` at once.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_adopted_sheep_without_a_channel_has_closed_channel_ends() {
        let dir = tempfile::tempdir().unwrap();

        let (_proc, mut io) = TokioRunner::new()
            .adopt(adopt_spec(&dir, None, None))
            .expect("the real runner must be able to adopt");

        assert!(io.to_child.is_closed());
        assert!(
            io.from_child.recv().await.is_none(),
            "a sheep with no channel must report one that is over, not one that is quiet"
        );
    }

    /// The plainest adoption, with `stdin_pipe` and `channel` the only
    /// handles it carries.
    ///
    /// Every other handle is `None`, which is the shape a sheep whose log
    /// opens had failed arrives in; nothing here reads them. The pid is this
    /// process's own because nothing below ever waits it: an adoption
    /// records a number, and the reaper is what would go looking for it.
    #[cfg(unix)]
    fn adopt_spec(
        dir: &tempfile::TempDir,
        stdin_pipe: Option<tokio::net::unix::pipe::Sender>,
        channel: Option<tokio::net::UnixStream>,
    ) -> AdoptSpec {
        AdoptSpec {
            pid: std::process::id(),
            out_file: dir.path().join("out.log"),
            err_file: dir.path().join("err.log"),
            out_pipe: None,
            err_pipe: None,
            out_log: None,
            err_log: None,
            stdin_pipe,
            channel,
            reaper: Arc::new(crate::runner::AdoptedReaper::new()),
        }
    }

    /// A spec for a real child, whose fd 0 is the point of the two cases
    /// below.
    ///
    /// A real child rather than a harness, since the question is what the
    /// spawn reads off the handle it is about to give away: an in-memory
    /// stand-in has no descriptor to read.
    ///
    /// The program is the caller's, and the choice matters more than it
    /// looks: a child that exits before the report is answered takes its
    /// log pump with it, and the case then fails on a dropped
    /// acknowledgement rather than on anything about descriptors.
    #[cfg(unix)]
    fn child_spec(dir: &tempfile::TempDir, program: &str, args: &[&str], stdin: bool) -> SpawnSpec {
        SpawnSpec {
            name: "web".to_string(),
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: None,
            env: BTreeMap::new(),
            out_file: dir.path().join("out.log"),
            err_file: dir.path().join("err.log"),
            channel: false,
            stdin,
            credentials: None,
        }
    }

    /// fails if a spawn does not report the descriptor it put on the child's
    /// fd 0.
    ///
    /// The number is read off `child.stdin` before the spawn hands it to the
    /// stdin pump, which is the only moment it is knowable, and getting that
    /// order wrong reports `None` for every sheep an operator can whisper
    /// to. Checked as a write end of a pipe rather than merely as some
    /// number: a report that named the wrong one of the five handles a spawn
    /// holds would still be `Some`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_spawn_reports_the_write_end_it_put_on_the_childs_stdin() {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};

        let dir = tempfile::tempdir().unwrap();
        // `cat` with a real pipe on fd 0 waits on it, so it is still there
        // to be reported on, and it exits when `io` is dropped below.
        let (_proc, io) = TokioRunner::new()
            .spawn(&child_spec(&dir, "/bin/cat", &[], true))
            .unwrap();

        let (done, ack) = oneshot::channel();
        io.log_ctl.send(LogCtl::ReportFds { done }).await.unwrap();
        let fds = timeout(PUMP_DEADLINE, ack)
            .await
            .expect("a descriptor report must be acknowledged")
            .expect("the pump must answer rather than drop the acknowledgement");

        let stdin = fds
            .stdin
            .expect("a sheep spawned with a stdin pipe carries its write end");
        let flags = OFlag::from_bits_truncate(fcntl(stdin, FcntlArg::F_GETFL).unwrap());
        assert_eq!(
            flags & OFlag::O_ACCMODE,
            OFlag::O_WRONLY,
            "the number reported for stdin must be the end the daemon writes"
        );
        assert_ne!(Some(stdin), fds.out_pipe);
        assert_ne!(Some(stdin), fds.err_pipe);
        drop(io);
    }

    /// fails if a sheep that never asked for a pipe on fd 0 carries a
    /// descriptor for one.
    ///
    /// `/dev/null` is what such a child has there, and it belongs to the
    /// child alone: naming a number here would have the successor adopt
    /// something no `shep whisper` will ever reach.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_spawn_without_stdin_reports_no_descriptor_for_it() {
        let dir = tempfile::tempdir().unwrap();
        // Not `cat` here: with `/dev/null` on fd 0 it reads EOF and exits at
        // once, which ends its log pump and leaves this case racing a
        // report against a pump that is already gone.
        let (mut proc, io) = TokioRunner::new()
            .spawn(&child_spec(&dir, "/bin/sleep", &["30"], false))
            .unwrap();

        let (done, ack) = oneshot::channel();
        io.log_ctl.send(LogCtl::ReportFds { done }).await.unwrap();
        let fds = timeout(PUMP_DEADLINE, ack)
            .await
            .expect("a descriptor report must be acknowledged")
            .expect("the pump must answer rather than drop the acknowledgement");

        assert_eq!(fds.stdin, None);
        // Killed rather than waited out: nothing here reads the child, and
        // a `sleep` left behind outlives the test binary.
        proc.signal_process(OperatorSignal::Kill).unwrap();
        drop(io);
    }

    /// A [`SpawnSpec`] carrying only what `what_exec_will_find` reads. Every
    /// other field is left at whatever is cheapest: nothing below spawns
    /// anything, so nothing below can be affected by them.
    fn preflight_spec(program: &str, cwd: Option<PathBuf>, path: Option<&str>) -> SpawnSpec {
        let mut env = BTreeMap::new();
        if let Some(path) = path {
            env.insert("PATH".to_string(), path.to_string());
        }
        SpawnSpec {
            name: "web".to_string(),
            program: program.to_string(),
            args: Vec::new(),
            cwd,
            env,
            out_file: PathBuf::from("/dev/null"),
            err_file: PathBuf::from("/dev/null"),
            channel: false,
            stdin: false,
            credentials: None,
        }
    }

    /// fails if a form the preflight must not decide starts being refused.
    ///
    /// This is the direction that costs a working Flockfile, so it is the
    /// list worth pinning as a sweep rather than one representative case.
    /// Every entry here is a real shape from the testbed Flockfile that
    /// produced the defect: absolute (`heatrotom`), relative to a `cwd`
    /// (`obscura`), a bare command on PATH (`node`, `npx`).
    #[test]
    fn a_program_that_is_really_there_is_nothing_to_report() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("srv");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        let path_dir = dir.path().to_string_lossy().into_owned();

        for spec in [
            preflight_spec(&bin.to_string_lossy(), None, None),
            preflight_spec("./srv", Some(dir.path().to_path_buf()), None),
            preflight_spec("srv", None, Some(&path_dir)),
            // The second PATH entry rather than the first, so a search that
            // only ever looks at one directory fails here.
            preflight_spec("srv", None, Some(&format!("/nonexistent:{path_dir}"))),
            // Not decidable, and so not decided: a relative path with no
            // `cwd` would be resolved against whatever directory the
            // shepherd was autostarted from.
            preflight_spec("./srv", None, None),
            // Likewise a bare name with no PATH to search.
            preflight_spec("srv", None, None),
            preflight_spec("srv", None, Some("")),
            preflight_spec("", None, None),
        ] {
            assert_eq!(
                what_exec_will_find(&spec),
                Preflight::Unknown,
                "reported something about a spec it cannot be certain about: {spec:?}"
            );
        }
    }

    /// Fails if `Impossible`, the verdict that refuses the whole batch, is
    /// ever returned for anything but a path, since a path is the one claim
    /// about the filesystem the daemon can settle.
    ///
    /// The reason string carries the resolved path, not the
    /// `./proto-enum-api` the operator wrote, so an operator can tell which
    /// directory it was looked for in.
    #[test]
    fn an_absent_path_is_impossible_and_names_the_path_that_was_tried() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            what_exec_will_find(&preflight_spec(
                "./proto-enum-api",
                Some(dir.path().to_path_buf()),
                None,
            )),
            // `join`, not a `/` in the format string: the separator shep
            // resolved this path with is the platform's, and on Windows
            // that is a backslash.
            Preflight::Impossible(format!(
                "no such file: {}",
                dir.path().join("./proto-enum-api").display()
            )),
        );
        // Absolute, and `/nonexistent/srv` is not absolute on Windows: a
        // path with no drive letter is relative to the current drive, and
        // this arm is reached only for a program the daemon can resolve
        // without a `cwd`.
        let absent = if cfg!(windows) {
            r"C:\nonexistent\srv"
        } else {
            "/nonexistent/srv"
        };
        assert_eq!(
            what_exec_will_find(&preflight_spec(absent, None, None)),
            Preflight::Impossible(format!("no such file: {absent}")),
        );
    }

    /// Fails if a filesystem error that is not "no such file" is ever read
    /// as absence.
    ///
    /// [`Path::exists`] returns `false` on any [`fs::metadata`] error, so an
    /// unreadable directory or an unsettled mount would otherwise refuse a
    /// whole batch over a filesystem merely unavailable for a moment.
    ///
    /// Two provocations: `ENOTDIR` (an invalid filename on Windows, where a
    /// blocked file answers absence instead), and `EACCES` via a `chmod
    /// 000` directory, skipped under root, which bypasses it. Mode
    /// restored before the assertion, or a panic leaves the `TempDir`
    /// unable to `Drop` through the locked directory.
    #[test]
    fn a_filesystem_error_that_is_not_absence_is_never_impossible() {
        let dir = tempfile::tempdir().unwrap();

        // ENOTDIR: `wall` is a file, so nothing can be under it.
        #[cfg(unix)]
        let unreadable = {
            let wall = dir.path().join("wall");
            fs::write(&wall, "not a directory").unwrap();
            wall.join("srv")
        };
        // ERROR_INVALID_NAME: `<` cannot appear in a Windows filename, so
        // the name is refused before lookup. A path through a file would
        // answer ERROR_PATH_NOT_FOUND instead, which is absence, making
        // the assertion below vacuous.
        #[cfg(windows)]
        let unreadable = dir.path().join("no<such<name");
        let kind = fs::metadata(&unreadable).unwrap_err().kind();
        assert_ne!(
            kind,
            io::ErrorKind::NotFound,
            "the provocation must be an error OTHER than not-found, or this \
             case proves nothing: {kind:?}"
        );
        assert_eq!(
            what_exec_will_find(&preflight_spec(&unreadable.to_string_lossy(), None, None)),
            Preflight::Unknown,
            "a path shep could not read is a suspicion, not a certainty, and \
             must never refuse a batch"
        );

        // EACCES: an unreadable directory between the cwd and the program.
        // Unix only: see this test's doc for why Windows gets no
        // equivalent rather than a weaker one.
        #[cfg(unix)]
        {
            if nix::unistd::Uid::effective().is_root() {
                return;
            }
            let locked = dir.path().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            let behind_the_wall = locked.join("srv");
            let observed = fs::metadata(&behind_the_wall)
                .map(|_| ())
                .map_err(|e| e.kind());
            let verdict = what_exec_will_find(&preflight_spec(
                &behind_the_wall.to_string_lossy(),
                None,
                None,
            ));
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

            assert_eq!(
                observed,
                Err(io::ErrorKind::PermissionDenied),
                "the chmod must actually bite, or the assertion below is vacuous"
            );
            assert_eq!(
                verdict,
                Preflight::Unknown,
                "a permission error on the way to the program must not take the rest \
                 of the flock down with it"
            );
        }
    }

    /// fails if a bare command off the PATH ever becomes `Impossible`.
    ///
    /// `Doubtful` is reported and carried on with, never refused: the
    /// daemon's own PATH under the unit `shep startup` installs is not the
    /// shell an operator tested in, so a flock whose one Node app cannot
    /// resolve `node` must still bring up every other app.
    ///
    /// The PATH searched is in the message on purpose: naming which one
    /// sends an operator to the terminal path that actually works.
    #[test]
    fn a_bare_command_off_the_path_is_only_ever_doubtful() {
        let found = what_exec_will_find(&preflight_spec("node", None, Some("/nonexistent")));

        assert_eq!(
            found,
            Preflight::Doubtful("`node` is not on the shepherd's PATH (/nonexistent)".to_string()),
        );
        assert!(
            !matches!(found, Preflight::Impossible(_)),
            "a claim about an environment must never refuse a batch"
        );
    }

    /// fails if a long PATH is printed in full, or a short one is not.
    ///
    /// This message reaches a terminal now, not only the shepherd's log:
    /// `spawn_fresh` puts a `Doubtful` reason into the reply once that app's
    /// own spawn has failed. A full interactive shell's PATH is unreadably
    /// long (see `PATH_ENTRIES_IN_MESSAGE`'s own doc), and dumping that into
    /// `error[spawn_failed]:` buries the sentence that matters.
    ///
    /// The short case is the one that must survive intact: a `shep startup`
    /// unit with no PATH of its own gets `assemble`'s three-entry fallback,
    /// and seeing those three IS the diagnosis.
    #[test]
    fn a_long_path_is_summarised_and_a_startup_units_own_path_is_not() {
        // Spelled in the platform's own PATH syntax. A unix PATH handed to
        // a Windows build is one entry, not six, so every assertion below
        // would be about a string this function never sees.
        let sep = PATH_LIST_SEPARATOR;
        let join = |entries: &[&str]| entries.join(&sep.to_string());

        let fallback = join(&["/usr/local/bin", "/usr/bin", "/bin"]);
        assert_eq!(
            summarise_path(&fallback),
            fallback,
            "the PATH a unit actually gets must print in full"
        );

        let long = join(&["/a", "/b", "/c", "/d", "/e", "/f"]);
        assert_eq!(
            summarise_path(&long),
            format!("{} and 2 more entries", join(&["/a", "/b", "/c", "/d"]))
        );

        // Exactly at the cap, which is where an off-by-one would show.
        let capped = join(&["/a", "/b", "/c", "/d"]);
        assert_eq!(summarise_path(&capped), capped);
    }

    // Everything else in this module needs a real OS child and lives in
    // `tests/real_runner.rs`; this one case needs no process at all.
    /// `cfg(unix)` alongside `signal_group`, the function it guards. There
    /// is no negative-pid primitive on Windows and so no zero-pid hazard:
    /// `kill_tree` addresses a job handle, which cannot accidentally name
    /// the daemon's own group the way `kill(0, ..)` can.
    #[cfg(unix)]
    #[test]
    fn a_zero_pid_is_refused_before_it_can_reach_the_daemons_own_group() {
        // `SIGCONT`, not a lethal signal: if `signal_group`'s zero guard is
        // ever deleted, this assertion must fail rather than take the test
        // harness's own process group down with it.
        let err = signal_group(0, Signal::SIGCONT).unwrap_err();
        assert_eq!(
            err.to_string(),
            "signal delivery failed: pid 0 is not a signallable process id"
        );
    }
}
