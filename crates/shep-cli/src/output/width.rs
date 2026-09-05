//! How wide a string looks, as opposed to how long it is.

use unicode_width::UnicodeWidthChar;

/// Columns one `char` occupies on a terminal.
///
/// Zero for the `Cc` general category (`char::is_control`): a newline
/// starts a new line instead of advancing one, and a tab's width is a
/// terminal decision. Zero means not measurable, not safe to print: see
/// [`sanitize_cell`] for that.
#[must_use]
pub(crate) fn char_columns(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Columns `s` occupies once ANSI escapes are discounted.
///
/// Sums [`char_columns`], not bytes or `char` count: `羊` is one `char` and
/// two columns, and a combining mark measures zero and rides along with its
/// base character. Assumes a caller never splits a string between a base
/// character and its mark.
///
/// Callers must run [`sanitize_cell`] first: this function only measures,
/// and a raw control character still measures zero without being safe to
/// print.
#[must_use]
pub(crate) fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // `[` falls inside the final-byte range `@..~`, so it must be
            // consumed as the CSI introducer first, or it gets mistaken for
            // the sequence's own end.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            width += char_columns(c);
        }
    }
    width
}

/// A cell, with every control character escaped or stripped so it cannot
/// split a table row or hand a terminal a raw byte it never chose to print.
///
/// A well-formed ANSI escape (the `\x1b[` introducer through a final byte
/// in `\u{40}..=\u{7e}`) survives untouched, since that is
/// [`super::paint::style_for`]'s own colouring. An escape that never closes
/// is dropped whole. `\n`, `\r` and `\t` become their two-character
/// spellings (`\\n`, `\\r`, `\\t`); every other control character is
/// dropped.
#[must_use]
pub(crate) fn sanitize_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                let mut seq = String::from("\u{1b}[");
                let mut closed = false;
                for c in chars.by_ref() {
                    seq.push(c);
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        closed = true;
                        break;
                    }
                }
                if closed {
                    out.push_str(&seq);
                }
            }
            // else: a bare ESC is not a CSI sequence, so it drops like any
            // other control character.
        } else {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                other if other.is_control() => {}
                other => out.push(other),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this module exists for: a styled cell is 14 bytes and 5
    /// columns, and padding it by `len()` pushes every later border right.
    #[test]
    fn a_styled_string_measures_its_visible_width_not_its_bytes() {
        let styled = "\u{1b}[32m(o.o)\u{1b}[0m";
        assert_eq!(styled.len(), 14, "the raw string really is longer");
        assert_eq!(visible_width(styled), 5);
        assert_eq!(visible_width("(o.o)"), 5);
    }

    /// Several escapes in one cell, and one at each end.
    #[test]
    fn every_escape_in_a_string_is_discounted() {
        assert_eq!(visible_width("\u{1b}[1m\u{1b}[32mup\u{1b}[0m"), 2);
        assert_eq!(visible_width("\u{1b}[0m"), 0);
        assert_eq!(visible_width(""), 0);
    }

    /// `café` pins this is not a byte count: 5 bytes, 4 columns. `日本` pins
    /// it is not a `char` count either: 2 `char`s, 6 bytes, 4 columns.
    #[test]
    fn non_ascii_text_counts_display_columns() {
        assert_eq!("café".len(), 5, "the raw string really is longer");
        assert_eq!(visible_width("café"), 4);
        assert_eq!("日本".chars().count(), 2);
        assert_eq!(visible_width("日本"), 4, "columns, not chars and not bytes");
    }

    /// fails if a combining mark is charged for a column of its own. `é`
    /// spelled as `e` + `U+0301` is two `char`s and one column.
    #[test]
    fn a_combining_mark_rides_along_with_its_base_character_for_free() {
        let decomposed = "cafe\u{301}";
        assert_eq!(decomposed.chars().count(), 5);
        assert_eq!(visible_width(decomposed), 4);
        assert_eq!(visible_width(decomposed), visible_width("café"));
    }

    /// fails if an escape's parameter bytes get measured as characters:
    /// `38;5;166m` would leak width if the CSI scan let it through.
    #[test]
    fn a_wide_character_beside_an_escape_is_measured_and_the_escape_is_not() {
        assert_eq!(visible_width("\u{1b}[38;5;166m日本\u{1b}[0m"), 4);
    }

    /// A `\t`'s width is a terminal decision, so it contributes zero.
    #[test]
    fn an_embedded_tab_contributes_no_width() {
        assert_eq!(visible_width("web\tworker"), 9);
    }

    /// A `\n` starts a new line rather than advancing one, so it is not a
    /// column either. `shep-core`'s `normalize()` does not reject an
    /// embedded newline in an app name, so this is reachable.
    #[test]
    fn an_embedded_newline_contributes_no_width() {
        assert_eq!(visible_width("web\nworker"), 9);
    }

    // --- sanitize_cell -------------------------------------------------

    #[test]
    fn an_embedded_newline_is_escaped_not_literal() {
        let sanitized = sanitize_cell("web\nworker");
        assert!(!sanitized.contains('\n'), "{sanitized:?}");
        assert_eq!(sanitized, "web\\nworker");
    }

    /// `\r` and `\t` get the same treatment as `\n`: zero-width but not
    /// safe to print.
    #[test]
    fn a_carriage_return_and_a_tab_are_also_escaped() {
        assert_eq!(sanitize_cell("a\rb"), "a\\rb");
        assert_eq!(sanitize_cell("a\tb"), "a\\tb");
    }

    /// Every other control character is dropped rather than escaped: a
    /// bell has no two-character spelling to invent.
    #[test]
    fn other_control_characters_are_dropped() {
        assert_eq!(sanitize_cell("a\u{7}b"), "ab"); // BEL
        assert_eq!(sanitize_cell("a\u{8}b"), "ab"); // backspace
    }

    /// A well-formed ANSI escape survives untouched: sanitizing must not
    /// un-colour a coloured cell.
    #[test]
    fn a_well_formed_escape_sequence_survives_untouched() {
        let styled = "\u{1b}[38;5;29m(o.o) online\u{1b}[0m";
        assert_eq!(sanitize_cell(styled), styled);
    }

    /// A bare `\x1b` with no `[` drops both characters. An `\x1b[` that
    /// never reaches a final byte is not a well-formed CSI sequence, so it
    /// drops in full rather than passing through unverified.
    #[test]
    fn an_unterminated_or_bare_escape_is_dropped_whole() {
        assert_eq!(
            sanitize_cell("a\u{1b}bc"),
            "ac",
            "bare ESC and the one character after it, both gone"
        );
        // Parameter bytes only, no final byte anywhere after the
        // introducer, so the whole sequence drops.
        assert_eq!(sanitize_cell("a\u{1b}[3;1"), "a");
    }

    /// Whatever `sanitize_cell` produces, `visible_width` measures
    /// honestly: no escaped byte left over to miscount.
    #[test]
    fn a_sanitized_cells_width_matches_its_escaped_spelling() {
        let sanitized = sanitize_cell("web\nworker");
        assert_eq!(visible_width(&sanitized), sanitized.chars().count());
    }
}
