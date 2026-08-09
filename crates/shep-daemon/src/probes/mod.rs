//! The `Prober` seam and the liveness probe loop (spec §7).
//!
//! [`spawn_liveness_task`] runs one sheep's readiness/liveness probe on
//! [`ProbeConfig::interval`](shep_core::config::ProbeConfig::interval),
//! reporting through a shared channel once
//! [`ProbeConfig::failure_threshold`](shep_core::config::ProbeConfig::failure_threshold)
//! *consecutive* probes have failed, then ending: a sheep already declared
//! unhealthy is about to be restarted, and the loop for its replacement —
//! armed against a new pid — is a new one.
//!
//! ## Reference
//!
//! - [`Prober`], [`ProbeFailure`]
//! - [`LivenessFailure`], [`spawn_liveness_task`]

use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use std::sync::Arc;

use tokio::sync::mpsc;

use shep_core::config::{ProbeConfig, ProbeTarget};

/// Why a probe did not pass.
///
/// Growth is expected: each new probe transport brings its own failure modes
/// (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    /// The probe did not finish inside `ProbeConfig::timeout`.
    Timeout,
    /// The transport failed before a verdict was possible — connection
    /// refused, DNS failure, the command could not be spawned. Carries the
    /// rendered reason.
    Transport(String),
    /// The probe completed and the answer was negative: a non-2xx status, or a
    /// non-zero exit. Carries the status or exit code.
    Rejected(String),
}

// A boxed future rather than RPITIT, because `Arc<dyn Prober>` is how the
// engine holds this and RPITIT is not dyn-compatible. One allocation per probe
// — once per `interval`, default 10s — against three extra generic parameters
// threaded through the actor and every fixture.
/// Runs one probe against one target.
///
/// # Design note: `timeout` enforcement is the implementation's job
///
/// [`spawn_liveness_task`] awaits [`Self::probe`] directly; it does not
/// additionally wrap the call in its own `tokio::time::timeout`. That means
/// a `Prober` whose `probe` future never resolves — hangs rather than
/// erroring — stalls the liveness loop forever: no further probes, no
/// report, ever. This is an accepted design risk, not a defect (bounding
/// every code path by `timeout` needs implementation-specific knowledge,
/// e.g. a connect timeout versus a read timeout, that this seam has no
/// business dictating) — but any implementor (`OsProber`, Task 8, chief
/// among them) must itself guarantee `probe` resolves within `timeout` on
/// every path, or a hung sheep's liveness detection silently stops working.
pub trait Prober: Send + Sync + 'static {
    /// Probes `target`, giving up after `timeout`.
    ///
    /// # Errors
    ///
    /// - [`ProbeFailure::Timeout`] — the probe did not finish inside
    ///   `timeout`.
    /// - [`ProbeFailure::Transport`] — the transport failed before a verdict
    ///   was possible (connection refused, DNS failure, the command could
    ///   not be spawned).
    /// - [`ProbeFailure::Rejected`] — the probe completed and the answer was
    ///   negative (a non-2xx status, or a non-zero exit).
    fn probe<'a>(
        &'a self,
        target: &'a ProbeTarget,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProbeFailure>> + Send + 'a>>;
}

/// Floor `spawn_liveness_task` enforces on `interval`, regardless of caller.
///
/// `shep-core`'s `normalize` already rejects an explicit `interval = "0"` in
/// a Flockfile (`NormalizeError::ZeroInterval`) — but that guard lives
/// behind boot wiring this module does not own (the same reason
/// `cron::MIN_MAX_SLEEP` keeps its own floor even though `shep-core`
/// separately rejects a too-small `max_cron_sleep`), so it protects only the
/// call site that reaches it, once one exists. Without a floor here too, any
/// caller — today's tests, or a boot path added later that forgets to route
/// through the validated config — could hand this loop a `Duration::ZERO`
/// interval and turn it into a hot spin: measured live at roughly 380 probes
/// per second, which for `ProbeKind::Exec` is that many process spawns per
/// second, per sheep, forever. The value matches `cron::MIN_MAX_SLEEP` in
/// spirit; it is declared independently because that constant is private to
/// its own module.
const MIN_PROBE_INTERVAL: Duration = Duration::from_millis(1_000);

