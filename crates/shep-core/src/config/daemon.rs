//! Daemon-level configuration: `$SHEP_HOME/shep.toml`
//!
//! Layering (spec §5): file < `SHEP_*` env < CLI flags. This module applies
//! the first two; the CLI applies its flags onto the returned struct.

use core::fmt;

use std::collections::BTreeMap;

use serde::Deserialize;

/// The `[daemon]` section
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonSection {
    /// Emit the daemon's own logs as JSON lines
    pub log_json: bool,
    /// Control-socket path override (default: `$SHEP_HOME/run/shep.sock`)
    pub socket: Option<std::path::PathBuf>,
    /// Dogs to autostart with the daemon (`shep enable` writes this)
    pub enabled_dogs: Vec<String>,
}

/// Parsed daemon configuration with raw per-dog sections
///
/// Dog sections stay untyped here: each dog deserializes its own
/// `[dog.<name>]` table so dog config schemas live with the dog code.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DaemonConfig {
    /// The `[daemon]` section
    pub daemon: DaemonSection,
    /// Raw `[dog.<name>]` sections keyed by dog name
    pub dog: BTreeMap<String, toml::Table>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawDaemonConfig {
    daemon: DaemonSection,
    dog: BTreeMap<String, toml::Table>,
}

impl DaemonConfig {
    /// Builds config from optional file source + environment overrides
    ///
    /// # Errors
    ///
    /// - [`DaemonConfigError::Toml`] — the file source is invalid TOML.
    /// - [`DaemonConfigError::BadEnvValue`] — a `SHEP_*` value is not
    ///   parseable (`SHEP_LOG_JSON` accepts `1|0|true|false`).
    pub fn load(
        file_source: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, DaemonConfigError> {
        let raw: RawDaemonConfig = match file_source {
            Some(src) => toml::from_str(src).map_err(|e| DaemonConfigError::Toml(e.to_string()))?,
            None => RawDaemonConfig::default(),
        };
        let mut cfg = Self {
            daemon: raw.daemon,
            dog: raw.dog,
        };
        if let Some(v) = env("SHEP_LOG_JSON") {
            cfg.daemon.log_json = match v.as_str() {
                "1" | "true" => true,
                "0" | "false" => false,
                _ => return Err(DaemonConfigError::BadEnvValue("SHEP_LOG_JSON", v)),
            };
        }
        if let Some(v) = env("SHEP_SOCKET") {
            cfg.daemon.socket = Some(std::path::PathBuf::from(v));
        }
        Ok(cfg)
    }
}

/// Error type returned from [`DaemonConfig::load`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonConfigError {
    /// `shep.toml` is invalid TOML (carries the parser message)
    Toml(String),
    /// A `SHEP_*` env var held an unparseable value (var name, value)
    BadEnvValue(&'static str, String),
}

impl fmt::Display for DaemonConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(m) => write!(f, "invalid shep.toml: {m}"),
            Self::BadEnvValue(var, v) => write!(f, "invalid value `{v}` for {var}"),
        }
    }
}

impl core::error::Error for DaemonConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = DaemonConfig::load(None, &no_env).unwrap();
        assert!(!cfg.daemon.log_json);
        assert!(cfg.daemon.enabled_dogs.is_empty());
        assert!(cfg.dog.is_empty());
    }

    #[test]
    fn file_sets_values_and_keeps_dog_sections_raw() {
        let src = r#"
[daemon]
log_json = true
enabled_dogs = ["metrics"]

[dog.metrics]
port = 9615
"#;
        let cfg = DaemonConfig::load(Some(src), &no_env).unwrap();
        assert!(cfg.daemon.log_json);
        assert_eq!(cfg.daemon.enabled_dogs, vec!["metrics"]);
        assert_eq!(cfg.dog["metrics"]["port"].as_integer(), Some(9615));
    }

    #[test]
    fn env_overrides_file() {
        let env = |k: &str| (k == "SHEP_LOG_JSON").then(|| "true".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nlog_json = false"), &env).unwrap();
        assert!(cfg.daemon.log_json);
    }

    #[test]
    fn socket_override_via_file_and_env() {
        let cfg = DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/a.sock\""), &no_env).unwrap();
        assert_eq!(
            cfg.daemon.socket.as_deref(),
            Some(std::path::Path::new("/tmp/a.sock"))
        );
        let env = |k: &str| (k == "SHEP_SOCKET").then(|| "/tmp/b.sock".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/a.sock\""), &env).unwrap();
        assert_eq!(
            cfg.daemon.socket.as_deref(),
            Some(std::path::Path::new("/tmp/b.sock"))
        );
    }

    #[test]
    fn bad_toml_is_a_typed_error() {
        assert!(matches!(
            DaemonConfig::load(Some("[daemon"), &no_env),
            Err(DaemonConfigError::Toml(_))
        ));
    }
}
