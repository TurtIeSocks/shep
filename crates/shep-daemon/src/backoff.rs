//! Exponential backoff with pinned integer arithmetic

use core::time::Duration;

use shep_core::config::AppConfig;

/// Compute restart delay based on app config and consecutive unstable exits.
///
/// - Fixed `restart_delay` field takes precedence if set (even over a stable exit)
/// - Else `consecutive_unstable == 0` (stable exit) → `None` (immediate restart)
/// - Else if `exp_backoff_restart_delay = Some(initial)`: uses iterative integer rule
///   `d = min(d * 3 / 2, 15000)` starting at `initial` for `consecutive_unstable = 1`,
///   applied `consecutive_unstable - 1` times. `AppConfig::default()` sets this to
///   `Some(100ms)`, so an unstable exit is throttled unless an operator opts out by
///   setting the field to `None` explicitly.
/// - Else (both unset) → `None` (immediate restart)
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

    /// Defect 2 fix: an app with neither `restart_delay` nor
    /// `exp_backoff_restart_delay` set is not "no delay" any more:
    /// `AppConfig::default()` now carries a 100ms `exp_backoff_restart_delay`
    /// (see its doc comment), so an unconfigured app still backs off on
    /// unstable exits.
    #[test]
    fn default_config_backs_off_unstable_exits() {
        let app = shep_core::config::AppConfig::minimal("p", "./p");
        assert_eq!(restart_delay(&app, 1).unwrap().as_millis(), 100);
        assert_eq!(restart_delay(&app, 2).unwrap().as_millis(), 150);
    }

    /// A stable exit (`consecutive_unstable == 0`) is unaffected by the
    /// default above: it stays `None` regardless of what's configured, so a
    /// healthy app restarting during a deploy is never throttled.
    #[test]
    fn stable_exit_is_never_delayed_even_with_a_configured_backoff() {
        let app = shep_core::config::AppConfig::minimal("p", "./p");
        assert_eq!(restart_delay(&app, 0), None);
    }

    /// Explicitly turning the backoff off (`exp_backoff_restart_delay =
    /// None`) is still honoured: this is the escape hatch documented on the
    /// field for an operator who wants the old unthrottled behaviour back.
    #[test]
    fn an_explicit_none_still_means_no_backoff() {
        let mut app = shep_core::config::AppConfig::minimal("p", "./p");
        app.exp_backoff_restart_delay = None;
        assert_eq!(restart_delay(&app, 1), None);
        assert_eq!(restart_delay(&app, 5), None);
    }

    /// The only opt-out an operator can actually write is the string `"0"`:
    /// `AppConfig`'s struct-level `#[serde(default)]` means an omitted key
    /// deserializes to `Some(100ms)`, not `None` -- TOML has no way to write
    /// a bare null. Parses a real Flockfile end to end, rather than poking
    /// the struct field directly, so a Deserialize regression in the
    /// grammar or the default would show up here too.
    #[test]
    fn the_toml_escape_hatch_is_the_string_zero_not_a_missing_key() {
        let src = r#"
[[app]]
name = "p"
script = "./p"
exp_backoff_restart_delay = "0"
"#;
        let flock =
            shep_core::config::Flockfile::parse(src, shep_core::config::FlockFormat::Toml).unwrap();
        let app = &flock.apps[0];
        assert_eq!(
            app.exp_backoff_restart_delay,
            Some(shep_core::values::UpDuration::from_millis(0))
        );
        assert_eq!(restart_delay(app, 1), Some(Duration::from_millis(0)));
    }
}
