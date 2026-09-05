//! Re-deriving an adopted sheep's start time from the operating system.
//!
//! [`Handover`](super::Handover) carries no `started_at`: a
//! [`tokio::time::Instant`] has no epoch and means nothing outside the
//! runtime that read it, so the successor asks the process table for the
//! second the pid started. Both platforms fill that field whatever refresh
//! kind is asked for. The answer is a wall-clock second and the field being
//! filled is monotonic, so the conversion goes through an age, and every
//! step of it saturates.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::time::Instant;

/// The refresh kind [`start_epoch_secs`] asks sysinfo for.
///
/// Nothing: the start time is filled whatever this says, so memory or CPU
/// would buy an adoption the memory poll's syscalls, and tasks a
/// `/proc/<pid>/task/` walk, for figures nothing here reads.
fn refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().without_tasks()
}

/// This machine's wall clock, in seconds since the Unix epoch.
///
/// A clock set before the epoch reads as `0`, so every process looks older
/// than the epoch and [`age_from_epoch`] saturates to zero.
fn wall_clock_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs())
}

/// The second `pid` started at, as the operating system reports it, or
/// `None` if it will not say.
///
/// Its own [`System`]: this runs during a rehydrate, before a supervisor
/// exists to borrow a retained table from. Only `pid` is refreshed, since a
/// whole-table walk per carried sheep would scale a handover's cost with the
/// machine's process count. A reported `0` is sysinfo's unfilled value, so it
/// reads as unknown rather than as the epoch.
fn start_epoch_secs(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh_kind());
    let started = system.process(pid)?.start_time();
    (started != 0).then_some(started)
}

/// The age of a process that started at `started_secs`, seen from a wall
/// clock reading `now_secs`, both in seconds since the Unix epoch.
///
/// Saturating: an NTP step, a resumed VM snapshot or a hand-set date moves
/// the wall clock backwards under a running process, and no pair of readings
/// can recover the true age once one of them lied. Zero is what an operator
/// already sees from a fresh spawn; a wrapping subtraction would print an age
/// of hundreds of billions of years.
fn age_from_epoch(started_secs: u64, now_secs: u64) -> Duration {
    Duration::from_secs(now_secs.saturating_sub(started_secs))
}

/// The `started_at` an adopted sheep running as `pid` should carry.
///
/// The age comes from the operating system and is subtracted from this
/// runtime's clock, so a sheep up three days is adopted as three days old. A
/// pid the process table will not describe falls back to [`Instant::now`]:
/// that sheep exited between the blob being written and read, and one uptime
/// column is not worth refusing a rehydrate over.
#[must_use]
pub(crate) fn started_at_of(pid: u32) -> Instant {
    let now = Instant::now();
    let Some(started_secs) = start_epoch_secs(pid) else {
        return now;
    };
    let age = age_from_epoch(started_secs, wall_clock_secs());
    // The monotonic clock counts from boot, so an age larger than the
    // machine's uptime cannot be subtracted from it. Reachable from a wall
    // clock that jumped forward since the process started.
    now.checked_sub(age).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_age_is_the_gap_between_the_two_readings() {
        assert_eq!(age_from_epoch(100, 400), Duration::from_secs(300));
    }

    #[test]
    fn a_clock_that_moved_backwards_gives_an_age_of_zero() {
        assert_eq!(age_from_epoch(400, 100), Duration::ZERO);
    }

    #[test]
    fn a_process_that_started_this_second_has_no_age_yet() {
        assert_eq!(age_from_epoch(1_700_000_000, 1_700_000_000), Duration::ZERO);
    }

    // Pid 0 is the one number that is never a process this daemon could
    // adopt, so it cannot collide with a real child.
    #[test]
    fn a_pid_the_table_does_not_know_reads_as_unknown() {
        assert_eq!(start_epoch_secs(0), None);
    }

    #[test]
    fn the_refresh_kind_asks_for_nothing_the_start_time_does_not_need() {
        let kind = refresh_kind();
        assert!(!kind.memory(), "the start time is not a memory reading");
        assert!(!kind.cpu(), "the start time is not a CPU reading");
        assert!(!kind.tasks(), "nothing here reads a thread");
    }

    /// Tests that spawn a real process and wait on real elapsed time.
    mod slow {
        use std::process::Command;

        use super::*;

        /// How long the child below is left alive before its start time is
        /// derived.
        ///
        /// Whole seconds, because the operating system reports whole
        /// seconds.
        const ALIVE_FOR: Duration = Duration::from_secs(3);

        /// The lower bound the derived uptime must clear.
        ///
        /// One second under [`ALIVE_FOR`], which is truncation rather than
        /// slack: both epoch readings round down, so their difference can be
        /// a full second short of the real interval and never more. Every
        /// other pressure on it lengthens the interval.
        const AT_LEAST: Duration = Duration::from_secs(2);

        /// The upper bound, for the arithmetic that inverts.
        ///
        /// Reading the epoch second as the age, or wrapping a backwards
        /// subtraction, lands tens of billions of seconds away; the lower
        /// bound alone would call that a pass.
        const AT_MOST: Duration = Duration::from_secs(3_600);

        // A plain `#[test]`, not a `#[tokio::test]`: this crate's tokio
        // tests run on a paused clock, where `Instant::now()` does not
        // advance across a real `thread::sleep` and a stamped `started_at`
        // would read the same as a derived one.
        #[test]
        fn an_adopted_child_that_has_been_alive_for_seconds_is_that_old() {
            let mut child = Command::new("/bin/sh")
                .arg("-c")
                .arg("sleep 30")
                .spawn()
                .expect("/bin/sh is present on every platform this module compiles for");
            let pid = child.id();

            std::thread::sleep(ALIVE_FOR);
            let derived = started_at_of(pid);
            let uptime = Instant::now().saturating_duration_since(derived);

            let _ = child.kill();
            let _ = child.wait();

            assert!(
                uptime >= AT_LEAST,
                "a child alive for {ALIVE_FOR:?} derived an uptime of {uptime:?}; \
                 a `started_at` stamped at the handover reads as ~0 here"
            );
            assert!(
                uptime <= AT_MOST,
                "a child alive for {ALIVE_FOR:?} derived an uptime of {uptime:?}, \
                 which is not an age but an arithmetic error"
            );
        }
    }
}
