//! The empty-flock watcher: turns a poll of the flock into a [`Sample`], and
//! debounces a run of empty samples before `runtime`'s foreground engine
//! (Task 9) decides the flock is gone rather than mid-recovery.
//!
//! Read decision 13 in this phase's plan before touching this file — the
//! debounce and the clean/failed split here are what decide between
//! [`crate::exit::ExitCode::Success`] and [`crate::exit::ExitCode::FlockEmpty`],
//! which is a resolution of a collision spec §9 named and left open.
//!
//! Not called anywhere yet outside this module's own tests: `runtime`'s
//! foreground engine, the only caller, is still unwritten (Task 9).
//! `#[allow(dead_code)]` on every item below says so explicitly rather than
//! inventing a call site nothing needs yet — same shape as
//! [`crate::exit::ExitCode::FlockEmpty`] itself.

use std::future::Future;
use std::time::Duration;

use shep_core::protocol::ProcessInfo;
use shep_core::status::ProcStatus;

/// What one poll of the flock says about whether the foreground engine should
/// still be running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Sample {
    /// At least one sheep is online, starting, stopping, or waiting to
    /// restart.
    Busy,
    /// Nothing is running and nothing failed.
    EmptyClean,
    /// Nothing is running and at least one sheep is `errored`.
    EmptyFailed,
}

/// Reads one `ListFlock` answer into a [`Sample`].
///
/// **Dogs do not count.** A `ProcessInfo` with `dog: Some(_)` is a metrics or
/// bark process the shepherd started for itself; a flock whose only remaining
/// entry is the metrics dog is an empty flock, and counting the dog would keep
/// a container alive forever after its app died. This is the one line of this
/// function that is easy to leave out and impossible to notice without a test.
///
/// `Stopping` counts as busy alongside `Starting`/`Online`/`WaitingRestart`:
/// it is reachable from exactly one path — a reload's outgoing instance,
/// see `ProcStatus::Stopping`'s own doc — but that instance is still alive on
/// the OS until it exits, and a reload in progress must not read as the
/// flock having gone away.
#[must_use]
#[allow(dead_code)]
pub fn sample(flock: &[ProcessInfo]) -> Sample {
    let mut any_errored = false;
    for info in flock {
        if info.dog.is_some() {
            continue;
        }
        match info.status {
            ProcStatus::Starting
            | ProcStatus::Online
            | ProcStatus::Stopping
            | ProcStatus::WaitingRestart => return Sample::Busy,
            ProcStatus::Errored => any_errored = true,
            ProcStatus::Stopped => {}
        }
    }
    if any_errored {
        Sample::EmptyFailed
    } else {
        Sample::EmptyClean
    }
}

/// Three consecutive empty samples, two seconds apart, before the engine
/// gives up — map.md's recorded contract, and not ceremony: a single sample
/// catches the gap between a sheep exiting and its backoff restart, and a
/// container torn down in that gap is a container torn down mid-recovery.
#[allow(dead_code)]
pub const STRIKES: u8 = 3;

/// The interval between polls while debouncing an emptying flock.
#[allow(dead_code)]
pub const INTERVAL: Duration = Duration::from_secs(2);

/// Polls `source` until [`STRIKES`] consecutive non-[`Sample::Busy`] readings
/// have been seen in a row, then returns the last of them.
///
/// `source` is polled immediately on entry, and then once per [`INTERVAL`]
/// after every reading that did not just complete the run — so a caller
/// scripting exactly `STRIKES` empty readings with no interleaved
/// [`Sample::Busy`] measures `STRIKES - 1` intervals of wait, not `STRIKES`.
/// A [`Sample::Busy`] reading resets the count to zero at any point in the
/// run; that reset is the entire reason this debounces rather than trusting
/// one sample, per [`STRIKES`]'s own doc.
///
/// The loop takes its readings through a generic `source` rather than a
/// concrete `Client` so this stays testable against a scripted sequence with
/// no socket involved; `runtime`'s real caller (Task 9) polls a live `Client`
/// behind the same closure shape.
#[allow(dead_code)]
pub async fn watch_until_empty<F, Fut>(mut source: F) -> Sample
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Sample>,
{
    let mut strikes: u8 = 0;
    loop {
        let reading = source().await;
        strikes = if reading == Sample::Busy {
            0
        } else {
            strikes + 1
        };
        if strikes >= STRIKES {
            return reading;
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use shep_core::protocol::{DogSource, ProcessInfo};
    use shep_core::status::ProcStatus;

    use super::*;

    /// A sheep row named `name` in `status`, otherwise empty.
    fn sheep_info(name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(1, name, status).build()
    }

    /// A dog row named `name` in `status` — `dog` set so [`sample`]'s filter
    /// skips it.
    fn dog_info(name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(1, name, status)
            .dog(Some(DogSource::BuiltIn))
            .build()
    }

    /// fails if a dog keeps a dead flock alive. `shep enable metrics` inside
    /// a container would otherwise mean the container never exits.
    #[test]
    fn a_flock_of_nothing_but_dogs_is_empty() {
        let flock = vec![dog_info("metrics", ProcStatus::Online)];
        assert_eq!(sample(&flock), Sample::EmptyClean);
    }

    /// fails if a sheep between backoff attempts reads as gone.
    #[test]
    fn a_sheep_waiting_to_restart_is_busy() {
        assert_eq!(
            sample(&[sheep_info("web", ProcStatus::WaitingRestart)]),
            Sample::Busy
        );
    }

    /// fails if a clean stop and a failure report the same thing — the whole
    /// of decision 13's exit-code split.
    #[test]
    fn an_errored_sheep_makes_an_empty_flock_a_failed_one() {
        assert_eq!(
            sample(&[sheep_info("web", ProcStatus::Stopped)]),
            Sample::EmptyClean
        );
        assert_eq!(
            sample(&[sheep_info("web", ProcStatus::Errored)]),
            Sample::EmptyFailed
        );
        assert_eq!(
            sample(&[
                sheep_info("a", ProcStatus::Stopped),
                sheep_info("b", ProcStatus::Errored)
            ]),
            Sample::EmptyFailed
        );
    }

    /// fails if the debounce is dropped or miscounted, and fails differently
    /// if a busy reading does not reset the strike count. On a paused clock
    /// (IR-46: the forcing mechanism is the test's own virtual-time advance
    /// via `tokio::time::sleep` inside `watch_until_empty` itself), so this
    /// measures the interval rather than waiting six real seconds.
    ///
    /// Script: clean, busy (resets), clean, clean, failed. Without the reset,
    /// three strikes land on the third reading (`clean`) and this returns
    /// `EmptyClean`; with it, three strikes land on the fifth (`failed`).
    /// Elapsed virtual time pins the interval count directly: four sleeps of
    /// `INTERVAL` between five readings, none after the last.
    #[tokio::test(start_paused = true)]
    async fn three_consecutive_empty_samples_are_needed_and_one_busy_one_resets_them() {
        let mut script = vec![
            Sample::EmptyClean,
            Sample::Busy,
            Sample::EmptyClean,
            Sample::EmptyClean,
            Sample::EmptyFailed,
        ]
        .into_iter();

        let start = tokio::time::Instant::now();
        let result = watch_until_empty(|| {
            let reading = script
                .next()
                .expect("script exhausted before debounce settled");
            async move { reading }
        })
        .await;

        assert_eq!(result, Sample::EmptyFailed);
        assert_eq!(start.elapsed(), INTERVAL * 4);
    }
}
