//! One sanitiser for every string an untrusted host can put in front of an
//! operator — [`sanitise`].
//!
//! ## Why this is its own module
//!
//! This code used to live inside `crate::dog_index`, next to the only
//! caller it then had, and that module's doc still explains *why* a string
//! out of the community index is hostile input. Review found the half it
//! did not cover: `dog_index` sanitised every string in a response **body**
//! and no string in a response **header**, while the whole premise of the
//! feature is that the host serving it is untrusted. A hostile `Location:`
//! on a 3xx reached an operator's terminal raw — screen cleared, window
//! title rewritten — because [`crate::fetch::FetchError::Redirect`]
//! captured the header verbatim and `emit_error`'s table arm is a bare
//! `writeln!`.
//!
//! Closing that means [`crate::fetch`] — the transport, the layer *below*
//! `dog_index` — needs this function too. A module cycle
//! (`fetch` -> `dog_index` -> `fetch`) is the wrong way to get it, so the
//! sanitiser moved down here to a leaf that depends on nothing and that
//! both layers can reach.
//!
//! The property `dog_index`'s own doc claims for itself — that every
//! security-relevant line sits in one file a reviewer can hold in their
//! head — is not lost by the move. It is now two files, and this one holds
//! nothing but the sanitiser.
//!
//! ## The rule
//!
//! Strip every character that is invisible, that moves the cursor, or that
//! reorders what follows it, then collapse the whitespace that stripping
//! leaves behind. **Non-ASCII prose is not the threat and survives
//! untouched**: a German or Japanese description is ordinary text, and a
//! sanitiser that strips it is a broken sanitiser.
//!
//! Stripping rather than escaping. Rendering `^[[2J` literally is arguably
//! more honest, but strip is simpler to get right and nothing anybody wants
//! to read is lost. The cost is that the printable tail of a sequence
//! survives as inert text: `\u{1b}[2J` prints as `[2J`. That is a broken
//! sequence, not a working one.
//!
//! **Not to be confused with [`crate::output::width::sanitize_cell`]**,
//! which the table renderer runs over every cell. That one deliberately
//! *keeps* a well-formed CSI sequence, because shep's own colouring is
//! made of them — so it is a layout guard, not a defence against a string
//! somebody else wrote. This one is the defence, and it runs first.

/// Strips everything that could drive a terminal out of `field`, returning
/// the cleaned text and whether anything was removed.
///
/// See this module's own doc for the rule, and [`is_unprintable`] for the
/// exact set.
///
/// **A string with nothing to strip is returned byte for byte**, and
/// reports `false`. That matters twice over: non-ASCII prose is ordinary
/// text and must survive untouched, and the reported flag drives an
/// operator-facing count that has to mean "this entry carried control
/// characters" rather than "this entry had two spaces in a row".
pub fn sanitise(field: &str) -> (String, bool) {
    if !field.chars().any(is_unprintable) {
        return (field.to_owned(), false);
    }
    let stripped: String = field
        .chars()
        .filter_map(|ch| {
            if !is_unprintable(ch) {
                Some(ch)
            } else if is_line_or_space_like(ch) {
                // A line break was separating two words; a plain deletion
                // would join them.
                Some(' ')
            } else {
                None
            }
        })
        .collect();
    let cleaned = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    (cleaned, true)
}

