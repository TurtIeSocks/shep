//! The dog side of the probe contract: one call a dog makes as its first
//! line, which answers every question shep asks its binary.
//!
//! A dog is a plugin process the shepherd supervises. Before shep adopts one,
//! and again whenever it needs the dog's config schema, it spawns the binary
//! with a flag and reads what comes back. [`probe`] answers both flags, so
//! neither the answer's format nor the flag names are ever typed by a dog
//! author:
//!
//! ```no_run
//! # #[derive(schemars::JsonSchema, shep_client::dogs::DogConfig)]
//! # struct MyDogConfig {}
//! fn main() {
//!     shep_client::dogs::probe::<MyDogConfig>(
//!         env!("CARGO_PKG_NAME"),
//!         env!("CARGO_PKG_VERSION"),
//!     );
//!     // ...normal startup, reached only when this run is not a probe.
//! }
//! ```
//!
//! # Why the name and version are arguments
//!
//! They are the two facts only the dog knows. `env!` expands where it is
//! written, so a `CARGO_PKG_VERSION` inside this crate would report
//! `shep-client`'s version to every dog that called it. Everything else in
//! the answer, the flag names, the key that carries the protocol number, and
//! the protocol number itself, comes from shep and is not the dog's to get
//! wrong.
//!
//! # Answering is optional, and that is what the `schema` feature turns off
//!
//! A dog that answers nothing is recorded as having no schema and is refused
//! nothing. Turning `schema` off (it is on by default) therefore is not a
//! broken state: [`probe`] still answers the version flag, and for the schema
//! flag it exits without printing, which reads to shep as a dog with no
//! schema.

use std::io::Write as _;

pub use shep_core::dogs::SECRET_KEY;
use shep_core::dogs::{SCHEMA_FLAG, SHEP_PROTOCOL_KEY, VERSION_FLAG};
/// The derive that implements [`DogConfig`], re-exported so a dog takes one
/// dependency rather than two.
///
/// Its own documentation carries the rules: which shapes accept
/// `#[shep(secret)]`, which refuse it, and what the expansion looks like.
pub use shep_macros::DogConfig;

/// What a dog's config type tells shep about itself: which of its fields are
/// credentials, and the schema extension key that says so.
///
/// Do not write this impl by hand. Derive it, and mark each credential field
/// with `#[shep(secret)]`. The derive exists precisely so that
/// [`SECRET_KEY`]'s value is never typed by a dog author: a transposed letter
/// in it compiles, validates, marks nothing, and paints a webhook credential
/// on screen.
pub trait DogConfig {
    /// The schema extension key a marked field carries. Always
    /// [`SECRET_KEY`]; it is an associated const so the derive can name it
    /// through this crate rather than reaching into `shep_core`.
    const SECRET_KEY: &'static str;

    /// The Rust identifiers of the fields marked `#[shep(secret)]`, deduped.
    /// A name repeated across the variants of an enum appears once, because
    /// this is a list of names to look for rather than places to look.
    const SECRET_FIELDS: &'static [&'static str];
}

/// A field marked `#[shep(secret)]` that names no property of the config
/// type's own schema, so the marker had nothing to land on.
///
/// A like-named property of some other type under `$defs` does not count.
/// That is a different field of a different type, and letting it answer for
/// this one is how a renamed credential ships unmarked.
///
/// `Debug` is derived: the field it names is an identifier from the dog's
/// own source, never a credential's value (IR-41).
#[cfg(feature = "schema")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFieldMissing {
    /// The Rust identifier that was marked and could not be found.
    pub field: &'static str,
}

#[cfg(feature = "schema")]
impl core::fmt::Display for SecretFieldMissing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "the field `{}` is marked `#[shep(secret)]` but this type's own \
             schema has no property of that name, so the credential would go \
             out unmarked. A `#[serde(rename)]` on the field, or a \
             `rename_all_fields` on the type, renames the property and leaves \
             the mark with nothing to land on.",
            self.field
        )
    }
}

