//! Re-deriving an adopted sheep's start time from the operating system.
//!
//! [`Handover`](super::Handover) carries no `started_at`, and no serializer
//! for it would help: a [`tokio::time::Instant`] has no epoch and means
//! nothing outside the runtime that read it. The successor asks the process
//! table instead, which is authoritative about a pid it did not spawn in a
//! way a carried value could never be.
//!
//! Without this, a sheep that has been up three days reports `0s` in
//! `shep flock`'s UPTIME column the moment a handover completes. That is
//! the one column an operator reads to see whether anything moved, so a
//! zero there says the opposite of what the handover achieved.
//!
//! # What the operating system is asked for
//!
//! One number: the second the process started, since the Unix epoch. Both
//! platforms this module compiles for fill it while the process table row is
//! built, unconditionally, rather than under a [`ProcessRefreshKind`]
//! branch. macOS reads `pbi_start_tvsec` out of `proc_pidinfo`; Linux takes
//! field 22 of `/proc/<pid>/stat`, divides by the clock tick and adds the
//! boot time. `limits::sample`'s `.with_cpu()` tuning, whose whole point is
//! that a field IS gated on the refresh kind and silently reads zero
//! without it, therefore does not apply here.
//!
//! The answer is a wall-clock second, and the field being filled is a
//! monotonic [`Instant`], so the conversion goes through an age: how long
//! ago the process started, subtracted from now. That is the only bridge
//! between the two clocks, and it is why every step of it saturates.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::time::Instant;

/// The refresh kind [`start_epoch_secs`] asks sysinfo for.
///
/// Neither memory nor CPU: the start time is not gated on the refresh kind
/// on either platform (see this module's doc), so widening it would cost an
/// adoption the syscalls of the 15-second memory poll for figures nothing
/// here reads.
///
/// `.without_tasks()` for the reason `limits::sample` gives at length. Its
/// symptom cannot reach this module, which looks up exactly one pid and
/// reads no total, but on Linux it is still a `readdir` of
/// `/proc/<pid>/task/` and a row per thread, bought for nothing.
fn refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing().without_tasks()
}

/// This machine's wall clock, in seconds since the Unix epoch.
///
/// A clock set before the epoch reads as `0`, which makes every process look
/// older than the epoch and so gives an age of zero once
/// [`age_from_epoch`] has saturated. Same answer as every other unusable
/// clock reading below, reached the same way.
fn wall_clock_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs())
}

/// The second `pid` started at, as the operating system reports it, or
/// `None` if it will not say.
///
/// A [`System`] of this call's own, built here and dropped at the end of it,
/// rather than the one `SysinfoSampler` retains for the memory poll. The
/// reason is not the one that makes `identify` do the same thing (a name
/// read out of a retained table is frozen at the pid's first sighting) but
/// a plainer one: this runs during a rehydrate, before a supervisor exists,
/// and there is no retained table to borrow.
///
/// Only `pid` is refreshed, never the whole machine. Adoption asks this once
/// per carried sheep, and a whole-table walk per sheep would make the cost
/// of a handover quadratic in a quantity (the machine's process count) that
/// has nothing to do with the flock's size.
///
/// A reported `0` is read as unknown rather than as the epoch. It is
/// sysinfo's initial value for the field, and a process that genuinely
/// started at midnight on 1 January 1970 is not a case worth preferring
/// over a field that was never filled.
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
/// Saturating rather than asserting, and that is a deliberate answer rather
/// than defensive habit. Neither reading is under this daemon's control: a
/// wall clock moves backwards whenever NTP steps it, a VM resumes from a
/// snapshot, or an operator sets the date, and a process started before such
/// a step then reports a start time in the future. Nothing can recover the
/// true age from two readings of a clock that lied about one of them, so
/// there is no repair to attempt and no invariant to assert. Zero is the
/// honest answer and also the harmless one, since it is what an operator
/// already sees from a fresh spawn.
///
/// The alternatives are both worse than a wrong column. A panic would take a
/// supervisor down over a clock adjustment, in the one code path whose
/// entire purpose is that a flock survives; a wrapping subtraction would
/// report the sheep as having started some hundreds of billions of years
/// ago, which the UPTIME column would print in full.
fn age_from_epoch(started_secs: u64, now_secs: u64) -> Duration {
    Duration::from_secs(now_secs.saturating_sub(started_secs))
}

