//! Daemon-level configuration: `$SHEP_HOME/shep.toml`
//!
//! Layering (spec §5): file < `SHEP_*` env < CLI flags. This module applies
//! the first two; the CLI applies its flags onto the returned struct.

use core::fmt;

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::values::UpDuration;

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
    /// Longest a cron worker sleeps before re-deriving its next occurrence.
    ///
    /// Shorter recovers faster from a suspended laptop or an NTP step and
    /// costs proportionally more wakeups per cron-configured sheep; longer
    /// is cheaper and drifts further. Unset means the daemon's own default.
    /// There is no upper bound: a very long value only degrades to sleeping
    /// straight through to the occurrence, which still fires.
    pub max_cron_sleep: Option<UpDuration>,
}

/// Floor on `[daemon] max_cron_sleep`.
///
/// Zero makes every sleep return immediately and turns the loop into a hot
/// spin that re-derives a schedule as fast as the runtime allows — while
/// still firing correctly, which is what makes it hard to attribute. Low
/// milliseconds are the same fault with a smaller constant. One second is a
/// floor no legitimate configuration wants to be under: a five-field cron
/// pattern cannot name anything finer than a minute, so even this is sixty
/// times more often than the tightest schedule can fire.
const MIN_CRON_SLEEP: UpDuration = UpDuration::from_millis(1_000);

/// Parsed daemon configuration with raw per-dog sections
///
/// Dog sections stay untyped here: each dog deserializes its own
/// `[dog.<name>]` table so dog config schemas live with the dog code.
#[derive(Clone, Default, PartialEq)]
pub struct DaemonConfig {
    /// The `[daemon]` section
    pub daemon: DaemonSection,
    /// Raw `[dog.<name>]` sections keyed by dog name
    pub dog: BTreeMap<String, toml::Table>,
}

/// Debug implementation does not leak dog config values (IR-41)
impl fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("daemon", &self.daemon)
            .field("dog", &format_args!("<{} tables>", self.dog.len()))
            .finish()
    }
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
    ///   parseable (`SHEP_LOG_JSON` accepts `1|0|true|false`;
    ///   `SHEP_MAX_CRON_SLEEP` parses as an [`UpDuration`]).
    /// - [`DaemonConfigError::BelowMinimum`] — the effective
    ///   `max_cron_sleep` (file or `SHEP_MAX_CRON_SLEEP`, whichever won) is
    ///   below the floor.
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
        // Provenance needs no tracking beyond this one flag: if the env var
        // was present it won, so the key it's validated (and reported)
        // under is its own; otherwise the value came from the file (or is
        // unset) and the key is the TOML one. Validating each layer as it
        // is read instead would make a good SHEP_MAX_CRON_SLEEP unable to
        // rescue a broken shep.toml, which is not what "file < env" means.
        let mut max_cron_sleep_key = "max_cron_sleep";
        if let Some(v) = env("SHEP_MAX_CRON_SLEEP") {
            let parsed = v
                .parse::<UpDuration>()
                .map_err(|_| DaemonConfigError::BadEnvValue("SHEP_MAX_CRON_SLEEP", v))?;
            cfg.daemon.max_cron_sleep = Some(parsed);
            max_cron_sleep_key = "SHEP_MAX_CRON_SLEEP";
        }
        if let Some(value) = cfg.daemon.max_cron_sleep
            && value < MIN_CRON_SLEEP
        {
            return Err(DaemonConfigError::BelowMinimum {
                key: max_cron_sleep_key,
                value,
                min: MIN_CRON_SLEEP,
            });
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
    /// A `[daemon]` duration is below the floor that keeps the daemon from
    /// spinning. Carries the key the user actually set — the TOML key or
    /// the environment variable, whichever supplied the winning value.
    BelowMinimum {
        /// `max_cron_sleep` or `SHEP_MAX_CRON_SLEEP`.
        key: &'static str,
        /// The value as the user wrote it.
        value: UpDuration,
        /// The floor it failed.
        min: UpDuration,
    },
}

impl fmt::Display for DaemonConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(m) => write!(f, "invalid shep.toml: {m}"),
            Self::BadEnvValue(var, v) => write!(f, "invalid value `{v}` for {var}"),
            Self::BelowMinimum { key, value, min } => {
                write!(
                    f,
                    "invalid value `{value}` for {key}: must be at least {min}"
                )
            }
        }
    }
}

