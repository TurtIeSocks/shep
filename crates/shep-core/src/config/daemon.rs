//! Daemon-level configuration: `$SHEP_HOME/shep.toml`
//!
//! Layering (spec §5): file < `SHEP_*` env < CLI flags. This module applies
//! the first two; the CLI applies its flags onto the returned struct.

use core::fmt;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::values::UpDuration;

/// The `[daemon]` section
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonSection {
    /// Emit the daemon's own logs as JSON lines
    pub log_json: bool,
    /// Lowest severity of the daemon's own records that reaches its log
    pub log_level: LogLevel,
    /// Control-socket path override (default: `$SHEP_HOME/run/shep.sock`)
    pub socket: Option<std::path::PathBuf>,
    /// Dogs to autostart with the daemon (`shep enable` writes this)
    pub enabled_dogs: Vec<String>,
    /// Where an adopted dog's binary lives, keyed by dog name
    /// (`shep adopt` writes this; `shep rehome` removes it).
    ///
    /// A name in [`Self::enabled_dogs`] with no entry here is a built-in
    /// dog, an argv branch of the shep binary itself. Not recorded inside
    /// `[dog.<name>]`: that table is the dog's own opaque configuration,
    /// and a shep-owned key inside it would collide with a third-party
    /// dog's schema.
    pub adopted_dogs: BTreeMap<String, PathBuf>,
    /// Longest a cron worker sleeps before re-deriving its next occurrence.
    ///
    /// Shorter recovers faster from a suspended laptop or an NTP step and
    /// costs proportionally more wakeups per cron-configured sheep; longer
    /// is cheaper and drifts further. Unset means the daemon's own default.
    /// There is no upper bound: a very long value only degrades to sleeping
    /// straight through to the occurrence, which still fires.
    pub max_cron_sleep: Option<UpDuration>,
}

/// How much of the daemon's own diagnostics reaches its log.
///
/// Written as one of the names below in `[daemon] log_level` or in
/// `SHEP_LOG_LEVEL`, lowercase and nothing else, the same closed grammar
/// `log_json` accepts, so a typo is a startup error naming the value
/// rather than a level silently reverting to the default.
///
/// The default is [`LogLevel::Warn`]. The daemon's records are dominated
/// by warn-and-continue arms, each the only account of a decision the
/// operator cannot otherwise see. [`LogLevel::Debug`] adds per-decision
/// detail firing per dropped restart and per child metric sample, a
/// firehose on a busy flock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Nothing at all: the daemon writes no records of its own.
    Off,
    /// Only faults the daemon could not work around.
    Error,
    /// Faults the daemon worked around, and what working around them cost.
    #[default]
    Warn,
    /// Lifecycle milestones: the daemon came up, the daemon is going down.
    Info,
    /// Per-decision detail: every restart weighed, every metric sampled.
    Debug,
    /// Everything the daemon can say about itself.
    Trace,
}

impl LogLevel {
    /// The one spelling this level is written as, in the file and in the
    /// environment alike
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// The level `name` spells, or `None` when it spells no level.
    ///
    /// The inverse of [`LogLevel::as_str`], and exact: an uppercase or
    /// mixed-case name is not a level here, because `SHEP_LOG_JSON` accepts
    /// no `TRUE` either.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }
}

/// Floor on `[daemon] max_cron_sleep`.
///
/// Zero makes every sleep return immediately, spinning the loop while
/// still firing correctly, which is what makes it hard to attribute. One
/// second is a floor no legitimate configuration wants to be under: a
/// five-field cron pattern cannot name anything finer than a minute.
const MIN_CRON_SLEEP: UpDuration = UpDuration::from_millis(1_000);

/// The `[whistle]` section.
///
/// One key, a gate rather than a tuning knob: `shep whistle`'s four
/// control tools exist only when this is `true`; its five read-only
/// tools exist regardless.
///
/// Lives only in `shep.toml`, no flag or env var, since config is
/// auditable where a flag is not. The shepherd itself never reads this
/// key; `shep whistle` reads the file directly. Declared here anyway
/// because `RawDaemonConfig` denies unknown fields, so an undeclared
/// `[whistle]` section would refuse the whole file to boot. `Debug` is
/// derived, not redacted: one boolean, nothing to leak.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WhistleSection {
    /// Whether `shep whistle` offers its control tools. Default `false`.
    pub allow_control: bool,
}

