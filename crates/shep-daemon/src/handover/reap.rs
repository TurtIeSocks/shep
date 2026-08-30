//! Reaping the sheep a successor adopted, one pid at a time and never
//! wildcard.
//!
//! An adopted sheep has no `tokio::process::Child` and there is no way to
//! build one: tokio produces a `Child` only from `Command::spawn`, and its
//! public API has no constructor from a bare pid. A successor therefore
//! cannot await an adopted sheep through tokio at all, so this is a second
//! reaping mechanism that runs permanently alongside tokio's own, in the
//! same process.
//!
//! # Why every wait here names a pid
//!
//! **`waitpid(-1, ..)` is forbidden in this process, and this repository
//! has already paid to learn it.** tokio reaps its own children by calling
//! `waitpid` on their exact pids when `SIGCHLD` fires. A blind wildcard
//! loop in the same process races that and sometimes wins, taking the
//! status tokio needed; tokio's own wait then answers `ECHILD` and a clean
//! exit reaches the supervisor as an `io::Error`. The record is in three
//! places: `crates/shep-cli/src/commands/reap.rs`, which is `shep runtime`'s
//! PID-1 init and says so in its own module doc; decision 14 in
//! `docs/decisions.md`; and `crates/shep-cli/tests/init.rs`, where CI
//! actually hit it. `crates/shep-daemon/src/tokio_runner.rs` is already
//! written to expect the loss and degrades to `{code: None, signal: None}`,
//! which means a stolen status is not merely an error, it is an exit code
//! and a signal number gone for good.
//!
//! That init loop's own wildcard wait is correct where it lives, because it
//! lives in a *separate process* from tokio's reaper. A successor cannot do
//! that: it is the same process. So the vocabulary here is borrowed from it
//! and the architecture deliberately is not.
//!
//! A targeted wait is safe for exactly the opposite reason. Nothing else in
//! this process holds a `Child` for an adopted pid, so nothing else will
//! ever wait on it, and taking its status steals nothing. The pid is still
//! a child of this process after the `execve`: an exec replaces the image,
//! not the process, so the parent/child relationships the predecessor had
//! are the ones the successor has.
//!
//! **Zero and negative pids are refused rather than passed through.**
//! `waitpid(0, ..)` waits on any child in this process group and
//! `waitpid(-n, ..)` on any child in group `n`. Both are the wildcard this
//! module exists to refuse, wearing a different number, and a pid that
//! arrived as a zero would otherwise turn a targeted wait into one silently.
//!
//! # How a wakeup arrives
//!
//! Each wait arms its own `SIGCHLD` stream through
//! `tokio::signal::unix::signal`, which multiplexes: every stream
//! registered for a kind is notified, so arming one here takes nothing away
//! from the stream tokio's own process driver arms for itself. The stream
//! is armed BEFORE the first look, so an exit that lands between the look
//! and the registration still wakes the loop instead of being lost. A pid
//! that had already exited needs no wakeup at all: the kernel holds the
//! zombie until somebody waits, so the first look reports the real status.

use std::collections::HashMap;
use std::io;
use std::sync::{Mutex, PoisonError};

use nix::errno::Errno;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal};

use crate::runner::ExitOutcome;

/// Waits on the pids a successor adopted, targeted and never wildcard.
///
/// One reaper serves a whole adopted flock; [`AdoptedReaper::wait`] takes
/// `&self` so several waits can be in flight at once, one per sheep.
///
/// `Debug` is derived: this holds pids and exit statuses, both of which an
/// operator already reads out of `shep flock`, and no environment value
/// ever reaches it.
#[derive(Debug, Default)]
pub struct AdoptedReaper {
    /// Statuses already taken from the kernel, by pid.
    ///
    /// A status can be collected exactly once, so a second wait on a pid
    /// already reaped would meet `ECHILD` and lose it. Remembering makes a
    /// repeated or concurrent wait replay the first answer instead, which
    /// is the contract `crate::runner::RunningProcess::wait` already states
    /// for tokio-supervised sheep.
    observed: Mutex<HashMap<i32, ExitOutcome>>,
}

