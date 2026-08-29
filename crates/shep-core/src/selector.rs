//! Target selection: one parse for every CLI verb and RPC filter
//!
//! Precedence: `all` > `fold:<name>` > `/regex/` > all-digits id > glob >
//! `name:slot` > name.

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
    /// One instance of one app, written `name:slot` on the CLI
    Instance {
        /// The app name, which cannot itself contain a colon
        name: String,
        /// The instance slot, counting from 0
        slot: u32,
    },
}

/// Whether `input` carries a glob metacharacter, and so was meant as a
/// pattern rather than as a name.
///
/// Checked after `all`, `fold:`, `/regex/` and the id form, so none of those
/// can be shadowed by a name that happens to contain one of these.
///
/// A name with no metacharacter stays an exact name -- `web.1` is the sheep
/// called `web.1`, not a pattern where `.` means "any character". That is the
/// whole reason globs are worth having over regex here: the punctuation in an
/// ordinary name means nothing.
fn is_glob(input: &str) -> bool {
    input.contains(['*', '?', '[', '{'])
}

/// Compiles a glob and hands back its regex source.
///
/// `globset` owns glob semantics rather than this module hand-rolling them:
/// `*`, `?`, character classes and `{a,b}` alternates all behave the way they
/// do everywhere else, and escaping the rest is its problem. The pattern it
/// produces is already anchored, so `zeus-*` matches `zeus-auth` and not
/// `my-zeus-auth`.
///
/// The `(?-u)` prefix is stripped because `globset` compiles for BYTES, where
/// `.` may match invalid UTF-8, and `regex::Regex` refuses that outright --
/// a name is a `String`, so matching in char mode is both correct here and
/// the only thing that compiles.
///
/// Deliberately turned into a [`ProcessSelector::Regex`] rather than a
/// selector variant of its own: `SelectorSpec` is the wire, and a new variant
/// there is a protocol change an older daemon could not deserialize. This
/// way a glob works against a shepherd built before globs existed.
///
/// [`ProcessSelector::Instance`] takes the protocol change this trick avoids,
/// because a slot is not part of a name and cannot be folded into a regex the
/// way a glob's characters can. That tradeoff is knowing: an older daemon
/// cannot deserialize `name:slot`, and there is no equivalent way around it.
///
/// # Errors
///
/// - [`SelectorError::BadGlob`] — the pattern is not a valid glob.
fn glob_to_regex(input: &str) -> Result<String, SelectorError> {
    let glob = globset::Glob::new(input).map_err(|e| SelectorError::BadGlob(e.to_string()))?;
    let source = glob.regex().to_string();
    Ok(source
        .strip_prefix("(?-u)")
        .map_or(source.clone(), ToString::to_string))
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
    /// - [`SelectorError::BadGlob`] — a pattern carrying `*`, `?`, `[` or `{`
    ///   was rejected by `globset` (carries its message).
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
        if is_glob(input) {
            return glob_to_regex(input)
                .and_then(|re| {
                    regex::Regex::new(&re).map_err(|e| SelectorError::BadRegex(e.to_string()))
                })
                .map(Self::Regex);
        }
        // Last, so every earlier form wins: `fold:` is a prefix test above,
        // a glob containing a colon was already turned into a regex, and an
        // all-digit input was already an id. A name cannot contain a colon
        // (`config::normalize` refuses one), so splitting on the last colon
        // cannot cut a name in half.
        if let Some((name, slot)) = input.rsplit_once(':')
            && !name.is_empty()
            && !slot.is_empty()
            && slot.bytes().all(|b| b.is_ascii_digit())
            && let Ok(slot) = slot.parse()
        {
            return Ok(Self::Instance {
                name: name.to_string(),
                slot,
            });
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
            Self::Id(_) | Self::Name(_) | Self::Instance { .. } => true,
            Self::All | Self::Regex(_) | Self::Fold(_) => false,
        }
    }

    /// Tests one sheep against this selector
    #[must_use]
    pub fn matches(&self, name: &str, id: u32, fold: Option<&str>, instance: Option<u32>) -> bool {
        match self {
            Self::All => true,
            Self::Id(want) => *want == id,
            Self::Name(want) => want == name,
            Self::Regex(re) => re.is_match(name),
            Self::Fold(want) => fold == Some(want.as_str()),
            // `None` means the peer daemon predates the slot field, so this
            // row cannot be shown to be the one asked for. Refusing to match
            // is the safe direction: a restart reaches nothing rather than
            // reaching every instance of the name.
            Self::Instance { name: want, slot } => want == name && instance == Some(*slot),
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
    /// A glob pattern was rejected by `globset` (carries its message)
    BadGlob(String),
}

impl fmt::Display for SelectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("selector is empty"),
            Self::EmptyFold => f.write_str("fold selector is missing a name"),
            Self::BadRegex(m) => write!(f, "invalid selector regex: {m}"),
            Self::BadGlob(m) => write!(f, "invalid selector glob: {m}"),
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
            SelectorSpec::Instance { name, slot } => Self::Instance { name, slot },
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
            ProcessSelector::Instance { name, slot } => SelectorSpec::Instance {
                name: name.clone(),
                slot: *slot,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of globs over regex: ordinary punctuation in a name means
    /// nothing. `web.1` is the sheep called `web.1`, not a pattern.
    #[test]
    fn a_name_without_a_metacharacter_is_still_an_exact_name() {
        for plain in ["zeus-auth", "web.1", "api_v2", "a-b-c"] {
            let parsed = ProcessSelector::parse(plain).unwrap();
            assert!(
                matches!(&parsed, ProcessSelector::Name(name) if name == plain),
                "{plain} carries no glob metacharacter and is a name, got {parsed:?}"
            );
        }
    }

    /// A glob is anchored, so it selects what it looks like it selects and
    /// nothing that merely contains it.
    #[test]
    fn a_glob_matches_by_prefix_and_not_by_substring() {
        let ProcessSelector::Regex(re) = ProcessSelector::parse("zeus-*").unwrap() else {
            panic!("a pattern with `*` is compiled to a regex");
        };
        assert!(re.is_match("zeus-auth"));
        assert!(re.is_match("zeus-create"));
        assert!(!re.is_match("my-zeus-auth"), "anchored: no substring match");
        assert!(!re.is_match("reactmap"));
    }

    /// Every metacharacter the `is_glob` gate names has to actually work,
    /// or the gate is claiming support it does not have.
    #[test]
    fn each_glob_metacharacter_compiles_and_matches() {
        let cases = [
            ("*api*", "my-api-thing", "web"),
            ("zeus-?", "zeus-1", "zeus-auth"),
            ("zeus-[ab]*", "zeus-auth", "zeus-create"),
            ("{web,api}", "api", "worker"),
        ];
        for (pattern, hit, miss) in cases {
            let ProcessSelector::Regex(re) = ProcessSelector::parse(pattern).unwrap() else {
                panic!("{pattern} must compile to a regex");
            };
            assert!(re.is_match(hit), "{pattern} must match {hit}");
            assert!(!re.is_match(miss), "{pattern} must not match {miss}");
        }
    }

    /// `all`, `fold:` and `/regex/` are decided before the glob gate, so a
    /// name that happens to carry a metacharacter cannot shadow them.
    #[test]
    fn the_earlier_forms_are_not_shadowed_by_the_glob_gate() {
        assert!(matches!(
            ProcessSelector::parse("all").unwrap(),
            ProcessSelector::All
        ));
        let fold = ProcessSelector::parse("fold:back*end").unwrap();
        assert!(
            matches!(&fold, ProcessSelector::Fold(name) if name == "back*end"),
            "a fold name may contain a metacharacter and is still a fold, got {fold:?}"
        );
        let ProcessSelector::Regex(re) = ProcessSelector::parse("/^zeus-/").unwrap() else {
            panic!("an explicit regex stays a regex");
        };
        assert!(re.is_match("zeus-auth"));
    }

    /// A glob `globset` refuses is reported as such rather than silently
    /// becoming a name that can never match.
    #[test]
    fn an_unparseable_glob_is_refused() {
        let err = ProcessSelector::parse("zeus-[").expect_err("an unclosed class is not a glob");
        assert!(
            matches!(err, SelectorError::BadGlob(_)),
            "expected BadGlob, got {err:?}"
        );
        assert!(err.to_string().contains("glob"), "{err}");
    }

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
        assert!(by_name.matches("web", 0, None, None));
        assert!(!by_name.matches("worker", 0, None, None));

        let by_regex = ProcessSelector::parse("/^w/").unwrap();
        assert!(by_regex.matches("worker", 9, None, None));
        assert!(!by_regex.matches("api", 9, None, None));

        let by_fold = ProcessSelector::parse("fold:backend").unwrap();
        assert!(by_fold.matches("anything", 0, Some("backend"), None));
        assert!(!by_fold.matches("anything", 0, None, None));

        assert!(
            ProcessSelector::parse("all")
                .unwrap()
                .matches("x", 42, None, None)
        );
        assert!(
            ProcessSelector::parse("42")
                .unwrap()
                .matches("x", 42, None, None)
        );
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
        assert!(sel.matches("web", 1, None, None));
        assert_eq!(
            SelectorSpec::from(&sel),
            SelectorSpec::Regex("^w".to_string())
        );
        for spec in [
            SelectorSpec::All,
            SelectorSpec::Id(3),
            SelectorSpec::Name("web".to_string()),
            SelectorSpec::Fold("backend".to_string()),
            SelectorSpec::Instance {
                name: "web".to_string(),
                slot: 2,
            },
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
    fn an_instance_form_parses_and_matches_only_its_slot() {
        let sel = ProcessSelector::parse("web:2").expect("parses");
        assert!(matches!(
            &sel,
            ProcessSelector::Instance { name, slot } if name == "web" && *slot == 2
        ));
        assert!(sel.matches("web", 7, None, Some(2)));
        assert!(!sel.matches("web", 7, None, Some(1)));
        assert!(!sel.matches("api", 7, None, Some(2)));
        assert!(
            !sel.matches("web", 7, None, None),
            "an older daemon's row carries no slot, so it cannot be the one asked for"
        );
    }

    #[test]
    fn an_instance_selector_names_one_entry_so_it_is_exact() {
        // The dog rule: an operator who named it reaches it, a wildcard does not.
        assert!(
            ProcessSelector::parse("metrics:0")
                .expect("parses")
                .is_exact()
        );
    }

    #[test]
    fn the_colon_forms_do_not_shadow_each_other() {
        assert!(matches!(
            ProcessSelector::parse("fold:web").expect("parses"),
            ProcessSelector::Fold(_)
        ));
        assert!(matches!(
            ProcessSelector::parse("web:2").expect("parses"),
            ProcessSelector::Instance { .. }
        ));
        // A trailing segment that is not a number is not a slot. Names cannot
        // hold a colon any more, so this is a name that will simply match nothing.
        assert!(matches!(
            ProcessSelector::parse("web:two").expect("parses"),
            ProcessSelector::Name(_)
        ));
        // A glob is still a glob: the glob test runs first.
        assert!(matches!(
            ProcessSelector::parse("web*:2").expect("parses"),
            ProcessSelector::Regex(_)
        ));
        // An id is still an id.
        assert!(matches!(
            ProcessSelector::parse("11").expect("parses"),
            ProcessSelector::Id(11)
        ));
    }

    #[test]
    fn an_instance_selector_round_trips_through_the_wire_form() {
        let sel = ProcessSelector::parse("web:2").expect("parses");
        let spec = crate::protocol::SelectorSpec::from(&sel);
        assert_eq!(
            spec,
            crate::protocol::SelectorSpec::Instance {
                name: "web".to_string(),
                slot: 2
            }
        );
        let back = ProcessSelector::try_from(spec).expect("converts back");
        assert!(matches!(back, ProcessSelector::Instance { .. }));
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
