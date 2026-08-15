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
    /// dog — an argv branch of the shep binary itself. That is the whole of
    /// the distinction, and it is deliberately NOT recorded inside
    /// `[dog.<name>]`: that table is the dog's own opaque configuration, and
    /// a shep-owned key inside it would collide with a third-party dog's
    /// schema.
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
/// `SHEP_LOG_LEVEL`, lowercase and nothing else — the same closed grammar
/// `log_json` accepts, so a typo is a startup error naming the value rather
/// than a level silently reverting to the default.
///
/// The default is [`LogLevel::Warn`]. The daemon's records are dominated by
/// warn-and-continue arms — an app that asked to be watched and could not be,
/// a cron pattern that would not parse, a memory ceiling a process tree
/// crossed — and each one is the *only* account of a decision the operator
/// cannot otherwise see. [`LogLevel::Debug`] adds per-decision detail that
/// fires per dropped restart and per child metric sample, which is a firehose
/// on a busy flock rather than a slightly noisier log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Nothing at all — the daemon writes no records of its own.
    Off,
    /// Only faults the daemon could not work around.
    Error,
    /// Faults the daemon worked around, and what working around them cost.
    #[default]
    Warn,
    /// Lifecycle milestones: the daemon came up, the daemon is going down.
    Info,
    /// Per-decision detail — every restart weighed, every metric sampled.
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
/// Zero makes every sleep return immediately and turns the loop into a hot
/// spin that re-derives a schedule as fast as the runtime allows — while
/// still firing correctly, which is what makes it hard to attribute. Low
/// milliseconds are the same fault with a smaller constant. One second is a
/// floor no legitimate configuration wants to be under: a five-field cron
/// pattern cannot name anything finer than a minute, so even this is sixty
/// times more often than the tightest schedule can fire.
const MIN_CRON_SLEEP: UpDuration = UpDuration::from_millis(1_000);

/// The `[whistle]` section
///
/// One key, and it is a gate rather than a tuning knob: `shep whistle`'s four
/// control tools (`start_sheep`, `stop_sheep`, `restart_sheep`,
/// `reload_sheep`) exist only when this is `true`, and its five read-only
/// tools exist regardless.
///
/// **This lives in the shepherd's config file and nowhere else.** There is no
/// `--allow-control` flag and no `SHEP_*` variable, deliberately: spec §14.7
/// rules that whistle's gate is daemon config because config is auditable and
/// flags are per-invocation. That is a legibility argument, not a
/// containment one — `--home`/`SHEP_HOME` already choose which `shep.toml`
/// gets read, so a flag would open nothing those don't already; a boolean in
/// a file just leaves a diff and an mtime an operator can audit.
///
/// The shepherd itself never reads this key — `shep whistle` reads the file
/// directly, at startup, in its own process. It is here because this struct is
/// the grammar of `shep.toml`, and a `[whistle]` section the grammar did not
/// know about would be an unknown field: `RawDaemonConfig` denies those, so
/// before this existed a file that turned the gate on stopped the shepherd
/// from booting at all.
///
/// `Debug` is derived rather than redacted (IR-41): one boolean, no secret,
/// nothing a `{:?}` could leak.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WhistleSection {
    /// Whether `shep whistle` offers its control tools. Default `false`.
    pub allow_control: bool,
}

