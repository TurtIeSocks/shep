//! Validation and normalization: `AppConfig` -> `ResolvedApp`
//!
//! `ResolvedApp` is a proof token: constructing one is only possible through
//! [`normalize`], so daemon code can require it and skip re-validation.

use core::fmt;

use std::collections::BTreeSet;

use globset::Glob;

use crate::config::{AppConfig, CronParseError, CronSchedule, ProbeConfig, ProbeTarget};

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
/// - [`NormalizeError::ZeroInterval`] — a probe's `interval` is explicitly
///   `0`.
/// - [`NormalizeError::WatchWithoutCwd`] — `watch` is `true` with no `cwd`
///   set.
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
    validate_probe(app.readiness_probe.as_ref(), "readiness_probe")?;
    validate_probe(app.liveness_probe.as_ref(), "liveness_probe")?;
    if app.watch && app.cwd.is_none() {
        // `watch` asked for a feature the daemon has no directory to arm:
        // there is no cwd in the Flockfile, and defaulting to the daemon's
        // own cwd risks watching the whole filesystem under a systemd unit
        // with no `WorkingDirectory=` (Rin, 2026-08-08).
        return Err(NormalizeError::WatchWithoutCwd { name: app.name });
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

/// Validates one probe's target and `failure_threshold`, if the probe is
/// configured. `probe` is the Flockfile field name (`"readiness_probe"` or
/// `"liveness_probe"`), carried into any error so the user knows which
/// field to edit. Its own parsed [`ProbeTarget`] is discarded — this
/// function's job is rejection; the daemon re-parses when it arms the probe.
fn validate_probe(probe: Option<&ProbeConfig>, name: &'static str) -> Result<(), NormalizeError> {
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
    if probe.interval.as_millis() == 0 {
        // Not a configuration anybody wants either: `spawn_liveness_task`
        // sleeps `interval` between probes, so a zero would turn it into a
        // hot spin — for `ProbeKind::Exec`, hundreds of process spawns per
        // second, per sheep, forever. shep-daemon's own loop floors this
        // independently too (its `MIN_PROBE_INTERVAL`), the same
        // belt-and-suspenders shape `cron::MIN_MAX_SLEEP` uses opposite
        // `max_cron_sleep`'s config-time floor — this crate does not own
        // the boot wiring that guarantees every `ProbeConfig` reaching the
        // loop came through here.
        return Err(NormalizeError::ZeroInterval { probe: name });
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
/// Growth is expected: this enum has already gained five variants across
/// Phase 4 Tasks 1 and 2, and will gain more this phase (IR-20).
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
    /// A `readiness_probe` or `liveness_probe` has `interval == 0`, which
    /// would spin its liveness/readiness loop as fast as the runtime
    /// allows.
    ZeroInterval {
        /// `"readiness_probe"` or `"liveness_probe"` — the Flockfile field
        /// name, so the error names the line the user has to edit.
        probe: &'static str,
    },
    /// `watch` is enabled but the app sets no `cwd`, so there is no
    /// directory to watch. Carries the app name.
    WatchWithoutCwd {
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
            Self::ZeroInterval { probe } => {
                write!(f, "{probe}.interval must be greater than 0")
            }
            Self::WatchWithoutCwd { name } => {
                write!(f, "sheep `{name}` has watch = true but no cwd to watch")
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
        // the liveness/readiness loop as fast as the runtime allows
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = crate::values::UpDuration::from_millis(0);
        app.readiness_probe = Some(probe);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ZeroInterval {
                probe: "readiness_probe"
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("greater than 0"), "{err}");
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
