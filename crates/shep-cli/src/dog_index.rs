//! The community dog index: fetching it, validating it, and treating every
//! string in it as hostile input.
//!
//! `shep dogs --available` reads a JSON document the docs site publishes at
//! [`DEFAULT_INDEX_URL`], listing the dogs an operator could adopt. That
//! document is built from pull requests by strangers, and **every string in
//! it is printed to a terminal**. A `description` carrying `\u{1b}[2J`
//! clears the operator's screen; `\u{1b}]0;` rewrites their window title;
//! and since shep emits colour of its own now, a well-placed escape can
//! imitate shep's own output with the reader having no way to tell an
//! entry's bytes from shep's. Without a guard, "somebody added a row to a
//! table" becomes "somebody can drive your terminal".
//!
//! So this module is the security boundary for everything the index says,
//! and it holds the whole of that boundary bar one function.
//!
//! ## What it does about that
//!
//! **Every string that survives is sanitised first** ([`sanitise`], which
//! lives in [`crate::terminal_safe`] and states the rule in full). Control
//! characters, and the invisible or reordering format characters that are
//! not control characters, are stripped; non-ASCII prose survives
//! untouched.
//!
//! The sanitiser sits in its own module rather than in this file because
//! the *body* of a response is not the only hostile string in a fetch. Its
//! **headers** are too, and those are read a layer below this one, in
//! [`crate::fetch`] — which cannot import from here without a module
//! cycle. That asymmetry was a real hole for the length of this branch: a
//! hostile `Location:` on a 3xx reached the terminal raw while every string
//! beside it in the body was cleaned. [`crate::terminal_safe`]'s own doc
//! has the history.
//!
//! **An entry that needed stripping still lists, and is counted**
//! ([`Index::sanitised`]). It is reported rather than quietly repaired
//! because silently fixing hostile input teaches nobody that it happened,
//! and a maintainer reading that count has a reason to go and look at the
//! pull request that added the row.
//!
//! **shep re-validates rather than trusting the site's own build**
//! ([`parse_index`]). The docs site validates this JSON at build time, but
//! that is a different program on a different machine, and it may be older,
//! newer, or bypassed entirely by whoever is serving `SHEP_DOG_INDEX`. So:
//! required fields present, `category` one of the six known, `repo` and any
//! `source` URL `https://`.
//!
//! **A malformed entry is skipped and counted, never fatal**
//! ([`Index::skipped`]). One bad row must not blank the listing, and the
//! skip must not be silent either.
//!
//! ## The wrapper, and why the document is not just the array
//!
//! The document is a JSON object, `{"$schema": ..., "version": 1, "dogs":
//! [...]}`, not a bare top-level array of entries the way `shep` 0.1.0 read
//! it. `$schema` is what gives a contributor's editor completion when they
//! add a row; a bare array can never carry that, or anything else beside
//! itself. `version` is what lets a future, incompatible reshape of this
//! wrapper announce itself instead of quietly parsing wrong: a `version`
//! this build does not recognise is [`IndexError::UnsupportedVersion`],
//! refused with a message that says to upgrade `shep`, not a parse error.
//! **This is a breaking change against shep 0.1.0**, which reads the old
//! bare-array shape and nothing else; there is no dual-format fallback,
//! because the docs site publishes one index for every shep that asks, and
//! 0.1.0 is hours old.
//!
//! Only the wrapper is new. Every entry inside `dogs` still validates
//! exactly as it always did -- the rest of this doc, unchanged below.
//!
//! ## Errors and the URL
//!
//! None of [`IndexError`]'s variants but [`IndexError::InsecureUrl`] names
//! the document's location, because the caller always knows it and would
//! otherwise print it twice. A caller renders these as
//! `reading the dog index from {url}: {err}`.

use core::fmt;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::fetch::{self, FetchError};
use crate::terminal_safe::sanitise;

/// Where the index lives when nothing overrides it: the docs site serves
/// this file verbatim, and `/dogs.json` is an exact file path that answers
/// 200 rather than redirecting, which is what lets [`crate::fetch::get`]
/// refuse redirects outright.
pub const DEFAULT_INDEX_URL: &str = "https://shep-pm.com/dogs.json";

