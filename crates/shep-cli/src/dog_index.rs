//! The community dog index: fetching it, validating it, and treating every
//! string in it as hostile input.
//!
//! `shep dogs --available` prints this document to a terminal, so every
//! string that survives is passed through [`sanitise`] first. Response
//! headers are hostile too and are sanitised a layer below, in
//! [`crate::fetch`], which cannot import from here without a cycle.
//!
//! shep re-validates every entry itself: `SHEP_DOG_INDEX` can point
//! anywhere. A malformed entry is skipped and counted, never fatal. Only
//! [`IndexError::InsecureUrl`] names the URL; callers render these as
//! `reading the dog index from {url}: {err}`.
use core::fmt;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::fetch::{self, FetchError};
use crate::terminal_safe::sanitise;

/// Where the index lives when nothing overrides it. An exact file path, so
/// it answers 200 rather than redirecting: [`crate::fetch::get`] refuses
/// redirects.
pub const DEFAULT_INDEX_URL: &str = "https://shep-pm.com/dogs.json";

/// The environment variable that overrides [`DEFAULT_INDEX_URL`], for
/// self-hosting and for the integration tests. Trusted input: whoever can
/// set it can already run `shep`.
pub const INDEX_URL_ENV: &str = "SHEP_DOG_INDEX";

/// The six categories a dog can be filed under, in the docs site's order.
/// Mirrors `web/src/data/dogs.ts`'s `CATEGORIES`; an entry naming anything
/// else is skipped.
const CATEGORIES: [&str; 6] = ["logs", "metrics", "alerts", "health", "deploy", "other"];

/// The only `version` this build's [`parse_index`] accepts. Bump this and
/// the published `dogs.json` together when the wrapper's shape changes. An
/// entry's own shape may grow independently: a bad entry is skipped, not
/// refused.
const SUPPORTED_INDEX_VERSION: u64 = 1;

/// The response cap. A megabyte is roughly two thousand entries at the size
/// the live index's own entries run.
const SIZE_LIMIT: usize = 1 << 20;

/// End-to-end budget for the fetch, connect and TLS handshake included.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Hosts that may serve the index over plain `http://`. See
/// [`require_secure_url`].
const LOOPBACK_HOSTS: [&str; 4] = ["localhost", "127.0.0.1", "::1", "[::1]"];

/// One dog an operator could adopt, with every string already sanitised.
///
/// `Debug` needs no redaction: every field came out of a public JSON
/// document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AvailableDog {
    /// The dog's own name, displayed rather than typed.
    pub name: String,
    /// The crate or repository name: the dog's real identity.
    pub package: String,
    /// The name this dog expects to be adopted under. A dog is given no
    /// argv and cannot learn its own adopted name, so `shep adopt <path>
    /// --name <name>` with the wrong `<name>` silently discards its entire
    /// `[dog.<name>]` section. Build an adopt line from this field, never
    /// from [`Self::name`] or [`Self::package`].
    pub adopt_as: String,
    /// One line describing what the dog does.
    pub description: String,
    /// HTTPS URL of the dog's repository.
    pub repo: String,
    /// SPDX license string.
    pub license: String,
    /// One of [`CATEGORIES`].
    pub category: String,
    /// How the dog is built.
    pub source: DogSourceKind,
}

/// How a dog is installed, tagged by `kind` exactly as the index tags it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DogSourceKind {
    /// Installable with `cargo install <package>` from crates.io. The
    /// package name is [`AvailableDog::package`], not a field here.
    Cargo {
        /// The exact version to name, for a dog that needs one. cargo
        /// resolves an absent version as `*`, and `*` never matches a
        /// pre-release. Absent for a dog on a normal release.
        version: Option<String>,
    },
    /// Installable with `cargo install --git <url>`, for a dog that is not
    /// on crates.io.
    CargoGit {
        /// The repository to install from, always `https://`.
        url: String,
    },
    /// Installable with `go install <module>@latest`.
    GoInstall {
        /// The Go module path. Not a URL, and not checked as one.
        module: String,
    },
    /// No one-line installer; `instructions` is prose, never a command to
    /// run.
    Manual {
        /// What the entry says to do instead, sanitised like every other
        /// string here.
        instructions: String,
    },
}

