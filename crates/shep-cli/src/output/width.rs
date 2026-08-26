//! How wide a string looks, as opposed to how long it is.

use unicode_width::UnicodeWidthChar;

/// Columns one `char` occupies on a terminal.
///
/// The single rule both width questions in this crate are built on:
/// [`visible_width`] below sums it over a string with ANSI escapes
/// discounted, and `lookout::view::flock::fit` sums it over a string with
/// nothing discounted. They differ in what they skip, never in what a
/// character is worth, and this function is why.
///
/// A control character measures zero. `UnicodeWidthChar::width` returns
/// `None` for exactly the `Cc` general category -- `U+0000..=U+001F` and
/// `U+007F..=U+009F`, the same set `char::is_control` names -- and zero is
/// the honest answer to "how wide is this": a newline starts a new line
/// instead of advancing one, and a tab expands to a variable number nothing
/// here can predict. Zero is not "safe to print"; [`sanitize_cell`] below is
/// what makes it safe, and the two stay separate functions.
#[must_use]
pub(crate) fn char_columns(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Columns `s` occupies once ANSI escapes are discounted.
///
/// A styled cell is `\x1b[32m(o.o)\x1b[0m`: 14 bytes, 5 columns. Padding it
/// by `len()` or even by `chars().count()` pushes every border after it to
/// the right, and the table looks broken in a way that is hard to attribute.
/// Three hand-drawn mockups during this feature's design made exactly this
/// mistake.
///
/// Measures display columns, via [`char_columns`], not characters. `羊` is
/// one `char` and two columns, so a six-character CJK name needs twice the
/// budget of a six-character ASCII one; counting `char`s gave both six and
/// pushed every border after the wide cell to the left. That floor was
/// deliberate while `unicode-width` would have been a new crate for a case
/// nobody had hit -- it is neither now, since `ratatui-core` already put it
/// in this tree and `lookout` needs the same rule.
///
/// Grapheme clusters are still not what this counts, and that stays a
/// deliberate floor: a combining mark measures zero and so rides along with
/// its base character for free, which is the case that matters. What is not
/// handled is a caller splitting a string *between* a base and its mark,
/// and no caller here does -- both truncating callers stop on a whole
/// `char` boundary and a zero-width mark never trips their budget check.
///
/// Control characters (`char::is_control`, which includes `\n` and `\t`)
/// measure as zero width, which [`char_columns`] explains. Zero is the
/// honest answer to "how wide is this", not "safe to print" -- a control
/// character can still split a table row in two or blow out a terminal's
/// tab stops, which is why
/// [`sanitize_cell`] exists as this function's own sibling below:
/// `table.rs`'s `render_boxed_ex` runs every cell through it before this
/// function ever measures one, so by the time `visible_width` sees a cell,
/// a bare control character has already been escaped or stripped down to
/// plain characters this function counts like any other. Measuring and
/// sanitising stay two different functions -- this one only ever does the
/// first -- but both now run, in that order, on every cell the box-drawn
/// renderer prints.
///
/// Called from non-test code by `table.rs`'s `column_widths` and
/// `boxed_row`, which pad every cell by this instead of by `len()`.
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
        } else {
            width += char_columns(c);
        }
    }
    width
}