/// The environment variable that overrides [`DEFAULT_INDEX_URL`], for
/// self-hosting an index and for pointing the integration tests at a local
/// server. An environment variable is trusted input under this project's
/// own threat model: whoever can set it can already run `shep`.
pub const INDEX_URL_ENV: &str = "SHEP_DOG_INDEX";

/// The six categories a dog can be filed under, in the order the docs site
/// groups them. Mirrors `web/src/data/dogs.ts`'s `CATEGORIES`; an entry
/// naming anything else is skipped rather than shown, because a category
/// shep does not know is a category shep cannot file or explain.
const CATEGORIES: [&str; 6] = ["logs", "metrics", "alerts", "health", "deploy", "other"];

/// The only `version` this build's [`parse_index`] accepts. Bump this, and
/// the docs site's published `dogs.json`, together, the day the wrapper's
/// own shape needs to change in a way an old `shep` could not read safely
/// -- an entry's own shape is free to grow independently, since a bad
/// entry is skipped and counted rather than refused.
const SUPPORTED_INDEX_VERSION: u64 = 1;

/// The response cap. A megabyte is roughly two thousand entries at the size
/// the live index's own entries run, so this bounds a hostile or broken
/// server without bounding any plausible index.
const SIZE_LIMIT: usize = 1 << 20;

/// End-to-end budget for the fetch, connect and TLS handshake included. A
/// discovery command an operator runs occasionally can afford to wait this
/// long; it cannot afford to hang.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Hosts that may serve the index over plain `http://` — see
/// [`require_secure_url`] for why these and nothing else.
const LOOPBACK_HOSTS: [&str; 4] = ["localhost", "127.0.0.1", "::1", "[::1]"];

/// One dog an operator could adopt, with every string already sanitised.
///
/// `Debug` is derived and needs no redaction: every field came out of a
/// public JSON document, and none of it is a credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AvailableDog {
    /// The dog's own name, displayed rather than typed.
    pub name: String,
    /// The crate or repository name: the dog's real identity.
    pub package: String,
    /// The name this dog expects to be adopted under, and the whole reason
    /// the detail view exists. A dog is given no argv and cannot learn its
    /// own adopted name, so `shep adopt <path> --name <name>` with the
    /// wrong `<name>` silently discards its entire `[dog.<name>]` section. An
    /// adopt line must be built from this field, never from
    /// [`Self::name`] or [`Self::package`].
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
///
/// Deliberately not a freeform string: "how do I install this" and "what
/// artifact would shep fetch" are two questions that look like one field,
/// and a tagged kind stays machine-readable if `shep install` ever exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DogSourceKind {
    /// Installable with `cargo install <package>` from crates.io. The
    /// package name is [`AvailableDog::package`] rather than a field here:
    /// it is the same fact, and an entry that could spell it two ways would
    /// eventually spell it two ways.
    Cargo {
        /// The exact version to name, for a dog that needs one. cargo
        /// resolves `*` by default and `*` never matches a pre-release, so
        /// an alpha that leaves this out ships a command that cannot find
        /// it: `could not find <crate> in registry `crates-io` with version
        /// `*``. Absent for a dog on a normal release, where a bare install
        /// is the right command.
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

/// A parsed index, and an honest account of what it cost to parse.
///
/// The two counts are printed rather than swallowed. A reader who sees
/// either has a reason to go and look at the index itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    /// The entries that validated, in the order the document listed them.
    pub dogs: Vec<AvailableDog>,
    /// How many entries were dropped for failing validation.
    pub skipped: usize,
    /// How many of the *surviving* entries had something stripped out of
    /// them. Counted once per entry, not once per field, and never for an
    /// entry that was skipped anyway — an entry cannot be both.
    pub sanitised: usize,
}

/// Why reading the index failed outright, as opposed to the per-entry
/// problems [`Index::skipped`] counts.
///
/// Not `#[non_exhaustive]`, per `docs/idiomatic-rust.md` IR-20: every module
/// in this crate is a private `mod`, so no out-of-tree consumer can match on
/// this enum at all and the attribute would guard a match nobody can write.
///
/// `Debug` needs no redaction: a dog index URL is a public document
/// location, never a bearer credential the way a webhook URL is.
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
    /// The bytes were JSON, but the top-level document was not an object.
    /// Distinguished from [`Self::Malformed`] because a document that
    /// parses as, say, a bare array -- `shep` 0.1.0's own index shape --
    /// is a wrong document rather than a broken one, and an empty listing
    /// would be the wrong answer to give for it.
    NotAnObject,
    /// The document's `version` field was missing, or named a version this
    /// build does not understand. Carries the value found, exactly as the
    /// document had it, so the message can say what arrived and what this
    /// build wanted instead; `None` when the key was absent entirely.
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