impl AdoptedReaper {
    /// A reaper that has seen nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for one adopted pid and reports how it exited.
    ///
    /// Returns as soon as the pid has a status, whether it exited before
    /// this call or long after it. Cancel-safe in the way that matters: a
    /// dropped wait loses no status, because a status this reaper has taken
    /// is remembered and replayed to the next caller.
    ///
    /// # Errors
    ///
    /// - [`io::ErrorKind::InvalidInput`] if `pid` is zero or does not fit
    ///   in an `i32`. Zero is refused because `waitpid(0, ..)` is a
    ///   group-wide wait, not a targeted one.
    /// - [`io::ErrorKind::NotFound`] if the kernel has no such child and
    ///   this reaper never took its status, which means something else in
    ///   this process reaped it and the exit is gone.
    /// - Any other `waitpid` errno, and a failure to arm a `SIGCHLD`
    ///   stream, reported as-is.
    pub async fn wait(&self, pid: u32) -> io::Result<ExitOutcome> {
        let pid = targeted(pid)?;
        // Armed before the first look: an exit that lands in between still
        // wakes this loop, where one armed afterwards would sleep through
        // the only notification it was ever going to get.
        let mut sigchld = signal(SignalKind::child())?;
        loop {
            if let Some(outcome) = self.look(pid)? {
                return Ok(outcome);
            }
            if sigchld.recv().await.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("the SIGCHLD stream closed while waiting for adopted pid {pid}"),
                ));
            }
        }
    }

    /// One non-blocking look at `pid`, replaying a status already taken.
    ///
    /// The lock spans the `waitpid` call deliberately, so two concurrent
    /// waits on the same pid cannot have one take the status while the
    /// other is between its own `ECHILD` and this map. Nothing awaits while
    /// it is held and `WNOHANG` never blocks.
    fn look(&self, pid: i32) -> io::Result<Option<ExitOutcome>> {
        let mut observed = self.observed.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(outcome) = observed.get(&pid) {
            return Ok(Some(*outcome));
        }
        match wait_once(pid) {
            Ok(Some(outcome)) => {
                observed.insert(pid, outcome);
                Ok(Some(outcome))
            }
            Ok(None) => Ok(None),
            Err(Errno::ECHILD) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "adopted pid {pid} is not a child of this process, so its exit status is lost"
                ),
            )),
            Err(errno) => Err(io::Error::from_raw_os_error(errno as i32)),
        }
    }
}

/// Checks that `pid` names one process and converts it for `waitpid`.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] for zero, which `waitpid` reads as this
/// process's whole group, and for anything too large to be an `i32`.
fn targeted(pid: u32) -> io::Result<i32> {
    let pid = i32::try_from(pid).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("pid {pid} is too large to wait on"),
        )
    })?;
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pid 0 is a group-wide wait, not an adopted sheep",
        ));
    }
    Ok(pid)
}

/// One `waitpid(pid, WNOHANG)`, retried past `EINTR`.
///
/// `Ok(None)` means the pid is still running. `EINTR` is retried rather
/// than reported, because reporting it as "still running" would send the
/// caller back to sleep on a `SIGCHLD` that may already have been the last
/// one it was going to get.
fn wait_once(pid: i32) -> Result<Option<ExitOutcome>, Errno> {
    loop {
        match waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)) {
            Ok(status) => return Ok(outcome_of(status)),
            Err(Errno::EINTR) => continue,
            Err(errno) => return Err(errno),
        }
    }
}

