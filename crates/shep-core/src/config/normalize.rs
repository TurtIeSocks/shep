//! Validation and normalization: `AppConfig` -> `ResolvedApp`
//!
//! `ResolvedApp` is a proof token: constructing one is only possible through
//! [`normalize`], so daemon code can require it and skip re-validation.

use core::fmt;

use std::collections::BTreeSet;

use globset::Glob;

use crate::config::{
    AppConfig, CronParseError, CronSchedule, KillSignal, ProbeConfig, ProbeTarget,
};
use crate::values::UpDuration;

/// Shortest `interval` a `liveness_probe` may name.
///
/// The daemon's liveness loop floors whatever it is handed at this same value
/// (its own `MIN_PROBE_INTERVAL`), so a smaller number would be *honoured* as
/// this one with nothing to say so — in a detached daemon, not even a log
/// line. That is the reasoning `max_cron_sleep` was settled on (`MIN_CRON_SLEEP`
/// rejects rather than clamps), and it applies here for the same reason: the
/// user's file is the only place the discrepancy could ever be noticed.
///
/// One second is a floor no legitimate configuration wants to be under. A
/// liveness check asked for more often than that is polling, and for
/// [`ProbeKind::Exec`](crate::config::ProbeKind::Exec) it is that many process
/// spawns per second, per sheep, for as long as the sheep runs.
const MIN_LIVENESS_INTERVAL: UpDuration = UpDuration::from_millis(1_000);

/// Shortest `interval` a `readiness_probe` may name.
///
/// A whole second lower than [`MIN_LIVENESS_INTERVAL`], and deliberately so:
/// the readiness wait honours its `interval` exactly as written and is bounded
/// by the app's `listen_timeout`, so there is no clamp for a rejection to keep
/// honest here — only the zero, which would spin that wait for the whole
/// `listen_timeout`. A fast app that polls every 20ms to leave `starting`
/// sooner is asking for something the daemon really does, so this floor must
/// not take it away.
const MIN_READINESS_INTERVAL: UpDuration = UpDuration::from_millis(1);

/// Longest `action_timeout` an app may name.
///
/// Not a floor this time but a ceiling, and for a different reason than
/// [`MIN_LIVENESS_INTERVAL`]'s: there, a smaller number was silently
/// honoured as the floor; here, a larger one could never be honoured by
/// ANY caller at all. The daemon clamps every RPC deadline a client can
/// possibly ask for — its own `MAX_DEADLINE_MS`, 60s, in `shep-daemon`'s
/// `rpc` module — so an `action_timeout` at or above that line describes a
/// wait the daemon could never finish inside any request budget, no matter
/// how generous a caller's own `Client::request_with_deadline` call is.
/// That is not "the caller forgot to widen its deadline" (this crate has no
/// way to see a caller's choice, and does not try to); it is "no choice
/// exists", which is what makes it a config error rather than a caller's to
/// fix. Set 2s under the hard clamp — the same margin `action_timeout`'s own
/// default keeps under the *default* RPC budget — so the daemon still has
/// room to build the `TimedOut` row and get it back down the wire after the
/// wait itself gives up.
const MAX_ACTION_TIMEOUT: UpDuration = UpDuration::from_millis(58_000);

/// A validated app config — only obtainable via [`normalize`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedApp {
    config: AppConfig,
}

impl ResolvedApp {
    /// Borrow the validated configuration
    #[must_use]
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Unwrap the validated configuration (consumes the proof token)
    #[must_use]
    pub fn into_config(self) -> AppConfig {
        self.config
    }
}

