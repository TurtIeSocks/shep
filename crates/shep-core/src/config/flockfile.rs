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
    ///   failure, carrying the backend's message. Json5 additionally rejects
    ///   sources nested past a depth of 64 before ever handing them to the
    ///   backend parser (json5's recursive-descent parser stack-overflows on
    ///   deeply nested input rather than returning an error).
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
                if json5_nesting_depth(source) > MAX_JSON5_NESTING_DEPTH {
                    return Err(FlockfileError::Json5(
                        "nesting depth exceeds 64".to_string(),
                    ));
                }
                json5::from_str(source).map_err(|e| FlockfileError::Json5(e.to_string()))?
            }
        };
        if raw.apps.is_empty() {
            return Err(FlockfileError::NoApps);
        }
        Ok(Self { apps: raw.apps })
    }
}

// json5's recursive-descent parser stack-overflows (SIGABRT, not a catchable
// error) on documents nested a few thousand levels deep — reproduced locally
// around ~4500 levels. 64 is far beyond anything a real Flockfile needs (the
// deepest legitimate nesting, a probe object inside an app object inside the
// app array inside the root object, is 4) and comfortably clear of the crash
// threshold.
const MAX_JSON5_NESTING_DEPTH: u32 = 64;

// Scans `source` for the maximum number of concurrently open `[`/`{`
// brackets. Skips characters inside quoted strings (single or double,
// backslash-escaped) and inside `//`/`/* */` comments, so bracket-like (and
// quote-like) characters there don't distort the count — a `'` inside a `//
// don't nest` comment must NOT be able to flip the scanner into string mode
// and make it ignore real brackets that follow (that was exactly the bug in
// the first version of this guard: it failed OPEN, letting an over-deep
// document reach json5 and crash it).
//
// Fails CLOSED on anything that isn't clean, well-terminated JSON5 lexing:
// an unterminated `/* ...` comment or an unterminated string at EOF returns
// `u32::MAX`, which always exceeds `MAX_JSON5_NESTING_DEPTH` — better to
// reject a malformed document than to under-count it and let it through.
// Saturating add/sub: a real document would fail the depth check long
// before `u32` could overflow.
fn json5_nesting_depth(source: &str) -> u32 {
    let mut depth: u32 = 0;
    let mut max_depth: u32 = 0;
    let mut in_string: Option<char> = None;
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(quote) = in_string {
            match c {
                '\\' => {
                    chars.next(); // skip the escaped character
                }
                q if q == quote => in_string = None,
                _ => {}
            }
            continue;
        }
        match c {
            '/' if chars.peek() == Some(&'/') => {
                chars.next(); // consume the second '/'
                for c2 in chars.by_ref() {
                    if c2 == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next(); // consume the '*'
                let mut prev = '\0';
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if prev == '*' && c2 == '/' {
                        closed = true;
                        break;
                    }
                    prev = c2;
                }
                if !closed {
                    return u32::MAX; // unterminated block comment
                }
            }
            '"' | '\'' => in_string = Some(c),
            '[' | '{' => {
                depth = depth.saturating_add(1);
                max_depth = max_depth.max(depth);
            }
            ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if in_string.is_some() {
        return u32::MAX; // unterminated string
    }
    max_depth
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

/// Finds the Flockfile in a directory (spec §5 ten-name order)
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
        // tempdir gives RAII cleanup instead of a manual remove_dir_all, so
        // a failing assertion above can't leak the directory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("flockfile.json"), "{}").unwrap();
        std::fs::write(dir.path().join("Flockfile.yaml"), "").unwrap();
        assert_eq!(
            discover(dir.path()),
            Some(dir.path().join("Flockfile.yaml"))
        );
        std::fs::write(dir.path().join("Flockfile.toml"), "").unwrap();
        assert_eq!(
            discover(dir.path()),
            Some(dir.path().join("Flockfile.toml"))
        );
    }

    #[test]
    fn json5_beyond_max_nesting_depth_is_rejected_without_crashing() {
        // json5's backend parser stack-overflows (SIGABRT) around ~4500
        // levels of nesting rather than returning an error — the depth
        // guard must reject this before ever calling into it. 5000 unclosed
        // `[` is nonsense JSON5, but the guard runs before any real parsing
        // is attempted, so that's fine.
        let src = "[".repeat(5000);
        assert_eq!(
            Flockfile::parse(&src, FlockFormat::Json5).unwrap_err(),
            FlockfileError::Json5("nesting depth exceeds 64".to_string())
        );
    }

    #[test]
    fn json5_nesting_depth_counts_concurrently_open_brackets() {
        let nested = format!("{}{}", "[".repeat(10), "]".repeat(10));
        assert_eq!(json5_nesting_depth(&nested), 10);
    }

    #[test]
    fn json5_nesting_depth_ignores_brackets_inside_strings() {
        let src = r#"{ "a": "[[[[[[[[[[", "b": "esc\"aped [ too" }"#;
        assert_eq!(json5_nesting_depth(src), 1); // only the outer `{`
    }

    #[test]
    fn json5_legitimately_nested_doc_still_parses() {
        // A probe object nested inside an app object inside the app array
        // inside the root object — depth 4, the deepest a real Flockfile
        // schema allows, and well under the depth-64 guard.
        let src = r#"{
            app: [{
                name: "web",
                script: "./srv",
                readiness_probe: { kind: "http", target: "http://localhost/x" },
            }],
        }"#;
        let flock = Flockfile::parse(src, FlockFormat::Json5).unwrap();
        assert_eq!(flock.apps.len(), 1);
    }

    #[test]
    fn json5_line_comment_apostrophe_does_not_hide_deep_nesting() {
        // Regression: a `'` inside a `//` comment must not flip the scanner
        // into string mode and make it ignore every bracket that follows —
        // that would let an over-deep document slip past the guard straight
        // into json5's stack overflow.
        let src = format!("// don't nest\n{}", "[".repeat(5000));
        assert_eq!(
            Flockfile::parse(&src, FlockFormat::Json5).unwrap_err(),
            FlockfileError::Json5("nesting depth exceeds 64".to_string())
        );
    }

    #[test]
    fn json5_block_comment_apostrophe_does_not_hide_deep_nesting() {
        let src = format!("/* it's fine */\n{}", "[".repeat(5000));
        assert_eq!(
            Flockfile::parse(&src, FlockFormat::Json5).unwrap_err(),
            FlockfileError::Json5("nesting depth exceeds 64".to_string())
        );
    }

    #[test]
    fn json5_benign_comment_does_not_undercount_a_real_document() {
        // Same depth-4 document as `json5_legitimately_nested_doc_still_parses`,
        // plus a comment (apostrophe included) that must be skipped cleanly
        // rather than throwing off the count.
        let src = r#"{
            /* it's the app list */
            app: [{
                name: "web",
                script: "./srv",
                readiness_probe: { kind: "http", target: "http://localhost/x" },
            }],
        }"#;
        let flock = Flockfile::parse(src, FlockFormat::Json5).unwrap();
        assert_eq!(flock.apps.len(), 1);
    }
}
