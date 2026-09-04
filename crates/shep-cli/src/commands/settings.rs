//! The settings screen's own reader and writer: the one place `shep.toml`'s
//! raw text meets [`DaemonConfig`]'s opinion about whether a value is legal.
//!
//! `shep_toml`'s own module doc says [`DaemonConfig::load`] is "the one
//! place the SHAPE of the file is decided" and that `shep_toml` "only ever
//! adds or removes the handful of keys each verb owns". Validation
//! therefore does not go there, and it does not go here as a change to
//! either type -- it goes in [`apply_setting`], which is the one function
//! that ever puts a mutated [`ShepToml`] and a real [`DaemonConfig::load`]
//! in the same place.
//!
//! [`load_settings`] answers the opposite question, and answers it from the
//! document, not from [`DaemonConfig`]: every `[daemon]` and `[whistle]`
//! section is `#[serde(default)]`, so a loaded config cannot tell "the
//! operator wrote this at its own default value" from "nobody ever wrote
//! it" -- see `shep_toml::ShepToml::daemon_log_json`'s own doc for that
//! argument in full. Reading a value out of a loaded [`DaemonConfig`] and
//! calling it the file's would silently destroy the distinction this
//! screen exists to show.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use shep_core::config::daemon::{DaemonSection, WhistleSection};
use shep_core::config::{DaemonConfig, DaemonConfigError, parse_daemon_bool};
use shep_core::values::UpDuration;

use crate::commands::shep_toml::{ShepToml, ShepTomlError};
use crate::dog::BUILT_IN_DOGS;
use crate::style::{StyleLevel, StyleSource};

// No new source enum. `crate::style::StyleSource` already has exactly
// `Flag`, `Env`, `Config`, `Default`, and "which layer decided" is one
// concept, so this reuses it rather than declaring a twin. Its `Display`
// spells `Flag` and `Env` as `--style` and `$SHEP_STYLE`, which are
// style-specific, and that is correct here because only `style_level`
// ever produces those two variants: the shepherd's own env and flags are
// invisible to this process, so a `[daemon]` field is only ever `Config`
// or `Default`.
//
// `Config` is not a claim that the shepherd is using the value. It says
// the key is in the file. See the design spec, "Two things decision 11
// does not cover".

/// One scalar as the screen shows it.
///
/// `Debug` is derived rather than redacted (IR-41): a rendered string and
/// a layer name, no secret, nothing a `{:?}` could leak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarView {
    /// Already rendered for display, defaults resolved.
    pub value: String,
    /// Which layer this value came from.
    pub source: StyleSource,
}

/// Which scalar an edit names.
///
/// `Debug` is derived rather than redacted (IR-41): a bare field name, no
/// secret, nothing a `{:?}` could leak.
///
/// All six variants are constructed by the lookout settings screen's own
/// `Settings::rows`, one per scalar row, in the fixed order they appear
/// below.
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

/// One edit, ready to apply.
///
/// `Debug` is derived rather than redacted (IR-41): the field name and the
/// text an operator typed into the screen's own editor, no secret among
/// the six scalars this reaches.
///
/// Constructed by `lookout::app`: `App::cycle_setting` for the four cycled
/// scalars, and `App::on_settings_text_key`'s `TextApply` arm for `socket`
/// and `max_cron_sleep`, the two this crate's free-text editor can also
/// [`Self::Unset`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingEdit {
    /// Write `field` to `value`.
    Set {
        /// Which scalar.
        field: SettingField,
        /// The text as typed or chosen -- unparsed until [`apply_setting`]
        /// validates it.
        value: String,
    },
    /// Remove `field`'s key, returning it to the compiled default.
    ///
    /// Only [`SettingField::Socket`] and [`SettingField::MaxCronSleep`]
    /// reach this: [`SettingField::StyleLevel`] is owned by `shep style`,
    /// which cannot clear it either, and the other three are not optional
    /// (design spec decision 5).
    Unset {
        /// Which scalar.
        field: SettingField,
    },
}

