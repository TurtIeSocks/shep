//! The `Clock` seam and the cron-restart worker (spec §4).
//!
//! [`spawn_cron_worker`] runs one name-group's `cron_restart` schedule for
//! as long as its [`tokio::task::JoinHandle`] lives, restarting every
//! instance of the name — stopped ones included, per [`ProcessSelector::Name`]'s
//! own reach — through the same [`SupervisorHandle::restart`] a manual
//! `shep restart` uses. It never touches the actor directly.

use core::time::Duration;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use shep_core::config::CronSchedule;
use shep_core::selector::ProcessSelector;

use crate::supervisor::{SupervisorError, SupervisorHandle};

/// Wall-clock reader.
///
/// Cron means wall time — 03:00 in a named zone — while every other deadline
/// in this engine is a `tokio::time::Instant` that `start_paused` can move.
/// The two cannot be the same clock, so this is the seam that lets a paused
/// test drive a cron schedule.
pub trait Clock: Send + Sync + 'static {
    /// The current instant in UTC.
    fn now_utc(&self) -> DateTime<Utc>;
}

/// `Clock` over the real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Longest a cron worker sleeps before re-deriving its next occurrence, when
/// `shep.toml` names no `max_cron_sleep`.
///
/// A single `sleep_until(next)` is wrong across a laptop suspend, an NTP step
/// or a DST wall-clock shift: the sleep was computed against a wall time that
/// no longer holds, and the job fires late by however far the clock moved.
/// Re-deriving at least this often bounds that error to one minute at the cost
/// of one wakeup per minute per cron-configured sheep.
///
/// Not read by any non-test code path yet: choosing between this default and
/// a configured `max_cron_sleep` is the daemon's boot wiring's job, and this
/// module only owns the worker and the constant, not the call site that
/// starts one. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
#[allow(dead_code)]
pub(crate) const DEFAULT_MAX_CRON_SLEEP: Duration = Duration::from_secs(60);

