//! [`Role`] bound to `anstyle`, the box-drawn table's own colour type --
//! this module is `output/`'s counterpart to `lookout/theme.rs`'s `ratatui`
//! binding of the exact same roles.
//!
//! The two cannot share code: `anstyle::Style` and `ratatui::style::Style`
//! come from different crates, and `mod lookout` is `#[cfg(unix)]` while
//! this module must compile everywhere `output/` does (spec's own "two-table
//! seam" section). So this is a second, independent binding of one shared
//! vocabulary (`crate::vocabulary`) rather than a wrapper over the first.
//!
//! Colour numbers are copied from `theme.rs`, not derived from it, and
//! `lookout::theme`'s own test module pins the two bindings against each
//! other so a change to one that forgets the other fails loudly rather than
//! drifting. Never define a face or a status-to-role mapping here -- both
//! live in `vocabulary.rs`, and a copy in either renderer is a review
//! defect (the module doc there says so first).

use anstyle::{Ansi256Color, AnsiColor, Color, Style};

use crate::vocabulary::Role;

/// One role's colour, at the depth `deep` selects.
///
/// `deep` is resolved once, at the seam (`style::Presentation::new`, from
/// `$TERM`/`$COLORTERM`) and threaded down as `Presentation::deep_colour` --
/// never read from the environment here, the same rule `Streams::style`'s
/// own doc states for every presentation input in this crate.
///
/// The indices are `lookout/theme.rs::Palette::detect`'s own: 29 `#00875f`
/// for meadow, 166 `#d75f00` for bark, 221 `#ffd75f` for butter, 245
/// `#8a8a8a` for ink3, each the nearest xterm-256 neighbour of the design
/// language's hex (`theme.rs`'s own comment has the full accounting). The
/// 16-colour fallback uses the same four named colours `theme.rs` does --
/// green, red, yellow, and bright-black, which is `anstyle`'s spelling of
/// what `ratatui` calls `DarkGray` (both are ANSI code 90).
#[must_use]
pub(crate) fn style_for(role: Role, deep: bool) -> Style {
    let colour = match (role, deep) {
        (Role::Meadow, true) => Color::Ansi256(Ansi256Color(29)),
        (Role::Bark, true) => Color::Ansi256(Ansi256Color(166)),
        (Role::Butter, true) => Color::Ansi256(Ansi256Color(221)),
        (Role::Ink3, true) => Color::Ansi256(Ansi256Color(245)),
        (Role::Meadow, false) => Color::Ansi(AnsiColor::Green),
        (Role::Bark, false) => Color::Ansi(AnsiColor::Red),
        (Role::Butter, false) => Color::Ansi(AnsiColor::Yellow),
        (Role::Ink3, false) => Color::Ansi(AnsiColor::BrightBlack),
    };
    Style::new().fg_color(Some(colour))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every role resolves a foreground colour at both tiers -- this module
    /// carries no `NO_COLOR` concept of its own (that veto happens upstream,
    /// in `Presentation::colour`), so there is no "off" case to check here.
    #[test]
    fn every_role_resolves_a_foreground_at_both_tiers() {
        for role in [Role::Meadow, Role::Bark, Role::Butter, Role::Ink3] {
            for deep in [true, false] {
                assert!(
                    style_for(role, deep).get_fg_color().is_some(),
                    "{role:?} at deep={deep} must set a foreground"
                );
            }
        }
    }

    /// `--bark` is reserved for errored (and, elsewhere, refused/
    /// destructive) -- the same rule `theme.rs`'s own
    /// `bark_is_reserved_for_errored_and_nothing_else` pins for the ratatui
    /// binding. Checked here as "no other role resolves bark's colour",
    /// since this module has no `ProcStatus` to switch on directly.
    #[test]
    fn bark_is_the_only_role_painted_bark() {
        for deep in [true, false] {
            let bark = style_for(Role::Bark, deep);
            for other in [Role::Meadow, Role::Butter, Role::Ink3] {
                assert_ne!(
                    style_for(other, deep),
                    bark,
                    "{other:?} at deep={deep} must not share bark's colour"
                );
            }
        }
    }

    /// The four roles resolve to four distinct colours at each tier -- a
    /// face or a colour that collides with another role would make the
    /// column ambiguous at a glance, defeating the whole point of colouring
    /// it.
    #[test]
    fn the_four_roles_are_pairwise_distinct_at_each_tier() {
        for deep in [true, false] {
            let styles = [
                style_for(Role::Meadow, deep),
                style_for(Role::Bark, deep),
                style_for(Role::Butter, deep),
                style_for(Role::Ink3, deep),
            ];
            for (i, a) in styles.iter().enumerate() {
                for (j, b) in styles.iter().enumerate() {
                    if i != j {
                        assert_ne!(
                            a, b,
                            "roles at index {i} and {j} share a colour at deep={deep}"
                        );
                    }
                }
            }
        }
    }
}