/// The `[style]` section: how much the CLI dresses up its output.
///
/// Read by the CLI only. The daemon has no opinion about how anyone likes
/// their tables, and parses this solely so an unknown key is not an error.
///
/// `Debug` is derived rather than redacted: one optional string, no
/// secret, nothing a `{:?}` could leak.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StyleSection {
    /// `full`, `plain` or `bare`. Absent means the CLI decides.
    pub level: Option<String>,
}

/// Parsed daemon configuration with raw per-dog sections.
///
/// Dog sections stay untyped here: each dog deserializes its own
/// `[dog.<name>]` table, so dog config schemas live with the dog code.
///
/// `#[non_exhaustive]` guards against a breaking struct literal as this
/// type grows sections, but is not a validation gate: its `pub` fields
/// can still be mutated after [`Self::load`]/[`Self::load_layered`]
/// validate, and shep-core cannot detect that.
#[non_exhaustive]
#[derive(Clone, Default, PartialEq)]
pub struct DaemonConfig {
    /// The `[daemon]` section
    pub daemon: DaemonSection,
    /// The `[whistle]` section
    pub whistle: WhistleSection,
    /// The `[style]` section
    pub style: StyleSection,
    /// The `[interpreters]` section: a script extension (no leading dot,
    /// `"js"` not `".js"`) mapped to the interpreter that runs it.
    ///
    /// Read by the CLI only, before a request reaches the wire: target
    /// resolution folds a match into an app's own
    /// [`AppConfig::interpreter`](crate::config::AppConfig::interpreter)
    /// only when that field is unset, and `--interpreter` on the command
    /// line outranks both. The daemon itself never reads this field.
    ///
    /// Declared here, like [`StyleSection`], so `RawDaemonConfig`'s
    /// `deny_unknown_fields` does not turn an unrecognized `[interpreters]`
    /// section into a hard parse error on every boot.
    pub interpreters: BTreeMap<String, String>,
    /// Raw `[dog.<name>]` sections keyed by dog name
    pub dog: BTreeMap<String, toml::Table>,
}

/// Redacts `dog`: only the table count is printed.
impl fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("daemon", &self.daemon)
            .field("whistle", &self.whistle)
            .field("style", &self.style)
            .field("interpreters", &self.interpreters)
            .field("dog", &format_args!("<{} tables>", self.dog.len()))
            .finish()
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawDaemonConfig {
    daemon: DaemonSection,
    whistle: WhistleSection,
    style: StyleSection,
    interpreters: BTreeMap<String, String>,
    dog: BTreeMap<String, toml::Table>,
}

