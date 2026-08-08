//! Restart decision logic: pure function that classifies an exit and determines whether to restart.
//!
//! Core rule: **signal exits (code=None) NEVER match stop_exit_codes**, even if
//! stop_exit_codes is configured. Only exits with a code (Some) are tested.

use core::time::Duration;

use shep_core::config::AppConfig;

use crate::backoff::restart_delay;
use crate::entry::RestartBudget;
use crate::runner::ExitOutcome;

/// Restart decision for a process exit
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Restart the process with an optional delay (None = immediate)
    Restart {
        /// Delay before restart (None = immediate restart)
        delay: Option<Duration>,
    },
    /// Stop cleanly, do not restart
    CleanStop,
    /// Budget exhausted; restart policy failed
    Errored,
}

/// Decide whether to restart based on an exit outcome.
///
/// Rule order (first match wins):
/// 1. Manual stop → CleanStop
/// 2. Exit code matches stop_exit_codes (ONLY if code.is_some()) → CleanStop
/// 3. !autorestart → CleanStop
/// 4. Otherwise: note_exit → if exhausted → Errored; else → Restart with delay
pub fn decide_on_exit(
    app: &AppConfig,
    budget: &mut RestartBudget,
    uptime: Duration,
    exit: ExitOutcome,
    manual_stop: bool,
) -> Decision {
    // Rule 1: manual stop wins
    if manual_stop {
        return Decision::CleanStop;
    }

    // Rule 2: exit code matched against stop_exit_codes (ONLY if code.is_some())
    if let Some(code) = exit.code
        && app.stop_exit_codes.contains(&code)
    {
        return Decision::CleanStop;
    }

    // Rule 3: autorestart is off
    if !app.autorestart {
        return Decision::CleanStop;
    }

    // Rule 4: classify stability and check budget
    let _stability = budget.note_exit(uptime, app.min_uptime.as_duration());
    if budget.exhausted(app.max_restarts) {
        return Decision::Errored;
    }

    let delay = restart_delay(app, budget.unstable_count());
    Decision::Restart { delay }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_stop_wins() {
        let app = AppConfig::minimal("test", "./test");
        let mut budget = RestartBudget::default();
        let exit = ExitOutcome {
            code: Some(1),
            signal: None,
        };
        let d = decide_on_exit(&app, &mut budget, Duration::from_millis(100), exit, true);
        assert!(matches!(d, Decision::CleanStop));
    }

    #[test]
    fn stop_exit_codes_match() {
        let mut app = AppConfig::minimal("test", "./test");
        app.stop_exit_codes = vec![0, 42];
        let mut budget = RestartBudget::default();
        let exit = ExitOutcome {
            code: Some(42),
            signal: None,
        };
        let d = decide_on_exit(&app, &mut budget, Duration::from_secs(1), exit, false);
        assert!(matches!(d, Decision::CleanStop));
    }

    #[test]
    fn signal_exit_never_matches_stop_exit_codes() {
        let mut app = AppConfig::minimal("test", "./test");
        app.stop_exit_codes = vec![0];
        let mut budget = RestartBudget::default();
        // Signal 15 (SIGTERM) with code=None
        let exit = ExitOutcome {
            code: None,
            signal: Some(15),
        };
        let d = decide_on_exit(&app, &mut budget, Duration::from_millis(100), exit, false);
        // Should restart, NOT CleanStop
        assert!(matches!(d, Decision::Restart { .. }));
    }

    #[test]
    fn autorestart_false_returns_clean_stop() {
        let mut app = AppConfig::minimal("test", "./test");
        app.autorestart = false;
        let mut budget = RestartBudget::default();
        let exit = ExitOutcome {
            code: Some(1),
            signal: None,
        };
        let d = decide_on_exit(&app, &mut budget, Duration::from_secs(1), exit, false);
        assert!(matches!(d, Decision::CleanStop));
    }

    #[test]
    fn budget_exhausted_returns_errored() {
        let mut app = AppConfig::minimal("test", "./test");
        app.min_uptime = "1000".parse().unwrap(); // 1000 ms
        // max_restarts uses the real shipped default (16, spec §4) — no
        // override needed now that `exhausted` uses `>=`.
        let mut budget = RestartBudget::default();

        // Make 16 consecutive unstable exits to exhaust the budget
        for _ in 0..16 {
            let exit = ExitOutcome {
                code: Some(1),
                signal: None,
            };
            let d = decide_on_exit(&app, &mut budget, Duration::from_millis(100), exit, false);
            // After the 16th decision, it should be Errored
            if budget.unstable_count() >= 16 {
                assert!(matches!(d, Decision::Errored));
                return;
            }
        }
        panic!("budget should have been exhausted");
    }

    #[test]
    fn stable_exit_returns_restart_with_no_delay() {
        let mut app = AppConfig::minimal("test", "./test");
        app.min_uptime = "500".parse().unwrap();
        let mut budget = RestartBudget::default();
        let exit = ExitOutcome {
            code: Some(1),
            signal: None,
        };
        // Uptime >= min_uptime means stable exit
        let d = decide_on_exit(&app, &mut budget, Duration::from_secs(1), exit, false);
        assert!(matches!(d, Decision::Restart { delay: None }));
    }

    proptest::proptest! {
        #[test]
        fn budget_errors_exactly_at_max_restarts(
            exits in proptest::collection::vec(0u64..500, 16..64)
        ) {
            // Every uptime < min_uptime(1000ms): the 16th decision must be
            // Errored, none before it. max_restarts uses the real shipped
            // default (16, spec §4) — no override needed now that
            // `exhausted` uses `>=`.
            let app = AppConfig::minimal("p", "./p");
            let mut budget = RestartBudget::default();
            for (i, ms) in exits.iter().enumerate() {
                let d = decide_on_exit(&app, &mut budget,
                    Duration::from_millis(*ms),
                    ExitOutcome { code: Some(1), signal: None }, false);
                if i < 15 {
                    proptest::prop_assert!(
                        matches!(d, Decision::Restart { delay: _ }),
                        "Expected Restart at i={}, got {:?}",
                        i,
                        d
                    );
                } else {
                    proptest::prop_assert!(
                        matches!(d, Decision::Errored),
                        "Expected Errored at i={}, got {:?}",
                        i,
                        d
                    );
                    break;
                }
            }
        }
    }
}
