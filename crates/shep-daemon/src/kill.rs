//! The kill ladder: graceful stop escalating to `SIGKILL`
//!
//! [`kill_process`] runs inside the sheep task that owns the live [`RunningProcess`]
//! (Task 9's per-sheep task). It is generic over [`RunningProcess`] so the same
//! ladder drives both `crate::fake::FakeProc` in engine tests and the real
//! `tokio_runner` child in production — this module itself stays portable
//! (no `cfg(unix)`) since it only touches the portable [`StopSignal`] enum and
//! the [`RunningProcess`] trait, never OS signal APIs directly.

use core::time::Duration;

use tokio::sync::mpsc;

use shep_core::config::{AppConfig, KillSignal};

use crate::channel::ShepherdMessage;
use crate::runner::{ExitOutcome, RunningProcess, StopSignal};

/// Runs the stop ladder against one live process and returns its exit outcome.
///
/// 1. If `app.shutdown_with_message` is set and `to_child` is present, sends
///    [`ShepherdMessage::Shutdown`] over the shepherd channel; otherwise sends
///    the app's configured [`StopSignal`] (resolved by the private
///    `stop_signal` parser from `app.kill_signal`) to the sheep's whole
///    process group — lambs included, see [`RunningProcess::signal`].
/// 2. Waits up to `grace` for the process to exit.
/// 3. On timeout, SIGKILLs the whole process tree and waits for that to land.
///
/// `grace` is the caller's, not the app's, because an app configures two of
/// them for two different asks: `kill_timeout` for an ordinary stop, and
/// `graceful_timeout` — longer by default — for the one stop that expects the
/// instance to finish the work in hand first, a reload's drain. Step 1 is
/// identical in both cases, including which of the two messages goes out:
/// `shutdown_with_message` keys that, and a drain wanting more patience is not
/// a reason to tell the child something different.
///
/// Delivery failures (message send, signal, `kill_tree`) are logged and never
/// panic: the caller always gets a terminal [`ExitOutcome`], even if every
/// delivery step failed and the timeout simply ran out the clock.
// Step 3 is not made redundant by step 1 reaching the same process group:
//
// - Every signal step 1 can send is catchable. `SIGTERM`/`INT`/`QUIT`/`USR2`
//   can all be trapped or ignored (`tests/real_runner.rs` runs a `trap ""
//   TERM` sheep that does exactly that); `SIGKILL` cannot. Escalating from
//   the polite signal to the unblockable one IS the ladder.
// - Under `shutdown_with_message`, step 1 sends no signal at all — a child
//   that never acts on the message has only this rung left.
// - Group membership is a snapshot taken at delivery: anything forked after
//   step 1's signal landed never saw it, and only step 3 re-sweeps the group.
//
// The rung it still cannot cover: `proc.wait()` resolves on the LEADER's
// exit, so a lamb that survives its own graceful signal and outlives the
// sheep ends the ladder at step 2 without ever reaching step 3. Waiting for
// the whole group to empty has no portable syscall, and the daemon never
// learns lamb pids (spec §7's shepherd channel does not report them), so
// nothing here can detect that case today.
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
/// Total over [`KillSignal`], with no fallback branch, because the grammar
/// and the rejection both live in shep-core now: a config that reached the
/// daemon at all came through `normalize`, which refuses any name this cannot
/// map. The `_ => Term` clamp this replaced is the whole of config.md #2 — it
/// turned a typo into SIGTERM for the life of the process and said so only in
/// a log line no detached daemon has a reader for.
///
/// The one branch that is still defensive is the unparseable name below. It
/// is unreachable through `normalize`, and it is an `error!` rather than a
/// `warn!` for that reason: reaching it means a config bypassed validation,
/// which is a bug in the daemon's own wiring and not something an operator
/// can fix by editing a file.
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

    /// The cap an ordinary stop passes — the app's own `kill_timeout`, which
    /// is what every caller outside a reload's drain hands `kill_process`.
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

    // fails if the ladder ignores the cap its caller passed and reads
    // `kill_timeout` off the app again — a drain would then SIGKILL the
    // instance at 1600ms, five seconds before the deadline it was promised,
    // and the whole point of draining is the time to finish work in hand.
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

    // fails if an unparseable `kill_signal` reaching the stop ladder (a state
    // `normalize` should have refused before the daemon ever saw it) stops
    // logging loudly. This branch is unreachable through a validated config;
    // it exists only for a wiring bug, and an `error!` line is the one signal
    // an operator staring at a detached daemon's logs would actually have.
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