/// Maps one `waitpid` return into the daemon's own [`ExitOutcome`].
///
/// Pure and `cfg`-free, so the one thing that must never blur can be
/// asserted without a child: a code and a signal are different fields and
/// stay that way. `crates/shep-cli/src/output/rows.rs`'s `exit_cell`
/// renders one or the other, so collapsing them makes the EXIT column lie.
///
/// `Stopped` and `Continued` need `WUNTRACED`/`WCONTINUED`, which nothing
/// here passes, so they cannot arrive in practice. They map to "not an
/// exit" rather than to a panic, so a future flag addition keeps a
/// supervising daemon alive.
fn outcome_of(status: WaitStatus) -> Option<ExitOutcome> {
    match status {
        WaitStatus::Exited(_, code) => Some(ExitOutcome {
            code: Some(code),
            signal: None,
        }),
        WaitStatus::Signaled(_, sig, _core_dumped) => Some(ExitOutcome {
            code: None,
            signal: Some(sig as i32),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // Every child here is deliberately never `Child::wait()`ed on: that is
    // the whole shape under test. An adopted sheep reaches a successor as a
    // bare pid with no handle to wait through, so the reaper is what
    // collects its status, and a `wait()` added to satisfy this lint would
    // take the status the assertions are about.
    #![expect(
        clippy::zombie_processes,
        reason = "the reaper collects these statuses; a Child::wait would take them first"
    )]

    use core::time::Duration;
    use std::io::Read as _;
    use std::process::{Command, Stdio};

    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    use super::{AdoptedReaper, outcome_of};
    use crate::runner::ExitOutcome;

    /// A child spawned the way an adopted sheep reaches a successor: by
    /// `std::process::Command`, so tokio holds no `Child` for it and
    /// nothing but this module will ever wait on it.
    fn adopted(script: &str) -> std::process::Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn a shell")
    }

    /// fails if an adopted pid's real exit code does not reach the caller.
    #[tokio::test]
    async fn an_adopted_pid_yields_its_real_exit_code() {
        let child = adopted("exit 7");
        let reaper = AdoptedReaper::new();
        let outcome = tokio::time::timeout(Duration::from_secs(10), reaper.wait(child.id()))
            .await
            .expect("the reaper answered within the budget")
            .expect("the adopted pid was reapable");
        assert_eq!(
            outcome,
            ExitOutcome {
                code: Some(7),
                signal: None
            }
        );
    }

    /// fails if a signalled adopted pid reports a code instead of a signal.
    #[tokio::test]
    async fn an_adopted_pid_killed_by_a_signal_reports_the_signal() {
        let child = adopted("sleep 30");
        let pid = child.id();
        signal::kill(
            Pid::from_raw(i32::try_from(pid).expect("a pid fits in an i32")),
            Signal::SIGKILL,
        )
        .expect("kill the adopted child");
        let reaper = AdoptedReaper::new();
        let outcome = tokio::time::timeout(Duration::from_secs(10), reaper.wait(pid))
            .await
            .expect("the reaper answered within the budget")
            .expect("the adopted pid was reapable");
        assert_eq!(
            outcome,
            ExitOutcome {
                code: None,
                signal: Some(9)
            }
        );
    }

    /// fails if the reaper waits for a `SIGCHLD` that already came and
    /// went. The kernel holds the zombie until somebody waits, so a pid
    /// that exited before the reaper's first look still has a real status
    /// to report; an implementation that awaits the signal before looking
    /// hangs here forever and the timeout turns that into a failure.
    #[tokio::test]
    async fn a_pid_that_exited_before_the_first_look_still_yields_its_status() {
        let mut child = adopted("echo bye; exit 5");
        let pid = child.id();
        let mut said = String::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_string(&mut said)
            .expect("read to EOF, which the child's own exit closes");
        assert_eq!(said.trim(), "bye");
        // EOF means the child is in its exit path; this gives the kernel
        // time to finish making it a zombie, so the reaper's first look is
        // genuinely after the exit rather than racing it.
        std::thread::sleep(Duration::from_millis(50));

        let reaper = AdoptedReaper::new();
        let outcome = tokio::time::timeout(Duration::from_secs(10), reaper.wait(pid))
            .await
            .expect("the reaper answered within the budget")
            .expect("the adopted pid was reapable");
        assert_eq!(
            outcome,
            ExitOutcome {
                code: Some(5),
                signal: None
            }
        );
    }

    /// fails if the reaper ever waits wildcard. The regression this file
    /// exists to prevent: a tokio-spawned child's status is pending at the
    /// moment the reaper looks, and both processes must still report their
    /// own real exit rather than one of them meeting an `io::Error`.
    ///
    /// The steal is made deterministic rather than left to a race. tokio
    /// does not call `waitpid` for a live `Child` until its `wait()` is
    /// polled, so a child that exits while nothing is awaiting it leaves
    /// its status sitting in the kernel; the adopted sheep is meanwhile
    /// still running, so the `SIGCHLD` that wakes the reaper is that other
    /// child's. A `waitpid(-1, ..)` in this position takes the pending
    /// status every time. Confirmed by implementing the wildcard and
    /// watching this test redden on its own.
    #[tokio::test]
    async fn reaping_an_adopted_pid_does_not_disturb_a_tokio_spawned_child() {
        let reaper = std::sync::Arc::new(AdoptedReaper::new());
        for round in 0..3 {
            // Exits only when its stdin closes, so it is alive across the
            // whole window below.
            let mut carried = adopted("read line; exit 4");
            let carried_pid = carried.id();
            let waiting = {
                let reaper = std::sync::Arc::clone(&reaper);
                tokio::spawn(async move { reaper.wait(carried_pid).await })
            };
            // Long enough for that wait to arm its SIGCHLD stream and take
            // its first look before anything else exits.
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut supervised = tokio::process::Command::new("/bin/sh")
                .args(["-c", "exit 3"])
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn a tokio-supervised child");
            // It exits here, with nothing polling its `wait()`, so its
            // status is pending when the reaper's wakeup arrives.
            tokio::time::sleep(Duration::from_millis(200)).await;

            drop(carried.stdin.take());
            let carried_exit = tokio::time::timeout(Duration::from_secs(10), waiting)
                .await
                .expect("the reaper answered within the budget")
                .expect("the waiting task did not panic")
                .unwrap_or_else(|error| panic!("round {round}: adopted pid unreapable: {error}"));
            assert_eq!(
                carried_exit,
                ExitOutcome {
                    code: Some(4),
                    signal: None
                },
                "round {round}: the adopted pid reported somebody else's status"
            );

            let supervised_exit = tokio::time::timeout(Duration::from_secs(10), supervised.wait())
                .await
                .expect("tokio answered within the budget")
                .unwrap_or_else(|error| {
                    panic!("round {round}: tokio's own status was stolen: {error}")
                });
            assert_eq!(
                supervised_exit.code(),
                Some(3),
                "round {round}: tokio's child reported somebody else's status"
            );
        }
    }

    /// fails if a second wait on an already-reaped pid answers `ECHILD`
    /// instead of replaying what the first one saw. A supervisor may ask
    /// twice, and the second answer must not be an error.
    #[tokio::test]
    async fn a_second_wait_replays_the_status_the_first_one_saw() {
        let child = adopted("exit 6");
        let reaper = AdoptedReaper::new();
        let first = tokio::time::timeout(Duration::from_secs(10), reaper.wait(child.id()))
            .await
            .expect("the reaper answered within the budget")
            .expect("the adopted pid was reapable");
        let second = tokio::time::timeout(Duration::from_secs(10), reaper.wait(child.id()))
            .await
            .expect("the replay answered within the budget")
            .expect("the replay is not an error");
        assert_eq!(first, second);
    }

    /// fails if pid 0 ever reaches `waitpid`. `waitpid(0, ..)` is not a
    /// targeted wait at all: it waits on any child in this process group,
    /// which is the wildcard this module exists to refuse, wearing a
    /// different number.
    #[tokio::test]
    async fn pid_zero_is_refused_rather_than_becoming_a_group_wide_wait() {
        let reaper = AdoptedReaper::new();
        let error = reaper.wait(0).await.expect_err("pid 0 is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// fails if an exit code and a signal are ever collapsed into one
    /// another. `crates/shep-cli/src/output/rows.rs`'s `exit_cell` renders
    /// one or the other, so a reaper that mixed them up would make the
    /// EXIT column lie.
    #[test]
    fn a_code_and_a_signal_are_never_collapsed_into_one_another() {
        assert_eq!(
            outcome_of(nix::sys::wait::WaitStatus::Exited(Pid::from_raw(7), 3)),
            Some(ExitOutcome {
                code: Some(3),
                signal: None
            })
        );
        assert_eq!(
            outcome_of(nix::sys::wait::WaitStatus::Signaled(
                Pid::from_raw(7),
                Signal::SIGTERM,
                false
            )),
            Some(ExitOutcome {
                code: None,
                signal: Some(15)
            })
        );
        assert_eq!(
            outcome_of(nix::sys::wait::WaitStatus::StillAlive),
            None,
            "still alive is not an exit"
        );
    }
}
