//! The PID-1 init split: whether this process should become the init
//! (`should_split`), the init's own loop (`run_init`), and the classifier
//! that tells the supervisor's own exit apart from an orphan's
//! (`classify`).
//!
//! The init is a separate process because tokio reaps its own children by
//! their exact pid when `SIGCHLD` fires: a blind `waitpid(-1, WNOHANG)`
//! loop in the same process races it and steals statuses the supervisor
//! needs. `commands::runtime` spawns the supervisor half by re-executing
//! this binary with `--supervise`.

use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal as unix_signal};

use crate::exit::ExitCode;

/// What one `waitpid` return means to the init loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// The supervisor child exited, carrying its own code or, if it died
    /// by a signal, `128 + signal`.
    Supervisor(u8),
    /// Somebody else's orphan, reaped and forgotten.
    Orphan,
    /// Nothing is ready; stop draining until the next `SIGCHLD` or tick.
    Nothing,
    /// No children at all: the drain loop's other exit.
    NoChildren,
}

/// One `waitpid(-1, WNOHANG)` result, in [`classify`]'s own vocabulary
///
/// Not `nix::sys::wait::WaitStatus`, which has no variant for "no children
/// left": that arrives as `Err(ECHILD)` and is one of the drain loop's two
/// exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The process at `pid` exited normally with `code`.
    Exited {
        /// The pid that exited.
        pid: i32,
        /// Its exit code, `WEXITSTATUS`.
        code: i32,
    },
    /// The process at `pid` was killed by `signal`.
    Signaled {
        /// The pid that was signalled.
        pid: i32,
        /// The signal that killed it, `WTERMSIG`.
        signal: i32,
    },
    /// Nothing was ready to report (`WNOHANG` with no pending status).
    StillAlive,
    /// `waitpid` answered `ECHILD`: no children left to wait for.
    NoChildren,
}

/// Classifies one `waitpid(-1, WNOHANG)` result
///
/// Keep it `cfg`-free and pure: [`outcome`] is the only platform arm, so a
/// Linux-only call cannot hide in a branch this machine never compiles.
///
/// A signalled child reports `128 + signal`, the convention `docker
/// inspect` reads.
#[must_use]
pub fn classify(status: WaitOutcome, supervisor: i32) -> Reaped {
    match status {
        WaitOutcome::Exited { pid, code } if pid == supervisor => Reaped::Supervisor(code as u8),
        WaitOutcome::Exited { .. } => Reaped::Orphan,
        WaitOutcome::Signaled { pid, signal } if pid == supervisor => {
            Reaped::Supervisor((128 + signal) as u8)
        }
        WaitOutcome::Signaled { .. } => Reaped::Orphan,
        WaitOutcome::StillAlive => Reaped::Nothing,
        WaitOutcome::NoChildren => Reaped::NoChildren,
    }
}

/// Maps one `waitpid` return into [`classify`]'s own vocabulary
///
/// Every unhandled status and every other `Errno`, `EINTR` included, maps
/// to [`WaitOutcome::StillAlive`] and is logged: PID 1 must not go down on
/// a transient event, and an added flag must not panic it either.
fn outcome(result: Result<WaitStatus, nix::errno::Errno>) -> WaitOutcome {
    use nix::errno::Errno;
    match result {
        Ok(WaitStatus::Exited(pid, code)) => WaitOutcome::Exited {
            pid: pid.as_raw(),
            code,
        },
        Ok(WaitStatus::Signaled(pid, sig, _core_dumped)) => WaitOutcome::Signaled {
            pid: pid.as_raw(),
            signal: sig as i32,
        },
        Ok(WaitStatus::StillAlive) => WaitOutcome::StillAlive,
        Err(Errno::ECHILD) => WaitOutcome::NoChildren,
        Ok(other) => {
            eprintln!(
                "shep runtime: init's waitpid reported an unexpected status {other:?} (ignored)"
            );
            WaitOutcome::StillAlive
        }
        Err(errno) => {
            eprintln!(
                "shep runtime: init's waitpid reported {errno} (ignored, not fatal to PID 1)"
            );
            WaitOutcome::StillAlive
        }
    }
}

