//! How much shep dresses up its output, and where that decision came from.

use std::ffi::OsStr;
use std::fmt;

/// How much shep dresses up its output.
///
/// One dial, not three switches: colour, boxes and sheep are not independent
/// tastes in practice. `NO_COLOR` stays orthogonal, being a convention about
/// colour alone.
///
/// `clap::ValueEnum` so `--style` and [`Self::parse`] agree on spelling by
/// construction. [`Self::parse`] alone reads the two free-form sources,
/// `$SHEP_STYLE` and `shep.toml`'s `[style] level`, and trims them; clap's
/// own parser does not trim, so routing `[style] level` through it once let
/// `level = " full "` fail silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum StyleLevel {
    /// Sheep, boxes and colour.
    Full,
    /// Boxes and colour, no sheep.
    Plain,
    /// Exactly what shep printed before any of this, and exactly what a pipe
    /// gets.
    Bare,
}

impl StyleLevel {
    /// Whether sheep appear at all.
    ///
    /// Read by the STATUS cell renderer and the milestone sheep art,
    /// both gated on this alongside `Format::Table`.
    pub(crate) const fn sheep(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether tables are box-drawn.
    ///
    /// Read by `output`'s `table_of` helper, which picks between
    /// [`crate::output::render_table`] and the box-drawn renderer.
    pub(crate) const fn boxes(self) -> bool {
        matches!(self, Self::Full | Self::Plain)
    }

    /// Whether anything is coloured. `NO_COLOR` can still veto this; it
    /// cannot enable it.
    ///
    /// Read by [`Presentation::new`], the one place `NO_COLOR` is folded
    /// in.
    pub(crate) const fn colour(self) -> bool {
        matches!(self, Self::Full | Self::Plain)
    }

    /// Parses one of the three level names, case-insensitively and with
    /// surrounding whitespace trimmed first.
    ///
    /// The one parser for both free-form text sources of a level:
    /// `$SHEP_STYLE` (via [`resolve`]) and `shep.toml`'s `[style] level`
    /// both read through this function, so the two cannot disagree on
    /// what counts as valid input.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "plain" => Some(Self::Plain),
            "bare" => Some(Self::Bare),
            _ => None,
        }
    }
}

impl fmt::Display for StyleLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "full",
            Self::Plain => "plain",
            Self::Bare => "bare",
        })
    }
}

/// Whether `NO_COLOR` vetoes colour: set and non-empty. An empty
/// `NO_COLOR=` reads as unset.
///
/// Lives here rather than in `lookout::theme::Palette::detect` so both
/// call one copy: `lookout` is `#[cfg(unix)]` and this module is not.
pub(crate) fn no_color_set(no_color: Option<&OsStr>) -> bool {
    no_color.is_some_and(|value| !value.is_empty())
}

/// Whether the terminal supports the 256-colour tier: `$COLORTERM`
/// containing `truecolor`/`24bit`, or `$TERM` containing `256color`.
/// Anything else gets the 16-colour fallback.
///
/// Lives here for the same reason [`no_color_set`] does.
pub(crate) fn deep_colour_terminal(term: Option<&OsStr>, colorterm: Option<&OsStr>) -> bool {
    colorterm.is_some_and(|value| {
        let value = value.to_string_lossy();
        value.contains("truecolor") || value.contains("24bit")
    }) || term.is_some_and(|value| value.to_string_lossy().contains("256color"))
}

/// The level the operator chose, whether colour survived `NO_COLOR`, and
/// how deep the terminal's own colour support is.
///
/// Four independent axes: `level` is a layout dial, `colour` is
/// `NO_COLOR`'s veto over it, `deep_colour` is a fact about the terminal,
/// and `width` a fourth. None implies another.
///
/// Resolved once, at [`Presentation::new`], and never per render: `width`
/// in particular reads the process's real controlling terminal, which a
/// per-attempt read would make a test's result depend on how it was
/// launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Presentation {
    pub(crate) level: StyleLevel,
    pub(crate) colour: bool,
    pub(crate) deep_colour: bool,
    /// The terminal's width in columns, or `80` when there is none to
    /// measure.
    pub(crate) width: usize,
}

impl Presentation {
    /// `Bare`, no colour, no depth. `width` here is never read: `Bare`
    /// takes the plain `render_table` path, which ignores it. The
    /// default for test fixtures that want today's plain output.
    ///
    /// No non-test caller: `#[allow(dead_code)]` says so rather than
    /// inventing one.
    #[allow(dead_code)]
    pub(crate) const BARE: Self = Self {
        level: StyleLevel::Bare,
        colour: false,
        deep_colour: false,
        width: 80,
    };

    /// Resolves `colour` and `deep_colour` from already-read environment
    /// values, and carries `width` through unchanged: terminal-ness and
    /// width are always parameters here, never a `std::env`/`crossterm`
    /// call inside a render function.
    pub(crate) fn new(
        level: StyleLevel,
        no_color: Option<&OsStr>,
        term: Option<&OsStr>,
        colorterm: Option<&OsStr>,
        width: usize,
    ) -> Self {
        Self {
            level,
            colour: level.colour() && !no_color_set(no_color),
            deep_colour: deep_colour_terminal(term, colorterm),
            width,
        }
    }
}

/// Which layer decided the level in force.
///
/// Reported by `shep style`, since an operator editing `shep.toml` and
/// seeing nothing change usually has `$SHEP_STYLE` set in a forgotten
/// shell profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StyleSource {
    /// `--style` on this invocation.
    Flag,
    /// `$SHEP_STYLE`.
    Env,
    /// `[style] level` in `shep.toml`.
    Config,
    /// Nothing said otherwise.
    Default,
}

