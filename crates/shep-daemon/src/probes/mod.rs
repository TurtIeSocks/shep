//! The `Prober` seam and the liveness probe loop (spec §7).
//!
//! `spawn_liveness_task` polls one sheep every `ProbeConfig::interval` and
//! reports once `ProbeConfig::failure_threshold` consecutive probes fail,
//! then ends; a restarted sheep gets a new loop, and a wedged app is noticed
//! at most `failure_threshold * interval` late. No TLS or redirects;
//! `timeout` enforcement is [`Prober`]'s own job.
//!
//! Readiness picks one source: `wait_ready`, then `readiness_probe`, then
//! neither (online at spawn). `failure_threshold` gates liveness only: a
//! readiness timeout still goes `online` with the budget untouched, unlike
//! liveness, which restarts and resets it.

pub(crate) mod os;
pub(crate) mod ready;

use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use std::sync::Arc;

use tokio::sync::mpsc;

use shep_core::config::{ProbeConfig, ProbeTarget};

/// Why a probe did not pass.
///
/// Growth is expected: each new probe transport brings its own failure modes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    /// The probe did not finish inside `ProbeConfig::timeout`.
    Timeout,
    /// The transport failed before a verdict was possible (connection
    /// refused, DNS failure, the command could not be spawned). Carries the
    /// rendered reason.
    Transport(String),
    /// The probe completed and the answer was negative: a non-2xx status, or a
    /// non-zero exit. Carries the status or exit code.
    Rejected(String),
}

// A boxed future rather than RPITIT: `Arc<dyn Prober>` is how the engine
// holds this, and RPITIT is not dyn-compatible.
/// Runs one probe against one target.
///
/// `spawn_liveness_task` awaits [`Self::probe`] directly with no
/// `tokio::time::timeout` wrapper, so a `probe` future that never resolves
/// stalls that sheep's liveness loop forever. Every implementor must itself
/// guarantee `probe` resolves within `timeout` on every path.
///
/// Public, with [`ProbeFailure`], because `tests/external_impls.rs`
/// implements it from outside this crate.
pub trait Prober: Send + Sync + 'static {
    /// Probes `target`, giving up after `timeout`.
    ///
    /// # Errors
    ///
    /// - [`ProbeFailure::Timeout`]: the probe did not finish inside
    ///   `timeout`.
    /// - [`ProbeFailure::Transport`]: the transport failed before a verdict
    ///   was possible (connection refused, DNS failure, the command could
    ///   not be spawned).
    /// - [`ProbeFailure::Rejected`]: the probe completed and the answer was
    ///   negative (a non-2xx status, or a non-zero exit).
    fn probe<'a>(
        &'a self,
        target: &'a ProbeTarget,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProbeFailure>> + Send + 'a>>;
}

/// Floor `spawn_liveness_task` enforces on `interval`, regardless of caller.
///
/// `shep-core`'s `normalize` already rejects a smaller `interval` in a
/// Flockfile, but that guard covers only callers that route through it.
/// Without this floor, an unvalidated `Duration::ZERO` would hot-spin the
/// loop (measured ~380 probes/second).
///
/// Same value as `shep-core`'s own floor, declared separately for the same
/// reason `MIN_CRON_SLEEP`/`cron::MIN_MAX_SLEEP` are: this crate must not
/// depend on that one.
const MIN_PROBE_INTERVAL: Duration = Duration::from_millis(1_000);

