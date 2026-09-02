//! Flockfile: discovery and multi-format parsing
//!
//! One document shape across formats: a list of app tables under the `app`
//! key (`[[app]]` in TOML). Parsing is strict serde — no code execution;
//! `.js` configs are the CLI's job (it shells out to node and feeds the
//! resulting JSON through [`FlockFormat::Json`]).

use core::fmt;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[cfg(feature = "schema")]
use schemars::Schema;

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;

/// A parsed Flockfile: the declared flock
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flockfile {
    /// App entries in declaration order
    pub apps: Vec<AppConfig>,
}

/// One app as the document declared it: the validated config, plus the keys
/// the document literally wrote.
///
/// The key set cannot be recovered from [`AppConfig`] afterwards.
/// `#[serde(default)]` gives every field a value, so a document naming four
/// keys deserializes identically to one naming forty. A later merge into a
/// running flock keys on what a template CLAIMS rather than on what its
/// values are, so the claim has to be carried out of the parser (see
/// [`Flockfile::parse_declared`]).
///
/// `Serialize`/`Deserialize` because this type is meant to travel inside a
/// wire request; the key sets are the whole reason such a request carries
/// this rather than a bare [`AppConfig`].
///
/// Derived `Debug` does not reach for [`AppConfig::env`]'s values: `config`'s
/// own manual `Debug` already redacts them (IR-41), and `declared_env` holds
/// only the env table's key NAMES, never the values behind them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredApp {
    /// The app, validated the same way [`Flockfile::parse`] validates one
    pub config: AppConfig,
    /// Top-level keys this app's table wrote, whatever their values
    pub declared: BTreeSet<String>,
    /// Keys inside this app's `env` table. Empty when `env` was not declared.
    pub declared_env: BTreeSet<String>,
}

// Forward-compat decision: application entries are locked to the `app` key
// on purpose — a typo'd key must fail loudly. `$schema` and `dog` are the
// two keys explicitly let in beside it (see their own field docs below); a
// future schema key gets added the same explicit way; older binaries then
// reject newer Flockfiles by design instead of silently ignoring config.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
// `rename` sets `schema_name`, which schemars uses as the root schema's
// `title`. The type is called `RawFlockfile` because it is the
// pre-validation twin of `Flockfile`; the document an operator writes is a
// Flockfile, and that is what the title has to say.
#[cfg_attr(feature = "schema", schemars(rename = "Flockfile"))]
#[serde(deny_unknown_fields)]
struct RawFlockfile {
    /// The editor's schema hint, read and discarded.
    ///
    /// This is the "future schema key" the comment above anticipated, added
    /// HERE explicitly rather than by relaxing `deny_unknown_fields`: a
    /// typo'd key must still fail loudly, and exactly one more key is now
    /// legal. shep does not validate against the named schema and makes no
    /// promise about it — it is a hint for the operator's editor, which is
    /// the only consumer that ever reads it.
    ///
    /// TOML Flockfiles do not need it: taplo's `#:schema <url>` directive is
    /// a comment, invisible to serde. JSON and JSON5 have no comment an
    /// editor agrees to look in, which is why this field exists at all.
    #[serde(default, rename = "$schema")]
    schema: Option<String>,
    /// A dog's own per-app configuration, read and discarded.
    ///
    /// Added the same way `$schema` was, explicitly rather than by relaxing
    /// `deny_unknown_fields`, so a typo'd key still fails loudly and exactly
    /// one more key is legal.
    ///
    /// It exists because the alternative is a Flockfile no daemon will accept.
    /// A dog that needs per-app configuration has nowhere to put it: shep-deploy
    /// wants a build command for the app it deploys, which belongs beside that
    /// app's declaration and nowhere else, and a Flockfile carrying one was
    /// refused outright by `shep start`. Measured 2026-08-28 against shep
    /// 0.1.8: an operator following shep-deploy's own README could not register
    /// their app at all. `unknown field `build`, expected `$schema` or `app``.
    ///
    /// It must BE a table. shep does not read what is inside it, does not
    /// validate it, and makes no promise about it. Those are two different
    /// claims and only the second one is a promise not to care: the dog that
    /// owns a key under this table is the only thing that understands it, and
    /// shep refusing a document because it does not recognise another
    /// program's config is a coupling neither side wants.
    ///
    /// Nested under one key rather than allowing loose top-level keys, so
    /// exactly one name is reserved and a typo anywhere else still fails.
    ///
    /// A map of ignored values rather than `IgnoredAny`, which would have
    /// accepted `dog = 5` and `dog = ["a"]` as happily as a table. Not reading
    /// what a dog wrote is the point; not caring whether it wrote a table at
    /// all is a different thing, and it would have made the one key this file
    /// adds the one key where a typo does not fail loudly.
    #[serde(default)]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<BTreeMap<String, serde_json::Value>>")
    )]
    dog: Option<BTreeMap<String, serde::de::IgnoredAny>>,
    #[serde(default, rename = "app")]
    apps: Vec<AppConfig>,
}

