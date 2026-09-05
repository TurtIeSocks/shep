//! Drawing a [`ConfigPane`]: a title naming the target, the field set's
//! groups as section headers, one row per field, and a cost column saying
//! what changing that field would cost.
//!
//! The layout is [`super::settings`]'s, and deliberately so: both screens
//! own the whole body between the title line and the status bar, both have
//! more rows than a terminal has lines, and both pay for chrome the
//! viewport cannot see. The scroll walk itself is shared
//! ([`super::scroll::to_cursor`]); the layout below is this pane's own,
//! because a uniform field list under four headers and a settings screen
//! with a caption, a column header and a dogs table have almost no lines in
//! common.
//!
//! A sheep pane is 39 rows plus a title, four headers and three blank
//! separators, so the chrome runs to eight lines before a marker is paid
//! for. Every one of them is counted here.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use shep_core::config::ApplyGroup;

use super::super::app::App;
use super::super::pane::{ConfigPane, Lock, PaneRow, PaneTarget};
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
/// [`ApplyGroup`] is `#[non_exhaustive]`, so the wildcard is required rather
/// than chosen. It answers `respawn`, the most conservative of the four and
/// the same fallback [`apply_group`](shep_core::config::apply_group) gives a
/// field name its own table has not been taught about: a group this pane has
/// not been taught about promises a restart rather than a silent claim that
/// the change applied.
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

/// The pane's own title: which sheep or dog is being edited, and that it is
/// read-only.
///
/// The dashboard's title line above this one names `$SHEP_HOME` and nothing
/// else, so without this the operator would be reading 39 fields with
/// nothing on screen saying whose they are.
fn title_line(pane: &ConfigPane, palette: Palette, width: u16) -> Line<'static> {
    let kind = match pane.target() {
        PaneTarget::Sheep { .. } => "sheep config",
    };
    Line::from(Span::styled(
        format!(
            "  {}",
            fit(
                &format!("{}  ({kind}, read-only)", pane.target().name()),
                body_width(width)
            )
        ),
        palette.muted(),
    ))
}

/// One field's row: the selection mark, a lock glyph, a flag, the key, the
/// value and what changing it costs.
///
/// The flag is the same pair `shep flock`'s own CFG column prints: `!` for a
/// field parked until a respawn, `*` for one an operator has overridden.
/// Pending wins when a field is both, because it is the sharper fact -- the
/// value on screen is not the value the running child holds.
///
/// The lock glyph sits in the column between the selection mark and the
/// flag, which every row already spent on a space, so it costs nothing and
/// cannot be truncated. It is a glyph rather than a style because a style
/// says nothing at all in `plain`, and nothing but the mark, the glyph, the
/// flag and the key is guaranteed to render at `MIN_TERM_WIDTH` -- the cost
/// cell, which is the only other place either fact appears, is the first
/// column this pane drops. See [`Lock`].
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
    let value = if field.secret && raw != "(unset)" {
        "<set>".to_owned()
    } else {
        raw
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
    // Muting reinforces the glyph and never carries a fact on its own. It
    // used to be the whole signal, and it said "read-only" about six fields
    // shep writes happily -- `args`, `ignore_watch`, `liveness_probe`,
    // `readiness_probe`, `stop_exit_codes` and `watch_options`, every one of
    // them merely a shape this pane has no widget for. A `plain` palette
    // renders muted as nothing, so a style could not have told those six
    // apart from `name` and `instances` even in principle.
    if field.editable {
        Line::from(Span::raw(text))
    } else {
        Line::from(Span::styled(text, palette.muted()))
    }
}

