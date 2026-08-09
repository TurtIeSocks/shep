//! Validation and normalization: `AppConfig` -> `ResolvedApp`
//!
//! `ResolvedApp` is a proof token: constructing one is only possible through
//! [`normalize`], so daemon code can require it and skip re-validation.

use core::fmt;

use std::collections::BTreeSet;

use crate::config::{AppConfig, CronParseError, CronSchedule};

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
        crate::config::cron::parse_timezone_name(tz_name).map_err(|()| {
            NormalizeError::InvalidTimezone {
                name: tz_name.clone(),
            }
        })?;
    }
    Ok(ResolvedApp { config: app })
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
}