/// Runs one sheep-group's cron schedule until the handle is dropped.
///
/// `max_sleep` bounds how long the loop parks before it re-reads the clock;
/// it changes how quickly the worker recovers from a wall-clock jump, never
/// whether an occurrence fires.
///
/// Cancellation: the returned handle aborts the loop on `abort()`; the loop
/// itself holds no state that needs unwinding.
pub fn spawn_cron_worker(
    name: String,
    schedule: CronSchedule,
    clock: Arc<dyn Clock>,
    supervisor: SupervisorHandle,
    max_sleep: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now = clock.now_utc();
            let next = match schedule.next_after(now) {
                Ok(Some(next)) => next,
                Ok(None) => {
                    tracing::info!(
                        name,
                        pattern = schedule.pattern(),
                        "cron_restart pattern has no further occurrence; worker ending"
                    );
                    return;
                }
                Err(err) => {
                    tracing::warn!(
                        name,
                        pattern = schedule.pattern(),
                        %err,
                        "cron schedule could not resolve its next occurrence; worker ending"
                    );
                    return;
                }
            };
            // `to_std` fails on a negative delta (`next` already due); a
            // zero sleep is the correct saturating behavior there, not a
            // reason to skip the loop's own re-check below.
            let until_next = (next - now).to_std().unwrap_or(Duration::ZERO);
            tokio::time::sleep(until_next.min(max_sleep)).await;

            // Missed occurrences are not replayed. The sleep above may have
            // been the capped `max_sleep`, not the full wait until `next` —
            // without this re-check, a capped sleep that expires before
            // `next` would fire early, every minute, forever. And because
            // `next` is re-derived from `clock.now_utc()` on every loop
            // iteration rather than tracked across a suspend, a daemon that
            // was asleep for six missed hourly occurrences restarts once,
            // not six: the loop's structure gives that at-most-one behavior
            // for free. Do not "fix" this into a catch-up loop.
            if clock.now_utc() >= next {
                match supervisor
                    .restart(ProcessSelector::Name(name.clone()))
                    .await
                {
                    Ok(_) => {}
                    Err(SupervisorError::NotFound) => {
                        // The sheep is gone but the registry has not
                        // disarmed this worker yet — expected during the
                        // window between the last instance stopping and the
                        // owner tearing this task down.
                        tracing::debug!(name, "cron fired but no sheep by this name is registered");
                    }
                    Err(err @ SupervisorError::SpawnFailed(_)) => {
                        // This occurrence is lost, but the schedule stands:
                        // the next iteration re-derives the following one.
                        tracing::warn!(name, %err, "cron-triggered restart failed to spawn");
                    }
                    Err(err @ SupervisorError::EngineStopped) => {
                        tracing::warn!(name, %err, "supervisor engine has shut down; cron worker ending");
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use tokio::sync::broadcast;

    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use crate::testing::{TestClock, test_paths};
    use shep_core::config::{AppConfig, normalize};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};

    /// Dyn-compatibility smoke test (IR-10): fails to compile the moment
    /// somebody adds a generic (non-dyn-safe) method to `Clock`.
    #[test]
    fn clock_is_dyn_compatible() {
        let _: &dyn Clock = &SystemClock;
    }

    /// Generous bound on how long a test may wait for an event on the
    /// (paused) tokio clock before concluding the worker is broken. Costs no
    /// real wall-clock time: the paused runtime auto-advances to this
    /// deadline only if nothing else becomes ready first.
    const EVENT_WAIT: Duration = Duration::from_secs(30);

    fn dt(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid RFC3339 timestamp")
    }

    /// One supervisor engine over a scripted runner with plenty of
    /// `never_exits` procs — enough for one initial start plus several
    /// cron-triggered restarts in any single test below.
    fn spawn_test_fixture() -> (
        SupervisorHandle,
        broadcast::Receiver<BusEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (events, rx) = broadcast::channel(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits(); 8]);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        (handle, rx, dir)
    }

    async fn start_named(handle: &SupervisorHandle, name: &str) {
        let app = AppConfig::minimal(name, "./srv");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    }

    /// Spawns a worker and yields once before returning.
    ///
    /// `tokio::time::advance` jumps the clock immediately and only then lets
    /// ready tasks run — a worker spawned right before a big jump would take
    /// its very first `clock.now_utc()` reading *after* the jump, past the
    /// occurrence the test means to observe (`next_after` is strictly-after,
    /// so a reading that lands exactly on the boundary skips it). Yielding
    /// once first lets the worker commit to its `next` while the clock still
    /// reads close to the caller's `now`, matching how it behaves in
    /// production: a freshly spawned worker is polled for the first time
    /// essentially immediately, long before any wall-clock jump.
    async fn spawn_worker_and_settle(
        name: &str,
        schedule: CronSchedule,
        clock: Arc<dyn Clock>,
        handle: &SupervisorHandle,
        max_sleep: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let worker =
            spawn_cron_worker(name.to_string(), schedule, clock, handle.clone(), max_sleep);
        tokio::task::yield_now().await;
        worker
    }

    /// Waits for the next `BusEvent::Process { event: Restart, .. }` for
    /// `name`, wrapped in a timeout so a worker that never restarts fails
    /// the test instead of hanging it (rule 4).
    async fn expect_restart(rx: &mut broadcast::Receiver<BusEvent>, name: &str) -> ProcessInfo {
        loop {
            match tokio::time::timeout(EVENT_WAIT, rx.recv()).await {
                Ok(Ok(BusEvent::Process {
                    event: ProcessEventKind::Restart,
                    info,
                    ..
                })) if info.name == name => return info,
                Ok(Ok(_)) => continue,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(err)) => panic!("event stream closed before a restart for {name}: {err}"),
                Err(_) => panic!("timed out waiting for a cron restart of {name}"),
            }
        }
    }

    /// Drains every already-queued event, panicking if any of them is a
    /// `Restart` for `name` — a claim that nothing happened, so it reads
    /// with `try_recv` rather than waiting on the (paused) clock to move.
    fn assert_no_restart_pending(rx: &mut broadcast::Receiver<BusEvent>, name: &str) {
        loop {
            match rx.try_recv() {
                Ok(BusEvent::Process {
                    event: ProcessEventKind::Restart,
                    info,
                    ..
                }) if info.name == name => {
                    panic!("unexpected cron restart of {name} observed");
                }
                Ok(_) => continue,
                Err(broadcast::error::TryRecvError::Empty) => return,
                Err(err) => panic!("event channel error while checking for no restart: {err}"),
            }
        }
    }

    /// Waits up to `window` for a `Restart` for `name`, panicking if one
    /// arrives — unlike [`assert_no_restart_pending`]'s bare `try_recv`,
    /// this actually polls, so a restart still working its way through the
    /// worker → actor → kill-ladder round trip gets the scheduling rounds it
    /// needs to land in the channel before the check gives up. Only safe to
    /// swap for the `try_recv` form when the caller already forced that
    /// round trip to settle (e.g. a prior [`expect_restart`]).
    async fn assert_no_restart_within(
        rx: &mut broadcast::Receiver<BusEvent>,
        name: &str,
        window: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Err(_) => return, // window elapsed with nothing matching — expected
                Ok(Ok(BusEvent::Process {
                    event: ProcessEventKind::Restart,
                    info,
                    ..
                })) if info.name == name => {
                    panic!(
                        "unexpected cron restart of {name} observed (restarts={})",
                        info.restarts
                    );
                }
                Ok(Ok(_)) => continue,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(err)) => {
                    panic!("event channel closed while checking for no restart of {name}: {err}")
                }
            }
        }
    }

    // fails if a loop fires on the capped sleep instead of the occurrence
    // (a `0 * * * *` restart at every wakeup rather than only at the top of
    // the hour would report far more than 3 restarts, or restarts whose
    // count does not land on 1, 2, 3 in order)
    #[tokio::test(start_paused = true)]
    async fn fires_at_the_top_of_three_successive_hours() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        start_named(&handle, name).await;
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        let mut observed = Vec::new();
        for _ in 0..3 {
            // Fine-grained stepping, not one big jump: a single
            // `advance(3600s)` would resolve the worker's own pending sleep
            // in one shot regardless of whether it re-checks `next` or fires
            // unconditionally on every wake — the defect this test exists to
            // catch would be invisible on a paused clock advanced that way.
            for _ in 0..120 {
                tokio::time::advance(Duration::from_secs(30)).await;
            }
            let info = expect_restart(&mut rx, name).await;
            observed.push(info.restarts);
        }
        assert_eq!(observed, vec![1, 2, 3]);
        worker.abort();
    }

    // fails if the cap (shorter than the hourly interval) causes an early
    // or repeated fire instead of exactly one restart at the boundary
    #[tokio::test(start_paused = true)]
    async fn thirty_second_steps_across_one_hour_yield_exactly_one_restart() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        start_named(&handle, name).await;
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:30Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        // 120 * 30s = one hour, crossing the 01:00:00 occurrence exactly once.
        for _ in 0..120 {
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        let info = expect_restart(&mut rx, name).await;
        assert_eq!(info.restarts, 1);
        assert_no_restart_pending(&mut rx, name);
        worker.abort();
    }

    // fails if a catch-up loop replays the backlog: a naive implementation
    // that tracks every missed boundary would restart six times, not once
    #[tokio::test(start_paused = true)]
    async fn one_jump_past_six_occurrences_yields_exactly_one_restart() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        start_named(&handle, name).await;
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        // A "suspended laptop": six hourly occurrences pass in one jump.
        tokio::time::advance(Duration::from_secs(6 * 3600 + 30)).await;
        let info = expect_restart(&mut rx, name).await;
        assert_eq!(info.restarts, 1);
        assert_no_restart_pending(&mut rx, name);
        worker.abort();
    }

    // fails if `Ok(None)` ("never fires again") is treated as "try again":
    // that implementation spins forever and the join below times out
    #[tokio::test(start_paused = true)]
    async fn exhausted_pattern_ends_the_task_without_restarting() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        // 30 February never occurs — Task 1's own "never matches" fixture.
        let schedule = CronSchedule::parse("0 0 30 2 *", None).unwrap();
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        tokio::time::timeout(EVENT_WAIT, worker)
            .await
            .expect("worker did not end for an exhausted schedule")
            .expect("worker task panicked");
        assert_no_restart_pending(&mut rx, name);
    }

    // fails two ways: a worker that outlives its sheep (a second restart
    // arrives after abort), and — because the first restart is observed
    // before the abort — a worker that never fired at all, which a bare
    // "no restart after abort" assertion would not catch
    #[tokio::test(start_paused = true)]
    async fn abort_stops_the_worker_after_observing_one_restart() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        start_named(&handle, name).await;
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        tokio::time::advance(Duration::from_secs(3600)).await;
        let info = expect_restart(&mut rx, name).await;
        assert_eq!(info.restarts, 1);

        worker.abort();
        tokio::time::advance(Duration::from_secs(3600)).await;
        // Not `assert_no_restart_pending`'s bare `try_recv`: a worker that
        // outlives its sheep would have a second restart still working
        // through the async round trip at this exact instant, and a check
        // that doesn't poll for it would pass by arriving too early.
        assert_no_restart_within(&mut rx, name, Duration::from_secs(10)).await;
    }

    // fails if `max_sleep` is ignored in favor of `DEFAULT_MAX_CRON_SLEEP`
    // (60s): that path wakes 60 times over the hour and reads the clock at
    // least 120 times (2 reads/iteration), well past this bound. A 10-minute
    // cap crossing a 3600s gap takes 6 iterations, so this loop's own shape
    // reads 12 times; the bound is loose enough to tolerate an
    // implementation that reads only once per iteration (7 reads) too.
    #[tokio::test(start_paused = true)]
    async fn ten_minute_cap_reads_the_clock_fewer_than_twenty_times() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        start_named(&handle, name).await;
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker = spawn_worker_and_settle(
            name,
            schedule,
            Arc::clone(&clock) as Arc<dyn Clock>,
            &handle,
            Duration::from_secs(600),
        )
        .await;

        // A single `advance(3600s)` jump would resolve the worker's own
        // pending sleep in one shot regardless of how it was capped — the
        // very difference this test exists to see would be invisible on a
        // paused clock. Stepping in 30s increments (finer than either cap
        // under discussion) instead lets the worker's own cadence, not the
        // test's, decide how many times it wakes.
        for _ in 0..120 {
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        let info = expect_restart(&mut rx, name).await;
        assert_eq!(info.restarts, 1);
        assert!(
            clock.reads() < 20,
            "expected fewer than 20 clock reads with a 10-minute max_sleep honored, got {}",
            clock.reads()
        );
        worker.abort();
    }
}
