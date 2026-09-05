//! The kill ladder: graceful stop escalating to `SIGKILL`
//!
//! [`kill_process`] runs inside the sheep task that owns the live
//! [`RunningProcess`], and is generic over that trait so one ladder drives
//! both the engine tests' fake and the real child. Portable: it touches only
//! [`StopSignal`] and the trait, never an OS signal API.

use core::time::Duration;

use tokio::sync::mpsc;

use shep_core::config::{AppConfig, KillSignal};

use crate::channel::ShepherdMessage;
use crate::runner::{ExitOutcome, RunningProcess, StopSignal};

/// Runs the stop ladder against one live process and returns its exit outcome.
///
/// `app.shutdown_with_message` with a `to_child` present sends
/// [`ShepherdMessage::Shutdown`]; otherwise the app's [`StopSignal`] goes to
/// the sheep's whole process group, lambs included. After `grace`,
/// [`RunningProcess::kill_tree`] sweeps that same group again.
///
/// `grace` is the caller's, not the app's: `kill_timeout` for an ordinary
/// stop, the longer `graceful_timeout` for a reload's drain. Delivery
/// failures are logged and never panic, so the caller always gets a terminal
/// [`ExitOutcome`].
// `kill_tree` is not redundant: the first rung's signal is catchable,
// `shutdown_with_message` sends none, and a fork born after it never saw
// it. A lamb outliving the sheep also skips this rung: `proc.wait()`
// already resolved on the leader's exit, so `kill_tree` is never called.
pub async fn kill_process<P: RunningProcess>(
    proc: &mut P,
    app: &AppConfig,
    to_child: Option<&mpsc::Sender<ShepherdMessage>>,
    grace: Duration,
) -> ExitOutcome {
    if app.shutdown_with_message
        && let Some(tx) = to_child
    {
        if let Err(err) = tx.send(ShepherdMessage::Shutdown).await {
            tracing::warn!(pid = proc.pid(), error = %err, "shepherd-channel shutdown message delivery failed");
        }
    } else if let Err(err) = proc.signal(stop_signal(app)) {
        tracing::warn!(pid = proc.pid(), error = %err, "stop signal delivery failed");
    }

    match tokio::time::timeout(grace, proc.wait()).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            if let Err(err) = proc.kill_tree() {
                tracing::warn!(pid = proc.pid(), error = %err, "SIGKILL tree delivery failed");
            }
            proc.wait().await
        }
    }
}