/// A parsed index, with the two counts a caller prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    /// The entries that validated, in the order the document listed them.
    pub dogs: Vec<AvailableDog>,
    /// How many entries were dropped for failing validation.
    pub skipped: usize,
    /// How many surviving entries had something stripped. Once per entry,
    /// never once per field, and never for a skipped entry.
    pub sanitised: usize,
}

/// Why reading the index failed outright, as opposed to the per-entry
/// problems [`Index::skipped`] counts.
///
/// Not `#[non_exhaustive]`: every module in this crate is private, so no
/// out-of-tree consumer can match on it. `Debug` needs no redaction, a dog
/// index URL being a public location rather than a credential.
#[derive(Debug)]
pub enum IndexError {
    /// The index URL was not `https://` and its host was not a loopback
    /// literal. Carries the URL, which is public by construction.
    InsecureUrl(String),
    /// The request itself failed, was refused, or came back malformed.
    Fetch(FetchError),
    /// The bytes were not JSON at all. Carries the parser's complaint,
    /// never the offending bytes.
    Malformed(String),
    /// The bytes were JSON, but the top level was not an object. Distinct
    /// from [`Self::Malformed`]: a bare array is a wrong document, not a
    /// broken one, and an empty listing would be the wrong answer for it.
    NotAnObject,
    /// The document's `version` was missing, or named a version this build
    /// does not understand. `found` is the value exactly as the document
    /// had it, `None` when the key was absent.
    UnsupportedVersion {
        /// The `version` value the document carried, if any.
        found: Option<Value>,
    },
    /// The document named a `version` this build understands, but its
    /// `dogs` field was missing or was not itself an array.
    MissingDogsArray,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsecureUrl(url) => write!(
                f,
                "dog index url {url} is not https://; the index is read over TLS \
                 unless it is served from loopback"
            ),
            Self::Fetch(source) => write!(f, "{source}"),
            Self::Malformed(reason) => write!(f, "the dog index was not valid json: {reason}"),
            Self::NotAnObject => write!(
                f,
                "the dog index was not a json object -- a bare array is the shape shep 0.1.0 \
                 read, and is no longer accepted"
            ),
            Self::UnsupportedVersion { found } => {
                let found = found
                    .as_ref()
                    .map_or_else(|| "unspecified".to_string(), Value::to_string);
                write!(
                    f,
                    "the dog index is version {found}, which this shep does not understand \
                     (this build reads version {SUPPORTED_INDEX_VERSION}); upgrade shep to read it"
                )
            }
            Self::MissingDogsArray => write!(f, "the dog index has no \"dogs\" array"),
        }
    }
}

impl core::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Fetch(source) => Some(source),
            Self::InsecureUrl(_)
            | Self::Malformed(_)
            | Self::NotAnObject
            | Self::UnsupportedVersion { .. }
            | Self::MissingDogsArray => None,
        }
    }
}

impl From<FetchError> for IndexError {
    fn from(source: FetchError) -> Self {
        Self::Fetch(source)
    }
}

/// Where to read the index from: `SHEP_DOG_INDEX` when it is set,
/// [`DEFAULT_INDEX_URL`] when it is not.
///
/// Set but empty comes back as the empty string rather than falling back,
/// so a script whose variable failed to expand gets a refusal.
pub fn index_url() -> String {
    std::env::var(INDEX_URL_ENV).unwrap_or_else(|_err| DEFAULT_INDEX_URL.to_owned())
}

/// Fetches the index at `url` and parses it.
///
/// The `https://` policy lives here rather than in [`crate::fetch::get`],
/// which speaks either scheme so a test can bind a plain-HTTP port. The
/// refusal happens before a packet is sent.
///
/// # Errors
/// - [`IndexError::InsecureUrl`] if `url` is `http://` and not loopback.
/// - [`IndexError::Fetch`] if the request failed; see [`crate::fetch`].
/// - [`IndexError::Malformed`], [`IndexError::NotAnObject`],
///   [`IndexError::UnsupportedVersion`], [`IndexError::MissingDogsArray`]
///   as [`parse_index`].
pub async fn fetch_index(url: &str) -> Result<Index, IndexError> {
    let target = fetch::parse_url(url)?;
    require_secure_url(url, &target)?;
    let bytes = fetch::get(&target, SIZE_LIMIT, TIMEOUT).await?;
    parse_index(&bytes)
}

