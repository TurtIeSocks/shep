//! How much shep dresses up its output, and where that decision came from.

use std::ffi::OsStr;
use std::fmt;

/// How much shep dresses up its output.
///
/// One dial rather than three switches. Colour, boxes and sheep are not
/// independent tastes in practice: someone who wants the sheep gone usually
/// wants a calmer table, and someone who wants today's output wants all of
/// it gone. `NO_COLOR` remains orthogonal because it is a cross-ecosystem
/// convention about colour alone, not about layout.
///
/// `clap::ValueEnum` so `--style` and this type's own [`Self::parse`] agree
/// on spelling by construction: clap's derive and `parse` both lowercase the
/// variant name, so `full`/`plain`/`bare` is the one grammar -- but it is
/// [`Self::parse`] alone that reads it for both of the two free-form text
/// sources, `$SHEP_STYLE` (`lib.rs`'s `resolve_style`, through
/// [`resolve`]) and `shep.toml`'s `[style] level` (`lib.rs`'s
/// `style_from_config`). The flag goes through clap's own parser instead,
/// because clap owns argv. `[style] level` used to go through
/// [`clap::ValueEnum::from_str`] as well -- a second parser for the same
/// grammar that happened to agree on case but not on whitespace, since
/// `from_str` does not trim: `SHEP_STYLE=" full "` resolved and
/// `level = " full "` silently did not. One grammar, one parser, now.
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
    /// Read by `output::rows::FlockRows`'s own STATUS cell, which draws the
    /// face at `Full` and nothing but the plain status word otherwise, and
    /// by the milestone sheep art elsewhere (`commands/query.rs`'s empty
    /// flock, `commands/muster.rs`'s restored roll) -- both gate on exactly
    /// this method, alongside `Format::Table`, before ever calling into
    /// `flourish`.
    pub(crate) const fn sheep(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether tables are box-drawn.
    ///
    /// Read by `output`'s private `table_of` helper, which is how every
    /// table renderer in the crate picks between
    /// [`crate::output::render_table`] and `table.rs`'s own `render_boxed`
    /// -- both `table_of` and `render_boxed` are `output`-internal, so this
    /// doc names them in prose rather than as intra-doc links a reader
    /// outside that module could not follow anyway.
    pub(crate) const fn boxes(self) -> bool {
        matches!(self, Self::Full | Self::Plain)
    }

    /// Whether anything is coloured. `NO_COLOR` can still veto this; it
    /// cannot enable it.
    ///
    /// Read by [`Presentation::new`], the one place `NO_COLOR` is folded in
    /// -- this method alone answers what the level asked for, before the
    /// environment gets a veto.
    pub(crate) const fn colour(self) -> bool {
        matches!(self, Self::Full | Self::Plain)
    }

    /// Parses one of the three level names, case-insensitively and with
    /// surrounding whitespace trimmed first.
    ///
    /// The one parser for both free-form text sources of a level:
    /// `$SHEP_STYLE` (via [`resolve`]) and `shep.toml`'s `[style] level`
    /// (`lib.rs`'s `style_from_config`) both read a level through this
    /// function and nowhere else, so the two can never silently disagree
    /// on what counts as valid input the way trimming-vs-not once let them.
    /// `pub(crate)` rather than private for exactly that reason:
    /// `style_from_config` lives in `lib.rs`, outside this module.
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

/// Whether `NO_COLOR` vetoes colour: set and non-empty. An empty `NO_COLOR=`
/// reads as unset -- the cross-ecosystem convention.
///
/// Lives here, not in `lookout::theme::Palette::detect`, so
/// [`Presentation::new`] and that method call one copy instead of each
/// restating the rule: `mod lookout` is `#[cfg(unix)]` and this module is
/// not, so here is the one place both a unix-only binding and an
/// unconditional one can reach.
pub(crate) fn no_color_set(no_color: Option<&OsStr>) -> bool {
    no_color.is_some_and(|value| !value.is_empty())
}

/// Whether the terminal supports the 256-colour tier: `$COLORTERM`
/// containing `truecolor`/`24bit`, or `$TERM` containing `256color`.
/// Anything else gets the 16-colour fallback -- erring narrow is the
/// recoverable direction.
///
/// Lives here for the same reason [`no_color_set`] does: `output::paint`'s
/// `anstyle` binding and `lookout::theme`'s `ratatui` binding both need the
/// same yes/no answer to the same question, and only one of the two modules
/// that binds it exists off unix.
pub(crate) fn deep_colour_terminal(term: Option<&OsStr>, colorterm: Option<&OsStr>) -> bool {
    colorterm.is_some_and(|value| {
        let value = value.to_string_lossy();
        value.contains("truecolor") || value.contains("24bit")
    }) || term.is_some_and(|value| value.to_string_lossy().contains("256color"))
}

/// The level the operator chose, whether colour survived `NO_COLOR`, and
/// how deep the terminal's own colour support is.
///
/// Two values rather than one because they are two axes: `level` is a
/// layout dial the operator sets, and `colour` is a cross-ecosystem
/// convention that vetoes colour without touching layout. `Full` with
/// colour vetoed still draws boxes and sheep, which is the whole reason
/// `NO_COLOR` is honoured as its own axis rather than folded into the dial.
/// `deep_colour` is a third, independent fact about the terminal itself,
/// and `width` a fourth -- none of the four implies another, so folding
/// any two together would either lose information or synthesize an answer
/// nobody gave.
///
/// All four are resolved once, at the seam ([`Presentation::new`], called
/// from `run_argv`) and never afterward: this is a fact about the operator
/// and the terminal, not about any one render attempt. Contrast
/// `table_of`'s (`output/mod.rs`) own STATUS-word retry, which is a
/// per-attempt decision local to that function and its one caller
/// (`FlockRows::rows_for`) -- it is threaded as a plain `bool` parameter on
/// `Render::rows_for` rather than living here, precisely because it is not
/// a fact resolved once at the seam the way these four are: nothing but
/// `table_of` ever writes it and nothing but `FlockRows::status_cell` ever
/// reads it, so it stays function-local rather than leaking onto
/// crate-wide state that ~100 sites construct. `width` belongs here
/// because a live `crossterm::terminal::size()` call reads the process's
/// real controlling terminal, including under `cargo test` when the test
/// binary was launched from an interactive shell -- only a harness with no
/// controlling terminal makes such a test pass by accident. Resolving
/// `width` once here and injecting it everywhere else keeps the function
/// that renders pure in its inputs and provable at any width a test
/// chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Presentation {
    pub(crate) level: StyleLevel,
    pub(crate) colour: bool,
    pub(crate) deep_colour: bool,
    /// The terminal's width in columns, or `80` when there is none to
    /// measure -- `output::terminal_width`'s own fallback, folded in once
    /// here instead of read again by every table `table_of` renders in one
    /// invocation.
    pub(crate) width: usize,
}

