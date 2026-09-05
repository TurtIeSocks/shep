//! The three chrome lines: the title, the link banner, and the status bar.
//!
//! Every sentence here is literal. The design language's standing rule is
//! that nothing about damage gets charming, and this file is where all of
//! shep's damage reporting on this screen lives — the frozen banner, the
//! drop notice, the refusal.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{
    ActionState, App, Control, InputMode, Link, RowKey, Settings, SettingsPrompt,
};
use super::super::pane::{ConfigPane, PanePending};
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

/// The bottom line: eight slots, highest priority first -- the settings
/// screen's own armed or in-flight edit, a dashboard armed confirm, the
/// settings screen's own free-text editor, the filter box while editing, a
/// notice, an in-flight action, the applied filter line, then the key hint
/// -- with the control state always rendered on the right. See the phase
/// plan's "Shapes the design named" #2 for why the box and the applied
/// filter line are two slots and not one. The editor slot sits ahead of the
/// filter box for the same reason the two confirms sit ahead of it:
/// `App::settings().and_then(Settings::typing)` is only ever `Some` while
/// the settings screen owns `InputMode::Text`, and the filter box's own
/// `App::filter` is untouched the whole time, so checking the editor first
/// is what keeps the two from ever answering the same keystroke with the
/// wrong sentence.
///
/// **The settings confirm moved here from the body pane.** It used to be
/// the last line `view::settings::content_lines` drew -- structurally the
/// first thing a short terminal's `.take(area.height)` truncation dropped,
/// which meant an operator could arm a candidate, see no change anywhere
/// on screen, and press Enter into an edit nothing showed them was coming.
/// This bar is a fixed row the layout never cuts, the same property the
/// dashboard's own sheep confirm (the slot below this one) already relies
/// on, so the fix is to give the settings screen's confirm the identical
/// treatment rather than teach the body pane to tier. The body still
/// echoes the same line beneath the table when there is room for it
/// (`content_lines`'s own doc), the same redundancy the free-text editor
/// slot below already has -- belt, not a second source of truth: both
/// read `Settings::pending`/`Settings::typing` directly.
#[must_use]
pub fn status_line(app: &App, width: u16) -> Line<'static> {
    let palette = app.palette();
    let (left, left_style) = if let Some(prompt) = app.settings().and_then(Settings::pending) {
        // Slot 1. The settings screen's own armed scalar or dog edit, or
        // its in-flight sentence once sent. Outranks everything below,
        // including the dashboard's own action confirm just underneath --
        // moot in practice, since `Msg::Settings`'s own `opening` arm
        // clears `self.action` the moment the screen opens and no
        // dashboard action can arm while it stays open (`on_key`'s own
        // settings short circuit), so the two can never actually compete
        // for this slot on the same frame.
        let text = if prompt.sent {
            format!("{}  sent, waiting for the shepherd", prompt.text)
        } else {
            format!("{}  enter confirms, any other key cancels", prompt.text)
        };
        (text, palette.attention())
    } else if let Some(prompt) = app.config_pane().and_then(pane_prompt) {
        // Slot 1b. The config pane's own armed edit or in-flight sentence.
        // Beside the settings screen's rather than under it for the reason
        // that one gives: this is a fixed row the layout never cuts, and
        // an operator who armed a config write must never be able to press
        // Enter into a change nothing showed them was coming. The two
        // cannot compete on one frame -- the pane and the settings screen
        // cannot both be open.
        let text = if prompt.sent {
            format!("{}  sent, waiting for the shepherd", prompt.text)
        } else {
            format!("{}  enter confirms, any other key cancels", prompt.text)
        };
        (text, palette.attention())
    } else if let Some((label, buffer)) = app.config_pane().and_then(pane_editor) {
        // The pane's own free-text editor, and the env sub-screen's, ahead
        // of the filter branch below for exactly the reason the settings
        // editor's own slot is: all of them share `InputMode::Text`, and a
        // bar that fell through would render the dashboard's untouched
        // query under the label `filter` while the pane draws something
        // else entirely on the same frame.
        (
            format!("{label}  {buffer}\u{258f}   enter applies   esc cancels"),
            palette.attention(),
        )
    } else if let Some(action) = app.action().filter(|a| !a.sent) {
        // Slot 2. A18: a question awaiting an answer outranks everything,
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
        // Slot 5. BELOW the notice, and that is load-bearing rather than a
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
    } else if let Some(pane) = app.config_pane() {
        // The pane owns the keyboard, so neither the filter line nor either
        // dashboard hint is true while it is up. Its own form, for the
        // reason this file's standing rule gives: a dashboard hint naming
        // `x stop` beside a pane where `x` does nothing is exactly the
        // asterisk that rule forbids. Four forms now rather than one: the
        // pane writes, so `space` and `enter` are named only where the gate
        // actually lets them act, and the env sub-screen's own keys are a
        // different set again.
        (
            pane_hint(app.control(), pane.env().is_some()).to_string(),
            palette.muted(),
        )
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

/// The config pane's armed or in-flight question, or [`None`].
///
/// Reuses [`SettingsPrompt`] rather than declaring a second two-field
/// struct that says the same thing: the bar asks one question of both
/// screens -- what is the sentence, and has it gone out.
fn pane_prompt(pane: &ConfigPane) -> Option<SettingsPrompt<'_>> {
    match pane.pending_edit()? {
        PanePending::Armed { text, .. } => Some(SettingsPrompt { text, sent: false }),
        PanePending::Sent { text } => Some(SettingsPrompt { text, sent: true }),
        PanePending::Typing { .. } => None,
    }
}