/// Refuses a plaintext index URL, unless it points at this machine.
///
/// HTTPS only, with a carve-out for a loopback literal: `SHEP_DOG_INDEX` is
/// an override for testing and self-hosting, and there is no wire between
/// two processes on one host. Exact equality against [`LOOPBACK_HOSTS`], so
/// `http://127.0.0.1.example.com/` cannot talk its way through a prefix or
/// suffix match. The sibling case, `http://evil.com@127.0.0.1/`, no longer
/// reaches here at all: [`fetch::parse_url`] runs first and refuses a
/// `user@` authority outright.
///
/// # Errors
/// - [`IndexError::InsecureUrl`] if `target` is `http://` and its host is
///   not one of [`LOOPBACK_HOSTS`].
fn require_secure_url(url: &str, target: &fetch::Target) -> Result<(), IndexError> {
    if target.https || LOOPBACK_HOSTS.contains(&target.host.as_str()) {
        Ok(())
    } else {
        Err(IndexError::InsecureUrl(url.to_owned()))
    }
}

/// Whether `version` spells [`SUPPORTED_INDEX_VERSION`], as either a JSON
/// integer or a JSON float.
///
/// JSON draws no line between `1` and `1.0`, and `Value::as_u64` alone
/// returns `None` for a number the parser represented as a float.
fn version_is_supported(version: &Value) -> bool {
    version.as_u64() == Some(SUPPORTED_INDEX_VERSION)
        || version.as_f64() == Some(SUPPORTED_INDEX_VERSION as f64)
}

/// Parses `bytes` as a community dog index, validating and sanitising every
/// entry.
///
/// Untyped JSON validated by hand: a `serde` derive would make one entry
/// with a wrong field type fail the whole document. A bad entry is counted
/// in [`Index::skipped`] and dropped, never an error.
///
/// # Errors
/// - [`IndexError::Malformed`] if `bytes` are not JSON, or not UTF-8.
/// - [`IndexError::NotAnObject`] if the top level is not an object.
/// - [`IndexError::UnsupportedVersion`] if `version` is missing or unknown.
/// - [`IndexError::MissingDogsArray`] if `dogs` is missing or not an array.
pub fn parse_index(bytes: &[u8]) -> Result<Index, IndexError> {
    let document: Value =
        serde_json::from_slice(bytes).map_err(|err| IndexError::Malformed(err.to_string()))?;
    let Value::Object(document) = document else {
        return Err(IndexError::NotAnObject);
    };
    if !document.get("version").is_some_and(version_is_supported) {
        return Err(IndexError::UnsupportedVersion {
            found: document.get("version").cloned(),
        });
    }
    let Some(entries) = document.get("dogs").and_then(Value::as_array) else {
        return Err(IndexError::MissingDogsArray);
    };

    let mut dogs = Vec::with_capacity(entries.len());
    let mut skipped = 0;
    let mut sanitised = 0;
    for entry in entries {
        // Per entry, not per field: three hostile strings are one row to
        // go and look at.
        let mut entry_sanitised = false;
        match validate_entry(entry, &mut entry_sanitised) {
            Some(dog) => {
                if entry_sanitised {
                    sanitised += 1;
                }
                dogs.push(dog);
            }
            // A skipped entry is not listed, so it is never also counted
            // as sanitised.
            None => skipped += 1,
        }
    }
    Ok(Index {
        dogs,
        skipped,
        sanitised,
    })
}

/// One entry, validated and sanitised, or `None` for the caller to count as
/// skipped.
///
/// Sanitises before validating: the cleaned string is the one that gets
/// printed, so it is the one that has to pass.
fn validate_entry(entry: &Value, sanitised: &mut bool) -> Option<AvailableDog> {
    let entry = entry.as_object()?;
    let name = field(entry, "name", sanitised)?;
    let package = field(entry, "package", sanitised)?;
    let adopt_as = field(entry, "adopt_as", sanitised)?;
    let description = field(entry, "description", sanitised)?;
    let repo = field(entry, "repo", sanitised)?;
    let license = field(entry, "license", sanitised)?;
    let category = field(entry, "category", sanitised)?;
    if !CATEGORIES.contains(&category.as_str()) {
        return None;
    }
    if !is_https(&repo) {
        return None;
    }
    let source = validate_source(entry.get("source")?, sanitised)?;
    Some(AvailableDog {
        name,
        package,
        adopt_as,
        description,
        repo,
        license,
        category,
        source,
    })
}