/// A sheep whose liveness probe hit `failure_threshold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LivenessFailure {
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
pub(crate) fn spawn_liveness_task(
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
            // Measured from the previous probe's completion, not a
            // `tokio::time::interval` grid: a grid's default
            // `MissedTickBehavior::Burst` would fire every overdue tick
            // back-to-back once a probe outlasts its own period.
            tokio::time::sleep(interval).await;

            match prober.probe(&target, timeout).await {
                Ok(()) => consecutive_failures = 0,
                Err(_failure) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= threshold {
                        // The receiver may already be gone; this loop's job
                        // ends either way, so the send result is discarded.
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

    /// The `ProbeTarget` every test below arms against; its contents are
    /// never read since `ScriptedProber::probe` ignores both arguments.
    fn target() -> ProbeTarget {
        ProbeTarget::Tcp {
            host: "localhost".to_string(),
            port: 5432,
        }
    }

    /// `probe_config`'s fixture-friendly baseline, with `failure_threshold`
    /// overwritten.
    fn config_with_threshold(threshold: u32) -> ProbeConfig {
        ProbeConfig {
            failure_threshold: threshold,
            ..probe_config(ProbeKind::Tcp, "localhost:5432")
        }
    }

    /// `n` full probe intervals, plus a hair: long enough to be sure `n`
    /// intervals have elapsed, short enough to stay strictly before
    /// interval `n + 1`.
    fn intervals(interval: Duration, n: u32) -> Duration {
        interval * n + Duration::from_millis(1)
    }

    /// Spawns a liveness task and yields once before returning.
    ///
    /// A task spawned right before the clock is driven forward would take
    /// its first `sleep` reading after that, missing the interval the
    /// drive was meant to cross. Yielding once first lets the loop commit
    /// to its initial sleep while the clock still reads close to "now".
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
    /// A bounded `timeout` + `recv`, not a bare `try_recv`: right after the
    /// clock is driven forward, a message already due has not necessarily
    /// reached this receiver's queue yet, so `try_recv` would read
    /// `Err(Empty)` regardless of whether the loop under test is correct.
    async fn assert_no_failure_within(rx: &mut mpsc::Receiver<LivenessFailure>, window: Duration) {
        match tokio::time::timeout(window, rx.recv()).await {
            Err(_) => {} // window elapsed with nothing arriving, expected
            Ok(Some(failure)) => panic!("unexpected liveness failure observed: {failure:?}"),
            Ok(None) => panic!("failures channel disconnected while checking for no failure"),
        }
    }

    #[test]
    fn prober_is_dyn_compatible() {
        let _: &dyn Prober = &ScriptedProber::new(vec![]);
    }

    // Also catches an off-by-one threshold check (`>` instead of `>=`).
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
        // Nothing else exercises this argument: `ScriptedProber` ignores it
        // otherwise, and `OsProber` is tested standalone.
        assert_eq!(
            prober.last_timeout(),
            timeout,
            "the loop must pass config.timeout, not some other duration, to Prober::probe"
        );
    }

    // No separate "a pass resets the counter" test: pinning the failure to
    // exactly interval 6 already implies it.
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

    // `ScriptedProber` repeats its last outcome forever, so only pinning the
    // `Instant` (not just that a failure arrives) catches `>` where `>=`
    // belongs.
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

    #[tokio::test(start_paused = true)]
    async fn no_further_probing_after_a_failure_is_reported() {
        let config = config_with_threshold(1);
        let interval = config.interval.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]));
        let (tx, mut rx) = mpsc::channel(8);
        // A held clone: this loop's own sender drops once it reports, and a
        // dropped channel would read as `Ok(None)` below, indistinguishable
        // from a correctly silent loop.
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

        assert_no_failure_within(&mut rx, intervals(interval, 3)).await;
        assert_eq!(
            prober.calls(),
            calls_after_report,
            "prober must not be called again after the loop has reported and ended"
        );
    }

    // A grid loop would fire every overdue tick back-to-back and reach 7
    // calls in this span, not 4.
    #[tokio::test(start_paused = true)]
    async fn a_slow_probe_still_paces_by_interval_after_completion() {
        let config = config_with_threshold(3); // never reached: every probe passes
        let interval = config.interval.as_duration();
        let prober = Arc::new(ScriptedProber::new(vec![Ok(())]).with_delay(interval * 2));
        let (tx, mut rx) = mpsc::channel(8);
        let _handle =
            spawn_and_settle(6, 600, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

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

    // Pins that `timeout` is honoured unclamped (not capped to `interval`)
    // and that a slow probe does not make the loop fire the next one early.
    #[tokio::test(start_paused = true)]
    async fn a_timeout_longer_than_the_interval_is_honoured_and_paces_the_next_probe() {
        let config = ProbeConfig {
            interval: shep_core::values::UpDuration::from_millis(2_000),
            timeout: shep_core::values::UpDuration::from_millis(5_000),
            ..config_with_threshold(3)
        };
        let interval = config.interval.as_duration();
        let timeout = config.timeout.as_duration();
        // A probe that takes its whole timeout to answer is what makes the
        // ordering visible: one cycle then costs `interval + timeout`, a
        // number neither duration produces on its own.
        let prober =
            Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]).with_delay(timeout));
        let (tx, mut rx) = mpsc::channel(8);
        let start = tokio::time::Instant::now();
        let _handle =
            spawn_and_settle(9, 900, config, Arc::clone(&prober) as Arc<dyn Prober>, tx).await;

        let failure = expect_failure(&mut rx, 9).await;

        assert_eq!(failure, LivenessFailure { id: 9, pid: 900 });
        assert_eq!(
            tokio::time::Instant::now() - start,
            (interval + timeout) * 3,
            "three probes that each sleep `interval` and then run for `timeout` must report \
             at 3 x (interval + timeout)"
        );
        assert_eq!(
            prober.last_timeout(),
            timeout,
            "the loop must pass config.timeout unclamped, even where it exceeds interval"
        );
        assert_eq!(prober.calls(), 3);
    }

    // Constructs `ProbeConfig` directly, bypassing `shep-core::normalize`'s
    // rejection, to prove this loop clamps independently.
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
