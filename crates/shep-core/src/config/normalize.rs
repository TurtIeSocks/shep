//! Validation and normalization: `AppConfig` -> `ResolvedApp`
//!
//! `ResolvedApp` is a proof token: constructing one is only possible through
//! [`normalize`], so daemon code can require it and skip re-validation.

use core::fmt;

use std::collections::BTreeSet;

use crate::config::AppConfig;

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
/// - [`ConfigError::MissingName`] — `name` is empty.
/// - [`ConfigError::MissingScript`] — `script` is empty.
/// - [`ConfigError::ZeroInstances`] — `instances == 0`.
/// - [`ConfigError::InvalidCron`] — `cron_restart` is not a 5-field pattern.
pub fn normalize(app: AppConfig) -> Result<ResolvedApp, ConfigError> {
    if app.name.is_empty() {
        return Err(ConfigError::MissingName);
    }
    if app.script.is_empty() {
        return Err(ConfigError::MissingScript);
    }
    if app.instances == 0 {
        return Err(ConfigError::ZeroInstances);
    }
    if let Some(pattern) = &app.cron_restart {
        // ponytail: field-count check only; croner dialect validation lands
        // with the daemon phase that actually schedules crons
        if pattern.split_whitespace().count() != 5 {
            return Err(ConfigError::InvalidCron(pattern.clone()));
        }
    }
    Ok(ResolvedApp { config: app })
}

/// Validates a whole flock, rejecting duplicate sheep names
///
/// # Errors
///
/// Everything [`normalize`] returns, plus
/// [`ConfigError::DuplicateName`] — two apps share a `name`.
pub fn normalize_all(apps: Vec<AppConfig>) -> Result<Vec<ResolvedApp>, ConfigError> {
    let mut seen = BTreeSet::new();
    apps.into_iter()
        .map(|app| {
            if !seen.insert(app.name.clone()) {
                return Err(ConfigError::DuplicateName(app.name));
            }
            normalize(app)
        })
        .collect()
}

/// Error type returned from [`normalize`] and [`normalize_all`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// `name` is empty
    MissingName,
    /// `script` is empty
    MissingScript,
    /// `instances` is zero
    ZeroInstances,
    /// `cron_restart` is not a 5-field cron pattern (carries the pattern)
    InvalidCron(String),
    /// Two apps in one flock share this name
    DuplicateName(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => f.write_str("app config is missing a name"),
            Self::MissingScript => f.write_str("app config is missing a script"),
            Self::ZeroInstances => f.write_str("instances must be at least 1"),
            Self::InvalidCron(p) => write!(f, "invalid cron pattern `{p}`"),
            Self::DuplicateName(n) => write!(f, "duplicate sheep name `{n}`"),
        }
    }
}

impl core::error::Error for ConfigError {}

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
    fn missing_name_and_script_are_distinct_errors() {
        assert_eq!(
            normalize(AppConfig::minimal("", "./srv")).unwrap_err(),
            ConfigError::MissingName
        );
        assert_eq!(
            normalize(AppConfig::minimal("web", "")).unwrap_err(),
            ConfigError::MissingScript
        );
    }

    #[test]
    fn zero_instances_rejected() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 0;
        assert_eq!(normalize(app).unwrap_err(), ConfigError::ZeroInstances);
    }

    #[test]
    fn bad_cron_pattern_rejected_with_pattern_in_error() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("not a cron".to_string());
        match normalize(app).unwrap_err() {
            ConfigError::InvalidCron(p) => assert_eq!(p, "not a cron"),
            other => panic!("expected InvalidCron, got {other:?}"),
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
            ConfigError::DuplicateName("web".to_string())
        );
    }
}