/// One entry's `source`, or `None` for an unknown `kind` or a missing
/// payload.
///
/// `kind` is matched raw and never sanitised: it is a tag rather than
/// prose, so a `kind` carrying an escape matches nothing and takes the
/// entry with it.
fn validate_source(source: &Value, sanitised: &mut bool) -> Option<DogSourceKind> {
    let source = source.as_object()?;
    match source.get("kind")?.as_str()? {
        "cargo" => {
            // Absent is a normal release; present-and-unusable takes the
            // entry with it.
            let version = match source.get("version") {
                None => None,
                Some(_) => Some(field(source, "version", sanitised)?),
            };
            Some(DogSourceKind::Cargo { version })
        }
        "cargo-git" => {
            let url = field(source, "url", sanitised)?;
            if !is_https(&url) {
                return None;
            }
            Some(DogSourceKind::CargoGit { url })
        }
        "go-install" => Some(DogSourceKind::GoInstall {
            module: field(source, "module", sanitised)?,
        }),
        "manual" => Some(DogSourceKind::Manual {
            instructions: field(source, "instructions", sanitised)?,
        }),
        _ => None,
    }
}

/// `object[name]` as a sanitised, non-empty string, or `None` when the field
/// is absent, is not a string, or is empty once cleaned.
///
/// A field that is nothing but control characters cleans to the empty
/// string and takes its entry with it. `sanitised` is OR-ed into, so one
/// call cannot clear what an earlier one recorded.
fn field(object: &Map<String, Value>, name: &str, sanitised: &mut bool) -> Option<String> {
    let raw = object.get(name)?.as_str()?;
    let (clean, changed) = sanitise(raw);
    *sanitised |= changed;
    if clean.is_empty() {
        return None;
    }
    Some(clean)
}