impl Presentation {
    /// `Bare`, no colour, no depth, and an unused `width`: `Bare` never
    /// reaches `Render::rows_for` at all ([`StyleLevel::boxes`] is false,
    /// so `table_of` takes the plain `render_table` path instead), and
    /// `render_table` never reads `width` -- so `80` here is a placeholder,
    /// not a real fallback, chosen only to match `output::terminal_width`'s
    /// own so a reader does not have to ask why it differs. The safe
    /// default for the many test fixtures in this crate that want today's
    /// plain output and no more.
    ///
    /// Every real `Streams` a running `shep` builds carries the
    /// `Presentation` `lib.rs`'s `run_argv` actually resolved, never this
    /// constant, so only test fixtures ever name it — a plain (non-test)
    /// build of this crate has no caller at all. `#[allow(dead_code)]` says
    /// so explicitly rather than inventing a call site nothing needs yet.
    #[allow(dead_code)]
    pub(crate) const BARE: Self = Self {
        level: StyleLevel::Bare,
        colour: false,
        deep_colour: false,
        width: 80,
    };

    /// Resolves `colour` and `deep_colour` from already-read environment
    /// values, and carries `width` through unchanged -- terminal-ness and
    /// terminal width are always parameters here, never a `std::env`/
    /// `crossterm` call inside the function that renders, the same idiom
    /// `commands::daemon::ansi_enabled` and `lookout::theme::Palette::detect`
    /// both follow.
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
/// Reported by `shep style` because the failure this prevents is an operator
/// editing `shep.toml` and seeing nothing change, with `$SHEP_STYLE` set in a
/// shell profile they have forgotten about.
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

    /// `Presentation::new` folds `NO_COLOR` into `colour` and never lets it
    /// switch colour ON for a level that did not ask for it in the first
    /// place -- `Bare`'s own `colour()` is already `false`, so no env value
    /// can override that.
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

    /// `deep_colour` follows the terminal, independent of `colour` --
    /// `table_of` never reaches `output::paint::style_for` unless `colour`
    /// is also true, so a `Presentation` with `deep_colour` set and
    /// `colour` false is a harmless, never-read combination rather than an
    /// invalid one.
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