/// Every line of the pane, top to bottom, laid out for a terminal `height`
/// rows tall.
///
/// `height` counts LINES and no more than that ever come back. Zero means
/// unlimited, which is what a test with no terminal behind it gets. See
/// [`super::scroll`] for why the viewport's own offset is a starting point
/// here rather than an answer.
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
    let mut lines = vec![title_line(pane, palette, width)];
    // The title is unconditional, so the body is laid out against what is
    // left after it. An empty form -- unreachable for a sheep, whose schema
    // is a committed file with 39 properties, but a dog answers `--schema`
    // for itself -- leaves the title as the whole pane.
    let body_budget = budget - 1;
    if pane.fields().is_empty() || body_budget == 0 {
        return lines;
    }
    let total = pane.rows().len();
    let cursor_row = pane.view().cursor().min(total - 1);
    lines.extend(super::scroll::to_cursor(
        cursor_row,
        pane.view().offset(),
        |offset| body_from(pane, palette, width, body_budget, offset),
        || cursor_only(pane, palette, width, body_budget, cursor_row),
    ));
    lines
}

/// Lays the body out from field `offset`, spending at most `budget` lines.
///
/// Every line pushed is counted, including the section headers, the blank
/// separators between them and both markers. The two markers are reserved
/// BEFORE a row is admitted rather than appended afterwards, so a height
/// that binds cuts a row instead of cutting the sentence that says a row was
/// cut.
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::fixtures;
    use super::*;
    use crate::lookout::app::{KeyPress, Msg};
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

    /// fails if the pane stops naming the section a scrolled window opened
    /// in, or stops saying how many rows it hid above.
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

    /// fails if any line of the pane renders wider than the terminal it was
    /// drawn for. `Buffer::set_line` clips in silence, so an overrun is a
    /// truncated cost cell with nothing saying it was cut -- the same claim
    /// `settings::every_settings_line_fits_the_terminal_it_was_drawn_for`
    /// makes for the screen this one borrowed its layout from.
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

    /// fails if the cost column stops saying why a Structural field cannot
    /// be edited, or if either kind of locked row stops being drawn muted.
    ///
    /// Muting reinforces the glyph on both kinds and carries no fact of its
    /// own, so `args` -- which shep writes happily -- is checked here too,
    /// against a cost cell that must keep saying `respawn` rather than
    /// `read-only`.
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
    /// `starts_with`, which only ever matched UNSELECTED rows and so could
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

    /// fails if the two flags stop marking the fields the shepherd reported,
    /// or start marking any others.
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
        assert_eq!(flagged('*'), ["instances", "max_restarts"]);
        assert_eq!(flagged('!'), ["kill_signal"]);
        assert_eq!(rows_of(&text).len(), 39, "every field is drawn at 120");
    }

    /// fails if the pane goes back to saying one thing about two different
    /// facts.
    ///
    /// `=` is shep refusing a config write to the field at all. `~` is only
    /// this pane having no widget for the shape, which is true of six fields
    /// `shep start <Flockfile>` writes happily -- and the cost cell beside
    /// each of them says `respawn` or `now`, not `read-only`, so a row
    /// carrying `=` there would be the pane contradicting itself.
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

    /// fails if the distinction above survives only at a comfortable width
    /// or only in colour.
    ///
    /// `MIN_TERM_WIDTH` drops the cost cell, which is the only other place
    /// either fact is written, and `plain` renders muted as nothing at all.
    /// So the glyph is the whole signal in exactly the case an operator is
    /// most likely to be in, and this is the test that says it survives
    /// there.
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

    /// fails if any single step of a walk to the bottom and back loses the
    /// cursor, at any height this pane says it can draw.
    ///
    /// The class this pane's layout was copied to avoid: chrome eats the
    /// budget, the selected row is never drawn, and every static frame
    /// still looks right. Six is `view::MIN_HEIGHT`, which leaves three
    /// lines of body under the pane's own title -- less than a group's
    /// first row costs in place, so those steps go through `cursor_only`.
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

    /// fails if the body outgrows the height it was given, which is how the
    /// marker that says rows were cut becomes the row that gets cut.
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

    /// fails if the title stops naming the sheep the pane is about. The
    /// dashboard's own title line above it names `$SHEP_HOME` and nothing
    /// else.
    #[test]
    fn the_title_names_the_target_and_says_it_is_read_only() {
        let text = text_of(&pane_lines(&web_pane(), fixtures::plain(), 120, 0));
        assert!(text[0].contains("web"), "{:?}", text[0]);
        assert!(
            text[0].contains("(sheep config, read-only)"),
            "{:?}",
            text[0]
        );
    }
}