/// Whether `ch` must never reach a terminal.
///
/// Two groups, and it is worth naming why each is here:
///
/// - **`char::is_control`**, which is the Unicode `Cc` category: `U+0000`
///   through `U+001F` and `U+007F` through `U+009F`. That covers `\u{1b}`
///   (the escape that opens `[2J`, `]0;` and every colour sequence),
///   `\r`, `\n`, `\t`, `\u{0}`, `\u{7}` — and `\u{9b}`, the
///   single-character CSI introducer, which is *not* `\u{1b}` and is easy
///   to forget precisely because it does not look like an escape.
/// - **Invisible and reordering format characters**, which are not control
///   characters and so survive the first test: zero-width spaces and
///   joiners, the bidi embeddings and overrides (`U+202E` can make
///   `exe.gnp` read as `png.exe`), the bidi isolates, the word joiner and
///   the invisible maths operators, the BOM, and the interlinear annotation
///   marks. `U+2028`/`U+2029` are in the list as line and paragraph
///   separators: newlines under another name.
///
/// Everything else survives, and that is the point. Accented Latin, kana,
/// Han, emoji and combining marks are ordinary prose in an ordinary
/// description, and a sanitiser that eats them is a broken sanitiser.
///
fn is_unprintable(ch: char) -> bool {
    ch.is_control()
        || matches!(ch,
            '\u{200b}'..='\u{200f}'   // zero-width space/non-joiner/joiner, LRM, RLM
            | '\u{2028}' | '\u{2029}' // line and paragraph separators
            | '\u{202a}'..='\u{202e}' // bidi embeddings and overrides
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{feff}'              // byte order mark / zero-width no-break space
            | '\u{fff9}'..='\u{fffb}' // interlinear annotation
        )
}
/// Whether a character [`is_unprintable`] rejected was separating words, and
/// so should leave a space behind rather than vanish.
fn is_line_or_space_like(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_escape_class_is_stripped() {
        // Each of these reaches a terminal if it survives. shep emits colour
        // itself, so a reader cannot tell an entry's bytes from shep's own.
        let hostile = [
            ("\u{1b}[2J", "clears the screen"),
            ("\u{1b}]0;pwned\u{7}", "rewrites the window title"),
            ("\u{1b}[31mred", "forges shep's own colour"),
            ("before\rafter", "overwrites the line with a bare CR"),
            ("a\u{0}b", "a nul byte"),
            ("tab\there", "a raw tab"),
            ("line\nbreak", "escapes the row"),
        ];
        for (input, why) in hostile {
            let (clean, changed) = sanitise(input);
            assert!(changed, "{why}: should have been reported as sanitised");
            assert!(
                !clean.contains('\u{1b}'),
                "{why}: escape survived in {clean:?}"
            );
            for ch in clean.chars() {
                assert!(
                    !ch.is_control(),
                    "{why}: control char survived in {clean:?}"
                );
            }
        }
    }

    #[test]
    fn ordinary_text_is_left_exactly_alone() {
        let (clean, changed) = sanitise("Rotates grown log files. MIT OR Apache-2.0.");
        assert_eq!(clean, "Rotates grown log files. MIT OR Apache-2.0.");
        assert!(!changed);
    }

    #[test]
    fn non_ascii_prose_survives_because_it_is_not_the_threat() {
        let (clean, changed) = sanitise("rotiert Protokolldateien");
        assert_eq!(clean, "rotiert Protokolldateien");
        assert!(!changed);
    }

    /// fails if a trailing escape survives. A lone `\u{1b}` at the very end
    /// of a field opens a sequence that the NEXT thing printed completes,
    /// which is whatever shep itself writes after the cell. It is the one
    /// escape that does nothing on its own and everything in context.
    #[test]
    fn a_lone_escape_at_the_end_of_a_string_is_stripped() {
        let (clean, changed) = sanitise("tail\u{1b}");
        assert_eq!(clean, "tail");
        assert!(changed);
    }

    /// fails if `\u{9b}` survives. It is the single-character CSI
    /// introducer: `\u{9b}2J` does what `\u{1b}[2J` does, in one character
    /// that is not `\u{1b}` and does not look like an escape. Covered here
    /// because it lives in the C1 block, which `char::is_control` includes
    /// and a hand-rolled `ch == '\u{1b}' || ch < ' '` check would not.
    #[test]
    fn the_single_character_csi_introducer_is_stripped() {
        let (clean, changed) = sanitise("clean\u{9b}2Jhere");
        assert!(changed);
        assert!(!clean.contains('\u{9b}'), "C1 CSI survived in {clean:?}");
    }

    /// fails if any invisible or reordering character survives. None of
    /// these is a control character, so `char::is_control` alone misses
    /// every one. `\u{202e}` is the interesting one: it reverses what
    /// follows, so a package named `shep-exe.gnp` renders as
    /// `shep-png.exe`, and the two entries are indistinguishable in a
    /// table.
    #[test]
    fn invisible_and_reordering_characters_are_stripped() {
        let hostile = [
            ('\u{202e}', "right-to-left override"),
            ('\u{202d}', "left-to-right override"),
            ('\u{2066}', "left-to-right isolate"),
            ('\u{200d}', "zero width joiner"),
            ('\u{200b}', "zero width space"),
            ('\u{2060}', "word joiner"),
            ('\u{feff}', "byte order mark"),
            ('\u{2028}', "line separator"),
        ];
        for (ch, why) in hostile {
            let (clean, changed) = sanitise(&format!("safe{ch}text"));
            assert!(changed, "{why}: should have been reported as sanitised");
            assert!(!clean.contains(ch), "{why}: survived in {clean:?}");
        }
    }

    /// fails if a stripped line break welds two words together. `line` and
    /// `break` are separate words in the source, and `linebreak` is a
    /// different string from either.
    #[test]
    fn a_stripped_line_break_leaves_a_space_behind() {
        let (clean, changed) = sanitise("line\nbreak");
        assert_eq!(clean, "line break");
        assert!(changed);
    }
}
