//! One `parse_selector`, shared by `lifecycle`, `logs`, `query`, `bleats`
//! and `trigger`.

use shep_core::selector::ProcessSelector;

use crate::exit::ExitCode;
use crate::output::Streams;

/// Parses `raw` client-side, so a malformed selector is a local usage error
/// rather than a round trip. The daemon re-parses it anyway.
///
/// Returns a [`ProcessSelector`], not a `SelectorSpec`: callers that put one
/// on the wire convert with `SelectorSpec::from(&selector)` themselves.
pub(crate) fn parse_selector(
    streams: &mut Streams<'_>,
    raw: &str,
) -> Result<ProcessSelector, ExitCode> {
    match ProcessSelector::parse(raw) {
        Ok(selector) => Ok(selector),
        Err(err) => Err(streams.fail(ExitCode::Usage, &err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Format;

    #[test]
    fn a_well_formed_selector_parses_without_touching_streams() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let selector = parse_selector(&mut streams, "web").unwrap();
        assert!(matches!(selector, ProcessSelector::Name(name) if name == "web"));
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    /// `/[/` is one of only three inputs the selector grammar rejects.
    #[test]
    fn a_malformed_selector_is_a_local_usage_error() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let code = parse_selector(&mut streams, "/[/").unwrap_err();
        assert_eq!(code, ExitCode::Usage);
        assert!(out.is_empty(), "a usage error goes to stderr, not stdout");
        assert!(
            !err.is_empty(),
            "the caller must be told why the selector was rejected"
        );
    }
}
