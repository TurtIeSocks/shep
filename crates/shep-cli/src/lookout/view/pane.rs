//! Drawing a [`ConfigPane`]: a title naming the target, the field set's
//! groups as section headers, one row per field, and a cost column saying
//! what changing that field would cost.
//!
//! The layout is [`super::settings`]'s: both screens own the whole body
//! between the title and the status bar, both have more rows than a
//! terminal has lines, and both pay for chrome the viewport cannot see.
//! The scroll walk is shared ([`super::scroll::to_cursor`]); the layout
//! below is this pane's own, since a field list under four headers and a
//! settings screen with a dogs table share almost no lines.
//!
//! A sheep pane is 39 rows plus a title, four headers and three blank
//! separators: eight lines of chrome before a marker is paid for.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use shep_core::config::ApplyGroup;

use super::super::app::App;
use super::super::pane::{ConfigPane, EnvPane, EnvRow, Lock, PanePending, PaneRow, PaneTarget};
use super::super::theme::Palette;
use super::flock::{fit, mark};
use super::scroll::Attempt;

/// The columns every line spends on the selection mark and the space after
/// it, before any cell is drawn. [`super::settings::GUTTER`]'s twin, and it
/// exists for the reason that one does: a budget that forgets it is a budget
/// every line overruns.
const GUTTER: u16 = 2;

/// The KEY cell at its full width, flag character included. Twenty-six is
/// `exp_backoff_restart_delay` plus its flag, the longest key the Flockfile
/// schema declares, so no field name is truncated at a width that can
/// afford the whole column.
const KEY_W: u16 = 26;

/// The floor KEY shrinks to before the COST column is dropped instead.
const KEY_MIN: u16 = 8;

/// The floor VALUE shrinks to. Below this the pane drops COST, and below
/// that it draws KEY alone.
const VALUE_MIN: u16 = 8;

/// The COST cell. Ten columns, which is exactly `next start`, the longest
/// word [`cost_label`] prints.
const COST_W: u16 = 10;

/// The narrowest body that still draws KEY, VALUE and COST.
const FULL_WIDTH: u16 = KEY_W + 2 + VALUE_MIN + 2 + COST_W;

/// The narrowest body that still draws a VALUE beside the KEY.
const VALUE_WIDTH: u16 = KEY_MIN + 2 + VALUE_MIN;

/// The width the rows are laid out in: the terminal minus [`GUTTER`].
const fn body_width(width: u16) -> u16 {
    width.saturating_sub(GUTTER)
}

/// What a change to a field in `group` costs, in an operator's words.
///
/// A prediction from the field's class, not the outcome of a specific
/// write: `watch` says `now` but can park, and `autostart` says `next
/// start` but takes effect at muster. The status bar reports what
/// actually happened; the row's `!` flag is the durable answer.
///
/// [`ApplyGroup`] is `#[non_exhaustive]`, so an untaught field falls back
/// to `respawn`, the most conservative of the four.
const fn cost_label(group: ApplyGroup) -> &'static str {
    match group {
        ApplyGroup::Live => "now",
        ApplyGroup::NextSpawn => "next start",
        ApplyGroup::Structural => "read-only",
        ApplyGroup::NeedsRespawn | _ => "respawn",
    }
}

/// The three cell widths for a body of `width`: KEY, VALUE, COST. A zero
/// means the column is not drawn at all.
///
/// The three always sum, with their two-space separators, to exactly
/// `width`, so no line can overrun the terminal it was laid out for.
/// COST goes first when the terminal narrows: arming a field repeats its
/// cost verbatim in the status bar, which is the same reasoning
/// [`super::settings`] gives for dropping its own cost cell first.
fn widths(width: u16) -> (u16, u16, u16) {
    if width >= FULL_WIDTH {
        let rest = width - COST_W - 2;
        let key = KEY_W.min(rest - VALUE_MIN - 2);
        (key, rest - key - 2, COST_W)
    } else if width >= VALUE_WIDTH {
        let key = KEY_W.min(width - VALUE_MIN - 2);
        (key, width - key - 2, 0)
    } else {
        (width, 0, 0)
    }
}

/// A section header, indented to match every field row's own mark-and-gap
/// prefix, the same as [`super::settings`]'s own.
fn section_header(label: &str, palette: Palette) -> Line<'static> {
    Line::from(Span::styled(format!("  {label}"), palette.muted()))
}

/// The pane's own title: which sheep or dog is being edited.
///
/// The dashboard's title line above this one names `$SHEP_HOME` and
/// nothing else, so this names whose 39 fields are on screen.
///
/// Carries no control-dependent word: what the keys do belongs in the key
/// hint (`view::status::pane_hint`), which already reads the gate.
fn title_line(pane: &ConfigPane, palette: Palette, width: u16) -> Line<'static> {
    let kind = match pane.target() {
        PaneTarget::Sheep { .. } => "sheep config",
        PaneTarget::Dog { .. } => "dog config",
    };
    Line::from(Span::styled(
        format!(
            "  {}",
            fit(
                &format!("{}  ({kind})", pane.target().name()),
                body_width(width)
            )
        ),
        palette.muted(),
    ))
}

