//! The `{{instance}}` grammar for Flockfile values.
//!
//! Two tokens, `{{instance}}` and `{{name}}`, in env values, args, and the
//! two log-path fields. An unknown token between doubled braces is refused
//! at config time rather than reaching a child process as literal text.
//!
//! Doubled braces avoid collision with single-brace content already in these
//! values: JSON blobs, regex quantifiers, Go or Helm templates passed
//! through as args.
//!
//! `{{{{` and `}}}}` escape to literal `{{` and `}}`. A lone `}}`, as in
//! `{"a":{"b":1}}`, is ordinary text and passes through unchanged.

use core::fmt;

/// The tokens this grammar knows, in the order an error lists them.
const TOKENS: &[&str] = &["instance", "name"];

/// A value that is not a valid template.
///
/// `pub(crate)`: `normalize` is the only caller, and wraps this in its own
/// [`NormalizeError::BadTemplate`](super::normalize::NormalizeError::BadTemplate).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TemplateError {
    /// A `{{...}}` naming something this grammar does not define
    UnknownToken {
        /// The token as the user wrote it, without the braces
        token: String,
    },
    /// A `{{` with no closing `}}`
    Unclosed,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownToken { token } => write!(
                f,
                "`{{{{{token}}}}}` is not a template token: valid tokens are {}",
                TOKENS
                    .iter()
                    .map(|t| format!("`{{{{{t}}}}}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
            Self::Unclosed => f.write_str("a `{{` in this value is never closed by a `}}`"),
        }
    }
}

impl core::error::Error for TemplateError {}

/// One piece of `value` as [`walk`] sees it: ordinary text, or a token name
/// with the braces stripped.
enum Segment<'a> {
    /// A run of ordinary text, copied through unchanged.
    Literal(&'a str),
    /// The name between a `{{` and its `}}`, braces stripped.
    Token(&'a str),
}

/// Walks `value`, calling `on_segment` for each literal run and each token.
///
/// One walker, one closure, so [`validate`] and [`render`] can never
/// disagree about what a token is.
fn walk(
    value: &str,
    mut on_segment: impl FnMut(Segment<'_>) -> Result<(), TemplateError>,
) -> Result<(), TemplateError> {
    let bytes = value.as_bytes();
    let mut at = 0;
    let mut literal_from = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"{{{{") {
            on_segment(Segment::Literal(&value[literal_from..at]))?;
            on_segment(Segment::Literal("{{"))?;
            at += 4;
            literal_from = at;
        } else if bytes[at..].starts_with(b"}}}}") {
            on_segment(Segment::Literal(&value[literal_from..at]))?;
            on_segment(Segment::Literal("}}"))?;
            at += 4;
            literal_from = at;
        } else if bytes[at..].starts_with(b"{{") {
            on_segment(Segment::Literal(&value[literal_from..at]))?;
            let rest = &value[at + 2..];
            let Some(end) = rest.find("}}") else {
                return Err(TemplateError::Unclosed);
            };
            on_segment(Segment::Token(&rest[..end]))?;
            at += 2 + end + 2;
            literal_from = at;
        } else {
            at += 1;
        }
    }
    on_segment(Segment::Literal(&value[literal_from..]))?;
    Ok(())
}

/// Checks that every `{{...}}` in `value` names a token this grammar defines.
///
/// `pub(crate)`: only `normalize` asks this, at config time. [`render`] stays
/// public since shep-daemon's `assemble` runs it on already-validated values.
///
/// # Errors
///
/// - [`TemplateError::UnknownToken`]: a token this grammar does not define.
/// - [`TemplateError::Unclosed`]: a `{{` with no closing `}}`.
pub(crate) fn validate(value: &str) -> Result<(), TemplateError> {
    walk(value, |segment| match segment {
        Segment::Literal(_) => Ok(()),
        Segment::Token(token) if TOKENS.contains(&token) => Ok(()),
        Segment::Token(token) => Err(TemplateError::UnknownToken {
            token: token.to_string(),
        }),
    })
}

/// Substitutes the tokens in `value`.
///
/// Call `validate` first: an unknown token renders as nothing, and an
/// unclosed `{{` renders truncated at that point.
#[must_use]
pub fn render(value: &str, name: &str, instance: u32) -> String {
    let mut out = String::with_capacity(value.len());
    let slot = instance.to_string();
    let _ = walk(value, |segment| {
        match segment {
            Segment::Literal(literal) => out.push_str(literal),
            Segment::Token("instance") => out.push_str(&slot),
            Segment::Token("name") => out.push_str(name),
            Segment::Token(_) => {}
        }
        Ok(())
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_tokens_render() {
        assert_eq!(render("z-{{instance}}", "worker", 3), "z-3");
        assert_eq!(render("{{name}}-{{instance}}d", "worker", 3), "worker-3d");
        assert_eq!(render("91{{instance}}", "worker", 7), "917");
    }

    #[test]
    fn a_value_with_no_token_is_returned_unchanged() {
        // The collision case the doubled braces exist for: single braces are
        // ordinary content and must survive untouched.
        for value in [
            r#"{"ts":"%t","level":"%l"}"#,
            r#"{"a":{"b":1}}"#,
            "^[a-z]{2,3}$",
            "plain",
        ] {
            assert_eq!(render(value, "worker", 1), value, "unchanged: {value}");
            assert!(validate(value).is_ok(), "and accepted: {value}");
        }
    }

    #[test]
    fn an_unknown_token_is_refused_by_name() {
        let err = validate("z-{{instnace}}").unwrap_err();
        assert!(matches!(&err, TemplateError::UnknownToken { token } if token == "instnace"));
        let rendered = err.to_string();
        assert!(rendered.contains("instnace"), "names the typo: {rendered}");
        assert!(
            rendered.contains("instance"),
            "and what is valid: {rendered}"
        );
        assert!(
            !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
            "no em or en dash in copy a user reads: {rendered}"
        );
    }

    #[test]
    fn doubling_escapes_a_literal_token() {
        assert_eq!(render("{{{{instance}}}}", "worker", 3), "{{instance}}");
        assert!(validate("{{{{ .Values.port }}}}").is_ok());
        assert_eq!(
            render("{{{{ .Values.port }}}}", "worker", 3),
            "{{ .Values.port }}",
            "a Helm template passes through for the tool that consumes it"
        );
    }

    #[test]
    fn an_unclosed_token_is_refused() {
        assert!(validate("z-{{instance").is_err());
    }
}
