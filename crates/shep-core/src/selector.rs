//! Target selection: one parse for every CLI verb and RPC filter
//!
//! Precedence: `all` > `fold:<name>` > `/regex/` > all-digits id > name.

use core::fmt;

/// A parsed process selector (spec §9: name, id, `all`, `/regex/`, `fold:`)
#[derive(Debug, Clone)]
pub enum ProcessSelector {
    /// Every sheep in the flock
    All,
    /// By numeric id
    Id(u32),
    /// By exact name
    Name(String),
    /// By regex over names (slash-delimited on the CLI)
    Regex(regex::Regex),
    /// Every sheep in a fold
    Fold(String),
}

impl ProcessSelector {
    /// Parses CLI selector syntax
    ///
    /// # Errors
    ///
    /// - [`SelectorError::Empty`] — empty input.
    /// - [`SelectorError::EmptyFold`] — `fold:` with no name.
    /// - [`SelectorError::BadRegex`] — `/re/` body rejected by the regex
    ///   crate (carries its message).
    pub fn parse(input: &str) -> Result<Self, SelectorError> {
        if input.is_empty() {
            return Err(SelectorError::Empty);
        }
        if input == "all" {
            return Ok(Self::All);
        }
        if let Some(fold) = input.strip_prefix("fold:") {
            if fold.is_empty() {
                return Err(SelectorError::EmptyFold);
            }
            return Ok(Self::Fold(fold.to_string()));
        }
        if input.len() >= 2 && input.starts_with('/') && input.ends_with('/') {
            let body = &input[1..input.len() - 1];
            return regex::Regex::new(body)
                .map(Self::Regex)
                .map_err(|e| SelectorError::BadRegex(e.to_string()));
        }
        if input.bytes().all(|b| b.is_ascii_digit())
            && let Ok(id) = input.parse()
        {
            return Ok(Self::Id(id));
        }
        Ok(Self::Name(input.to_string()))
    }

    /// Whether this selector names ONE entry the caller already knew of, by
    /// its name or its id, rather than sweeping whatever matches.
    ///
    /// The distinction a dog turns on: a dog is a process an operator
    /// installed, not a member of the flock `all` means, so a wildcard must
    /// pass it by while `shep restart metrics` still reaches it.
    /// [`Self::Regex`] and [`Self::Fold`] are wildcards here even when they
    /// happen to match one entry — what matters is that the operator did not
    /// name it.
    #[must_use]
    pub const fn is_exact(&self) -> bool {
        match self {
            Self::Id(_) | Self::Name(_) => true,
            Self::All | Self::Regex(_) | Self::Fold(_) => false,
        }
    }

    /// Tests one sheep against this selector
    #[must_use]
    pub fn matches(&self, name: &str, id: u32, fold: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Id(want) => *want == id,
            Self::Name(want) => want == name,
            Self::Regex(re) => re.is_match(name),
            Self::Fold(want) => fold == Some(want.as_str()),
        }
    }
}

/// Error type returned from [`ProcessSelector::parse`]
///
/// `#[non_exhaustive]`: only two of today's four selector kinds have a
/// failure mode of their own (`fold:` with no name, `/regex/` that will not
/// compile) — a future kind with its own malformed-value class, such as a
/// `status:` filter rejecting a name that is not a known state, would need a
/// new variant rather than stretching [`Self::BadRegex`] to mean something
/// it does not, and shep-core is a published library an out-of-tree matcher
/// should not break for (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorError {
    /// The selector string was empty
    Empty,
    /// `fold:` with no fold name after the colon
    EmptyFold,
    /// The `/regex/` body failed to compile (carries the regex message)
    BadRegex(String),
}

impl fmt::Display for SelectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("selector is empty"),
            Self::EmptyFold => f.write_str("fold selector is missing a name"),
            Self::BadRegex(m) => write!(f, "invalid selector regex: {m}"),
        }
    }
}

impl core::error::Error for SelectorError {}

impl std::convert::TryFrom<crate::protocol::SelectorSpec> for ProcessSelector {
    type Error = SelectorError;

    /// Compiles a wire selector into a matchable one
    ///
    /// # Errors
    ///
    /// - [`SelectorError::BadRegex`] — the peer-supplied pattern fails to
    ///   compile or exceeds the 1 MiB compiled-size bound.
    fn try_from(spec: crate::protocol::SelectorSpec) -> Result<Self, Self::Error> {
        use crate::protocol::SelectorSpec;
        Ok(match spec {
            SelectorSpec::All => Self::All,
            SelectorSpec::Id(id) => Self::Id(id),
            SelectorSpec::Name(name) => Self::Name(name),
            SelectorSpec::Fold(fold) => Self::Fold(fold),
            SelectorSpec::Regex(src) => Self::Regex(
                // Peer-supplied pattern: bound compiled-program memory.
                regex::RegexBuilder::new(&src)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| SelectorError::BadRegex(e.to_string()))?,
            ),
        })
    }
}

