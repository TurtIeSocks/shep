//! Flockfile: discovery and multi-format parsing
//!
//! One document shape across formats: a list of app tables under the `app`
//! key (`[[app]]` in TOML). Parsing is strict serde — no code execution;
//! `.js` configs are the CLI's job (it shells out to node and feeds the
//! resulting JSON through [`FlockFormat::Json`]).

use core::fmt;

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::AppConfig;

/// A parsed Flockfile: the declared flock
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flockfile {
    /// App entries in declaration order
    pub apps: Vec<AppConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFlockfile {
    #[serde(default, rename = "app")]
    apps: Vec<AppConfig>,
}

/// Input format of a Flockfile
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlockFormat {
    /// `Flockfile.toml` — `[[app]]` tables
    Toml,
    /// `.yaml`/`.yml`
    Yaml,
    /// Strict JSON
    Json,
    /// JSON5 (comments, trailing commas)
    Json5,
}

impl FlockFormat {
    /// Maps a file extension to its format (`None` = unsupported, e.g. `.js`)
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            "json" => Some(Self::Json),
            "json5" => Some(Self::Json5),
            _ => None,
        }
    }
}

impl Flockfile {
    /// Parses Flockfile source text in the given format
    ///
    /// # Errors
    ///
    /// - Format variants ([`FlockfileError::Toml`] etc.) — backend parse
    ///   failure, carrying the backend's message.
    /// - [`FlockfileError::NoApps`] — parsed fine but declared no apps.
    pub fn parse(source: &str, format: FlockFormat) -> Result<Self, FlockfileError> {
        let raw: RawFlockfile = match format {
            FlockFormat::Toml => {
                toml::from_str(source).map_err(|e| FlockfileError::Toml(e.to_string()))?
            }
            FlockFormat::Yaml => {
                serde_yml::from_str(source).map_err(|e| FlockfileError::Yaml(e.to_string()))?
            }
            FlockFormat::Json => {
                serde_json::from_str(source).map_err(|e| FlockfileError::Json(e.to_string()))?
            }
            FlockFormat::Json5 => {
                json5::from_str(source).map_err(|e| FlockfileError::Json5(e.to_string()))?
            }
        };
        if raw.apps.is_empty() {
            return Err(FlockfileError::NoApps);
        }
        Ok(Self { apps: raw.apps })
    }
}

const DISCOVERY_ORDER: [&str; 10] = [
    "Flockfile.toml",
    "Flockfile.yaml",
    "Flockfile.yml",
    "Flockfile.json",
    "Flockfile.json5",
    "flockfile.toml",
    "flockfile.yaml",
    "flockfile.yml",
    "flockfile.json",
    "flockfile.json5",
];

/// Finds the Flockfile in a directory (spec §5 order, extended with the
/// `.yml`/`.json5` spellings — spec updated to this ten-name list)
#[must_use]
pub fn discover(dir: &Path) -> Option<PathBuf> {
    DISCOVERY_ORDER
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// Error type returned from [`Flockfile::parse`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlockfileError {
    /// TOML backend rejected the source (carries its message)
    Toml(String),
    /// YAML backend rejected the source
    Yaml(String),
    /// JSON backend rejected the source
    Json(String),
    /// JSON5 backend rejected the source
    Json5(String),
    /// The document parsed but declared no apps
    NoApps,
}

impl fmt::Display for FlockfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(m) => write!(f, "invalid TOML Flockfile: {m}"),
            Self::Yaml(m) => write!(f, "invalid YAML Flockfile: {m}"),
            Self::Json(m) => write!(f, "invalid JSON Flockfile: {m}"),
            Self::Json5(m) => write!(f, "invalid JSON5 Flockfile: {m}"),
            Self::NoApps => f.write_str("Flockfile declares no apps"),
        }
    }
}

impl core::error::Error for FlockfileError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_array_of_tables() {
        let src = r#"
[[app]]
name = "web"
script = "./srv"

[[app]]
name = "worker"
script = "python3"
args = ["job.py"]
"#;
        let flock = Flockfile::parse(src, FlockFormat::Toml).unwrap();
        assert_eq!(flock.apps.len(), 2);
        assert_eq!(flock.apps[1].name, "worker");
    }

    #[test]
    fn json_and_json5_and_yaml() {
        let json = r#"{ "app": [{ "name": "web", "script": "./srv" }] }"#;
        assert_eq!(
            Flockfile::parse(json, FlockFormat::Json)
                .unwrap()
                .apps
                .len(),
            1
        );

        let json5 = r#"{ app: [{ name: "web", script: "./srv" }], /* comment */ }"#;
        assert_eq!(
            Flockfile::parse(json5, FlockFormat::Json5)
                .unwrap()
                .apps
                .len(),
            1
        );

        let yaml = "app:\n  - name: web\n    script: ./srv\n";
        assert_eq!(
            Flockfile::parse(yaml, FlockFormat::Yaml)
                .unwrap()
                .apps
                .len(),
            1
        );
    }

    #[test]
    fn empty_app_list_is_an_error() {
        assert_eq!(
            Flockfile::parse("app: []\n", FlockFormat::Yaml).unwrap_err(),
            FlockfileError::NoApps
        );
    }

    #[test]
    fn parse_errors_carry_the_backend_message() {
        match Flockfile::parse("not toml [[", FlockFormat::Toml).unwrap_err() {
            FlockfileError::Toml(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Toml error, got {other:?}"),
        }
    }

    #[test]
    fn format_from_path() {
        use std::path::Path;
        assert_eq!(
            FlockFormat::from_path(Path::new("Flockfile.toml")),
            Some(FlockFormat::Toml)
        );
        assert_eq!(
            FlockFormat::from_path(Path::new("f.yml")),
            Some(FlockFormat::Yaml)
        );
        assert_eq!(
            FlockFormat::from_path(Path::new("f.json5")),
            Some(FlockFormat::Json5)
        );
        assert_eq!(FlockFormat::from_path(Path::new("f.js")), None);
    }

    #[test]
    fn discover_prefers_toml_then_capitalized() {
        let dir = std::env::temp_dir().join(format!("shep-flock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("flockfile.json"), "{}").unwrap();
        std::fs::write(dir.join("Flockfile.yaml"), "").unwrap();
        assert_eq!(discover(&dir), Some(dir.join("Flockfile.yaml")));
        std::fs::write(dir.join("Flockfile.toml"), "").unwrap();
        assert_eq!(discover(&dir), Some(dir.join("Flockfile.toml")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
