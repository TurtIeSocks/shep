//! The settings screen's own reader and writer: the one place `shep.toml`'s
//! raw text meets [`DaemonConfig`]'s opinion about whether a value is legal.
//!
//! Validation lives in [`apply_setting`], the only function that puts a
//! mutated [`ShepToml`] and a real [`DaemonConfig::load`] in one place.
//!
//! [`load_settings`] reads the document, never a loaded [`DaemonConfig`]:
//! every `[daemon]` and `[whistle]` section is `#[serde(default)]`, so a
//! loaded config cannot tell a key written at its own default from a key
//! nobody wrote, which is the distinction this screen exists to show.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use shep_core::config::daemon::{DaemonSection, WhistleSection};
use shep_core::config::{DaemonConfig, DaemonConfigError, parse_daemon_bool};
use shep_core::values::UpDuration;

use crate::commands::shep_toml::{ShepToml, ShepTomlError};
use crate::dog::BUILT_IN_DOGS;
use crate::style::{StyleLevel, StyleSource};

// `crate::style::StyleSource` is reused for "which layer decided". Only
// `style_level` ever reads as `Flag` or `Env`: the shepherd's own env and
// flags are invisible here, so a `[daemon]` field is `Config` or `Default`.
// `Config` says the key is in the file, not that the shepherd is using it.

/// One scalar as the screen shows it
///
/// `Debug` is derived, not redacted: a rendered string and a layer name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarView {
    /// Already rendered for display, defaults resolved.
    pub value: String,
    /// Which layer this value came from.
    pub source: StyleSource,
}

/// Which scalar an edit names
///
/// `Debug` is derived, not redacted: a bare field name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingField {
    /// `[daemon] log_level`.
    LogLevel,
    /// `[daemon] log_json`.
    LogJson,
    /// `[daemon] socket`.
    Socket,
    /// `[daemon] max_cron_sleep`.
    MaxCronSleep,
    /// `[whistle] allow_control`.
    AllowControl,
    /// `[style] level`.
    StyleLevel,
}

/// One edit, ready to apply
///
/// `Debug` is derived, not redacted: none of the six scalars this reaches
/// is a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingEdit {
    /// Write `field` to `value`.
    Set {
        /// Which scalar.
        field: SettingField,
        /// The text as typed or chosen, unparsed until [`apply_setting`]
        /// validates it.
        value: String,
    },
    /// Remove `field`'s key, returning it to the compiled default.
    ///
    /// Only [`SettingField::Socket`] and [`SettingField::MaxCronSleep`]
    /// reach this; the other four have no unset form.
    Unset {
        /// Which scalar.
        field: SettingField,
    },
}

/// Everything the screen reads off disk in one go
///
/// `Debug` is derived, not redacted: a dog's own webhook token lives in
/// `dogs.toml`, which this type never reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSnapshot {
    /// `[daemon] log_level`.
    pub log_level: ScalarView,
    /// `[daemon] log_json`.
    pub log_json: ScalarView,
    /// `[daemon] socket`.
    pub socket: ScalarView,
    /// `[daemon] max_cron_sleep`.
    pub max_cron_sleep: ScalarView,
    /// `[whistle] allow_control`.
    pub allow_control: ScalarView,
    /// `[style] level`, resolved.
    pub style_level: ScalarView,
    /// What `[style] level` says in the document, or `None` when the
    /// document never wrote it.
    ///
    /// The one field needing both halves: `--style` and `$SHEP_STYLE` are
    /// lookout's own, so `style_level` is the level in force and this is
    /// the level on disk. Cycling from the resolved one would propose a
    /// no-op write under `$SHEP_STYLE=bare` over a file saying `full`.
    pub style_level_in_file: Option<String>,
    /// Every candidate dog: [`BUILT_IN_DOGS`] plus every `adopted_dogs`
    /// key, sorted, deduplicated.
    pub dogs: Vec<DogView>,
}

/// One row of the dogs table
///
/// `Debug` is derived, not redacted: a name, a bool and a binary's path.
/// The dog's own config, which can hold a secret, lives in `dogs.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogView {
    /// The dog's name.
    pub name: String,
    /// Whether `[daemon] enabled_dogs` names it.
    pub enabled: bool,
    /// `None` for a built-in dog; the adopted binary's path otherwise.
    pub adopted_path: Option<PathBuf>,
}