/// A cell, with every control character escaped or stripped so it cannot
/// split a table row or hand a terminal a raw byte it never chose to print.
///
/// Whole-branch review item 3: `visible_width`'s own doc named this job and
/// no code ever did it -- `boxed_row` (`table.rs`) pushed a cell straight
/// through, so a name with an embedded newline (`shep-core`'s `normalize()`
/// rejects only `/`, `\`, `.` and `..`, nothing about control characters)
/// split its own row and misaligned every border beneath it.
///
/// A legitimate ANSI escape survives untouched: the same CSI scan
/// [`visible_width`] uses, kept a well-formed sequence (the `\x1b[`
/// introducer through a final byte in `\u{40}..=\u{7e}`) verbatim, because
/// that is [`super::paint::style_for`]'s own colouring and stripping it
/// would un-colour every cell this renderer draws. An escape that never
/// closes -- an operator-chosen string carrying a bare or unterminated
/// `\x1b[` -- is dropped in full: nothing of a sequence this function
/// cannot prove is well-formed reaches the terminal, which is the one case
/// [`visible_width`]'s own zero-width answer could not rule out on its own
/// (a stray `\x1b` still measures as zero, whether or not anything ever
/// closes it).
///
/// `\n`, `\r` and `\t` are escaped to their two-character spellings
/// (`\\n`, `\\r`, `\\t`) rather than dropped silently, so a reader can see
/// something was there without the table breaking -- the same convention
/// `rows::preview_body` already uses for `\n`/`\r` in a trigger reply body,
/// generalised here to every cell this renderer prints and extended to
/// `\t`, which that function never touched. Every other control character
/// (bell, backspace, and the rest `char::is_control` names) is dropped: none
/// carries meaning in a table cell the way a name's embedded newline might,
/// and a match arm per byte would bloat this function for input an operator
/// never intentionally supplies.
///
/// Called once per cell, in `table.rs`'s `render_boxed_ex`, before either
/// its own `column_widths` or `boxed_row` ever sees it -- so the width
/// [`visible_width`] measures and the bytes the cell actually prints always
/// agree. Sanitising twice, once per call site, would have risked exactly
/// that drift if the two ever normalised a cell differently.
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
                // else: an escape that never closes is dropped whole --
                // see this function's own doc for why.
            }
            // else: a bare ESC not followed by `[` is not a CSI sequence at
            // all -- dropped the same as any other control character below.
        } else {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                other if other.is_control() => {} // dropped -- see this function's own doc
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

    /// Non-ASCII names are real: a table that miscounts them misaligns for
    /// the people least able to work around it.
    ///
    /// The two halves are two different mistakes. `café` pins that this is
    /// not a byte count -- 5 bytes, 4 columns. `日本` pins that it is not a
    /// `char` count either: 2 `char`s, 6 bytes, and **4 columns**, which is
    /// the reading a terminal actually draws. An earlier version of this
    /// test asserted `2` here and was the thing keeping the bug pinned.
    #[test]
    fn non_ascii_text_counts_display_columns() {
        assert_eq!("café".len(), 5, "the raw string really is longer");
        assert_eq!(visible_width("café"), 4);
        assert_eq!("日本".chars().count(), 2);
        assert_eq!(visible_width("日本"), 4, "columns, not chars and not bytes");
    }

    /// fails if a combining mark is charged for a column of its own. `é`
    /// spelled as `e` + `U+0301` is two `char`s and one column, and a table
    /// that pads it by `char` count comes out one short. This is the case
    /// [`visible_width`]'s doc gives for why grapheme clustering is not
    /// needed on top of a per-`char` width rule.
    #[test]
    fn a_combining_mark_rides_along_with_its_base_character_for_free() {
        let decomposed = "cafe\u{301}";
        assert_eq!(decomposed.chars().count(), 5);
        assert_eq!(visible_width(decomposed), 4);
        assert_eq!(visible_width(decomposed), visible_width("café"));
    }

    /// fails if an escape's *parameter* bytes start being measured as the
    /// wide characters they are not. Belt and braces on the CSI scan: the
    /// per-`char` rule now returns something other than `1` for some
    /// characters, so a scan that leaked `38;5;166m` would leak a width that
    /// no longer even matches its character count.
    #[test]
    fn a_wide_character_beside_an_escape_is_measured_and_the_escape_is_not() {
        assert_eq!(visible_width("\u{1b}[38;5;166m日本\u{1b}[0m"), 4);
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

    // --- Whole-branch review item 3: sanitize_cell -------------------------

    /// The bug this function exists for: a name carrying a raw newline must
    /// not survive into a cell, because a literal `\n` in the middle of a
    /// `boxed_row` output splits that row across two printed lines and
    /// misaligns every border beneath it.
    #[test]
    fn an_embedded_newline_is_escaped_not_literal() {
        let sanitized = sanitize_cell("web\nworker");
        assert!(!sanitized.contains('\n'), "{sanitized:?}");
        assert_eq!(sanitized, "web\\nworker");
    }

    /// `\r` and `\t` get the same treatment as `\n` -- both are named
    /// alongside it in `visible_width`'s own doc as zero-width but not
    /// "safe to print".
    #[test]
    fn a_carriage_return_and_a_tab_are_also_escaped() {
        assert_eq!(sanitize_cell("a\rb"), "a\\rb");
        assert_eq!(sanitize_cell("a\tb"), "a\\tb");
    }

    /// Every other control character is dropped rather than escaped by
    /// name -- a bell has no two-character spelling this function invents
    /// one for.
    #[test]
    fn other_control_characters_are_dropped() {
        assert_eq!(sanitize_cell("a\u{7}b"), "ab"); // BEL
        assert_eq!(sanitize_cell("a\u{8}b"), "ab"); // backspace
    }

    /// A well-formed ANSI escape -- `output::paint::style_for`'s own
    /// colouring -- survives untouched: sanitizing must not un-colour the
    /// STATUS cell this whole feature exists to colour.
    #[test]
    fn a_well_formed_escape_sequence_survives_untouched() {
        let styled = "\u{1b}[38;5;29m(o.o) online\u{1b}[0m";
        assert_eq!(sanitize_cell(styled), styled);
    }

    /// A bare `\x1b` with no `[` is [`visible_width`]'s own "two-character
    /// sequence" case (that function's own doc), so both characters drop
    /// together here too -- the same convention, not a special case this
    /// function invents. An `\x1b[` that never reaches a final byte is not a
    /// *well-formed* CSI sequence, so it is dropped in full rather than
    /// passed through on the assumption that whatever looks like an escape
    /// must be one of ours. Neither should reach the terminal raw.
    #[test]
    fn an_unterminated_or_bare_escape_is_dropped_whole() {
        assert_eq!(
            sanitize_cell("a\u{1b}bc"),
            "ac",
            "bare ESC and the one character after it, both gone"
        );
        // Parameter bytes only (digits and `;`, both outside the
        // `\u{40}..=\u{7e}` final-byte range) after the introducer -- a real
        // word here would risk one of its own letters accidentally closing
        // the sequence, the same trap `visible_width`'s own CSI scan warns
        // about for `[` itself. With no final byte anywhere in the rest of
        // the string, there is no point to resume printing from, so the
        // introducer and everything after it are gone -- not just the
        // introducer.
        assert_eq!(sanitize_cell("a\u{1b}[3;1"), "a");
    }

    /// The property this function exists to hold end to end: whatever
    /// `sanitize_cell` produces, `visible_width` can measure honestly --
    /// no escaped byte left over for it to miscount, and the escaped
    /// spelling of a control character measured like any other text.
    #[test]
    fn a_sanitized_cells_width_matches_its_escaped_spelling() {
        let sanitized = sanitize_cell("web\nworker");
        assert_eq!(visible_width(&sanitized), sanitized.chars().count());
    }
}
