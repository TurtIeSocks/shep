//! The `Clock` seam and the cron-restart worker (spec §4).
//!
//! [`spawn_cron_worker`] runs one name-group's `cron_restart` schedule for
//! as long as its [`tokio::task::JoinHandle`] lives, restarting every
//! instance of the name — stopped ones included, per [`ProcessSelector::Name`]'s
//! own reach — through [`SupervisorHandle::restart_automatic`]. It never
//! touches the actor directly.
//!
//! An occurrence is not a person's `shep restart`, and goes in declaring that:
//! an operator's `stop` arriving while a cron-triggered restart is still
//! mid-kill-ladder takes the sheep back off it, rather than coming back
//! `Online` because it lost a race nobody was waiting on. The restart is
//! otherwise identical to a manual one, budget reset included.
//!
//! # When an occurrence fires
//!
//! Against **wall time**, in the app's `cron_timezone`, re-derived from the
//! clock on every iteration rather than tracked across one long sleep. That
//! is what makes the behaviour under a moving clock predictable:
//!
//! - **A laptop suspend, or an NTP step forward.** The worker wakes, reads
//!   the clock, and asks the schedule for the next occurrence *after now*.
//!   Occurrences that passed while it was asleep are gone.
//! - **A DST shift, or a step backward.** Same question, same answer, and
//!   the schedule is evaluated in its own zone rather than in UTC. An
//!   occurrence falling in a spring-forward gap lands on the first valid
//!   instant after the gap if the pattern names a fixed time, and is skipped
//!   if it is a wildcard or interval; a fall-back's repeated hour resolves to
//!   one instant rather than firing on both passes. All three are pinned by
//!   `shep_core::config::CronSchedule`'s own tests, and its `next_after` doc
//!   carries the one corner where two successive searches can return the same
//!   wall-clock occurrence.
//!
//! **A missed occurrence is never replayed**, and this is a choice rather
//! than a limitation. Replaying means a daemon that was asleep for six
//! hourly occurrences restarts a sheep six times on wake, in a burst, for
//! schedules whose whole point was to spread work out. Catch-up would also
//! have to decide how far back to look, and every answer to that is
//! arbitrary. So a suspended machine's flock restarts **once**, at the next
//! occurrence, and a schedule with no further occurrence at all ends its
//! worker instead of spinning.
//!
//! # What `max_cron_sleep` trades
//!
//! The loop parks for `min(time until next occurrence, max_cron_sleep)` and
//! re-checks. The knob buys **recovery speed after a clock jump**, and pays
//! in **wakeups**:
//!
//! - Shorter recovers faster from a suspend or an NTP step, because the
//!   worker re-reads the clock sooner, and costs one wakeup per interval per
//!   cron-configured name.
//! - Longer wakes less often and drifts for longer after a jump.
//! - Neither changes **whether** an occurrence fires. The re-check after the
//!   sleep is what guarantees that: a capped sleep that expires early loops
//!   and sleeps again rather than firing.
//!
//! It defaults to `DEFAULT_MAX_CRON_SLEEP` and is set by `[daemon]
//! max_cron_sleep` (or `SHEP_MAX_CRON_SLEEP`). Values below one second are
//! rejected at config load rather than clamped — see `MIN_MAX_SLEEP` for
//! why this function *also* floors what it is handed.
//!
//! # Caveats
//!
//! - **Five-field standard cron only.** No seconds field, and none of
//!   croner's `L`/`W`/`#`/`?` extensions. The seven vixie `@nicknames` are
//!   accepted, but shep expands them itself before croner sees them, so the
//!   dialect stays literally five-field.
//! - **Granularity is the loop's, not the schedule's.** A restart fires on
//!   the first wake at or after its occurrence; the wake is scheduled, not
//!   instantaneous, so a busy runtime can land it late by however long the
//!   task waits to be polled.
//! - **The restart is group-wide.** Every instance of the name goes down and
//!   comes back together, stopped instances included. If you need the app to
//!   keep serving through it, that is what reload is for.
//!
//! ## Reference
//!
//! - [`Clock`], [`SystemClock`]
//! - [`spawn_cron_worker`]
//! - `DEFAULT_MAX_CRON_SLEEP`, `MIN_MAX_SLEEP`
//! - [`shep_core::config::CronSchedule`] — the pattern's own grammar and
//!   timezone resolution

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
/// Applied in exactly one place — `boot`'s `options.max_cron_sleep.unwrap_or`
/// — and that must stay the only one: a second default (in the CLI's
/// `boot_options`, or as a serde default back in `shep-core`) is how two
/// supposedly identical constants drift apart. `shep-core` carries the floor
/// and never the default; the daemon carries the default and never the floor.
/// Because `boot` is unix-only, a non-unix build of this crate's library
/// target still has no reader at all, which is what the `dead_code` allowance
/// below is for.
#[allow(dead_code)]
pub(crate) const DEFAULT_MAX_CRON_SLEEP: Duration = Duration::from_secs(60);

