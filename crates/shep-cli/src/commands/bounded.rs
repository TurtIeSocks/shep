//! Running a child process under a deadline
//!
//! [`run_bounded`] is [`std::process::Command::output`] with a deadline, for
//! the `.js` Flockfile bridge: a config module that leaves a server or timer
//! running keeps node's event loop alive after `require` returned.
//!
//! It takes no stdin and offers no cancellation. Both streams drain on their
//! own threads, since a pipe holds 64 KiB before a write blocks and a child
//! blocked writing never exits.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How often [`run_bounded`] asks whether the child has exited.
///
/// The budgets this runs under are seconds long, so 10ms costs a few hundred
/// wakeups and adds at most one interval to an honest run.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// What a bounded run came back with.
#[derive(Debug)]
pub(crate) enum Bounded {
    /// The child exited on its own inside the budget, and both its streams
    /// arrived.
    Exited(Output),
    /// The child was still running at the deadline, and was killed.
    Killed,
    /// The child exited on its own, but something it left behind still holds
    /// a captured pipe, so its output never arrived inside the budget.
    /// Nothing was killed: whatever holds the pipe is not shep's child.
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
            // The receiver is already gone whenever the run timed out, which
            // is an ordinary end for this thread.
            let _ = sender.send(read);
        });
        Self(receiver)
    }

    /// The stream's bytes, or `None` if they did not arrive inside `budget`.
    ///
    /// A reader thread that died without sending also answers `None`, folding
    /// a panic into the timeout verdict.
    fn collect_within(&self, budget: Duration) -> Option<std::io::Result<Vec<u8>>> {
        self.0.recv_timeout(budget).ok()
    }
}

/// Runs `command` to completion, killing it once it outlives `budget`.
///
/// Takes both output pipes for itself, so the caller must not set stdout or
/// stderr. stdin is left as it was.
///
/// The budget covers the reads as well as the wait: a grandchild can hold the
/// inherited pipe open after the child exits, and calling that
/// [`Bounded::Killed`] would claim a kill that never happened.
///
/// # Errors
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

    // `try_wait` first, deadline second: a child that exited in the last
    // interval has already done its work, and killing it over the microseconds
    // since would throw away output shep asked for.
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            child.kill()?;
            // Reaped here rather than left to the `Child` drop, which does
            // not wait: a killed child nobody waits for is a zombie.
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

    /// `sleep 5 &` in a shell that exits straight after leaves a process
    /// holding the inherited pipes, so the wait ends at once and only the
    /// reads run out of budget. 2s is long enough that a slow `sh` start is
    /// not a kill, short enough to stay inside the backgrounded `sleep 5`.
    #[test]
    fn a_child_whose_output_outlives_it_is_not_reported_as_killed() {
        let started = Instant::now();
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

    /// 200_000 bytes is past the 64 KiB a pipe holds, so a child writing this
    /// much blocks against a reader that has not started.
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
