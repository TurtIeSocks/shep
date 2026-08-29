//! The `{{instance}}` grammar for Flockfile values.
//!
//! Two tokens, `{{instance}}` and `{{name}}`, in env values, args, and the
//! two log-path fields. Anything else between doubled braces is refused by
//! name at config time, so a typo dies at `shep start` rather than reaching
//! a child process as a literal string.
//!
//! # Why doubled braces
//!
//! Single braces are ordinary content in the values this runs over: JSON
//! blobs, regex quantifiers such as `{2,3}`, and Go or Helm templates passed
//! through as args. Under a single-brace grammar with an unknown token
//! refused, `LOG_FORMAT = '{"ts":"%t"}'` would stop a working Flockfile from
//! starting. Doubled braces almost never appear by accident.
//!
//! # Escaping
//!
//! `{{{{` is a literal `{{` and `}}}}` is a literal `}}`, which is
//! `format!`'s own doubling rule one level up. A lone `}}` is ordinary text,
//! deliberately: `{"a":{"b":1}}` ends in one and must survive.

use core::fmt;

/// The tokens this grammar knows, in the order an error lists them.
const TOKENS: &[&str] = &["instance", "name"];

/// A value that is not a valid template.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
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

/// Walks `value`, calling `on_token` for each token and `on_literal` for each
/// run of ordinary text.
///
/// One walker so [`validate`] and [`render`] can never disagree about what a
/// token is.
fn walk(
    value: &str,
    mut on_literal: impl FnMut(&str),
    mut on_token: impl FnMut(&str) -> Result<(), TemplateError>,
) -> Result<(), TemplateError> {
    let bytes = value.as_bytes();
    let mut at = 0;
    let mut literal_from = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"{{{{") {
            on_literal(&value[literal_from..at]);
            on_literal("{{");
            at += 4;
            literal_from = at;
        } else if bytes[at..].starts_with(b"}}}}") {
            on_literal(&value[literal_from..at]);
            on_literal("}}");
            at += 4;
            literal_from = at;
        } else if bytes[at..].starts_with(b"{{") {
            on_literal(&value[literal_from..at]);
            let rest = &value[at + 2..];
            let Some(end) = rest.find("}}") else {
                return Err(TemplateError::Unclosed);
            };
            on_token(&rest[..end])?;
            at += 2 + end + 2;
            literal_from = at;
        } else {
            at += 1;
        }
    }
    on_literal(&value[literal_from..]);
    Ok(())
}

/// Checks that every `{{...}}` in `value` names a token this grammar defines.
///
/// # Errors
///
/// - [`TemplateError::UnknownToken`]: a token this grammar does not define.
/// - [`TemplateError::Unclosed`]: a `{{` with no closing `}}`.
pub fn validate(value: &str) -> Result<(), TemplateError> {
    walk(
        value,
        |_| {},
        |token| {
            if TOKENS.contains(&token) {
                Ok(())
            } else {
                Err(TemplateError::UnknownToken {
                    token: token.to_string(),
                })
            }
        },
    )
}

/// Substitutes the tokens in `value`.
///
/// Call [`validate`] first: an unknown token here renders as nothing, because
/// `normalize` is the seam that refuses one and a value reaching this
/// function has already passed it.
#[must_use]
pub fn render(value: &str, name: &str, instance: u32) -> String {
    let out = core::cell::RefCell::new(String::with_capacity(value.len()));
    let slot = instance.to_string();
    let _ = walk(
        value,
        |literal| out.borrow_mut().push_str(literal),
        |token| {
            match token {
                "instance" => out.borrow_mut().push_str(&slot),
                "name" => out.borrow_mut().push_str(name),
                _ => {}
            }
            Ok(())
        },
    );
    out.into_inner()
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