impl core::error::Error for DaemonConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::UpDuration;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    // fails if a serde default invents 60s in shep-core and takes the
    // "unset" state away from the layer below
    #[test]
    fn missing_max_cron_sleep_leaves_the_field_none() {
        let cfg = DaemonConfig::load(None, &no_env).unwrap();
        assert_eq!(cfg.daemon.max_cron_sleep, None);
    }

    // fails if the field is a bare integer, where "5m" is a TOML error and
    // "5" is five milliseconds
    #[test]
    fn max_cron_sleep_file_value_parses_via_upduration() {
        let cfg = DaemonConfig::load(Some("[daemon]\nmax_cron_sleep = \"5m\""), &no_env).unwrap();
        assert_eq!(
            cfg.daemon.max_cron_sleep,
            Some(UpDuration::from_millis(5 * 60_000))
        );
    }

    // fails if the env read is placed before the file is folded in, or
    // omitted entirely
    #[test]
    fn env_max_cron_sleep_beats_file_value() {
        let env = |k: &str| (k == "SHEP_MAX_CRON_SLEEP").then(|| "90s".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nmax_cron_sleep = \"5m\""), &env).unwrap();
        assert_eq!(
            cfg.daemon.max_cron_sleep,
            Some(UpDuration::from_millis(90_000))
        );
    }

    // fails if the env read swallows its parse failure (`.ok()` and drop
    // it, or an `Err` arm that only logs), leaving the file's value
    // silently in force and the typo invisible
    #[test]
    fn bad_env_max_cron_sleep_is_a_typed_error() {
        let env = |k: &str| (k == "SHEP_MAX_CRON_SLEEP").then(|| "banana".to_string());
        assert_eq!(
            DaemonConfig::load(None, &env),
            Err(DaemonConfigError::BadEnvValue(
                "SHEP_MAX_CRON_SLEEP",
                "banana".to_string()
            ))
        );
    }

    // fails if the floor is compared with `>` instead of `>=`, or the check
    // silently clamps instead of rejecting
    #[test]
    fn max_cron_sleep_floor_rejects_below_one_second() {
        let cfg = DaemonConfig::load(Some("[daemon]\nmax_cron_sleep = \"1s\""), &no_env).unwrap();
        assert_eq!(
            cfg.daemon.max_cron_sleep,
            Some(UpDuration::from_millis(1_000))
        );

        assert_eq!(
            DaemonConfig::load(Some("[daemon]\nmax_cron_sleep = \"999\""), &no_env),
            Err(DaemonConfigError::BelowMinimum {
                key: "max_cron_sleep",
                value: UpDuration::from_millis(999),
                min: UpDuration::from_millis(1_000),
            })
        );
    }

    // fails if only the file value is validated and never the override, or
    // if the reported key is the file's even though the environment
    // introduced the fault
    #[test]
    fn env_max_cron_sleep_floor_check_runs_on_the_winner() {
        let env = |k: &str| (k == "SHEP_MAX_CRON_SLEEP").then(|| "0".to_string());
        assert_eq!(
            DaemonConfig::load(Some("[daemon]\nmax_cron_sleep = \"5m\""), &env),
            Err(DaemonConfigError::BelowMinimum {
                key: "SHEP_MAX_CRON_SLEEP",
                value: UpDuration::from_millis(0),
                min: UpDuration::from_millis(1_000),
            })
        );
    }

    // fails if the message wording drifts (e.g. "invalid" alone, or the
    // `key`/`min` operands swapped) without anyone noticing — this is the
    // entire user-facing payload of the reject-don't-clamp decision: it is
    // what actually reaches `shepd.err.log` on exit code 4.
    #[test]
    fn below_minimum_display_is_exact() {
        let err = DaemonConfigError::BelowMinimum {
            key: "max_cron_sleep",
            value: UpDuration::from_millis(999),
            min: UpDuration::from_millis(1_000),
        };
        assert_eq!(
            err.to_string(),
            "invalid value `999` for max_cron_sleep: must be at least 1s"
        );
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

    #[test]
    fn debug_redacts_dog_values() {
        // Dog tables carry things like webhook URLs; a lazy derive(Debug)
        // would land them in daemon logs. Exact string pinned so that
        // regression fails here instead of leaking a secret.
        let cfg = DaemonConfig::load(Some("[dog.metrics]\nport = 9615"), &no_env).unwrap();
        assert_eq!(
            format!("{cfg:?}"),
            "DaemonConfig { daemon: DaemonSection { log_json: false, socket: None, enabled_dogs: [], max_cron_sleep: None }, dog: <1 tables> }"
        );
    }
}
