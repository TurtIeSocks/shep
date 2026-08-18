//! The design language's semantic colours, mapped onto a terminal.
//!
//! `docs/shep-design/README.md` assigns meaning to four tokens — `--meadow`
//! for online and healthy, `--bark` for errored and refused and destructive
//! and nothing else, `--butter` for attention, `--ink-3` for muted labels and
//! captions — and states one rule this module exists to keep: *errors get a
//! colour, not a face*. A terminal has 16 or 256 colours rather than hex
//! tokens, so this maps rather than quotes.
//!
//! Two things are deliberately NOT mapped. `--paper` is the design language's
//! page background; painting it here would fight the operator's own terminal
//! theme and lose on half of them, so no background is painted at all and
//! ordinary text is [`Color::Reset`]. `--barn` is scenery-only in the design
//! language's own words, and there is no scenery in a dashboard.
//!
//! **Colour is always redundant with text here.** Every coloured cell is a cell
//! whose text already says the same thing: the STATUS column prints `errored`
//! and `--bark` sits on top of that word. That is what makes both downgrades
//! below losses of decoration rather than losses of information.

use ratatui::style::{Color, Style};
use std::ffi::OsStr;

use shep_core::status::ProcStatus;

/// The four semantic colours, resolved for one terminal.
///
/// Constructed once at startup — by `super::mod`'s `lookout`, from
/// `Palette::detect` — and carried in `super::app::App`; never re-derived per
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    meadow: Option<Color>,
    bark: Option<Color>,
    butter: Option<Color>,
    ink3: Option<Color>,
}

impl Palette {
    /// Resolves the palette from the environment, taken as arguments rather
    /// than read here.
    ///
    /// A pure function over its inputs, the same shape (and for the same
    /// testability reason) as `crate::commands::daemon::ansi_enabled`: the
    /// caller in `super::mod` does the `std::env` reads.
    ///
    /// - `no_color` set and **non-empty** flattens everything. An empty
    ///   `NO_COLOR=` is an unset one — the cross-ecosystem convention, and the
    ///   one already pinned for the shepherd's own log output.
    /// - `colorterm` containing `truecolor` or `24bit`, or `term` containing
    ///   `256color`, gets the 256-colour indices.
    /// - Anything else gets the 16 named colours. Erring narrow is the
    ///   recoverable direction: a deep terminal shown 16 colours looks
    ///   flatter, while a shallow one sent `\x1b[38;5;166m` can print the
    ///   escape as literal text.
    #[must_use]
    pub fn detect(
        no_color: Option<&OsStr>,
        term: Option<&OsStr>,
        colorterm: Option<&OsStr>,
    ) -> Self {
        if no_color.is_some_and(|value| !value.is_empty()) {
            return Self {
                meadow: None,
                bark: None,
                butter: None,
                ink3: None,
            };
        }
        let deep = colorterm.is_some_and(|value| {
            let value = value.to_string_lossy();
            value.contains("truecolor") || value.contains("24bit")
        }) || term.is_some_and(|value| value.to_string_lossy().contains("256color"));

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

    /// The style for one sheep's STATUS cell.
    ///
    /// `Errored` is the only status that gets `--bark`; `waiting-restart` is
    /// `--butter`, because it is a state to watch rather than damage that has
    /// happened. `Stopping` and `Stopped` are muted: a sheep that was asked to
    /// go and went is not a problem.
    #[must_use]
    pub fn status(self, status: ProcStatus) -> Style {
        // The mapping lives in `crate::vocabulary`, so the CLI's table and
        // this pane cannot drift. This method is now the ratatui BINDING of
        // it, and nothing more.
        Self::fg(match crate::vocabulary::role_of(status) {
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

    /// A refused action. `--bark`'s third permitted use, per the design
    /// language's own list — errored, refused, destructive.
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

    /// fails if `NO_COLOR` stops being honoured, or if an EMPTY `NO_COLOR=`
    /// starts being treated as set. The empty case is the one that regresses
    /// silently: a user who exports `NO_COLOR=` with no value would lose every
    /// colour in the dashboard and have nothing to blame it on. Same rule
    /// `commands::daemon::ansi_enabled` already pins for the shepherd's own
    /// log output.
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

    /// fails if an unknown terminal starts being handed 256-colour indices.
    /// The recoverable direction is the narrow one: a 256-colour terminal shown
    /// 16 colours looks flatter, while a 16-colour terminal sent
    /// `\x1b[38;5;166m` can print the escape as literal text.
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

    /// fails if `--bark` leaks onto anything that is not errored, refused or
    /// destructive. The design language reserves that colour and says so in
    /// those words; `waiting-restart` is the live temptation, because it is a
    /// state an operator worries about — and it is `--butter`, attention, not
    /// damage.
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

    /// fails if a status stops being distinguishable by TEXT alone. Colour in
    /// this dashboard is always redundant with the word beside it — that is
    /// what makes `NO_COLOR` a loss of decoration rather than a loss of
    /// information, and it is the house rule that the theme never costs
    /// clarity. This test is the rule, written where it can fail.
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
}