/// A sheep whose liveness probe hit `failure_threshold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessFailure {
    /// The sheep's id.
    pub id: u32,
    /// The pid this loop was armed against, for the same reason
    /// [`LimitBreach::root_pid`](crate::limits::LimitBreach) carries one.
    pub pid: u32,
}

/// Runs a sheep's liveness probe until the returned handle is aborted.
///
/// Reports through `failures` once `failure_threshold` consecutive probes have
/// failed, then ends: a sheep that has been declared unhealthy is about to be
/// restarted, and the loop for its replacement is a new one.
///
/// Must be called from within a Tokio runtime context: it spawns the probing
/// task immediately, the same way `spawn_supervisor`, `spawn_cron_worker` and
/// `PollingEnforcer::start` already document for themselves.
pub fn spawn_liveness_task(
    id: u32,
    pid: u32,
    config: ProbeConfig,
    target: ProbeTarget,
    prober: Arc<dyn Prober>,
    failures: mpsc::Sender<LivenessFailure>,
) -> tokio::task::JoinHandle<()> {
    let interval = config.interval.as_duration().max(MIN_PROBE_INTERVAL);
    let timeout = config.timeout.as_duration();
    let threshold = config.failure_threshold;
    tokio::spawn(async move {
        let mut consecutive_failures: u32 = 0;
        loop {
            // Interval is measured from the PREVIOUS probe's completion, not
            // from this loop's start: sleeping first, then probing, gives a
            // struggling app a full `interval` of quiet between probes even
            // when the probe itself runs long. A `tokio::time::interval`
            // grid ticking independently of how long the probe takes would
            // instead fire every overdue tick back-to-back once a probe
            // outlasts its own period — its default `MissedTickBehavior::
            // Burst` — which is exactly the shape that turns a slow service
            // into a dead one.
            tokio::time::sleep(interval).await;

            match prober.probe(&target, timeout).await {
                Ok(()) => consecutive_failures = 0,
                Err(_failure) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= threshold {
                        // The receiver may already be gone (Task 12's
                        // reporting task shut down alongside the engine);
                        // this loop's job ends either way, so the send
                        // result is intentionally discarded rather than
                        // branched on.
                        let _ = failures.send(LivenessFailure { id, pid }).await;
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::testing::{ScriptedProber, probe_config};
    use shep_core::config::ProbeKind;

    /// Generous bound on how long a test may wait for a liveness failure on
    /// the (paused) tokio clock. Costs no real wall-clock time: the paused
    /// runtime auto-advances to this deadline only if nothing else becomes
    /// ready first.
    const FAILURE_WAIT: Duration = Duration::from_secs(120);

    /// The `ProbeTarget` every test below arms against — its contents are
    /// never read: `ScriptedProber::probe` ignores both of its arguments.
    fn target() -> ProbeTarget {
        ProbeTarget::Tcp {
            host: "localhost".to_string(),
            port: 5432,
        }
    }

    /// `config_with_threshold(3)`'s baseline, with `failure_threshold`
    /// overwritten — `probe_config` fixes every other field at
    /// fixture-friendly (production-default) values.
    fn config_with_threshold(threshold: u32) -> ProbeConfig {
        ProbeConfig {
            failure_threshold: threshold,
            ..probe_config(ProbeKind::Tcp, "localhost:5432")
        }
    }

    /// `n` full probe intervals, plus a hair — long enough to be sure `n`
    /// intervals have elapsed and been processed, short enough to stay
    /// strictly before interval `n + 1`. Mirrors `limits::tests::ticks`, but
    /// takes `interval` as a parameter: this loop's cadence is per-config,
    /// not a shared crate-wide constant.
    fn intervals(interval: Duration, n: u32) -> Duration {
        interval * n + Duration::from_millis(1)
    }

    /// Spawns a liveness task and yields once before returning.
    ///
    /// Mirrors `cron::tests::spawn_worker_and_settle` and
    /// `limits::tests::start_and_settle`: a task spawned right before the
    /// clock is driven forward would take its very first `sleep` reading
    /// after that, missing the interval the drive was meant to cross.
    /// Yielding once first lets the loop commit to its initial sleep while
    /// the clock still reads close to "now".
    async fn spawn_and_settle(
        id: u32,
        pid: u32,
        config: ProbeConfig,
        prober: Arc<dyn Prober>,
        failures: mpsc::Sender<LivenessFailure>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = spawn_liveness_task(id, pid, config, target(), prober, failures);
        tokio::task::yield_now().await;
        handle
    }

    async fn expect_failure(rx: &mut mpsc::Receiver<LivenessFailure>, id: u32) -> LivenessFailure {
        match tokio::time::timeout(FAILURE_WAIT, rx.recv()).await {
            Ok(Some(failure)) => failure,
            Ok(None) => panic!("failures channel closed before a failure for id {id} arrived"),
            Err(_) => panic!("timed out waiting for a liveness failure for id {id}"),
        }
    }

    /// Waits up to `window` for a liveness failure, panicking if one
    /// arrives.
    ///
    /// A bounded `timeout` + `recv`, not a bare `try_recv` (Global
    /// Constraints rule 11): right after the clock is driven forward, a
    /// message already due has not necessarily reached this receiver's
    /// queue yet, so a bare `try_recv` reads `Err(Empty)` regardless of
    /// whether the loop under test is correct — it cannot fail, so it
    /// guards nothing.
    async fn assert_no_failure_within(rx: &mut mpsc::Receiver<LivenessFailure>, window: Duration) {
        match tokio::time::timeout(window, rx.recv()).await {
            Err(_) => {} // window elapsed with nothing arriving — expected
            Ok(Some(failure)) => panic!("unexpected liveness failure observed: {failure:?}"),
            Ok(None) => panic!("failures channel disconnected while checking for no failure"),
        }
    }

    /// Dyn-compatibility smoke test (IR-10): fails to compile the moment
    /// somebody adds a generic (non-dyn-safe) method to `Prober`.
    #[test]
    fn prober_is_dyn_compatible() {
        let _: &dyn Prober = &ScriptedProber::new(vec![]);
    }

    // fails if the threshold check is off by one (`>` instead of `>=`), and
    // fails if the loop probes on a `tokio::time::interval` grid instead of
    // sleeping the full `interval` between one probe's completion and the
    // next — a grid loop fires its first probe at t=0 and its third at
    // 2×interval, not 3×interval.
    #[tokio::test(start_paused = true)]
    async fn three_consecutive_failures_report_at_exactly_three_intervals() {
        let config = config_with_threshold(3);
        let interval = config.interval.as_duration();
        let timeout = config.timeout.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![
            Err(ProbeFailure::Timeout),
            Err(ProbeFailure::Timeout),
            Err(ProbeFailure::Timeout),
        ]));
        let (tx, mut rx) = mpsc::channel(8);
        let start = tokio::time::Instant::now();
        let _handle =
            spawn_and_settle(1, 100, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

        let failure = expect_failure(&mut rx, 1).await;

        assert_eq!(failure, LivenessFailure { id: 1, pid: 100 });
        assert_eq!(
            tokio::time::Instant::now() - start,
            interval * 3,
            "liveness failure should be reported at exactly the third probe, not before or after"
        );
        // fails if `spawn_liveness_task` wires the wrong argument into
        // `prober.probe(&target, ..)` — e.g. `interval` where `timeout`
        // belongs, or `Duration::ZERO` — since nothing else exercises this
        // parameter: `ScriptedProber` ignores it for every other purpose,
        // and `OsProber` (Task 8) is tested standalone, never through this
        // loop.
        assert_eq!(
            prober.last_timeout(),
            timeout,
            "the loop must pass config.timeout, not some other duration, to Prober::probe"
        );
    }

    // No separate "a pass resets the counter" test: an earlier version of
    // this test ran the same `[Fail, Fail, Pass, Fail, Fail]` timeline as
    // `counter_re_accumulates_after_a_reset_and_trips_on_the_sixth_probe`
    // below and asserted only "no failure within 5 intervals." That claim
    // is a strict corollary of the test below's — which pins the failure to
    // *exactly* interval 6 — not an independent one: `ScriptedProber`
    // repeats its last scripted outcome (`Fail`) forever, so this timeline
    // was never going to stay silent past interval 6 either way, and the
    // 5-interval bound was really just "however far below 6 stays true," a
    // fact the removed test never stated. Mutation testing already showed
    // the equivalence directly — removing the counter's reset broke *both*
    // the old absence check (an unexpected failure arrived early) and this
    // test's exact-instant assertion (tripped at 4×interval, not 6×) for
    // the same single-line bug. A test whose failure mode is a strictly
    // weaker read of a fact this one already pins exactly earns deletion
    // instead of a documentation-only note in the brief.
    //
    // fails if the counter resets on a pass but then double-counts
    // afterward, or if a reset counter never re-arms and the loop stops
    // checking the threshold at all
    #[tokio::test(start_paused = true)]
    async fn counter_re_accumulates_after_a_reset_and_trips_on_the_sixth_probe() {
        let config = config_with_threshold(3);
        let interval = config.interval.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![
            Err(ProbeFailure::Timeout),
            Err(ProbeFailure::Timeout),
            Ok(()),
            Err(ProbeFailure::Timeout),
            Err(ProbeFailure::Timeout),
            Err(ProbeFailure::Timeout),
        ]));
        let (tx, mut rx) = mpsc::channel(8);
        let start = tokio::time::Instant::now();
        let _handle =
            spawn_and_settle(3, 300, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

        let failure = expect_failure(&mut rx, 3).await;

        assert_eq!(failure, LivenessFailure { id: 3, pid: 300 });
        assert_eq!(
            tokio::time::Instant::now() - start,
            interval * 6,
            "the sixth probe (three failures after the reset) should be the one that trips"
        );
    }

    // fails if the comparison is `>` where `>=` belongs: with a threshold of
    // 1, `>` would need a SECOND failure to trip. `ScriptedProber` repeats
    // its last scripted outcome forever, so a bare "did a failure ever
    // arrive" assertion would not catch this — the second failure the `>`
    // mutation waits for is just as available as the first. Pinning the
    // `Instant` at exactly one interval is what makes the difference
    // observable.
    #[tokio::test(start_paused = true)]
    async fn threshold_of_one_reports_after_a_single_failure() {
        let config = config_with_threshold(1);
        let interval = config.interval.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]));
        let (tx, mut rx) = mpsc::channel(8);
        let start = tokio::time::Instant::now();
        let _handle =
            spawn_and_settle(4, 400, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

        let failure = expect_failure(&mut rx, 4).await;

        assert_eq!(failure, LivenessFailure { id: 4, pid: 400 });
        assert_eq!(
            tokio::time::Instant::now() - start,
            interval,
            "threshold of one must report after exactly one probe, not two"
        );
    }

    // fails if the loop keeps probing a sheep it has already declared dead
    // instead of ending once it has reported
    #[tokio::test(start_paused = true)]
    async fn no_further_probing_after_a_failure_is_reported() {
        let config = config_with_threshold(1);
        let interval = config.interval.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]));
        let (tx, mut rx) = mpsc::channel(8);
        // A held clone, not the moved original: `failures` is documented as
        // a shared sender cloned once per arming, and this loop DOES report
        // and end partway through this test, so without a spare clone kept
        // alive here, the channel would close the instant the loop's own
        // sender drops, and the `assert_no_failure_within` below would
        // observe `Ok(None)` (disconnected) instead of a timeout — "channel
        // closed" and "loop correctly silent" must stay distinguishable,
        // and only a live spare sender keeps them so.
        let _handle = spawn_and_settle(
            5,
            500,
            config,
            Arc::clone(&prober) as Arc<dyn Prober>,
            tx.clone(),
        )
        .await;

        expect_failure(&mut rx, 5).await;
        let calls_after_report = prober.calls();

        // A bounded `timeout` clearing the final deadline by a hair, per
        // Global Constraints rule 11 — an explicit `advance` would resolve
        // only the one sleep already pending and could not observe whether
        // the loop issued (and this counter recorded) a probe it should not
        // have.
        assert_no_failure_within(&mut rx, intervals(interval, 3)).await;
        assert_eq!(
            prober.calls(),
            calls_after_report,
            "prober must not be called again after the loop has reported and ended"
        );
    }

    // fails if the loop probes on a `tokio::time::interval` grid instead of
    // sleeping the full `interval` after each probe resolves: a probe that
    // takes 2×interval to resolve would, on that grid, fire every overdue
    // tick back-to-back (`MissedTickBehavior::Burst`) and reach 7 calls in
    // this span, not 4
    #[tokio::test(start_paused = true)]
    async fn a_slow_probe_still_paces_by_interval_after_completion() {
        let config = config_with_threshold(3); // never reached: every probe passes
        let interval = config.interval.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![Ok(())]).with_delay(interval * 2));
        let (tx, mut rx) = mpsc::channel(8);
        let _handle =
            spawn_and_settle(6, 600, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

        // Cross the span with a bounded timeout that clears the twelfth
        // interval's deadline by a hair, per Global Constraints rule 11 —
        // not `12 × advance(interval)`, which would resolve only the one
        // sleep already pending on each call and undercount every probe
        // this loop actually issued in between.
        assert_no_failure_within(&mut rx, intervals(interval, 12)).await;
        assert_eq!(prober.calls(), 4);
    }

    // fails if the task outlives its sheep: probing continues even after
    // the handle is aborted
    #[tokio::test(start_paused = true)]
    async fn aborting_the_handle_stops_probing() {
        let config = config_with_threshold(1000); // high enough it never trips
        let interval = config.interval.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]));
        let (tx, _rx) = mpsc::channel(8);
        let handle =
            spawn_and_settle(7, 700, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

        tokio::time::sleep(intervals(interval, 2)).await;
        let calls_before_abort = prober.calls();
        assert!(
            calls_before_abort > 0,
            "expected at least one probe before abort"
        );

        handle.abort();

        tokio::time::sleep(intervals(interval, 3)).await;
        assert_eq!(
            prober.calls(),
            calls_before_abort,
            "prober must not be called again once the handle has been aborted"
        );
    }

    // fails if a zero (or otherwise sub-floor) `interval` is trusted instead
    // of clamped to `MIN_PROBE_INTERVAL`: an unclamped `Duration::ZERO`
    // would spin the loop as fast as the runtime allows — measured live at
    // roughly 380 probes/sec, which for `ProbeKind::Exec` means that many
    // process spawns per second, per sheep, forever. `shep-core::normalize`
    // rejects an explicit `interval = "0"` in a Flockfile, but this test
    // constructs a `ProbeConfig` directly (as a caller that skipped
    // normalization, or a future boot path with a bug, would) to prove this
    // loop does not simply trust that every caller validated first.
    //
    // Confirmed live (mutation testing, not just written down): with the
    // clamp removed, this test does not fail — it hangs. A `Duration::ZERO`
    // sleep resolves the instant it is polled, on a paused clock exactly as
    // on a real one, so the loop never actually parks on a pending timer for
    // the runtime's auto-advance to fast-forward past; it just spins,
    // consuming real CPU forever. The backstop for that mutation is the CI
    // job's own timeout, the same shape `cron::tests::
    // exhausted_pattern_ends_the_task_without_restarting` already documents
    // for its own busy-spin mutation.
    #[tokio::test(start_paused = true)]
    async fn a_zero_interval_is_clamped_instead_of_hot_spinning() {
        let config = ProbeConfig {
            interval: shep_core::values::UpDuration::from_millis(0),
            ..config_with_threshold(1000) // high enough it never trips
        };
        let prober = Arc::new(ScriptedProber::new(vec![Ok(())]));
        let (tx, _rx) = mpsc::channel(8);
        let _handle =
            spawn_and_settle(8, 800, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

        // Cross exactly 5 floored intervals; a hot-spinning loop would rack
        // up far more than 5 calls in this same span.
        tokio::time::sleep(intervals(MIN_PROBE_INTERVAL, 5)).await;
        assert_eq!(
            prober.calls(),
            5,
            "interval must be clamped to MIN_PROBE_INTERVAL, not trusted as-is"
        );
    }
}