/// What [`apply_setting`] can fail with
///
/// `Debug` is derived, not redacted: [`Self::Config`] forwards to
/// [`ShepTomlError`]'s own redacted `Debug`, and [`Self::Invalid`] carries
/// a refusal message naming a key and a rule, never a value from the file.
#[derive(Debug)]
pub enum SettingError {
    /// [`ShepToml::try_edit`]'s setup or write failed: the lock, the read,
    /// or the rename.
    Config(ShepTomlError),
    /// [`DaemonConfig::load`] refused the document this edit would have
    /// written, or the value is not legal for its field. Carries the
    /// refusal message, so the operator is told which key and why.
    Invalid(String),
}

impl std::fmt::Display for SettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(err) => write!(f, "{err}"),
            Self::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl core::error::Error for SettingError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Config(err) => Some(err),
            Self::Invalid(_) => None,
        }
    }
}

impl From<ShepTomlError> for SettingError {
    fn from(err: ShepTomlError) -> Self {
        Self::Config(err)
    }
}

/// Reads the snapshot, taking no lock: see [`ShepToml::read_only`]
///
/// `socket_default` is the socket this lookout is connected over, so an
/// absent `[daemon] socket` shows the live answer rather than a guess.
/// `style` is `resolve_style`'s already-resolved pair.
///
/// # Errors
/// [`ShepTomlError::Io`] if `path` exists and could not be read.
/// [`ShepTomlError::Parse`] if `path` exists and is not valid TOML.
pub fn load_settings(
    path: &Path,
    socket_default: &Path,
    style: (StyleLevel, StyleSource),
) -> Result<SettingsSnapshot, ShepTomlError> {
    let doc = ShepToml::read_only(path)?;
    let daemon_default = DaemonSection::default();
    let whistle_default = WhistleSection::default();

    let log_level = match doc.daemon_log_level() {
        Some(value) => ScalarView {
            value,
            source: StyleSource::Config,
        },
        None => ScalarView {
            value: daemon_default.log_level.as_str().to_string(),
            source: StyleSource::Default,
        },
    };
    let log_json = match doc.daemon_log_json() {
        Some(value) => ScalarView {
            value: value.to_string(),
            source: StyleSource::Config,
        },
        None => ScalarView {
            value: daemon_default.log_json.to_string(),
            source: StyleSource::Default,
        },
    };
    let socket = match doc.daemon_socket() {
        Some(value) => ScalarView {
            value: value.display().to_string(),
            source: StyleSource::Config,
        },
        None => ScalarView {
            value: socket_default.display().to_string(),
            source: StyleSource::Default,
        },
    };
    let max_cron_sleep = match doc.daemon_max_cron_sleep() {
        Some(value) => ScalarView {
            value,
            source: StyleSource::Config,
        },
        None => ScalarView {
            value: render_compiled_max_cron_sleep(daemon_default.max_cron_sleep),
            source: StyleSource::Default,
        },
    };
    let allow_control = match doc.whistle_allow_control() {
        Some(value) => ScalarView {
            value: value.to_string(),
            source: StyleSource::Config,
        },
        None => ScalarView {
            value: whistle_default.allow_control.to_string(),
            source: StyleSource::Default,
        },
    };
    let (level, source) = style;
    let style_level = ScalarView {
        value: level.to_string(),
        source,
    };
    let style_level_in_file = doc.style_level();

    Ok(SettingsSnapshot {
        log_level,
        log_json,
        socket,
        max_cron_sleep,
        allow_control,
        style_level,
        style_level_in_file,
        dogs: dog_candidates(&doc),
    })
}

/// [`DaemonSection::default`]'s own `max_cron_sleep`, rendered for the
/// screen's default row
///
/// A `None` default renders as "not set": the daemon's own fallback lives
/// in `shep-daemon` and is unreachable from here, so naming a duration
/// would be a guess.
fn render_compiled_max_cron_sleep(default: Option<UpDuration>) -> String {
    default.map_or_else(|| "not set".to_string(), |value| value.to_string())
}