/// Validates one app config
///
/// # Errors
///
/// - [`NormalizeError::MissingName`] — `name` is empty.
/// - [`NormalizeError::InvalidName`] — `name` contains a path separator or is `.`/`..`.
/// - [`NormalizeError::MissingScript`] — `script` is empty.
/// - [`NormalizeError::ZeroInstances`] — `instances == 0`.
/// - [`NormalizeError::InvalidCron`] — `cron_restart` is not valid in
///   croner's dialect (carries the pattern and the rejection reason).
/// - [`NormalizeError::InvalidTimezone`] — `cron_timezone` is not a name in
///   the IANA time-zone database.
/// - [`NormalizeError::InvalidProbe`] — `readiness_probe` or `liveness_probe`
///   has a target [`ProbeTarget::parse`] rejects (carries which probe and
///   the rendered reason).
/// - [`NormalizeError::ZeroFailureThreshold`] — a probe's `failure_threshold`
///   is explicitly `0`.
/// - [`NormalizeError::IntervalBelowMinimum`] — a probe's `interval` is under
///   the floor its own loop honours: a full second for `liveness_probe`, and
///   only "greater than zero" for `readiness_probe` (carries which probe, the
///   value and the floor).
/// - [`NormalizeError::ZeroMaxMemory`] — `max_memory` is `0`.
/// - [`NormalizeError::ActionTimeoutTooLong`] — `action_timeout` is at or
///   above the ceiling no RPC caller could ever be given room to wait past
///   (carries the app name, the value and the ceiling).
/// - [`NormalizeError::InvalidKillSignal`] — `kill_signal` names a signal the
///   daemon's stop ladder cannot send (carries the app name and the value).
/// - [`NormalizeError::WatchWithoutCwd`] — `watch` is `true` with no `cwd`
///   set.
/// - [`NormalizeError::ZeroWatchDelay`] — `watch_delay` is `0`.
/// - [`NormalizeError::InvalidWatchGlob`] — a `watch_options` or
///   `ignore_watch` pattern globset will not compile (carries the app name,
///   which of the two lists, the pattern and the reason).
pub fn normalize(app: AppConfig) -> Result<ResolvedApp, NormalizeError> {
    if app.name.is_empty() {
        return Err(NormalizeError::MissingName);
    }
    if app.name.contains(['/', '\\']) || app.name == "." || app.name == ".." {
        return Err(NormalizeError::InvalidName(app.name));
    }
    if app.script.is_empty() {
        return Err(NormalizeError::MissingScript);
    }
    if app.instances == 0 {
        return Err(NormalizeError::ZeroInstances);
    }
    if let Some(pattern) = &app.cron_restart {
        CronSchedule::parse(pattern, app.cron_timezone.as_deref()).map_err(|e| match e {
            CronParseError::Pattern { pattern, reason } => {
                NormalizeError::InvalidCron { pattern, reason }
            }
            CronParseError::Timezone { name } => NormalizeError::InvalidTimezone { name },
        })?;
    } else if let Some(tz_name) = &app.cron_timezone {
        // A Flockfile can carry `cron_timezone` with no `cron_restart` to
        // pair it with — still a typo the user wants to hear about (spec §5).
        crate::config::cron::parse_timezone_name(tz_name).ok_or_else(|| {
            NormalizeError::InvalidTimezone {
                name: tz_name.clone(),
            }
        })?;
    }
    validate_probe(
        app.readiness_probe.as_ref(),
        "readiness_probe",
        MIN_READINESS_INTERVAL,
    )?;
    validate_probe(
        app.liveness_probe.as_ref(),
        "liveness_probe",
        MIN_LIVENESS_INTERVAL,
    )?;
    if app.max_memory.is_some_and(|limit| limit.bytes() == 0) {
        // A ceiling every live process is over, armed against every poll: the
        // enforcer would report a breach on its first reading and on every
        // reading after it, and the restart that follows is automatic, which
        // RESETS the restart budget rather than spending it. `max_restarts`
        // cannot end that loop, so it has to be refused here.
        return Err(NormalizeError::ZeroMaxMemory { name: app.name });
    }
    if let Some(name) = &app.kill_signal
        && KillSignal::parse(name).is_none()
    {
        // Rejected rather than clamped, and this one is the sharpest case of
        // that trade in the file. The daemon's stop ladder used to fall back
        // to SIGTERM and log a warning, which meant a typo cost the operator
        // every stop and every reload for the life of the process, with the
        // only evidence in a detached daemon's log at the moment of a stop.
        // `max_cron_sleep` and `MIN_LIVENESS_INTERVAL` reject for the same
        // reason at lower stakes: the user's file is the only place a
        // silently-substituted value could ever be noticed.
        return Err(NormalizeError::InvalidKillSignal {
            name: app.name,
            value: name.clone(),
        });
    }
    if app.action_timeout > MAX_ACTION_TIMEOUT {
        // Rejected rather than clamped, the same trade `MIN_LIVENESS_INTERVAL`
        // and `max_cron_sleep` already made: a daemon running detached has no
        // reader for a log line saying the value was silently lowered, so the
        // Flockfile would be the only place the discrepancy ever showed up —
        // and here there is no honest lowered value to fall back to anyway,
        // since every value above the ceiling is equally unreachable by any
        // caller.
        return Err(NormalizeError::ActionTimeoutTooLong {
            name: app.name,
            value: app.action_timeout,
            max: MAX_ACTION_TIMEOUT,
        });
    }
    if app.watch && app.cwd.is_none() {
        // `watch` asked for a feature the daemon has no directory to arm:
        // there is no cwd in the Flockfile, and defaulting to the daemon's
        // own cwd risks watching the whole filesystem under a systemd unit
        // with no `WorkingDirectory=` (Rin, 2026-08-08).
        return Err(NormalizeError::WatchWithoutCwd { name: app.name });
    }
    if app.reuse_port {
        // Accepted, stored and displayed since it was added, and read by
        // nothing. Refusing is the honest answer while that is true: an
        // operator who writes it is asking for behaviour shep does not have,
        // and finding out at parse time beats finding out from a port
        // conflict in production.
        return Err(NormalizeError::ReusePortUnimplemented { name: app.name });
    }
    if app.watch_delay == Some(UpDuration::from_millis(0)) {
        // notify's debouncer derives its own poll tick as `watch_delay / 4`
        // and runs it on a dedicated OS thread, so a zero turns that thread
        // into `loop { sleep(0); lock(); }`: measured at 5.98s of user CPU
        // across a three-second watch that costs 0.00s at the 500ms default.
        // shep-daemon's watch arming floors this independently too (its
        // `MIN_WATCH_DELAY`), the same belt-and-suspenders shape
        // `validate_probe`'s interval check has opposite the liveness loop's
        // own floor.
        return Err(NormalizeError::ZeroWatchDelay { name: app.name });
    }
    // Both lists are checked whether or not `watch` is on. A pattern globset
    // will not compile is a typo, and the user wants it named now rather than
    // the day they flip `watch = true` and wonder why saving a file changes
    // nothing — the same reasoning that makes `watch` without `cwd` a config
    // error above (Rin, 2026-08-09).
    validate_watch_globs(&app.name, "watch_options", &app.watch_options)?;
    validate_watch_globs(&app.name, "ignore_watch", &app.ignore_watch)?;
    Ok(ResolvedApp { config: app })
}

