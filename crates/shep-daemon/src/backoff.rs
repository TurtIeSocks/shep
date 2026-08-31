//! Exponential backoff with pinned integer arithmetic

use core::time::Duration;
use std::time::SystemTime;

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

/// The delay a sheep adopted across a handover still owes, given the moment
/// its predecessor recorded the respawn as falling due.
///
/// The delay belongs to the sheep's own exit, not to the handover: an app
/// with `restart_delay = "1h"` whose shepherd is reloaded at minute 59 is
/// owed the remaining minute, not another hour. `due` is what makes that
/// recoverable — a wall-clock moment, which survives an `execve` where the
/// [`tokio::time::Instant`] the original timer slept against does not.
///
/// Returns what [`schedule_restart`](crate::supervisor) should sleep, in the
/// spelling that function already uses: `None` means "respawn now", not "no
/// restart".
///
/// # The three cases, and the ruling on each
///
/// - **`due` has already passed.** Respawn immediately, with no floor. The
///   delay exists to space the respawn from the exit, and that spacing has
///   been served in the only terms it was ever expressed in. A handover
///   happening inside the window does not un-serve it, and a floor would
///   invent pacing neither the operator configured nor the predecessor owed.
///   The `None` this returns still hops through a task and a mailbox send,
///   so "immediate" stays an observable scheduling step rather than an
///   inline respawn.
/// - **The wall clock jumped backwards** — NTP, a suspend — leaving `due -
///   now` longer than the app's configured delay. Clamped to that delay.
///   This is the cost of a clock that can move under a deadline, and the
///   clamp bounds it by what this daemon would have waited anyway: the new
///   behaviour can never make a sheep wait longer than the old one did. A
///   FORWARD jump needs no rule — it shortens the wait, at worst to zero,
///   which is the case above.
/// - **`due` is `None`.** A predecessor from before this field existed
///   carried a `WaitingRestart` sheep and silently dropped the deadline, so
///   there is nothing to anchor to and the fallback is what that
///   predecessor's successor did: the delay a FIRST unstable exit would get.
///   Erring long, deliberately — an operator's pacing is never shortened by
///   a reload.
///
/// The clamp's ceiling is `restart_delay(app, 1)`, the same value the
/// fallback returns, because that is this image's whole belief about what
/// this app is owed: the carried restart COUNT does not survive into the
/// budget (`install_adopted` installs a fresh [`RestartBudget`], since the
/// window it counts over is a run of wall-clock this image never observed).
/// A ceiling of `None` means the app is owed no delay at all, so no carried
/// deadline can make it wait.
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
    // `Err` is a `due` in the past, which is the first case above.
    let remaining = due.duration_since(now).unwrap_or(Duration::ZERO);
    let remaining = match ceiling {
        Some(ceiling) => remaining.min(ceiling),
        None => Duration::ZERO,
    };
    // Zero folds into `None` rather than being slept for: the two are the
    // same instruction to `schedule_restart`, and `None` is the spelling
    // every other caller uses for it.
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

    // --- what an adopted sheep still owes ------------------------------

    /// An app whose respawn is paced by a fixed hour, which is the shape the
    /// whole carried deadline exists for: long enough that restarting the
    /// clock at a handover is a difference an operator would notice.
    fn hourly() -> AppConfig {
        let mut app = AppConfig::minimal("p", "./p");
        app.restart_delay = Some("1h".parse().unwrap());
        app
    }

    /// A fixed instant to measure every carried deadline against.
    ///
    /// Not `SystemTime::now()`, and the difference is the whole reason this
    /// exists. `adopted_restart_delay` takes `now` as an argument, so a test
    /// that builds a deadline from one `now()` and passes another reads the
    /// clock twice and can only assert a RANGE. That range is a claim about
    /// how long the test thread was descheduled between two lines, which on
    /// a loaded runner is not a claim worth making (IR-26, IR-36).
    const NOW: SystemTime = SystemTime::UNIX_EPOCH;

    /// A moment `secs` after [`NOW`], as a predecessor would have recorded it.
    fn due_in(secs: u64) -> SystemTime {
        NOW + Duration::from_secs(secs)
    }

    /// Fails if an adopted sheep's remaining wait is rounded up to the whole
    /// configured delay, which is what restarts the clock at every handover.
    ///
    /// Exact, because both sides come from [`NOW`] rather than from two
    /// reads of the wall clock. What the case is really about is the upper
    /// bound: anything at or above the hour is the old behaviour.
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

    /// Fails if a deadline that has already elapsed schedules another wait.
    ///
    /// `None` is `schedule_restart`'s own spelling for "respawn now", and it
    /// still costs a task and a mailbox hop, so this is not a synchronous
    /// respawn hiding behind a zero.
    #[test]
    fn a_deadline_already_past_respawns_immediately() {
        let past = NOW - Duration::from_secs(30);
        assert_eq!(adopted_restart_delay(&hourly(), Some(past), NOW), None);
    }

    /// Fails if a wall clock that jumped backwards can make a sheep wait
    /// longer than its own configuration ever asked for.
    ///
    /// A deadline two hours out under a one-hour delay is not a schedule
    /// anybody wrote: it is an NTP correction, or a suspend, moving the
    /// clock under a deadline recorded before it. The clamp bounds the new
    /// behaviour by the old one, so the worst a jump can do is give back
    /// exactly what shipped in v0.1.20.
    #[test]
    fn a_clock_that_jumped_backwards_is_clamped_to_the_configured_delay() {
        assert_eq!(
            adopted_restart_delay(&hourly(), Some(due_in(7200)), NOW),
            Some(Duration::from_secs(3600))
        );
    }

    /// Fails if a blob written before the deadline was carried stops getting
    /// the behaviour its own predecessor's successor gave it.
    ///
    /// Absent means unknown here, not "due now": that predecessor carried a
    /// `WaitingRestart` sheep happily and simply had no field to put the
    /// moment in. Erring long is the safe reading, and it is what v0.1.20
    /// did for every sheep.
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

    /// Fails if an app that opted out of every delay is made to wait by a
    /// carried deadline.
    ///
    /// Its ceiling is `None`, which is this image's whole belief about what
    /// the app is owed. A deadline in the future can only have come from a
    /// predecessor running a different configuration, or from a clock that
    /// moved, and neither is a reason to start throttling an app whose
    /// operator turned throttling off.
    #[test]
    fn an_app_that_opted_out_of_backoff_is_never_delayed_by_a_deadline() {
        let mut app = AppConfig::minimal("p", "./p");
        app.exp_backoff_restart_delay = None;
        assert_eq!(adopted_restart_delay(&app, Some(due_in(3600)), NOW), None);
        assert_eq!(adopted_restart_delay(&app, None, NOW), None);
    }
}