/// Everything the screen reads off disk in one go.
///
/// `Debug` is derived rather than redacted (IR-41): every field here is
/// already a rendered [`ScalarView`] or a [`DogView`], neither of which
/// carries a secret -- a dog's own webhook token lives in `dogs.toml`,
/// which this type never reads.
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
    /// Every candidate dog: [`BUILT_IN_DOGS`] plus every `adopted_dogs`
    /// key, sorted, deduplicated.
    pub dogs: Vec<DogView>,
}

/// One row of the dogs table.
///
/// `Debug` is derived rather than redacted (IR-41): a name, a bool and an
/// adopted binary's path, none of which is a secret -- the dog's own
/// config, which can be, lives in `dogs.toml` and this type never touches
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogView {
    /// The dog's name.
    pub name: String,
    /// Whether `[daemon] enabled_dogs` names it.
    pub enabled: bool,
    /// `None` for a built-in dog; the adopted binary's path otherwise.
    pub adopted_path: Option<PathBuf>,
}

/// What [`apply_setting`] can fail with.
///
/// `Debug` is derived rather than redacted (IR-41): [`Self::Config`]
/// forwards to [`ShepTomlError`]'s own manually redacted `Debug`, which is
/// where a secret in the document (a dog's webhook token in an
/// un-migrated `[dog.<name>]` table) would actually surface, and
/// [`Self::Invalid`] carries either `DaemonConfig::load`'s own refusal
/// message or this module's own -- both describe a key and a rule, never
/// a value read back out of the file.
///
/// Not `#[non_exhaustive]`, the same reasoning [`ShepTomlError`] states
/// for itself in this same crate: shep-cli has no published surface, so
/// there is no downstream `match` outside this binary for the attribute
/// to protect, and every exhaustive `match` on this enum is one we want
/// the compiler to break the moment a new failure mode is added.
#[derive(Debug)]
pub enum SettingError {
    /// [`ShepToml::try_edit`]'s own setup or write failed -- the lock, the
    /// read, or the rename.
    Config(ShepTomlError),
    /// [`DaemonConfig::load`] refused the document this edit would have
    /// written, or the edit itself was not a legal value for its field
    /// (an unparseable boolean, an unrecognised style level). Carries the
    /// loader's own message, or this function's own, so the operator is
    /// told which key and why.
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

/// Reads the snapshot. Takes no lock -- see [`ShepToml::read_only`], the
/// door this reads through.
///
/// `socket_default` is the socket this lookout is connected over
/// (`ShepPaths::socket`), so an absent `[daemon] socket` shows the live
/// answer by construction rather than a recomputed guess. `style` is the
/// already-resolved pair `lib.rs`'s `resolve_style` returns: every
/// `[daemon]`/`[whistle]` field can only ever read as [`StyleSource::Config`]
/// or [`StyleSource::Default`], because the layers that would make it
/// [`StyleSource::Env`] or [`StyleSource::Flag`] belong to a different
/// process, but `[style] level` is lookout's own and can be either of
/// those two as well.
///
/// # Errors
/// [`ShepTomlError::Io`] if `path` exists and could not be read.
/// [`ShepTomlError::Parse`] if `path` exists and is not valid TOML.
///
/// Called by `lookout::mod::run_ui`'s `Effect::LoadSettings` arm, inside the
/// `spawn_blocking` closure that arm runs the whole read through.
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

    Ok(SettingsSnapshot {
        log_level,
        log_json,
        socket,
        max_cron_sleep,
        allow_control,
        style_level,
        dogs: dog_candidates(&doc),
    })
}

/// [`DaemonSection::default`]'s own `max_cron_sleep`, rendered for the
/// screen's default row. Read straight off the compiled struct rather than
/// a typed literal (this module's own instruction, and a real one: the
/// floor and the daemon's own fallback both live in `shep-daemon`, outside
/// this crate). Today that default is `None` -- the daemon falls back to
/// its own private `DEFAULT_MAX_CRON_SLEEP`, unreachable from here -- so
/// "not set" is the honest answer rather than a guessed duration; a
/// [`Some`] compiled default would render through [`UpDuration`]'s own
/// `Display` unchanged.
fn render_compiled_max_cron_sleep(default: Option<UpDuration>) -> String {
    default.map_or_else(|| "not set".to_string(), |value| value.to_string())
}

