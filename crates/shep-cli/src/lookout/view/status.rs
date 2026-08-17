//! The three chrome lines: the title, the link banner, and the status bar.
//!
//! Every sentence here is literal. The design language's standing rule is
//! that nothing about damage gets charming, and this file is where all of
//! shep's damage reporting on this screen lives — the frozen banner, the
//! drop notice, the refusal.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{App, Control, InputMode, Link};
use super::flock::fit;

/// The title: what this is, where it points, and how big the flock is.
///
/// `right` carries a leading space so the two halves never touch: `fit`
/// pads a short `left` with trailing spaces, which already keeps them apart
/// at ordinary widths, but a `left` too long to fit gets truncated to
/// EXACTLY the budget `right`'s own length reserves — with no leading space
/// on `right`, the `…` and the flock count would land in adjacent columns
/// with nothing between them. The leading space shrinks that budget by one,
/// which is all a truncated `left` needs.
#[must_use]
pub fn title_line(app: &App, home: &str, width: u16) -> Line<'static> {
    let palette = app.palette();
    let left = format!("shep lookout   {home}");
    let visible = app.rows().len();
    let total = app.flock_len();
    let right = if app.filter().is_empty() {
        format!(" {total} in the flock")
    } else {
        format!(" {visible} of {total} in the flock")
    };
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

