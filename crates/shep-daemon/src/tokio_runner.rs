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
//! [`Flush`](crate::runner::LogCtl::Flush) writes out what a stream has
//! buffered and waits for it to land, which is the barrier `shep flush`
//! truncates behind.
//!
//! # Shepherd-channel fd lifecycle
//!
//! The child's end of the `UnixStream::pair()` is handed to the child via
//! `command_fds::FdMapping` as fd 3. Not an intra-doc link: `command-fds`
//! is a `cfg(unix)` dependency, so the link resolves on unix and fails the
//! doc build on Windows. That mapping captures the fd inside a
//! `pre_exec` closure owned by the `Command`, so the parent process's extra
//! reference to the same fd stays open until the `Command` itself is
//! dropped — done explicitly, immediately after `spawn()`, so the daemon's
//! side of the channel sees a clean EOF once the child closes or exits
//! rather than being kept artificially open by our own leftover reference.

use core::time::Duration;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(unix)]
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
use tokio::time::{Instant, sleep_until};

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

/// Capacity of every channel a spawn wires up — generous enough that a
/// bursty child doesn't back-pressure against a sheep task that's merely
/// slow to poll, without buffering unboundedly.
const CHANNEL_CAPACITY: usize = 32;

/// Bytes a log file buffers before the pump writes them through.
///
/// `tokio::fs::File` hands every `write` to the blocking pool, and that
/// dispatch — not the `write(2)` under it — is what a log line costs the
/// daemon: measured at 32.8 us of daemon CPU per line against 0.99 us for the
/// same line written unbuffered from a plain loop. Batching amortises one
/// dispatch over a whole buffer instead of paying it per line.
const LOG_BUFFER: usize = 8 * 1024;

/// How long a line may sit in that buffer before the pump writes it out
/// anyway.
///
/// Measured from the FIRST unflushed line rather than the most recent, so a
/// steady trickle cannot push the deadline out indefinitely; a busy stream
/// fills [`LOG_BUFFER`] and flushes long before this fires. Without it a
/// sheep that logged one line and went quiet would leave that line in the
/// buffer until its next one, which for some sheep is never.
const IDLE_FLUSH: Duration = Duration::from_millis(50);

/// Real [`crate::runner::ProcessRunner`] over actual OS processes.
#[derive(Debug, Default)]
pub struct TokioRunner;

/// The exit code [`TokioProc::kill_tree`] terminates a job with.
///
/// Windows exits carry no signal number, so this value is all a reader of
/// `ProcessInfo::last_exit` or the `EXIT` column ever sees for a sheep the
/// daemon killed. `137` is chosen rather than std's own `TerminateProcess(h, 1)`
/// because it is `128 + 9` — the shell convention for "killed by SIGKILL"
/// that `commands::reap::classify` already reads on unix — so the same number
/// means the same thing in a listing on either platform.
#[cfg(windows)]
const KILL_TREE_EXIT_CODE: u32 = 137;

/// Distinguishes one spawn's shepherd-channel pipe from every other's.
///
/// The pipe namespace is machine-global, so a name has to be unique across
/// every sheep this daemon runs AND across any other daemon on the host —
/// hence process id plus this counter rather than the sheep's name, which
/// two `$SHEP_HOME`s could both be using. Monotonic rather than reused, so a
/// restarted sheep never inherits a name its dying predecessor still holds a
/// handle on.
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
///
/// Public because it is [`TokioRunner`]'s [`ProcessRunner::Proc`], and an
/// associated type cannot be less visible than the trait impl that names it.
#[derive(Debug)]
pub struct TokioProc {
    /// Captured once at spawn time — `tokio::process::Child::id` reports
    /// `None` after the child has been waited to completion, but callers
    /// (e.g. a kill ladder) may still need the pid after that point.
    ///
    /// Also the whole of an adopted sheep's identity: a successor inherits a
    /// pid and no `Child` at all, which is why this was never read off the
    /// child in the first place.
    pid: u32,
    proc: Supervised,
    /// The job object this sheep and every process it spawns belong to —
    /// Windows' stand-in for the unix process group `process_group(0)`
    /// establishes, and what [`RunningProcess::kill_tree`] terminates.
    ///
    /// Held for the proc's whole life because the handle IS the group: drop
    /// it and there is nothing left to address the tree by. See
    /// [`crate::sys_windows`] for what it does and does not guarantee.
    #[cfg(windows)]
    job: crate::sys_windows::Job,
}

