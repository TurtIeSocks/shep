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
/// ## The second group grew, and here is the tell that it had to
///
/// The list below started with the characters an author thinks of first,
/// and review found twelve survivors of exactly the same classes. The
/// clearest was `U+061C` ARABIC LETTER MARK: a bidi control indistinguishable
/// in kind from `U+200E`/`U+200F`, which were already stripped. An
/// inconsistency inside one class is the tell that the class was
/// enumerated from memory rather than from its definition, so each range
/// below now names a *class* and takes all of it:
///
/// - **Invisible by design.** `U+00AD` soft hyphen, `U+034F` combining
///   grapheme joiner, the Mongolian free variation selectors and vowel
///   separator (`U+180B`..`U+180F`), the variation selectors
///   (`U+FE00`..`U+FE0F` and the supplement `U+E0100`..`U+E01EF`), and the
///   musical beam/slur/phrase marks (`U+1D173`..`U+1D17A`).
/// - **Blank but not whitespace.** The Hangul fillers `U+115F`, `U+1160`,
///   `U+3164` and `U+FFA0` render as nothing at all, so two entries can be
///   made to look identical while comparing unequal.
/// - **Tags** (`U+E0000`..`U+E007F`). `U+E0041` is an invisible `A`: the
///   block can carry an entire hidden ASCII string through any check that
///   reads what it can see.
/// - **The rest of the `U+2060` block.** `U+2060`..`U+2064` and
///   `U+2066`..`U+2069` were listed with the gap between them left in.
///   `U+206A`..`U+206F` are the deprecated format characters (symmetric
///   swapping, Arabic form shaping, national digit shapes) — same class,
///   same invisibility — so the range is now the contiguous
///   `U+2060`..`U+206F` rather than two halves and a reason to wonder about
///   the middle.
///
/// One deliberate cost: stripping `U+FE0F` drops an emoji's *presentation*
/// selector, so `❤️` prints as `❤`. The character survives, the entry
/// survives, and a variation selector is otherwise a free channel for
/// hiding bytes in a string a human is being asked to trust.
fn is_unprintable(ch: char) -> bool {
    ch.is_control()
        || matches!(ch,
            '\u{ad}'                  // soft hyphen
            | '\u{34f}'               // combining grapheme joiner
            | '\u{61c}'               // arabic letter mark (bidi, as U+200E/F)
            | '\u{115f}' | '\u{1160}' // hangul choseong/jungseong fillers
            | '\u{180b}'..='\u{180f}' // mongolian variation selectors, vowel separator
            | '\u{200b}'..='\u{200f}' // zero-width space/non-joiner/joiner, LRM, RLM
            | '\u{2028}' | '\u{2029}' // line and paragraph separators
            | '\u{202a}'..='\u{202e}' // bidi embeddings and overrides
            | '\u{2060}'..='\u{206f}' // word joiner, invisible operators, isolates, deprecated
            | '\u{3164}'              // hangul filler
            | '\u{fe00}'..='\u{fe0f}' // variation selectors
            | '\u{feff}'              // byte order mark / zero-width no-break space
            | '\u{ffa0}'             // halfwidth hangul filler
            | '\u{fff9}'..='\u{fffb}' // interlinear annotation
            | '\u{1d173}'..='\u{1d17a}' // musical beam, slur, phrase marks
            | '\u{e0000}'..='\u{e007f}' // tags: an invisible ascii alphabet
            | '\u{e0100}'..='\u{e01ef}' // variation selectors supplement
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

    /// fails if any of the twelve survivors review found still reaches a
    /// terminal. Each is invisible or reordering, and each is the same
    /// *class* as something the list already stripped -- which is exactly
    /// why they were missed: the class was enumerated from memory rather
    /// than from its definition. `\u{61c}` is the clearest case, a bidi
    /// control sitting beside `\u{200e}`/`\u{200f}`, which were stripped
    /// all along.
    #[test]
    fn the_invisible_classes_are_taken_whole_not_as_a_remembered_subset() {
        let hostile = [
            ('\u{ad}', "soft hyphen"),
            ('\u{34f}', "combining grapheme joiner"),
            ('\u{61c}', "arabic letter mark, a bidi control"),
            ('\u{115f}', "hangul choseong filler, renders blank"),
            ('\u{1160}', "hangul jungseong filler, renders blank"),
            ('\u{180e}', "mongolian vowel separator"),
            ('\u{206b}', "deprecated: activate symmetric swapping"),
            ('\u{3164}', "hangul filler, renders blank"),
            ('\u{fe0f}', "variation selector 16"),
            ('\u{ffa0}', "halfwidth hangul filler, renders blank"),
            ('\u{1d173}', "musical symbol begin beam"),
            ('\u{e0041}', "tag latin capital A: an invisible letter"),
            ('\u{e0101}', "variation selector supplement"),
        ];
        for (ch, why) in hostile {
            let (clean, changed) = sanitise(&format!("safe{ch}text"));
            assert!(changed, "{why}: should have been reported as sanitised");
            assert!(!clean.contains(ch), "{why}: survived in {clean:?}");
        }
    }

    /// fails if the tags block can still smuggle a whole hidden string past
    /// a reader. `U+E0000`..`U+E007F` maps one-to-one onto ASCII, so a name
    /// can carry an invisible second name that no human sees and every
    /// `contains` matches.
    #[test]
    fn a_hidden_ascii_string_written_in_tags_does_not_survive() {
        let hidden: String = "rm -rf"
            .chars()
            .map(|c| char::from_u32(0xe_0000 + c as u32).unwrap())
            .collect();
        let (clean, changed) = sanitise(&format!("Spot{hidden}"));
        assert!(changed);
        assert_eq!(clean, "Spot");
    }

    /// fails if the widened list started eating prose. This is the check
    /// that says the ranges above are ranges of *format* characters and not
    /// of everything unfamiliar: kana, Han, accented Latin, a combining
    /// mark and an emoji base all have to come back byte for byte.
    #[test]
    fn prose_in_other_scripts_still_survives_the_wider_list() {
        for ordinary in [
            "\u{30ed}\u{30b0}\u{3092}\u{30ed}\u{30fc}\u{30c6}\u{30fc}\u{30c8}", // rotates logs, in katakana
            "\u{65e5}\u{8a8c}\u{306e}\u{56de}\u{8ee2}",                         // ditto, in kanji
            "rotiert Protokolldateien",
            "cafe\u{301}", // combining acute: a mark, not a format character
            "\u{2764}",    // an emoji base, unaccompanied by its selector
        ] {
            let (clean, changed) = sanitise(ordinary);
            assert_eq!(clean, ordinary, "prose was altered");
            assert!(!changed, "prose was reported as sanitised");
        }
    }
}