/// The bottom line: six slots, highest priority first — an armed confirm,
/// the filter box while editing, a notice, an in-flight action, the applied
/// filter line, then the key hint — with the control state always rendered
/// on the right. See the phase plan's "Shapes the design named" #2 for why
/// the box and the applied filter line are two slots and not one.
///
/// Slots 1 (armed confirm) and 4 (in-flight action) are Task 9's; this task
/// leaves the chain shaped for them without writing their arms.
#[must_use]
pub fn status_line(app: &App, width: u16) -> Line<'static> {
    let palette = app.palette();
    let (left, left_style) = if app.mode() == InputMode::Text {
        // ABOVE the notice, not below it. `Msg::BusLagged`,
        // `BusEvent::Dropped` and `BusEvent::DaemonShutdown` all raise notices
        // with no keypress involved, and they keep arriving while somebody is
        // typing a sheep name; a notice that covered the box would take the
        // query off the screen mid-word, and `on_text_key` does not clear
        // notices, so nothing the operator typed would bring it back. The
        // notice is not lost: it is what this slot shows the moment the box
        // closes. A report of a past event does not outrank an interaction in
        // progress.
        //
        // The cursor is a character, not a style: the ANSI gallery renders
        // foregrounds only, and a reversed cell would come out unstyled there.
        // Same call the selection marker already makes.
        (
            format!(
                "filter  {}\u{258f}   enter applies   esc cancels   ctrl-c quits",
                app.filter()
            ),
            palette.attention(),
        )
    } else if let Some(notice) = app.notice() {
        (
            notice.to_string(),
            if notice.is_grave() {
                palette.refusal()
            } else {
                palette.attention()
            },
        )
    } else if !app.filter().is_empty() {
        (
            format!("filter \"{}\"   / edit   esc clear", app.filter()),
            palette.muted(),
        )
    } else {
        (hint_for(app.control()), palette.muted())
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

/// The key hint.
///
/// One form for both control states in this task, which is what shipped: there
/// is nothing to advertise behind the gate yet. Task 9 gives it a second form
/// once the three action keys exist, at which point this file's standing rule
/// applies to it. Writing that second form here would put `x stop   R restart
/// L reload` on the screen of a build where two of those three keys do
/// nothing, which is the asterisk-instead-of-a-hint failure the rule is about,
/// shipped by the plan rather than by the code.
///
/// The text is 59 characters, up from the 48 that shipped. It still truncates
/// at the 39 columns the 49-column gap test leaves for it, and the first 40
/// characters are byte-identical to the old hint, so
/// `a_truncated_hint_still_leaves_a_gap_before_the_control_label` measures
/// exactly what it was written to measure and the `narrow` and `cramped`
/// frames do not move.
fn hint_for(_control: Control) -> String {
    "q quit   j/k select   g/G first/last   r refresh   / filter".to_string()
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

    use shep_core::protocol::BusEvent;

    use super::super::fixtures::{editing_app, filtered_app, rendered};
    use super::*;
    use crate::lookout::app::{App, KeyPress, Msg};
    use crate::lookout::theme::Palette;

    /// fails if the truncated key hint ever butts straight against the
    /// control-state label again. Pinned at 49 columns, not because any
    /// gallery scene happens to be that width, but because that is exactly
    /// where the bug shipped: the default hint is 59 characters, the label
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
    /// The replacement is 59 characters, up from the original 48, but its
    /// first 40 characters are unchanged, so
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

    /// fails if the title stops carrying the flock's real size while a filter
    /// is on. `{visible} in the flock` alone understates the flock, and an
    /// operator who cannot see that a filter is hiding rows is an operator
    /// about to conclude that sheep have vanished.
    #[test]
    fn the_title_counts_both_numbers_while_a_filter_is_on() {
        let app = filtered_app("web");
        let title = rendered(&title_line(&app, "/home/rin/.shep", 120));
        assert!(title.contains("2 of 4 in the flock"), "got {title:?}");
    }

    /// fails if the unfiltered title changes at all. It is on every frame in
    /// the gallery and nothing about this feature touches it.
    #[test]
    fn the_unfiltered_title_is_unchanged() {
        let app = filtered_app("");
        let title = rendered(&title_line(&app, "/home/rin/.shep", 120));
        assert!(title.contains("4 in the flock"), "got {title:?}");
        assert!(
            !title.contains(" of "),
            "no second number when nothing is hidden"
        );
    }

    /// fails if the bar stops saying what the two filter keys do. The whole
    /// argument for `esc` meaning something different while a filter is set is
    /// that the screen says so at the moment it is true.
    #[test]
    fn the_bar_names_the_filter_keys_while_a_filter_is_applied() {
        let app = filtered_app("web");
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("filter \"web\""), "the query, quoted: {bar:?}");
        assert!(bar.contains("/ edit"), "got {bar:?}");
        assert!(bar.contains("esc clear"), "got {bar:?}");
    }

    /// fails if the box stops showing what is being typed, or stops naming the
    /// only three keys that mean anything while it is open.
    #[test]
    fn the_bar_carries_the_query_and_a_cursor_while_editing() {
        let app = editing_app("we");
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("filter  we\u{258f}"),
            "query then cursor: {bar:?}"
        );
        assert!(bar.contains("enter applies"), "got {bar:?}");
        assert!(bar.contains("esc cancels"), "got {bar:?}");
        assert!(bar.contains("ctrl-c quits"), "got {bar:?}");
    }

    /// fails if a notice covers the box. A `Dropped` event arrives with no
    /// keypress and `on_text_key` does not clear notices, so a bar that
    /// ranked the notice higher would take the operator's half-typed query
    /// off the screen and leave it off until they pressed Enter or Esc.
    #[test]
    fn a_notice_raised_while_typing_does_not_cover_the_box() {
        let mut app = editing_app("we");
        app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("filter  we\u{258f}"),
            "the box is still there: {bar:?}"
        );
        assert!(!bar.contains("dropped 3 events"), "got {bar:?}");
    }

    /// fails if the deferred notice never gets its turn. The mirror of the
    /// test above: hiding a notice under the box is only honest if closing the
    /// box shows it.
    #[test]
    fn closing_the_box_shows_the_notice_that_was_waiting() {
        let mut app = editing_app("we");
        app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
        app.update(Msg::Key(KeyPress::FilterApply));
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("dropped 3 events"), "got {bar:?}");
    }

    /// fails if `/` stops being advertised. It is the only way into the box
    /// and nothing else on the screen hints at it.
    #[test]
    fn the_read_only_hint_advertises_the_filter_key() {
        let app = filtered_app("");
        let hint = rendered(&status_line(&app, 200));
        assert!(hint.contains("/ filter"), "got {hint:?}");
    }
}
