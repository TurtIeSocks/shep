//! The `Clock` seam and the cron-restart worker.
//!
//! [`spawn_cron_worker`] runs one name-group's `cron_restart` schedule for as
//! long as its handle lives, restarting every instance of the name (stopped
//! ones included) through [`SupervisorHandle::restart_automatic`], budget
//! reset included. An operator's `stop` racing an in-flight occurrence
//! wins, since this is not a person's `shep restart`.
//!
//! Re-derives the next occurrence from wall time on every wake instead of
//! sleeping across one long interval, so a clock jump costs one late
//! restart rather than a replayed backlog. `max_cron_sleep` bounds how
//! late; five-field standard cron only.

use core::time::Duration;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use shep_core::config::CronSchedule;
use shep_core::selector::ProcessSelector;

use crate::supervisor::{SupervisorError, SupervisorHandle};

/// Wall-clock reader.
///
/// Cron means wall time, not the `tokio::time::Instant` every other
/// deadline in this engine uses, so this is the seam that lets a paused
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
/// Re-deriving at least this often bounds the lateness a suspend, NTP step
/// or DST shift can cause to one minute, at the cost of one wakeup per
/// minute per cron-configured sheep.
///
/// Applied in exactly one place, `boot`'s `options.max_cron_sleep.unwrap_or`:
/// a second default would let the two drift apart. `shep-core` carries the
/// floor, never the default. `#[allow(dead_code)]` because a non-unix build
/// (`boot` is unix-only) has no reader.
#[allow(dead_code)]
pub(crate) const DEFAULT_MAX_CRON_SLEEP: Duration = Duration::from_secs(60);

/// Floor `spawn_cron_worker` enforces on `max_sleep`, regardless of caller.
///
/// `shep-core` rejects a configured value below this bound, but that guard
/// only covers the call site that routes through it. Without a floor here
/// too, a `Duration::ZERO` turns the loop into a hot spin that still fires
/// correctly, which makes the failure hard to attribute. Declared
/// independently from `shep-core`'s `MIN_CRON_SLEEP` (private, and a
/// different duration type), matching it in spirit only.
const MIN_MAX_SLEEP: Duration = Duration::from_millis(1_000);