/// Validates one of an app's two watch glob lists, rejecting any pattern
/// globset will not compile. `field` is the Flockfile field name
/// (`"watch_options"` or `"ignore_watch"`), carried into any error so the
/// user knows which list to edit. The compiled globs are discarded — this
/// function's job is rejection; the daemon builds its own watch filter when
/// it arms the watch.
fn validate_watch_globs(
    name: &str,
    field: &'static str,
    patterns: &[String],
) -> Result<(), NormalizeError> {
    for pattern in patterns {
        Glob::new(pattern).map_err(|err| NormalizeError::InvalidWatchGlob {
            name: name.to_string(),
            field,
            pattern: pattern.clone(),
            reason: err.to_string(),
        })?;
    }
    Ok(())
}

/// Validates one probe's target, `failure_threshold` and `interval`, if the
/// probe is configured. `probe` is the Flockfile field name
/// (`"readiness_probe"` or `"liveness_probe"`), carried into any error so the
/// user knows which field to edit; `min_interval` is the floor that probe's
/// own loop in the daemon honours, which is why the two call sites pass
/// different ones. Its own parsed [`ProbeTarget`] is discarded — this
/// function's job is rejection; the daemon re-parses when it arms the probe.
fn validate_probe(
    probe: Option<&ProbeConfig>,
    name: &'static str,
    min_interval: UpDuration,
) -> Result<(), NormalizeError> {
    let Some(probe) = probe else {
        return Ok(());
    };
    ProbeTarget::parse(probe).map_err(|reason| NormalizeError::InvalidProbe {
        probe: name,
        reason: reason.to_string(),
    })?;
    if probe.failure_threshold == 0 {
        // Unhealthy before the first probe ever runs — not a configuration
        // anybody wants, and it would make the liveness loop restart the
        // sheep immediately and forever.
        return Err(NormalizeError::ZeroFailureThreshold { probe: name });
    }
    if probe.interval < min_interval {
        // Not a configuration anybody wants either. Both probe loops sleep
        // `interval` between attempts, so a zero turns either into a hot
        // spin — for `ProbeKind::Exec`, hundreds of process spawns per
        // second, per sheep. A liveness interval that is merely *small*
        // is refused for a second reason: `spawn_liveness_task` rounds it UP
        // to its own `MIN_PROBE_INTERVAL`, which would leave the user's file
        // the only place the discrepancy exists and nothing anywhere to
        // report it. Rejecting rather than clamping is what `max_cron_sleep`
        // settled on for that same trade; the daemon-side floor stays too,
        // because this crate does not own the boot wiring that guarantees
        // every `ProbeConfig` reaching the loop came through here.
        return Err(NormalizeError::IntervalBelowMinimum {
            probe: name,
            value: probe.interval,
            min: min_interval,
        });
    }
    Ok(())
}

/// Validates a whole flock, rejecting duplicate sheep names
///
/// # Errors
///
/// Everything [`normalize`] returns, plus
/// [`NormalizeError::DuplicateName`] — two apps share a `name`.
pub fn normalize_all(apps: Vec<AppConfig>) -> Result<Vec<ResolvedApp>, NormalizeError> {
    let mut seen = BTreeSet::new();
    apps.into_iter()
        .map(|app| {
            if !seen.insert(app.name.clone()) {
                return Err(NormalizeError::DuplicateName(app.name));
            }
            normalize(app)
        })
        .collect()
}

