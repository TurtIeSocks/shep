//! Reaping the sheep a successor adopted, one pid at a time and never
//! wildcard.
//!
//! An adopted sheep has no `tokio::process::Child` and none can be built
//! from a bare pid, so this reaper runs alongside tokio's own in the same
//! process. `waitpid(-1, ..)` here would race tokio's reaper for a status
//! tokio needed, losing an exit code and a signal for good; zero and
//! negative pids are that wildcard wearing a different number, so they are
//! refused. A targeted wait steals nothing: nothing else in this process
//! holds a `Child` for an adopted pid, and an exec replaces the image rather
//! than the process, so those pids are still its children. Each wait arms
//! its own multiplexed `SIGCHLD` stream before its first look.

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
/// `&self`, so several waits can be in flight at once, one per sheep.
///
/// `Debug` is derived: pids and exit statuses only, no environment value.
#[derive(Debug, Default)]
pub struct AdoptedReaper {
    /// Statuses already taken from the kernel, by pid.
    ///
    /// A status can be collected once, so a second wait would meet `ECHILD`
    /// and lose it. Remembering replays the first answer instead, which is
    /// what `crate::runner::RunningProcess::wait` already promises.
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
    /// A dropped wait loses no status: one already taken is replayed.
    ///
    /// # Errors
    ///
    /// - [`io::ErrorKind::InvalidInput`] if `pid` is zero, which `waitpid`
    ///   reads as a group-wide wait, or does not fit in an `i32`.
    /// - [`io::ErrorKind::NotFound`] if the kernel has no such child and
    ///   this reaper never took its status, so the exit is gone.
    /// - Any other `waitpid` errno, or a failure to arm `SIGCHLD`, as-is.
    pub async fn wait(&self, pid: u32) -> io::Result<ExitOutcome> {
        let pid = targeted(pid)?;
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
    /// The lock spans the `waitpid` call, so two concurrent waits on one pid
    /// cannot have one take the status while the other sits between its own
    /// `ECHILD` and this map. Nothing awaits while it is held, and `WNOHANG`
    /// never blocks.
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
/// `Ok(None)` means the pid is still running. Reporting `EINTR` as that
/// would send the caller back to sleep on a `SIGCHLD` that may already have
/// been the last one.
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
/// A code and a signal are different fields and stay that way:
/// `crates/shep-cli/src/output/rows.rs`'s `exit_cell` renders one or the
/// other. `Stopped` and `Continued` need `WUNTRACED`/`WCONTINUED`, which
/// nothing here passes; they map to "not an exit" rather than to a panic.
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
    // An adopted sheep reaches a successor as a bare pid with no handle, so
    // the reaper collects the status a `Child::wait()` would take first.
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

    /// A child spawned the way an adopted sheep reaches a successor: through
    /// `std::process::Command`, so tokio holds no `Child` for it.
    fn adopted(script: &str) -> std::process::Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn a shell")
    }

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

    /// The kernel holds the zombie until somebody waits, so an
    /// implementation that awaits the signal before looking hangs here and
    /// the timeout turns that into a failure.
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
        // time to finish making it a zombie.
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

    /// The steal is deterministic rather than raced: tokio calls `waitpid`
    /// for a live `Child` only once its `wait()` is polled, so the
    /// supervised child's status sits pending in the kernel while the
    /// adopted sheep is still running. A `waitpid(-1, ..)` in this position
    /// takes that pending status every time.
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
            // Long enough to arm the SIGCHLD stream and take a first look
            // before anything else exits.
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut supervised = tokio::process::Command::new("/bin/sh")
                .args(["-c", "exit 3"])
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn a tokio-supervised child");
            // Nothing polls its `wait()`, so its status is pending when the
            // reaper's wakeup arrives.
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

    #[tokio::test]
    async fn pid_zero_is_refused_rather_than_becoming_a_group_wide_wait() {
        let reaper = AdoptedReaper::new();
        let error = reaper.wait(0).await.expect_err("pid 0 is refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

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