/// Every candidate dog: [`BUILT_IN_DOGS`] plus every adopted name, sorted
/// and deduplicated, each paired with whether it is enabled and its path.
fn dog_candidates(doc: &ShepToml) -> Vec<DogView> {
    let enabled = doc.enabled_dog_names();
    let mut names: BTreeSet<String> = BUILT_IN_DOGS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    names.extend(doc.adopted_dog_names());
    names
        .into_iter()
        .map(|name| {
            let adopted_path = doc.adopted_dog_path(&name);
            let is_enabled = enabled.contains(&name);
            DogView {
                name,
                enabled: is_enabled,
                adopted_path,
            }
        })
        .collect()
}

/// Applies one edit under the config lock, validating before it saves
///
/// The mutated document is loaded with `&|_| None` in place of an env
/// layer: this process's environment is not the shepherd's. A loader `Err`
/// returns before [`ShepToml::try_edit`] saves, leaving the file untouched
/// down to its inode.
///
/// # Errors
/// [`SettingError::Config`] if the lock, the read or the write failed, or
/// the document holds the key as a shape the setter cannot write into.
/// [`SettingError::Invalid`] if the value is not legal for its field, or
/// [`SettingEdit::Unset`] named a field with no unset form.
pub fn apply_setting(path: &Path, edit: &SettingEdit) -> Result<(), SettingError> {
    ShepToml::try_edit(path, |doc| -> Result<(), SettingError> {
        match edit {
            SettingEdit::Set { field, value } => set_field(doc, *field, value)?,
            SettingEdit::Unset { field } => unset_field(doc, *field)?,
        }
        let rendered = doc.rendered();
        DaemonConfig::load(Some(&rendered), &|_| None)
            .map_err(|err: DaemonConfigError| SettingError::Invalid(err.to_string()))?;
        Ok(())
    })
}

/// The `Set` half of [`apply_setting`]'s match, one field per arm.
fn set_field(doc: &mut ShepToml, field: SettingField, value: &str) -> Result<(), SettingError> {
    match field {
        SettingField::LogLevel => doc.set_daemon_log_level(value)?,
        SettingField::LogJson => doc.set_daemon_log_json(parse_bool_field(value)?)?,
        SettingField::Socket => doc.set_daemon_socket(Path::new(value))?,
        SettingField::MaxCronSleep => doc.set_daemon_max_cron_sleep(value)?,
        SettingField::AllowControl => doc.set_whistle_allow_control(parse_bool_field(value)?)?,
        SettingField::StyleLevel => {
            let level = StyleLevel::parse(value).ok_or_else(|| {
                SettingError::Invalid(format!("{value} does not name a style level"))
            })?;
            doc.set_style_level(level)?;
        }
    }
    Ok(())
}

/// The `Unset` half of [`apply_setting`]'s match. Only
/// [`SettingField::Socket`] and [`SettingField::MaxCronSleep`] have an
/// unsetter; every other field refuses.
fn unset_field(doc: &mut ShepToml, field: SettingField) -> Result<(), SettingError> {
    match field {
        SettingField::Socket => doc.unset_daemon_socket(),
        SettingField::MaxCronSleep => doc.unset_daemon_max_cron_sleep(),
        _ => {
            return Err(SettingError::Invalid(
                "this field has no unset form".to_string(),
            ));
        }
    }
    Ok(())
}