impl DaemonConfig {
    /// Builds config from optional file source + environment overrides.
    ///
    /// `file < env`, validated. Equivalent to [`Self::load_layered`] with
    /// an empty [`DaemonOverrides`].
    ///
    /// # Errors
    /// - [`DaemonConfigError::Toml`]: the file source is invalid TOML.
    /// - [`DaemonConfigError::BadEnvValue`]: a `SHEP_*` value is not parseable.
    /// - [`DaemonConfigError::BelowMinimum`]: the effective `max_cron_sleep` is below the floor.
    pub fn load(
        file_source: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, DaemonConfigError> {
        Self::load_layered(file_source, env, &DaemonOverrides::new())
    }

    /// Builds config from optional file source + environment + CLI-flag
    /// overrides.
    ///
    /// `file < env < flags` (spec §5), validated exactly once, at the end,
    /// so a later layer can rescue a value an earlier one would reject.
    ///
    /// # Errors
    /// - [`DaemonConfigError::Toml`]: the file source is invalid TOML.
    /// - [`DaemonConfigError::BadEnvValue`]: a `SHEP_*` value is not parseable.
    /// - [`DaemonConfigError::BelowMinimum`]: the effective `max_cron_sleep` is below the floor.
    pub fn load_layered(
        file_source: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
        overrides: &DaemonOverrides,
    ) -> Result<Self, DaemonConfigError> {
        let raw: RawDaemonConfig = match file_source {
            Some(src) => toml::from_str(src).map_err(|e| DaemonConfigError::Toml(e.to_string()))?,
            None => RawDaemonConfig::default(),
        };
        let mut cfg = Self {
            daemon: raw.daemon,
            whistle: raw.whistle,
            style: raw.style,
            interpreters: raw.interpreters,
            dog: raw.dog,
        };
        if let Some(v) = env("SHEP_LOG_JSON") {
            cfg.daemon.log_json = match parse_daemon_bool(&v) {
                Some(value) => value,
                None => return Err(DaemonConfigError::BadEnvValue("SHEP_LOG_JSON", v)),
            };
        }
        if let Some(v) = env("SHEP_LOG_LEVEL") {
            let Some(level) = LogLevel::from_name(&v) else {
                return Err(DaemonConfigError::BadEnvValue("SHEP_LOG_LEVEL", v));
            };
            cfg.daemon.log_level = level;
        }
        if let Some(v) = env("SHEP_SOCKET") {
            cfg.daemon.socket = Some(std::path::PathBuf::from(v));
        }
        // Whichever layer last wrote max_cron_sleep is the key the refusal
        // names, so the operator is pointed at the thing they can edit.
        // Validating per layer instead would stop a good override from
        // rescuing a bad one below it.
        let mut max_cron_sleep_key = "max_cron_sleep";
        if let Some(v) = env("SHEP_MAX_CRON_SLEEP") {
            let parsed = v
                .parse::<UpDuration>()
                .map_err(|_| DaemonConfigError::BadEnvValue("SHEP_MAX_CRON_SLEEP", v))?;
            cfg.daemon.max_cron_sleep = Some(parsed);
            max_cron_sleep_key = "SHEP_MAX_CRON_SLEEP";
        }
        if let Some(value) = overrides.log_json {
            cfg.daemon.log_json = value;
        }
        if let Some(value) = overrides.log_level {
            cfg.daemon.log_level = value;
        }
        if let Some(value) = &overrides.socket {
            cfg.daemon.socket = Some(value.clone());
        }
        if let Some(value) = overrides.max_cron_sleep {
            cfg.daemon.max_cron_sleep = Some(value);
            max_cron_sleep_key = "--max-cron-sleep";
        }
        cfg.validate(max_cron_sleep_key)?;
        Ok(cfg)
    }

    /// Checks every invariant a `DaemonConfig` carries, whatever layers
    /// produced it. One call site, at the bottom of [`Self::load_layered`]: validating
    /// per layer would stop a good `--max-cron-sleep` from rescuing a
    /// broken `shep.toml`.
    ///
    /// `key` is provenance: the spelling the operator actually set, so the
    /// refusal names the thing they can edit. Private; guards construction,
    /// not a later mutation of a `pub` field.
    ///
    /// # Errors
    /// - [`DaemonConfigError::BelowMinimum`]: `max_cron_sleep` is under the floor.
    fn validate(&self, key: &'static str) -> Result<(), DaemonConfigError> {
        if let Some(value) = self.daemon.max_cron_sleep
            && value < MIN_CRON_SLEEP
        {
            return Err(DaemonConfigError::BelowMinimum {
                key,
                value,
                min: MIN_CRON_SLEEP,
            });
        }
        Ok(())
    }
}

/// The CLI-flag layer of `file < env < flags` (spec §5).
///
/// Every field is `Option`: `None` means the flag was absent and the
/// layer below wins. Nothing here validates; [`DaemonConfig::load_layered`]
/// runs the single validation pass once, after all three layers.
///
/// `#[non_exhaustive]`: this type grows a field whenever the hidden
/// `daemon` subcommand grows a flag. Build one with [`Self::new`] and the
/// chained setters.
///
/// `Debug` is derived, not redacted: four values, none a secret.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonOverrides {
    /// `--log-json`
    pub log_json: Option<bool>,
    /// `--log-level`
    pub log_level: Option<LogLevel>,
    /// `--socket`
    pub socket: Option<PathBuf>,
    /// `--max-cron-sleep`
    pub max_cron_sleep: Option<UpDuration>,
}

impl DaemonOverrides {
    /// An empty layer: every flag absent.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the `--log-json` override.
    #[must_use]
    pub fn log_json(mut self, value: Option<bool>) -> Self {
        self.log_json = value;
        self
    }

    /// Sets the `--log-level` override.
    #[must_use]
    pub fn log_level(mut self, value: Option<LogLevel>) -> Self {
        self.log_level = value;
        self
    }

    /// Sets the `--socket` override.
    #[must_use]
    pub fn socket(mut self, value: Option<PathBuf>) -> Self {
        self.socket = value;
        self
    }

    /// Sets the `--max-cron-sleep` override.
    #[must_use]
    pub fn max_cron_sleep(mut self, value: Option<UpDuration>) -> Self {
        self.max_cron_sleep = value;
        self
    }
}

