//! Exponential backoff with pinned integer arithmetic

use core::time::Duration;

use shep_core::config::AppConfig;

/// Compute restart delay based on app config and consecutive unstable exits.
///
/// - Fixed `restart_delay` field takes precedence if set
/// - Else if `exp_backoff_restart_delay = Some(initial)`: uses iterative integer rule
///   `d = min(d * 3 / 2, 15000)` starting at `initial` for `consecutive_unstable = 1`,
///   applied `consecutive_unstable - 1` times
/// - `consecutive_unstable == 0` (stable exit) → `None` (immediate restart)
/// - Else None
pub fn restart_delay(app: &AppConfig, consecutive_unstable: u32) -> Option<Duration> {
    // Fixed delay takes precedence
    if let Some(fixed) = app.restart_delay {
        return Some(fixed.as_duration());
    }

    // Stable exit → immediate restart
    if consecutive_unstable == 0 {
        return None;
    }

    // Exponential backoff
    if let Some(initial) = app.exp_backoff_restart_delay {
        let mut d = initial.as_millis();

        // Apply the iterative rule consecutive_unstable - 1 times
        for _ in 1..consecutive_unstable {
            d = (d * 3 / 2).min(15000);
        }

        return Some(Duration::from_millis(d));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_sequence_is_pinned() {
        let mut app = shep_core::config::AppConfig::minimal("p", "./p");
        app.exp_backoff_restart_delay = Some("100".parse().unwrap());
        let expected: [u64; 15] = [
            100, 150, 225, 337, 505, 757, 1135, 1702, 2553, 3829, 5743, 8614, 12921, 15000, 15000,
        ];
        for (i, want) in expected.iter().enumerate() {
            let got = restart_delay(&app, (i + 1) as u32).unwrap();
            assert_eq!(
                got.as_millis() as u64,
                *want,
                "consecutive_unstable={}",
                i + 1
            );
        }
    }

    #[test]
    fn stable_exit_no_delay() {
        let mut app = shep_core::config::AppConfig::minimal("p", "./p");
        app.exp_backoff_restart_delay = Some("100".parse().unwrap());
        assert_eq!(restart_delay(&app, 0), None);
    }

    #[test]
    fn fixed_delay_overrides_backoff() {
        let mut app = shep_core::config::AppConfig::minimal("p", "./p");
        app.restart_delay = Some("500".parse().unwrap());
        app.exp_backoff_restart_delay = Some("100".parse().unwrap());
        assert_eq!(restart_delay(&app, 1).unwrap().as_millis(), 500);
    }

    #[test]
    fn neither_set_returns_none() {
        let app = shep_core::config::AppConfig::minimal("p", "./p");
        assert_eq!(restart_delay(&app, 1), None);
        assert_eq!(restart_delay(&app, 5), None);
    }
}