/// Error type returned from [`normalize`] and [`normalize_all`]
///
/// Growth is expected: every config surface this crate learns to validate
/// brings its own rejection reasons with it (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    /// `name` is empty
    MissingName,
    /// `name` contains `/` or `\` or is `.`/`..` — it becomes a filesystem
    /// path stem, so these would escape the shep home (carries the name)
    InvalidName(String),
    /// `script` is empty
    MissingScript,
    /// `instances` is zero
    ZeroInstances,
    /// `cron_restart` is not valid in croner's dialect. Carries the pattern
    /// and the rejection reason — croner's own sentence where croner did the
    /// rejecting, ours where shep's pre-parse pass did.
    InvalidCron {
        /// The pattern as the user wrote it
        pattern: String,
        /// Why it was rejected
        reason: String,
    },
    /// `cron_timezone` is not a name in the IANA time-zone database
    InvalidTimezone {
        /// The value as the user wrote it
        name: String,
    },
    /// Two apps in one flock share this name
    DuplicateName(String),
    /// A `readiness_probe` or `liveness_probe` target is malformed. Carries
    /// which probe and the rendered reason.
    InvalidProbe {
        /// `"readiness_probe"` or `"liveness_probe"` — the Flockfile field
        /// name, so the error names the line the user has to edit.
        probe: &'static str,
        /// [`ProbeTarget::parse`]'s rendered rejection reason.
        reason: String,
    },
    /// A `readiness_probe` or `liveness_probe` has `failure_threshold == 0`.
    ZeroFailureThreshold {
        /// `"readiness_probe"` or `"liveness_probe"` — the Flockfile field
        /// name, so the error names the line the user has to edit.
        probe: &'static str,
    },
    /// A `readiness_probe` or `liveness_probe` has an `interval` under the
    /// floor its own loop in the daemon honours. At `0` that would spin the
    /// loop as fast as the runtime allows; a `liveness_probe` under a full
    /// second would instead be silently polled at that second.
    IntervalBelowMinimum {
        /// `"readiness_probe"` or `"liveness_probe"` — the Flockfile field
        /// name, so the error names the line the user has to edit.
        probe: &'static str,
        /// The value as the user wrote it.
        value: UpDuration,
        /// The floor it failed.
        min: UpDuration,
    },
    /// `max_memory` is `0` — a ceiling every live process is already over, so
    /// the enforcer would restart the sheep on every poll forever. Carries
    /// the app name.
    ZeroMaxMemory {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
    },
    /// `action_timeout` is at or above `normalize`'s own ceiling — a wait no
    /// RPC caller could ever be given enough deadline to outlast, since the
    /// daemon clamps every deadline a caller can ask for. Carries the app
    /// name, the value as written, and the ceiling it failed.
    ActionTimeoutTooLong {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// The value as the user wrote it.
        value: UpDuration,
        /// The ceiling it failed.
        max: UpDuration,
    },
    /// `kill_signal` names a signal the daemon's stop ladder cannot send.
    /// Carries the app name and the value as written.
    InvalidKillSignal {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// The value as the user wrote it.
        value: String,
    },
    /// `watch` is enabled but the app sets no `cwd`, so there is no
    /// directory to watch. Carries the app name.
    WatchWithoutCwd {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
    },
    /// `reuse_port` is set, and nothing reads it.
    ///
    /// Refused rather than ignored (Rin, 2026-08-19). The field parsed,
    /// stored and displayed for several phases while no production code
    /// consulted it, so a Flockfile could ask for `SO_REUSEPORT` and quietly
    /// not get it. A config that silently does nothing is worse than one
    /// that will not load.
    ReusePortUnimplemented {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
    },
    /// `watch_delay` is `0`, which would spin the debouncer's own OS thread.
    /// Carries the app name.
    ZeroWatchDelay {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
    },
    /// A `watch_options` or `ignore_watch` pattern is one globset will not
    /// compile, so the watch it describes could never be armed.
    InvalidWatchGlob {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// `"watch_options"` or `"ignore_watch"` — the Flockfile field name,
        /// so the error names which of the two lists to edit.
        field: &'static str,
        /// The pattern as the user wrote it.
        pattern: String,
        /// globset's own rendered reason.
        reason: String,
    },
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => f.write_str("app config is missing a name"),
            Self::InvalidName(n) => {
                write!(
                    f,
                    "sheep name `{n}` may not contain a path separator or be `.` or `..`"
                )
            }
            Self::MissingScript => f.write_str("app config is missing a script"),
            Self::ZeroInstances => f.write_str("instances must be at least 1"),
            Self::InvalidCron { pattern, reason } => {
                write!(f, "invalid cron_restart pattern `{pattern}`: {reason}")
            }
            Self::InvalidTimezone { name } => {
                write!(f, "`{name}` is not a recognized IANA timezone")
            }
            Self::DuplicateName(n) => write!(f, "duplicate sheep name `{n}`"),
            Self::InvalidProbe { probe, reason } => write!(f, "{probe}: {reason}"),
            Self::ZeroFailureThreshold { probe } => {
                write!(f, "{probe}.failure_threshold must be at least 1")
            }
            Self::IntervalBelowMinimum { probe, value, min } => {
                write!(f, "{probe}.interval is `{value}`: must be at least {min}")
            }
            Self::ZeroMaxMemory { name } => {
                write!(
                    f,
                    "sheep `{name}` has max_memory = 0, a limit nothing can stay under"
                )
            }
            Self::ActionTimeoutTooLong { name, value, max } => {
                write!(
                    f,
                    "sheep `{name}` has action_timeout = {value}: must be at most {max}, \
                     the longest wait any caller's deadline could ever cover"
                )
            }
            Self::InvalidKillSignal { name, value } => {
                write!(
                    f,
                    "`{name}`: kill_signal `{value}` is not one shep can send (accepted: {})",
                    KillSignal::ACCEPTED.join(", ")
                )
            }
            Self::ReusePortUnimplemented { name } => {
                write!(
                    f,
                    "`{name}`: reuse_port is accepted by the schema but not yet implemented, \
                     so shep refuses it rather than ignoring it. Remove the line to load this \
                     Flockfile."
                )
            }
            Self::WatchWithoutCwd { name } => {
                write!(f, "sheep `{name}` has watch = true but no cwd to watch")
            }
            Self::ZeroWatchDelay { name } => {
                write!(
                    f,
                    "sheep `{name}` has watch_delay = 0: must be greater than 0"
                )
            }
            Self::InvalidWatchGlob {
                name,
                field,
                pattern,
                reason,
            } => write!(
                f,
                "sheep `{name}` has an invalid {field} pattern `{pattern}`: {reason}"
            ),
        }
    }
}