/// What the pane's open editor is labelled, and what is in it.
///
/// Two editors, one slot: a field edit is labelled with the field, an env
/// edit with `env` and the key, and the env sub-screen's `+ new` row with
/// the grammar it wants, since there is no key yet to name.
fn pane_editor(pane: &ConfigPane) -> Option<(String, &str)> {
    if let Some(env) = pane.env() {
        return match env.typing()? {
            (Some(key), buffer) => Some((format!("env {key} ="), buffer)),
            (None, buffer) => Some(("new env KEY=value".to_owned(), buffer)),
        };
    }
    match pane.pending_edit()? {
        PanePending::Typing { key, buffer } => Some((format!("editing {key}"), buffer.as_str())),
        PanePending::Armed { .. } | PanePending::Sent { .. } => None,
    }
}

/// The config pane's own key hint.
///
/// Four forms, not one, and for this file's standing rule rather than for
/// completeness: a hint that needs a footnote to be true is an asterisk.
/// `space cycle` and `enter edit` are named only under
/// [`Control::Allowed`], because under [`Control::ReadOnly`] both keys
/// refuse -- the same reason `hint_for` omits `x`, `R` and `L` from the
/// dashboard's read-only form.
///
/// The env sub-screen gets its own pair because its keys are a different
/// set: `esc` backs out to the field list rather than closing the pane, and
/// `g/G` and `r` drop out to make room for what `enter` does there.
///
/// The read-only field-list form is byte-identical to what shipped before
/// the pane could write, so nothing that measured it has moved.
const fn pane_hint(control: Control, env_open: bool) -> &'static str {
    match (control, env_open) {
        (Control::ReadOnly, false) => {
            "esc/e close   j/k select   g/G first/last   r refresh   q quit"
        }
        (Control::Allowed, false) => {
            "esc/e close   j/k select   r refresh   space cycle   enter edit   q quit"
        }
        (Control::ReadOnly, true) => "esc back   e close   j/k select   r refresh   q quit",
        (Control::Allowed, true) => {
            "esc back   e close   j/k select   r refresh   enter set   q quit"
        }
    }
}