#[cfg(feature = "schema")]
impl core::error::Error for SecretFieldMissing {}

/// The JSON Schema a dog answers the schema flag with: what `schemars`
/// generates for `T`, with every field `T` marked `#[shep(secret)]` carrying
/// [`SECRET_KEY`]. The marks land on `T`'s own fields: the types `T` merely
/// mentions answer for themselves, and are asked separately when one of them
/// is what shep asked about.
///
/// [`probe`] calls this. It is public because a dog that renders its own
/// settings, or a test that wants to see the marks, should not have to spawn
/// itself to get them.
///
/// # Errors
///
/// [`SecretFieldMissing`] when a marked name matches no property of `T`'s own
/// representation. That is a credential which did not get marked, so it is an
/// error rather than a silent pass. A property of that name under `$defs`
/// belongs to another type, so it neither carries the mark nor answers for
/// it.
#[cfg(feature = "schema")]
pub fn config_schema<T: DogConfig + schemars::JsonSchema>()
-> Result<schemars::Schema, SecretFieldMissing> {
    let mut schema = schemars::SchemaGenerator::default().into_root_schema_for::<T>();
    let mut found: Vec<&'static str> = Vec::new();
    // `None` only for a schema that is the bare `true` or `false`, which has
    // no property to mark and so leaves every marked name unfound below.
    if let Some(root) = schema.as_object_mut() {
        mark_secrets_in_object(root, T::SECRET_FIELDS, T::SECRET_KEY, &mut found);
    }
    for field in T::SECRET_FIELDS {
        if !found.contains(field) {
            return Err(SecretFieldMissing { field });
        }
    }
    Ok(schema)
}

/// Where `schemars` puts the schema of every OTHER named type the root
/// mentions, and the one keyword [`mark_secrets_in_object`] refuses to walk.
#[cfg(feature = "schema")]
const DEFS: &str = "$defs";