/// Parsed daemon configuration with raw per-dog sections
///
/// Dog sections stay untyped here: each dog deserializes its own
/// `[dog.<name>]` table so dog config schemas live with the dog code.
///
/// `#[non_exhaustive]`: this struct has grown a section per phase — `whistle`
/// most recently — and each one would otherwise be a breaking change for an
/// out-of-tree struct literal. That is IR-20's ordinary reasoning applied to
/// a struct. **It is not a validation gate**, and this type is deliberately
/// not the proof token [`crate::config::ResolvedApp`] is: the attribute blocks
/// struct literals and functional-update syntax from outside this crate, but
/// not field mutation, and [`Self::default`] followed by an assignment to
/// `daemon.max_cron_sleep` reaches an unvalidated value without a literal.
///
/// The contract is therefore stated, not enforced. [`Self::load`] and
/// [`Self::load_layered`] are the validating constructors; a caller that
/// mutates a loaded config afterwards is out of contract, and shep-core does
/// not detect it and, with public fields, cannot.
///
/// That is the right trade here because nothing ever *receives* one of these.
/// `ResolvedApp` protects a property of travel — the supervisor is handed one
/// and must trust normalization it cannot see. Every production site loads a
/// `DaemonConfig` and consumes it within a few lines (`run_daemon` renders it
/// straight into `BootOptions`; shep-daemon's `dogs` reads one
/// `[dog.<name>]` table; shep-cli's `whistle::gate` reads one boolean), and
/// the daemon holds a `BootOptions`, not this. Guarding the one
/// `max_cron_sleep` floor against a caller who is already out of contract
/// would cost accessors for every field of every section, including a
/// `BTreeMap<String, toml::Table>` two crates legitimately read. If an
/// out-of-tree caller ever does need to mutate and re-check, the answer is to
/// make `validate` public — one line, non-breaking — not to privatise the
/// fields. `docs/specs/deferred.md` records this as resolved.
///
/// Nothing in the repository observes the attribute itself: it is invisible
/// inside the defining crate, and seeing it needs a `trybuild` compile-fail
/// tier this project declined once already for `ProcessInfo` (see
/// `tests/process_info_builder_from_outside_the_crate.rs`, which admits the
/// same gap and is required to stay shep-core's only `tests/` file).
#[non_exhaustive]
#[derive(Clone, Default, PartialEq)]
pub struct DaemonConfig {
    /// The `[daemon]` section
    pub daemon: DaemonSection,
    /// The `[whistle]` section
    pub whistle: WhistleSection,
    /// Raw `[dog.<name>]` sections keyed by dog name
    pub dog: BTreeMap<String, toml::Table>,
}

/// Debug implementation does not leak dog config values (IR-41)
impl fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonConfig")
            .field("daemon", &self.daemon)
            .field("whistle", &self.whistle)
            .field("dog", &format_args!("<{} tables>", self.dog.len()))
            .finish()
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RawDaemonConfig {
    daemon: DaemonSection,
    whistle: WhistleSection,
    dog: BTreeMap<String, toml::Table>,
}