/// One field's row: the selection mark, a lock glyph, a flag, the key, the
/// value and what changing it costs.
///
/// The flag matches `shep flock`'s CFG column: `!` for a field parked
/// until a respawn, `*` for one an operator has overridden. Pending wins
/// when both apply, since the value on screen is not what the running
/// child holds.
///
/// The lock is a glyph, not a style, since a style says nothing in
/// `plain`. It sits between the mark and the flag rather than in the cost
/// cell, since cost is the first column [`widths`] drops. See [`Lock`].
fn field_line(
    pane: &ConfigPane,
    index: usize,
    selected: bool,
    width: u16,
    palette: Palette,
) -> Line<'static> {
    let Some(field) = pane.fields().fields().get(index) else {
        return Line::default();
    };
    let (key_w, value_w, cost_w) = widths(body_width(width));
    let flag = match (pane.is_pending(&field.key), pane.is_overridden(&field.key)) {
        (true, _) => '!',
        (false, true) => '*',
        (false, false) => ' ',
    };
    // A secret's value is never rendered, only whether there is one. The
    // Flockfile schema marks nothing secret today; a dog's own schema can,
    // and this pane draws both.
    let raw = pane.value(&field.key);
    // An editor open on this field replaces the cell with what is being
    // typed, cursor included. The same `\u{258f}` every text box in
    // lookout draws; a character rather than a reversed cell because the
    // ANSI gallery renders foregrounds only.
    let typing = match pane.pending_edit() {
        Some(PanePending::Typing { key, buffer }) if *key == field.key => Some(buffer),
        _ => None,
    };
    let value = match typing {
        Some(buffer) => format!("{buffer}\u{258f}"),
        None if field.secret && raw != "(unset)" => "<set>".to_owned(),
        None => raw,
    };

    let lock = match pane.lock(&field.key) {
        // Fixed: no surface edits this one, not just this pane.
        Some(Lock::Refused) => '=',
        // Shown only. A Flockfile still writes it, and the cost cell beside
        // it reports what doing so would cost.
        Some(Lock::NoWidget) => '~',
        None => ' ',
    };
    let mut text = format!("{}{lock}", mark(selected));
    text.push_str(&fit(&format!("{flag}{}", field.key), key_w));
    if value_w > 0 {
        text.push_str("  ");
        text.push_str(&fit(&value, value_w));
    }
    if cost_w > 0 {
        text.push_str("  ");
        let cost = pane.cost(&field.key).map_or("", cost_label);
        text.push_str(&fit(cost, cost_w));
    }
    // Muting reinforces the glyph but carries no fact alone: a `plain`
    // palette renders muted as nothing, so style could not tell a locked
    // row from an editable one on its own.
    if field.editable {
        Line::from(Span::raw(text))
    } else {
        Line::from(Span::styled(text, palette.muted()))
    }
}

/// The question an armed edit reads as, or the one already sent. [`None`]
/// while nothing is in flight and while an editor is open, since an editor
/// draws in its own field's row instead ([`field_line`]).
fn confirm_text(pane: &ConfigPane) -> Option<&str> {
    match pane.pending_edit()? {
        PanePending::Armed { text, .. } | PanePending::Sent { text, .. } => Some(text),
        PanePending::Typing { .. } => None,
    }
}

/// The env sub-screen: the sheep's env key names, and a row to add one on.
///
/// Values are never drawn: `Request::SheepConfig` answers with keys alone,
/// so every key reads `<set>` regardless of what the pane knows. That is a
/// property of the wire, not a truncation, and the title says so.
///
/// Laid out through [`super::scroll::to_cursor`], the same walk the field
/// list uses (one uniform row, no group headers), so the cursor is drawn
/// at every height this pane claims to support.
fn env_lines(
    pane: &ConfigPane,
    env: &EnvPane,
    palette: Palette,
    width: u16,
    budget: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "  {}",
            fit(
                &format!("{}  env (write-only)", pane.target().name()),
                body_width(width)
            )
        ),
        palette.muted(),
    ))];
    let mut body_budget = budget - 1;
    // The same echo the field list draws under its own title, and the same
    // belt-not-a-second-source-of-truth argument: `view::status` puts this
    // sentence on a row the layout never cuts, and both read
    // `ConfigPane::pending_edit`.
    if let Some(text) = confirm_text(pane)
        && body_budget > 0
    {
        lines.push(Line::from(Span::styled(
            format!("  {}", fit(text, body_width(width))),
            palette.attention(),
        )));
        body_budget -= 1;
    }
    if body_budget == 0 {
        return lines;
    }
    let rows = env.rows();
    let cursor_row = env.view().cursor().min(rows.len().saturating_sub(1));
    lines.extend(super::scroll::to_cursor(
        cursor_row,
        env.view().offset(),
        |offset| env_body_from(env, palette, width, body_budget, offset),
        || vec![env_line(env, cursor_row, true, width, palette)],
    ));
    lines
}

