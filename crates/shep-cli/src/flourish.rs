//! Sheep for the moments with nothing else to look at.
//!
//! Three places only: an empty flock, a flock entirely stopped, and `shep
//! muster`. Never on an error and never after a destructive verb --
//! `docs/terminology.md`'s rule is that the theme never costs clarity, and a
//! sheep beside `error[not_found]` makes a failure harder to read and reads
//! as flippant to someone debugging at 2am.
//!
//! Uncoloured, deliberately. Every face in one call here shares the same
//! status, so `output::paint::style_for`'s colour would repeat information
//! the words beside it already give -- unlike the STATUS column, where one
//! table mixes several statuses and colour is the only thing separating them
//! at a glance. `welcome.rs`, the one other place shep draws ASCII, makes the
//! same call for the same reason. Threading `crate::style::Presentation`
//! through here to answer a question with one static answer per call would
//! also mean widening the three signatures below, which this module's own
//! tests pin as taking a bare count and nothing else.

use shep_core::status::ProcStatus;

use crate::vocabulary::face;

/// The most sheep any flourish draws, so `shep muster` over forty processes
/// does not paint a field.
const MOST: usize = 5;

/// The gap between two faces on the same row.
///
/// Shoulder to shoulder (no gap at all) reads as noise rather than as
/// several sheep -- correction 1 caught this by rendering `(-.-)(-.-)(-.-)`
/// and finding it illegible. `welcome.rs`'s own art separates its three
/// sheep by three spaces; this matches it rather than inventing a second
/// convention for the same picture.
const GAP: &str = "   ";

/// A row of `count` faces (at least one, capped at [`MOST`]), indented four
/// columns to match `welcome.rs`'s own left margin.
fn row(status: ProcStatus, count: usize) -> String {
    let face = face(status);
    let n = count.clamp(1, MOST);
    let mut line = String::from("    ");
    for i in 0..n {
        if i > 0 {
            line.push_str(GAP);
        }
        line.push_str(face);
    }
    line
}

/// Nothing registered yet: `shep flock` with an empty roll.
///
/// Names the way out (`shep start`) because this state exists at the exact
/// moment an operator is asking "what now" -- a face with nothing beside it
/// would answer a question nobody asked.
pub(crate) fn empty_flock() -> String {
    format!(
        "\n{}\n    no sheep in the flock yet\n    `shep start <script>` adds one\n\n",
        row(ProcStatus::Stopped, 1)
    )
}

/// Registered, every one of them at rest: `shep flock` where `count` sheep
/// are all `Stopped`.
///
/// `count` is the caller's own count of sheep (dogs excluded, and
/// `Stopping` -- a shutdown in progress, not a flock at rest -- excluded
/// too); see `commands::query::sheep_flourish` for exactly what qualifies.
pub(crate) fn all_asleep(count: usize) -> String {
    format!(
        "\n{}\n    {count} in the flock, all asleep\n    `shep start <name>` wakes one\n\n",
        row(ProcStatus::Stopped, count)
    )
}

/// The flock is back: `shep muster` restored `count` sheep.
pub(crate) fn mustered(count: usize) -> String {
    format!(
        "\n{}\n    {count} back on their feet\n",
        row(ProcStatus::Online, count)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capped, so `shep muster` over forty processes does not paint a field.
    #[test]
    fn a_flourish_never_shows_more_than_five_sheep() {
        for n in [1, 2, 5, 6, 40, 400] {
            let art = mustered(n);
            let sheep = art.matches("(o.o)").count();
            assert!(sheep <= 5, "{n} sheep rendered {sheep} faces:\n{art}");
            assert!(sheep >= 1, "at least one, even for {n}:\n{art}");
        }
    }

    /// The empty state exists to answer "what now", so it must say.
    #[test]
    fn the_empty_flock_names_the_next_command() {
        let art = empty_flock();
        assert!(art.contains("shep start"), "{art}");
    }

    /// No em dashes in copy a user reads.
    #[test]
    fn the_flourishes_carry_no_em_dashes() {
        for art in [empty_flock(), all_asleep(3), mustered(2)] {
            assert!(!art.contains('\u{2014}'), "em dash in {art:?}");
            assert!(!art.contains('\u{2013}'), "en dash in {art:?}");
        }
    }

    /// Every line inside 80 columns, like the welcome.
    #[test]
    fn the_flourishes_fit_an_eighty_column_terminal() {
        for art in [empty_flock(), all_asleep(5), mustered(5)] {
            for line in art.lines() {
                assert!(line.chars().count() <= 80, "line too wide: {line:?}");
            }
        }
    }
}
