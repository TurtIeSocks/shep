//! One `parse_selector`, shared by every verb that takes a selector off the
//! command line.
//!
//! This used to be four near-identical private copies, one per verb module
//! (`lifecycle`, `logs`, `query`, `bleats`) — a `trigger` module would have
//! made five. Pulled out on its own, as its own commit, so the verb that
//! needed a fifth copy could land on top of one shared function instead of
//! adding to the pile.

use shep_core::selector::ProcessSelector;

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{Streams, emit_error};

/// Parses `raw` client-side, so a malformed selector is a fast local usage
/// error rather than a round trip to the daemon (the daemon re-parses it
/// too, but only after this one already succeeded).
///
/// Returns a [`ProcessSelector`], not a `SelectorSpec`: most callers put a
/// selector on the wire and convert what this returns themselves, with
/// `SelectorSpec::from(&selector)` — `bleats` is the one exception, and
/// never puts a selector on the wire at all (the daemon's topic filter has
/// no sheep identity to match one against), so it uses what this returns
/// directly.
pub(crate) fn parse_selector(
    streams: &mut Streams<'_>,
    fmt: Format,
    raw: &str,
) -> Result<ProcessSelector, ExitCode> {
    match ProcessSelector::parse(raw) {
        Ok(selector) => Ok(selector),
        Err(err) => {
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Usage.code_str(),
                &err.to_string(),
            );
            Err(ExitCode::Usage)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_selector_parses_without_touching_streams() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let selector = parse_selector(&mut streams, Format::Table, "web").unwrap();
        assert!(matches!(selector, ProcessSelector::Name(name) if name == "web"));
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    /// `/[/` is one of the only three inputs the selector grammar rejects —
    /// same fixture `lifecycle`'s and `logs`'s own malformed-selector tests
    /// use.
    #[test]
    fn a_malformed_selector_is_a_local_usage_error() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let code = parse_selector(&mut streams, Format::Table, "/[/").unwrap_err();
        assert_eq!(code, ExitCode::Usage);
        assert!(out.is_empty(), "a usage error goes to stderr, not stdout");
        assert!(
            !err.is_empty(),
            "the caller must be told why the selector was rejected"
        );
    }
}
