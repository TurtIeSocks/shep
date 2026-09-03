//! `$SHEP_HOME/dogs.toml`, a dog's own settings.
//!
//! One table per dog, keyed by the name the dog was registered under, with
//! no prefix: `[metrics]` here is what `[dog.metrics]` was in `shep.toml`
//! before the move. The daemon serves a section verbatim over the socket as
//! `Response::DogSection` and never interprets it, so this type parses
//! exactly far enough to find the right table and no further.
//!
//! Hand-editable on purpose, and deliberately not a locked shep-owned store
//! like `overrides.json`. A dog's config is authored intent rather than
//! derived state, and an operator on a box with only a shell has to be able
//! to set one without a dashboard.

use core::fmt;
use std::collections::BTreeMap;

/// Every `[<dog>]` table in `dogs.toml`
///
/// `Debug` is redacted (IR-41): a dog section routinely carries a webhook
/// URL with a bearer token in it, and this type exists to be logged near
/// the boot path.
#[derive(Clone, Default, PartialEq)]
pub struct DogsConfig {
    /// Raw `[<name>]` tables keyed by dog name
    pub dog: BTreeMap<String, toml::Table>,
}

impl fmt::Debug for DogsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DogsConfig")
            .field("dog", &format_args!("<{} tables>", self.dog.len()))
            .finish()
    }
}

impl DogsConfig {
    /// Parses `dogs.toml`, or answers empty when there is no file
    ///
    /// # Errors
    ///
    /// - [`DogsConfigError::Toml`] when `source` is not valid TOML.
    pub fn load(source: Option<&str>) -> Result<Self, DogsConfigError> {
        let Some(source) = source else {
            return Ok(Self::default());
        };
        let dog = toml::from_str(source).map_err(DogsConfigError::Toml)?;
        Ok(Self { dog })
    }
}

/// Why `dogs.toml` could not be read
#[derive(Debug)]
// One variant today. `#[non_exhaustive]` so a second reading failure (a
// permissions error, once this type learns to open the file itself) is
// additive rather than breaking.
#[non_exhaustive]
pub enum DogsConfigError {
    /// The file is not valid TOML
    Toml(toml::de::Error),
}

impl fmt::Display for DogsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(err) => write!(f, "invalid TOML in dogs.toml: {err}"),
        }
    }
}

impl core::error::Error for DogsConfigError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Toml(err) => Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_loads_as_an_empty_map() {
        let config = DogsConfig::load(None).expect("None is not an error");
        assert!(config.dog.is_empty());
    }

    #[test]
    fn sections_are_keyed_by_name_with_no_prefix() {
        let source = "[metrics]\nbind = \"127.0.0.1:9615\"\n\n[bark.sinks]\noncall = { kind = \"discord\" }\n";
        let config = DogsConfig::load(Some(source)).expect("valid TOML");
        assert_eq!(
            config.dog.keys().collect::<Vec<_>>(),
            vec!["bark", "metrics"]
        );
        assert_eq!(
            config.dog["metrics"]["bind"].as_str(),
            Some("127.0.0.1:9615")
        );
    }

    #[test]
    fn invalid_toml_is_a_named_error() {
        let err = DogsConfig::load(Some("[metrics")).expect_err("unterminated table header");
        assert!(matches!(err, DogsConfigError::Toml(_)));
    }

    // IR-41: this map routinely holds webhook URLs with a bearer token in the
    // path. `Debug` is the one thing between such a token and any future
    // `tracing::debug!("{config:?}")`, so the exact string is pinned rather
    // than the shape.
    #[test]
    fn debug_redacts_every_dog_section() {
        let source =
            "[bark.sinks]\noncall = { url = \"https://discord.com/api/webhooks/SECRET\" }\n";
        let config = DogsConfig::load(Some(source)).expect("valid TOML");
        assert_eq!(format!("{config:?}"), "DogsConfig { dog: <1 tables> }");
    }
}
