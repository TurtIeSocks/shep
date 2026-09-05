//! [`Role`] bound to `anstyle`, for the box-drawn table.
//!
//! A second, independent binding of the roles `lookout/theme.rs` binds to
//! `ratatui`: the two style types come from different crates, and `mod
//! lookout` is `#[cfg(unix)]` while this must compile everywhere `output/`
//! does. Colour numbers are copied from `theme.rs` and pinned against it by
//! that module's tests. Faces and the status-to-role mapping live in
//! `vocabulary.rs`, never here.

use anstyle::{Ansi256Color, AnsiColor, Color, Style};

use crate::vocabulary::Role;

/// One role's colour, at the depth `deep` selects.
///
/// `deep` is resolved at the seam (`style::Presentation::new`) and threaded
/// down as `Presentation::deep_colour`, never read from the environment
/// here. The 256-colour indices are `lookout/theme.rs::Palette::detect`'s
/// own, each the nearest xterm-256 neighbour of the design language's hex;
/// the 16-colour fallback uses the same four named colours.
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

    /// No "off" case: `NO_COLOR` is vetoed upstream, in
    /// `Presentation::colour`.
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

    /// Checked as "no other role resolves bark's colour": this module has no
    /// `ProcStatus` to switch on.
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
