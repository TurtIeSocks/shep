//! The three chrome lines: the title, the link banner, and the status bar.
//!
//! Every sentence here is literal. The design language's standing rule is
//! that nothing about damage gets charming, and this file is where all of
//! shep's damage reporting on this screen lives — the frozen banner, the
//! drop notice, the refusal.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{App, Control, Link};
use super::flock::fit;

/// The title: what this is, where it points, and how big the flock is.
#[allow(dead_code)]
#[must_use]
pub fn title_line(app: &App, home: &str, width: u16) -> Line<'static> {
    let palette = app.palette();
    let left = format!("shep lookout   {home}");
    let count = app.rows().len();
    let right = format!("{count} in the flock");
    Line::from(vec![
        Span::raw(fit(
            &left,
            width.saturating_sub(u16::try_from(right.chars().count()).unwrap_or(0)),
        )),
        Span::styled(right, palette.muted()),
    ])
}

/// The banner, when there is one. `None` while the link is live.
///
/// The frozen sentence is the whole of Rin's ruling in one line: it names
/// what happened, and it names when the values stopped being current, so an
/// operator reading a screen full of `online` knows exactly how much to
/// trust it.
#[allow(dead_code)]
#[must_use]
pub fn banner_line(app: &App) -> Option<Line<'static>> {
    let palette = app.palette();
    match app.link() {
        Link::Live => None,
        Link::Retrying { attempt } => Some(Line::from(Span::styled(
            format!("the shepherd stopped answering — reconnecting (attempt {attempt})"),
            palette.attention(),
        ))),
        Link::Lost { at_local } => Some(Line::from(Span::styled(
            format!("the shepherd has died — these values are frozen as of {at_local}"),
            palette.alarm(),
        ))),
    }
}

/// The bottom line: a notice if there is one, else the key hints; then the
/// control state, always.
#[allow(dead_code)]
#[must_use]
pub fn status_line(app: &App, width: u16) -> Line<'static> {
    let palette = app.palette();
    let (left, left_style) = match app.notice() {
        Some(notice) => (
            notice.to_string(),
            if notice.is_grave() {
                palette.refusal()
            } else {
                palette.attention()
            },
        ),
        None => (
            "q quit   j/k scroll   g/G top/bottom   r refresh   x stop".to_string(),
            palette.muted(),
        ),
    };
    // Always rendered, in both states. An operator who does not know whether
    // their dashboard can act is one keystroke from finding out the wrong
    // way.
    let right = match app.control() {
        Control::ReadOnly => "read-only",
        Control::Allowed => "control enabled",
    };
    let right_len = u16::try_from(right.chars().count()).unwrap_or(0);
    Line::from(vec![
        Span::styled(fit(&left, width.saturating_sub(right_len)), left_style),
        Span::styled(right, palette.muted()),
    ])
}

/// A run of `─` across the pane, under the header.
///
/// One rule, not a box. `output::table`'s own doc argues that a table a
/// user can `awk` over beats one that looks nice; the same instinct applies
/// to a pane an operator reads at 3am, and a full border costs two columns
/// and two rows of the thing they are trying to read.
#[allow(dead_code)]
#[must_use]
pub fn rule_line(style: Style, width: u16) -> Line<'static> {
    Line::from(Span::styled("─".repeat(usize::from(width)), style))
}
