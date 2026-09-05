//! The design language's semantic colours, mapped onto a terminal.
//!
//! Maps `--meadow` (online, healthy), `--bark` (errored, refused,
//! destructive), `--butter` (attention) and `--ink-3` (muted) onto 16 or
//! 256 terminal colours, per `docs/shep-design/README.md`.
//!
//! `--paper` is never painted: it would fight the operator's own terminal
//! background, so ordinary text stays [`Color::Reset`]. `--barn` is
//! scenery-only and has no analog here.
//!
//! Every coloured cell's text already says the same thing, so `NO_COLOR`
//! costs decoration, never information.

use ratatui::style::{Color, Style};
use std::ffi::OsStr;

use shep_core::status::ProcStatus;

use crate::vocabulary::Reported;

/// The four semantic colours, resolved for one terminal.
///
/// Constructed once at startup by `lookout`, from `Palette::detect`, and
/// carried in `super::app::App`. Never re-derived per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    meadow: Option<Color>,
    bark: Option<Color>,
    butter: Option<Color>,
    ink3: Option<Color>,
}

impl Palette {
    /// Resolves the palette from the environment, taken as arguments rather
    /// than read here so callers can test it without touching `std::env`.
    ///
    /// An empty `NO_COLOR=` counts as unset, the cross-ecosystem convention.
    /// A terminal claiming truecolor, 24-bit or 256-colour support gets the
    /// indexed colours; anything else gets the 16 named ones, since a
    /// shallow terminal fed a 256-colour escape can print it as literal text.
    #[must_use]
    pub fn detect(
        no_color: Option<&OsStr>,
        term: Option<&OsStr>,
        colorterm: Option<&OsStr>,
    ) -> Self {
        if crate::style::no_color_set(no_color) {
            return Self {
                meadow: None,
                bark: None,
                butter: None,
                ink3: None,
            };
        }
        let deep = crate::style::deep_colour_terminal(term, colorterm);

        if deep {
            // xterm-256 indices chosen as the nearest neighbours of the design
            // language's own hexes: 29 #00875f for --meadow #2E8B57, 166
            // #d75f00 for --bark #E0552B, 221 #ffd75f for --butter #F3C44C,
            // 245 #8a8a8a for --ink-3 #7A8C80.
            Self {
                meadow: Some(Color::Indexed(29)),
                bark: Some(Color::Indexed(166)),
                butter: Some(Color::Indexed(221)),
                ink3: Some(Color::Indexed(245)),
            }
        } else {
            Self {
                meadow: Some(Color::Green),
                bark: Some(Color::Red),
                butter: Some(Color::Yellow),
                ink3: Some(Color::DarkGray),
            }
        }
    }

    /// The style for a group row's STATUS cell, where only a bare
    /// `ProcStatus` is available.
    ///
    /// `Errored` is the only status that gets `--bark`; `waiting-restart` is
    /// `--butter`, a state to watch rather than damage that happened.
    /// `Stopping` and `Stopped` are muted.
    #[must_use]
    pub fn status(self, status: ProcStatus) -> Style {
        self.role_style(crate::vocabulary::role_of(status))
    }

    /// The style for one row's STATUS cell, sheep or dog. A silent dog wears
    /// `--butter` here exactly as it does in `shep flock`'s own table.
    #[must_use]
    pub fn reported(self, reported: Reported) -> Style {
        self.role_style(reported.role())
    }

    /// The mapping lives in `crate::vocabulary`, so the CLI's table and this
    /// pane cannot drift.
    fn role_style(self, role: crate::vocabulary::Role) -> Style {
        Self::fg(match role {
            crate::vocabulary::Role::Meadow => self.meadow,
            crate::vocabulary::Role::Butter => self.butter,
            crate::vocabulary::Role::Bark => self.bark,
            crate::vocabulary::Role::Ink3 => self.ink3,
        })
    }

    /// Muted: column headers, the home path in the title, key hints.
    #[must_use]
    pub fn muted(self) -> Style {
        Self::fg(self.ink3)
    }

    /// Damage that has happened: the frozen banner, a failed poll.
    #[must_use]
    pub fn alarm(self) -> Style {
        Self::fg(self.bark)
    }

    /// A refused action: `--bark`'s third permitted use, alongside errored
    /// and destructive.
    #[must_use]
    pub fn refusal(self) -> Style {
        Self::fg(self.bark)
    }

    /// Something to look at that is not damage: reconnecting, a dropped-event
    /// notice.
    #[must_use]
    pub fn attention(self) -> Style {
        Self::fg(self.butter)
    }