/// Where to read the index from: `SHEP_DOG_INDEX` when it is set, and
/// [`DEFAULT_INDEX_URL`] when it is not.
///
/// A variable that is set but empty is returned as the empty string rather
/// than quietly falling back, so a script whose variable failed to expand
/// gets a refusal instead of a silent trip to the real network.
pub fn index_url() -> String {
    std::env::var(INDEX_URL_ENV).unwrap_or_else(|_err| DEFAULT_INDEX_URL.to_owned())
}

/// Fetches the index at `url` and parses it.
///
/// The `https://` policy lives here rather than in [`crate::fetch::get`],
/// the same transport/policy split `dog::bark::sinks` already draws with its
/// own `require_secure_scheme`: the transport speaks either scheme so a test
/// can bind an ephemeral plain-HTTP port, and the caller that cares enforces
/// TLS on top. The refusal happens before a single packet is sent.
///
/// # Errors
/// - [`IndexError::InsecureUrl`] — `url` is `http://` to somewhere that is
///   not loopback.
/// - [`IndexError::Fetch`] — `url` did not parse, or the request failed,
///   was redirected, answered non-2xx, exceeded [`SIZE_LIMIT`], or ran past
///   [`TIMEOUT`]. See [`crate::fetch`]'s own module doc for the full
///   refusal order.
/// - [`IndexError::Malformed`], [`IndexError::NotAnObject`],
///   [`IndexError::UnsupportedVersion`], [`IndexError::MissingDogsArray`] —
///   as [`parse_index`].
pub async fn fetch_index(url: &str) -> Result<Index, IndexError> {
    let target = fetch::parse_url(url)?;
    require_secure_url(url, &target)?;
    let bytes = fetch::get(&target, SIZE_LIMIT, TIMEOUT).await?;
    parse_index(&bytes)
}

/// Refuses a plaintext index URL, unless it points at this machine.
///
/// The blanket rule is HTTPS only: no plaintext, no downgrade. The carve-out
/// is for a loopback literal, and it costs nothing — there is no wire
/// between two processes on one host for anybody to listen to, and an
/// attacker who is already on the box has better options than reading a
/// public JSON document in flight. It exists because `SHEP_DOG_INDEX` is
/// documented as an override "for testing and self-hosting", and without it
/// neither the integration tests nor an operator serving their own index
/// from a sidecar could ever point at a local port.
///
/// The check is exact equality against [`LOOPBACK_HOSTS`], deliberately, so
/// no `http://127.0.0.1.example.com/` or `http://evil.com@127.0.0.1/` can
/// talk its way through a prefix or suffix match.
///
/// # Errors
/// - [`IndexError::InsecureUrl`] — `target` is `http://` and its host is
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
/// JSON itself draws no line between `1` and `1.0` -- both are the number
/// one -- so `serde_json::Value::as_u64` alone is too narrow a check: it
/// returns `None` for a number the parser represented as a float, which
/// `1.0` (written with a decimal point, however the index was generated)
/// always is. Refusing that spelling would tell an operator to "upgrade
/// shep to read it" ([`IndexError::UnsupportedVersion`]'s own message)
/// for an index this build already understands perfectly.
fn version_is_supported(version: &Value) -> bool {
    version.as_u64() == Some(SUPPORTED_INDEX_VERSION)
        || version.as_f64() == Some(SUPPORTED_INDEX_VERSION as f64)
}

