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
        // `x` (stop) is bound and still always refuses — see `App::on_key`
        // — so it is left out of the hint rather than marked somehow. This
        // file's own standing rule is that every sentence here is literal;
        // a hint that needs a footnote to be true is not literal, it is an
        // asterisk. The key still works as a refusal, and still exercises
        // the control gate; it is only the advertisement that is gone.
        None => (
            // `select`/`first/last`, not `scroll`/`top/bottom`: the pane
            // carries a cursor now and the viewport is derived from it. Same
            // 48 characters as the original 12a scroll-hint text, so the
            // truncation test at 49 columns still measures what it was
            // written to measure.
            "q quit   j/k select   g/G first/last   r refresh".to_string(),
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
    // `+ 1` reserves one column of gap so a truncated left side's `…` never
    // butts straight against the label. Without it `fit` fills every column
    // up to `right`, and a truncation and a right-aligned label landing on
    // the same frame is the ordinary case, not a corner one — see
    // `a_truncated_hint_still_leaves_a_gap_before_the_control_label` below,
    // pinned at the exact width where this shipped without it.
    // The gap rides inside the right span, styled the same as the label
    // rather than as its own `Span::raw`, so the two-span, single-colour
    // shape of every wider scene's status line is unchanged byte-for-byte.
    let left_width = width.saturating_sub(right_len).saturating_sub(1);
    Line::from(vec![
        Span::styled(fit(&left, left_width), left_style),
        Span::styled(format!(" {right}"), palette.muted()),
    ])
}

/// A run of `─` across the pane, under the header.
///
/// One rule, not a box. `output::table`'s own doc argues that a table a
/// user can `awk` over beats one that looks nice; the same instinct applies
/// to a pane an operator reads at 3am, and a full border costs two columns
/// and two rows of the thing they are trying to read.
#[must_use]
pub fn rule_line(style: Style, width: u16) -> Line<'static> {
    Line::from(Span::styled("─".repeat(usize::from(width)), style))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::time::Instant;

    use super::*;
    use crate::lookout::app::App;
    use crate::lookout::theme::Palette;

    /// fails if the truncated key hint ever butts straight against the
    /// control-state label again. Pinned at 49 columns, not because any
    /// gallery scene happens to be that width, but because that is exactly
    /// where the bug shipped: the default hint is 48 characters, the label
    /// 9, and at this width the hint truncates while the label still fits,
    /// which is the one combination that makes a missing gap visible. (An
    /// earlier version of this comment tied the width to the `narrow`
    /// gallery scene; `narrow` moved to 51 columns in Phase 12b, and this
    /// test did not need to move with it — 49 is a property of the hint and
    /// the label, not of any one scene.)
    #[test]
    fn a_truncated_hint_still_leaves_a_gap_before_the_control_label() {
        let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let app = App::new(
            palette,
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let line = status_line(&app, 49);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(rendered.chars().count(), 49, "must fill the full width");
        assert!(
            rendered.ends_with(" read-only"),
            "expected a space before the label, got: {rendered:?}"
        );
        assert!(
            !rendered.contains("…read-only"),
            "the ellipsis must not butt straight against the label: {rendered:?}"
        );
    }

    /// fails if the key hint keeps saying `scroll` after the keys stopped
    /// scrolling. This is the only one of the four renamed names an operator
    /// ever reads — it is on every frame in the gallery — so leaving it would
    /// be shipping the exact lie this task exists to remove, on the one
    /// surface where it is visible.
    ///
    /// The replacement is the same 48 characters as the original, so
    /// `a_truncated_hint_still_leaves_a_gap_before_the_control_label` at 49
    /// columns is measuring the same thing it measured before.
    #[test]
    fn the_key_hint_says_what_the_keys_now_do() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let hint: String = status_line(&app, 200)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(hint.contains("j/k select"), "got {hint:?}");
        assert!(hint.contains("g/G first/last"), "got {hint:?}");
        assert!(
            !hint.contains("scroll"),
            "the pane no longer scrolls: {hint:?}"
        );
    }

    /// fails if the gap logic breaks the ordinary, untruncated case — a wide
    /// terminal where the hint fits with room to spare.
    #[test]
    fn a_wide_status_line_still_pads_out_to_the_full_width() {
        let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let app = App::new(
            palette,
            Control::Allowed,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let line = status_line(&app, 120);
        let rendered: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert_eq!(rendered.chars().count(), 120);
        assert!(rendered.ends_with(" control enabled"));
    }
}
