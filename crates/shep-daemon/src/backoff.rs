//! Exponential backoff with pinned integer arithmetic

use core::time::Duration;
use std::time::SystemTime;

use shep_core::config::AppConfig;

/// Delay before a restart, given the app's config and how many exits in a
/// row have been unstable.
///
/// A fixed `restart_delay` wins if set. Else a stable exit
/// (`consecutive_unstable == 0`) gets none. Else `exp_backoff_restart_delay`
/// steps `d = min(d * 3 / 2, 15000)` from its initial value,
/// `consecutive_unstable - 1` times; `AppConfig::default()` sets it to
/// 100ms, so an unconfigured app still backs off unless the field is
/// explicitly set to `None`.
pub fn restart_delay(app: &AppConfig, consecutive_unstable: u32) -> Option<Duration> {
    if let Some(fixed) = app.restart_delay {
        return Some(fixed.as_duration());
    }

    if consecutive_unstable == 0 {
        return None;
    }

    if let Some(initial) = app.exp_backoff_restart_delay {
        let mut d = initial.as_millis();

        for _ in 1..consecutive_unstable {
            d = (d * 3 / 2).min(15000);
        }

        return Some(Duration::from_millis(d));
    }

    None
}

/// The delay a sheep adopted across a handover still owes, given the moment
/// its predecessor recorded the respawn as due. Belongs to the sheep's
/// exit, not the handover: an app with `restart_delay = "1h"` reloaded at
/// minute 59 is owed the remaining minute, not another hour. `due` is a
/// wall-clock moment, so it survives the `execve` where a
/// [`tokio::time::Instant`] would not.
///
/// Returns what [`schedule_restart`](crate::supervisor) should sleep;
/// `None` means "respawn now". A `due` already past returns immediately. A
/// `due` further out than `restart_delay(app, 1)` clamps to that value,
/// also the fallback when `due` is `None`; the carried restart count plays
/// no part, since a fresh restart budget does not inherit it either.
#[must_use]
pub fn adopted_restart_delay(
    app: &AppConfig,
    due: Option<SystemTime>,
    now: SystemTime,
) -> Option<Duration> {
    let ceiling = restart_delay(app, 1);
    let Some(due) = due else {
        return ceiling;
    };
    // A `due` in the past makes `duration_since` return `Err`.
    let remaining = due.duration_since(now).unwrap_or(Duration::ZERO);
    let remaining = match ceiling {
        Some(ceiling) => remaining.min(ceiling),
        None => Duration::ZERO,
    };
    // Zero folds into `None`, `schedule_restart`'s own spelling for immediate.
    (remaining > Duration::ZERO).then_some(remaining)
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
    fn default_config_backs_off_unstable_exits() {
        let app = shep_core::config::AppConfig::minimal("p", "./p");
        assert_eq!(restart_delay(&app, 1).unwrap().as_millis(), 100);
        assert_eq!(restart_delay(&app, 2).unwrap().as_millis(), 150);
    }

    #[test]
    fn stable_exit_is_never_delayed_even_with_a_configured_backoff() {
        let app = shep_core::config::AppConfig::minimal("p", "./p");
        assert_eq!(restart_delay(&app, 0), None);
    }

    #[test]
    fn an_explicit_none_still_means_no_backoff() {
        let mut app = shep_core::config::AppConfig::minimal("p", "./p");
        app.exp_backoff_restart_delay = None;
        assert_eq!(restart_delay(&app, 1), None);
        assert_eq!(restart_delay(&app, 5), None);
    }

    /// Parses a full Flockfile rather than setting the field directly, so a
    /// Deserialize regression in the grammar or the default shows up here too.
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

    // --- what an adopted sheep still owes ---

    /// An hour-long delay: long enough that restarting the clock at a
    /// handover is a difference an operator would notice.
    fn hourly() -> AppConfig {
        let mut app = AppConfig::minimal("p", "./p");
        app.restart_delay = Some("1h".parse().unwrap());
        app
    }

    /// A fixed instant, not `SystemTime::now()`: `adopted_restart_delay`
    /// takes `now` as an argument, so a fixed value avoids reading the
    /// clock twice and asserting a range instead of an exact result.
    const NOW: SystemTime = SystemTime::UNIX_EPOCH;

    /// A moment `secs` after [`NOW`], as a predecessor would have recorded it.
    fn due_in(secs: u64) -> SystemTime {
        NOW + Duration::from_secs(secs)
    }

    /// Exact, because both sides come from [`NOW`] rather than two reads of
    /// the wall clock.
    #[test]
    fn an_adopted_sheep_waits_out_only_what_was_left() {
        let left = adopted_restart_delay(&hourly(), Some(due_in(60)), NOW)
            .expect("a minute of an hour is still a wait");
        assert_eq!(
            left,
            Duration::from_secs(60),
            "a minute left of an hour must come back as that minute, not as an hour"
        );
    }

    #[test]
    fn a_deadline_already_past_respawns_immediately() {
        let past = NOW - Duration::from_secs(30);
        assert_eq!(adopted_restart_delay(&hourly(), Some(past), NOW), None);
    }

    #[test]
    fn a_clock_that_jumped_backwards_is_clamped_to_the_configured_delay() {
        assert_eq!(
            adopted_restart_delay(&hourly(), Some(due_in(7200)), NOW),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn no_carried_deadline_falls_back_to_a_first_unstable_exit() {
        assert_eq!(
            adopted_restart_delay(&hourly(), None, NOW),
            Some(Duration::from_secs(3600))
        );
        let backing_off = AppConfig::minimal("p", "./p");
        assert_eq!(
            adopted_restart_delay(&backing_off, None, NOW),
            Some(Duration::from_millis(100)),
            "an app on the default backoff gets its first step, not its current one"
        );
    }

    #[test]
    fn an_app_that_opted_out_of_backoff_is_never_delayed_by_a_deadline() {
        let mut app = AppConfig::minimal("p", "./p");
        app.exp_backoff_restart_delay = None;
        assert_eq!(adopted_restart_delay(&app, Some(due_in(3600)), NOW), None);
        assert_eq!(adopted_restart_delay(&app, None, NOW), None);
    }
}