/// The committed Flockfile JSON Schema.
///
/// `include_str!` deliberately: it makes the file a compile-time input, so
/// deleting it fails the build and changing `AppConfig` fails the test
/// below with the command that fixes it. A committed schema nobody
/// regenerates is a lie with a filename, and the only reliable guard is one
/// that runs in `cargo test` rather than in a CI job somebody can forget.
///
/// It lives INSIDE this package, not at the repository root. `cargo package`
/// packs only files under the package directory, and shep-core and shep
/// are both published (`docs/releasing.md`), so a root-relative
/// `include_str!` would compile here and fail for everyone who runs
/// `cargo install shep`.
///
/// Read only by `the_committed_schema_is_current` below, so a plain `cargo
/// build`/`clippy` (no `#[cfg(test)]`) sees no reader and flags it dead.
/// `#[allow(dead_code)]` says so explicitly rather than moving the
/// `include_str!` into the test itself, which would trade away the one
/// property this constant exists for: living outside `#[cfg(test)]` is what
/// makes deleting the file fail every build, not just `cargo test`.
#[cfg(feature = "schema")]
pub const COMMITTED: &str = include_str!("../../assets/flockfile.schema.json");

/// How to regenerate the committed copy. Named in the drift test's own
/// failure message, so a red test is self-service.
///
/// Same `#[allow(dead_code)]` reasoning as [`COMMITTED`] just above: its one
/// reader is that same test.
#[cfg(feature = "schema")]
#[allow(dead_code)]
const REGENERATE: &str =
    "cargo run --bin shep -- schema > crates/shep-core/assets/flockfile.schema.json";

/// Renders the Flockfile JSON Schema: the document grammar, pretty-printed
/// with a trailing newline so the committed file is a well-formed text file.
///
/// Generated from `RawFlockfile` — the type serde actually deserializes a
/// Flockfile into — so the schema and the parser cannot drift: they are the
/// same declaration. `AppConfig` supplies the per-app half and lands in
/// `$defs`.
///
/// The schema describes the **deserializer**, not the normalizer.
/// `AppConfig::kill_signal` is `Option<String>` here and stays a plain string
/// in the schema, even though `config::normalize` accepts only four
/// spellings: the schema's job is to describe what serde will parse, and a
/// schema that described a validation step running elsewhere at another time
/// would be wrong the moment those two diverged, in a way no test could
/// catch.
#[cfg(feature = "schema")]
#[track_caller]
#[must_use]
pub fn flockfile_schema_string() -> String {
    let schema = flockfile_schema_json();
    let mut rendered =
        serde_json::to_string_pretty(&schema).expect("a schemars Schema always serializes");
    rendered.push('\n');
    rendered
}

