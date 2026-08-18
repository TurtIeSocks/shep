//! How much shep dresses up its output, and where that decision came from.

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
/// variant name, so `full`/`plain`/`bare` is the one grammar, read by the
/// flag, `$SHEP_STYLE`, and `shep.toml`'s `[style] level` alike (the latter
/// two go through [`clap::ValueEnum::from_str`], the flag through clap's own
/// parser).
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
    /// Not called outside this module's own tests yet: sheep art itself is
    /// Task 6's job. `#[allow(dead_code)]` says so explicitly rather than
    /// inventing a call site nothing needs yet.
    #[allow(dead_code)]
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
    /// Not called outside this module's own tests yet: nothing this crate
    /// renders emits colour before Task 6, so there is nothing yet for this
    /// to gate. `#[allow(dead_code)]` says so explicitly rather than
    /// inventing a call site nothing needs yet.
    #[allow(dead_code)]
    pub(crate) const fn colour(self) -> bool {
        matches!(self, Self::Full | Self::Plain)
    }

    /// Parses one of the three level names, case-insensitively.
    fn parse(raw: &str) -> Option<Self> {
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
}