/// Whether `url` is one this module will print as a link an operator might
/// copy.
fn is_https(url: &str) -> bool {
    url.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// `web/`, three directories above this file, only when it actually
    /// exists.
    ///
    /// It is absent once this crate is extracted on its own, so the drift
    /// guards below skip rather than fail there.
    fn workspace_web_dir() -> Option<PathBuf> {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web"));
        dir.is_dir().then(|| dir.to_path_buf())
    }

    /// Reads `web/{relative}`, or `None` outside the workspace checkout
    /// (see [`workspace_web_dir`]).
    ///
    /// # Panics
    /// Inside the workspace, if `relative` cannot be read: that is real
    /// drift the guard exists to catch.
    #[track_caller]
    fn read_workspace_web_file(relative: &str) -> Option<String> {
        let dir = workspace_web_dir()?;
        Some(
            std::fs::read_to_string(dir.join(relative)).unwrap_or_else(|err| {
                panic!("web/{relative} exists in the workspace but could not be read: {err}")
            }),
        )
    }

    /// The live index's own single entry, verbatim from
    /// `web/public/dogs.json`.
    fn valid_entry() -> serde_json::Value {
        serde_json::json!({
            "name": "Spot",
            "package": "shep-log-rotate",
            "adopt_as": "log-rotate",
            "description": "Rotates grown log files and asks the shepherd to reopen them.",
            "repo": "https://github.com/shep-pm/shep-log-rotate",
            "license": "MIT OR Apache-2.0",
            "category": "logs",
            "source": {
                "kind": "cargo-git",
                "url": "https://github.com/shep-pm/shep-log-rotate"
            }
        })
    }

    /// Wraps `entries` in the real document shape: `{"$schema": ...,
    /// "version": 1, "dogs": [...]}`. Every fixture builds through this,
    /// never a bare `serde_json::Value::Array`, which
    /// [`the_old_bare_array_format_is_refused`] pins as refused.
    fn wrap_index(entries: Vec<Value>) -> String {
        serde_json::json!({
            "$schema": "https://shep-pm.com/dogs.schema.json",
            "version": SUPPORTED_INDEX_VERSION,
            "dogs": entries,
        })
        .to_string()
    }

    fn one_entry_with(field: &str, value: &str) -> String {
        let mut entry = valid_entry();
        entry[field] = serde_json::Value::String(value.to_string());
        wrap_index(vec![entry])
    }

    fn one_entry_with_description(description: &str) -> String {
        one_entry_with("description", description)
    }

    fn one_entry_with_category(category: &str) -> String {
        one_entry_with("category", category)
    }

    fn one_entry_with_repo(repo: &str) -> String {
        one_entry_with("repo", repo)
    }

    /// Serves `body` once as a 200 on an ephemeral loopback port, and
    /// returns the URL to read it from.
    async fn serve_index(body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.unwrap();
            // Drains the request so the client's write never stalls on a
            // full socket buffer.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        format!("http://127.0.0.1:{}/dogs.json", addr.port())
    }

    /// Three entries, the middle one missing `adopt_as`.
    const THREE_ENTRIES_MIDDLE_BROKEN: &[u8] = br#"{
      "$schema": "https://shep-pm.com/dogs.schema.json",
      "version": 1,
      "dogs": [
      {
        "name": "Spot",
        "package": "shep-log-rotate",
        "adopt_as": "log-rotate",
        "description": "Rotates grown log files.",
        "repo": "https://github.com/shep-pm/shep-log-rotate",
        "license": "MIT OR Apache-2.0",
        "category": "logs",
        "source": { "kind": "cargo-git", "url": "https://github.com/shep-pm/shep-log-rotate" }
      },
      {
        "name": "Nameless",
        "package": "shep-nameless",
        "description": "Has no adopt_as, so nobody could adopt it correctly.",
        "repo": "https://github.com/example/shep-nameless",
        "license": "MIT",
        "category": "other",
        "source": { "kind": "manual", "instructions": "Build it yourself." }
      },
      {
        "name": "Rex",
        "package": "shep-watchdog",
        "adopt_as": "watchdog",
        "description": "Barks when a sheep stops answering.",
        "repo": "https://github.com/example/shep-watchdog",
        "license": "Apache-2.0",
        "category": "health",
        "source": { "kind": "go-install", "module": "github.com/example/shep-watchdog" }
      }
    ]}"#;

    #[test]
    fn a_sanitised_entry_still_lists_and_is_counted() {
        let index = parse_index(one_entry_with_description("clean\u{1b}[2Jhere").as_bytes())
            .expect("parses");
        assert_eq!(
            index.dogs.len(),
            1,
            "a hostile description does not remove the dog"
        );
        assert_eq!(index.sanitised, 1);
        assert!(!index.dogs[0].description.contains('\u{1b}'));
    }

    #[test]
    fn a_malformed_entry_is_skipped_and_counted_while_its_neighbours_list() {
        let index = parse_index(THREE_ENTRIES_MIDDLE_BROKEN).expect("parses");
        assert_eq!(index.dogs.len(), 2);
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn an_unknown_category_is_skipped_rather_than_shown() {
        let index = parse_index(one_entry_with_category("logz").as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 0);
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn a_non_https_repo_is_skipped() {
        let index =
            parse_index(one_entry_with_repo("http://example.com/x").as_bytes()).expect("parses");
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn the_old_bare_array_format_is_refused() {
        let bare = serde_json::Value::Array(vec![valid_entry()]).to_string();
        assert!(
            matches!(parse_index(bare.as_bytes()), Err(IndexError::NotAnObject)),
            "a bare array must be refused now, not silently accepted"
        );
    }

    /// The two refusals ask for different operator actions.
    #[test]
    fn an_object_with_no_version_is_unsupported_not_malformed() {
        let err = parse_index(b"{}").expect_err("no version field");
        assert!(
            matches!(err, IndexError::UnsupportedVersion { found: None }),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("unspecified"),
            "the message must say no version was given: {err}"
        );
    }

    #[test]
    fn a_version_this_build_does_not_understand_is_refused_with_an_upgrade_message() {
        let document = serde_json::json!({ "version": 99, "dogs": [] }).to_string();
        let err = parse_index(document.as_bytes()).expect_err("unsupported version");
        let IndexError::UnsupportedVersion { found } = &err else {
            panic!("wrong variant: {err:?}");
        };
        assert_eq!(found.as_ref().and_then(serde_json::Value::as_u64), Some(99));
        let message = err.to_string();
        assert!(message.contains("99"), "{message}");
        assert!(message.contains("upgrade"), "{message}");
    }

    #[test]
    fn a_version_spelled_with_a_decimal_point_is_still_supported() {
        let as_decimal = SUPPORTED_INDEX_VERSION as f64;
        let document = serde_json::json!({ "version": as_decimal, "dogs": [] }).to_string();
        let index = parse_index(document.as_bytes())
            .expect("a decimal-point spelling is the same number as SUPPORTED_INDEX_VERSION");
        assert!(index.dogs.is_empty());
    }

    #[test]
    fn a_supported_version_with_no_dogs_field_is_refused() {
        let document = serde_json::json!({ "version": SUPPORTED_INDEX_VERSION }).to_string();
        assert!(matches!(
            parse_index(document.as_bytes()),
            Err(IndexError::MissingDogsArray)
        ));
    }

    #[test]
    fn an_empty_dogs_array_is_a_valid_empty_index() {
        let index = parse_index(wrap_index(vec![]).as_bytes()).expect("parses");
        assert!(index.dogs.is_empty());
        assert_eq!(index.skipped, 0);
    }

    // --- Extra hostile cases ---

    /// Stripping per character rather than per sequence is what makes this
    /// a non-event, and it is a property of the sanitiser rather than the
    /// renderer.
    #[test]
    fn an_escape_split_across_a_field_boundary_cannot_reassemble() {
        let mut entry = valid_entry();
        entry["name"] = serde_json::Value::String("Spot\u{1b}".to_string());
        entry["description"] = serde_json::Value::String("[2J and the screen is gone".to_string());
        let document = wrap_index(vec![entry]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 1);
        let dog = &index.dogs[0];
        let joined = format!("{}{}", dog.name, dog.description);
        assert!(!joined.contains('\u{1b}'), "reassembled in {joined:?}");
        assert_eq!(
            index.sanitised, 1,
            "counted once for the entry, not per field"
        );
    }

    /// Ten thousand escapes is well inside the 1 MiB fetch cap, so the
    /// guard here is the sanitiser rather than the size limit.
    #[test]
    fn a_long_run_of_escapes_is_stripped_without_losing_the_entry() {
        let hostile = format!("{}real text", "\u{1b}".repeat(10_000));
        let index = parse_index(one_entry_with_description(&hostile).as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 1);
        assert_eq!(index.dogs[0].description, "real text");
        assert_eq!(index.sanitised, 1);
    }

    #[test]
    fn a_field_that_is_nothing_but_control_characters_skips_the_entry() {
        let index = parse_index(one_entry_with_description("\u{1b}\u{7}\r\n\t").as_bytes())
            .expect("parses");
        assert_eq!(index.dogs.len(), 0);
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn a_skipped_entry_is_not_also_counted_as_sanitised() {
        let mut entry = valid_entry();
        entry["description"] = serde_json::Value::String("hostile\u{1b}[2J".to_string());
        entry["category"] = serde_json::Value::String("logz".to_string());
        let document = wrap_index(vec![entry]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(index.skipped, 1);
        assert_eq!(index.sanitised, 0);
    }

    /// The reason the parse goes through untyped JSON: a `Deserialize`
    /// derive would make `"name": 42` a whole-document error.
    #[test]
    fn a_field_of_the_wrong_json_type_skips_only_its_own_entry() {
        let mut broken = valid_entry();
        broken["name"] = serde_json::json!(42);
        let mut other = valid_entry();
        other["package"] = serde_json::Value::String("shep-watchdog".to_string());
        let document = wrap_index(vec![broken, other]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 1);
        assert_eq!(index.skipped, 1);
        assert_eq!(index.dogs[0].package, "shep-watchdog");
    }

    #[test]
    fn a_cargo_source_parses_and_carries_no_fields_of_its_own() {
        let mut entry = valid_entry();
        entry["source"] = serde_json::json!({ "kind": "cargo" });
        let document = wrap_index(vec![entry]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(index.skipped, 0);
        assert_eq!(index.dogs.len(), 1);
        assert_eq!(index.dogs[0].source, DogSourceKind::Cargo { version: None });
    }

    /// `repo` is a link; a `cargo-git` `url` is pasted into a shell, so it
    /// gets the same https check and not a weaker one.
    #[test]
    fn a_non_https_cargo_git_source_url_is_skipped() {
        let mut entry = valid_entry();
        entry["source"] = serde_json::json!({ "kind": "cargo-git", "url": "http://example.com/x" });
        let document = wrap_index(vec![entry]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 0);
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn an_unknown_source_kind_is_skipped() {
        let mut entry = valid_entry();
        entry["source"] =
            serde_json::json!({ "kind": "curl-bash", "url": "https://example.com/x" });
        let document = wrap_index(vec![entry]);

        assert_eq!(parse_index(document.as_bytes()).expect("parses").skipped, 1);
    }

    /// Every `source.kind` this file accepts, with a minimal source that
    /// parses as it. Tests only: `validate_source` matches string literals,
    /// since a match on a const is not a match.
    const SOURCE_KINDS: [(&str, &str); 4] = [
        ("cargo", r#"{"kind":"cargo"}"#),
        (
            "cargo-git",
            r#"{"kind":"cargo-git","url":"https://example.com/x"}"#,
        ),
        (
            "go-install",
            r#"{"kind":"go-install","module":"example.com/x"}"#,
        ),
        ("manual", r#"{"kind":"manual","instructions":"build it"}"#),
    ];

    #[test]
    fn a_cargo_source_keeps_the_version_it_names() {
        let mut entry = valid_entry();
        entry["source"] = serde_json::json!({ "kind": "cargo", "version": "0.1.0-alpha.1" });
        let document = wrap_index(vec![entry]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(
            index.dogs[0].source,
            DogSourceKind::Cargo {
                version: Some("0.1.0-alpha.1".to_string())
            }
        );
    }

    #[test]
    fn a_cargo_source_with_an_empty_version_is_skipped() {
        let mut entry = valid_entry();
        entry["source"] = serde_json::json!({ "kind": "cargo", "version": "" });
        let document = wrap_index(vec![entry]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 0);
        assert_eq!(index.skipped, 1);
    }

    /// The dangerous direction: a kind the docs site accepts and
    /// `validate_source` does not drops a published entry from every
    /// listing, counted only as an anonymous `1 entry skipped`.
    #[test]
    fn every_listed_source_kind_actually_parses() {
        for (kind, source) in SOURCE_KINDS {
            let mut entry = valid_entry();
            entry["source"] = serde_json::from_str(source).expect("fixture is JSON");
            let document = wrap_index(vec![entry]);

            let index = parse_index(document.as_bytes()).expect("parses");
            assert_eq!(
                index.dogs.len(),
                1,
                "source.kind {kind:?} is listed as supported but its entry was skipped"
            );
        }
    }

    /// Skips outside the workspace checkout, see
    /// [`read_workspace_web_file`].
    #[test]
    fn the_source_kinds_match_the_docs_site_list() {
        let Some(dogs_ts) = read_workspace_web_file("src/data/dogs.ts") else {
            return;
        };

        // Past the `=` before splitting on quotes: the declaration reads
        // `const SOURCE_KINDS: readonly DogSource["kind"][] = [...]`, and
        // that `"kind"` in the type sits before the array.
        let after = dogs_ts
            .split_once("const SOURCE_KINDS")
            .expect("web/src/data/dogs.ts declares SOURCE_KINDS")
            .1
            .split_once('=')
            .expect("the SOURCE_KINDS declaration has an initialiser")
            .1;
        let literal = after
            .split_once("];")
            .expect("the SOURCE_KINDS array is closed")
            .0;
        let site: Vec<&str> = literal.split('"').skip(1).step_by(2).collect();

        let ours: Vec<&str> = SOURCE_KINDS.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            site, ours,
            "web/src/data/dogs.ts and dog_index.rs disagree about the source kinds"
        );
    }

    /// Two independent six-string lists in two languages, and nothing but
    /// this test holds them equal. Only the runtime array is read: the
    /// `DogCategory` union above it is what the array is typed against, so
    /// TypeScript fails the site's own build if those two disagree.
    ///
    /// Skips outside the workspace checkout, see
    /// [`read_workspace_web_file`].
    #[test]
    fn the_categories_match_the_docs_site_list() {
        let Some(dogs_ts) = read_workspace_web_file("src/data/dogs.ts") else {
            return;
        };

        let after = dogs_ts
            .split_once("export const CATEGORIES")
            .expect("web/src/data/dogs.ts declares CATEGORIES")
            .1;
        let literal = after
            .split_once("];")
            .expect("the CATEGORIES array is closed")
            .0;
        let site: Vec<&str> = literal.split('"').skip(1).step_by(2).collect();

        assert_eq!(
            site,
            CATEGORIES.to_vec(),
            "web/src/data/dogs.ts and dog_index.rs disagree about the categories"
        );
    }

    /// The checked-in editor schema (`web/public/dogs.schema.json`)
    /// against both lists this file enforces.
    ///
    /// Skips outside the workspace checkout, see
    /// [`read_workspace_web_file`].
    #[test]
    fn the_schema_agrees_with_the_categories_and_source_kinds() {
        let Some(schema) = read_workspace_web_file("public/dogs.schema.json") else {
            return;
        };
        let schema: Value = serde_json::from_str(&schema).expect("dogs.schema.json is valid json");
        let entry_schema = &schema["properties"]["dogs"]["items"];

        let schema_categories: Vec<&str> = entry_schema["properties"]["category"]["enum"]
            .as_array()
            .expect("category.enum is an array")
            .iter()
            .map(|v| v.as_str().expect("each category is a string"))
            .collect();
        assert_eq!(
            schema_categories,
            CATEGORIES.to_vec(),
            "dogs.schema.json and dog_index.rs disagree about the categories"
        );

        let schema_kinds: Vec<&str> = entry_schema["properties"]["source"]["oneOf"]
            .as_array()
            .expect("source.oneOf is an array")
            .iter()
            .map(|variant| {
                variant["properties"]["kind"]["const"]
                    .as_str()
                    .expect("each source variant names a const kind")
            })
            .collect();
        let ours: Vec<&str> = SOURCE_KINDS.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            schema_kinds, ours,
            "dogs.schema.json and dog_index.rs disagree about the source kinds"
        );
    }

    /// It is the one URL an operator never types, so nothing else would
    /// notice.
    #[test]
    fn the_default_index_url_is_https() {
        assert!(
            DEFAULT_INDEX_URL.starts_with("https://"),
            "{DEFAULT_INDEX_URL}"
        );
    }

    /// The refusal lands before any connection is attempted, so this can
    /// name a host it never reaches.
    #[tokio::test]
    async fn a_plain_http_index_url_is_refused_before_it_connects() {
        let err = fetch_index("http://example.com/dogs.json")
            .await
            .expect_err("refused");
        let IndexError::InsecureUrl(url) = err else {
            panic!("wrong variant: {err:?}")
        };
        assert_eq!(url, "http://example.com/dogs.json");
    }

    #[tokio::test]
    async fn a_host_that_merely_contains_a_loopback_literal_is_still_refused() {
        for url in [
            "http://127.0.0.1.example.com/dogs.json",
            "http://localhost.example.com/dogs.json",
        ] {
            assert!(
                matches!(fetch_index(url).await, Err(IndexError::InsecureUrl(_))),
                "{url} was not refused"
            );
        }
    }

    /// The third URL that used to sit in the loop above. It is still
    /// refused, but a step earlier and by a different rule:
    /// [`fetch::parse_url`] refuses a `user@` authority before
    /// [`require_secure_url`] ever sees a host to compare.
    #[tokio::test]
    async fn a_loopback_literal_behind_a_userinfo_prefix_is_refused_at_parse_time() {
        let err = fetch_index("http://evil.com@127.0.0.1/dogs.json")
            .await
            .expect_err("refused");
        assert!(
            matches!(err, IndexError::Fetch(FetchError::Url(_))),
            "wrong variant: {err:?}"
        );
    }

    /// The await's forcing mechanism is `fetch_index`'s own ten second
    /// budget: a server that never answers fails this test rather than
    /// hanging it.
    #[tokio::test]
    async fn a_loopback_http_index_is_read_because_that_is_how_a_local_one_is_served() {
        let url = serve_index(one_entry_with_description("clean\u{1b}[2Jhere")).await;
        let index = fetch_index(&url).await.expect("read");
        assert_eq!(index.dogs.len(), 1);
        assert_eq!(index.sanitised, 1);
        assert!(!index.dogs[0].description.contains('\u{1b}'));
    }
}
