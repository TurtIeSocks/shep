//! The empty-flock watcher: turns a poll of the flock into a [`Sample`], and
//! debounces a run of empty samples before `runtime`'s foreground engine
//! decides the flock is gone rather than mid-recovery.
//!
//! The clean/failed split decides between [`crate::exit::ExitCode::Success`]
//! and [`crate::exit::ExitCode::FlockEmpty`].

use std::future::Future;
use std::time::Duration;

use shep_core::protocol::ProcessInfo;
use shep_core::status::ProcStatus;

/// What one poll of the flock says about whether the foreground engine should
/// still be running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// Dogs do not count: counting one would keep a container alive forever after
/// its app died. `Stopping` counts as busy, so a reload in progress does not
/// read as the flock having gone away.
#[must_use]
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

/// Consecutive empty samples before the engine gives up: a single sample
/// catches the gap between a sheep exiting and its backoff restart.
pub const STRIKES: u8 = 3;

/// The interval between polls while debouncing an emptying flock.
pub const INTERVAL: Duration = Duration::from_secs(2);

/// Polls `source` until [`STRIKES`] consecutive non-[`Sample::Busy`] readings
/// have been seen in a row, then returns the last of them.
///
/// `source` is polled immediately on entry, then once per [`INTERVAL`] after
/// every reading that did not complete the run, so `STRIKES` uninterrupted
/// empty readings measure `STRIKES - 1` intervals of wait. A [`Sample::Busy`]
/// reading resets the count to zero.
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

    fn sheep_info(name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(1, name, status).build()
    }

    fn dog_info(name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(1, name, status)
            .dog(Some(DogSource::BuiltIn))
            .build()
    }

    #[test]
    fn a_flock_of_nothing_but_dogs_is_empty() {
        let flock = vec![dog_info("metrics", ProcStatus::Online)];
        assert_eq!(sample(&flock), Sample::EmptyClean);
    }

    #[test]
    fn a_sheep_waiting_to_restart_is_busy() {
        assert_eq!(
            sample(&[sheep_info("web", ProcStatus::WaitingRestart)]),
            Sample::Busy
        );
    }

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

    /// Without the reset, three strikes land on the third reading and this
    /// returns `EmptyClean`. The paused clock lets `start.elapsed()` pin four
    /// sleeps of `INTERVAL` between five readings, none after the last.
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