/// Delegates to [`parse_daemon_bool`] so the screen's grammar and
/// `SHEP_LOG_JSON`'s cannot disagree: typing `1` here must work too.
fn parse_bool_field(value: &str) -> Result<bool, SettingError> {
    parse_daemon_bool(value)
        .ok_or_else(|| SettingError::Invalid(format!("{value} is not a valid boolean")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The style pair every test that does not care about `[style]` uses.
    fn style_fixture() -> (StyleLevel, StyleSource) {
        (StyleLevel::Full, StyleSource::Config)
    }

    fn socket_default_fixture() -> PathBuf {
        PathBuf::from("/var/run/shep-settings-fixture.sock")
    }

    #[test]
    fn a_fresh_home_reads_every_scalar_as_the_default() {
        // What `scaffold_first_run_interpreters` leaves behind.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        ShepToml::edit(&path, ShepToml::write_starter_interpreters).unwrap();

        let snap = load_settings(&path, &socket_default_fixture(), style_fixture()).unwrap();
        assert_eq!(snap.log_level.source, StyleSource::Default);
        assert_eq!(snap.log_level.value, "warn");
        assert_eq!(snap.log_json.source, StyleSource::Default);
        assert_eq!(snap.allow_control.source, StyleSource::Default);
        assert_eq!(snap.max_cron_sleep.source, StyleSource::Default);
        assert_eq!(
            snap.style_level_in_file, None,
            "a fresh home has never written [style], so the reader must say so"
        );
    }

    #[test]
    fn a_declared_scalar_reads_as_config_even_at_its_default_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nlog_level = \"warn\"\n\n[style]\nlevel = \"bare\"\n",
        )
        .unwrap();

        let snap = load_settings(&path, &socket_default_fixture(), style_fixture()).unwrap();
        assert_eq!(snap.log_level.value, "warn");
        assert_eq!(
            snap.log_level.source,
            StyleSource::Config,
            "a key written to its own default is still a key someone wrote"
        );
        assert_eq!(
            snap.style_level_in_file,
            Some("bare".to_string()),
            "load_settings must read [style] level off the real document, \
             not just carry the resolved level `style_fixture` supplies"
        );
    }

    // `.ino()` needs `std::os::unix::fs::MetadataExt`, so unix only.
    #[cfg(unix)]
    #[test]
    fn a_value_the_loader_refuses_leaves_the_file_byte_identical() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        let before = "# mine\n[daemon]\nlog_level = \"debug\"\n";
        std::fs::write(&path, before).unwrap();
        let inode_before = std::fs::metadata(&path).unwrap().ino();

        let refusal = apply_setting(
            &path,
            &SettingEdit::Set {
                field: SettingField::MaxCronSleep,
                value: "500ms".into(),
            },
        );

        assert!(matches!(refusal, Err(SettingError::Invalid(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            inode_before,
            "a refusal must not stage and rename, which is what try_edit buys"
        );
    }

    #[test]
    fn the_refusal_carries_the_loaders_own_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "").unwrap();

        let Err(SettingError::Invalid(message)) = apply_setting(
            &path,
            &SettingEdit::Set {
                field: SettingField::MaxCronSleep,
                value: "500ms".into(),
            },
        ) else {
            panic!("a value under the floor must be refused");
        };
        assert!(
            message.contains("max_cron_sleep"),
            "the operator has to be told which key: {message}"
        );
    }

    #[test]
    fn unsetting_an_optional_field_returns_it_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nmax_cron_sleep = \"30s\"\n").unwrap();

        apply_setting(
            &path,
            &SettingEdit::Unset {
                field: SettingField::MaxCronSleep,
            },
        )
        .unwrap();

        let snap = load_settings(&path, &socket_default_fixture(), style_fixture()).unwrap();
        assert_eq!(snap.max_cron_sleep.source, StyleSource::Default);
    }

    #[test]
    fn every_built_in_dog_is_a_candidate_even_when_nothing_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "").unwrap();

        let snap = load_settings(&path, &socket_default_fixture(), style_fixture()).unwrap();
        let names: Vec<&str> = snap.dogs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["bark", "metrics"]);
        assert!(snap.dogs.iter().all(|d| !d.enabled));
    }

    #[test]
    fn an_adopted_dog_joins_the_candidates_and_carries_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nenabled_dogs = [\"otel\"]\n\n[daemon.adopted_dogs]\notel = \"/usr/local/bin/shep-otel\"\n",
        )
        .unwrap();

        let snap = load_settings(&path, &socket_default_fixture(), style_fixture()).unwrap();
        let otel = snap.dogs.iter().find(|d| d.name == "otel").unwrap();
        assert!(otel.enabled);
        assert_eq!(
            otel.adopted_path.as_deref(),
            Some(Path::new("/usr/local/bin/shep-otel"))
        );
        let metrics = snap.dogs.iter().find(|d| d.name == "metrics").unwrap();
        assert_eq!(metrics.adopted_path, None, "a built-in dog has no path");
    }
}
