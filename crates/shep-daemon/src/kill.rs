//! The kill ladder: graceful stop escalating to `SIGKILL`
//!
//! [`kill_process`] runs inside the sheep task that owns the live [`RunningProcess`]
//! (Task 9's per-sheep task). It is generic over [`RunningProcess`] so the same
//! ladder drives both [`crate::fake::FakeProc`] in engine tests and the real
//! `tokio_runner` child in production — this module itself stays portable
//! (no `cfg(unix)`) since it only touches the portable [`StopSignal`] enum and
//! the [`RunningProcess`] trait, never OS signal APIs directly.

use tokio::sync::mpsc;

use shep_core::config::AppConfig;

use crate::channel::ShepherdMessage;
use crate::runner::{ExitOutcome, RunningProcess, StopSignal};

/// Runs the stop ladder against one live process and returns its exit outcome.
///
/// 1. If `app.shutdown_with_message` is set and `to_child` is present, sends
///    [`ShepherdMessage::Shutdown`] over the shepherd channel; otherwise sends
///    the app's configured [`StopSignal`] (resolved by the private
///    `stop_signal` parser from `app.kill_signal`).
/// 2. Waits up to `app.kill_timeout` for the process to exit.
/// 3. On timeout, SIGKILLs the whole process tree and waits for that to land.
///
/// Delivery failures (message send, signal, `kill_tree`) are logged and never
/// panic: the caller always gets a terminal [`ExitOutcome`], even if every
/// delivery step failed and the timeout simply ran out the clock.
pub async fn kill_process<P: RunningProcess>(
    proc: &mut P,
    app: &AppConfig,
    to_child: Option<&mpsc::Sender<ShepherdMessage>>,
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

    match tokio::time::timeout(app.kill_timeout.as_duration(), proc.wait()).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            if let Err(err) = proc.kill_tree() {
                tracing::warn!(pid = proc.pid(), error = %err, "SIGKILL tree delivery failed");
            }
            proc.wait().await
        }
    }
}

/// Parses `app.kill_signal` into a [`StopSignal`]; unset defaults to `SIGTERM`.
///
/// Recognized names (case-insensitive, with or without the `SIG` prefix):
/// `SIGTERM`/`TERM`, `SIGINT`/`INT`, `SIGQUIT`/`QUIT`, `SIGUSR2`/`USR2`. An
/// unrecognized name falls back to `SIGTERM` and logs a warning rather than
/// failing the stop ladder over a config typo.
fn stop_signal(app: &AppConfig) -> StopSignal {
    let Some(name) = app.kill_signal.as_deref() else {
        return StopSignal::Term;
    };
    match name.to_ascii_uppercase().as_str() {
        "SIGTERM" | "TERM" => StopSignal::Term,
        "SIGINT" | "INT" => StopSignal::Int,
        "SIGQUIT" | "QUIT" => StopSignal::Quit,
        "SIGUSR2" | "USR2" => StopSignal::Usr2,
        _ => {
            tracing::warn!(
                kill_signal = name,
                "unknown kill_signal, defaulting to SIGTERM"
            );
            StopSignal::Term
        }
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
            credentials: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn obedient_process_exits_on_signal_without_kill_tree() {
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
        let (mut proc, _io) = runner.spawn(&spec()).unwrap();
        let app = AppConfig::minimal("web", "./srv");

        let start = Instant::now();
        let outcome = kill_process(&mut proc, &app, None).await;
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
        let outcome = kill_process(&mut proc, &app, None).await;
        let elapsed = start.elapsed();

        assert_eq!(elapsed, Duration::from_millis(1600));
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

        let outcome = kill_process(&mut proc, &app, Some(&io.to_child)).await;

        // The fake resolves an obeys_signal wait to Term (15) on a Shutdown
        // message too (Task 3 rule) — but it's never recorded as a signal() call.
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

        let outcome = kill_process(&mut proc, &app, None).await;

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
    fn stop_signal_unknown_name_falls_back_to_term() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.kill_signal = Some("SIGBOGUS".to_string());
        assert_eq!(stop_signal(&app), StopSignal::Term);
    }
}
