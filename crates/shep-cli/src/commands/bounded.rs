//! Running a child process under a deadline
//!
//! [`std::process::Command::output`] waits for as long as the child cares to
//! run, which is the right default nearly everywhere in this crate: the other
//! commands shep spawns are probes and renderers that exit on their own. The
//! one place it is wrong is the `.js` Flockfile bridge, where the child is
//! running a file the operator wrote. A config module that leaves a server or
//! a timer running keeps node's event loop alive after `require` returned, so
//! node never exits and `output` waits for it forever.
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
/// Not an `Option<Output>`: which way the budget ran out is the answer this
/// type exists to carry, and a bare `None` at the call site says nothing
/// about it. The two failures need different sentences, because only one of
/// them involves shep killing anything.
#[derive(Debug)]
pub(crate) enum Bounded {
    /// The child exited on its own inside the budget, and both its streams
    /// arrived.
    Exited(Output),
    /// The child was still running at the deadline, and was killed.
    Killed,
    /// The child exited on its own, but something it left behind still holds
    /// a captured pipe, so its output never arrived inside the budget.
    /// Nothing was killed: whatever is holding the pipe is not shep's child
    /// and shep has no handle on it.
    OutputHeldOpen,
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
/// The budget covers the reads as well as the wait, and the two ends are
/// reported apart. A child can exit while a grandchild it spawned holds the
/// inherited pipe open: a collection that blocked there would undo the whole
/// point of the deadline, and calling it [`Bounded::Killed`] would claim a
/// kill that never happened.
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

    // `try_wait` first, deadline second. A child that exited in the last
    // interval has already done its work, and killing it over the microseconds
    // between its exit and this wakeup would throw away output shep asked for.
    // Sleeping only as far as the deadline is what keeps that window down to
    // one syscall rather than one poll interval.
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            child.kill()?;
            // Reaped here rather than left to the `Child` drop, which does
            // not wait: a killed child nobody waits for is a zombie for as
            // long as shep runs.
            child.wait()?;
            return Ok(Bounded::Killed);
        }
        std::thread::sleep(POLL_INTERVAL.min(left));
    };

    let Some(stdout) = stdout.collect_within(deadline.saturating_duration_since(Instant::now()))
    else {
        return Ok(Bounded::OutputHeldOpen);
    };
    let Some(stderr) = stderr.collect_within(deadline.saturating_duration_since(Instant::now()))
    else {
        return Ok(Bounded::OutputHeldOpen);
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
            matches!(outcome, Bounded::Killed),
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

    /// fails if a child that exited is reported as killed. `sleep 5 &` in a
    /// shell that exits straight after leaves a process holding the pipes it
    /// inherited, so the wait ends at once and only the reads run out of
    /// budget. Nothing is killed on that path, and the two answers exist so
    /// the caller does not have to claim otherwise.
    ///
    /// **The budget is the one number here that can be wrong in both
    /// directions**, which is why it is 2s rather than the 200ms it started
    /// at. It has to outlast the shell's own startup, or the shell is still
    /// running at the deadline and the answer is `Killed` for a reason that
    /// has nothing to do with the case. It has to stay well under the
    /// backgrounded `sleep 5`, or the reads finish and the answer is
    /// `Exited`. 200ms cleared the first bar on unix and did not clear it on
    /// a loaded `windows-latest` runner, where `sh` is Git Bash: green on
    /// three runs, then `Killed` on the fourth. 2s is roughly ten times a
    /// normal start and still less than half the sleep.
    ///
    /// The sibling case above needs no such widening: it asserts `Killed`,
    /// and a slow start only makes that more certain.
    #[test]
    fn a_child_whose_output_outlives_it_is_not_reported_as_killed() {
        let started = Instant::now();
        // Two seconds, not two hundred milliseconds, and the difference is
        // the whole reason this test was intermittent. The budget bounds BOTH
        // halves of `run_bounded`: the wait for the child AND the reads after
        // it. At 200ms a runner slow enough that `sh` had not yet forked its
        // `sleep` and exited took the `left.is_zero()` branch, killed the
        // child, and returned `Killed` -- the one answer this test exists to
        // rule out. Failed twice on musl on 2026-08-28 while every other
        // platform passed.
        //
        // Two seconds is still far inside the `sleep 5` that holds the pipes,
        // so the outcome under test is unchanged and the elapsed assertion
        // below still means something. It just stops sharing a budget with
        // process startup on a contended box.
        let outcome = run_bounded(
            Command::new("sh").arg("-c").arg("sleep 5 & exit 0"),
            Duration::from_secs(2),
        )
        .expect("sh is on every host this crate compiles for");

        assert!(
            matches!(outcome, Bounded::OutputHeldOpen),
            "the shell exits at once and the backgrounded sleep holds both \
             pipes: {outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget did not bound the reads, in {:?}",
            started.elapsed()
        );
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