/// One row of the env sub-screen: the selection mark, the key, and `<set>`.
///
/// The `+ new` row carries no value cell: there is no key yet for one to
/// belong to.
fn env_line(
    env: &EnvPane,
    index: usize,
    selected: bool,
    width: u16,
    palette: Palette,
) -> Line<'static> {
    let (key_w, value_w, _) = widths(body_width(width));
    let typed = selected
        .then(|| env.typing())
        .flatten()
        .map(|(_, buffer)| buffer);
    let (key, value) = match env.rows().get(index).copied() {
        Some(EnvRow::Key(key_index)) => (
            env.keys().get(key_index).cloned().unwrap_or_default(),
            typed.map_or_else(|| "<set>".to_owned(), |buffer| format!("{buffer}\u{258f}")),
        ),
        // On `+ new` the buffer is the whole `KEY=value`, so it replaces
        // the key cell rather than the value cell.
        Some(EnvRow::New) => match typed {
            Some(buffer) => (format!("{buffer}\u{258f}"), String::new()),
            None => ("+ new".to_owned(), String::new()),
        },
        None => return Line::default(),
    };
    let mut text = format!("{} ", mark(selected));
    text.push_str(&fit(&key, key_w));
    if value_w > 0 && !value.is_empty() {
        text.push_str("  ");
        text.push_str(&fit(&value, value_w));
    }
    if matches!(env.rows().get(index), Some(EnvRow::New)) {
        return Line::from(Span::styled(text, palette.muted()));
    }
    Line::from(Span::raw(text))
}

/// Lays the sub-screen's body out from row `offset`, spending at most
/// `budget` lines. Both markers are reserved before a row is admitted, the
/// same rule [`body_from`] follows.
fn env_body_from(
    env: &EnvPane,
    palette: Palette,
    width: u16,
    budget: usize,
    offset: usize,
) -> Attempt {
    let rows = env.rows();
    let total = rows.len();
    let cursor_row = env.view().cursor().min(total.saturating_sub(1));
    let above = usize::from(offset > 0);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut drawn = 0usize;
    for index in offset..total {
        if lines.len() + 1 + above + usize::from(index + 1 < total) > budget {
            break;
        }
        lines.push(env_line(env, index, index == cursor_row, width, palette));
        drawn += 1;
    }
    let hidden_below = total.saturating_sub(offset + drawn);
    if hidden_below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ... {hidden_below} below"),
            palette.muted(),
        )));
    }
    if offset > 0 {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("  ... {offset} above"),
                palette.muted(),
            )),
        );
    }
    Attempt {
        cursor_drawn: drawn > 0 && (offset..offset + drawn).contains(&cursor_row),
        lines,
    }
}

/// Every line of the pane, top to bottom, laid out for a terminal `height`
/// rows tall.
///
/// `height` counts lines, and no more than that ever come back. Zero means
/// unlimited, which is what a test with no terminal behind it gets. See
/// [`super::scroll`] for why the viewport's offset is a starting point
/// here, not an answer.
#[must_use]
pub fn pane_lines(
    pane: &ConfigPane,
    palette: Palette,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let budget = if height == 0 {
        usize::MAX
    } else {
        usize::from(height)
    };
    if budget == 0 {
        return Vec::new();
    }
    if let Some(env) = pane.env() {
        return env_lines(pane, env, palette, width, budget);
    }
    let mut lines = vec![title_line(pane, palette, width)];
    // The title is unconditional, so the body is laid out against what is
    // left after it. An empty form (unreachable for a sheep, whose schema
    // is a committed file with 39 properties, but a dog answers `--schema`
    // for itself) leaves the title as the whole pane.
    let mut body_budget = budget - 1;
    // The confirm echoed under the title, the same redundancy the settings
    // screen has: `view::status` draws the same sentence on a fixed row,
    // and both read `ConfigPane::pending_edit`. Subtracted from the
    // budget rather than appended, per `body_from`'s own doc on markers.
    if let Some(text) = confirm_text(pane)
        && body_budget > 0
    {
        lines.push(Line::from(Span::styled(
            format!("  {}", fit(text, body_width(width))),
            palette.attention(),
        )));
        body_budget -= 1;
    }
    // The one line a dog pane has that a sheep pane does not: shep does not
    // know what a dog's field costs, so every row's COST cell is empty.
    // Reserved out of the budget before rows are laid out, for the same
    // reason the confirm echo is: a footer appended afterwards is a line
    // nothing counted.
    let footer = match pane.target() {
        PaneTarget::Sheep { .. } => None,
        PaneTarget::Dog { name, .. } => (body_budget > 0)
            .then(|| format!("shep publishes the change; {name} decides what to reload")),
    };
    if footer.is_some() {
        body_budget -= 1;
    }
    if !pane.fields().is_empty() && body_budget > 0 {
        let total = pane.rows().len();
        let cursor_row = pane.view().cursor().min(total - 1);
        lines.extend(super::scroll::to_cursor(
            cursor_row,
            pane.view().offset(),
            |offset| body_from(pane, palette, width, body_budget, offset),
            || cursor_only(pane, palette, width, body_budget, cursor_row),
        ));
    }
    if let Some(text) = footer {
        lines.push(Line::from(Span::styled(
            format!("  {}", fit(&text, body_width(width))),
            palette.muted(),
        )));
    }
    lines
}