/// Parses `bytes` as a community dog index, validating and sanitising every
/// entry.
///
/// Deserialised as untyped JSON and validated by hand rather than straight
/// into [`AvailableDog`]: a `serde` derive would make one entry with a
/// wrong field *type* fail the whole document, and the one thing this must
/// never do is let a single bad row blank the listing.
///
/// # Errors
/// - [`IndexError::Malformed`] — `bytes` are not JSON, or not UTF-8.
/// - [`IndexError::NotAnObject`] — `bytes` are JSON, but the top level is
///   not an object (a bare array, `shep` 0.1.0's own shape, included).
/// - [`IndexError::UnsupportedVersion`] — `version` is missing, or is not
///   [`SUPPORTED_INDEX_VERSION`].
/// - [`IndexError::MissingDogsArray`] — `version` is understood, but
///   `dogs` is missing or is not an array.
///
/// Nothing an individual entry can do produces an error. A bad entry is
/// counted in [`Index::skipped`] and dropped.
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
        // Per entry, not per field: an entry with three hostile strings is
        // one row to go and look at, not three.
        let mut entry_sanitised = false;
        match validate_entry(entry, &mut entry_sanitised) {
            Some(dog) => {
                if entry_sanitised {
                    sanitised += 1;
                }
                dogs.push(dog);
            }
            // A skipped entry is never also counted as sanitised: it is not
            // listed, so there is nothing sanitised about it to report.
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
/// Sanitising happens *before* validating, and that order is deliberate: the
/// cleaned string is the one that gets printed, so it is the one that has to
/// pass. Validating the raw string and printing the cleaned one would be
/// checking something other than what ships.
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
/// `kind` itself is matched raw and never sanitised. It is a tag rather than
/// prose, so a `kind` carrying an escape simply matches nothing and takes
/// the entry with it — which is the right answer, and narrower than
/// cleaning it up and then matching.
fn validate_source(source: &Value, sanitised: &mut bool) -> Option<DogSourceKind> {
    let source = source.as_object()?;
    match source.get("kind")?.as_str()? {
        "cargo" => {
            // Optional, but not lax: absent is a normal release, while
            // present-and-unusable takes the entry with it, the same as
            // every other malformed field in this file.
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
/// A field that is nothing but control characters cleans to the empty string
/// and takes its entry with it. That is the right answer: a dog with no
/// printable name is not a listing, it is a blank row.
///
/// `sanitised` is OR-ed into rather than assigned, so one call cannot clear
/// what an earlier one recorded.
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
    /// It exists inside the shep workspace checkout and nowhere else:
    /// `crates/shep-cli` has no package `include` rule (deliberately -- see
    /// [`read_workspace_web_file`]), so once this crate is extracted on its
    /// own -- crates.io, a vendored copy, `cargo package`'s own
    /// verification -- `web/` is simply absent. The three drift guards
    /// below need to tell those two situations apart before they can read
    /// anything, which is a question only a runtime check can answer:
    /// `include_str!` resolves at compile time regardless of which branch
    /// a test reaches, so it could not tell them apart, and every one of
    /// those downstream `cargo test` runs failed to compile before this
    /// existed.
    fn workspace_web_dir() -> Option<PathBuf> {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web"));
        dir.is_dir().then(|| dir.to_path_buf())
    }

    /// Reads `web/{relative}`, or `None` outside the workspace checkout
    /// (see [`workspace_web_dir`]).
    ///
    /// `web/` present but `relative` missing is not that case -- it is real
    /// drift inside the checkout the guard exists to catch -- and panics
    /// exactly as loudly as the `include_str!` this replaces would have.
    ///
    /// # Panics
    /// Inside the workspace, if `relative` cannot be read.
    fn read_workspace_web_file(relative: &str) -> Option<String> {
        let dir = workspace_web_dir()?;
        Some(
            std::fs::read_to_string(dir.join(relative)).unwrap_or_else(|err| {
                panic!("web/{relative} exists in the workspace but could not be read: {err}")
            }),
        )
    }

    /// The live index's own single entry, verbatim from
    /// `web/public/dogs.json` — a shape a real contributor's pull request
    /// produces, rather than one invented here.
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

    /// A one-entry index built from [`valid_entry`] with `field` replaced by
    /// `value`. Built through `serde_json` rather than string formatting so
    /// a hostile `value` is JSON-escaped the way a real index serving one
    /// would have to escape it — serde_json refuses an unescaped control
    /// character inside a string, so a hand-formatted fixture would fail to
    /// parse for the wrong reason.
    /// Wraps `entries` in the real document shape: `{"$schema": ...,
    /// "version": 1, "dogs": [...]}`. Every fixture below a real index
    /// document builds through this, never a bare
    /// `serde_json::Value::Array` -- that shape is what
    /// [`the_old_bare_array_format_is_refused`] proves `parse_index`
    /// refuses now.
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
    /// returns the URL to read it from. Same shape as `fetch`'s own test
    /// harness, aimed at a whole index instead of a canned response.
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

    /// Three entries, the middle one missing `adopt_as` — the field whose
    /// absence is silent everywhere else, since a dog adopted under the
    /// wrong name loses its whole config section without saying so.
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
        // A missing `adopt_as`, beside two good entries.
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

    /// fails if `shep` 0.1.0's own index shape -- a bare top-level array,
    /// no wrapper at all -- is still silently accepted. This is the
    /// breaking change the wrapper introduces, proven directly rather than
    /// assumed: a published index in the new shape must not also be
    /// readable as the old one, or a version-skew bug here would go
    /// unnoticed until an actual 0.1.0 install hit it.
    #[test]
    fn the_old_bare_array_format_is_refused() {
        let bare = serde_json::Value::Array(vec![valid_entry()]).to_string();
        assert!(
            matches!(parse_index(bare.as_bytes()), Err(IndexError::NotAnObject)),
            "a bare array must be refused now, not silently accepted"
        );
    }

    /// fails if a well-formed object with no `version` field at all reads
    /// as malformed rather than as an unsupported (in this case: entirely
    /// unspecified) version -- the two are different refusals with
    /// different operator actions, and conflating them would send someone
    /// hunting for a JSON syntax error that is not there.
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

    /// fails if a `version` this build does not recognise reaches the
    /// operator as a parse error instead of a named refusal that says to
    /// upgrade -- the entire reason the field exists rather than being
    /// decoration. `99` is never going to be a real version this file
    /// assigns.
    ///
    /// Mutation check: hardcoding `parse_index`'s version check to `true`
    /// (accepting anything) reddens this -- `dogs: []` would otherwise
    /// parse to an empty, successful index instead of refusing.
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

    /// fails if `"version": 1.0` (a decimal-point spelling of the same
    /// number `SUPPORTED_INDEX_VERSION` names) is refused as unsupported.
    /// JSON does not distinguish `1` from `1.0` -- both are the number
    /// one -- so a hand-written or differently-serialised index spelling
    /// it this way is not a version this build fails to understand; it is
    /// the same version. Refusing it would send an operator to "upgrade
    /// shep" (the exact message [`IndexError::UnsupportedVersion`]
    /// carries) for a document this build already reads correctly.
    ///
    /// Mutation check: reverting `version_is_supported` to a bare
    /// `Value::as_u64` comparison reddens this.
    #[test]
    fn a_version_spelled_with_a_decimal_point_is_still_supported() {
        let as_decimal = SUPPORTED_INDEX_VERSION as f64;
        let document = serde_json::json!({ "version": as_decimal, "dogs": [] }).to_string();
        let index = parse_index(document.as_bytes())
            .expect("a decimal-point spelling is the same number as SUPPORTED_INDEX_VERSION");
        assert!(index.dogs.is_empty());
    }

    /// fails if a document naming a supported version but no `dogs` field
    /// at all is read as an empty index instead of refused -- an operator
    /// whose self-hosted index dropped the field entirely by accident
    /// needs to hear about it, not see a silently empty listing.
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

    // ---------------------------------------------------------------
    // Extra hostile cases, beyond the nine above. Each names the thing a
    // stranger's pull request could do that the nine do not cover.
    // ---------------------------------------------------------------

    /// fails if an escape can be smuggled in halves. Splitting `\u{1b}[2J`
    /// across two fields is the obvious way around a guard that looks for
    /// whole sequences: neither half is a sequence, and the renderer prints
    /// them next to each other. Stripping per character rather than per
    /// sequence is what makes this a non-event, and this test is what pins
    /// that property to the sanitiser rather than to the renderer.
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

    /// fails if a long run of escapes costs anything but its own removal.
    /// Ten thousand of them is well inside the 1 MiB fetch cap, so the
    /// guard has to be the sanitiser rather than the size limit.
    #[test]
    fn a_long_run_of_escapes_is_stripped_without_losing_the_entry() {
        let hostile = format!("{}real text", "\u{1b}".repeat(10_000));
        let index = parse_index(one_entry_with_description(&hostile).as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 1);
        assert_eq!(index.dogs[0].description, "real text");
        assert_eq!(index.sanitised, 1);
    }

    /// fails if an entry whose every character is hostile becomes a blank
    /// row instead of a skipped one. A name that sanitises to nothing is
    /// not a listing.
    #[test]
    fn a_field_that_is_nothing_but_control_characters_skips_the_entry() {
        let index = parse_index(one_entry_with_description("\u{1b}\u{7}\r\n\t").as_bytes())
            .expect("parses");
        assert_eq!(index.dogs.len(), 0);
        assert_eq!(index.skipped, 1);
    }

    /// fails if an entry can be both skipped and counted as sanitised. It
    /// is not listed, so there is nothing sanitised about it to report, and
    /// double-counting would make the two footer numbers add up to more
    /// entries than the document had.
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

    /// fails if one entry with a wrong field TYPE takes the document down
    /// with it. This is why the parse goes through untyped JSON: a
    /// `#[derive(Deserialize)]` into the real struct would make `"name": 42`
    /// a whole-document error, and blanking the listing is exactly the
    /// outcome the skip-and-count rule exists to prevent.
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

    /// fails if a crates.io dog stops parsing, which is the common case:
    /// most dogs are published, and `cargo` carries no fields precisely
    /// because the package name is already `package`.
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

    /// fails if a plaintext install URL is printed as a command to run.
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

    /// fails if a `source.kind` shep does not understand is shown anyway.
    /// The tag is matched raw, never sanitised, so a kind carrying an
    /// escape matches nothing and takes its entry with it.
    #[test]
    fn an_unknown_source_kind_is_skipped() {
        let mut entry = valid_entry();
        entry["source"] =
            serde_json::json!({ "kind": "curl-bash", "url": "https://example.com/x" });
        let document = wrap_index(vec![entry]);

        assert_eq!(parse_index(document.as_bytes()).expect("parses").skipped, 1);
    }

    /// Every `source.kind` this file accepts, and a minimal source that
    /// should parse as it. Only the tests read this: `validate_source`
    /// matches string literals directly, since a match on a const is not a
    /// match. The two tests below hold this list equal to the docs site's
    /// AND equal to the validator, which are different failures.
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

    /// fails if a pre-release dog's version is dropped on the way through.
    ///
    /// Measured, not assumed: `cargo install shep-log-rotate` with no
    /// version answers `could not find shep-log-rotate in registry
    /// `crates-io` with version `*``, because `*` never matches a
    /// pre-release. Every dog published before 1.0 needs the version
    /// carried, so losing it here ships a command that cannot work.
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

    /// fails if a `version` that is present but unusable is quietly
    /// ignored, leaving a bare install command beside a half-parsed entry.
    /// Absent is fine; present-and-broken takes the entry, like every other
    /// malformed field here.
    #[test]
    fn a_cargo_source_with_an_empty_version_is_skipped() {
        let mut entry = valid_entry();
        entry["source"] = serde_json::json!({ "kind": "cargo", "version": "" });
        let document = wrap_index(vec![entry]);

        let index = parse_index(document.as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 0);
        assert_eq!(index.skipped, 1);
    }

    /// fails if a kind this file claims to support is skipped in practice.
    ///
    /// This is the dangerous direction, and the same one the category test
    /// describes: a kind the docs site accepts and shep does not means a
    /// contributor's entry publishes and is then dropped by every
    /// `shep dogs --available`, counted as an anonymous `1 entry skipped`.
    /// Adding a kind to the list and to dogs.ts without adding it to
    /// `validate_source` would otherwise pass every other test here.
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

    /// fails if the CLI's source kinds and the docs site's drift apart.
    ///
    /// Same shape as the category test below, and the same silence when it
    /// breaks. `cargo` was missing here for the whole of the index's first
    /// life, so every published dog would have been advertised with a git
    /// install; that gap is what this pins shut.
    ///
    /// Skips outside the workspace checkout -- see
    /// [`read_workspace_web_file`].
    #[test]
    fn the_source_kinds_match_the_docs_site_list() {
        let Some(dogs_ts) = read_workspace_web_file("src/data/dogs.ts") else {
            return;
        };

        // Past the `=` before splitting on quotes: the declaration reads
        // `const SOURCE_KINDS: readonly DogSource["kind"][] = [...]`, and
        // that `"kind"` in the type sits before the array. The category
        // test needs no such step because its declaration carries no
        // quotes ahead of its own `=`.
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

    /// fails if the CLI's category list and the docs site's drift apart.
    ///
    /// They are two independent six-string lists in two languages, and
    /// nothing but this test holds them equal. Drift is silent and it cuts
    /// both ways: a category the site accepts and shep does not means a
    /// contributor's entry builds, publishes, and is then skipped by every
    /// `shep dogs --available` that reads it -- counted only as an
    /// anonymous `1 entry skipped`.
    ///
    /// Reads `web/src/data/dogs.ts` at runtime rather than with
    /// `include_str!`, and skips outside the workspace checkout -- see
    /// [`read_workspace_web_file`].
    ///
    /// Only the runtime array is read here. The `DogCategory` union above
    /// it in the same file cannot drift on its own -- the array is typed
    /// `readonly DogCategory[]`, so TypeScript fails the site's own build
    /// if they disagree.
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

    /// fails if the checked-in editor schema (`web/public/dogs.schema.json`)
    /// drifts from either list this file itself enforces -- the docs
    /// trigger's own "$schema pointing at a 404 is worse than none" applies
    /// just as much to "$schema pointing at a schema that lies". This is
    /// the same drift-guard shape as the two tests above, aimed at the
    /// schema instead of the docs site's `dogs.ts`, and reading real JSON
    /// with `serde_json` rather than splitting text, since the schema (and
    /// only the schema, of these three files) actually is JSON.
    ///
    /// Mutation check: deleting the `"cargo"` variant from the schema's
    /// `source.oneOf` (its real, once-shipped bug -- caught by hand while
    /// writing this test, not by the test itself, which is exactly the
    /// gap this closes) reddens the source-kinds half of this assertion.
    ///
    /// Skips outside the workspace checkout -- see
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

    /// fails if the default index URL ever becomes plaintext. It is the one
    /// URL an operator never types, so nothing else would notice.
    #[test]
    fn the_default_index_url_is_https() {
        assert!(
            DEFAULT_INDEX_URL.starts_with("https://"),
            "{DEFAULT_INDEX_URL}"
        );
    }

    /// fails if a plaintext index URL is fetched. The refusal must land
    /// before any connection is attempted, which is why this can name a
    /// host it never reaches: if the check regressed, this test would try
    /// to open a socket to `example.com` and fail differently.
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

    /// fails if a host that only looks like loopback gets the carve-out.
    /// A prefix or suffix match would let `127.0.0.1.example.com` and
    /// `evil.com@127.0.0.1` (which `parse_url` hands over as a host
    /// verbatim) through, and both resolve somewhere else entirely.
    #[tokio::test]
    async fn a_host_that_merely_contains_a_loopback_literal_is_still_refused() {
        for url in [
            "http://127.0.0.1.example.com/dogs.json",
            "http://evil.com@127.0.0.1/dogs.json",
            "http://localhost.example.com/dogs.json",
        ] {
            assert!(
                matches!(fetch_index(url).await, Err(IndexError::InsecureUrl(_))),
                "{url} was not refused"
            );
        }
    }

    /// fails if a locally served index cannot be read. This is the carve-out
    /// the https policy makes for loopback, and the path every integration
    /// test of the verb takes; without it there is no way to exercise the
    /// feature without the live site.
    ///
    /// The await's forcing mechanism is `fetch_index`'s own ten second
    /// budget, which is the code under test: a server that never answers
    /// fails this test rather than hanging it.
    #[tokio::test]
    async fn a_loopback_http_index_is_read_because_that_is_how_a_local_one_is_served() {
        let url = serve_index(one_entry_with_description("clean\u{1b}[2Jhere")).await;
        let index = fetch_index(&url).await.expect("read");
        assert_eq!(index.dogs.len(), 1);
        assert_eq!(index.sanitised, 1);
        assert!(!index.dogs[0].description.contains('\u{1b}'));
    }
}