/// Returns the Flockfile JSON Schema.
///
/// Generated from `RawFlockfile` — the type serde actually deserializes a
/// Flockfile into — so the schema and the parser cannot drift: they are the
/// same declaration. `AppConfig` supplies the per-app half and lands in
/// `$defs`.
///
/// The schema describes the **deserializer**, not the normalizer.
/// `AppConfig::kill_signal` is `Option<String>` here and stays a plain string
/// in the schema, even though `config::normalize` accepts only four
/// spellings: the schema's job is to describe what serde will parse, and a
/// schema that described a validation step running elsewhere at another time
/// would be wrong the moment those two diverged, in a way no test could
/// catch.
///
/// # Panics
///
/// Never in practice: schemars produces a `serde_json::Value` tree, which
/// `to_string_pretty` cannot fail on. `#[track_caller]` so a future change
/// that makes it fallible reports the caller (IR-24).
#[cfg(feature = "schema")]
#[track_caller]
#[must_use]
pub fn flockfile_schema_json() -> Schema {
    schemars::schema_for!(RawFlockfile)
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
        let raw = parse_into::<RawFlockfile>(source, format)?;
        let RawFlockfile {
            schema: _schema,
            // Discarded here, deliberately and by name. Whatever a dog wrote
            // under `[dog]` is that dog's to read out of the file itself; shep
            // only had to stop refusing the document for containing it.
            dog: _dog,
            apps,
        } = raw;
        if apps.is_empty() {
            return Err(FlockfileError::NoApps);
        }
        Ok(Self { apps })
    }

    /// Parses `text` and reports, per app, which keys the document wrote.
    ///
    /// Runs the exact same per-format parse and validation [`Flockfile::parse`]
    /// does — so a document that `parse` accepts or refuses is accepted or
    /// refused here for the same reason — then separately deserializes the
    /// same source into a [`serde_json::Value`] and reads each app table's
    /// keys off it. `AppConfig`'s `#[serde(default)]` erases which keys a
    /// document actually named, which is exactly the information the value
    /// pass recovers.
    ///
    /// # Errors
    ///
    /// Every error [`Flockfile::parse`] returns, for the same inputs.
    pub fn parse_declared(
        text: &str,
        format: FlockFormat,
    ) -> Result<Vec<DeclaredApp>, FlockfileError> {
        let raw = parse_into::<RawFlockfile>(text, format)?;
        let RawFlockfile {
            schema: _schema,
            dog: _dog,
            apps,
        } = raw;
        if apps.is_empty() {
            return Err(FlockfileError::NoApps);
        }

        // A document that reached this point already parsed successfully
        // into `RawFlockfile` above, so the same source deserializing into a
        // generic `Value` cannot fail for a reason the `RawFlockfile` pass
        // would not already have caught.
        let value = parse_into::<serde_json::Value>(text, format)?;
        let tables: Vec<Option<&serde_json::Map<String, serde_json::Value>>> = value
            .get("app")
            .and_then(serde_json::Value::as_array)
            .map(|apps| apps.iter().map(serde_json::Value::as_object).collect())
            .unwrap_or_default();

        Ok(apps
            .into_iter()
            .enumerate()
            .map(|(index, config)| {
                let table = tables.get(index).copied().flatten();
                let declared = table
                    .map(|t| t.keys().cloned().collect())
                    .unwrap_or_default();
                let declared_env = table
                    .and_then(|t| t.get("env"))
                    .and_then(serde_json::Value::as_object)
                    .map(|e| e.keys().cloned().collect())
                    .unwrap_or_default();
                DeclaredApp {
                    config,
                    declared,
                    declared_env,
                }
            })
            .collect())
    }
}

