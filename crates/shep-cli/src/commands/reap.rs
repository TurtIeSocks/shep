//! The PID-1 init split: whether this process should become the init
//! (`should_split`), the init's own loop (`run_init`), and the classifier
//! that tells the supervisor's own exit apart from an orphan's
//! (`classify`).
//!
//! Read decision 14 in Phase 15's plan before touching this file. The short
//! version: tokio's own async process type reaps its own children by
//! calling `waitpid` on their exact pid when `SIGCHLD` fires, so a blind
//! `waitpid(-1, WNOHANG)` loop in the *same* process races it and
//! sometimes wins, stealing the status tokio needed and turning a clean
//! exit into an `io::Error`. So
//! `shep runtime` splits into two processes when it is PID 1: this module is
//! the init half, forwarding signals to the supervisor and reaping every
//! process the kernel reparents here; `commands::runtime` spawns the
//! supervisor half by re-executing this same binary with `--supervise`.
//!
//! [`classify`] and its `Reaped`/`WaitOutcome` vocabulary carry no
//! `target_os` branch of their own, deliberately: the platform arm
//! ([`outcome`]) is the only place a Linux-only call could hide inside a
//! branch this machine never compiles, so it is kept to a bare `match` with
//! no logic in it.

use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal as unix_signal};

use crate::exit::ExitCode;

/// What one `waitpid` return means to the init loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// The supervisor child exited. Carries the status to exit with —
    /// its own code, or `128 + signal` if it died by one.
    Supervisor(u8),
    /// Somebody else's orphan, reaped and forgotten.
    Orphan,
    /// Nothing is ready right now — stop looping and wait for the next
    /// `SIGCHLD` or tick.
    Nothing,
    /// There are no children at all. The supervisor is already accounted
    /// for by an earlier `Supervisor`, if it is ever going to be; this is
    /// the drain loop's other exit.
    NoChildren,
}

/// One `waitpid(-1, WNOHANG)` result, in [`classify`]'s own vocabulary.
///
/// `Result`, not `nix::sys::wait::WaitStatus`, because "there are no
/// children" is not a status — nix 0.29's `WaitStatus` has no such variant,
/// and `ECHILD` arrives as `Err`. It is one of the drain loop's two exits,
/// so a shim that could not represent it would not compile against the loop
/// it exists for.
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

/// Classifies one `waitpid(-1, WNOHANG)` result.
///
/// `cfg`-free and pure, deliberately: the platform arm around it
/// ([`outcome`]) is two lines, so a Linux-only method call cannot hide
/// inside a branch this machine never compiles.
///
/// The exit status follows the shell convention the container ecosystem
/// reads: a signalled child is `128 + signal`, so a supervisor killed by
/// `SIGKILL` reports 137 exactly as `docker inspect` expects.
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

/// Maps one `waitpid` return into [`classify`]'s own vocabulary.
///
/// `Stopped`/`Continued`/the ptrace variants need flags this loop never
/// passes (`WUNTRACED`/`WCONTINUED`), so they cannot arrive in practice —
/// mapped to [`WaitOutcome::StillAlive`] rather than matched as
/// unreachable, so a future flag addition fails loudly instead of
/// panicking PID 1. Every other `Err` — `EINTR` above all — is equally
/// transient and must not end the loop; both cases are logged so a stuck
/// event stays visible without taking the process down.
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

/// Drains every pending `waitpid(-1, WNOHANG)` result — the supervisor and
/// any orphan the kernel reparented here — until nothing is left ready.
/// Returns as soon as the supervisor's own exit is seen; see [`run_init`]'s
/// own doc for why nothing waits for a remaining orphan once the
/// supervisor is gone.
///
/// `Pid::from_raw(-1)` is load-bearing: reaping only `supervisor`'s pid
/// (`Pid::from_raw(supervisor)`) would leave every reparented orphan a
/// zombie, which is the whole reason PID 1 needs this loop at all.
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
/// the supervisor.
///
/// `const` and pure over three scalars, so every case can be asserted — a
/// test harness is never PID 1, and the branch whose wrong answer is a fork
/// bomb is exactly the one that cannot be reached by running the code.
///
/// `forced` is `$SHEP_FORCE_INIT`, read by the caller and never here. It
/// exists so a test harness can drive a real init at all, following
/// `$SHEP_TERM_PANIC_PROBE`'s shape elsewhere in this crate — without it
/// the signal forwarding, which is the reason an init exists, has no test
/// that can fail.
///
/// **`supervise` wins over everything.** The init passes `--supervise` to
/// its child and the child inherits the environment, so a `forced` that
/// could override it would make every child split again: `shep runtime`
/// under that variable would be a fork bomb rather than a test.
#[must_use]
pub const fn should_split(pid: u32, supervise: bool, forced: bool) -> bool {
    !supervise && (pid == 1 || forced)
}

/// Builds (but does not spawn) the supervisor half's command: this same
/// binary, re-executed with this process's own arguments plus
/// `--supervise`.
///
/// `std::env::args_os().skip(1)` — not the parsed [`crate::cli::Cli`] —
/// re-using [`std::env::current_exe`], so the child's argv is
/// identity-preserving: under `shep runtime x` the child is
/// `shep runtime x --supervise`; under `shep-runtime x` it is
/// `shep-runtime x --supervise`, keeping the alias binary's own name.
///
/// # Errors
/// [`std::env::current_exe`] failed to resolve this binary's own path.
fn supervisor_command() -> std::io::Result<std::process::Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).arg("--supervise");
    Ok(cmd)
}