/// Where this proc's exit comes from: tokio, or a targeted `waitpid`.
///
/// The two arms are the two ways a sheep can be under this daemon's care.
/// A spawned one has a `tokio::process::Child` and tokio does the waiting.
/// An adopted one crossed an `execve` into a successor that has no `Child`
/// for it and no way to make one, since only `Command::spawn` produces those,
/// so its exit is collected by [`AdoptedReaper`]'s targeted wait instead.
///
/// Only the wait differs. `signal`, `signal_process` and `kill_tree` all
/// address the pid, which is the same number either way.
#[derive(Debug)]
enum Supervised {
    /// Started by this daemon, and waited by tokio.
    Spawned(Child),
    /// Inherited across a handover, and waited by the reaper the successor
    /// shares between every sheep it adopted.
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
            // The reaper carries the same cancel-safety contract by its own
            // route: a status it has taken is remembered and replayed, so a
            // dropped wait loses nothing and a second one answers the same.
            // It reports an error where tokio cannot (nothing else in this
            // process may reap an adopted pid, and something that did has
            // taken the exit with it), which lands on the same degenerate
            // outcome the failed-wait arm below returns.
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
        // tokio::process::Child::wait is documented cancel-safe (repeat
        // calls, or calls after a dropped-mid-flight future, replay the
        // cached result instead of restarting) — RunningProcess::wait's own
        // cancel-safety contract is inherited directly from it, no extra
        // latching needed on our side (contrast the scripted fake, which
        // has to hand-roll that latch itself).
        match child.wait().await {
            Ok(status) => ExitOutcome {
                code: status.code(),
                // Always `None` on Windows, and that is the truth rather
                // than a stub: nothing kills a Windows process by signal, so
                // there is no signal number for an exit to carry. Every
                // Windows exit has a code — including one this daemon caused
                // itself, which is why `kill_tree` picks its termination code
                // deliberately (see `KILL_TREE_EXIT_CODE`).
                #[cfg(windows)]
                signal: None,
                #[cfg(unix)]
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

    #[cfg(unix)]
    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError> {
        signal_group(self.pid, to_nix_signal(sig))
    }

    /// Refuses, on every platform-supported signal, and the refusal is the
    /// design rather than a gap left open.
    ///
    /// **Windows has no way to deliver anything SIGTERM-shaped to an
    /// arbitrary process.** The nearest mechanism,
    /// `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group)`, reaches only
    /// console-subsystem processes that share a console with the caller and
    /// that installed their own `SetConsoleCtrlHandler`; a detached shepherd
    /// shares a console with nothing, and a GUI app or a runtime that never
    /// registered a handler would ignore it regardless. Pretending otherwise
    /// — returning `Ok(())` having done nothing — would make the ladder
    /// believe a polite stop was delivered and silently convert every
    /// `shep stop` into a hang followed by a kill, with no line anywhere
    /// saying why.
    ///
    /// So what actually happens on `shep stop`, spelled out because it is an
    /// operator-visible behaviour difference and belongs in the release
    /// notes as much as here:
    ///
    /// - An app that opts into the shepherd channel (`shutdown_with_message`)
    ///   is unaffected. `kill::kill_process` sends
    ///   `ShepherdMessage::Shutdown` and never reaches this method at all.
    ///   **That is the supported graceful path on Windows.**
    /// - Any other app gets this refusal, which the ladder logs; then the
    ///   ladder waits out its full `grace` and escalates to
    ///   [`Self::kill_tree`]. So `shep stop` costs `kill_timeout` and ends in
    ///   a termination, which is `shep kill` with a delay in front of it.
    ///
    /// The wait is kept rather than short-circuited deliberately. Skipping
    /// straight to `kill_tree` would be faster and would be wrong: a
    /// channel-using app whose `Shutdown` crossed with this call still
    /// deserves the grace period it was promised, and `kill.rs` is one
    /// shared ladder whose timing contract should not fork per platform.
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

    /// Terminates the sheep's whole job — every process it spawned, however
    /// deeply nested.
    ///
    /// Strictly more complete than the unix rung it mirrors. `kill.rs`'s
    /// module comment records that a grandchild which calls `setsid` escapes
    /// its process group and survives `SIGKILL`; a job member cannot leave
    /// its job, and cannot spawn outside it, because
    /// `sys_windows::Job::create` does not grant breakaway. The
    /// escaped-`setsid` hole simply does not exist on this platform.
    #[cfg(windows)]
    fn kill_tree(&mut self) -> Result<(), RunnerError> {
        self.job
            .terminate(KILL_TREE_EXIT_CODE)
            .map_err(|error| RunnerError::SignalFailed(error.to_string()))
    }

    /// `SIGKILL` is delivered; the other eight names are refused by name.
    ///
    /// A per-signal refusal rather than a whole-verb one, which is what
    /// keeps `shep signal <sheep> SIGKILL` working while
    /// `shep signal <sheep> SIGHUP` says precisely what it cannot do. Seven
    /// of the nine (`Hup`, `Quit`, `Usr1`, `Usr2`, `Winch`, `Cont`, `Term`)
    /// have no delivery mechanism to a foreign Windows process at all.
    ///
    /// `Int` is refused too, and that one is a judgement rather than an
    /// absence: `GenerateConsoleCtrlEvent(CTRL_C_EVENT, ..)` exists, but
    /// Ctrl+C is *disabled by default* in a process group created with
    /// `CREATE_NEW_PROCESS_GROUP` — which is exactly how [`TokioRunner`]
    /// spawns every sheep — so it would arrive nowhere while reading as
    /// success. A refusal that names the reason beats a delivery that
    /// silently is not one.
    ///
    /// Deliberately per-PROCESS, matching the unix arm's positive-pid
    /// `kill`: `start_kill` terminates this sheep alone and leaves its lambs
    /// running, which is the documented difference between this method and
    /// [`Self::kill_tree`].
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
        // POSITIVE pid, unlike `signal_group`'s negative one. That single
        // character is the difference between the two contracts, so it gets
        // its own function rather than a boolean on the existing one.
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
#[cfg(unix)]
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
/// An explicit match, like [`to_nix_signal`], rather than a numeric
/// conversion: shep-core deliberately holds no raw signal numbers (they differ
/// by platform — `SIGUSR1` is 10 on Linux and 30 on macOS), so this is the one
/// place in the workspace where the two vocabularies meet, and an unmapped
/// variant must be a compile error rather than a runtime one.
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
/// Two arms, and which one a program takes is decided by a single `/`:
///
/// - **With a `/`** the program is a path, and a path is a claim about the
///   filesystem this can settle. Absolute, or relative to `spec.cwd`, and
///   either way the file is either there or it is not.
///   [`Preflight::Impossible`], which its caller refuses a whole batch over.
/// - **Without one** it is a bare command exec resolves through `PATH`, and
///   that is a claim about an environment rather than about a path. At most
///   [`Preflight::Doubtful`], never `Impossible`. See [`Preflight`] for the
///   argument; the short version is that a `shep startup` unit's `PATH` is
///   not the operator's shell's, so refusing a batch here would keep a whole
///   flock down over one app's interpreter.
///
/// The PATH searched is `spec.env`'s own, not the daemon's ambient one: that
/// map IS the child's whole environment (`spawn` calls `env_clear` then
/// `envs`), so it is the exact list exec is about to walk, and naming it in
/// the message is what tells an operator which `PATH` was actually looked in.
///
/// [`Preflight::Unknown`] for everything else, which is anything that would
/// have to be guessed at:
///
/// - a program that resolves, whether by path or on `PATH`. Reported as
///   "nothing knowable", not as "this will work": the executable bit, the
///   shebang and the `user`/`group` drop are all still ahead of it.
/// - a relative path with no `cwd`. The child would resolve it against the
///   DAEMON's working directory, whatever the shepherd happened to be
///   autostarted from.
/// - a bare command with no usable `PATH` in `spec.env`. `assemble`'s
///   `base_env` always seeds one, so only a spec built some other way gets
///   here.
/// - an empty program, which `normalize` refuses upstream.
///
/// Existence only, never the executable bit: a file that is there and cannot
/// be exec'd is the spawn's business, and mode bits under a `user`/`group`
/// drop are not a thing to be confident about from here. And existence is
/// read through [`definitely_absent`] rather than [`Path::exists`], which
/// collapses a permission error into "absent" -- see that function for why
/// the distinction decides whether a flock comes up.
fn what_exec_will_find(spec: &SpawnSpec) -> Preflight {
    if spec.program.is_empty() {
        return Preflight::Unknown;
    }
    let program = Path::new(&spec.program);
    // A `/` is the operator's claim that this is a path rather than a
    // command name, and Windows spells the same claim with a `\\`. Reading
    // only `/` there does not break a spawn: the fall-through looks the
    // program up on PATH, does not find it, and the spawn proceeds and
    // works. What it costs is the clear refusal this whole function
    // exists to produce, on the one platform whose paths never match.
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
    // Absent from the PATH only if EVERY entry says so, and only if every
    // entry that did not was a plain `NotFound`. One unreadable directory
    // means exec may still find the program there, and claiming otherwise
    // would put a sentence in the shepherd's log that is simply untrue.
    // Cheaper to be wrong here than in the arm above -- this one can only
    // reach a log line, never a refusal -- but a misleading message is the
    // same class of fault either way.
    // `split_paths`, not a `split` on a separator of our own: the
    // separator is `;` on Windows, where a `:` sits INSIDE every entry
    // rather than between two, and entries there may additionally be
    // quoted. Getting either wrong makes every entry unreadable, every
    // lookup a `NotFound`, and the sentence below a lie about a program
    // that is on the PATH after all.
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
/// Four, because the `PATH` that matters here is a startup unit's and
/// [`base_env`](crate::assemble)'s fallback for a unit that has none is three
/// entries (`/usr/local/bin:/usr/bin:/bin`). The case an operator actually
/// hits therefore prints in full, with one to spare.
///
/// The case being cut off is an interactive shell's `PATH`, measured at
/// thirty-one entries and just over two kilobytes on this machine, which is
/// unreadable in a terminal error. It is also the case where printing it in
/// full teaches nothing: a daemon autostarted from a shell inherited that
/// operator's own `PATH`, so it is the one they would go and check anyway.
const PATH_ENTRIES_IN_MESSAGE: usize = 4;

/// What separates one `PATH` entry from the next.
///
/// `:` on unix and `;` on Windows, where every entry starts with a drive
/// letter and a `:` of its own. Display only: the lookup in
/// [`what_exec_will_find`] goes through `std::env::split_paths`, which
/// also understands the quoting a summary line cannot reproduce.
const PATH_LIST_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

/// `path` as a message should print it: in full when short, and otherwise its
/// first [`PATH_ENTRIES_IN_MESSAGE`] entries with a count of the rest.
///
/// The shape of the `PATH` is what the reader needs, not every byte of it.
/// Seeing `/usr/local/bin:/usr/bin:/bin` is what tells an operator the
/// shepherd is running under a unit rather than under their shell, which is
/// the whole diagnosis.
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
/// [`Path::exists`] is the obvious call here and is the wrong one. It is
/// documented to return `false` on ANY [`fs::metadata`] error, so a
/// permission error on an intermediate directory, an unsettled mount and a
/// race all read identically to "absent". Every one of those would have made
/// [`what_exec_will_find`] answer [`Preflight::Impossible`] and refuse a
/// whole batch on a filesystem that was merely unavailable for a moment,
/// which is the failure mode that verdict exists to avoid.
///
/// So: `NotFound` and nothing else. `PermissionDenied` included, anything
/// that is not a flat "no such file" is a suspicion rather than a certainty
/// and belongs in [`Preflight::Unknown`], where the spawn reports it as it
/// always did and no other app in the batch pays for it.
///
/// Follows symlinks, as `exists` did and as exec does: a symlink whose
/// target is gone answers `NotFound` here, which is a real certainty and the
/// right one. A directory and a file with no execute bit both answer `Ok`
/// and so are not absent, which is also right -- neither can be exec'd, but
/// that is the spawn's business and not a claim this can settle.
fn definitely_absent(path: &Path) -> bool {
    matches!(fs::metadata(path), Err(err) if err.kind() == io::ErrorKind::NotFound)
}

impl ProcessRunner for TokioRunner {
    type Proc = TokioProc;

    /// Reports a `program` that provably is not there, and nothing else.
    ///
    /// `program` is what `assemble` resolved the app's `script` and
    /// `interpreter` down to, so this checks the one file exec will name and
    /// never a script that an interpreter takes as its first ARGUMENT: an
    /// app running `npx next start` is checked at `npx`, and `next` is
    /// npx's business.
    ///
    /// Deliberately under-tightened: see this module's `what_exec_will_find`
    /// for every form it declines to decide, and for why a bare command is
    /// only ever doubted while a path can be refused.
    fn preflight(&self, spec: &SpawnSpec) -> Preflight {
        what_exec_will_find(spec)
    }

    /// Takes a sheep this image inherited rather than started.
    ///
    /// Nothing is spawned, opened or signalled. The carried pipe read ends
    /// are handed to the same log pump a spawn feeds, the carried
    /// log handles are written through rather than reopened, and the pid is
    /// the one the sheep has been running under all along. From the sheep's
    /// side the shepherd was never away.
    ///
    /// The three channels a spawn may wire are all closed here rather than
    /// left dangling, exactly as a spawn closes the ones its own spec did
    /// not ask for. Phase 2a's fitness gate refuses to carry any sheep with
    /// a shepherd channel or a stdin pipe, so a carried sheep has neither,
    /// and a caller's `is_closed()` says so at once instead of a send
    /// buffering into a channel nobody drains.
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
            reaper,
        } = spec;

        let (logs_tx, logs_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (log_ctl_tx, log_ctl_rx) = mpsc::channel(CHANNEL_CAPACITY);
        // Read off the readers before they are moved into the pump, as the
        // spawn path does: these are the numbers the NEXT handover carries,
        // and a successor that reported its predecessor's would be naming
        // descriptors this process does not have.
        let pipes = PipeFds {
            out: out_pipe.as_ref().map(AsRawFd::as_raw_fd),
            err: err_pipe.as_ref().map(AsRawFd::as_raw_fd),
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
        drop(from_child_tx);
        let (to_child, to_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        drop(to_child_rx);
        let (to_stdin, to_stdin_rx) = mpsc::channel(CHANNEL_CAPACITY);
        drop(to_stdin_rx);

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
        // SpawnSpec::env is documented fully-resolved with "no daemon-env
        // leakage beyond this map" — env_clear() before envs() is what makes
        // that contract actually hold, since Command inherits the daemon's
        // ambient env by default otherwise.
        command.env_clear();
        command.envs(&spec.env);
        // `/dev/null` unless the app asked for a pipe. Piping unconditionally
        // would change how a great many programs behave — a closed fd 0 is how
        // they decide they are non-interactive — so this follows `spec.channel`
        // in being opened only on request. See `AppConfig::stdin`.
        command.stdin(if spec.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        // New process group rooted at the child itself, so kill_tree's
        // negative-pid SIGKILL reaches it and its descendants without also
        // reaching the daemon's own group.
        #[cfg(unix)]
        command.process_group(0);

        // The Windows equivalent is a job object, which cannot be asked for
        // at spawn time — a process is ASSIGNED to one after it exists — so
        // the containment half happens below, immediately after `spawn()`.
        // What is set here are the two creation flags that make that
        // assignment meaningful and keep the child from stealing a console:
        //
        // - `CREATE_NEW_PROCESS_GROUP` roots a console process group at the
        //   child, so a Ctrl+C in whatever console launched the shepherd is
        //   not broadcast to every sheep in the flock. Exactly the isolation
        //   `process_group(0)` buys on unix.
        // - `CREATE_NO_WINDOW` keeps a console child from flashing up a
        //   window. A supervised service has nowhere to draw one, and its
        //   stdout and stderr are already piped to log files.
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

        // No Windows arm, and none is possible without becoming a different
        // feature. Dropping privilege there means `CreateProcessWithLogonW`
        // or `CreateProcessAsUser` against a real token, which needs either a
        // plaintext password at spawn time or `SeAssignPrimaryTokenPrivilege`
        // and a full LSA logon session. A partial version would be worse than
        // the refusal, so `user`/`group` in a Flockfile are refused outright:
        // `privilege::resolve`'s non-unix arm is what performs that refusal,
        // and it runs long before a spawn, which is why this assertion can be
        // a `debug_assert` rather than an error path — reaching here with
        // credentials set would mean that refusal had been bypassed.
        #[cfg(windows)]
        debug_assert!(
            spec.credentials.is_none(),
            "privilege::resolve must refuse user/group on Windows before a spawn is reached"
        );

        let (from_child_tx, from_child_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (to_child_tx, to_child_rx) = mpsc::channel(CHANNEL_CAPACITY);

        // The Windows shepherd channel: a named pipe the child opens by
        // name, rather than a descriptor it inherits.
        //
        // The fd-3 contract cannot hold here and the reason is not fixable
        // by cleverness: `command-fds` is a `pre_exec`-based unix-only crate,
        // and `cmd.exe` has no fd-3 redirection, so
        // `docs/shepherd-channel.md`'s decisive promise — "a shell script
        // doing `read -r line <&3` works" — has no Windows reading.
        //
        // **The wire format is untouched.** Newline-delimited JSON, the same
        // `ready`/`metric`/`action-reply` outbound and `shutdown`/`action`
        // inbound shapes, so `shep trigger`'s request/reply flow and its
        // correlation id survive unchanged. Only "how do I obtain the
        // handle" moves: the daemon exports `SHEP_CHANNEL_PIPE`, and an app
        // opens that path like any other file. `SHEP_CHANNEL_FD` is
        // deliberately NOT set, so an app can branch on which variable is
        // present rather than guessing from the platform.
        #[cfg(windows)]
        if spec.channel {
            use shep_core::transport;

            // Unique per spawn, not per home: two instances of one app must
            // not share a channel, and a restarted sheep must not inherit a
            // name a dying predecessor still holds.
            //
            // The nonce is a separate job from that uniqueness: the pipe is
            // created before the child exists, with the default security
            // descriptor (which grants read to Everyone and restricts write),
            // and the single `accept` below authenticates nobody. So a
            // hostile local account that reaches the pipe first starves a
            // `wait_ready` sheep and reads daemon-to-child frames.
            //
            // **128 bits closes prediction, not observation, and the
            // difference matters.** An attacker cannot guess this name. An
            // attacker CAN enumerate it: the pipe namespace lists to any
            // unprivileged local user (measured, 190 pipes from a
            // non-elevated session), so one polling it sees the name appear
            // and can race the child. Closing that needs a restrictive DACL,
            // which needs `create_with_security_attributes_raw`, which needs
            // unsafe -- and shep-core is `#![forbid(unsafe_code)]`, so it
            // would have to move behind `sys_windows`. Tracked in
            // `docs/specs/deferred.md`; the nonce is the speed bump until
            // then.
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

            // Accepted on a task rather than awaited here, because `spawn` is
            // synchronous and the child cannot connect until it has been
            // started, which happens below.
            //
            // The `closed()` arm is what bounds that task. An app that never
            // opens the pipe would otherwise park it forever, holding the
            // listener, `to_child_rx` and a sender clone for the daemon's
            // life, once per spawn and so again on every autorestart. Unix
            // has no equivalent leak to match: there `spawn_channel_pumps`
            // runs unconditionally and its reader ends at EOF when the
            // child's fd 3 closes. `Sender::closed` resolves when `run_sheep`
            // drops the receiver, which is that same lifetime, and
            // `Listener::accept` is cancel-safe, so racing the two is sound.
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

        #[cfg(unix)]
        if spec.channel {
            command.env("SHEP_CHANNEL_FD", "3");
            // Not negotiation — the shepherd still cannot ask an app what it
            // speaks — but an app that wants to be defensive can now tell a
            // channel it understands from one it does not, instead of
            // failing to parse a line with nothing connecting that failure to
            // a protocol change. One line, taken while it is still free.
            command.env("SHEP_CHANNEL_VERSION", CHANNEL_VERSION);
            let (daemon_end, child_end) = UnixStream::pair().map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel socketpair: {error}"))
            })?;
            let std_child_end = child_end.into_std().map_err(|error| {
                RunnerError::SpawnFailed(format!("shepherd channel into_std: {error}"))
            })?;
            // `O_NONBLOCK` on this descriptor is inherited, never chosen:
            // `UnixStream::pair()` sets it on BOTH ends because tokio's own
            // half needs it, and `into_std` documents that it leaves the flag
            // exactly as it found it. The child inherits whatever this fd
            // carries across the exec, so leaving it set hands every app a
            // non-blocking fd 3 — a plain `read <&3` gets `EAGAIN` rather
            // than parking, and a child waiting to be told something (the
            // `{"kind":"shutdown"}` of `shutdown_with_message`, say) fails
            // instead of waiting. Runtimes with an event loop set their own
            // descriptors non-blocking anyway and never notice; an app that
            // simply reads does. Clearing it here is what makes fd 3 an
            // ordinary blocking descriptor on the far side, and nothing on
            // this side wants it back: the daemon's end is a separate
            // descriptor, still tokio's, still non-blocking.
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

        // Containment, as early as it can possibly happen: the child exists
        // now and everything it spawns from here inherits the job. See
        // `sys_windows::Job::assign` for the residual race this cannot close
        // and why closing it would mean re-implementing `CreateProcessW`.
        //
        // A failure here is fatal to the spawn rather than a warning. A sheep
        // outside its job is one `kill_tree` cannot reach, so `shep stop`
        // would report success and leave the process running — the single
        // worst failure mode a supervisor has. Better to refuse the start.
        #[cfg(windows)]
        let job = {
            let job = crate::sys_windows::Job::create().map_err(|error| {
                RunnerError::SpawnFailed(format!("job object for {}: {error}", spec.name))
            })?;
            let handle = child.raw_handle().ok_or_else(|| {
                RunnerError::SpawnFailed("child exited before it could be contained".to_string())
            })?;
            if let Err(error) = job.assign(handle) {
                // The child is already running and is NOT in a job, so it
                // must not be left behind: nothing would be able to stop its
                // descendants afterwards.
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
        // Read off the handles this spawn is about to give away, and before
        // the `take`s below move them: this is the only place the numbers a
        // handover carries are known.
        #[cfg(unix)]
        let pipes = PipeFds {
            out: child.stdout.as_ref().map(AsRawFd::as_raw_fd),
            err: child.stderr.as_ref().map(AsRawFd::as_raw_fd),
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
            // Dropped rather than left dangling, so a caller's `is_closed()`
            // says "no pipe here" immediately instead of the send silently
            // buffering into a channel nobody will drain — the same choice the
            // no-channel arm above makes for fd 3.
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
/// currently open on it — `None` when the open failed (see [`open_append`]).
///
/// Generic over the sink only so a test can count the writes that reach it —
/// how OFTEN the buffer spills is the whole point of [`LOG_BUFFER`], and it
/// is observable in neither the bytes on disk nor the wall clock. Production
/// only ever builds the default.
#[derive(Debug)]
struct LogFile<W = tokio::fs::File> {
    path: PathBuf,
    handle: Option<BufWriter<W>>,
    /// When the oldest line the pump has not tried to flush yet was
    /// appended; the pump reads it as an [`IDLE_FLUSH`] deadline.
    ///
    /// Cleared by every flush ATTEMPT, successful or not: a file that cannot
    /// be written must not turn the idle flush into a twenty-per-second
    /// retry loop, and the next appended line re-arms it either way.
    buffered_since: Option<Instant>,
}

/// Where one stream's log handle comes from when a pump starts.
///
/// Two arms because a successor's pump starts differently from a spawn's: it
/// is handed a handle that is already open on the file, and opening the path
/// again instead would lose `O_APPEND` and the guarantee that goes with it
/// (see [`open_append`]). The path travels with the handle either way, so a
/// later [`LogCtl::Reopen`] behaves identically for both.
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
    /// The successor's half of a handover. `O_APPEND` is a file status flag
    /// on the open file description, so it crossed the `execve` with the
    /// descriptor and is still set; reopening `path` here would produce a
    /// handle that passes a naive write test and then writes at its own
    /// tracked offset, which is the sparse hole [`open_append`] documents.
    ///
    /// Nothing else about the file changes. [`Self::reopen`] still goes back
    /// through [`open_append`] by path, so a rotation works on a carried
    /// handle exactly as it does on an opened one.
    #[cfg(unix)]
    fn from_file(path: PathBuf, file: tokio::fs::File) -> Self {
        Self {
            path,
            handle: Some(BufWriter::with_capacity(LOG_BUFFER, file)),
            buffered_since: None,
        }
    }

    /// Opens `path` for appending, keeping the path for later reopens.
    ///
    /// A failed open is not fatal here — it is already logged, and the pump
    /// must still drain the child's streams whether or not it can write
    /// them anywhere. [`LogFile::reopen`] is the one that reports, because
    /// there a caller is waiting to hear.
    async fn open(path: PathBuf) -> Self {
        let handle = open_append(&path)
            .await
            .ok()
            .map(|file| BufWriter::with_capacity(LOG_BUFFER, file));
        Self {
            path,
            handle,
            buffered_since: None,
        }
    }

    /// Flushes and closes the current handle, then opens the path again.
    ///
    /// Flushing first is what makes [`LogCtl::Reopen`]'s acknowledgement
    /// worth having: an [`Self::append`] only reaches the buffer and a write
    /// through it only means the `write(2)` was queued, while `flush` empties
    /// the buffer and waits for the operation in flight — so every line read
    /// before the reopen has reached the OLD file (the renamed one, in the
    /// rotation this exists for) by the time the caller hears back. The
    /// buffer travels with the handle that is being dropped, so a reopen that
    /// skipped it would DISCARD those lines rather than merely delay them.
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
        self.buffered_since = None;
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
                self.handle = Some(BufWriter::with_capacity(LOG_BUFFER, handle));
                Ok(())
            }
            Err(error) => Err(ReopenError {
                message: format!("{}: {error}", self.path.display()),
            }),
        }
    }

    /// This stream's open log-file descriptor, or `None` when the open
    /// failed and there is no handle at all.
    ///
    /// Read off the live handle rather than remembered, so a
    /// [`Self::reopen`] that swapped the file cannot leave a stale number
    /// behind for a handover to carry.
    #[cfg(unix)]
    fn raw_fd(&self) -> Option<RawFd> {
        self.handle
            .as_ref()
            .map(|handle| handle.get_ref().as_raw_fd())
    }
}

impl<W: AsyncWrite + Unpin> LogFile<W> {
    /// Appends one line and its newline to the buffer, logging (never
    /// propagating) a write failure — a log we cannot write to must not stop
    /// the pump draining the child's pipes.
    ///
    /// The newline is a second write into that same buffer rather than a
    /// joined copy of the line: the file sees one contiguous run of bytes
    /// either way, so the copy bought nothing but an allocation per line.
    async fn append(&mut self, line: &str) {
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        let written = async {
            handle.write_all(line.as_bytes()).await?;
            handle.write_all(b"\n").await
        }
        .await;
        self.buffered_since.get_or_insert_with(Instant::now);
        if let Err(error) = written {
            tracing::error!(path = ?self.path, %error, "log file append failed");
        }
    }

    /// Writes out whatever the buffer holds and waits for it to reach the
    /// file, keeping the handle open.
    ///
    /// The whole of [`LogCtl::Flush`], and the reason `shep flush` has two
    /// halves: an [`Self::append`] only reaches the buffer, and a write
    /// through the buffer returns once the real `write(2)` is queued — so
    /// truncating the path without waiting here can empty the file a moment
    /// before lines it already accepted land at offset 0 of it.
    ///
    /// A stream whose open failed has no handle and nothing buffered, so it
    /// has nothing to wait for and answers `Ok`.
    ///
    /// # Errors
    ///
    /// A buffered or already-dispatched write failed — a full disk, an
    /// unlinked filesystem, an IO error the queued `write(2)` hit. Unlike
    /// [`Self::reopen`]'s own flush this is reported rather than logged:
    /// there no caller depends on the result (the handle is being replaced
    /// by a working one), while here the caller is about to truncate this
    /// exact path and the un-landed bytes are what it is racing.
    async fn flush(&mut self) -> Result<(), FlushError> {
        self.buffered_since = None;
        let Some(handle) = self.handle.as_mut() else {
            return Ok(());
        };
        handle.flush().await.map_err(|error| FlushError {
            message: format!("{}: {error}", self.path.display()),
        })
    }
}

/// The descriptor numbers of a pump's two stream readers.
///
/// Told to the pump rather than read off its readers, because the pump is
/// generic over them and an in-memory stream has no descriptor at all. The
/// caller holding the real `ChildStdout` is the one place the numbers are
/// known, and it reads them off the same object it is about to hand over.
///
/// Empty on Windows, which has no descriptors to carry and no handover to
/// carry them: `Arm::for_daemon` returns the stop-and-start arm there.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default)]
struct PipeFds {
    /// The read end of the child's stdout, while the pump still holds it.
    out: Option<RawFd>,
    /// The read end of the child's stderr, while the pump still holds it.
    err: Option<RawFd>,
}

/// See the unix definition above; there is nothing to carry here.
#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, Default)]
struct PipeFds;

/// Both of a sheep's log files — the pair one [`LogCtl::Reopen`] swaps, and
/// the pair one [`LogCtl::Flush`] drains — and the two stream descriptors
/// alongside them, which is the set a handover carries.
#[derive(Debug)]
struct LogFiles {
    out: LogFile,
    err: LogFile,
    /// The stream read ends the pump still holds, cleared per stream as
    /// each one ends.
    ///
    /// Cleared rather than left standing because the reader is dropped at
    /// the same moment, which closes the descriptor: a number the kernel is
    /// free to hand to the next `open` must never reach a handover blob.
    pipes: PipeFds,
}

impl LogFiles {
    /// The file a line from this stream is appended to (`err` picks stderr).
    fn stream(&mut self, err: bool) -> &mut LogFile {
        if err { &mut self.err } else { &mut self.out }
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

    /// Flushes both buffers, logging rather than reporting a failure — no
    /// caller is waiting on an idle flush, and the pump must go on draining
    /// the child's pipes whether or not its logs can be written.
    async fn flush_idle(&mut self) {
        for file in [&mut self.out, &mut self.err] {
            if let Err(error) = file.flush().await {
                tracing::error!(%error, "log file idle flush failed");
            }
        }
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
            // The flush comes FIRST and in the same request, which is the
            // whole point of the variant — see `LogCtl::ReportFds`. A
            // failure is logged rather than reported: the answer has no room
            // for one, and the descriptor is still the one to carry, since
            // the successor inherits the same handle on the same file and is
            // the only party left that could write to it.
            #[cfg(unix)]
            LogCtl::ReportFds { done } => {
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
                });
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

/// Waits for room on `logs_tx`, serving control requests and idle flushes
/// while it waits.
///
/// Returns `None` once the `logs` receiver is gone.
///
/// The idle flush is here for the same reason the control branch is: a pump
/// parked on a full `logs` channel is parked for as long as the sheep task
/// takes, which is unbounded. Without a branch of its own the line just
/// appended would sit in the buffer for exactly that long, and
/// [`IDLE_FLUSH`] would bound nothing.
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
        // Recomputed every iteration from the STORED mark, so losing the
        // race never extends the window (`snapshot::run_writer`'s debounce
        // is the same shape).
        let flush_at = files.flush_deadline();
        // Every branch is documented cancel-safe, as `select!` requires: a
        // `reserve` that loses the race has taken no slot, a `recv` that
        // loses it has taken no message, and a `sleep_until` that loses it
        // is rebuilt against the same absolute deadline.
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
/// The file write is ISSUED before the line is forwarded, but it lands in a
/// [`BufWriter`] rather than in the file, and a write through that buffer
/// only means the real `write(2)` was queued onto the blocking pool. A
/// receiver that observes a line on `logs_tx` therefore cannot conclude the
/// file already holds it. The barriers that can be relied on are
/// [`LogCtl::Reopen`]'s and [`LogCtl::Flush`]'s acknowledgements, which both
/// drain the buffer and wait; absent either, [`IDLE_FLUSH`] bounds how long
/// a line stays buffered.
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
        };
        let mut out_lines = stdout.map(|reader| BufReader::new(reader).lines());
        let mut err_lines = stderr.map(|reader| BufReader::new(reader).lines());

        while out_lines.is_some() || err_lines.is_some() {
            // Recomputed every iteration from the STORED mark; see
            // `reserve_slot`, which carries the same branch for the window
            // this loop cannot see.
            let flush_at = files.flush_deadline();
            tokio::select! {
                result = next_line(&mut out_lines) => {
                    match deliver_line(result, false, &mut files, &logs_tx, &mut ctl_rx).await {
                        AfterLine::KeepReading => {}
                        // Dropping the reader closes the descriptor, so the
                        // number stops being ours in the same statement it
                        // stops being readable.
                        AfterLine::StreamEnded => {
                            out_lines = None;
                            #[cfg(unix)]
                            {
                                files.pipes.out = None;
                            }
                        }
                        AfterLine::LogsClosed => break,
                    }
                }
                result = next_line(&mut err_lines) => {
                    match deliver_line(result, true, &mut files, &logs_tx, &mut ctl_rx).await {
                        AfterLine::KeepReading => {}
                        // As above: the number goes with the reader.
                        AfterLine::StreamEnded => {
                            err_lines = None;
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

                // Cancel-safe: rebuilt against the same absolute deadline
                // every iteration, so losing the race costs nothing.
                () = sleep_until(flush_at.unwrap_or_else(Instant::now)), if flush_at.is_some() => {
                    files.flush_idle().await;
                }
            }
        }
        // A `BufWriter` cannot flush itself as it drops, and every way out of
        // the loop above drops both of them: without this, whatever a child
        // wrote since the last flush would be lost at its exit — the lines an
        // operator reaches for first.
        files.flush_idle().await;
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
///
/// # Security
///
/// The open itself is [`open_log_path`]'s, which adds `O_NOFOLLOW` — so a
/// symlink planted at `path` is refused rather than appended through. That
/// flag rides ALONGSIDE `.append(true)`, never in place of it: dropping
/// `O_APPEND` brings back the sparse hole the paragraph above exists to
/// prevent. See [`open_log_path`] for what `O_NOFOLLOW` does and does not
/// cover.
///
/// [`check_log_ancestry`] runs FIRST, ahead of the `mkdir` below and not
/// merely ahead of the open. Ordering is the point: `create_dir_all` treats
/// an existing symlink-to-a-directory as a directory and walks straight
/// through it, so a check that ran after it would leave a root shepherd
/// having created `0700` directories at a path someone else chose before
/// refusing to write the log file there.
async fn open_append(path: &Path) -> io::Result<tokio::fs::File> {
    check_log_ancestry(path)
        .inspect_err(|error| tracing::error!(?path, %error, "log ancestry check failed"))?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let mut builder = tokio::fs::DirBuilder::new();
        builder.recursive(true);
        // No `DIR_MODE` on Windows — see `boot::create_dir_at_dir_mode`'s
        // Windows arm for why there is no scalar mode to set, and what is
        // guarding the control plane there instead.
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
/// mid-line, and a REPL reading the result would see a command neither caller
/// sent. Serial also means a line queued behind one the app is not reading
/// waits — which is correct, and which is why the caller bounds its own wait
/// rather than this task bounding the write (a write abandoned halfway would
/// leave a partial line in the pipe, which is worse than a slow one).
///
/// A request whose caller has stopped listening is DROPPED rather than
/// written. The supervisor bounds its own wait at `STDIN_WRITE_TIMEOUT` and
/// abandons the `oneshot` when that expires; without this check the request
/// stayed queued and landed whenever the app finally drained its pipe, so an
/// operator who read `not_written` and retried twice had all three lines
/// delivered at once — a command nobody meant to send, which is the same
/// hazard `rpc`'s newline and carriage-return refusal exists to prevent.
///
/// It is a reduction, not a guarantee, and the one case it cannot reach is
/// the commonest one: the line the pump is already blocked in `write_all` on
/// is past the point where anything here could take it back, and abandoning
/// it halfway would leave a partial line in the pipe. `LineOutcome::NotWritten`
/// says so.
///
/// Generic over the writer for the same reason [`spawn_log_pump`] is generic
/// over its readers: a `tokio::io::duplex` half is an `AsyncWrite`, so a test
/// can wedge this pump on a full pipe with no child process in it. The only
/// production caller passes a [`ChildStdin`](tokio::process::ChildStdin).
///
/// Ends when the last sender drops, which closes the child's stdin and gives
/// the app EOF. That is the sheep task letting go of `ProcIo`, i.e. the child
/// exiting — never before.
fn spawn_stdin_pump<W>(stdin: Option<W>, mut rx: mpsc::Receiver<StdinWrite>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut stdin) = stdin else {
            // `Stdio::piped()` was set and `child.stdin` was still `None`,
            // which std does not do — but answering nothing would hang every
            // caller, so the requests are drained and refused instead.
            while let Some(StdinWrite { done, .. }) = rx.recv().await {
                let _ = done.send(Err(RunnerError::WriteFailed(
                    "this child has no stdin pipe".to_string(),
                )));
            }
            return;
        };
        while let Some(StdinWrite { line, done }) = rx.recv().await {
            if done.is_closed() {
                // Nobody is waiting for this line any more: the supervisor's
                // `STDIN_WRITE_TIMEOUT` expired and it dropped the receiver.
                // Writing it now would deliver a line the operator has
                // already been told was not written — see this function's
                // own doc.
                continue;
            }
            let mut bytes = line.into_bytes();
            // Exactly one terminator, appended here and nowhere else. The wire
            // carries the line without one (`Request::SendLine::line`), so this
            // is the single place the question "is a newline included" is ever
            // answered.
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
/// Generic over the transport rather than naming one: the daemon's end is a
/// `UnixStream` half of a socketpair on unix and an accepted named-pipe
/// server instance on Windows, and the pumps care about neither — only that
/// it carries bytes both ways. `tokio::io::split` in place of
/// `UnixStream::into_split` is what makes that true, since `NamedPipeServer`
/// has no `into_split` of its own.
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

    /// One pump over two streams and two real files — everything
    /// [`spawn_log_pump`] takes, with no child process involved.
    ///
    /// Generic over the writing side only so the descriptor cases can swap
    /// the in-memory pair for a real one: an in-memory pipe has no
    /// descriptor at all, which is exactly why the pump is TOLD its stream
    /// numbers rather than reading them off its own readers. Everything
    /// else about the two harnesses is identical, and every case that is
    /// about bytes uses the cheaper [`PumpHarness::start`].
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

    /// A pump reading two REAL pipes, for the cases that are about
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

        /// Sends a [`LogCtl::ReportFds`] and waits for the answer.
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
    }

    impl<W: AsyncWrite + Unpin> PumpHarness<W> {
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
    /// A line observed on `logs` has reached the stream's BUFFER, not
    /// necessarily the file (see [`spawn_log_pump`]'s ordering note), so this
    /// polls for [`IDLE_FLUSH`] to write it through instead of asserting it
    /// already has.
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
    /// The numbers must be the pump's own, not a guess: an fd number is only
    /// meaningful in the process that owns it, and the whole handover is
    /// built on carrying exactly these. The two stream numbers are asserted
    /// against the ones this harness handed over, which is the half a
    /// looser `is_some()` would let a wrong-but-open descriptor pass.
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

    /// Fails if a report still names a stream whose descriptor the pump has
    /// already let go of.
    ///
    /// The `files.pipes.out = None` in the `StreamEnded` arm is the guard
    /// that keeps a closed descriptor out of a handover blob, and nothing
    /// else in this file exercises it.
    /// [`a_pump_reports_the_descriptors_it_holds`] reports while both
    /// streams are live, so it passes whether or not the clearing exists.
    ///
    /// A number is the whole of what the blob carries, and a closed one is
    /// free for the next `open` in this process to be handed. The successor
    /// would then adopt whatever that open produced as this sheep's stdout,
    /// which `adopt`'s kind check catches only when the two happen to be
    /// different kinds of object.
    ///
    /// One stream, not both. `err_pipe` staying at the harness's own number
    /// is what makes this a case about the ended stream rather than about a
    /// pump that stopped answering.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_report_after_a_stream_ends_names_no_descriptor_for_it() {
        let mut pump = PumpHarness::start_over_pipes();
        pump.feed(false, "before-the-eof").await;

        drop(pump.out_writer);
        let_the_pump_settle().await;

        // Sent through the field rather than through `report_fds`, because
        // dropping the writer above moves out of the harness and a `&self`
        // method is no longer reachable. Same request either way.
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
    /// settling in between — deliberately not [`assert_file_settles`], since
    /// polling would let [`IDLE_FLUSH`] pass the case that the report itself
    /// is supposed to have flushed. A blob whose descriptors are ready but
    /// whose bytes are not is a log gap the successor cannot repair, because
    /// the bytes died with the image at the exec.
    #[cfg(unix)]
    #[tokio::test]
    async fn reporting_flushes_first() {
        let mut pump = PumpHarness::start_over_pipes();
        pump.feed(false, "before-the-blob").await;
        pump.feed(true, "and-on-stderr").await;

        let _ = pump.report_fds().await;

        assert_eq!(
            fs::read_to_string(&pump.out_path).unwrap(),
            "before-the-blob\n"
        );
        assert_eq!(
            fs::read_to_string(&pump.err_path).unwrap(),
            "and-on-stderr\n"
        );
    }

    /// A sink standing in for the [`tokio::fs::File`] a real [`LogFile`]
    /// holds, counting the writes that reach it.
    ///
    /// The count is the only place the buffering is visible. Bytes on disk
    /// come out the same either way, and the timing difference is exactly
    /// what a contended runner cannot be asked about — so a counter is what
    /// pins [`LOG_BUFFER`] against a future append that writes straight
    /// through.
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
    /// That write is the whole cost this change exists to remove:
    /// `tokio::fs::File` hands each one to the blocking pool, measured at
    /// 32.8 us of daemon CPU per line against 0.99 us for the `write(2)`
    /// underneath it. A buffer turns N of them into one per bufferful, and
    /// the count is the only observable that says so.
    ///
    /// Paused clock (IR-33), and that is load-bearing rather than
    /// conventional: no time passes, so [`IDLE_FLUSH`] never fires and every
    /// write counted below is the buffer spilling or the explicit flush at
    /// the end. Nothing here can be made flaky by a contended runner.
    #[tokio::test(start_paused = true)]
    async fn a_run_of_lines_costs_one_write_per_bufferful_not_one_per_line() {
        // 69 characters, so `append`'s newline makes the 70-byte line the
        // measurement behind `LOG_BUFFER` used.
        const LINE: &str = "012345678901234567890123456789012345678901234567890123456789012345678";
        const LINE_BYTES: usize = LINE.len() + 1;
        let lines = 3 * LOG_BUFFER / LINE_BYTES;

        let sink = WriteCounter::default();
        let mut log = LogFile {
            path: PathBuf::from("counted.log"),
            handle: Some(BufWriter::with_capacity(LOG_BUFFER, sink.clone())),
            buffered_since: None,
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
    /// something asks — that is, if [`IDLE_FLUSH`] bounds nothing.
    ///
    /// A sheep that logs once and goes quiet is the whole case: no `Flush`,
    /// no reopen, and no second line to push the first one out. One line of
    /// 70 bytes is nowhere near [`LOG_BUFFER`], so the idle flush is the only
    /// thing that can write it through.
    ///
    /// Paused clock and a counting sink (IR-33), for two different reasons.
    /// The clock, because the wait is on [`IDLE_FLUSH`] rather than on any
    /// real work, and a real 50 ms is a claim about the machine. The sink,
    /// because "the bytes left the buffer" is what is being asserted, and a
    /// file would answer that only once the blocking pool caught up — which
    /// is the wall clock again, through the filesystem.
    #[tokio::test(start_paused = true)]
    async fn a_line_from_a_sheep_that_then_goes_quiet_still_reaches_its_file() {
        const LINE: &str = "the-only-line";
        let sink = WriteCounter::default();
        let mut log = LogFile {
            path: PathBuf::from("quiet.log"),
            handle: Some(BufWriter::with_capacity(LOG_BUFFER, sink.clone())),
            buffered_since: None,
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
            LINE.len() + 1,
            "the line and its newline must reach the file once the window closes"
        );
        assert_eq!(
            log.buffered_since, None,
            "a flush must retire the deadline, or the pump re-arms it every window"
        );
    }

    /// Fails if the idle-flush window is measured from the NEWEST buffered
    /// line rather than the oldest: a stream logging just inside the window
    /// would then keep pushing the deadline out, and "buffered but not on
    /// disk" would have no bound at all.
    ///
    /// Paused clock (IR-33): the question is arithmetic over `Instant`s, and
    /// an append that fits in the buffer touches no file to wait on.
    #[tokio::test(start_paused = true)]
    async fn the_idle_flush_window_is_measured_from_the_oldest_buffered_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = LogFiles {
            out: LogFile::open(dir.path().join("out.log")).await,
            err: LogFile::open(dir.path().join("err.log")).await,
            pipes: PipeFds::default(),
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
    /// write them into the FRESH file — an operator reading the archive finds
    /// a gap, and reading the live log finds lines from before the rotation.
    /// Exact equality on both paths is what tells those two apart from a
    /// reopen that got it right.
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

        assert_eq!(fs::read_to_string(&rotated).unwrap(), before);
        assert_eq!(fs::read_to_string(&pump.out_path).unwrap(), "");

        let mut after = String::new();
        for n in 0..40 {
            let line = format!("after-{n}");
            pump.feed(false, &line).await;
            after.push_str(&line);
            after.push('\n');
        }
        pump.flush().await;

        assert_eq!(fs::read_to_string(&pump.out_path).unwrap(), after);
        assert_eq!(
            fs::read_to_string(&rotated).unwrap(),
            before,
            "the archive must have stopped growing at the swap"
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

    /// Fails if [`open_append`] stops opening through
    /// [`open_log_path`] — without the `O_NOFOLLOW` it adds, a symlink planted
    /// where a sheep's `out_file` will be is opened and appended through, and
    /// every log line the sheep writes lands in whatever it points at. Nothing
    /// else in this module can see that: every other case here opens a real
    /// file, where following a symlink and refusing to are the same open.
    ///
    /// The target's bytes are the assertion that matters — a `create(true)`
    /// open that followed the link would leave them and add to them, so
    /// "unchanged" is what separates a refusal from a successful follow. The
    /// message is asserted too, because [`LogFile::reopen`] hands it to an
    /// operator who is waiting to hear which path failed and why.
    ///
    /// `#[cfg(unix)]`: this whole module is, and so are `O_NOFOLLOW` and
    /// `std::os::unix::fs::symlink`.
    /// `cfg(unix)` because it needs `std::os::unix::fs::symlink` to build
    /// the hazard. The refusal it exercises is `open_log_path`'s
    /// `O_NOFOLLOW`, which has no Windows counterpart wired up yet — a
    /// Windows reparse-point refusal would be
    /// `FILE_FLAG_OPEN_REPARSE_POINT` plus a `symlink_metadata` check, both
    /// safe std, and it is named in the operator docs as a gap rather than
    /// silently assumed to hold.
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
    /// An append reaches the buffer and nothing else, and even once the
    /// buffer is written through, a `tokio::fs::File` reports a failed write
    /// on the NEXT operation rather than the one that failed: `write_all`
    /// returns as soon as the real `write(2)` is queued on the blocking pool,
    /// so that write's error is owed to whoever asks next. `flush` is who
    /// asks, for both layers. This is the whole reason the flush half of
    /// `shep flush` reports where [`LogFile::reopen`]'s own flush logs — and
    /// a flush that answered without asking would swallow the only signal
    /// there is that a sheep's log went unwritten.
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
            handle: Some(BufWriter::with_capacity(
                LOG_BUFFER,
                tokio::fs::File::open(&path).await.unwrap(),
            )),
            buffered_since: None,
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

    /// Fails if a log handle carried across a handover writes anywhere but
    /// the end of the file.
    ///
    /// Not merely "the handle is writable". `O_APPEND` is a file status flag
    /// on the open file description, so it crosses an exec with the
    /// descriptor, and a handle that lost it writes at its own tracked
    /// offset instead: the first line already in the file is overwritten
    /// here, and after a `copytruncate` rotation the same difference leaves
    /// a sparse hole the size of everything rotated away. The content
    /// assertion below is what tells the two apart, which is why it reads
    /// both lines rather than checking the file is non-empty.
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
            fs::read_to_string(&path).unwrap(),
            "first\nsecond\n",
            "a carried handle must append, not write at its own offset"
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

    /// Room in the in-memory pipe standing in for a child's stdin.
    ///
    /// Four bytes, far under the first line the stdin case writes, so the
    /// pump parks inside `write_all` on that first request and cannot reach
    /// the next one until the test starts reading. That parking is what
    /// makes the case's ordering a fact rather than a hope.
    const STDIN_BUFFER: usize = 4;

    // fails if the pump writes a line whose caller has stopped waiting. The
    // supervisor bounds its own wait at `STDIN_WRITE_TIMEOUT` and then
    // abandons the `oneshot`; a pump that still wrote the request would
    // deliver a line the operator was already told was `not_written`, so an
    // operator who retried twice would have all three arrive at once.
    //
    // Real clock rather than this crate's usual paused one (IR-33), like
    // every other case in this module: the forcing mechanism is the pipe
    // being drained and then closed, which no virtual-time advance reaches.
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

    /// A [`SpawnSpec`] carrying only what [`what_exec_will_find`] reads. Every
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

    /// fails if the check stops catching the defect: the third app of an
    /// eleven-app Flockfile pointing at an unbuilt binary, which registered
    /// two apps, failed, and never reached the other eight.
    ///
    /// `Impossible`, which its caller refuses the whole batch over, and only
    /// ever for a program with a `/` in it. A path is a claim about the
    /// filesystem, and this is what makes it a claim the daemon can settle.
    ///
    /// The reason string is asserted to carry the RESOLVED path, not the
    /// `./proto-enum-api` the operator wrote, because "no such file:
    /// ./proto-enum-api" is exactly the message that left them guessing
    /// which directory it was looked for in.
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

    /// fails if a filesystem error that is not "no such file" is ever read as
    /// absence.
    ///
    /// [`Path::exists`] returns `false` on ANY [`fs::metadata`] error, so an
    /// unreadable intermediate directory, an unsettled mount and a race all
    /// look exactly like a missing file. Under the previous `exists` call
    /// each of them answered [`Preflight::Impossible`] and refused a whole
    /// batch over a filesystem that was merely unavailable for a moment.
    ///
    /// Two provocations, because one of them cannot be trusted to bite
    /// everywhere:
    ///
    /// - **A metadata error that is not absence.** On unix that is
    ///   `ENOTDIR`: a regular file used as an intermediate path component.
    ///   Uid-independent, so it runs on every machine and in every
    ///   container, and this is the case that keeps the test honest when
    ///   the second one is skipped. Windows needs a different provocation,
    ///   because a path through a file answers `ERROR_PATH_NOT_FOUND`
    ///   there, which IS absence and would make the case vacuous. An
    ///   invalid filename is used instead, for `ERROR_INVALID_NAME`.
    /// - **`EACCES`**, a `chmod 000` directory. Unix only, and not for
    ///   want of the scenario: Windows has no `set_permissions` that can
    ///   produce it, so reproducing it there means an ACL edit rather
    ///   than a one-line chmod. The scenario an operator
    ///   actually hits, and the one worth naming, but root bypasses
    ///   directory search permission, so under a root CI container the
    ///   `chmod` does not bite and the lookup would answer a true
    ///   `NotFound`. Skipped there rather than asserted vacuously.
    ///
    /// The mode is restored BEFORE the assertion rather than after: a failed
    /// assertion panics, and a `TempDir` whose `Drop` cannot descend into a
    /// `chmod 000` directory turns one red test into a leaked directory and
    /// a second, unrelated error.
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
        // the name is refused before anything is looked up. A path through
        // a file will not do here: Windows answers that with
        // ERROR_PATH_NOT_FOUND, which is absence, and the assertion below
        // would be vacuous.
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

    /// fails if a bare command off the PATH ever becomes `Impossible`, which
    /// is the assertion this case exists for.
    ///
    /// `Doubtful` is reported and carried on with, never refused. The
    /// daemon's own PATH under the unit `shep startup` installs is not the
    /// shell an operator tested in: homebrew's `node` on Apple Silicon is in
    /// `/opt/homebrew/bin` and nvm's is under `$HOME`, so a `shep startup`
    /// flock whose one Node app cannot resolve `node` must still bring up
    /// every other app. Refusing here would keep the whole flock down.
    ///
    /// The PATH that was searched is in the message on purpose. "`node` is
    /// not on the shepherd's PATH" without saying WHICH path sends an
    /// operator to check the one in their terminal, which is the one that
    /// works.
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
    // `tests/real_runner.rs`; this one case is reachable with no process at
    // all, so it belongs here (IR-38).
    /// `cfg(unix)` alongside `signal_group`, the function it guards. There
    /// is no negative-pid primitive on Windows and so no zero-pid hazard:
    /// `kill_tree` addresses a job HANDLE, which cannot accidentally name
    /// the daemon's own group the way `kill(0, ..)` can.
    #[cfg(unix)]
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