/// The key hint.
///
/// Three forms now: the settings screen's own, and the dashboard's two. The
/// config pane's is [`PANE_HINT`] above and is not one of these -- it is a
/// constant rather than a branch here, because that screen has one form and
/// `hint_for`'s whole subject is choosing between several.
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
/// The settings forms below follow the same rule against each other: the
/// read-only one is a prefix of the control one, and the two edit keys are
/// appended rather than inserted.
///
/// `e edit` is appended after `s settings` under that same rule, and is two
/// characters shorter than `e config` deliberately: at 84 the control form
/// still renders whole at 100 columns, and at 86 it would need 102, so an
/// operator on a 100-column terminal with the gate open would lose the tail
/// of the one word that says the key exists.
///
/// The settings screen takes `control` for the same reason the dashboard
/// does, which it did not until a whole-branch review caught it: the
/// argument was taken and then thrown away on the `settings_open` branch,
/// so a read-only lookout was told `space cycle` about a key that refuses.
/// That is exactly the asterisk the rule above forbids, and the dashboard
/// omits `x`, `R` and `L` for the same reason.
fn hint_for(control: Control, settings_open: bool) -> String {
    if settings_open {
        // `esc/s close` names both keys that close the screen, which is how
        // `s` gets said here at all -- on this screen `s` is the close key,
        // not the open one. `r` and `Enter` were missing outright.
        return match control {
            Control::ReadOnly => "esc/s close   j/k select   g/G first/last   r refresh   q quit",
            Control::Allowed => {
                "esc/s close   j/k select   g/G first/last   r refresh   space cycle   enter apply   q quit"
            }
        }
        .to_string();
    }
    match control {
        Control::ReadOnly => {
            "q quit   j/k select   g/G first/last   r refresh   / filter   s settings   e edit"
        }
        // `g/G` and `r` drop out to make room. They are the two an operator
        // rediscovers by pressing them; an action key is not.
        Control::Allowed => {
            "q quit   j/k select   / filter   x stop   R restart   L reload   s settings   e edit"
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
        acting_app, allowed_app, app_in_settings, app_in_settings_on, app_in_settings_with_control,
        armed_app, armed_app_with_a_filter_and_a_notice, editing_app, filtered_app, rendered,
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

    /// fails if the settings screen advertises its edit keys behind a
    /// closed gate, or stops naming the three keys it binds and never
    /// mentioned.
    ///
    /// The same rule as the test above, on the screen that ignored it:
    /// `hint_for` took `control` and discarded it whenever the settings
    /// screen was open, so a read-only lookout read `space cycle` next to
    /// the word `read-only` on the same line, about a key that refuses.
    /// The published gallery pinned it.
    ///
    /// `Enter`, `r` and `s` are all bound on that screen and none of them
    /// was named. `s` arrives as `esc/s close`, because on this screen `s`
    /// is the key that closes rather than the one that opens.
    #[test]
    fn the_settings_edit_keys_are_advertised_only_when_the_gate_is_open() {
        let closed = rendered(&status_line(&app_in_settings(), 200));
        for key in ["space cycle", "enter apply"] {
            assert!(
                !closed.contains(key),
                "{key} advertised read-only: {closed:?}"
            );
        }
        let open = rendered(&status_line(&app_in_settings_with_control(), 200));
        for key in ["space cycle", "enter apply"] {
            assert!(
                open.contains(key),
                "{key} missing when the gate is open: {open:?}"
            );
        }
        for both in [&closed, &open] {
            assert!(both.contains("esc/s close"), "got {both:?}");
            assert!(both.contains("r refresh"), "got {both:?}");
        }
    }

    /// fails if `q` stops being named on the settings screen's own status
    /// bar -- `App` handles it there (`app.rs`'s settings key dispatch),
    /// same as on the dashboard, but neither settings hint form said so.
    #[test]
    fn q_quit_is_named_on_the_settings_screen_in_both_control_states() {
        let closed = rendered(&status_line(&app_in_settings(), 200));
        let open = rendered(&status_line(&app_in_settings_with_control(), 200));
        for hint in [&closed, &open] {
            assert!(hint.contains("q quit"), "got {hint:?}");
        }
    }

    /// The pane's cursor, walked onto `key` the way an operator walks it.
    fn pane_to(app: &mut App, key: &str) {
        let index = app
            .config_pane()
            .expect("the pane is open")
            .fields()
            .fields()
            .iter()
            .position(|field| field.key == key)
            .unwrap_or_else(|| panic!("no field named {key}"));
        app.update(Msg::Key(KeyPress::SelectFirst));
        for _ in 0..index {
            app.update(Msg::Key(KeyPress::SelectDown));
        }
    }

    /// fails if the pane advertises its edit keys behind a closed gate, the
    /// same rule the settings screen already carries. The pane used to be
    /// read-only, so its hint had one form; `space` and `enter` refuse
    /// under `--allow-control`'s absence and must not be named there.
    #[test]
    fn the_panes_edit_keys_are_advertised_only_when_the_gate_is_open() {
        let closed = rendered(&status_line(
            &super::super::fixtures::app_in_sheep_pane(),
            200,
        ));
        let open = rendered(&status_line(
            &super::super::fixtures::app_in_sheep_pane_with_control(),
            200,
        ));
        for key in ["space cycle", "enter edit"] {
            assert!(
                !closed.contains(key),
                "{key} advertised read-only: {closed:?}"
            );
            assert!(
                open.contains(key),
                "{key} missing with the gate open: {open:?}"
            );
        }
        for both in [&closed, &open] {
            assert!(both.contains("esc/e close"), "got {both:?}");
            assert!(both.contains("q quit"), "got {both:?}");
            assert!(!both.contains("x stop"), "got {both:?}");
        }
    }

    /// fails if the env sub-screen keeps the field list's own hint, whose
    /// `esc/e close` is wrong there: `esc` backs out to the field list.
    #[test]
    fn the_env_sub_screen_says_esc_backs_out_rather_than_closes() {
        let mut app = super::super::fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "env");
        app.update(Msg::Key(KeyPress::Confirm));
        let bar = rendered(&status_line(&app, 200));
        assert!(bar.contains("esc back"), "got {bar:?}");
        assert!(!bar.contains("esc/e close"), "got {bar:?}");
        assert!(bar.contains("enter set"), "got {bar:?}");
    }

    /// fails if an armed config write stops reaching the one row the layout
    /// never cuts. The settings screen's own confirm moved here for exactly
    /// this reason: an operator must never press Enter into a change
    /// nothing on screen showed them was coming.
    #[test]
    fn an_armed_pane_edit_reaches_the_status_bar_and_says_so_once_sent() {
        let mut app = super::super::fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        app.update(Msg::Key(KeyPress::Cycle));
        let armed = rendered(&status_line(&app, 200));
        assert!(armed.contains("set autorestart = false"), "got {armed:?}");
        assert!(armed.contains("enter confirms"), "got {armed:?}");

        app.update(Msg::Key(KeyPress::Confirm));
        let sent = rendered(&status_line(&app, 200));
        assert!(sent.contains("set autorestart = false"), "got {sent:?}");
        assert!(
            sent.contains("sent, waiting for the shepherd"),
            "got {sent:?}"
        );
    }

    /// fails if either of the pane's two editors falls through to the
    /// filter box's own line, which would render the dashboard's untouched
    /// query under the label `filter` while the pane draws something else
    /// on the same frame.
    #[test]
    fn the_panes_editors_get_their_own_status_line_rather_than_the_filters() {
        let mut app = super::super::fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "cwd");
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Key(KeyPress::TextChar('x')));
        let field = rendered(&status_line(&app, 200));
        assert!(field.contains("editing cwd"), "got {field:?}");
        assert!(!field.contains("filter"), "got {field:?}");
        app.update(Msg::Key(KeyPress::TextAbandon));

        pane_to(&mut app, "env");
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Key(KeyPress::TextChar('y')));
        let env = rendered(&status_line(&app, 200));
        assert!(env.contains("env DB_HOST ="), "got {env:?}");
        assert!(env.contains('y'), "got {env:?}");
        assert!(!env.contains("filter"), "got {env:?}");
    }

    /// fails if the status bar keeps offering the dashboard's own keys
    /// while the config pane owns the keyboard. `x stop` in particular: it
    /// does nothing from in there, and this file's standing rule is that a
    /// hint needing a footnote is an asterisk rather than a hint.
    #[test]
    fn the_config_pane_gets_its_own_key_hint() {
        let app = super::super::fixtures::app_in_sheep_pane();
        let bar = status_line(&app, 120).to_string();
        assert!(bar.contains("esc/e close"), "got {bar:?}");
        assert!(bar.contains("r refresh"), "got {bar:?}");
        assert!(!bar.contains("x stop"), "got {bar:?}");
        assert!(!bar.contains("s settings"), "got {bar:?}");
    }
}