impl DaemonConfig {
    /// Builds config from optional file source + environment overrides
    ///
    /// `file < env`, validated. Equivalent to
    /// [`Self::load_layered`] with an empty [`DaemonOverrides`]; unchanged
    /// for every existing caller.
    ///
    /// # Errors
    ///
    /// - [`DaemonConfigError::Toml`] — the file source is invalid TOML.
    /// - [`DaemonConfigError::BadEnvValue`] — a `SHEP_*` value is not
    ///   parseable (`SHEP_LOG_JSON` accepts `1|0|true|false`;
    ///   `SHEP_LOG_LEVEL` accepts a [`LogLevel`] name;
    ///   `SHEP_MAX_CRON_SLEEP` parses as an [`UpDuration`]).
    /// - [`DaemonConfigError::BelowMinimum`] — the effective
    ///   `max_cron_sleep` (file, `SHEP_MAX_CRON_SLEEP`, whichever won) is
    ///   below the floor.
    pub fn load(
        file_source: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, DaemonConfigError> {
        Self::load_layered(file_source, env, &DaemonOverrides::new())
    }

    /// Builds config from optional file source + environment + CLI-flag
    /// overrides
    ///
    /// `file < env < flags` (spec §5), validated exactly once, at the end —
    /// see the private `validate` method below for why validating per layer
    /// instead would be wrong.
    ///
    /// # Errors
    ///
    /// - [`DaemonConfigError::Toml`] — the file source is invalid TOML.
    /// - [`DaemonConfigError::BadEnvValue`] — a `SHEP_*` value is not
    ///   parseable (`SHEP_LOG_JSON` accepts `1|0|true|false`;
    ///   `SHEP_LOG_LEVEL` accepts a [`LogLevel`] name;
    ///   `SHEP_MAX_CRON_SLEEP` parses as an [`UpDuration`]).
    /// - [`DaemonConfigError::BelowMinimum`] — the effective
    ///   `max_cron_sleep` (file, `SHEP_MAX_CRON_SLEEP`, or
    ///   `--max-cron-sleep`, whichever won) is below the floor.
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
        // Provenance needs no tracking beyond this one flag: whichever layer
        // last wrote max_cron_sleep is the key the refusal names, so the
        // operator is pointed at the thing they can actually edit.
        // Validating each layer as it is read instead would make a good
        // SHEP_MAX_CRON_SLEEP unable to rescue a broken shep.toml, or a good
        // --max-cron-sleep unable to rescue either, which is not what
        // "file < env < flags" means.
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
    /// produced it.
    ///
    /// One call site, at the bottom of [`Self::load_layered`], and that is
    /// the point: validating per layer would stop a good `--max-cron-sleep`
    /// from rescuing a broken `shep.toml`, which is not what
    /// `file < env < flags` means. The same reasoning the env layer already
    /// carries, extended one layer up.
    ///
    /// `key` is provenance — the spelling the operator actually set, so the
    /// refusal names the thing they can edit.
    ///
    /// Private. It guards construction, not mutation: a caller outside this
    /// crate can assign to a `pub` field afterwards and this never runs
    /// again. See the type's own doc for why that is accepted rather than
    /// closed.
    ///
    /// # Errors
    ///
    /// - [`DaemonConfigError::BelowMinimum`] — `max_cron_sleep` is under the
    ///   floor that keeps the cron loop from spinning.
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
/// Every field is `Option`: `None` means the flag was absent and the layer
/// below wins. Nothing here validates — [`DaemonConfig::load_layered`] runs
/// [`DaemonConfig`]'s single validation pass once, after all three layers,
/// so a flag can rescue a file the layer below would have rejected.
///
/// `#[non_exhaustive]` because this type grows a field every time the hidden
/// `daemon` subcommand grows a flag; that is anticipated by construction, not
/// hypothetical — the same field-growth reasoning [`DaemonConfig`] carries,
/// and like it, not a claim that the value was validated. Build one with
/// [`Self::new`] and the chained setters — the consuming-self shape
/// `ProcessInfo::builder` already uses in this workspace, and the shape
/// `#[non_exhaustive]` requires, since it rules out struct literals and
/// functional update from outside.
///
/// `Debug` is derived rather than redacted (IR-41): four values, none of
/// them a secret — a socket path and a log level are already visible in
/// `ps`.
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
    /// An empty layer — every flag absent.
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

/// The boolean grammar of `shep.toml` and the `SHEP_*` environment: `1`, `0`,
/// `true`, `false`, and nothing else.
///
/// One function so the file/env layer and the `--log-json` flag cannot drift.
/// clap's own `BoolishValueParser` additionally accepts
/// `yes`/`no`/`y`/`n`/`on`/`off`; using it would widen the grammar on the flag
/// side only, and widening an input grammar beyond spec is a named drift risk
/// on this project.
///
/// The name says whose grammar this is on purpose. It is not a general
/// boolean parser and must not be widened into one: the whole value of
/// exporting it is that there is exactly one answer to "what counts as true
/// in shep's daemon config".
#[must_use]
pub fn parse_daemon_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// Error type returned from [`DaemonConfig::load`]
///
/// `#[non_exhaustive]`: every `[daemon]` key this crate learns to validate
/// brings its own rejection reason, and `deferred.md`'s daemon-config flags
/// layer is a whole set of them at once (IR-20 — the same reasoning
/// [`NormalizeError`](crate::config::NormalizeError) states for the per-app
/// side).
#[non_exhaustive]
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

    /// fails if `adopted_dogs` is not `default`ed, or is declared outside
    /// `deny_unknown_fields`'s reach: a `shep.toml` written before it
    /// existed must still load, and a typo'd key must still be refused.
    /// Both halves matter — dropping `default` breaks every existing file,
    /// and the table is the one place an operator names a binary shep is
    /// about to run at the daemon's own trust level.
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

    // The default decides what an operator who configured nothing actually
    // sees, and every warn-and-continue arm in the daemon rides on it. `Off`
    // would hide all of them; `Info` and below would bury them.
    //
    // fails if the `#[default]` attribute moves to another variant, or if a
    // serde default invents a level the enum's own `Default` does not agree
    // with.
    #[test]
    fn an_unset_log_level_is_warn() {
        assert_eq!(
            DaemonConfig::load(None, &no_env).unwrap().daemon.log_level,
            LogLevel::Warn
        );
    }

    // One owner for the six names (Rule 9). `as_str`, `from_name` and serde's
    // `rename_all` are three separate spellings of the same mapping, and
    // nothing but this makes them agree.
    //
    // fails if any one of the three drifts from the other two — a `rename_all`
    // dropped or changed to `snake_case`, an `as_str` arm returning the
    // variant name verbatim, a `from_name` arm mapped to the wrong level.
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

    // fails if the env read is placed before the file is folded in, or omitted
    // entirely — the shape that leaves a knob parsed and never applied, which
    // is exactly what `log_json` itself was until this level joined it.
    #[test]
    fn env_log_level_beats_file_value() {
        let env = |k: &str| (k == "SHEP_LOG_LEVEL").then(|| "debug".to_string());
        let cfg = DaemonConfig::load(Some("[daemon]\nlog_level = \"error\""), &env).unwrap();
        assert_eq!(cfg.daemon.log_level, LogLevel::Debug);
    }

    // fails if the env read swallows an unknown name and leaves the default
    // standing — a daemon that silently logs at `warn` after being asked for
    // `trace` is indistinguishable from one with nothing to say. Also fails if
    // the grammar is widened to accept case-insensitive names, which
    // `SHEP_LOG_JSON` does not accept either.
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

    // fails if the enum grows a `#[serde(other)]` catch-all, which would turn
    // a misspelled level in `shep.toml` into a silent fallback instead of the
    // startup error `ExitCode::InvalidConfig` reports.
    //
    // Also fails if `log_level` is dropped from `DaemonSection` altogether,
    // which the variant alone cannot tell apart: `deny_unknown_fields` answers
    // an undefined key with the same `Toml` variant. Asserting the message
    // merely mentions `verbose` would not separate them either — a
    // `deny_unknown_fields` error echoes the offending source line, the value
    // included, which was checked against a key this section really does not
    // define. Only "unknown *variant*" is exclusive to the level's own name
    // being rejected, so that is what is pinned; the wording is serde's, and
    // it is also what an operator reads out of `ExitCode::InvalidConfig`.
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

    // fails if `[whistle]` stops being a section the shepherd will start
    // with. This is not a hypothetical: `RawDaemonConfig` denies unknown
    // fields, so before this section existed the same input returned
    // `DaemonConfigError::Toml` and `shep daemon` exited 4 — an operator who
    // turned whistle's control tools on lost their shepherd on the next
    // boot.
    #[test]
    fn a_whistle_section_parses_and_defaults_to_refusing_control() {
        let cfg = DaemonConfig::load(Some("[whistle]\nallow_control = true\n"), &no_env).unwrap();
        assert!(cfg.whistle.allow_control);

        let absent = DaemonConfig::load(Some("[daemon]\nlog_level = \"info\"\n"), &no_env).unwrap();
        assert!(
            !absent.whistle.allow_control,
            "a file with no [whistle] section leaves control off"
        );

        // The third case, and it is a DIFFERENT code path from the second:
        // an absent `[whistle]` table is filled by `RawDaemonConfig`'s own
        // container-level `#[serde(default)]`, which never consults the
        // field's serde default at all. A present-but-empty table is the
        // only input that does. Without this line, a field-level
        // `#[serde(default = "...")]` on `allow_control` could flip the gate
        // open and no test in this file would notice — which is exactly
        // what the first draft's mutation assumed it was proving.
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
        // contains `"allow_contro"`, so an assertion on the stem would pass
        // on a message that named only what serde EXPECTED and never quoted
        // what the operator actually wrote. serde's `deny_unknown_fields`
        // message is "unknown field `allow_contro`, expected
        // `allow_control`", and the closing backtick is what distinguishes
        // the two.
        assert!(
            message.contains("unknown field `allow_contro`"),
            "the message quotes the key that was not understood: {message}"
        );
    }

    // fails if validation moves back into a per-layer position — the flags
    // layer must be able to rescue a file the layer below would reject,
    // which is what `file < env < flags` means. Same rule the env layer's
    // own comment already states.
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

    // Pins that `load` (the two-layer file+env path) and `load_layered`
    // (the three-layer file+env+flags path `load` itself delegates to)
    // agree when no flag is set. It does NOT catch a `bool` field standing
    // in for `Option<bool>`: `load` routes through `load_layered` on both
    // sides of the `assert_eq!`, so a mutation on that line lands on both
    // sides alike and this test stays green. That guard is
    // `file_sets_values_and_keeps_dog_sections_raw` and `env_overrides_file`
    // in this file, and cli_e2e's
    // `shep_log_json_makes_the_daemons_own_records_json` — each pins an
    // actual value coming through a specific layer, which a flattened
    // `Option<bool>` would get wrong.
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

    #[test]
    fn debug_redacts_dog_values() {
        // Dog tables carry things like webhook URLs; a lazy derive(Debug)
        // would land them in daemon logs. Exact string pinned so that
        // regression fails here instead of leaking a secret.
        let cfg = DaemonConfig::load(Some("[dog.metrics]\nport = 9615"), &no_env).unwrap();
        assert_eq!(
            format!("{cfg:?}"),
            "DaemonConfig { daemon: DaemonSection { log_json: false, log_level: Warn, socket: None, enabled_dogs: [], adopted_dogs: {}, max_cron_sleep: None }, whistle: WhistleSection { allow_control: false }, dog: <1 tables> }"
        );
    }
}
