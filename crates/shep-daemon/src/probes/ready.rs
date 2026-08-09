//! `ReadinessSource` and `await_ready` — the gate between `starting` and
//! `online` (spec §7).
//!
//! A sheep whose app configures `wait_ready` or `readiness_probe` does not
//! reach `online` the instant it spawns: the supervisor holds it at
//! `starting` until [`await_ready`] resolves — the shepherd channel says
//! `{"kind":"ready"}`, or a probe first passes. [`ReadinessSource::
//! Heuristic`] exists and is fully implemented here, but is reachable only
//! from reload's `AwaitReady` state, not from `start` — see the supervisor's
//! own gating logic for why (spec §7 gates reload, not every plain start).
//!
//! ## Reference
//!
//! - [`ReadinessSource`], [`Readiness`], [`await_ready`]

use core::time::Duration;

use std::sync::Arc;

use tokio::sync::oneshot;

use shep_core::config::{AppConfig, ProbeConfig, ProbeTarget, ProbeTargetError};

use super::Prober;

/// Where a sheep's readiness signal comes from (spec §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessSource {
    /// `wait_ready = true`: the shepherd channel's `{"kind":"ready"}`.
    Channel,
    /// `readiness_probe` is set: the first passing probe.
    Probe(ProbeConfig, ProbeTarget),
    /// Neither is configured: readiness is the deadline elapsing.
    Heuristic,
}

impl ReadinessSource {
    /// Derives the source from an app's configuration.
    ///
    /// `wait_ready` wins over `readiness_probe` when both are set: the
    /// channel is the app telling us directly, and a probe is an outside
    /// guess at the same fact.
    ///
    /// # Errors
    ///
    /// Whatever [`ProbeTarget::parse`] returns for the app's
    /// `readiness_probe` target — [`ProbeTargetError::Empty`],
    /// [`HttpsUnsupported`](ProbeTargetError::HttpsUnsupported),
    /// [`NotHttpUrl`](ProbeTargetError::NotHttpUrl),
    /// [`MissingHost`](ProbeTargetError::MissingHost),
    /// [`InvalidHost`](ProbeTargetError::InvalidHost),
    /// [`InvalidPath`](ProbeTargetError::InvalidPath),
    /// [`MissingPort`](ProbeTargetError::MissingPort) or
    /// [`BadPort`](ProbeTargetError::BadPort). In practice `normalize` has
    /// already rejected every one of them, so an `Err` here means the caller
    /// adopted an app that never went through `normalize`.
    pub fn of(config: &AppConfig) -> Result<Self, ProbeTargetError> {
        if config.wait_ready {
            return Ok(Self::Channel);
        }
        match &config.readiness_probe {
            Some(probe) => Ok(Self::Probe(probe.clone(), ProbeTarget::parse(probe)?)),
            None => Ok(Self::Heuristic),
        }
    }
}

/// How a readiness wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The signal arrived inside the deadline.
    Ready,
    /// The deadline elapsed with no signal.
    TimedOut,
}

