//! The probe contract between shep and a dog: the flag names, the
//! `shep-protocol:` line's grammar, and the schema's secret marker key.
//!
//! Shared by `shep-cli`'s `adopt` (the asker) and `shep_client::dogs::probe`
//! (the answerer), so the two agree by construction rather than by copying
//! a doc snippet.

/// The flag a candidate is spawned with when shep asks for its version; the
/// contract `docs/dogs.md` publishes. Read by `shep-cli`'s `adopt`, answered
/// by `shep_client::dogs::probe`.
pub const VERSION_FLAG: &str = "--version";

/// The flag a candidate is spawned with when shep asks for its config
/// schema. Asked by `shep-cli`'s `adopt` on the same terms as
/// [`VERSION_FLAG`]: a dog that answers nothing is refused nothing.
pub const SCHEMA_FLAG: &str = "--schema";

/// The one key [`parse_version_answer`] reads in a `--version` answer.
/// Every other `shep-` key is reserved for a number this shep has not
/// heard of, and is ignored rather than refused, so a dog written against a
/// later contract stays adoptable by this one.
pub const SHEP_PROTOCOL_KEY: &str = "shep-protocol";

/// The schemars extension key that marks a config field as a credential.
/// Written by the `DogConfig` derive. A typo here fails silently: the schema
/// still validates, the field is simply not marked, and a credential can
/// render unredacted.
pub const SECRET_KEY: &str = "x-shep-secret";

/// What a dog answered [`VERSION_FLAG`] with, parsed by
/// [`parse_version_answer`] from the format `docs/dogs.md` publishes.
///
/// `protocol` decides whether the dog can handshake at all; `version` only
/// names the build. `protocol` is optional: an absent one reads as unknown,
/// not a fault.
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
/// `None` when there is no line 1. Unknown keys, blank lines, key order and
/// a non-numeric `shep-protocol` are all tolerated rather than refused;
/// only an exact [`SHEP_PROTOCOL_KEY`] carrying a decimal is believed.
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