/// The `started_at` an adopted sheep running as `pid` should carry.
///
/// The process's age comes from the operating system and is subtracted from
/// this runtime's own clock, so a sheep that has been up three days is
/// adopted as three days old and `shep flock` reports its real uptime across
/// the handover.
///
/// Never fails, and deliberately: a pid the process table will not describe
/// falls back to [`Instant::now`], which reports an uptime restarting at the
/// handover. That is the case where the sheep exited between the
/// predecessor writing the blob and this image reading it, and one sheep's
/// uptime column is not worth refusing a whole rehydrate over.
#[must_use]
pub(crate) fn started_at_of(pid: u32) -> Instant {
    let now = Instant::now();
    let Some(started_secs) = start_epoch_secs(pid) else {
        return now;
    };
    let age = age_from_epoch(started_secs, wall_clock_secs());
    // Saturating a third time, for the third reason. This runtime's
    // monotonic clock counts from the machine's boot, so an age larger than
    // the machine has been up cannot be subtracted from it. That is not
    // reachable from an honest pair of readings, but it is reachable from a
    // wall clock that jumped forward since the process started, and the
    // answer is the same as everywhere above: report zero rather than
    // panic.
    now.checked_sub(age).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    // fails if the age is read off either reading alone rather than as the
    // gap between them
    #[test]
    fn an_age_is_the_gap_between_the_two_readings() {
        assert_eq!(age_from_epoch(100, 400), Duration::from_secs(300));
    }

    // fails if the subtraction is not saturating: a plain `-` panics in a
    // debug build and wraps to ~584 billion years in a release one, and
    // either is reachable from a clock that NTP stepped backwards under a
    // running sheep
    #[test]
    fn a_clock_that_moved_backwards_gives_an_age_of_zero() {
        assert_eq!(age_from_epoch(400, 100), Duration::ZERO);
    }

    // fails if a process observed in the same second it started reports
    // anything but zero
    #[test]
    fn a_process_that_started_this_second_has_no_age_yet() {
        assert_eq!(age_from_epoch(1_700_000_000, 1_700_000_000), Duration::ZERO);
    }

    // fails if a pid nothing is running under is treated as an error rather
    // than as an unknown start time. Pid 0 is the one number that is never a
    // process this daemon could adopt on either platform, so it cannot
    // collide with a real child the way a large made-up pid could.
    #[test]
    fn a_pid_the_table_does_not_know_reads_as_unknown() {
        assert_eq!(start_epoch_secs(0), None);
    }

    // fails if the refresh kind widens: the start time is filled whatever it
    // says, so memory or CPU here would buy an adoption the poll's syscalls
    // for nothing, and `tasks` a `/proc/<pid>/task/` walk on top
    #[test]
    fn the_refresh_kind_asks_for_nothing_the_start_time_does_not_need() {
        let kind = refresh_kind();
        assert!(!kind.memory(), "the start time is not a memory reading");
        assert!(!kind.cpu(), "the start time is not a CPU reading");
        assert!(!kind.tasks(), "nothing here reads a thread");
    }

    /// Tests that spawn a real process and wait on real elapsed time.
    ///
    /// The inner loop skips this module with `--skip ::slow::`; the full
    /// suite still runs them because nothing here is `#[ignore]`d.
    mod slow {
        use std::process::Command;

        use super::*;

        /// How long the child below is left alive before its start time is
        /// derived.
        ///
        /// Whole seconds because the operating system reports whole seconds:
        /// a sub-second interval could not be distinguished from zero by any
        /// implementation, correct or not.
        const ALIVE_FOR: Duration = Duration::from_secs(3);

        /// The lower bound the derived uptime must clear.
        ///
        /// One second under [`ALIVE_FOR`], and the missing second is
        /// truncation rather than slack: both epoch readings round down, so
        /// their difference can be a full second short of the real interval
        /// (a child started at `t = 100.0` and read at `t = 102.99` reports
        /// an age of 2). It can never be more than that short, and every
        /// other pressure on this number (a loaded runner, a slow spawn, a
        /// scheduler that oversleeps) makes the real interval LONGER, so
        /// the bound only gets easier to clear.
        const AT_LEAST: Duration = Duration::from_secs(2);

        /// The upper bound, generous by design.
        ///
        /// Nothing about a correct derivation comes close to it. It is here
        /// for the arithmetic that inverts: an implementation reading the
        /// epoch second as the age itself, or wrapping a backwards
        /// subtraction, lands tens of billions of seconds away rather than
        /// one or two, and the lower bound alone would call that a pass.
        const AT_MOST: Duration = Duration::from_secs(3_600);

        // fails if an adopted sheep's `started_at` is stamped at the moment
        // of the handover instead of derived from the operating system.
        // That is exactly what this path did before, and it reports a
        // three-day-old sheep as `0s` in `shep flock`'s UPTIME column the
        // instant a successor takes over.
        //
        // A real child and real elapsed time are the whole case: the claim
        // is about what the process table says, and there is no seam that
        // could fake a pid's start time without also faking the thing under
        // test. That is why this lives in `slow`, and why it needs a quiet
        // machine no more than any other case here does: the assertion is
        // a one-sided bound a full second below the interval, not a window.
        //
        // A plain `#[test]`, not a `#[tokio::test]`: this crate's tokio
        // tests run on a paused clock, where `Instant::now()` does not
        // advance across a real `thread::sleep` and both a correct
        // derivation and a stamped one would read the same.
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