/// Lays the body out from field `offset`, spending at most `budget` lines.
///
/// Every line pushed is counted, including section headers, blank
/// separators between them and both markers. The two markers are reserved
/// before a row is admitted rather than appended afterwards, so a height
/// that binds cuts a row instead of the sentence saying a row was cut.
fn body_from(
    pane: &ConfigPane,
    palette: Palette,
    width: u16,
    budget: usize,
    offset: usize,
) -> Attempt {
    let rows = pane.rows();
    let total = rows.len();
    let cursor_row = pane.view().cursor().min(total.saturating_sub(1));
    // The `... N above` marker is inserted at the top once everything under
    // it is laid out, so its line is held back from the very first check.
    let above = usize::from(offset > 0);
    // Whether a row at `index` still leaves room for `need` more lines. The
    // `... N below` marker is only owed when a row follows this one: a row
    // that fills the last line with nothing under it needs no marker.
    let room = |taken: usize, need: usize, index: usize| {
        taken + need + above + usize::from(index + 1 < total) <= budget
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_group: Option<&str> = None;
    // A group's header, and the blank line ahead of it for every group after
    // the first, held here rather than pushed straight away: it is pushed
    // alongside the first row of its group that survives the offset skip,
    // so a window opening in the middle of `control` still says `control`.
    let mut pending_header: Vec<Line<'static>> = Vec::new();
    let mut drawn = 0usize;

    for (index, row) in rows.iter().enumerate() {
        let PaneRow::Field(field_index) = *row;
        let group = pane
            .fields()
            .fields()
            .get(field_index)
            .and_then(|field| field.group.as_deref());
        if current_group != group {
            let mut header = Vec::new();
            if current_group.is_some() {
                header.push(Line::default());
            }
            if let Some(group) = group {
                header.push(section_header(group, palette));
            }
            pending_header = header;
            current_group = group;
        }
        if index < offset {
            continue;
        }
        if !room(lines.len(), pending_header.len() + 1, index) {
            break;
        }
        lines.append(&mut pending_header);
        lines.push(field_line(
            pane,
            field_index,
            pane.cursor() == Some(*row),
            width,
            palette,
        ));
        drawn += 1;
    }

    // Counted off what this pass actually drew, not off the viewport's own
    // arithmetic: the viewport hides rows against a line budget it cannot
    // see spent, so its answer and this one disagree the moment the chrome
    // costs anything.
    let hidden_below = total.saturating_sub(offset + drawn);
    if hidden_below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ... {hidden_below} below"),
            palette.muted(),
        )));
    }
    if offset > 0 {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("  ... {offset} above"),
                palette.muted(),
            )),
        );
    }

    Attempt {
        cursor_drawn: drawn > 0 && (offset..offset + drawn).contains(&cursor_row),
        lines,
    }
}

/// The cursor's own row, alone, for a body too short to hold the chrome its
/// group costs.
///
/// The last resort, reached only when every offset down to the cursor's own
/// left it undrawn. A group's first row costs a blank line and a header
/// above it before it may be drawn at all, and the two markers on top of
/// that: four lines for one row, where `view::MIN_HEIGHT` leaves this pane
/// three after its title. A pane that declares a minimum height should draw
/// something at it, and the selected row is the something.
///
/// Markers are added around it while they fit, the cursor's row first: it is
/// the one line this function exists to guarantee.
fn cursor_only(
    pane: &ConfigPane,
    palette: Palette,
    width: u16,
    budget: usize,
    cursor_row: usize,
) -> Vec<Line<'static>> {
    let rows = pane.rows();
    let mut lines = Vec::new();
    if let Some(PaneRow::Field(index)) = rows.get(cursor_row).copied() {
        lines.push(field_line(pane, index, true, width, palette));
    }
    let hidden_below = rows.len().saturating_sub(cursor_row + 1);
    if cursor_row > 0 && lines.len() < budget {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("  ... {cursor_row} above"),
                palette.muted(),
            )),
        );
    }
    if hidden_below > 0 && lines.len() < budget {
        lines.push(Line::from(Span::styled(
            format!("  ... {hidden_below} below"),
            palette.muted(),
        )));
    }
    lines
}