impl fmt::Display for StyleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Flag => "--style",
            Self::Env => "$SHEP_STYLE",
            Self::Config => "shep.toml",
            Self::Default => "the default",
        })
    }
}

/// Picks the level in force, and says which layer picked it.
///
/// An unparseable `$SHEP_STYLE` falls through to the next source rather than
/// failing: a typo in a shell profile must not make every shep command
/// unusable, and the level is a preference rather than a correctness input.
pub(crate) fn resolve(
    flag: Option<StyleLevel>,
    env: Option<&str>,
    config: Option<StyleLevel>,
) -> (StyleLevel, StyleSource) {
    if let Some(level) = flag {
        return (level, StyleSource::Flag);
    }
    if let Some(level) = env.and_then(StyleLevel::parse) {
        return (level, StyleSource::Env);
    }
    if let Some(level) = config {
        return (level, StyleSource::Config);
    }
    (StyleLevel::Full, StyleSource::Default)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First hit wins, and the source is reported: an operator whose env var
    /// and config file disagree needs to know which one won.
    #[test]
    fn the_flag_beats_the_env_beats_the_config_beats_the_default() {
        assert_eq!(
            resolve(
                Some(StyleLevel::Bare),
                Some("full"),
                Some(StyleLevel::Plain)
            ),
            (StyleLevel::Bare, StyleSource::Flag)
        );
        assert_eq!(
            resolve(None, Some("bare"), Some(StyleLevel::Full)),
            (StyleLevel::Bare, StyleSource::Env)
        );
        assert_eq!(
            resolve(None, None, Some(StyleLevel::Plain)),
            (StyleLevel::Plain, StyleSource::Config)
        );
        assert_eq!(
            resolve(None, None, None),
            (StyleLevel::Full, StyleSource::Default)
        );
    }

    /// An unreadable `$SHEP_STYLE` falls through rather than failing every
    /// command: a typo in a shell profile must not make shep unusable.
    #[test]
    fn an_unparseable_env_value_falls_through_to_the_next_source() {
        assert_eq!(
            resolve(None, Some("shiny"), Some(StyleLevel::Bare)),
            (StyleLevel::Bare, StyleSource::Config)
        );
    }

    /// The three levels are three answers to three questions, and `bare` is
    /// exactly what a pipe gets.
    #[test]
    fn each_level_answers_all_three_questions() {
        assert_eq!(
            (
                StyleLevel::Full.sheep(),
                StyleLevel::Full.boxes(),
                StyleLevel::Full.colour()
            ),
            (true, true, true)
        );
        assert_eq!(
            (
                StyleLevel::Plain.sheep(),
                StyleLevel::Plain.boxes(),
                StyleLevel::Plain.colour()
            ),
            (false, true, true)
        );
        assert_eq!(
            (
                StyleLevel::Bare.sheep(),
                StyleLevel::Bare.boxes(),
                StyleLevel::Bare.colour()
            ),
            (false, false, false)
        );
    }

    /// `NO_COLOR` unset or empty is not set. An operator who exports
    /// `NO_COLOR=` with no value must not silently lose every colour this
    /// crate draws, in the table or the dashboard alike.
    #[test]
    fn no_color_set_treats_an_empty_value_as_unset() {
        assert!(!no_color_set(None));
        assert!(!no_color_set(Some(OsStr::new(""))));
        assert!(no_color_set(Some(OsStr::new("1"))));
    }

    /// The other rule moved out of `theme.rs`: a `COLORTERM` naming
    /// truecolor/24bit, or a `TERM` naming 256color, is the 256-colour
    /// tier; anything else, including nothing at all, is the 16-colour
    /// fallback.
    #[test]
    fn deep_colour_terminal_reads_colorterm_then_term() {
        assert!(!deep_colour_terminal(None, None));
        assert!(!deep_colour_terminal(Some(OsStr::new("vt100")), None));
        assert!(deep_colour_terminal(
            Some(OsStr::new("xterm-256color")),
            None
        ));
        assert!(deep_colour_terminal(None, Some(OsStr::new("truecolor"))));
        assert!(deep_colour_terminal(
            Some(OsStr::new("dumb")),
            Some(OsStr::new("24bit"))
        ));
    }

    /// `Presentation::new` folds `NO_COLOR` into `colour`, and never lets
    /// it switch colour on for a level that did not ask for it: `Bare`'s
    /// own `colour()` is already `false`.
    #[test]
    fn presentation_new_folds_no_color_into_the_levels_own_answer() {
        let full_untouched = Presentation::new(StyleLevel::Full, None, None, None, 80);
        assert!(full_untouched.colour);

        let full_vetoed =
            Presentation::new(StyleLevel::Full, Some(OsStr::new("1")), None, None, 80);
        assert!(!full_vetoed.colour);

        let bare_with_no_color_unset = Presentation::new(
            StyleLevel::Bare,
            None,
            Some(OsStr::new("xterm-256color")),
            None,
            80,
        );
        assert!(
            !bare_with_no_color_unset.colour,
            "bare never asked for colour; NO_COLOR being unset does not grant it"
        );
    }

    /// `deep_colour` follows the terminal, independent of `colour`: a
    /// `Presentation` with `deep_colour` set and `colour` false is a
    /// harmless, never-read combination rather than an invalid one.
    #[test]
    fn presentation_new_resolves_deep_colour_from_the_terminal() {
        let deep = Presentation::new(
            StyleLevel::Full,
            None,
            Some(OsStr::new("xterm-256color")),
            None,
            80,
        );
        assert!(deep.deep_colour);

        let shallow =
            Presentation::new(StyleLevel::Full, None, Some(OsStr::new("vt100")), None, 80);
        assert!(!shallow.deep_colour);
    }
}