/// Every candidate dog: [`BUILT_IN_DOGS`] plus every name
/// [`ShepToml::adopted_dog_names`] returns, sorted and deduplicated by
/// [`BTreeSet`], each paired with whether [`ShepToml::enabled_dog_names`]
/// names it and its adopted path if it has one.
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

/// Applies one edit under the config lock, validating before it saves.
///
/// Goes through [`ShepToml::try_edit`]: the task-2 setter or unsetter runs
/// first, then the mutated document is rendered
/// ([`ShepToml::rendered`]) and handed to [`DaemonConfig::load`] with
/// `&|_| None` in place of an env layer -- the same reasoning
/// `commands::daemon`'s `reload_with_wait` already gives at its own
/// pre-flight check: this process's environment is not the shepherd's, so
/// layering it would refuse a file the shepherd would have survived and
/// pass one it would not. A loader `Err` becomes
/// [`SettingError::Invalid`] and returns from the closure before
/// [`ShepToml::try_edit`] ever calls its own `save`, which is what leaves
/// the file untouched down to its inode.
///
/// # Errors
/// [`SettingError::Config`] -- the lock, the read or the write itself
/// failed, or the field this document already held the key as a shape
/// the setter cannot write into (see [`ShepTomlError::WrongShape`]).
/// [`SettingError::Invalid`] -- the value is not legal for its field
/// (an unparseable boolean, an unrecognised style level, a duration
/// [`DaemonConfig::load`] refuses), or [`SettingEdit::Unset`] named a
/// field that has no unset form.
///
/// Called by `lookout::mod::run_ui`'s `Effect::WriteSetting` arm, the same
/// `spawn_blocking` shape [`load_settings`]'s own note describes.
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

/// The `Set` half of [`apply_setting`]'s match, factored out for
/// readability: one field per arm, each reaching straight for its own
/// task-2 setter.
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
/// unsetter to reach; every other field refuses, per [`SettingEdit::Unset`]'s
/// own doc.
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

/// Delegates to [`parse_daemon_bool`] rather than hand-rolling a near
/// duplicate: that function already accepts `"1"`/`"0"`/`"true"`/`"false"`
/// for `SHEP_LOG_JSON`, and [`StyleLevel::parse`]'s own doc states the
/// principle this follows -- the screen's grammar and the environment's
/// grammar for the same field must never be able to silently disagree on
/// what counts as valid input. An operator who knows `SHEP_LOG_JSON=1`
/// works must not be refused typing `1` here.
fn parse_bool_field(value: &str) -> Result<bool, SettingError> {
    parse_daemon_bool(value)
        .ok_or_else(|| SettingError::Invalid(format!("{value} is not a valid boolean")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The style pair every test that does not care about `[style]` uses:
    /// a resolved level with the source the config layer would report.
    fn style_fixture() -> (StyleLevel, StyleSource) {
        (StyleLevel::Full, StyleSource::Config)
    }

    fn socket_default_fixture() -> PathBuf {
        PathBuf::from("/var/run/shep-settings-fixture.sock")
    }

    #[test]
    fn a_fresh_home_reads_every_scalar_as_the_default() {
        // What `scaffold_first_run_interpreters` actually leaves behind, and
        // the state most operators open this screen in.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        ShepToml::edit(&path, ShepToml::write_starter_interpreters).unwrap();

        let snap = load_settings(&path, &socket_default_fixture(), style_fixture()).unwrap();
        assert_eq!(snap.log_level.source, StyleSource::Default);
        assert_eq!(snap.log_level.value, "warn");
        assert_eq!(snap.log_json.source, StyleSource::Default);
        assert_eq!(snap.allow_control.source, StyleSource::Default);
        assert_eq!(snap.max_cron_sleep.source, StyleSource::Default);
    }

    #[test]
    fn a_declared_scalar_reads_as_config_even_at_its_default_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nlog_level = \"warn\"\n").unwrap();

        let snap = load_settings(&path, &socket_default_fixture(), style_fixture()).unwrap();
        assert_eq!(snap.log_level.value, "warn");
        assert_eq!(
            snap.log_level.source,
            StyleSource::Config,
            "a key written to its own default is still a key someone wrote"
        );
    }

    // `.ino()` needs `std::os::unix::fs::MetadataExt`, so this one test is
    // unix-only; the CI Windows leg still runs the other six.
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
