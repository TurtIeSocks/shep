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
        if input.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(id) = input.parse() {
                return Ok(Self::Id(id));
            }
        }
        Ok(Self::Name(input.to_string()))
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

    #[test]
    fn a_name_that_looks_numeric_is_an_id() {
        // Documented precedence (spec §9): digits select by id. A sheep
        // literally named "42" must be selected by /^42$/ or renamed.
        assert!(matches!(
            ProcessSelector::parse("42").unwrap(),
            ProcessSelector::Id(42)
        ));
    }
}
