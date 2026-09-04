//! The probe contract between shep and a dog: the flag names, the
//! `shep-protocol:` line's grammar, and the schema's secret marker key.
//!
//! # Why this lives here and not in the crate that asks or the crate that answers
//!
//! `shep-cli`'s `adopt` spawns a candidate binary with [`VERSION_FLAG`] and
//! [`SCHEMA_FLAG`] and parses what it prints; `shep-client` (the dog side)
//! answers both from `shep_client::dogs::probe`. Before that call existed,
//! both sides read a string a dog author hand-typed from a snippet in
//! `docs/dogs.md`, so a typo read as "protocol unknown" and nothing said so.
//! One definition, owned by the crate both already depend on, is what lets
//! the asker and the answerer agree by construction instead of by copying a
//! doc snippet correctly.
//!
//! The asker itself (spawning the binary, applying the timeout, deciding
//! whether an unknown protocol refuses an adopt) stays in `shep-cli`,
//! beside the rest of the vetting `adopt` already does. Only the shape of
//! the question and the answer moves here.
//!
//! [`DogVersion`] moved with the parser rather than staying behind: its two
//! fields (`version`, `protocol`) are plain data with no CLI-specific type
//! in them, and it IS the grammar `parse_version_answer` returns, so
//! splitting the struct from the function that builds it would put one
//! definition of the answer's shape in one crate and the reader of that
//! shape in another.

/// The flag a candidate is spawned with when shep asks for its version, and
/// the one `docs/dogs.md` publishes as the contract. Read by
/// `shep-cli`'s `adopt`; answered, from release 2, by
/// `shep_client::dogs::probe`.
pub const VERSION_FLAG: &str = "--version";

/// The flag a candidate is spawned with when shep asks for its config
/// schema. Asked by `shep-cli`'s `adopt`, beside the version and on the
/// same terms: a dog that answers nothing is refused nothing. Answered by
/// `shep_client::dogs::probe`.
pub const SCHEMA_FLAG: &str = "--schema";

/// The one key [`parse_version_answer`] reads in a `--version` answer.
/// Every other `shep-` key is reserved for a number this shep has not
/// heard of, and is ignored rather than refused, so a dog written against a
/// later contract stays adoptable by this one.
pub const SHEP_PROTOCOL_KEY: &str = "shep-protocol";

/// The schemars extension key that marks a config field as a credential.
/// Written by the `DogConfig` derive, which exists so that no dog author
/// ever types it; the reader in `shep lookout` that redacts a field
/// carrying it arrives in a later task. Getting this string right matters
/// more than the other three here, because a typo in it does not fail
/// loudly: the schema still validates, the field is simply not marked, and
/// a credential can end up rendered on screen.
pub const SECRET_KEY: &str = "x-shep-secret";

/// What a dog answered [`VERSION_FLAG`] with, parsed by
/// [`parse_version_answer`] from the format `docs/dogs.md` publishes.
///
/// Two fields rather than one, because they answer different questions:
/// `protocol` decides whether the dog can handshake at all, and `version`
/// only says which build it is. A dog may give the second and not the
/// first, which is why `protocol` is optional and an absent one reads as
/// unknown rather than as a fault.
#[derive(Debug, PartialEq, Eq)]
pub struct DogVersion {
    /// The last whitespace-separated field of line 1, the version. The
    /// name before it is ignored, so a crate whose name differs from the
    /// dog's registered name answers correctly without knowing it.
    pub version: String,
    /// The `shep-protocol` line's value, and `None` when the answer carried
    /// no such line or carried one that is not a decimal number. Answering
    /// is optional, so `None` is an unknown protocol rather than a fault.
    pub protocol: Option<u32>,
}

/// Parses the format `docs/dogs.md` publishes: `<name> <version>` on line
/// 1, then `<key>: <value>` lines.
///
/// `None` when there is no line 1 to read a version from. Everything past
/// that is tolerated rather than refused: unknown keys, blank lines, key
/// order, and a `shep-protocol` that is not a number, because a shep that
/// refuses a dog over the shape of text the dog never promised to print is
/// refusing on its own guess. The strictness is all in the other direction:
/// only an exact [`SHEP_PROTOCOL_KEY`] carrying a decimal is believed, and
/// only a believed protocol can refuse.
#[must_use]
pub fn parse_version_answer(text: &str) -> Option<DogVersion> {
    let mut lines = text.lines();
    let version = lines.next()?.split_whitespace().next_back()?.to_string();
    let mut protocol = None;
    for line in lines {
        if let Some((key, value)) = line.split_once(':')
            && key.trim() == SHEP_PROTOCOL_KEY
        {
            protocol = value.trim().parse().ok();
        }
    }
    Some(DogVersion { version, protocol })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_output_is_no_answer() {
        assert_eq!(parse_version_answer(""), None);
    }

    #[test]
    fn a_bad_protocol_number_reads_as_unknown_not_a_fault() {
        assert_eq!(
            parse_version_answer("shep-otel 0.1.3\nshep-protocol: two\n"),
            Some(DogVersion {
                version: "0.1.3".to_string(),
                protocol: None,
            })
        );
    }
}