/// Runs one sheep-group's cron schedule until the handle is dropped.
///
/// `max_sleep` bounds how long the loop parks before it re-reads the clock;
/// it changes how quickly the worker recovers from a wall-clock jump, never
/// whether an occurrence fires. Clamped to at least `MIN_MAX_SLEEP`.
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
    let max_sleep = max_sleep.max(MIN_MAX_SLEEP);
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
            // next_after is strictly after now, so next - now is always
            // positive and to_std cannot take the Err arm; unwrap_or is
            // defensive, in case that contract ever loosens.
            let until_next = (next - now).to_std().unwrap_or(Duration::ZERO);
            tokio::time::sleep(until_next.min(max_sleep)).await;

            // Re-check: the sleep above may be the capped `max_sleep`, not
            // the full wait until `next`, so firing unconditionally here
            // would fire early every minute. `next` is re-derived each
            // iteration, so a daemon asleep for six occurrences restarts once.
            if clock.now_utc() >= next {
                match supervisor
                    .restart_automatic(ProcessSelector::Name(name.clone()))
                    .await
                {
                    Ok(_) => {}
                    Err(SupervisorError::NotFound) => {
                        // Expected: the registry has not disarmed this
                        // worker yet, between the last instance stopping
                        // and the owner tearing the task down.
                        tracing::debug!(name, "cron fired but no sheep by this name is registered");
                    }
                    Err(err @ SupervisorError::SpawnFailed(_)) => {
                        // This occurrence is lost, but the schedule stands:
                        // the next iteration re-derives the following one.
                        tracing::warn!(name, %err, "cron-triggered restart failed to spawn");
                    }
                    Err(
                        err @ (SupervisorError::ReopenFailed(_)
                        | SupervisorError::FlushFailed(_)
                        | SupervisorError::ReloadInFlight(_)
                        | SupervisorError::InvalidScale(_)
                        | SupervisorError::CannotStart(_)
                        | SupervisorError::IsADog(_)
                        | SupervisorError::InvalidEnv(_)
                        | SupervisorError::InvalidField(_)
                        | SupervisorError::Overrides(_)),
                    ) => {
                        // None of these nine can arrive here. A restart
                        // writes no logs, reloads nothing, scales nothing,
                        // and names no dog, field or override. Named rather
                        // than a catch-all, so a new variant fails to compile.
                        tracing::warn!(name, %err, "cron-triggered restart reported an unrelated failure");
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
    use crate::bus::SharedEvent;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use crate::testing::{TestClock, test_paths};
    use shep_core::config::{AppConfig, normalize};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
    use shep_core::status::ProcStatus;

    /// Fails to compile if a generic (non-dyn-safe) method is added to `Clock`.
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
    /// `never_exits` procs: enough for one initial start plus several
    /// cron-triggered restarts in any single test below.
    fn spawn_test_fixture() -> (
        SupervisorHandle,
        broadcast::Receiver<SharedEvent>,
        tempfile::TempDir,
    ) {
        spawn_test_fixture_with(vec![ProcScript::never_exits(); 8])
    }

    /// [`spawn_test_fixture`] over a caller-chosen script pool, for the one
    /// case that needs a sheep which sits out its whole kill ladder rather
    /// than a merely long-lived one.
    fn spawn_test_fixture_with(
        scripts: Vec<ProcScript>,
    ) -> (
        SupervisorHandle,
        broadcast::Receiver<SharedEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (events, rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(scripts);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        (handle, rx, dir)
    }

    async fn start_named(handle: &SupervisorHandle, name: &str) {
        let app = AppConfig::minimal(name, "./srv");
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
    }

    /// Spawns a worker and yields once before returning.
    ///
    /// `tokio::time::advance` jumps the clock before ready tasks run, so a
    /// worker spawned right before a jump would take its first
    /// `clock.now_utc()` reading after it, past the occurrence under test
    /// (`next_after` is strictly-after). Yielding once lets the worker
    /// commit to `next` while the clock still reads close to `now`.
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
    /// the test instead of hanging it.
    async fn expect_restart(rx: &mut broadcast::Receiver<SharedEvent>, name: &str) -> ProcessInfo {
        loop {
            match tokio::time::timeout(EVENT_WAIT, rx.recv())
                .await
                .map(|received| received.map(|event| event.to_event()))
            {
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
    /// `Restart` for `name`: a claim that nothing happened, so it reads
    /// with `try_recv` rather than waiting on the (paused) clock to move.
    fn assert_no_restart_pending(rx: &mut broadcast::Receiver<SharedEvent>, name: &str) {
        loop {
            match rx.try_recv().map(|event| event.to_event()) {
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
    /// arrives. Unlike [`assert_no_restart_pending`]'s bare `try_recv`, this
    /// polls, so a restart still working through the kill ladder gets the
    /// scheduling rounds it needs. Only safe to swap for `try_recv` once
    /// the caller already forced that round trip to settle.
    async fn assert_no_restart_within(
        rx: &mut broadcast::Receiver<SharedEvent>,
        name: &str,
        window: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv())
                .await
                .map(|received| received.map(|event| event.to_event()))
            {
                Err(_) => return, // window elapsed with nothing matching: expected
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
            // Fine-grained stepping, not one big jump: a single advance
            // would resolve the pending sleep in one shot regardless of
            // whether it re-checks `next`, hiding the defect this test
            // exists to catch.
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

    // Fails by hanging, not failing: `Ok(None) => continue` busy-spins with
    // no `.await`, so CI's own timeout is the backstop, not this test's
    // `timeout`. No in-test watchdog task: `start_paused` blocks
    // `multi_thread`, so nothing here could preempt the spin.
    #[tokio::test(start_paused = true)]
    async fn exhausted_pattern_ends_the_task_without_restarting() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        // 30 February never occurs: the canonical "never matches" pattern.
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

    // The worker's other exit path: the engine stopping, not the schedule
    // running out. Fails if `EngineStopped` falls through instead of
    // returning, firing restarts into a mailbox nobody reads forever. No
    // scripts in the fixture: the engine is down before any occurrence fires.
    #[tokio::test(start_paused = true)]
    async fn the_worker_ends_when_the_supervisor_engine_has_stopped() {
        let (handle, _rx, _dir) = spawn_test_fixture_with(Vec::new());
        let name = "web";
        handle.shutdown().await;
        // The premise, stated rather than assumed: with the actor gone, the
        // restart this worker is about to attempt answers `EngineStopped`.
        assert_eq!(
            handle
                .restart_automatic(ProcessSelector::Name(name.to_string()))
                .await
                .unwrap_err(),
            SupervisorError::EngineStopped
        );

        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        // Stepped finer than the sleep cap rather than jumped (rule 11), so
        // the worker's own cadence decides when it wakes on the occurrence.
        for _ in 0..120 {
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        tokio::time::timeout(EVENT_WAIT, worker)
            .await
            .expect("the worker did not end after the engine shut down")
            .expect("worker task panicked");
    }

    // Fails two ways: a worker that outlives its sheep (a second restart
    // arrives after abort), or one that never fired at all, since the
    // first restart is observed before the abort.
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

    // The one pattern whose next occurrence lands exactly on
    // `DEFAULT_MAX_CRON_SLEEP`: the clamp and the true occurrence coincide,
    // the only place an off-by-one in either can show. Fails if the
    // re-check used `>` instead of `>=`, or if `next_after` became inclusive.
    #[tokio::test(start_paused = true)]
    async fn a_per_minute_pattern_fires_once_on_the_max_sleep_boundary() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        start_named(&handle, name).await;
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("* * * * *", None).unwrap();
        // Captured before the worker so the offsets below read as wall-clock
        // times: `TestClock` maps `epoch + (Instant::now() - started)`, so an
        // elapsed 60s here is exactly "the clock now says 00:01:00".
        let start = tokio::time::Instant::now();
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        // A hair short of the boundary: nothing may fire early.
        assert_no_restart_within(&mut rx, name, Duration::from_millis(59_999)).await;
        let first = expect_restart(&mut rx, name).await;
        assert_eq!(first.restarts, 1);
        assert_eq!(
            tokio::time::Instant::now() - start,
            Duration::from_secs(60),
            "the occurrence and the sleep cap coincide here, so the restart belongs at \
             exactly 00:01:00 -- neither dropped nor early"
        );

        // ...and exactly once: the window ends a hair before 00:02:00.
        assert_no_restart_within(&mut rx, name, Duration::from_millis(59_999)).await;
        let second = expect_restart(&mut rx, name).await;
        assert_eq!(second.restarts, 2);
        assert_eq!(
            tokio::time::Instant::now() - start,
            Duration::from_secs(120),
            "the schedule must survive its own boundary and fire the next occurrence too"
        );
        worker.abort();
    }

    // Fails if `max_sleep` is ignored in favor of `DEFAULT_MAX_CRON_SLEEP`
    // (60s): that path wakes 60 times over the hour, well past this bound.
    // Loose enough to tolerate an implementation reading the clock once or
    // twice per iteration.
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

        // A single advance would resolve the pending sleep in one shot
        // regardless of how it was capped. Stepping in 30s increments lets
        // the worker's own cadence, not the test's, decide how many times
        // it wakes.
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

    // Fails if `max_sleep.max(MIN_MAX_SLEEP)` becomes plain `max_sleep`: a
    // sub-second value (not `Duration::ZERO`, which hangs rather than
    // reddens) that still fires correctly, so only the clock-read count
    // catches the spin.
    #[tokio::test(start_paused = true)]
    async fn a_sub_second_max_sleep_is_floored_instead_of_waking_every_millisecond() {
        let (handle, _rx, _dir) = spawn_test_fixture_with(Vec::new());
        let name = "web";
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker = spawn_worker_and_settle(
            name,
            schedule,
            Arc::clone(&clock) as Arc<dyn Clock>,
            &handle,
            Duration::from_millis(1),
        )
        .await;

        // Five floored sleeps' worth of virtual time: one `MIN_MAX_SLEEP`
        // per step while the floor holds, one millisecond without it, so no
        // step here can outrun the loop under test.
        tokio::time::sleep(MIN_MAX_SLEEP * 5).await;
        assert!(
            clock.reads() < 20,
            "max_sleep must be floored at MIN_MAX_SLEEP: five floored sleeps cost about \
             two clock reads each, got {}",
            clock.reads()
        );
        worker.abort();
    }

    // An operator's `stop` racing a cron-triggered kill ladder: the
    // operator's intent wins, ending `Stopped` rather than resurrected.
    // Fails if the worker calls `restart` instead of `restart_automatic`,
    // which lets `handle_exited` respawn behind the stop's back.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_beats_a_cron_triggered_restart_mid_ladder() {
        // Four procs: both instances' initial spawn, the untouched
        // instance's respawn, and the respawn a broken implementation
        // performs behind the stop's back.
        let (handle, mut rx, _dir) = spawn_test_fixture_with(vec![
            ProcScript::ignores_signals(), // held for the whole 1600ms ladder
            ProcScript::never_exits(),     // exits the moment the ladder signals it
            ProcScript::never_exits(),     // the untouched instance's respawn
            ProcScript::never_exits(),     // the respawn a broken implementation performs
        ]);
        let name = "web";
        let mut app = AppConfig::minimal(name, "./srv");
        app.instances = 2;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap();
        let listed = handle.list().await;
        let (held, released) = (listed[0].id, listed[1].id);

        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let schedule = CronSchedule::parse("0 * * * *", None).unwrap();
        let worker =
            spawn_worker_and_settle(name, schedule, clock, &handle, DEFAULT_MAX_CRON_SLEEP).await;

        // The occurrence claims both instances' next exit and starts both
        // kill ladders. Only the second sheep's ladder can finish without
        // the clock moving, so its restart lands while the first is still
        // mid-ladder.
        tokio::time::advance(Duration::from_secs(3600)).await;
        let restarted = expect_restart(&mut rx, name).await;
        assert_eq!(
            (restarted.id, restarted.restarts),
            (released, 1),
            "the occurrence never reached the actor, so the stop below would \
             race nothing -- got {restarted:?}"
        );
        // Aborted before the stop so the worker cannot fire a second
        // occurrence once the paused clock advances past the next hour.
        worker.abort();

        let stopped = handle.stop(ProcessSelector::Id(held)).await.unwrap();
        assert_eq!(stopped.len(), 1);
        assert_eq!(
            (stopped[0].id, stopped[0].status, stopped[0].restarts),
            (held, ProcStatus::Stopped, 0),
            "an operator's stop was silently converted into the cron-triggered \
             restart it raced -- got {stopped:?}"
        );
        let listed = handle.list().await;
        assert_eq!(
            (listed[0].id, listed[0].status, listed[0].pid),
            (held, ProcStatus::Stopped, None),
            "the sheep an operator stopped is running again -- got {listed:?}"
        );
        assert_eq!(
            (listed[1].id, listed[1].status),
            (released, ProcStatus::Online),
            "the instance the operator did not name must still be up, \
             restarted by the occurrence -- got {listed:?}"
        );
    }
}
