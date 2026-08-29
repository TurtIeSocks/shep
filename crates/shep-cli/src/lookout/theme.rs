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
    ///   one already pinned for the shepherd's own log output. Calls
    ///   [`crate::style::no_color_set`], the one copy of the rule both this
    ///   binding and `output::paint`'s share, since `mod lookout` is
    ///   `#[cfg(unix)]` and `crate::style` is not.
    /// - `colorterm` containing `truecolor` or `24bit`, or `term` containing
    ///   `256color`, gets the 256-colour indices -- also
    ///   [`crate::style::deep_colour_terminal`] now, for the same reason.
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

    /// The design spec's own rule (§6, "the two renderings agree by
    /// construction"): this binding and `output::paint`'s -- the CLI
    /// table's own binding of the same roles -- must resolve every role to
    /// the same underlying colour, at both tiers. Extracts the numeric
    /// index or name from each side and compares those directly, rather
    /// than re-hardcoding one shared literal in both this test and
    /// `paint`'s own: a renumbering on one side that forgets the other
    /// fails this, which two independently-chosen matching literals would
    /// not catch.
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
    /// so `ratatui::style::Color::DarkGray` and `anstyle::AnsiColor::BrightBlack`
    /// -- the same ANSI code 90 under two different names -- compare equal.
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