/// Writes `key` into every property named in `secrets` that belongs to the
/// root type's OWN representation, and records which names were reached.
///
/// The whole node is walked rather than only its `properties`, because a
/// struct is the one shape that has a top-level `properties` to read. An
/// internally tagged enum is a `oneOf` of one object per variant, an untagged
/// one an `anyOf`, and an adjacently tagged one nests each variant's fields
/// under the content property's own `properties`. Reading the top level alone
/// would find nothing in all three, mark nothing, and say nothing, which is
/// the failure this whole contract exists to remove.
///
/// [`DEFS`] is where the walk stops, and that is what keeps the mark on the
/// type shep asked about. Entering it marks a property because some stranger
/// one level down shares a name, and records that name as found, which then
/// answers the missing-name check on behalf of a field that never got marked.
/// Both halves were live at once: `BarkConfig::sinks` holds webhook
/// credentials and is marked, `Rule::sinks` lists sink names and is not, and
/// the second came out marked while a renamed credential could have come out
/// plain.
///
/// Stopping there costs the root nothing. A root enum's variant objects are
/// emitted inline, and a recursive root points at itself with `$ref: "#"`
/// rather than through a definition, so no shape this contract accepts keeps
/// its own fields in `$defs`. A `$ref` needs no skipping of its own: it is a
/// string, and a string is a leaf.
///
/// Two consequences remain, both deliberate. Marking is by NAME within that
/// representation, so two variants sharing a field name are both marked even
/// if only one carried the attribute; that over-redacts, which is the safe
/// direction. And a property whose schema is not an object (a bare `true`,
/// which is legal JSON Schema and which `schemars` does not emit for a typed
/// field) is left alone and not recorded as found, so it surfaces as
/// [`SecretFieldMissing`] rather than as a silent miss.
#[cfg(feature = "schema")]
fn mark_secrets_in_object(
    map: &mut serde_json::Map<String, serde_json::Value>,
    secrets: &[&'static str],
    key: &str,
    found: &mut Vec<&'static str>,
) {
    if let Some(serde_json::Value::Object(properties)) = map.get_mut("properties") {
        for name in secrets {
            if let Some(serde_json::Value::Object(property)) = properties.get_mut(*name) {
                property.insert(key.to_owned(), serde_json::Value::Bool(true));
                if !found.contains(name) {
                    found.push(name);
                }
            }
        }
    }
    for (keyword, value) in map.iter_mut() {
        if keyword == DEFS {
            continue;
        }
        mark_secrets(value, secrets, key, found);
    }
}

/// [`mark_secrets_in_object`] for a node that may be any JSON value: an
/// object is marked, an array is descended (a `oneOf` is one), and anything
/// else is a leaf.
#[cfg(feature = "schema")]
fn mark_secrets(
    node: &mut serde_json::Value,
    secrets: &[&'static str],
    key: &str,
    found: &mut Vec<&'static str>,
) {
    match node {
        serde_json::Value::Object(map) => mark_secrets_in_object(map, secrets, key, found),
        serde_json::Value::Array(items) => {
            for item in items {
                mark_secrets(item, secrets, key, found);
            }
        }
        _ => {}
    }
}

/// Answers shep's probes, and returns when this run is not a probe, so a dog
/// calls it as the first line of `main` and carries on into normal startup.
///
/// `name` and `version` are the dog's own, ordinarily
/// `env!("CARGO_PKG_NAME")` and `env!("CARGO_PKG_VERSION")`. Only the version
/// is read by shep; the name is there because a human runs `--version` too.
///
/// # Exits
///
/// This function ends the process when it answered, with
/// [`process::exit`](std::process::exit): the run was shep asking a question,
/// and a dog that answered and then started up would leave a process behind
/// for shep to kill. Nothing after the call runs in that case, and no
/// destructor runs either, which is why it belongs on the first line of
/// `main` before anything has been opened.
///
/// The exit status is 0 for an answer given, and 1 for a config type whose
/// [`SecretFieldMissing`] makes its schema unpublishable. That failure prints
/// to stderr, so a dog author who runs the flag by hand sees it, and shep
/// sees an empty answer and records a dog with no schema.
#[cfg(feature = "schema")]
pub fn probe<T: DogConfig + schemars::JsonSchema>(name: &str, version: &str) {
    match first_argument().as_deref() {
        Some(VERSION_FLAG) => answer(&version_answer(name, version)),
        Some(SCHEMA_FLAG) => match schema_answer::<T>() {
            Ok(json) => answer(&json),
            Err(err) => {
                eprintln!("{err}");
                std::process::exit(1);
            }
        },
        _ => (),
    }
}

/// Answers shep's probes, and returns when this run is not a probe, so a dog
/// calls it as the first line of `main` and carries on into normal startup.
///
/// `name` and `version` are the dog's own, ordinarily
/// `env!("CARGO_PKG_NAME")` and `env!("CARGO_PKG_VERSION")`. Only the version
/// is read by shep; the name is there because a human runs `--version` too.
///
/// This is the build with the `schema` feature off, so there is no schema to
/// answer with: the schema flag exits without printing, and shep records a
/// dog with no schema, which it refuses nothing for. Exiting rather than
/// falling through is the kinder half of that, since the alternative is a dog
/// booting itself with an argument it does not understand while shep waits
/// out a timeout for an answer that is never coming.
///
/// # Exits
///
/// This function ends the process when it answered, with
/// [`process::exit`](std::process::exit), status 0. Nothing after the call
/// runs in that case, and no destructor runs either, which is why it belongs
/// on the first line of `main` before anything has been opened.
#[cfg(not(feature = "schema"))]
pub fn probe<T: DogConfig>(name: &str, version: &str) {
    match first_argument().as_deref() {
        Some(VERSION_FLAG) => answer(&version_answer(name, version)),
        Some(SCHEMA_FLAG) => std::process::exit(0),
        _ => (),
    }
}

/// The argument shep spawns a probe with, which is the only one it passes.
///
/// The first argument and not a scan of all of them, because that is the
/// contract `docs/dogs.md` publishes and a dog's own arguments are its
/// business. A dog whose real command line happens to start with one of these
/// flags was being probed as far as this contract is concerned.
fn first_argument() -> Option<String> {
    std::env::args().nth(1)
}

/// Prints an answer and ends the process, which is what makes a probe run
/// end where a normal run would carry on.
///
/// The explicit flush is not decoration: [`std::process::exit`] runs no
/// destructor, so nothing else would push a partial buffer out. Both answers
/// end in a newline, which Rust's line-buffered stdout would flush anyway,
/// and neither of those two facts is one to leave the contract resting on.
fn answer(text: &str) -> ! {
    let mut stdout = std::io::stdout();
    // A write to a closed stdout is not something a dog can do anything
    // about, and shep reads it as silence, which is a legal answer.
    let _ = write!(stdout, "{text}");
    let _ = stdout.flush();
    std::process::exit(0);
}

/// The `--version` answer, whole, ending in a newline.
///
/// Split out from [`probe`] so the format can be tested against
/// [`shep_core::dogs::parse_version_answer`], which is the code that reads
/// it. The two used to be a docs snippet and a parser with nothing holding
/// them together.
fn version_answer(name: &str, version: &str) -> String {
    format!(
        "{name} {version}\n{SHEP_PROTOCOL_KEY}: {}\n",
        crate::PROTOCOL_VERSION
    )
}

/// The `--schema` answer, whole, ending in a newline.
///
/// # Errors
///
/// [`SecretFieldMissing`], straight from [`config_schema`].
#[cfg(feature = "schema")]
fn schema_answer<T: DogConfig + schemars::JsonSchema>() -> Result<String, SecretFieldMissing> {
    let schema = config_schema::<T>()?;
    // The same expectation shep-core's own schema printer holds: a schemars
    // `Schema` is a `serde_json::Value` already, so serializing it cannot
    // meet a type serde_json has no representation for.
    let json = serde_json::to_string_pretty(&schema).expect("a schemars Schema always serializes");
    Ok(format!("{json}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(schemars::JsonSchema, DogConfig)]
    #[allow(dead_code, reason = "read by the generated schema, not by Rust")]
    struct Webhook {
        #[shep(secret)]
        url: String,
        channel: String,
    }

    #[derive(schemars::JsonSchema, DogConfig)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    #[allow(dead_code, reason = "read by the generated schema, not by Rust")]
    enum Sink {
        Discord {
            #[shep(secret)]
            url: String,
            quiet: bool,
        },
        Slack {
            #[shep(secret)]
            url: String,
        },
    }

    #[derive(schemars::JsonSchema, DogConfig)]
    #[allow(dead_code, reason = "read by the generated schema, not by Rust")]
    struct Renamed {
        #[shep(secret)]
        #[serde(rename = "webhook_url")]
        url: String,
    }

    /// A type the root only mentions, so `schemars` hoists it into `$defs`.
    /// Its `token` is an ordinary string, and it is named to collide with
    /// the credential the two roots below mark.
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code, reason = "read by the generated schema, not by Rust")]
    struct Inner {
        token: String,
    }

    #[derive(schemars::JsonSchema, DogConfig)]
    #[allow(dead_code, reason = "read by the generated schema, not by Rust")]
    struct Outer {
        #[shep(secret)]
        token: String,
        inner: Inner,
    }

    #[derive(schemars::JsonSchema, DogConfig)]
    #[allow(dead_code, reason = "read by the generated schema, not by Rust")]
    struct RenamedOuter {
        #[shep(secret)]
        #[serde(rename = "api_token")]
        token: String,
        inner: Inner,
    }

    /// Both halves in one test on purpose: an implementation that marked
    /// every property would pass a test that only checked the marked one.
    #[test]
    fn a_secret_field_carries_the_marker_and_a_plain_one_does_not() {
        let schema = config_schema::<Webhook>().expect("`url` is a real property");
        let props = schema
            .as_value()
            .get("properties")
            .expect("a derived struct schema has properties");

        assert_eq!(
            props.get("url").and_then(|url| url.get(SECRET_KEY)),
            Some(&serde_json::Value::Bool(true)),
            "the marked field carries the marker"
        );
        assert_eq!(
            props.get("channel").and_then(|it| it.get(SECRET_KEY)),
            None,
            "the unmarked field carries nothing"
        );
    }

    /// A tagged enum has no top-level `properties` at all: it is a `oneOf` of
    /// one object per variant. Marking code that reads only the top level
    /// finds nothing, marks nothing, and says nothing.
    #[test]
    fn a_marker_reaches_every_variant_of_a_tagged_enum_and_no_plain_field() {
        let schema = config_schema::<Sink>().expect("`url` is a real property in both variants");
        let variants = schema
            .as_value()
            .get("oneOf")
            .and_then(|it| it.as_array())
            .expect("a tagged enum is a oneOf");
        assert_eq!(variants.len(), 2);

        for variant in variants {
            let props = variant
                .get("properties")
                .expect("each variant carries its own properties");
            assert_eq!(
                props.get("url").and_then(|url| url.get(SECRET_KEY)),
                Some(&serde_json::Value::Bool(true)),
                "every occurrence of the marked name is marked"
            );
            assert_eq!(
                props.get("quiet").and_then(|it| it.get(SECRET_KEY)),
                None,
                "a plain field in the same variant carries nothing"
            );
        }
    }

    #[test]
    fn a_renamed_secret_field_is_an_error_rather_than_a_silent_pass() {
        assert_eq!(
            config_schema::<Renamed>(),
            Err(SecretFieldMissing { field: "url" })
        );
    }

    /// A mark names a field of the type shep asked about, so a like-named
    /// property of some other type the root merely mentions is not that
    /// field and must come out plain. `Rule::sinks` is the live case: it
    /// lists sink NAMES, one level under a `BarkConfig::sinks` that really
    /// does hold credentials.
    #[test]
    fn a_like_named_property_of_a_nested_type_is_left_plain() {
        let schema = config_schema::<Outer>().expect("`token` is a real property of the root");
        let schema = schema.as_value();

        assert_eq!(
            schema.pointer("/properties/token/x-shep-secret"),
            Some(&serde_json::Value::Bool(true)),
            "the root's own marked field carries the marker"
        );
        assert_eq!(
            schema.pointer("/$defs/Inner/properties/token/x-shep-secret"),
            None,
            "a stranger that shares the name is not the marked field"
        );
    }

    /// The same reach, pointing the other way: a `$defs` entry that happens
    /// to carry the pre-rename name would satisfy the missing-field check on
    /// behalf of a credential that shipped unmarked.
    #[test]
    fn a_renamed_secret_field_is_an_error_even_when_a_nested_type_has_the_old_name() {
        assert_eq!(
            config_schema::<RenamedOuter>(),
            Err(SecretFieldMissing { field: "token" })
        );
    }

    /// The two ends of the grammar: what a dog prints and what shep reads.
    /// Pinned as a round trip rather than as a string, because the format is
    /// only ever interesting to the parser.
    #[test]
    fn the_version_answer_parses_with_the_shepherds_own_parser() {
        let answer = version_answer("shep-otel", "0.1.3");
        let parsed = shep_core::dogs::parse_version_answer(&answer)
            .expect("the answer shep's own parser cannot read is the bug this pins");

        assert_eq!(parsed.version, "0.1.3");
        assert_eq!(parsed.protocol, Some(crate::PROTOCOL_VERSION));
    }
}