/// Maps `app.kill_signal` onto a [`StopSignal`]; unset defaults to `SIGTERM`.
///
/// Total over [`KillSignal`]: a config that reached the daemon came through
/// `normalize`, which refuses any name this cannot map. The unparseable
/// branch below is therefore a wiring bug in the daemon rather than anything
/// an operator can fix, and logs at `error!` for that reason.
fn stop_signal(app: &AppConfig) -> StopSignal {
    let Some(name) = app.kill_signal.as_deref() else {
        return StopSignal::Term;
    };
    let Some(signal) = KillSignal::parse(name) else {
        tracing::error!(
            kill_signal = name,
            "kill_signal reached the stop ladder unvalidated; normalize should have refused it. \
             Falling back to SIGTERM"
        );
        return StopSignal::Term;
    };
    match signal {
        KillSignal::Term => StopSignal::Term,
        KillSignal::Int => StopSignal::Int,
        KillSignal::Quit => StopSignal::Quit,
        KillSignal::Usr2 => StopSignal::Usr2,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use tokio::time::{Duration, Instant};

    use super::*;
    use crate::channel::ShepherdMessage;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::runner::{ProcessRunner, SpawnSpec};
    use crate::testing::capture_logs;

    /// The cap an ordinary stop passes: the app's own `kill_timeout`.
    fn stop_grace(app: &AppConfig) -> Duration {
        app.kill_timeout.as_duration()
    }

    fn spec() -> SpawnSpec {
        SpawnSpec {
            name: "web".to_string(),
            program: "/bin/true".to_string(),
            args: vec![],
            cwd: None,
            env: BTreeMap::new(),
            out_file: PathBuf::from("/tmp/shep-kill-test-out.log"),
            err_file: PathBuf::from("/tmp/shep-kill-test-err.log"),
            channel: true,
            stdin: false,
            credentials: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn obedient_process_exits_on_signal_without_kill_tree() {
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        let app = AppConfig::minimal("web", "./srv");

        let start = Instant::now();
        let outcome = kill_process(&mut proc, &app, None, stop_grace(&app)).await;
        let elapsed = start.elapsed();

        assert_eq!(outcome.signal, Some(15));
        assert_eq!(runner.kill_counts(), vec![0]);
        assert_eq!(elapsed, Duration::from_millis(0));
    }

    #[tokio::test(start_paused = true)]
    async fn defiant_process_is_killed_after_the_full_kill_timeout() {
        let runner = ScriptedRunner::new(vec![ProcScript::ignores_signals()]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        let app = AppConfig::minimal("web", "./srv"); // default kill_timeout = 1600ms

        let start = Instant::now();
        let outcome = kill_process(&mut proc, &app, None, stop_grace(&app)).await;
        let elapsed = start.elapsed();

        assert_eq!(elapsed, Duration::from_millis(1600));
        assert_eq!(runner.kill_counts(), vec![1]);
        assert_eq!(outcome.signal, Some(9));
    }

    // fails if the ladder reads `kill_timeout` off the app instead of the cap
    // its caller passed, SIGKILLing a drain five seconds early
    #[tokio::test(start_paused = true)]
    async fn a_drain_waits_the_caps_it_was_given_not_the_apps_kill_timeout() {
        let runner = ScriptedRunner::new(vec![ProcScript::ignores_signals()]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        // default kill_timeout = 1600ms, default graceful_timeout = 8000ms
        let app = AppConfig::minimal("web", "./srv");
        let grace = app.graceful_timeout.as_duration();
        assert_ne!(grace, stop_grace(&app), "the two caps must differ to test");

        let start = Instant::now();
        let outcome = kill_process(&mut proc, &app, None, grace).await;

        assert_eq!(start.elapsed(), Duration::from_millis(8000));
        assert_eq!(runner.kill_counts(), vec![1]);
        assert_eq!(outcome.signal, Some(9));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_with_message_sends_shutdown_instead_of_signal() {
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (mut proc, io) = runner.spawn(&spec()).unwrap();
        let mut fake_io = runner.io_handles(0);
        let mut app = AppConfig::minimal("web", "./srv");
        app.shutdown_with_message = true;

        let outcome = kill_process(&mut proc, &app, Some(&io.to_child), stop_grace(&app)).await;

        // The fake resolves an `obeys_signal` wait to Term (15) on a
        // `Shutdown` message too, but records no `signal()` call.
        assert_eq!(outcome.signal, Some(15));
        assert!(runner.signals(0).is_empty());

        let observed = fake_io.to_child_rx.recv().await.unwrap();
        assert_eq!(observed, ShepherdMessage::Shutdown);
    }

    #[tokio::test(start_paused = true)]
    async fn custom_kill_signal_sends_sigint() {
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        let mut app = AppConfig::minimal("web", "./srv");
        app.kill_signal = Some("SIGINT".to_string());

        let outcome = kill_process(&mut proc, &app, None, stop_grace(&app)).await;

        assert_eq!(outcome.signal, Some(2));
        assert_eq!(runner.signals(0), vec![2]);
    }

    #[test]
    fn stop_signal_defaults_to_term_when_unset() {
        let app = AppConfig::minimal("web", "./srv");
        assert_eq!(stop_signal(&app), StopSignal::Term);
    }

    #[test]
    fn stop_signal_parses_known_names_case_insensitively() {
        let cases = [
            ("SIGTERM", StopSignal::Term),
            ("term", StopSignal::Term),
            ("SIGINT", StopSignal::Int),
            ("int", StopSignal::Int),
            ("SIGQUIT", StopSignal::Quit),
            ("quit", StopSignal::Quit),
            ("SIGUSR2", StopSignal::Usr2),
            ("usr2", StopSignal::Usr2),
        ];
        for (name, want) in cases {
            let mut app = AppConfig::minimal("web", "./srv");
            app.kill_signal = Some(name.to_string());
            assert_eq!(stop_signal(&app), want, "kill_signal={name}");
        }
    }

    #[test]
    fn an_unvalidated_kill_signal_reaching_the_ladder_falls_back_to_term_and_logs_loudly() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.kill_signal = Some("SIGBOGUS".to_string());

        let mut signal = None;
        let logs = capture_logs(|| {
            signal = Some(stop_signal(&app));
        });

        assert_eq!(signal, Some(StopSignal::Term));
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("SIGBOGUS"), "{logs}");
    }
}