/// Draws the pane into `area`, straight into `buffer`.
pub fn draw_pane(app: &App, pane: &ConfigPane, area: Rect, buffer: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for (offset, line) in pane_lines(pane, app.palette(), area.width, area.height)
        .iter()
        .enumerate()
        .take(usize::from(area.height))
    {
        let offset = u16::try_from(offset).unwrap_or(0);
        buffer.set_line(area.x, area.y + offset, line, area.width);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::fixtures;
    use super::*;
    use crate::lookout::app::{Effect, KeyPress, Msg};
    use crate::lookout::frames::render_text;
    use crate::output::width::visible_width;

    /// The pane the rest of this module renders: `web`, with two overridden
    /// fields, one pending and two env keys.
    fn web_pane() -> ConfigPane {
        ConfigPane::sheep(fixtures::sheep_config_view())
    }

    /// Every line as a plain string, styles dropped.
    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    /// The whole pane at a comfortable width, unbounded. The snapshot is the
    /// assertion: it pins the title, the four section headers in order, all
    /// 39 rows, the two flags and the cost cell beside each one.
    #[test]
    fn a_sheep_pane_at_a_comfortable_width() {
        let lines = pane_lines(&web_pane(), fixtures::plain(), 120, 0);
        insta::assert_snapshot!("sheep_pane_wide", text_of(&lines).join("\n"));
    }

    #[test]
    fn a_sheep_pane_scrolled_to_the_cron_section_labels_it() {
        let mut pane = web_pane();
        pane.set_rows(8);
        pane.move_to_last();
        let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, 9));
        assert!(text.len() <= 9, "{text:?}");
        assert!(text.iter().any(|line| line.contains("above")), "{text:?}");
        assert!(
            text.iter().any(|line| line.trim() == "cron"),
            "the visible section is labelled: {text:?}"
        );
        assert!(!text.iter().any(|line| line.contains("below")), "{text:?}");
    }

    /// `Buffer::set_line` clips in silence, so an overrun renders as a
    /// truncated cost cell with nothing saying it was cut.
    #[test]
    fn every_pane_line_fits_the_width_it_was_drawn_for() {
        let pane = web_pane();
        for width in super::super::MIN_TERM_WIDTH..=200 {
            for line in text_of(&pane_lines(&pane, fixtures::plain(), width, 0)) {
                assert!(
                    visible_width(&line) <= usize::from(width),
                    "width {width} drew {}: {line:?}",
                    visible_width(&line)
                );
            }
        }
    }

    /// Checks `args` too, since shep writes it happily: muting must mark
    /// both reasons a row can be locked, not just the structural one.
    #[test]
    fn a_structural_field_renders_muted_and_the_cost_column_says_why() {
        let pane = web_pane();
        let lines = pane_lines(&pane, fixtures::coloured(), 120, 0);
        let instances = lines
            .iter()
            .find(|line| text_of(core::slice::from_ref(line))[0].contains("instances"))
            .expect("every field is drawn at 120 columns");
        assert!(
            text_of(core::slice::from_ref(instances))[0].contains("read-only"),
            "{instances:?}"
        );
        assert_eq!(
            instances.spans[0].style,
            fixtures::coloured().muted(),
            "a refused row is muted"
        );

        let args = lines
            .iter()
            .find(|line| text_of(core::slice::from_ref(line))[0].contains("~ args"))
            .expect("a field with no widget is drawn too");
        let rendered = text_of(core::slice::from_ref(args))[0].clone();
        assert!(
            rendered.contains("respawn") && !rendered.contains("read-only"),
            "shep writes `args`, so its cost is a real cost: {rendered:?}"
        );
        assert_eq!(
            args.spans[0].style,
            fixtures::coloured().muted(),
            "muting says `not from here`, which is true of both kinds"
        );
    }

    /// A field row split into its four fixed leading parts: the selection
    /// mark, the lock glyph, the flag and the key. Positional rather than a
    /// `starts_with`, which only ever matched unselected rows and so could
    /// not see a glyph on the one row the cursor was on.
    fn parts(line: &str) -> Option<(char, char, char, String)> {
        let mut chars = line.chars();
        let mark = chars.next()?;
        let lock = chars.next()?;
        let flag = chars.next()?;
        let key = chars.as_str().split_whitespace().next()?.to_string();
        (mark == '>' || mark == ' ').then_some((mark, lock, flag, key))
    }

    /// Every field row, as its four leading parts. Headers, markers, blanks
    /// and the title are dropped: none of them names a field.
    fn rows_of(text: &[String]) -> Vec<(char, char, char, String)> {
        let keys: Vec<String> = web_pane()
            .fields()
            .fields()
            .iter()
            .map(|field| field.key.clone())
            .collect();
        text.iter()
            .filter_map(|line| parts(line))
            .filter(|(_, _, _, key)| keys.contains(key))
            .collect()
    }

    #[test]
    fn the_flags_mark_exactly_the_overridden_and_pending_fields() {
        let text = text_of(&pane_lines(&web_pane(), fixtures::plain(), 120, 0));
        let flagged = |wanted: char| -> Vec<String> {
            rows_of(&text)
                .into_iter()
                .filter(|(_, _, flag, _)| *flag == wanted)
                .map(|(_, _, _, key)| key)
                .collect()
        };
        assert_eq!(flagged('*'), ["reuse_port", "max_restarts"]);
        assert_eq!(flagged('!'), ["kill_signal"]);
        assert_eq!(rows_of(&text).len(), 39, "every field is drawn at 120");
    }

    /// `=` is shep refusing the write outright; `~` is only this pane
    /// having no widget for the shape. Six fields shep writes happily
    /// carry `~`, so their cost cell must say `respawn` or `now`, never
    /// `read-only`.
    #[test]
    fn a_refused_field_and_one_the_pane_has_no_widget_for_get_different_glyphs() {
        let text = text_of(&pane_lines(&web_pane(), fixtures::plain(), 120, 0));
        let glyphed = |wanted: char| -> Vec<String> {
            rows_of(&text)
                .into_iter()
                .filter(|(_, lock, _, _)| *lock == wanted)
                .map(|(_, _, _, key)| key)
                .collect()
        };
        assert_eq!(glyphed('='), ["instances", "name"]);
        assert_eq!(
            glyphed('~'),
            [
                "args",
                "ignore_watch",
                "liveness_probe",
                "readiness_probe",
                "stop_exit_codes",
                "watch_options"
            ]
        );
        assert_eq!(glyphed(' ').len(), 39 - 2 - 6);
    }

    /// `MIN_TERM_WIDTH` drops the cost cell and `plain` renders muted as
    /// nothing, so the glyph is the whole signal in exactly the case an
    /// operator is most likely to be in.
    #[test]
    fn the_two_glyphs_survive_the_narrowest_width_and_a_palette_with_no_colour() {
        let mut pane = web_pane();
        pane.move_to_last();
        let width = super::super::MIN_TERM_WIDTH;
        let text = text_of(&pane_lines(&pane, fixtures::plain(), width, 0));
        let rows = rows_of(&text);
        assert!(
            !text[1..]
                .iter()
                .any(|line| parts(line).is_some() && line.contains("read-only")),
            "no field row can afford the cost cell at {width}: {text:?}"
        );
        let glyph = |key: &str| rows.iter().find(|(_, _, _, k)| k == key).map(|r| r.1);
        assert_eq!(glyph("instances"), Some('='));
        assert_eq!(glyph("args"), Some('~'));
        assert_eq!(glyph("watch"), Some(' '));
    }

    /// The whole frame at `height`, through the same `note_body_rows` and
    /// `draw` the event loop runs before each one.
    fn screen_at(app: &mut crate::lookout::app::App, height: u16) -> String {
        let area = Rect::new(0, 0, 120, height);
        app.note_body_rows(super::super::body_rows(area));
        let mut terminal = Terminal::new(TestBackend::new(120, height)).unwrap();
        terminal
            .draw(|frame| super::super::draw(app, frame))
            .unwrap();
        render_text(terminal.backend().buffer())
    }

    /// How many rows the frame marks as selected. One, always.
    fn marked(text: &str) -> usize {
        text.lines().filter(|line| line.starts_with('>')).count()
    }

    /// Guards against the class of bug where chrome eats the budget and
    /// the selected row is never drawn, while every static frame still
    /// looks right. Six is `view::MIN_HEIGHT`; three lines of body under
    /// the title is less than a group's first row costs, so those steps
    /// go through `cursor_only`.
    #[test]
    fn the_cursor_survives_every_step_of_a_walk_down_and_back_up() {
        for height in [6u16, 7, 8, 10, 14, 20, 45] {
            let mut app = fixtures::app_in_sheep_pane();
            let total = app.config_pane().unwrap().rows().len();
            for step in 0..=total {
                let text = screen_at(&mut app, height);
                assert_eq!(marked(&text), 1, "{height} rows, {step} down:\n{text}");
                app.update(Msg::Key(KeyPress::SelectDown));
            }
            for step in 0..=total {
                let text = screen_at(&mut app, height);
                assert_eq!(marked(&text), 1, "{height} rows, {step} up:\n{text}");
                app.update(Msg::Key(KeyPress::SelectUp));
            }
        }
    }

    /// The marker that says rows were cut would itself become the row
    /// that gets cut.
    #[test]
    fn the_body_never_outgrows_the_height_it_was_given() {
        let mut pane = web_pane();
        for height in 1..=60u16 {
            pane.set_rows(usize::from(height.saturating_sub(1)));
            for cursor in [0usize, 7, 20, 38] {
                pane.move_to_first();
                pane.move_by(isize::try_from(cursor).unwrap());
                let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, height));
                assert!(
                    text.len() <= usize::from(height),
                    "height {height}, cursor {cursor}: {text:?}"
                );
            }
        }
    }

    /// `view::status` draws the same sentence on a fixed row; this is the
    /// belt beside that brace, and both read `ConfigPane::pending_edit`.
    #[test]
    fn an_armed_edit_is_echoed_under_the_title() {
        let mut pane = web_pane();
        pane.move_to_key("autorestart");
        pane.cycle(Instant::now());
        let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, 0));
        assert!(text[1].contains("set autorestart = false"), "{:?}", text[1]);

        pane.take_armed(0);
        let sent = text_of(&pane_lines(&pane, fixtures::plain(), 120, 0));
        assert_eq!(
            sent[1], text[1],
            "the wording must not change between the question and its answer"
        );
    }

    /// The pane's cursor, walked onto `key` the way an operator walks it.
    fn pane_to(app: &mut crate::lookout::app::App, key: &str) {
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

    /// Both directions: `watch` is `Live`, draws `now`, and can still
    /// park. `autostart` is `NextSpawn`, draws `next start`, and takes
    /// effect at muster, not at the next spawn. The column is never
    /// corrected after a reply, since a reply covers one row of
    /// thirty-nine; it stays a prediction everywhere, the bar reports the
    /// outcome, and the row's `!` flag carries it afterwards.
    #[test]
    fn the_cost_column_predicts_and_the_status_bar_reports() {
        for (key, column, pending, sentence) in [
            ("watch", "now", true, "waits for `shep reload web`"),
            ("autostart", "next start", false, "is set"),
        ] {
            let mut app = fixtures::app_in_sheep_pane_with_control();
            pane_to(&mut app, key);

            let text = text_of(&pane_lines(
                app.config_pane().unwrap(),
                fixtures::plain(),
                120,
                0,
            ));
            let row = text
                .iter()
                .find(|line| {
                    line.split_whitespace().next() == Some(key)
                        || line
                            .get(3..)
                            .is_some_and(|rest| rest.split_whitespace().next() == Some(key))
                })
                .unwrap_or_else(|| panic!("{key} is drawn at 120 columns: {text:?}"));
            assert!(row.contains(column), "{key}: {row:?}");

            app.update(Msg::Key(KeyPress::Cycle));
            let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
                panic!("{key}: the confirm sends");
            };
            app.update(Msg::Replied {
                sent,
                result: Ok(shep_core::protocol::Response::SheepFieldSet {
                    name: "web".to_string(),
                    key: key.to_string(),
                    pending,
                }),
            });
            let bar = crate::lookout::view::status::status_line(&app, 200).to_string();
            assert!(bar.contains(sentence), "{key}: {bar:?}");
        }
    }

    /// The only place on screen that says which field the buffer belongs
    /// to.
    #[test]
    fn an_open_editor_draws_its_buffer_in_the_fields_own_row() {
        let mut pane = web_pane();
        pane.move_to_key("cwd");
        pane.begin_typing();
        for typed in "/srv".chars() {
            pane.type_char(typed);
        }
        let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, 0));
        let row = text
            .iter()
            .find(|line| line.contains(" cwd"))
            .expect("cwd is drawn at 120 columns");
        assert!(row.contains("/srv\u{258f}"), "{row:?}");
        assert!(
            !text.iter().any(|line| line.contains("set cwd")),
            "an editor is not a confirm: {text:?}"
        );
    }

    /// A dog pane over the bark dog, with a sink in its section.
    fn bark_pane() -> ConfigPane {
        let schema = crate::dog::builtin_schema("bark").expect("bark is a built-in");
        ConfigPane::dog(
            "bark".into(),
            None,
            schema,
            "poll = \"60s\"\nhistory_bytes = 4096\n\n[sinks.ops]\nkind = \"slack\"\nurl = \"https://hooks.example/x\"\n"
                .into(),
        )
    }

    /// The whole dog pane at a comfortable width. The snapshot is the
    /// assertion: the title says `dog config`, the rows are flat with no
    /// section headers, every COST cell is empty, and the foot says once
    /// that the dog decides.
    ///
    /// The three assertions ahead of it fail loudly rather than as a
    /// snapshot diff: a webhook URL on screen is the leak the whole secret
    /// contract exists to prevent.
    #[test]
    fn a_dog_pane_at_a_comfortable_width() {
        let text = text_of(&pane_lines(&bark_pane(), fixtures::plain(), 120, 0));
        assert!(
            !text.iter().any(|line| line.contains("hooks.example")),
            "a secret is never rendered: {text:?}"
        );
        assert!(text.iter().any(|line| line.contains("<set>")), "{text:?}");
        assert!(
            text.iter()
                .any(|line| line.contains("decides what to reload")),
            "{text:?}"
        );
        insta::assert_snapshot!("dog_pane_wide", text.join("\n"));
    }

    #[test]
    fn a_dog_panes_footer_is_paid_for_out_of_the_height_it_was_given() {
        let mut pane = bark_pane();
        for height in 1..=20u16 {
            pane.set_rows(usize::from(height.saturating_sub(1)));
            for cursor in [0usize, 2, 4] {
                pane.move_to_first();
                pane.move_by(isize::try_from(cursor).unwrap());
                let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, height));
                assert!(
                    text.len() <= usize::from(height),
                    "height {height}, cursor {cursor}: {text:?}"
                );
            }
        }
    }

    /// The footer is the newest line and the longest, and
    /// `Buffer::set_line` clips in silence.
    #[test]
    fn every_dog_pane_line_fits_the_width_it_was_drawn_for() {
        let pane = bark_pane();
        for width in super::super::MIN_TERM_WIDTH..=200 {
            for line in text_of(&pane_lines(&pane, fixtures::plain(), width, 0)) {
                assert!(
                    visible_width(&line) <= usize::from(width),
                    "width {width} drew {}: {line:?}",
                    visible_width(&line)
                );
            }
        }
    }

    /// The env sub-screen at a comfortable width. The snapshot is the
    /// assertion: the title says write-only, both of the fixture's keys are
    /// listed with `<set>` rather than a value, and the `+ new` row is last.
    #[test]
    fn the_env_sub_screen_at_a_comfortable_width() {
        let mut pane = web_pane();
        pane.open_env();
        let lines = pane_lines(&pane, fixtures::plain(), 120, 0);
        insta::assert_snapshot!("env_sub_screen", text_of(&lines).join("\n"));
    }

    #[test]
    fn an_armed_env_write_is_echoed_and_never_quotes_the_value() {
        let mut pane = web_pane();
        pane.open_env();
        pane.env_mut().unwrap().begin_typing();
        for typed in "hunter2".chars() {
            pane.env_mut().unwrap().type_char(typed);
        }
        let (key, value) = pane.env_mut().unwrap().apply_typing().unwrap();
        pane.arm_env(key, value.map(Into::into), Instant::now());
        let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, 0));
        assert!(text[1].contains("set env DB_HOST"), "{:?}", text[1]);
        assert!(!text.join("\n").contains("hunter2"), "{text:?}");
    }

    /// The shepherd sends keys with no values at all, so a rendered value
    /// could only have been invented.
    #[test]
    fn the_env_sub_screen_never_renders_a_value() {
        let mut pane = ConfigPane::sheep({
            let mut config = shep_core::config::AppConfig {
                name: "web".to_string(),
                ..Default::default()
            };
            config
                .env
                .insert("DB_PASSWORD".to_string(), "hunter2".to_string());
            shep_core::protocol::SheepConfigView::new(config, Vec::new(), Vec::new())
        });
        pane.open_env();
        let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, 0)).join("\n");
        assert!(text.contains("DB_PASSWORD"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("<set>"), "{text}");
    }

    #[test]
    fn the_env_cursor_survives_every_step_of_a_walk_down_and_back_up() {
        let mut pane = ConfigPane::sheep({
            let mut config = shep_core::config::AppConfig {
                name: "web".to_string(),
                ..Default::default()
            };
            for index in 0..20 {
                config
                    .env
                    .insert(format!("KEY_{index:02}"), "x".to_string());
            }
            shep_core::protocol::SheepConfigView::new(config, Vec::new(), Vec::new())
        });
        pane.open_env();
        for height in [1u16, 2, 3, 6, 8, 14, 30] {
            pane.env_mut().unwrap().move_to_first();
            let body = usize::from(height.saturating_sub(1));
            pane.env_mut().unwrap().set_rows(body);
            let total = pane.env().unwrap().rows().len();
            for step in 0..=total {
                let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, height));
                assert!(
                    text.len() <= usize::from(height),
                    "height {height}, step {step}: {text:?}"
                );
                if height > 1 {
                    assert_eq!(
                        text.iter().filter(|line| line.starts_with('>')).count(),
                        1,
                        "height {height}, step {step}: {text:?}"
                    );
                }
                pane.env_mut().unwrap().move_by(1);
            }
            for step in 0..=total {
                let text = text_of(&pane_lines(&pane, fixtures::plain(), 120, height));
                if height > 1 {
                    assert_eq!(
                        text.iter().filter(|line| line.starts_with('>')).count(),
                        1,
                        "height {height}, step {step} up: {text:?}"
                    );
                }
                pane.env_mut().unwrap().move_by(-1);
            }
        }
    }

    #[test]
    fn every_env_line_fits_the_width_it_was_drawn_for() {
        let mut pane = web_pane();
        pane.open_env();
        pane.env_mut().unwrap().begin_typing();
        for typed in "A_VERY_LONG_ENV_KEY_NAME=and a value longer still".chars() {
            pane.env_mut().unwrap().type_char(typed);
        }
        for width in super::super::MIN_TERM_WIDTH..=200 {
            for line in text_of(&pane_lines(&pane, fixtures::plain(), width, 0)) {
                assert!(
                    visible_width(&line) <= usize::from(width),
                    "width {width} drew {}: {line:?}",
                    visible_width(&line)
                );
            }
        }
    }

    #[test]
    fn the_title_names_the_target_and_no_longer_calls_it_read_only() {
        let text = text_of(&pane_lines(&web_pane(), fixtures::plain(), 120, 0));
        assert!(text[0].contains("web"), "{:?}", text[0]);
        assert!(text[0].contains("(sheep config)"), "{:?}", text[0]);
        assert!(!text[0].contains("read-only"), "{:?}", text[0]);
    }
}