impl From<&ProcessSelector> for crate::protocol::SelectorSpec {
    fn from(sel: &ProcessSelector) -> Self {
        use crate::protocol::SelectorSpec;
        match sel {
            ProcessSelector::All => SelectorSpec::All,
            ProcessSelector::Id(id) => SelectorSpec::Id(*id),
            ProcessSelector::Name(name) => SelectorSpec::Name(name.clone()),
            ProcessSelector::Regex(re) => SelectorSpec::Regex(re.as_str().to_string()),
            ProcessSelector::Fold(fold) => SelectorSpec::Fold(fold.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rules() {
        assert!(matches!(
            ProcessSelector::parse("all").unwrap(),
            ProcessSelector::All
        ));
        assert!(matches!(
            ProcessSelector::parse("3").unwrap(),
            ProcessSelector::Id(3)
        ));
        assert!(matches!(
            ProcessSelector::parse("web").unwrap(),
            ProcessSelector::Name(n) if n == "web"
        ));
        assert!(matches!(
            ProcessSelector::parse("/^w/").unwrap(),
            ProcessSelector::Regex(_)
        ));
        assert!(matches!(
            ProcessSelector::parse("fold:backend").unwrap(),
            ProcessSelector::Fold(fname) if fname == "backend"
        ));
    }

    #[test]
    fn parse_errors() {
        assert_eq!(
            ProcessSelector::parse("").unwrap_err(),
            SelectorError::Empty
        );
        assert_eq!(
            ProcessSelector::parse("fold:").unwrap_err(),
            SelectorError::EmptyFold
        );
        assert!(matches!(
            ProcessSelector::parse("/((/").unwrap_err(),
            SelectorError::BadRegex(_)
        ));
    }

    #[test]
    fn matching() {
        let by_name = ProcessSelector::parse("web").unwrap();
        assert!(by_name.matches("web", 0, None));
        assert!(!by_name.matches("worker", 0, None));

        let by_regex = ProcessSelector::parse("/^w/").unwrap();
        assert!(by_regex.matches("worker", 9, None));
        assert!(!by_regex.matches("api", 9, None));

        let by_fold = ProcessSelector::parse("fold:backend").unwrap();
        assert!(by_fold.matches("anything", 0, Some("backend")));
        assert!(!by_fold.matches("anything", 0, None));

        assert!(
            ProcessSelector::parse("all")
                .unwrap()
                .matches("x", 42, None)
        );
        assert!(ProcessSelector::parse("42").unwrap().matches("x", 42, None));
    }

    /// fails if `Fold` or `Regex` is counted as exact. Either mistake makes
    /// `shep reload /^web/` sweep up a dog, which is the failure the split
    /// exists to prevent — and it is invisible until a flock happens to run
    /// a dog whose name the pattern matches.
    #[test]
    fn only_a_name_or_an_id_names_one_entry_the_caller_knew_of() {
        assert!(ProcessSelector::Name("bark".into()).is_exact());
        assert!(ProcessSelector::Id(4).is_exact());
        assert!(!ProcessSelector::All.is_exact());
        assert!(!ProcessSelector::Fold("api".into()).is_exact());
        // Built through the real parser: a `Regex` is a wildcard even when
        // its pattern is a literal that can only ever match one name.
        assert!(!ProcessSelector::parse("/^bark$/").unwrap().is_exact());
    }

    #[test]
    fn a_name_that_looks_numeric_is_an_id() {
        // Documented precedence (spec §9): digits select by id. A sheep
        // literally named "42" must be selected by /^42$/ or renamed.
        assert!(matches!(
            ProcessSelector::parse("42").unwrap(),
            ProcessSelector::Id(42)
        ));
    }

    #[test]
    fn selector_spec_bridges() {
        use crate::protocol::SelectorSpec;
        let sel: ProcessSelector = SelectorSpec::Regex("^w".to_string()).try_into().unwrap();
        assert!(sel.matches("web", 1, None));
        assert_eq!(
            SelectorSpec::from(&sel),
            SelectorSpec::Regex("^w".to_string())
        );
        for spec in [
            SelectorSpec::All,
            SelectorSpec::Id(3),
            SelectorSpec::Name("web".to_string()),
            SelectorSpec::Fold("backend".to_string()),
        ] {
            let sel: ProcessSelector = spec.clone().try_into().unwrap();
            assert_eq!(SelectorSpec::from(&sel), spec);
        }
    }

    #[test]
    fn selector_spec_bad_regex_is_typed_error() {
        use crate::protocol::SelectorSpec;
        assert!(matches!(
            ProcessSelector::try_from(SelectorSpec::Regex("((".to_string())).unwrap_err(),
            SelectorError::BadRegex(_)
        ));
    }

    #[test]
    fn selector_spec_oversized_regex_is_rejected() {
        // Peer-supplied pattern: size_limit bounds compiled-program memory.
        // The pattern (a|b|...)^N where N is the number of alternations
        // repeated many times generates a huge compiled regex that exceeds
        // the 1 MiB limit: many alternations * repetition factor.
        use crate::protocol::SelectorSpec;
        let huge = format!("(a{}){{10000}}", "|b".repeat(100_000));
        assert!(ProcessSelector::try_from(SelectorSpec::Regex(huge)).is_err());
    }
}