// The per-format deserialize `Flockfile::parse` and `Flockfile::parse_declared`
// both start from, generic over the target type so the same four backends
// serve both `RawFlockfile` (validation) and `serde_json::Value` (recovering
// the document's literal keys). Kept as the ONE place that knows the four
// backends, so the two callers cannot drift on what a valid document looks
// like.
fn parse_into<T: serde::de::DeserializeOwned>(
    source: &str,
    format: FlockFormat,
) -> Result<T, FlockfileError> {
    match format {
        FlockFormat::Toml => {
            toml::from_str(source).map_err(|e| FlockfileError::Toml(e.to_string()))
        }
        FlockFormat::Yaml => {
            serde_saphyr::from_str(source).map_err(|e| FlockfileError::Yaml(e.to_string()))
        }
        FlockFormat::Json => {
            serde_json::from_str(source).map_err(|e| FlockfileError::Json(e.to_string()))
        }
        FlockFormat::Json5 => {
            if json5_nesting_depth(source) > MAX_JSON5_NESTING_DEPTH {
                return Err(FlockfileError::Json5(
                    "nesting depth exceeds 64".to_string(),
                ));
            }
            json5::from_str(source).map_err(|e| FlockfileError::Json5(e.to_string()))
        }
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
///
/// `#[non_exhaustive]`: shep-core is a library crate, so an out-of-tree
/// consumer can match this exhaustively and a new variant would break them
/// with no version bump to say so (IR-20). Growth is anticipated per
/// backend, not per format: `.js` Flockfiles do NOT appear here, because
/// shep-core never executes anything — the node bridge lives in shep-cli
/// (`commands::lifecycle`) and feeds its output back through
/// [`FlockFormat::Json`], which is what this module's own doc promises.
#[non_exhaustive]
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

    /// fails if a `.js` name is ever added to the discovery order. The maintainer's
    /// ruling, 2026-08-15: a `.js` Flockfile is read only when named
    /// explicitly on the command line, because reading one runs node on it,
    /// and `cd` into a cloned repo followed by `shep start` must not execute
    /// a stranger's JavaScript. Discovery is the path with no operator in
    /// the loop, so it is the path that must never reach node.
    #[test]
    fn discovery_never_names_a_js_file_and_stays_ten_names() {
        assert_eq!(DISCOVERY_ORDER.len(), 10);
        for name in DISCOVERY_ORDER {
            assert!(
                !name.ends_with(".js"),
                "{name} would let `shep start` execute a repo's JavaScript"
            );
            assert!(FlockFormat::from_path(Path::new(name)).is_some());
        }
    }

    #[test]
    fn yaml_deep_nesting_is_rejected_without_crashing() {
        // Adversarial probe locked in as a regression test (json5 taught us
        // to distrust backends here): 5000-deep flow-style nesting must
        // return Err from serde-saphyr, never overflow the stack.
        let deep = "[".repeat(5000);
        let result = Flockfile::parse(&deep, FlockFormat::Yaml);
        assert!(matches!(result, Err(FlockfileError::Yaml(_))));
    }

    #[test]
    fn yaml_alias_bomb_is_bounded() {
        // Billion-laughs shape: each level aliases the previous twice. The
        // backend must reject or resolve it bounded — this test completing
        // quickly (and the doc failing schema-wise) is the assertion.
        let mut bomb = String::from("a: &a [\"x\",\"x\"]\n");
        for i in 1..9 {
            bomb.push_str(&format!(
                "{c}: &{c} [*{p},*{p}]\n",
                c = (b'a' + i) as char,
                p = (b'a' + i - 1) as char
            ));
        }
        let result = Flockfile::parse(&bomb, FlockFormat::Yaml);
        assert!(result.is_err(), "alias bomb must not produce a valid flock");
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

    /// Resolves a `$ref` into `$defs`, one hop, and returns the subschema.
    /// Everything with a `schema_name` is referenced rather than inlined, so
    /// an assertion that does not follow the ref is asserting about a
    /// `{"$ref": …}` object and passes or fails for the wrong reason.
    #[cfg(feature = "schema")]
    fn resolved<'a>(
        root: &'a serde_json::Value,
        node: &'a serde_json::Value,
    ) -> &'a serde_json::Value {
        match node.get("$ref").and_then(serde_json::Value::as_str) {
            Some(r) => {
                let name = r
                    .strip_prefix("#/$defs/")
                    .expect("every $ref in this schema points into $defs");
                &root["$defs"][name]
            }
            None => node,
        }
    }

    /// fails whenever the Flockfile grammar changes and the committed schema
    /// does not. That includes a doc-comment edit: schemars reads `///` into
    /// `description`, which is the point — those become hover text in the
    /// operator's editor — so a docs-only change is a real schema change and
    /// regenerating is the correct response, not a sign anything broke.
    #[cfg(feature = "schema")]
    #[test]
    fn the_committed_schema_is_current() {
        assert_eq!(
            flockfile_schema_string(),
            COMMITTED,
            "crates/shep-core/assets/flockfile.schema.json is stale. Regenerate it:\n    {REGENERATE}\n\
             A doc-comment edit on AppConfig counts; schemars puts doc comments \
             into `description`."
        );
    }

    /// fails if the artefact goes back to describing ONE APP. The document is
    /// `{"app": [ … ]}`; a schema whose own `required` names `name` and
    /// `script` is an AppConfig schema under a Flockfile filename, and every
    /// real Flockfile would fail against it.
    #[cfg(feature = "schema")]
    #[test]
    fn the_schema_describes_a_document_not_one_app() {
        let schema: serde_json::Value = serde_json::from_str(&flockfile_schema_string()).unwrap();
        assert!(schema["properties"]["app"].is_object(), "{schema}");
        assert_eq!(schema["properties"]["app"]["type"], "array", "{schema}");
        assert!(
            schema["properties"]["name"].is_null(),
            "root must not be an app: {schema}"
        );
        assert!(schema["$defs"]["AppConfig"].is_object(), "{schema}");
    }

    /// fails if the schema starts describing `normalize`'s grammar instead of
    /// serde's. The four signal names belong to a validation step elsewhere;
    /// a schema that listed them would be describing something it cannot see.
    #[cfg(feature = "schema")]
    #[test]
    fn kill_signal_stays_an_unconstrained_string() {
        let schema: serde_json::Value = serde_json::from_str(&flockfile_schema_string()).unwrap();
        let field = resolved(
            &schema,
            &schema["$defs"]["AppConfig"]["properties"]["kill_signal"],
        );
        let types = field["type"]
            .as_array()
            .unwrap_or_else(|| panic!("kill_signal must carry a type array: {field}"));
        assert!(
            types.iter().any(|t| t == "string"),
            "kill_signal must accept a string: {field}"
        );
        assert!(
            field.get("enum").is_none(),
            "kill_signal must not become an enum of the four signal names: {field}"
        );
        assert!(
            field.get("pattern").is_none(),
            "kill_signal must not become pattern-constrained: {field}"
        );
    }

    /// fails if MemSize or UpDuration reverts to a derive and starts
    /// describing its inner integer. Follows the `$ref` — the fields are
    /// references into `$defs`, not inline schemas.
    #[cfg(feature = "schema")]
    #[test]
    fn duration_and_memory_fields_are_string_shaped() {
        let schema: serde_json::Value = serde_json::from_str(&flockfile_schema_string()).unwrap();
        let app = &schema["$defs"]["AppConfig"]["properties"];

        // `min_uptime: UpDuration` (not `Option`) is a bare `$ref`.
        let min_uptime = resolved(&schema, &app["min_uptime"]);
        assert_eq!(min_uptime["type"], "string", "{min_uptime}");
        assert_eq!(min_uptime["pattern"], r"^\d+(ms|h|m|s)?$", "{min_uptime}");

        // `max_memory: Option<MemSize>` is a `$ref` under `anyOf` beside `"null"`.
        let any_of = app["max_memory"]["anyOf"]
            .as_array()
            .unwrap_or_else(|| panic!("max_memory must be anyOf: {}", app["max_memory"]));
        let ref_node = any_of
            .iter()
            .find(|v| v.get("$ref").is_some())
            .unwrap_or_else(|| panic!("max_memory's anyOf must carry a $ref: {any_of:?}"));
        let max_memory = resolved(&schema, ref_node);
        assert_eq!(max_memory["type"], "string", "{max_memory}");
        assert_eq!(max_memory["pattern"], r"^\d+(G|M|K)?$", "{max_memory}");
    }

    /// fails if a Flockfile carrying a dog's own configuration is refused.
    ///
    /// A dog with per-app configuration has nowhere else to put it: it belongs
    /// beside the app's declaration, in the repository the dog deploys. Before
    /// this, `deny_unknown_fields` refused the whole document, so an operator
    /// following shep-deploy's own README could not `shep start` their app at
    /// all. Measured 2026-08-28 against shep 0.1.8: "unknown field `build`,
    /// expected `$schema` or `app`".
    ///
    /// The contents are deliberately not validated. shep does not know what a
    /// dog's keys mean and refusing a document for not recognising another
    /// program's config is a coupling neither side wants.
    #[test]
    fn a_dog_table_is_accepted_and_ignored() {
        let src = r#"
[dog.deploy]
command = "npm run build"
artifacts = ["dist/app.js"]

[dog.some-other-dog]
anything = { nested = true, count = 3 }

[[app]]
name = "web"
script = "./srv"
"#;
        let flock =
            Flockfile::parse(src, FlockFormat::Toml).expect("a dog's table is not an error");
        assert_eq!(flock.apps.len(), 1);
        assert_eq!(flock.apps[0].name, "web");
    }

    /// fails if `dog` accepts something that is not a table.
    ///
    /// Not reading what a dog wrote is deliberate. Not caring whether it wrote
    /// a table at all is a different thing, and it would make this the one key
    /// in the document where a typo does not fail loudly, which is the rule
    /// the rest of the file is built on.
    #[test]
    fn a_dog_that_is_not_a_table_is_refused() {
        for value in ["5", "\"nope\"", "[1, 2]", "true"] {
            let src = format!("dog = {value}\n\n[[app]]\nname = \"web\"\nscript = \"./srv\"\n");
            assert!(
                Flockfile::parse(&src, FlockFormat::Toml).is_err(),
                "`dog = {value}` is not a table and must be refused"
            );
        }
    }

    /// fails if a typo anywhere else stops failing loudly.
    ///
    /// Exactly one more key is legal, which is the whole reason the table is
    /// nested under one name rather than allowing loose top-level keys.
    #[test]
    fn a_key_that_is_not_dog_still_fails() {
        let src = r#"
[build]
command = "npm run build"

[[app]]
name = "web"
script = "./srv"
"#;
        let err = Flockfile::parse(src, FlockFormat::Toml)
            .expect_err("an unknown top-level key must still be refused");
        assert!(
            format!("{err}").contains("build"),
            "the refusal must name the key: {err}"
        );
    }

    #[test]
    fn a_schema_key_is_accepted_and_ignored() {
        let src = r#"{ "$schema": "./flockfile.schema.json",
                       "app": [{ "name": "web", "script": "./srv" }] }"#;
        let flock = Flockfile::parse(src, FlockFormat::Json).unwrap();
        assert_eq!(flock.apps.len(), 1);
    }

    /// fails if the new field is implemented by relaxing
    /// `deny_unknown_fields` instead of naming one more key — which would
    /// silently accept every typo the document lock exists to catch.
    #[test]
    fn one_more_key_is_legal_and_no_others_are() {
        let src = r#"{ "schema": "x", "app": [{ "name": "w", "script": "./s" }] }"#;
        assert!(
            matches!(
                Flockfile::parse(src, FlockFormat::Json),
                Err(FlockfileError::Json(_))
            ),
            "bare `schema` (no $) must still be an unknown field"
        );
    }

    #[test]
    fn a_toml_flockfile_takes_the_key_too() {
        let src = "\"$schema\" = \"./flockfile.schema.json\"\n\
                   [[app]]\nname = \"web\"\nscript = \"./srv\"\n";
        assert_eq!(
            Flockfile::parse(src, FlockFormat::Toml).unwrap().apps.len(),
            1
        );
    }

    /// fails if the declared key set is inferred from values rather than read
    /// from the document. `autorestart = true` is also the DEFAULT, so a
    /// parser that reports "fields that differ from Default" would miss it,
    /// and a later file load would then overwrite an operator who had
    /// deliberately turned it off.
    #[test]
    fn declared_reports_keys_the_document_wrote_even_at_their_default() {
        let text = r#"
[[app]]
name = "web"
script = "./srv"
autorestart = true
"#;
        let apps = Flockfile::parse_declared(text, FlockFormat::Toml).unwrap();
        assert_eq!(apps.len(), 1);
        let declared = &apps[0].declared;
        assert!(declared.contains("autorestart"), "declared: {declared:?}");
        assert!(declared.contains("name"));
        assert!(declared.contains("script"));
        assert!(
            !declared.contains("max_memory"),
            "a key nobody wrote is not declared"
        );
        assert_eq!(declared.len(), 3);
    }

    /// fails if env keys are not reported separately. `env` is the only map
    /// of user-supplied keys in `AppConfig`, so the merge treats it one
    /// level deeper than every other field.
    #[test]
    fn declared_env_reports_the_keys_inside_the_env_table() {
        let text = r#"
[[app]]
name = "web"
script = "./srv"
env = { DB_HOST = "", NODE_ENV = "production" }
"#;
        let apps = Flockfile::parse_declared(text, FlockFormat::Toml).unwrap();
        assert_eq!(
            apps[0].declared_env.iter().collect::<Vec<_>>(),
            vec!["DB_HOST", "NODE_ENV"]
        );
        assert!(apps[0].declared.contains("env"));
    }

    /// fails if a format other than TOML loses the key set. All four go
    /// through one generic intermediate, so a regression here means the
    /// intermediate was bypassed for a format.
    #[test]
    fn declared_survives_every_parse_format() {
        let json = r#"{"app":[{"name":"web","script":"./srv","autorestart":true}]}"#;
        let apps = Flockfile::parse_declared(json, FlockFormat::Json).unwrap();
        assert!(apps[0].declared.contains("autorestart"));
    }
}