/// Drains pending statuses until nothing is ready, returning as soon as the
/// supervisor's own exit is seen
///
/// `Pid::from_raw(-1)` is load-bearing: waiting on `supervisor`'s pid alone
/// would leave every reparented orphan a zombie.
fn drain(supervisor: i32) -> Option<u8> {
    loop {
        let result = waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG));
        match classify(outcome(result), supervisor) {
            Reaped::Supervisor(status) => return Some(status),
            Reaped::Orphan => continue,
            Reaped::Nothing | Reaped::NoChildren => return None,
        }
    }
}

/// Whether this process should become the PID-1 init and re-exec itself as
/// the supervisor
///
/// `forced` is `$SHEP_FORCE_INIT`, read by the caller, so a test harness
/// can drive a real init. `supervise` wins over both other arguments: the
/// child inherits the environment, so a `forced` that could override it
/// would split again at every level.
#[must_use]
pub const fn should_split(pid: u32, supervise: bool, forced: bool) -> bool {
    !supervise && (pid == 1 || forced)
}

/// Builds (but does not spawn) the supervisor half's command
///
/// Raw `args_os` and `current_exe` rather than the parsed [`crate::cli::Cli`],
/// so the child's argv keeps the name it was invoked under: `shep-runtime x`
/// re-execs as `shep-runtime x --supervise`.
///
/// # Errors
/// [`std::env::current_exe`] failed to resolve this binary's own path.
fn supervisor_command() -> std::io::Result<std::process::Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).arg("--supervise");
    Ok(cmd)
}

/// The async body of [`run_init`], factored out to keep the
/// `std::process::exit` call to one site
///
/// Nothing here may panic: no `unwrap`, no `expect`, no indexing. A panic
/// in PID 1 takes the container down with no diagnostic. Errors are logged
/// and the loop continues, except a failure to spawn the supervisor or to
/// arm a handler before it, which ends the loop.
async fn init_loop() -> i32 {
    macro_rules! armed_or_fail {
        ($kind:expr, $name:literal) => {
            match unix_signal($kind) {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!(
                        "shep runtime: init could not register a {} handler: {err}",
                        $name
                    );
                    return i32::from(ExitCode::Failure as u8);
                }
            }
        };
    }

    // Armed before the supervisor is spawned: a signal that arrived in the
    // gap between spawn and registration would otherwise be lost.
    let mut sigterm = armed_or_fail!(SignalKind::terminate(), "SIGTERM");
    let mut sigint = armed_or_fail!(SignalKind::interrupt(), "SIGINT");
    let mut sighup = armed_or_fail!(SignalKind::hangup(), "SIGHUP");
    let mut sigquit = armed_or_fail!(SignalKind::quit(), "SIGQUIT");
    let mut sigchld = armed_or_fail!(SignalKind::child(), "SIGCHLD");

    // The handle is dropped once its pid is read: `drain`'s `waitpid(-1, …)`
    // is the only wait in this process, and `Child::drop` neither kills nor
    // waits.
    let supervisor_pid = match supervisor_command().and_then(|mut cmd| cmd.spawn()) {
        Ok(child) => child.id() as i32,
        Err(err) => {
            eprintln!("shep runtime: init could not start the supervisor: {err}");
            return i32::from(ExitCode::Failure as u8);
        }
    };
    eprintln!("shep runtime: init supervising pid {supervisor_pid}");

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    /// Forwards `sig` to `pid` if a signal actually arrived. A `None`
    /// `received` means the stream closed, which would otherwise busy-loop
    /// this arm.
    fn forward_if_live(received: Option<()>, pid: i32, sig: Signal) {
        if received.is_none() {
            return;
        }
        if let Err(err) = signal::kill(Pid::from_raw(pid), sig) {
            eprintln!("shep runtime: init could not forward {sig} to pid {pid}: {err}");
        }
    }

    loop {
        tokio::select! {
            received = sigterm.recv() => forward_if_live(received, supervisor_pid, Signal::SIGTERM),
            received = sigint.recv() => forward_if_live(received, supervisor_pid, Signal::SIGINT),
            received = sighup.recv() => forward_if_live(received, supervisor_pid, Signal::SIGHUP),
            received = sigquit.recv() => forward_if_live(received, supervisor_pid, Signal::SIGQUIT),
            received = sigchld.recv() => {
                if received.is_some() && let Some(status) = drain(supervisor_pid) {
                    return i32::from(status);
                }
            }
            _ = ticker.tick() => {
                if let Some(status) = drain(supervisor_pid) {
                    return i32::from(status);
                }
            }
        }
    }
}