/// Floor `spawn_cron_worker` enforces on `max_sleep`, regardless of caller.
///
/// `shep-core`'s `DaemonConfig::load` already rejects a configured
/// `max_cron_sleep` below this same one-second bound — but that guard lives
/// behind boot wiring this module does not own (see `DEFAULT_MAX_CRON_SLEEP`'s
/// doc above), so it protects only the call site that reaches it, once one
/// exists. Without a floor here too, any caller — today's tests, or a boot
/// path added later that forgets to route through the validated config —
/// could hand this function a `Duration::ZERO` and turn the loop into a hot
/// spin that re-derives the schedule as fast as the runtime allows while
/// still firing correctly, which is exactly what makes that failure mode
/// hard to attribute. The value matches `shep-core`'s `MIN_CRON_SLEEP` in
/// spirit; it is declared independently rather than imported because that
/// constant is private to its module and the two crates use different
/// duration types (`UpDuration` there, `core::time::Duration` here).
const MIN_MAX_SLEEP: Duration = Duration::from_millis(1_000);

/// Runs one sheep-group's cron schedule until the handle is dropped.
///
/// `max_sleep` bounds how long the loop parks before it re-reads the clock;
/// it changes how quickly the worker recovers from a wall-clock jump, never
/// whether an occurrence fires. Clamped to at least `MIN_MAX_SLEEP` — see
/// its doc for why this function keeps its own floor instead of trusting the
/// caller to have validated one.
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
            // `next` is `schedule.next_after(now)`, which is strictly after
            // `now` by contract (`CronSchedule::next_after`'s own doc), so
            // `next - now` is always positive and `to_std` cannot actually
            // take the `Err` arm here. `unwrap_or(Duration::ZERO)` is
            // defensive, not a path this loop can reach today — it costs
            // nothing to keep and saves a `.expect()` panic if that contract
            // ever loosens.
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
                    .restart_automatic(ProcessSelector::Name(name.clone()))
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
                    Err(
                        err @ (SupervisorError::ReopenFailed(_)
                        | SupervisorError::FlushFailed(_)
                        | SupervisorError::ReloadInFlight(_)
                        | SupervisorError::InvalidScale(_)
                        | SupervisorError::CannotStart(_)),
                    ) => {
                        // A restart touches no log files, starts no reload,
                        // scales nothing and registers no batch, so none of
                        // the five can arrive.
                        // Named rather than swept into a catch-all, so a
                        // variant this path CAN produce still fails to
                        // compile here.
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
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use crate::testing::{TestClock, test_paths};
    use shep_core::config::{AppConfig, normalize};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
    use shep_core::status::ProcStatus;

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
        spawn_test_fixture_with(vec![ProcScript::never_exits(); 8])
    }

    /// [`spawn_test_fixture`] over a caller-chosen script pool, for the one
    /// case that needs a sheep which sits out its whole kill ladder rather
    /// than a merely long-lived one.
    fn spawn_test_fixture_with(
        scripts: Vec<ProcScript>,
    ) -> (
        SupervisorHandle,
        broadcast::Receiver<BusEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (events, rx) = broadcast::channel(64);
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
    // that mutation (`Ok(None) => continue`) has no `.await` between loop
    // iterations, so it starves this current-thread runtime outright rather
    // than raising the `tokio::time::timeout` below — a pure busy-spin never
    // yields back to the executor, so the timeout's own timer future is
    // never polled again either. This test does not fail on that mutation;
    // it hangs forever, and the backstop is the CI job's own timeout, not
    // this test's `tokio::time::timeout`. Don't "fix" that with an in-test
    // watchdog task: `start_paused` cannot be combined with
    // `flavor = "multi_thread"` (tokio's own proc macro rejects it at
    // compile time), so on this single-threaded runtime a watchdog *task*
    // would starve alongside the spin it's meant to catch — only a real OS
    // thread could preempt it, and this test doesn't reach for one.
    #[tokio::test(start_paused = true)]
    async fn exhausted_pattern_ends_the_task_without_restarting() {
        let (handle, mut rx, _dir) = spawn_test_fixture();
        let name = "web";
        // 30 February never occurs — the canonical "never matches" pattern.
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

    // The worker's other exit: not a schedule that ran out, but the engine it
    // restarts through going away. `exhausted_pattern_ends_the_task_without_
    // restarting` covers the first; nothing covered this one.
    //
    // fails if the `EngineStopped` arm falls through to the next iteration
    // instead of returning: the worker would keep re-deriving occurrences and
    // firing restarts into a mailbox nobody reads, one per occurrence, for as
    // long as the process lives.
    //
    // No scripts in the fixture, and that is the honest count: the engine is
    // shut down before the occurrence fires, so no spawn is reachable under
    // either implementation — the correct one or the fallen-through one.
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

    // Boundary sweep (IR-40): the one pattern whose next occurrence lands
    // exactly on `DEFAULT_MAX_CRON_SLEEP`. Five fields cannot express seconds
    // at all, so per-minute is the tightest granularity the dialect has — and
    // it is also the interesting one, because 60s is the default sleep cap:
    // the clamp and the true next occurrence coincide, which is the only
    // place an off-by-one in either can show. Built with the default rather
    // than a custom `max_sleep` so the coincidence is real. The subsystem's
    // other boundary, a pattern with no further occurrence, is already pinned
    // by `exhausted_pattern_ends_the_task_without_restarting` above.
    //
    // The middle assertion is the at-most-one-catch-up claim, and it takes
    // the bounded-window form Global Constraints rule 11 asks for: the window
    // both crosses the span where a second firing would land and makes the
    // claim, and it is deliberately sized to stop a hair before 00:02:00 —
    // the occurrence that legitimately follows — because a window that
    // outran it would auto-advance straight into a real restart and turn a
    // pass into a confusing failure.
    //
    // fails if the re-check is `>` rather than `>=`: a wake landing exactly
    // on its occurrence would then decline to fire and re-derive the next
    // one, and the schedule would go quiet forever. And fails if
    // `next_after` were to become inclusive of `now`, which at this boundary
    // makes the post-restart iteration re-derive the SAME occurrence, sleep
    // zero and fire again immediately.
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

    // fails if `max_sleep.max(MIN_MAX_SLEEP)` becomes plain `max_sleep`: a
    // caller that skipped `shep-core`'s config-time rejection would then have
    // this loop re-derive its schedule a thousand times a second, per
    // cron-configured name, while still firing every occurrence correctly —
    // which is exactly what makes that burn hard to attribute to its cause.
    //
    // A sub-second value rather than the `Duration::ZERO` the floor's own doc
    // names, because zero does not redden this test — it hangs it. A zero
    // sleep resolves the instant it is polled, so the loop never parks on a
    // pending timer for the paused runtime to auto-advance past; it spins,
    // and the backstop is the CI job's own timeout, the shape
    // `exhausted_pattern_ends_the_task_without_restarting` already documents
    // above. One millisecond parks on a real timer, which keeps the mutation
    // observable as a count.
    //
    // The occurrence is an hour out, so nothing fires under either
    // implementation and no script is reachable — on a paused clock the only
    // trace a wakeup leaves is a clock read.
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

        // Five floored sleeps' worth of virtual time. The runtime's own
        // auto-advance walks it in whatever steps the worker's timers ask for
        // — one `MIN_MAX_SLEEP` while the floor holds, one millisecond
        // without it — so no step here can outrun the loop under test
        // (rule 11).
        tokio::time::sleep(MIN_MAX_SLEEP * 5).await;
        assert!(
            clock.reads() < 20,
            "max_sleep must be floored at MIN_MAX_SLEEP: five floored sleeps cost about \
             two clock reads each, got {}",
            clock.reads()
        );
        worker.abort();
    }

    // An operator's `stop` landing on a sheep whose kill ladder a cron
    // occurrence already started. Nobody typed the occurrence, so the
    // operator's intent wins: the sheep named ends `Stopped`, never
    // respawned, and `stop()` reports that honestly.
    //
    // Two instances, because one could not tell a pass from a test whose
    // occurrence never reached the actor at all — with a single sheep, a
    // `stop` that simply arrived first produces the very same `Stopped`. The
    // second instance is left alone precisely so its restart is observable:
    // waiting on that restart is both the proof the occurrence fired and the
    // barrier that puts it strictly before the `stop`, since one
    // `begin_manual` claims both instances' markers in the same synchronous
    // pass.
    //
    // fails if the cron worker declares `CommandOrigin::Operator` — calling
    // `restart` rather than `restart_automatic`: `claim_manual` then keeps
    // the occurrence's marker under plain first-command-wins, `handle_exited`
    // respawns, and the `stop()` caller is handed an `Online` snapshot of a
    // sheep that is genuinely back up with `restarts: 1`.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_beats_a_cron_triggered_restart_mid_ladder() {
        // Four procs, which is the most this test can demand: both instances'
        // initial ones, the respawn the untouched instance legitimately
        // performs, and the respawn a broken implementation performs behind
        // the stop's back. A pool of three would answer that fourth spawn
        // `SpawnFailed("script exhausted")` and land the bug in `Errored`
        // rather than the `Online` that shows how bad it is.
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

        // The occurrence claims BOTH instances' next exit and starts both kill
        // ladders. Only the second sheep's ladder can finish without the clock
        // moving, so its restart lands while the first is still mid-ladder.
        tokio::time::advance(Duration::from_secs(3600)).await;
        let restarted = expect_restart(&mut rx, name).await;
        assert_eq!(
            (restarted.id, restarted.restarts),
            (released, 1),
            "the occurrence never reached the actor, so the stop below would \
             race nothing -- got {restarted:?}"
        );
        // Aborted before the stop so the worker cannot fire a SECOND
        // occurrence into the assertions below once the paused clock
        // auto-advances past the next top of the hour. Its restart is already
        // in the actor's hands; the dropped reply receiver only means nobody
        // reads the answer.
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