/// The async body of [`run_init`], factored out so the `std::process::exit`
/// call in `run_init` is the crate's only one — see `run_init`'s own doc
/// for why stepping around [`ExitCode`] is confined to that single site.
///
/// Nothing here may panic: no `unwrap`, no `expect`, no indexing. PID 1
/// taking the container down on a panic leaves no diagnostic; every error
/// is logged to stderr and the loop continues, except a failure to spawn
/// the supervisor at all (or to install a signal handler before it), which
/// ends the loop immediately with a failure status.
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

    // `child.wait()` is never called on the spawned handle — this loop is
    // the only thing in this process that waits on anything, and it does
    // so through `drain`'s own `waitpid(-1, …)`, never through `Child`. The
    // handle itself is dropped the moment its pid is read; `Child::drop`
    // neither kills nor waits, so nothing is lost by not holding it.
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

    /// Forwards `sig` to `pid` if `received` says a signal actually
    /// arrived — `tokio::signal::unix::Signal::recv` returning `None`
    /// means the stream itself closed, and reacting to that as if a
    /// signal arrived would forward nothing while busy-looping the arm.
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
/// every process the kernel reparents here.
///
/// **`std::process::Command`, never tokio's own async process type.** tokio
/// reaps its own children by calling `waitpid` on their pids when `SIGCHLD`
/// fires; a blind `waitpid(-1)` loop in the same process would race it and
/// consume statuses the supervisor needs — spec §4 promises exit code and
/// signal are recorded exactly. That race is the whole reason PID 1 is a
/// separate process here and not a loop inside the supervisor.
///
/// Forwarded signals go to the supervisor's pid, not its process group: the
/// supervisor owns its own flock's groups and runs its own stop ladder over
/// them.
///
/// It does not wait for orphans once the supervisor is gone — the drain
/// loop's last call already covers whatever was pending, and the container
/// is being torn down; a wedged orphan must not hold the exit open.
///
/// **This function never returns, and it calls `std::process::exit` at
/// exactly one site — the only one in this crate.** The status it reports
/// is the supervisor's — its own exit code, or `128 + signal` — and
/// [`ExitCode`] is a closed eleven-variant enum that cannot represent
/// either 3 or 137. Widening that funnel to an arbitrary byte for one call
/// site would undo decision 13's whole argument, and the status is not
/// shep's to classify anyway: it is a foreign status being relayed. The
/// cost of stepping around the funnel is destructors and buffer flushes,
/// and here that cost is zero: this process boots no shepherd, holds no
/// socket, owns no flock, installs no log subscriber, and deliberately
/// never calls `child.wait()`. Rust's stderr is unbuffered and its stdout
/// line-buffered, so its diagnostics are already out by the time this
/// returns.
pub async fn run_init() -> std::convert::Infallible {
    let status = init_loop().await;
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the supervisor is told apart from every orphan the wrong
    /// way — the whole reason `classify` takes the supervisor's own pid as
    /// an argument rather than assuming pid ordering.
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

    /// fails if a signalled supervisor reports 0, or reports the raw
    /// signal number — `docker inspect` reads 128+n and an orchestrator
    /// restarting on a non-zero status would see a clean exit for a
    /// SIGKILL.
    #[test]
    fn a_signalled_supervisor_exits_128_plus_the_signal() {
        assert_eq!(
            classify(WaitOutcome::Signaled { pid: 7, signal: 9 }, 7),
            Reaped::Supervisor(137)
        );
    }

    /// fails if the loop's other exit is not classified. `ECHILD` is how
    /// "nothing left to reap" arrives, and it arrives as an `Err`.
    #[test]
    fn no_children_and_nothing_ready_are_told_apart() {
        assert_eq!(classify(WaitOutcome::NoChildren, 7), Reaped::NoChildren);
        assert_eq!(classify(WaitOutcome::StillAlive, 7), Reaped::Nothing);
    }

    /// fails if the split fires anywhere but PID 1 (or under the test
    /// switch), which would mean every developer running `shep runtime` on
    /// a laptop gets two processes and a re-exec. The real call site
    /// passes `std::process::id()`, and that value is asserted here rather
    /// than mocked.
    #[test]
    fn the_init_split_does_not_fire_outside_pid_one() {
        assert_ne!(std::process::id(), 1, "a test harness is never PID 1");
        assert!(!should_split(std::process::id(), false, false));
    }

    /// fails if `--supervise` stops disabling the split, which is a fork
    /// bomb in two different ways — a mis-read pid, and a child that
    /// inherited `$SHEP_FORCE_INIT` from the init that spawned it.
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

    /// fails if `drain` leaves a reparented orphan a zombie instead of
    /// reaping it. Linux only, and lives here rather than in
    /// `tests/init.rs` because `drain` is private to this crate — an
    /// external integration test cannot reach it at all, given `lib.rs`'s
    /// three-function public surface (decision 1).
    /// `tests/init.rs::a_reparented_orphan_is_reaped` proves the OS-level
    /// subreaper mechanism itself works; this proves this crate's own
    /// `drain` actually uses it. Step 10.7's mutation 1
    /// (`waitpid(child_pid, …)` in place of `waitpid(-1, …)`) reddens
    /// this test and nothing else — without it, that mutation would be
    /// invisible to the whole suite.
    #[cfg(target_os = "linux")]
    #[test]
    fn drain_reaps_a_real_reparented_orphan() {
        use std::io::Read as _;

        nix::sys::prctl::set_child_subreaper(true).expect("Linux has supported this since 3.4");

        // A shell that backgrounds a short-lived grandchild, prints its
        // pid, and exits — reparenting the grandchild to this test
        // process, the same shape `tests/init.rs`'s own Linux case uses.
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

        // A pid this loop will never actually see exit — `drain` must
        // never confuse the orphan for it.
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