    fn fg(colour: Option<Color>) -> Style {
        colour.map_or_else(Style::default, |colour| Style::default().fg(colour))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn no_color_flattens_the_palette_and_an_empty_one_does_not() {
        let off = Palette::detect(Some(OsStr::new("1")), None, None);
        assert_eq!(off.status(ProcStatus::Errored), Style::default());
        assert_eq!(off.muted(), Style::default());

        let empty = Palette::detect(
            Some(OsStr::new("")),
            Some(OsStr::new("xterm-256color")),
            None,
        );
        assert_ne!(empty.status(ProcStatus::Errored), Style::default());
    }

    /// A 16-colour terminal sent an unrecognized escape can print it as
    /// literal text, which is worse than a flatter palette.
    #[test]
    fn an_unknown_terminal_gets_the_sixteen_colour_palette() {
        let sixteen = Palette::detect(None, Some(OsStr::new("vt100")), None);
        assert_eq!(sixteen.status(ProcStatus::Errored).fg, Some(Color::Red));
        assert_eq!(sixteen.status(ProcStatus::Online).fg, Some(Color::Green));

        let deep = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        assert_eq!(
            deep.status(ProcStatus::Errored).fg,
            Some(Color::Indexed(166))
        );

        let truecolor = Palette::detect(
            None,
            Some(OsStr::new("dumb")),
            Some(OsStr::new("truecolor")),
        );
        assert_eq!(
            truecolor.status(ProcStatus::Online).fg,
            Some(Color::Indexed(29))
        );
    }

    /// `waiting-restart` is the live temptation: it is `--butter`, attention,
    /// not damage.
    #[test]
    fn bark_is_reserved_for_errored_and_nothing_else() {
        let p = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let bark = Some(Color::Indexed(166));
        assert_eq!(p.status(ProcStatus::Errored).fg, bark);
        for other in [
            ProcStatus::Online,
            ProcStatus::Starting,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::WaitingRestart,
        ] {
            assert_ne!(
                p.status(other).fg,
                bark,
                "{other} must not be bark-coloured"
            );
        }
        // The two non-status uses the design language does allow.
        assert_eq!(p.alarm().fg, bark);
        assert_eq!(p.refusal().fg, bark);
    }

    #[test]
    fn every_status_is_legible_with_no_colour_at_all() {
        let off = Palette::detect(Some(OsStr::new("1")), None, None);
        let mut seen = std::collections::BTreeSet::new();
        for status in [
            ProcStatus::Online,
            ProcStatus::Starting,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
        ] {
            assert_eq!(off.status(status), Style::default());
            assert!(
                seen.insert(status.to_string()),
                "two statuses share one word"
            );
        }
        assert_eq!(seen.len(), 6);
    }

    /// This binding and `output::paint`'s must resolve every role to the
    /// same colour, at both tiers. Compares the extracted index or name
    /// rather than a shared literal, so a renumbering on one side that
    /// forgets the other still fails this.
    #[test]
    fn the_anstyle_binding_agrees_with_this_ones_colours() {
        use crate::vocabulary::Role;

        let deep = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let shallow = Palette::detect(None, Some(OsStr::new("vt100")), None);

        for (status, role) in [
            (ProcStatus::Online, Role::Meadow),
            (ProcStatus::WaitingRestart, Role::Butter),
            (ProcStatus::Stopped, Role::Ink3),
            (ProcStatus::Errored, Role::Bark),
        ] {
            let ratatui_deep = deep
                .status(status)
                .fg
                .expect("the deep tier always sets a foreground");
            let anstyle_deep = crate::output::paint::style_for(role, true)
                .get_fg_color()
                .expect("style_for always sets a foreground");
            assert_eq!(
                ansi256_index(ratatui_deep),
                ansi256_index_anstyle(anstyle_deep),
                "{role:?} disagrees at the 256-colour tier"
            );

            let ratatui_shallow = shallow
                .status(status)
                .fg
                .expect("the shallow tier always sets a foreground");
            let anstyle_shallow = crate::output::paint::style_for(role, false)
                .get_fg_color()
                .expect("style_for always sets a foreground");
            assert_eq!(
                named_colour(ratatui_shallow),
                named_colour_anstyle(anstyle_shallow),
                "{role:?} disagrees at the 16-colour tier"
            );
        }
    }

    fn ansi256_index(c: Color) -> u8 {
        match c {
            Color::Indexed(i) => i,
            other => panic!("expected an indexed ratatui colour, got {other:?}"),
        }
    }

    fn ansi256_index_anstyle(c: anstyle::Color) -> u8 {
        match c {
            anstyle::Color::Ansi256(indexed) => indexed.0,
            other => panic!("expected an Ansi256 anstyle colour, got {other:?}"),
        }
    }

    /// A colour-family name independent of either crate's own enum spelling,
    /// so `ratatui::style::Color::DarkGray` and
    /// `anstyle::AnsiColor::BrightBlack` compare equal.
    fn named_colour(c: Color) -> &'static str {
        match c {
            Color::Green => "green",
            Color::Red => "red",
            Color::Yellow => "yellow",
            Color::DarkGray => "bright-black",
            other => panic!("no name recorded for {other:?}"),
        }
    }

    fn named_colour_anstyle(c: anstyle::Color) -> &'static str {
        match c {
            anstyle::Color::Ansi(anstyle::AnsiColor::Green) => "green",
            anstyle::Color::Ansi(anstyle::AnsiColor::Red) => "red",
            anstyle::Color::Ansi(anstyle::AnsiColor::Yellow) => "yellow",
            anstyle::Color::Ansi(anstyle::AnsiColor::BrightBlack) => "bright-black",
            other => panic!("no name recorded for {other:?}"),
        }
    }
}