/// Waits for `source`'s readiness signal, giving up after `deadline`.
///
/// `channel` carries the shepherd channel's ready notification and is read
/// only by [`ReadinessSource::Channel`]; a `Probe`/`Heuristic` wait ignores
/// it entirely, so a `{"kind":"ready"}` write against a probe- or
/// heuristic-gated sheep has nothing here to wake.
pub async fn await_ready(
    source: &ReadinessSource,
    deadline: Duration,
    channel: oneshot::Receiver<()>,
    prober: Arc<dyn Prober>,
) -> Readiness {
    // No `_` arm: three sources, three arms, and a fourth source added later
    // fails `cargo check` right here instead of silently taking a default
    // this function never chose (Global Constraints rule 6).
    match source {
        ReadinessSource::Channel => {
            // A dropped sender (the sheep exited before ever signalling)
            // must not resolve this wait early — that is the exit path's
            // job, and deciding here would race it. So a closed channel
            // switches to a future that never resolves, leaving the outer
            // `timeout` below as the only thing that can end the wait, at
            // the deadline it was always going to end at.
            let wait_for_signal = async {
                if channel.await.is_err() {
                    core::future::pending::<()>().await;
                }
            };
            match tokio::time::timeout(deadline, wait_for_signal).await {
                Ok(()) => Readiness::Ready,
                Err(_elapsed) => Readiness::TimedOut,
            }
        }
        ReadinessSource::Probe(config, target) => {
            let interval = config.interval.as_duration();
            let timeout = config.timeout.as_duration();
            // Probe first, sleep after: the first probe must land at t=0,
            // not after one `interval` — an interval-first loop would add a
            // full interval to every gated start. `failure_threshold` is
            // deliberately never read here: it is a liveness concept, and
            // giving up after N failures would turn this wait into a
            // second, smaller deadline nested inside the real one.
            let poll = async {
                loop {
                    if prober.probe(target, timeout).await.is_ok() {
                        return;
                    }
                    tokio::time::sleep(interval).await;
                }
            };
            match tokio::time::timeout(deadline, poll).await {
                Ok(()) => Readiness::Ready,
                Err(_elapsed) => Readiness::TimedOut,
            }
        }
        ReadinessSource::Heuristic => {
            // The elapse IS the signal here — the one branch that returns
            // `Ready` from a deadline racing nothing, rather than from an
            // event beating one.
            tokio::time::sleep(deadline).await;
            Readiness::Ready
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use std::sync::Arc;

    use tokio::sync::oneshot;

    use super::*;
    use crate::probes::ProbeFailure;
    use crate::testing::{ScriptedProber, probe_config};
    use shep_core::config::ProbeKind;
    use shep_core::values::UpDuration;

    /// A prober `Channel`/`Heuristic` waits never call — an empty script
    /// would panic on the first `probe()` call, so using it here proves
    /// those two arms never reach the prober at all.
    fn unused_prober() -> Arc<dyn Prober> {
        Arc::new(ScriptedProber::new(vec![]))
    }

    fn tcp_source(interval_ms: u64) -> ReadinessSource {
        let config = ProbeConfig {
            interval: UpDuration::from_millis(interval_ms),
            ..probe_config(ProbeKind::Tcp, "localhost:5432")
        };
        let target = ProbeTarget::parse(&config).expect("a valid tcp target parses");
        ReadinessSource::Probe(config, target)
    }

    // fails if a wait ignores the channel signal and always runs the full
    // deadline
    #[tokio::test(start_paused = true)]
    async fn channel_ready_before_the_deadline_resolves_at_signal_time() {
        let (tx, rx) = oneshot::channel();
        let start = tokio::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = tx.send(());
        });

        let readiness = await_ready(
            &ReadinessSource::Channel,
            Duration::from_secs(3),
            rx,
            unused_prober(),
        )
        .await;

        assert_eq!(readiness, Readiness::Ready);
        assert_eq!(
            tokio::time::Instant::now() - start,
            Duration::from_millis(500)
        );
    }

    // fails if a wait never gives up, which would hang a start forever
    #[tokio::test(start_paused = true)]
    async fn channel_with_no_signal_times_out_at_exactly_the_deadline() {
        let (_tx, rx) = oneshot::channel(); // held: never signals, never drops
        let start = tokio::time::Instant::now();
        let deadline = Duration::from_secs(3);

        let readiness = await_ready(&ReadinessSource::Channel, deadline, rx, unused_prober()).await;

        assert_eq!(readiness, Readiness::TimedOut);
        assert_eq!(tokio::time::Instant::now() - start, deadline);
    }

    // fails if a closed channel is treated as a decision instead of the exit
    // path's own signal: a sheep that died is handled elsewhere, and
    // resolving early here would race it
    #[tokio::test(start_paused = true)]
    async fn channel_dropped_without_signalling_times_out_at_the_deadline_not_immediately() {
        let (tx, rx) = oneshot::channel();
        drop(tx);
        let start = tokio::time::Instant::now();
        let deadline = Duration::from_secs(3);

        let readiness = await_ready(&ReadinessSource::Channel, deadline, rx, unused_prober()).await;

        assert_eq!(readiness, Readiness::TimedOut);
        assert_eq!(tokio::time::Instant::now() - start, deadline);
    }

    // fails if failure_threshold — a liveness concept — is applied here,
    // giving up after three failures instead of riding out the deadline
    #[tokio::test(start_paused = true)]
    async fn probe_becomes_ready_two_failures_then_a_pass_at_two_intervals() {
        let source = tcp_source(1_000);
        let (_tx, rx) = oneshot::channel();
        let prober = Arc::new(ScriptedProber::new(vec![
            Err(ProbeFailure::Timeout),
            Err(ProbeFailure::Timeout),
            Ok(()),
        ]));
        let start = tokio::time::Instant::now();

        let readiness = await_ready(&source, Duration::from_secs(10), rx, prober).await;

        assert_eq!(readiness, Readiness::Ready);
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(2));
    }

    // fails if the loop sleeps one interval before its first probe — an
    // interval-first loop adds a full interval to every gated start
    #[tokio::test(start_paused = true)]
    async fn probe_ready_immediately_when_the_first_probe_passes() {
        let source = tcp_source(1_000);
        let (_tx, rx) = oneshot::channel();
        let prober = Arc::new(ScriptedProber::new(vec![Ok(())]));
        let start = tokio::time::Instant::now();

        let readiness = await_ready(&source, Duration::from_secs(10), rx, prober).await;

        assert_eq!(readiness, Readiness::Ready);
        assert_eq!(
            tokio::time::Instant::now(),
            start,
            "the first probe must run at t=0, not after one interval"
        );
    }

    // fails if a probe wait has no deadline of its own
    #[tokio::test(start_paused = true)]
    async fn probe_that_always_fails_times_out_at_the_deadline() {
        let source = tcp_source(1_000);
        let (_tx, rx) = oneshot::channel();
        let prober = Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]));
        let start = tokio::time::Instant::now();
        let deadline = Duration::from_secs(3);

        let readiness = await_ready(&source, deadline, rx, prober).await;

        assert_eq!(readiness, Readiness::TimedOut);
        assert_eq!(tokio::time::Instant::now() - start, deadline);
    }

    // fails if this returns TimedOut instead of Ready — the heuristic's own
    // elapse IS the readiness signal, and the two verdicts drive different
    // log lines at the call site
    #[tokio::test(start_paused = true)]
    async fn heuristic_becomes_ready_when_its_deadline_elapses() {
        let (_tx, rx) = oneshot::channel();
        let start = tokio::time::Instant::now();
        let deadline = Duration::from_secs(3);

        let readiness =
            await_ready(&ReadinessSource::Heuristic, deadline, rx, unused_prober()).await;

        assert_eq!(readiness, Readiness::Ready);
        assert_eq!(tokio::time::Instant::now() - start, deadline);
    }

    // --- ReadinessSource::of ---

    // fails if readiness_probe is consulted before wait_ready, so an app
    // configuring both would probe instead of waiting on its own explicit
    // signal
    #[test]
    fn wait_ready_wins_over_a_configured_probe() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.wait_ready = true;
        app.readiness_probe = Some(probe_config(ProbeKind::Tcp, "localhost:5432"));

        assert_eq!(ReadinessSource::of(&app), Ok(ReadinessSource::Channel));
    }

    // fails if a configured probe is ignored when wait_ready is unset
    #[test]
    fn a_configured_probe_is_used_when_wait_ready_is_unset() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.readiness_probe = Some(probe_config(ProbeKind::Tcp, "localhost:5432"));
        let target = ProbeTarget::parse(app.readiness_probe.as_ref().unwrap()).unwrap();

        assert_eq!(
            ReadinessSource::of(&app),
            Ok(ReadinessSource::Probe(
                app.readiness_probe.clone().unwrap(),
                target
            ))
        );
    }

    // fails if an app configuring neither is treated as gated
    #[test]
    fn neither_configured_is_heuristic() {
        let app = AppConfig::minimal("web", "./srv");
        assert_eq!(ReadinessSource::of(&app), Ok(ReadinessSource::Heuristic));
    }

    // fails if a malformed readiness_probe target is silently accepted here
    // instead of surfacing ProbeTarget::parse's own rejection
    #[test]
    fn a_malformed_probe_target_is_rejected() {
        let mut app = AppConfig::minimal("web", "./srv");
        let bad_probe = probe_config(ProbeKind::Http, "not-a-url");
        app.readiness_probe = Some(bad_probe.clone());

        let expected = ProbeTarget::parse(&bad_probe).unwrap_err();
        assert_eq!(ReadinessSource::of(&app), Err(expected));
    }
}