/// Runs as PID 1: spawns the supervisor, forwards signals to it, and reaps
/// every process the kernel reparents here
///
/// Spawns with `std::process::Command`: tokio's own process type reaps by
/// pid on `SIGCHLD` and would race this loop's `waitpid(-1)`. Signals go to
/// the supervisor's pid, not its process group, which the supervisor runs
/// its own stop ladder over. Once the supervisor is gone nothing waits for
/// a remaining orphan.
///
/// Never returns. It exits through `std::process::exit`, since [`ExitCode`]
/// cannot represent a relayed status such as 137. Nothing is lost by
/// skipping destructors: no socket, no flock, no log subscriber.
pub async fn run_init() -> std::convert::Infallible {
    let status = init_loop().await;
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_supervisor_is_told_apart_from_every_orphan() {
        assert_eq!(
            classify(WaitOutcome::Exited { pid: 7, code: 3 }, 7),
            Reaped::Supervisor(3)
        );
        assert_eq!(
            classify(WaitOutcome::Exited { pid: 8, code: 3 }, 7),
            Reaped::Orphan
        );
    }

    #[test]
    fn a_signalled_supervisor_exits_128_plus_the_signal() {
        assert_eq!(
            classify(WaitOutcome::Signaled { pid: 7, signal: 9 }, 7),
            Reaped::Supervisor(137)
        );
    }

    #[test]
    fn no_children_and_nothing_ready_are_told_apart() {
        assert_eq!(classify(WaitOutcome::NoChildren, 7), Reaped::NoChildren);
        assert_eq!(classify(WaitOutcome::StillAlive, 7), Reaped::Nothing);
    }

    #[test]
    fn the_init_split_does_not_fire_outside_pid_one() {
        assert_ne!(std::process::id(), 1, "a test harness is never PID 1");
        assert!(!should_split(std::process::id(), false, false));
    }

    #[test]
    fn supervise_disables_the_split_whatever_the_pid_and_the_switch_say() {
        assert!(should_split(1, false, false));
        assert!(!should_split(1, true, false));
        assert!(!should_split(4242, false, false));
        assert!(
            should_split(4242, false, true),
            "the test switch reaches the init"
        );
        assert!(
            !should_split(4242, true, true),
            "and --supervise still wins"
        );
        assert!(!should_split(1, true, true), "and it still wins at PID 1");
    }

    /// Lives here rather than in `tests/init.rs` because `drain` is private
    /// to this crate.
    #[cfg(target_os = "linux")]
    #[test]
    fn drain_reaps_a_real_reparented_orphan() {
        use std::io::Read as _;

        nix::sys::prctl::set_child_subreaper(true).expect("Linux has supported this since 3.4");

        // Backgrounds a short-lived grandchild, prints its pid and exits,
        // reparenting the grandchild to this test process.
        let mut shell = std::process::Command::new("/bin/sh")
            .args(["-c", "(sleep 0.2; exit 3) & echo $!"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the shell");
        let mut printed_pid = String::new();
        shell
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_string(&mut printed_pid)
            .expect("read the grandchild's pid off the shell's stdout");
        let _ = shell.wait();
        let grandchild: i32 = printed_pid
            .trim()
            .parse()
            .expect("the shell printed a valid pid");

        // A pid this loop will never see exit: `drain` must not confuse
        // the orphan for it.
        let bogus_supervisor = -2;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut still_present = true;
        while std::time::Instant::now() < deadline && still_present {
            assert_eq!(
                drain(bogus_supervisor),
                None,
                "a fictitious supervisor pid must never be reported as reaped"
            );
            still_present =
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_ok();
            if still_present {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(
            !still_present,
            "the grandchild (pid {grandchild}) must have been reaped by `drain`, not left a zombie"
        );
    }
}
