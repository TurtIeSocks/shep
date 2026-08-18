//! How wide a string looks, as opposed to how long it is.

/// Columns `s` occupies once ANSI escapes are discounted.
///
/// A styled cell is `\x1b[32m(o.o)\x1b[0m`: 14 bytes, 5 columns. Padding it
/// by `len()` or even by `chars().count()` pushes every border after it to
/// the right, and the table looks broken in a way that is hard to attribute.
/// Three hand-drawn mockups during this feature's design made exactly this
/// mistake.
///
/// Counts characters rather than grapheme clusters or east-asian width.
/// That is a deliberate floor, not an oversight: shep names are operator-
/// chosen identifiers, the alternative is a `unicode-width` dependency for
/// a case nobody has hit, and the property test in `table.rs` will catch it
/// the moment someone does.
///
/// Control characters (`char::is_control`, which includes `\n` and `\t`)
/// measure as zero width. Neither occupies a column: a newline starts a new
/// line instead of advancing one, and a tab expands to a variable number
/// nothing here can predict. Zero is the honest answer to "how wide is
/// this", not "safe to print" -- a control character can still split a
/// table row in two or blow out a terminal's tab stops, and stripping or
/// escaping it before it reaches a cell is the box-drawn renderer's job in
/// Task 4, not this function's. Measuring and sanitising are different
/// problems; this function only does the first.
///
/// Not called outside this module's own tests yet: the caller is Task 4,
/// the box-drawn table that pads a cell by this instead of by `len()`.
/// `#[allow(dead_code)]` says so explicitly rather than inventing a call
/// site nothing needs yet.
#[allow(dead_code)]
#[must_use]
pub(crate) fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // `[` (0x5b) is itself inside @..~, so it must be consumed as
            // the CSI introducer before scanning for the real final byte --
            // checking the range starting there mistakes `[` for the
            // sequence's own end and leaks every parameter byte after it
            // (`32m`) through as visible width. Anything after ESC that
            // isn't `[` is a two-character sequence instead; both forms are
            // zero-width.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            width += 1;
        }
    }
    width
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

    /// Non-ASCII names are real: a table that miscounts them misaligns for
    /// the people least able to work around it.
    #[test]
    fn non_ascii_text_counts_characters() {
        assert_eq!(visible_width("café"), 4);
        assert_eq!(visible_width("日本"), 2, "counted as chars, not bytes");
    }

    /// A `\t` occupies no fixed number of columns -- expansion is a
    /// terminal's decision, not this function's -- so it contributes zero
    /// rather than the one `chars().count()` would give it.
    #[test]
    fn an_embedded_tab_contributes_no_width() {
        assert_eq!(visible_width("web\tworker"), 9);
    }

    /// A `\n` starts a new line instead of advancing one, so it is not a
    /// column either. `normalize()` (shep-core) rejects only `/`, `\`, `.`
    /// and `..` in an app name, so a name carrying an embedded newline
    /// reaches this function today -- this is the case the reviewer flagged
    /// as reachable rather than theoretical.
    #[test]
    fn an_embedded_newline_contributes_no_width() {
        assert_eq!(visible_width("web\nworker"), 9);
    }
}
