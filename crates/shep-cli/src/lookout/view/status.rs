//! The three chrome lines: the title, the link banner, and the status bar.
//!
//! Every sentence here is literal. The design language's standing rule is
//! that nothing about damage gets charming, and this file is where all of
//! shep's damage reporting on this screen lives — the frozen banner, the
//! drop notice, the refusal.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{ActionState, App, Control, InputMode, Link, RowKey, Settings};
use super::flock::fit;
use super::settings::field_label;

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
/// The frozen sentence is the whole of the maintainer's ruling in one line: it names
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

/// The bottom line: seven slots, highest priority first -- an armed
/// confirm, the settings screen's own free-text editor, the filter box
/// while editing, a notice, an in-flight action, the applied filter line,
/// then the key hint -- with the control state always rendered on the
/// right. See the phase plan's "Shapes the design named" #2 for why the
/// box and the applied filter line are two slots and not one. The editor
/// slot sits ahead of the filter box for the same reason the confirm sits
/// ahead of both: `App::settings().and_then(Settings::typing)` is only
/// ever `Some` while the settings screen owns `InputMode::Text`, and the
/// filter box's own `App::filter` is untouched the whole time, so checking
/// the editor first is what keeps the two from ever answering the same
/// keystroke with the wrong sentence.
#[must_use]
pub fn status_line(app: &App, width: u16) -> Line<'static> {
    let palette = app.palette();
    let (left, left_style) = if let Some(action) = app.action().filter(|a| !a.sent) {
        // Slot 1. A18: a question awaiting an answer outranks everything,
        // including the filter box, which it cannot coexist with anyway
        // because `/` cancels a confirm before it opens the box.
        (confirm_prompt(&action), palette.attention())
    } else if let Some((field, buffer)) = app.settings().and_then(Settings::typing) {
        // The settings screen's own free-text editor, checked ahead of the
        // dashboard's filter-box branch below: both share `InputMode::Text`
        // (task 8's editor reuses the filter box's keymap), but this one is
        // typing into `socket` or `max_cron_sleep`, not `App::filter`. A bar
        // that fell through to the filter branch here would render the
        // dashboard's own (untouched) query under the label "filter" while
        // the body pane's own `editing socket: ...` line says something
        // else entirely on the same frame -- the screen would contradict
        // itself. `field_label` is shared with `view::settings` so the two
        // panes can never disagree about what to call the field.
        (
            format!(
                "editing {}  {buffer}\u{258f}   enter applies   esc cancels",
                field_label(*field)
            ),
            palette.attention(),
        )
    } else if app.mode() == InputMode::Text {
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
    } else if let Some(action) = app.action() {
        // Slot 4. BELOW the notice, and that is load-bearing rather than a
        // concession. `arm`'s "one action is already in flight" IS a notice,
        // so a bar that put this line above notices would swallow the answer
        // to the operator's own keypress: they press `R` while a stop is out,
        // the screen does not change, and the dashboard has hidden a refusal
        // it made. Every bus-raised notice (`Dropped`, `BusLagged`,
        // `DaemonShutdown`) would be equally invisible for as long as an
        // action was in flight. A18 puts the notice above the filter line and
        // the design puts this line above the filter line; the notice above
        // this is what satisfies both.
        //
        // What the design DOES require of this state is that a keypress
        // cannot wipe it, and that is a property of the reducer, not of this
        // order: the next keypress clears the notice and this line comes
        // back.
        let text = in_flight_text(&action);
        // `attention`, the same butter the non-grave notice uses. Not a
        // modal, not a box, not a `ratatui::widgets::Clear`: there is no
        // overlay anywhere in this module, and one rule under the header
        // beats a full border for a pane somebody reads at 3am.
        (text, palette.attention())
    } else if app.settings().is_none() && !app.filter().is_empty() {
        // Gated on the screen being closed: the filter survives the swap
        // into settings (`App::on_settings_key` never touches it), but `/`
        // and `esc` mean something else entirely while the screen owns the
        // keyboard, so this line would be false the moment it stayed up.
        (
            format!("filter \"{}\"   / edit   esc clear", app.filter()),
            palette.muted(),
        )
    } else {
        (
            hint_for(app.control(), app.settings().is_some()),
            palette.muted(),
        )
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

/// The confirm prompt's own sentence: which verb, which target, and how to
/// answer.
///
/// A group row is the one place a keypress reaches several processes, so
/// the prompt says how many before the operator commits. A single sheep
/// keeps the `(id N)` form unchanged from before this feature.
fn confirm_prompt(action: &ActionState<'_>) -> String {
    match action.target {
        RowKey::Sheep(id) => format!(
            "{} {} (id {id})? enter confirms, any other key cancels",
            action.verb.label(),
            action.name
        ),
        RowKey::Group(name) => {
            let count = action.count;
            format!(
                "{} all {count} instances of {name}? enter confirms, any other key cancels",
                action.verb.label()
            )
        }
    }
}

/// The in-flight line: the same verb-and-target naming [`confirm_prompt`]
/// uses, once the request has already gone out.
fn in_flight_text(action: &ActionState<'_>) -> String {
    match action.target {
        RowKey::Sheep(id) => format!(
            "{} {} (id {id}): sent, waiting for the shepherd",
            action.verb.label(),
            action.name
        ),
        RowKey::Group(name) => format!(
            "{} all {} instances of {name}: sent, waiting for the shepherd",
            action.verb.label(),
            action.count
        ),
    }
}

/// The key hint.
///
/// Three forms now: the settings screen's own, and the dashboard's two.
/// `settings_open` picks between them, and wins outright, because the
/// dashboard's `x`/`R`/`L`/`r`/`/` mean nothing while the screen owns the
/// keyboard. This file's standing rule: a hint that needs a footnote to be
/// true is an asterisk, not a hint, so `Control::Allowed`'s dashboard form
/// is only ever handed to a dashboard where `x`, `R` and `L` really do arm a
/// confirm, and the settings form is only ever handed to a dashboard where
/// they do nothing at all.
///
/// `s settings` is APPENDED to both dashboard forms, never inserted: the
/// read-only text's first 40 characters have to stay byte-identical for
/// `a_truncated_hint_still_leaves_a_gap_before_the_control_label` and the
/// `narrow`/`cramped` gallery frames to keep measuring what they were
/// written to measure, and appending is the one edit that cannot move them.
fn hint_for(control: Control, settings_open: bool) -> String {
    if settings_open {
        return "esc close   j/k select   g/G first/last   space cycle".to_string();
    }
    match control {
        Control::ReadOnly => {
            "q quit   j/k select   g/G first/last   r refresh   / filter   s settings"
        }
        // `g/G` and `r` drop out to make room. They are the two an operator
        // rediscovers by pressing them; an action key is not.
        Control::Allowed => {
            "q quit   j/k select   / filter   x stop   R restart   L reload   s settings"
        }
    }
    .to_string()
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

    use super::super::fixtures::{
        acting_app, allowed_app, app_in_settings_on, armed_app,
        armed_app_with_a_filter_and_a_notice, editing_app, filtered_app, rendered,
    };
    use super::*;
    use crate::commands::settings::SettingField;
    use crate::lookout::app::{ActionVerb, App, KeyPress, Msg};
    use crate::lookout::theme::Palette;

    /// fails if the truncated key hint ever butts straight against the
    /// control-state label again. Pinned at 49 columns, not because any
    /// gallery scene happens to be that width, but because that is exactly
    /// where the bug shipped: the default hint is 59 characters, the label
    /// 9, and at this width the hint truncates while the label still fits,
    /// which is the one combination that makes a missing gap visible. 49
    /// is a property of the hint and the label, not of any one gallery
    /// scene.
    #[test]
    fn a_truncated_hint_still_leaves_a_gap_before_the_control_label() {
        let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let app = App::new(
            palette,
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
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
            "/home/ada/.shep".to_string(),
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
            "/home/ada/.shep".to_string(),
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
        let title = rendered(&title_line(&app, "/home/ada/.shep", 120));
        assert!(title.contains("2 of 4 in the flock"), "got {title:?}");
    }

    /// fails if the unfiltered title changes at all. It is on every frame in
    /// the gallery and nothing about this feature touches it.
    #[test]
    fn the_unfiltered_title_is_unchanged() {
        let app = filtered_app("");
        let title = rendered(&title_line(&app, "/home/ada/.shep", 120));
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

    /// fails if the bar falls through to the dashboard's own filter box
    /// while the settings screen's free-text editor owns `InputMode::Text`
    /// instead -- the review finding this pins: before the fix, this state
    /// rendered "filter  <the dashboard's untouched query>" under the
    /// editor's own body line saying "editing socket: ...", contradicting
    /// itself on one frame.
    #[test]
    fn the_bar_shows_the_settings_editor_rather_than_the_filter_box() {
        let mut app = app_in_settings_on(SettingField::Socket);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("editing socket  "),
            "names the field being typed: {bar:?}"
        );
        assert!(
            bar.contains("/home/ada/.shep/run/shep.sock\u{258f}"),
            "shows the buffer and the cursor, not the dashboard's own filter: {bar:?}"
        );
        assert!(
            !bar.contains("filter "),
            "must not read as the filter box: {bar:?}"
        );
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
        app.update(Msg::Key(KeyPress::TextApply));
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

    /// fails if the prompt stops naming the verb and the exact sheep, or stops
    /// saying what answers it. A question an operator has to guess the answer
    /// to is worse than no question.
    #[test]
    fn an_armed_confirm_names_the_verb_the_sheep_and_the_answer() {
        let app = armed_app(ActionVerb::Restart);
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("restart api (id 2)?"), "got {bar:?}");
        assert!(
            bar.contains("enter confirms, any other key cancels"),
            "got {bar:?}"
        );
    }

    /// fails if the in-flight line stops saying that the shepherd has not
    /// answered yet. Nothing on the table has changed at this point, because
    /// nothing the shepherd said has changed yet, and the bar is the only
    /// thing on screen that knows a request is out.
    #[test]
    fn an_in_flight_action_says_it_is_waiting() {
        let app = acting_app(ActionVerb::Stop);
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("stop api (id 2): sent, waiting for the shepherd"),
            "got {bar:?}"
        );
    }

    /// fails if the prompt loses its place at the top of the bar. A question
    /// awaiting an answer outranks a report of something that already
    /// happened, which outranks a persistent state the title also signals
    /// (A18).
    #[test]
    fn the_confirm_outranks_a_notice_and_the_filter_line() {
        let app = armed_app_with_a_filter_and_a_notice();
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("stop api (id 2)?"), "got {bar:?}");
        assert!(
            !bar.contains("filter \""),
            "the filter line is below it: {bar:?}"
        );
    }

    /// fails if a refusal made while an action is in flight never reaches the
    /// screen. THIS is the test the four-slot ordering would have failed, and
    /// no reducer test can catch it: `arm`'s "one action is already in flight"
    /// is a `Notice`, so a bar that ranked the in-flight line above notices
    /// would set it, assert it in the reducer, and never draw it. The operator
    /// presses `R` while a stop is out, nothing on screen changes, and the
    /// dashboard has silently swallowed the answer to their own keypress.
    ///
    /// The second half is the same defect arriving from the bus rather than
    /// from a key: `Dropped`, `BusLagged` and `DaemonShutdown` would all be
    /// invisible for as long as an action was in flight.
    #[test]
    fn a_refusal_while_an_action_is_in_flight_reaches_the_bar() {
        let mut app = acting_app(ActionVerb::Stop);
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("one action is already in flight"),
            "the refusal is on the bar, not only in the reducer: {bar:?}"
        );

        let mut app = acting_app(ActionVerb::Stop);
        app.update(Msg::Event(BusEvent::DaemonShutdown));
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("the shepherd is shutting down"), "got {bar:?}");
    }

    /// fails if the in-flight line does not come back once the notice is
    /// cleared. The mirror of the test above, and what makes ranking the
    /// notice higher honest rather than lossy: the covering is transient, and
    /// the next keypress ends it.
    #[test]
    fn the_in_flight_line_comes_back_when_the_notice_clears() {
        let mut app = acting_app(ActionVerb::Stop);
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        app.update(Msg::Key(KeyPress::SelectDown));
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("stop api (id 2): sent, waiting for the shepherd"),
            "got {bar:?}"
        );
    }

    /// fails if the action keys are advertised behind a closed gate. This
    /// file's standing rule: a hint that needs a footnote to be true is an
    /// asterisk, not a hint.
    #[test]
    fn the_action_keys_are_advertised_only_when_the_gate_is_open() {
        let closed = rendered(&status_line(&filtered_app(""), 200));
        for key in ["x stop", "R restart", "L reload"] {
            assert!(
                !closed.contains(key),
                "{key} advertised read-only: {closed:?}"
            );
        }
        let open = rendered(&status_line(&allowed_app(), 200));
        for key in ["x stop", "R restart", "L reload"] {
            assert!(
                open.contains(key),
                "{key} missing when the gate is open: {open:?}"
            );
        }
        assert!(
            open.contains("/ filter"),
            "and the filter key survives both forms"
        );
    }
}
