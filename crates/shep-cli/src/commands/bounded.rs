//! Running a child process under a deadline
//!
//! [`std::process::Command::output`] waits for as long as the child cares to
//! run, which is the right default nearly everywhere in this crate: the other
//! commands shep spawns are probes and renderers that exit on their own. The
//! one place it is wrong is the `.js` Flockfile bridge, where the child is
//! running a file the operator wrote. A config module that starts a server at
//! require time never returns, and `output` would wait for it forever.
//!
//! [`run_bounded`] is `output` with a deadline: the same captured stdout and
//! stderr, plus a [`Bounded::TimedOut`] answer once the child outlives its
//! budget. It is deliberately not general — no stdin, no cancellation, and no
//! partial output on the timeout path — because its one caller needs none of
//! that, and each of them is a decision better made by whoever turns up
//! needing it.
//!
//! ## Why both streams are drained on threads
//!
//! A pipe holds 64 KiB before a write blocks, and a child blocked writing is
//! a child that never exits. A deadline that polled the child first and read
//! the pipes afterwards would therefore be a deadline that cannot fire on the
//! very children it exists for: the loud ones. One thread per stream keeps
//! both draining for the whole run, which is what `Command::output` does
//! internally for the same reason.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How often [`run_bounded`] asks whether the child has exited.
///
/// 10ms. The budgets this runs under are seconds long, so the poll costs a
/// few hundred wakeups against a child that is already misbehaving, and the
/// worst it can add to an honest run is one interval.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// What a bounded run came back with.
///
/// Not an `Option<Output>`: the timeout is the answer this type exists to
/// carry, and a bare `None` at the call site says nothing about which of the
/// two happened.
#[derive(Debug)]
pub(crate) enum Bounded {
    /// The child exited on its own, inside the budget.
    Exited(Output),
    /// The child outlived the budget and was killed.
    TimedOut,
}

/// One of a child's output streams, read to the end on its own thread.
struct Draining(Receiver<std::io::Result<Vec<u8>>>);

impl Draining {
    /// Starts draining `source`, returning the handle its bytes arrive on.
    fn of<R: Read + Send + 'static>(mut source: R) -> Self {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let read = source.read_to_end(&mut buffer).map(|_count| buffer);
            // The receiver is already gone whenever the run timed out and
            // stopped caring, which is an ordinary end for this thread and
            // not a failure to report anywhere.
            let _ = sender.send(read);
        });
        Self(receiver)
    }

    /// The stream's bytes, or `None` if they did not arrive inside `budget`.
    ///
    /// A reader thread that died without sending also answers `None`. That
    /// folds a panic into the timeout verdict, which is honest enough for a
    /// thread whose whole body is one `read_to_end`.
    fn collect_within(&self, budget: Duration) -> Option<std::io::Result<Vec<u8>>> {
        self.0.recv_timeout(budget).ok()
    }
}

/// Runs `command` to completion, killing it once it outlives `budget`.
///
/// stdout and stderr are captured, so the caller must not set either — this
/// takes both pipes for itself. stdin is the caller's to decide and is left
/// exactly as it was.
///
/// The budget covers the reads as well as the wait. A child can exit while a
/// grandchild it spawned holds the inherited pipe open, and a collection that
/// blocked there would undo the whole point of the deadline.
///
/// # Errors
///
/// - The child could not be spawned, killed, or waited for.
/// - A stream could not be read.
pub(crate) fn run_bounded(command: &mut Command, budget: Duration) -> std::io::Result<Bounded> {
    let deadline = Instant::now() + budget;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = Draining::of(
        child
            .stdout
            .take()
            .expect("stdout is piped on the line that spawned this child"),
    );
    let stderr = Draining::of(
        child
            .stderr
            .take()
            .expect("stderr is piped on the line that spawned this child"),
    );

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            // Reaped here rather than left to the `Child` drop, which does
            // not wait: a killed child nobody waits for is a zombie for as
            // long as shep runs.
            child.wait()?;
            return Ok(Bounded::TimedOut);
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    let Some(stdout) = stdout.collect_within(deadline.saturating_duration_since(Instant::now()))
    else {
        return Ok(Bounded::TimedOut);
    };
    let Some(stderr) = stderr.collect_within(deadline.saturating_duration_since(Instant::now()))
    else {
        return Ok(Bounded::TimedOut);
    };
    Ok(Bounded::Exited(Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the budget is measured from the wrong end, or if a child
    /// that ignores it is waited on anyway.
    #[test]
    fn a_child_that_outlives_its_budget_is_killed_rather_than_waited_for() {
        let started = Instant::now();
        let outcome = run_bounded(Command::new("sleep").arg("30"), Duration::from_millis(100))
            .expect("the child spawns, and killing it is the only other syscall");

        assert!(
            matches!(outcome, Bounded::TimedOut),
            "a 30s sleep cannot finish inside 100ms: {outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the sleep was waited out rather than killed, in {:?}",
            started.elapsed()
        );
    }

    /// fails if the budget is enforced against a child that met it, or if
    /// its output is dropped on the way back.
    #[test]
    fn a_child_that_finishes_comes_back_with_both_streams() {
        let outcome = run_bounded(
            Command::new("sh")
                .arg("-c")
                .arg("printf wool; printf bleat >&2"),
            Duration::from_secs(30),
        )
        .expect("sh is on every host this crate compiles for");

        let Bounded::Exited(output) = outcome else {
            panic!("a printf finishes well inside 30s: {outcome:?}");
        };
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "wool");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "bleat");
    }

    /// fails if either stream is read after the wait rather than during it.
    /// 200_000 bytes is past the 64 KiB a pipe holds, so a child writing
    /// this much blocks against a reader that has not started, and the
    /// budget then expires on a child that had already done its work.
    #[test]
    fn a_child_louder_than_one_pipeful_is_not_deadlocked_against_its_own_budget() {
        let outcome = run_bounded(
            Command::new("sh")
                .arg("-c")
                .arg("head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2"),
            Duration::from_secs(30),
        )
        .expect("sh is on every host this crate compiles for");

        let Bounded::Exited(output) = outcome else {
            panic!("the writes finish in milliseconds against a draining reader: {outcome:?}");
        };
        assert_eq!(output.stdout.len(), 200_000);
        assert_eq!(output.stderr.len(), 200_000);
    }
}