/// The boolean grammar of `shep.toml` and the `SHEP_*` environment: `1`,
/// `0`, `true`, `false`, and nothing else.
///
/// One function so the file/env layer and the `--log-json` flag cannot
/// drift. clap's own `BoolishValueParser` additionally accepts
/// `yes`/`no`/`y`/`n`/`on`/`off`; using it would widen the grammar on the
/// flag side only.
///
/// Not a general boolean parser: exporting it only under this name keeps
/// exactly one answer to what counts as true in shep's daemon config.
#[must_use]
pub fn parse_daemon_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// Error type returned from [`DaemonConfig::load`].
///
/// `#[non_exhaustive]`: every `[daemon]` key this crate learns to validate
/// brings its own rejection reason, and `deferred.md`'s daemon-config
/// flags layer is a whole set of them at once.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonConfigError {
    /// `shep.toml` is invalid TOML (carries the parser message)
    Toml(String),
    /// A `SHEP_*` env var held an unparseable value (var name, value)
    BadEnvValue(&'static str, String),
    /// A `[daemon]` duration is below the floor that keeps the daemon from
    /// spinning. Carries the key the user actually set: the TOML key or
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
    // `key`/`min` operands swapped): this is what actually reaches
    // `shepd.err.log` on exit code 4.
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

    /// `adopted_dogs` needs `default` (existing files predate it) and
    /// `deny_unknown_fields` (a typo names a binary shep would otherwise
    /// run at the daemon's own trust level).
    #[test]
    fn adopted_dogs_default_empty_and_round_trip_by_name() {
        let bare = DaemonConfig::load(Some("[daemon]\nlog_json = true\n"), &no_env).unwrap();
        assert!(bare.daemon.adopted_dogs.is_empty());

        let src = r#"
[daemon]
enabled_dogs = ["metrics", "otel"]

[daemon.adopted_dogs]
otel = "/usr/local/bin/shep-otel"
"#;
        let cfg = DaemonConfig::load(Some(src), &no_env).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["metrics", "otel"]);
        assert_eq!(
            cfg.daemon.adopted_dogs.get("otel"),
            Some(&std::path::PathBuf::from("/usr/local/bin/shep-otel"))
        );
        assert!(
            !cfg.daemon.adopted_dogs.contains_key("metrics"),
            "a name with no entry here is a built-in, and that is the whole distinction"
        );
    }

    #[test]
    fn env_overrides_file() {
        let env = |k: &str| (k == "SHEP_LOG_JSON").then(|| "true".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nlog_json = false"), &env).unwrap();
        assert!(cfg.daemon.log_json);
    }

    // The default decides what an unconfigured operator actually sees:
    // `Off` hides every warn-and-continue arm, `Info` and below bury them.
    // fails if `#[default]` moves, or a serde default disagrees with it.
    #[test]
    fn an_unset_log_level_is_warn() {
        assert_eq!(
            DaemonConfig::load(None, &no_env).unwrap().daemon.log_level,
            LogLevel::Warn
        );
    }

    // `as_str`, `from_name` and serde's `rename_all` are three separate
    // spellings of the same mapping; nothing else keeps them in agreement.
    // fails if any one drifts from the other two.
    #[test]
    fn every_log_level_name_means_the_same_thing_in_the_file_and_the_environment() {
        let levels = [
            LogLevel::Off,
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ];
        for level in levels {
            let name = level.as_str();
            assert_eq!(LogLevel::from_name(name), Some(level), "from_name({name})");

            let file = format!("[daemon]\nlog_level = \"{name}\"");
            let cfg = DaemonConfig::load(Some(&file), &no_env).unwrap();
            assert_eq!(cfg.daemon.log_level, level, "[daemon] log_level = {name:?}");

            let env = |k: &str| (k == "SHEP_LOG_LEVEL").then(|| name.to_string());
            let cfg = DaemonConfig::load(None, &env).unwrap();
            assert_eq!(cfg.daemon.log_level, level, "SHEP_LOG_LEVEL={name}");
        }
    }

    // fails if the env read is placed before the file is folded in, or
    // omitted entirely.
    #[test]
    fn env_log_level_beats_file_value() {
        let env = |k: &str| (k == "SHEP_LOG_LEVEL").then(|| "debug".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nlog_level = \"error\""), &env).unwrap();
        assert_eq!(cfg.daemon.log_level, LogLevel::Debug);
    }

    // fails if the env read swallows an unknown name and leaves the
    // default standing, or if the grammar is widened to accept
    // case-insensitive names.
    #[test]
    fn bad_env_log_level_is_a_typed_error() {
        for value in ["verbose", "WARN", ""] {
            let env = |k: &str| (k == "SHEP_LOG_LEVEL").then(|| value.to_string());
            assert_eq!(
                DaemonConfig::load(None, &env),
                Err(DaemonConfigError::BadEnvValue(
                    "SHEP_LOG_LEVEL",
                    value.to_string()
                )),
                "SHEP_LOG_LEVEL={value:?}"
            );
        }
    }

    // fails if a `#[serde(other)]` catch-all swallows a misspelled level
    // into a silent fallback. Pins "unknown variant", not just the
    // misspelled name, since that phrase is the only one exclusive to the
    // level being rejected rather than to some other unknown key.
    #[test]
    fn bad_file_log_level_is_a_toml_error() {
        let err = DaemonConfig::load(Some("[daemon]\nlog_level = \"verbose\""), &no_env)
            .expect_err("a misspelled level must not parse");
        let DaemonConfigError::Toml(message) = err else {
            panic!("a misspelled level is a TOML error, not {err:?}");
        };
        assert!(
            message.contains("unknown variant `verbose`"),
            "the error must reject the level's own name, not some other key: {message:?}"
        );
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

    // fails if `[whistle]` becomes an unrecognized section: `shep daemon`
    // would exit 4, and an operator who turned control tools on would
    // lose their shepherd on the next boot.
    #[test]
    fn a_whistle_section_parses_and_defaults_to_refusing_control() {
        let cfg = DaemonConfig::load(Some("[whistle]\nallow_control = true\n"), &no_env).unwrap();
        assert!(cfg.whistle.allow_control);

        let absent = DaemonConfig::load(Some("[daemon]\nlog_level = \"info\"\n"), &no_env).unwrap();
        assert!(
            !absent.whistle.allow_control,
            "a file with no [whistle] section leaves control off"
        );

        // A present-but-empty table is the only input that reaches
        // `allow_control`'s own field-level default; an absent `[whistle]`
        // table is filled by the container-level default instead.
        let empty_table = DaemonConfig::load(Some("[whistle]\n"), &no_env).unwrap();
        assert!(
            !empty_table.whistle.allow_control,
            "a [whistle] section with no keys leaves control off"
        );
    }

    // fails if the section silently accepts a key it does not implement. A
    // `[whistle] allow_contro = true` typo that parsed would leave an
    // operator certain the gate was open and whistle certain it was shut,
    // with nothing anywhere saying otherwise.
    #[test]
    fn a_misspelled_whistle_key_is_a_named_error() {
        let err =
            DaemonConfig::load(Some("[whistle]\nallow_contro = true\n"), &no_env).unwrap_err();
        let DaemonConfigError::Toml(message) = err else {
            panic!("a misspelled key is a TOML error, got {err:?}")
        };
        // The full quoted form, not the bare stem: `"allow_control"` also
        // contains `"allow_contro"`, so a stem-only assertion could pass
        // on a message naming only what serde expected.
        assert!(
            message.contains("unknown field `allow_contro`"),
            "the message quotes the key that was not understood: {message}"
        );
    }

    // fails if validation moves back into a per-layer position: a later
    // layer must be able to rescue a value an earlier one would reject.
    #[test]
    fn a_flag_rescues_a_below_floor_file_value() {
        let cfg = DaemonConfig::load_layered(
            Some("[daemon]\nmax_cron_sleep = \"500\"\n"),
            &no_env,
            &DaemonOverrides::new().max_cron_sleep(Some(UpDuration::from_millis(300_000))),
        )
        .unwrap();
        assert_eq!(
            cfg.daemon.max_cron_sleep,
            Some(UpDuration::from_millis(300_000))
        );
    }

    // fails if a below-floor FLAG is accepted, or if the refusal names the
    // TOML key the operator did not set.
    #[test]
    fn a_below_floor_flag_is_refused_naming_the_flag() {
        let err = DaemonConfig::load_layered(
            None,
            &no_env,
            &DaemonOverrides::new().max_cron_sleep(Some(UpDuration::from_millis(500))),
        )
        .unwrap_err();
        assert_eq!(
            err,
            DaemonConfigError::BelowMinimum {
                key: "--max-cron-sleep",
                value: UpDuration::from_millis(500),
                min: MIN_CRON_SLEEP,
            }
        );
        assert!(err.to_string().contains("--max-cron-sleep"), "got: {err}");
    }

    // fails if a flag stops beating the env layer.
    #[test]
    fn a_flag_beats_the_environment() {
        let env = |k: &str| (k == "SHEP_LOG_LEVEL").then(|| "trace".to_string());
        let cfg = DaemonConfig::load_layered(
            Some("[daemon]\nlog_level = \"error\"\n"),
            &env,
            &DaemonOverrides::new().log_level(Some(LogLevel::Info)),
        )
        .unwrap();
        assert_eq!(cfg.daemon.log_level, LogLevel::Info);
    }

    // Pins that `load` and `load_layered` agree when no flag is set. Does
    // not catch a `bool` standing in for `Option<bool>`, since both sides
    // route through the same code; other tests in this file and cli_e2e
    // pin that instead.
    #[test]
    fn an_absent_flag_leaves_every_lower_layer_alone() {
        let src = "[daemon]\nlog_json = true\nlog_level = \"debug\"\nsocket = \"/tmp/s.sock\"\n";
        let layered =
            DaemonConfig::load_layered(Some(src), &no_env, &DaemonOverrides::new()).unwrap();
        let plain = DaemonConfig::load(Some(src), &no_env).unwrap();
        assert_eq!(layered, plain);
    }

    #[test]
    fn the_bool_grammar_is_exactly_four_spellings() {
        assert_eq!(parse_daemon_bool("1"), Some(true));
        assert_eq!(parse_daemon_bool("0"), Some(false));
        assert_eq!(parse_daemon_bool("true"), Some(true));
        assert_eq!(parse_daemon_bool("false"), Some(false));
        for wider in ["yes", "no", "on", "off", "TRUE", "y"] {
            assert_eq!(
                parse_daemon_bool(wider),
                None,
                "{wider} must not be a boolean here"
            );
        }
    }

    // fails if `[interpreters]` stops parsing as a plain extension ->
    // interpreter map, or if a value written as a bare word (no quotes
    // needed, since these are ordinary TOML strings) fails to round-trip.
    #[test]
    fn interpreters_parses_as_an_extension_map() {
        let cfg = DaemonConfig::load(
            Some("[interpreters]\njs = \"node\"\npy = \"python3\"\n"),
            &no_env,
        )
        .unwrap();
        assert_eq!(cfg.interpreters.get("js").map(String::as_str), Some("node"));
        assert_eq!(
            cfg.interpreters.get("py").map(String::as_str),
            Some("python3")
        );
        assert_eq!(cfg.interpreters.len(), 2);
    }

    // An empty/absent `[interpreters]` must not fail a `shep.toml` that
    // never mentions the section, which is most of them until an operator
    // (or the first-run scaffold) writes one.
    #[test]
    fn interpreters_defaults_to_empty() {
        assert!(
            DaemonConfig::load(None, &no_env)
                .unwrap()
                .interpreters
                .is_empty()
        );
        assert!(
            DaemonConfig::load(Some("[daemon]\nlog_json = true\n"), &no_env)
                .unwrap()
                .interpreters
                .is_empty()
        );
    }

    // `[interpreters]` values are arbitrary extension keys, not a fixed
    // field set, so `deny_unknown_fields` (which governs struct fields)
    // must not reject an extension this build has never heard of.
    #[test]
    fn an_unrecognised_extension_is_not_an_unknown_field() {
        let cfg = DaemonConfig::load(Some("[interpreters]\nlua = \"lua5.4\"\n"), &no_env).unwrap();
        assert_eq!(
            cfg.interpreters.get("lua").map(String::as_str),
            Some("lua5.4")
        );
    }

    // A value that is not a string (an operator's `js = 5`, say) is still
    // a parse error, shep-core's usual fail-loudly-at-parse-time rule.
    #[test]
    fn a_non_string_interpreter_value_is_a_parse_error() {
        assert!(DaemonConfig::load(Some("[interpreters]\njs = 5\n"), &no_env).is_err());
    }

    #[test]
    fn debug_redacts_dog_values() {
        // Dog tables carry things like webhook URLs; a lazy derive(Debug)
        // would land them in daemon logs. Exact string pinned so that
        // regression fails here instead of leaking a secret.
        let cfg = DaemonConfig::load(Some("[dog.metrics]\nport = 9615"), &no_env).unwrap();
        assert_eq!(
            format!("{cfg:?}"),
            "DaemonConfig { daemon: DaemonSection { log_json: false, log_level: Warn, socket: None, enabled_dogs: [], adopted_dogs: {}, max_cron_sleep: None }, whistle: WhistleSection { allow_control: false }, style: StyleSection { level: None }, interpreters: {}, dog: <1 tables> }"
        );
    }
}