impl core::error::Error for NormalizeError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Refused, not ignored. `reuse_port` parsed and stored for several
    /// phases while no production code read it, so a Flockfile could ask for
    /// `SO_REUSEPORT` and quietly not get it (Rin's call, 2026-08-19).
    #[test]
    fn reuse_port_is_refused_while_nothing_implements_it() {
        let mut app = AppConfig::minimal("web", "./server");
        app.reuse_port = true;

        let err = normalize(app).expect_err("reuse_port must not load");
        assert!(
            matches!(err, NormalizeError::ReusePortUnimplemented { ref name } if name == "web"),
            "the refusal names the entry to edit: {err:?}"
        );

        let rendered = err.to_string();
        assert!(
            rendered.contains("not yet implemented"),
            "the message says why rather than just refusing: {rendered}"
        );
        assert!(
            !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
            "no em or en dash in copy a user reads: {rendered}"
        );
    }

    /// The default is off, so every Flockfile that does not mention it keeps
    /// loading. Pins that the refusal above cannot become a wall for
    /// everyone.
    #[test]
    fn an_app_that_never_mentions_reuse_port_still_normalizes() {
        normalize(AppConfig::minimal("web", "./server"))
            .expect("the common case must be untouched");
    }
    use crate::config::AppConfig;

    #[test]
    fn valid_minimal_config_normalizes() {
        let resolved = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        assert_eq!(resolved.config().name, "web");
    }

    #[test]
    fn names_that_reach_the_filesystem_are_rejected() {
        // A name becomes a log/pid file stem via Path::join; a slash-prefixed
        // or dotdot name would escape $SHEP_HOME. Reject at the config boundary.
        for bad in ["/etc/passwd", "..", ".", "a/b", "a\\b"] {
            assert_eq!(
                normalize(AppConfig::minimal(bad, "./srv")).unwrap_err(),
                NormalizeError::InvalidName(bad.to_string())
            );
        }
        assert!(normalize(AppConfig::minimal("web-1", "./srv")).is_ok());
    }

    #[test]
    fn missing_name_and_script_are_distinct_errors() {
        assert_eq!(
            normalize(AppConfig::minimal("", "./srv")).unwrap_err(),
            NormalizeError::MissingName
        );
        assert_eq!(
            normalize(AppConfig::minimal("web", "")).unwrap_err(),
            NormalizeError::MissingScript
        );
    }

    #[test]
    fn zero_instances_rejected() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 0;
        assert_eq!(normalize(app).unwrap_err(), NormalizeError::ZeroInstances);
    }

    #[test]
    fn bad_cron_pattern_rejected_with_pattern_and_reason_carried_through() {
        // fails if the reason is not carried through from croner. This
        // input is three tokens, already rejected by the token-count
        // stopgap that used to sit here, so it guards the pattern/reason
        // plumbing, not the dialect check itself — see the next test for
        // the case that actually proves the stopgap is gone.
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("not a cron".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidCron { pattern, reason } => {
                assert_eq!(pattern, "not a cron");
                assert!(!reason.is_empty());
            }
            other => panic!("expected InvalidCron, got {other:?}"),
        }
    }

    #[test]
    fn five_tokens_of_garbage_cron_pattern_rejected() {
        // fails if the validator is still a token counter: the stopgap this
        // replaced accepted exactly this input, since it only counted
        // whitespace-separated tokens.
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("99 99 99 99 99".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidCron { pattern, .. } => {
                assert_eq!(pattern, "99 99 99 99 99");
            }
            other => panic!("expected InvalidCron, got {other:?}"),
        }
    }

    #[test]
    fn bad_cron_timezone_rejected_alongside_a_valid_cron_restart() {
        // fails if the `cron_restart` branch maps CronParseError::Timezone to
        // anything but NormalizeError::InvalidTimezone. CronSchedule::parse
        // resolves the zone before it looks at the pattern, so a valid pattern
        // paired with a bad zone is the only input that reaches that arm — the
        // zone-with-no-pattern test below takes the separate `else if` branch.
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("0 3 * * *".to_string());
        app.cron_timezone = Some("Mars/Olympus".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidTimezone { name } => assert_eq!(name, "Mars/Olympus"),
            other => panic!("expected InvalidTimezone, got {other:?}"),
        }
    }

    #[test]
    fn cron_timezone_validated_even_without_cron_restart() {
        // fails if timezone validation is skipped when there's no pattern to
        // pair it with — a Flockfile with only a bad `cron_timezone` is a
        // typo the user wants to hear about (spec §5).
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_timezone = Some("Mars/Olympus".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidTimezone { name } => assert_eq!(name, "Mars/Olympus"),
            other => panic!("expected InvalidTimezone, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_names_rejected_across_a_flock() {
        let apps = vec![
            AppConfig::minimal("web", "./a"),
            AppConfig::minimal("web", "./b"),
        ];
        assert_eq!(
            normalize_all(apps).unwrap_err(),
            NormalizeError::DuplicateName("web".to_string())
        );
    }

    fn probe_config(target: &str) -> crate::config::ProbeConfig {
        crate::config::ProbeConfig {
            kind: crate::config::ProbeKind::Http,
            target: target.to_string(),
            interval: crate::values::UpDuration::from_millis(10_000),
            timeout: crate::values::UpDuration::from_millis(5_000),
            failure_threshold: 3,
        }
    }

    #[test]
    fn malformed_readiness_probe_target_rejected_naming_the_field() {
        // fails if validate_probe is never called for readiness_probe, or if
        // it drops which of the two probe fields the rejection came from
        let mut app = AppConfig::minimal("web", "./srv");
        app.readiness_probe = Some(probe_config("not-a-url"));
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidProbe { probe, reason } => {
                assert_eq!(probe, "readiness_probe");
                assert!(!reason.is_empty());
            }
            other => panic!("expected InvalidProbe, got {other:?}"),
        }
    }

    #[test]
    fn malformed_liveness_probe_target_rejected_naming_the_field() {
        // fails if only readiness_probe is ever validated, leaving a bad
        // liveness_probe target to surface later at the daemon's first poll
        let mut app = AppConfig::minimal("web", "./srv");
        app.liveness_probe = Some(probe_config("not-a-url"));
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidProbe { probe, .. } => assert_eq!(probe, "liveness_probe"),
            other => panic!("expected InvalidProbe, got {other:?}"),
        }
    }

    #[test]
    fn valid_probe_targets_accepted() {
        // fails if validate_probe rejects a well-formed target outright
        let mut app = AppConfig::minimal("web", "./srv");
        app.readiness_probe = Some(probe_config("http://127.0.0.1:8080/healthz"));
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn zero_failure_threshold_rejected() {
        // fails if failure_threshold is never inspected — a threshold of 0
        // means "unhealthy before the first probe ever runs"
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.failure_threshold = 0;
        app.readiness_probe = Some(probe);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ZeroFailureThreshold {
                probe: "readiness_probe"
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("at least 1"), "{err}");
    }

    #[test]
    fn zero_interval_rejected() {
        // fails if interval is never inspected — a zero interval would spin
        // the readiness wait as fast as the runtime allows for the whole
        // `listen_timeout` (`await_ready` deliberately does not floor it)
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = UpDuration::from_millis(0);
        app.readiness_probe = Some(probe);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::IntervalBelowMinimum {
                probe: "readiness_probe",
                value: UpDuration::from_millis(0),
                min: MIN_READINESS_INTERVAL,
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("must be at least"), "{err}");
    }

    #[test]
    fn a_liveness_interval_under_the_floor_is_rejected_rather_than_clamped() {
        // fails if the liveness check is `interval == 0` rather than a
        // floor. A 500ms interval survives an equality check and is then
        // rounded UP to a full second by `spawn_liveness_task`'s own
        // `MIN_PROBE_INTERVAL` — an app polled at half the rate its
        // Flockfile asks for, with nothing anywhere to say so: that clamp
        // writes no record at all, so not even the daemon's own log names
        // it. Also fails if the rejection drops the value
        // the user wrote, which is the one number that tells them what to
        // edit.
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = UpDuration::from_millis(500);
        app.liveness_probe = Some(probe);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::IntervalBelowMinimum {
                probe: "liveness_probe",
                value: UpDuration::from_millis(500),
                min: MIN_LIVENESS_INTERVAL,
            }
        );
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn a_liveness_interval_exactly_at_the_floor_is_accepted() {
        // fails if the comparison is `<=` rather than `<` — the floor is a
        // value the liveness loop honours exactly, so naming it must not be
        // an error (IR-40: sweep the boundary, not just past it).
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = MIN_LIVENESS_INTERVAL;
        app.liveness_probe = Some(probe);
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn a_sub_second_readiness_interval_is_accepted() {
        // fails if both probes are validated against the liveness floor. A
        // readiness wait is bounded by `listen_timeout` and honours its
        // `interval` exactly as written (`await_ready` argues the case
        // itself), so a fast app polling every 50ms to leave `starting`
        // sooner is asking for something the daemon really does — refusing
        // it would take a working feature away to fix a clamp that only the
        // liveness loop has.
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = UpDuration::from_millis(50);
        app.readiness_probe = Some(probe);
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn zero_max_memory_rejected() {
        // fails if `max_memory` is never inspected. Zero is a ceiling every
        // live process is already over, so the enforcer breaches on its
        // first reading and every reading after it — and the restart that
        // follows is automatic, which RESETS the restart budget, so
        // `max_restarts` never ends the loop.
        let mut app = AppConfig::minimal("web", "./srv");
        app.max_memory = Some(crate::values::MemSize::from_bytes(0));
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ZeroMaxMemory {
                name: "web".to_string()
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("max_memory"), "{err}");
    }

    #[test]
    fn a_nonzero_max_memory_is_accepted() {
        // fails if the check fires on `max_memory` being set at all rather
        // than on its being zero — that would refuse every app that
        // configures a limit, which is the whole feature
        let mut app = AppConfig::minimal("web", "./srv");
        app.max_memory = Some("512M".parse().unwrap());
        assert!(normalize(app).is_ok());
    }

    /// fails if a `kill_signal` shep cannot send is accepted here. Accepting it
    /// is what put SIGTERM on the wire for the life of the process with nothing
    /// but one daemon log line to say so — the clamp this rejection replaces.
    #[test]
    fn a_kill_signal_shep_cannot_send_is_refused_by_name() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.kill_signal = Some("SIGUSR1".to_string());

        let err = normalize(app).unwrap_err();

        assert_eq!(
            err,
            NormalizeError::InvalidKillSignal {
                name: "web".to_string(),
                value: "SIGUSR1".to_string(),
            }
        );
        // The message has to name the accepted set, because the operator's next
        // move is picking a different word and there is nowhere else to look.
        let rendered = err.to_string();
        assert!(rendered.contains("SIGUSR1"), "{rendered}");
        assert!(rendered.contains("SIGTERM"), "{rendered}");
        assert!(rendered.contains("SIGUSR2"), "{rendered}");
    }

    /// fails if the four supported names, their bare forms, or a lowercase
    /// spelling stop being accepted. This is the compatibility half: every
    /// spelling `stop_signal` accepted before this task must still normalize.
    #[test]
    fn every_spelling_the_daemon_already_accepted_still_normalizes() {
        for name in [
            "SIGTERM", "TERM", "sigterm", "term", "SIGINT", "INT", "SIGQUIT", "QUIT", "SIGUSR2",
            "USR2", "sigusr2",
        ] {
            let mut app = AppConfig::minimal("web", "./srv");
            app.kill_signal = Some(name.to_string());
            assert!(
                normalize(app).is_ok(),
                "`{name}` was accepted before this task and must still be"
            );
        }
    }

    /// fails if an unset `kill_signal` is refused — the overwhelmingly common
    /// case, and the one a validation pass is most likely to break by treating
    /// `None` as an empty string.
    #[test]
    fn an_unset_kill_signal_is_not_a_config_error() {
        let app = AppConfig::minimal("web", "./srv");
        assert!(app.kill_signal.is_none());
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn action_timeout_past_the_ceiling_is_rejected() {
        // fails if `action_timeout` is never inspected. One millisecond over
        // the ceiling is deliberate: a test at a round number like 60s could
        // pass by coincidence if the check used the wrong constant entirely
        // (`MAX_DEADLINE_MS` itself, say, instead of the margin under it).
        let mut app = AppConfig::minimal("web", "./srv");
        app.action_timeout = UpDuration::from_millis(MAX_ACTION_TIMEOUT.as_millis() + 1);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ActionTimeoutTooLong {
                name: "web".to_string(),
                value: UpDuration::from_millis(MAX_ACTION_TIMEOUT.as_millis() + 1),
                max: MAX_ACTION_TIMEOUT,
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("action_timeout"), "{err}");
    }

    #[test]
    fn action_timeout_at_the_ceiling_is_accepted() {
        // fails if the comparison is `>=` rather than `>` — the ceiling
        // itself still leaves the daemon its full margin under the hard
        // clamp, so it is not one of the values nothing could ever satisfy.
        let mut app = AppConfig::minimal("web", "./srv");
        app.action_timeout = MAX_ACTION_TIMEOUT;
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn the_default_action_timeout_is_accepted() {
        // fails if `AppConfig::default()`'s own value ever drifts past the
        // ceiling normalize enforces — the one combination that must never
        // reject the config nobody customized.
        assert!(normalize(AppConfig::minimal("web", "./srv")).is_ok());
    }

    #[test]
    fn zero_watch_delay_rejected() {
        // fails if `watch_delay` is never inspected. notify's debouncer
        // derives its poll tick as `watch_delay / 4` and sleeps it on its own
        // OS thread, so zero is `loop { sleep(0); lock(); }` — measured at
        // 5.98s of user CPU across a three-second watch that costs 0.00s at
        // the 500ms default.
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_delay = Some(UpDuration::from_millis(0));
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ZeroWatchDelay {
                name: "web".to_string()
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("watch_delay"), "{err}");
    }

    #[test]
    fn a_zero_watch_delay_is_rejected_with_watch_off() {
        // fails if the check is nested inside the `watch` block: an app
        // carrying `watch_delay = "0"` with `watch = false` would normalize
        // clean, and the spin would arrive the day someone flips `watch =
        // true` — the same reasoning that puts the glob checks outside it
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_delay = Some(UpDuration::from_millis(0));
        assert!(matches!(
            normalize(app).unwrap_err(),
            NormalizeError::ZeroWatchDelay { .. }
        ));
    }

    #[test]
    fn a_nonzero_watch_delay_is_accepted() {
        // fails if the check fires on `watch_delay` being set at all rather
        // than on its being zero — that would refuse every app that tunes
        // its own debounce
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_delay = Some(UpDuration::from_millis(1));
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn default_failure_threshold_from_toml_accepted() {
        // fails if the check fires on the ordinary default instead of only
        // an explicit 0. Deserializes a Flockfile snippet that omits
        // `failure_threshold` entirely, so this exercises the real
        // `#[serde(default = "default_failure_threshold")]` path
        // (config/app.rs) rather than duplicating `probe_config`'s
        // hardcoded `3` — a literal that wouldn't notice if the wired
        // default ever changed.
        let src = r#"
name = "web"
script = "./srv"

[readiness_probe]
kind = "http"
target = "http://127.0.0.1:8080/healthz"
"#;
        let app: AppConfig = toml::from_str(src).unwrap();
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn watch_true_without_cwd_rejected_naming_the_app() {
        // fails if a validator never looks at `watch`, or looks at it but
        // carries no app name, leaving the user unable to tell which
        // Flockfile entry to edit
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::WatchWithoutCwd {
                name: "web".to_string()
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("no cwd to watch"), "{err}");
    }

    #[test]
    fn watch_true_with_cwd_accepted() {
        // fails if the check fires on `watch` alone, ignoring that a cwd was
        // actually provided
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn a_watch_options_glob_that_will_not_compile_is_rejected() {
        // fails if `watch_options` patterns are never compiled at config
        // time — the sheep would then report `online` with no watch armed
        // and nothing but a log line to say so. Also fails if the rejection
        // blames the whole list instead of the one bad pattern: the valid
        // `src/**` comes first, so an error carrying it, or carrying the
        // patterns joined together, is not the pattern the user must fix.
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_options = vec!["src/**".to_string(), "[".to_string()];
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::InvalidWatchGlob {
                name: "web".to_string(),
                field: "watch_options",
                pattern: "[".to_string(),
                reason: Glob::new("[").unwrap_err().to_string(),
            }
        );
        // fails if the message drops the app name, the list or the pattern —
        // the three things that name the Flockfile line to edit.
        let rendered = err.to_string();
        for expected in ["web", "watch_options", "`[`"] {
            assert!(
                rendered.contains(expected),
                "{expected} missing: {rendered}"
            );
        }
    }

    #[test]
    fn an_ignore_watch_glob_that_will_not_compile_is_rejected() {
        // fails if only `watch_options` is ever compiled, leaving a mistyped
        // `ignore_watch` to cost the app its watch at arm time instead
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.ignore_watch = vec!["[".to_string()];
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidWatchGlob { field, pattern, .. } => {
                assert_eq!(field, "ignore_watch");
                assert_eq!(pattern, "[");
            }
            other => panic!("expected InvalidWatchGlob, got {other:?}"),
        }
    }

    #[test]
    fn a_glob_that_will_not_compile_is_rejected_with_watch_off() {
        // fails if glob validation is nested inside the `watch` check: an app
        // carrying a mistyped glob with `watch = false` would then normalize
        // clean, and the typo would surface only the day someone flips
        // `watch = true`
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_options = vec!["[".to_string()];
        assert!(matches!(
            normalize(app).unwrap_err(),
            NormalizeError::InvalidWatchGlob { .. }
        ));
    }

    #[test]
    fn well_formed_watch_globs_are_accepted() {
        // fails if the new check rejects patterns globset compiles happily —
        // recursive `**`, a character class, a negated character class and a
        // brace alternation are all ordinary globset syntax a Flockfile is
        // entitled to use. Also fails if the check is wired to a parser that
        // is not globset's: every one of these is valid to globset and a
        // syntax error to a regex engine.
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_options = vec!["src/**/*.rs".to_string(), "*.[ch]".to_string()];
        app.ignore_watch = vec!["target/**".to_string(), "**/[!.]*.{tmp,swp}".to_string()];
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn watch_options_without_watch_or_cwd_accepted() {
        // fails if the check is keyed on `watch_options` being non-empty
        // rather than on `watch` being true — that would reject a Flockfile
        // that never asked to be watched
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_options = vec!["src/**".to_string()];
        assert!(normalize(app).is_ok());
    }
}
