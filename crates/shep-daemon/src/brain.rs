//! Restart decision logic.
//!
//! A signal exit (`code: None`) never matches `stop_exit_codes`; only `Some(code)` is tested.

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

/// Decides whether to restart, checking in order: manual stop, a
/// `stop_exit_codes` match, `autorestart`, then the restart budget.
pub fn decide_on_exit(
    app: &AppConfig,
    budget: &mut RestartBudget,
    uptime: Duration,
    exit: ExitOutcome,
    manual_stop: bool,
) -> Decision {
    if manual_stop {
        return Decision::CleanStop;
    }

    if let Some(code) = exit.code
        && app.stop_exit_codes.contains(&code)
    {
        return Decision::CleanStop;
    }

    if !app.autorestart {
        return Decision::CleanStop;
    }

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
        let exit = ExitOutcome {
            code: None,
            signal: Some(15),
        };
        let d = decide_on_exit(&app, &mut budget, Duration::from_millis(100), exit, false);
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
        // max_restarts is the real default (16); exhausted uses >=.
        let mut budget = RestartBudget::default();

        for _ in 0..16 {
            let exit = ExitOutcome {
                code: Some(1),
                signal: None,
            };
            let d = decide_on_exit(&app, &mut budget, Duration::from_millis(100), exit, false);
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
            // ms range stays under the default min_uptime (1000ms), so every exit is unstable.
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
