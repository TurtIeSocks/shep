//! The settings screen: `[daemon]`, `[whistle]`, `[style]` then `[dogs]`,
//! drawn straight into the buffer rather than composed as a `Vec<Line>` the
//! way the dashboard's own panes are (this screen owns the whole body
//! between the title and the status bar, so there is no outer `draw` left to
//! hand lines to).
//!
//! Every row goes through [`fit`], the same truncation `view::flock` uses:
//! a value cut short says so with the same trailing `…`, rather than
//! spilling into the column beside it.
//!
//! All six scalars are editable. Four (`log_level`, `log_json`,
//! `allow_control`, `style level`) cycle, and their armed candidate is what
//! one extra line under the dogs table shows, when there is one. `socket`
//! and `max_cron_sleep` are free text instead: `Enter` on either row opens
//! an editor in that same slot ([`Settings::typing`]), and both are the
//! only two fields an empty buffer can unset. The dogs table's own
//! enable/disable is editable as well.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::super::app::{App, Settings, SettingsRow};
use super::super::theme::Palette;
use super::flock::{fit, mark};
use crate::commands::settings::{DogView, ScalarView, SettingField, SettingsSnapshot};

/// The dogs caption, verbatim. Not "space applies": this screen arms and
/// confirms like every other action in lookout (`x`, `R`, `L`), and a
/// caption claiming otherwise would be wrong on screen rather than merely
/// wrong in a doc.
const DOGS_CAPTION: &str = "space arms, Enter applies; a dog needs no reload";

/// The floor on the dogs table's NAME column, matching
/// [`super::flock::NAME_MIN`]'s own reasoning: whatever the fixed columns
/// leave, NAME never shrinks below a name worth reading.
const DOG_NAME_MIN: u16 = 8;

/// One column of the dogs table.
///
/// `Debug` is derived rather than redacted (IR-41): a bare variant name,
/// nothing a `{:?}` could leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DogColumn {
    /// The dog's name. The flexible column.
    Name,
    /// Whether `[daemon] enabled_dogs` names it -- a fact about the file,
    /// never about whether the shepherd has actually started it.
    InFile,
    /// `built-in`, or the adopted binary's path.
    Source,
    /// The join against the live flock: whether the dog is actually up.
    /// This task never renders it -- task 9's `dog_rows` supplies the data
    /// this cell needs, and until then drawing a RUNNING header over a
    /// column with nothing under it would be a rendering bug wearing a
    /// header. The variant and its place in [`DOG_TIERS`] exist now anyway, so
    /// task 9 extends the drop order rather than re-arguing it.
    Running,
}

impl DogColumn {
    /// The header text.
    #[must_use]
    const fn header(self) -> &'static str {
        match self {
            Self::Name => "NAME",
            Self::InFile => "IN FILE",
            Self::Source => "SOURCE",
            Self::Running => "RUNNING",
        }
    }

    /// The fixed width of this column's cells. `Name` reports `0` -- see
    /// [`super::flock::Column::width`]'s own doc for why.
    #[must_use]
    const fn width(self) -> u16 {
        match self {
            Self::Name => 0,
            // 7: `IN FILE`, its own header -- `yes`/`no` cells are shorter.
            Self::InFile => 7,
            // 24: an adopted binary's path can run long, and this is the
            // column that shows it whole up to that budget before `fit`
            // truncates it.
            Self::Source => 24,
            // 15, matching `super::flock::Column::Status`'s own width and
            // reasoning: task 9's join is expected to report the same kind
            // of word (`online`, `silent`, `not running`) the flock table's
            // STATUS column does.
            Self::Running => 15,
        }
    }
}

const ALL_DOG_COLUMNS: &[DogColumn] = &[
    DogColumn::Name,
    DogColumn::InFile,
    DogColumn::Source,
    DogColumn::Running,
];
const NO_SOURCE: &[DogColumn] = &[DogColumn::Name, DogColumn::InFile, DogColumn::Running];
const FLOOR_DOG_COLUMNS: &[DogColumn] = &[DogColumn::Name, DogColumn::Running];

/// The narrowest terminal the dogs table's floor column set will draw into:
/// [`DogColumn::Running`] plus [`DOG_NAME_MIN`] plus one gap.
const DOG_MIN_WIDTH: u16 = DogColumn::Running.width() + DOG_NAME_MIN + 2;

/// Width thresholds, widest first, mirroring [`super::flock::TIERS`]'s own
/// shape and the reasoning it argues at length: least-diagnostic first.
///
/// SOURCE goes first because it is the widest column here and an adopted
/// path can run long -- the same reasoning `flock::TIERS` gives for SMIT,
/// its own widest column. IN FILE goes second: it says what the document
/// declares, which RUNNING (once task 9 wires it) answers a sharper version
/// of -- "is it up" rather than "is it named". NAME and RUNNING are the
/// floor for the same reason ID/NAME/STATUS is the flock table's: those two
/// are the pane.
const DOG_TIERS: &[(u16, &[DogColumn])] = &[
    (61, ALL_DOG_COLUMNS),
    (34, NO_SOURCE),
    (DOG_MIN_WIDTH, FLOOR_DOG_COLUMNS),
];

/// The widest dogs-table column set that fits `width`.
///
/// Includes [`DogColumn::Running`] in every tier, even though nothing in
/// this task draws it -- see that variant's own doc. A caller that wants the
/// columns this task actually renders filters it out; `draw_settings` does
/// exactly that.
#[must_use]
pub fn columns_for(width: u16) -> &'static [DogColumn] {
    DOG_TIERS
        .iter()
        .find(|(threshold, _)| width >= *threshold)
        .map_or(FLOOR_DOG_COLUMNS, |(_, columns)| *columns)
}

/// What NAME gets once the fixed dogs-table columns and their separators
/// are paid for. [`super::flock::name_width`]'s own twin.
fn dog_name_width(width: u16, columns: &[DogColumn]) -> u16 {
    let fixed: u16 = columns.iter().map(|column| column.width()).sum();
    let gaps = u16::try_from(columns.len().saturating_sub(1)).unwrap_or(0) * 2;
    width
        .saturating_sub(fixed)
        .saturating_sub(gaps)
        .max(DOG_NAME_MIN)
}

/// One dog's cell text.
///
/// [`DogColumn::Running`] is never reached in this task: [`draw_settings`]
/// filters it out of the rendered column set before calling this, per
/// that variant's own doc. The arm stays here rather than being left
/// unimplemented so the match is exhaustive and a future column cannot fall
/// through it unnoticed.
fn dog_cell(dog: &DogView, column: DogColumn) -> String {
    match column {
        DogColumn::Name => dog.name.clone(),
        DogColumn::InFile => if dog.enabled { "yes" } else { "no" }.to_string(),
        DogColumn::Source => dog
            .adopted_path
            .as_ref()
            .map_or_else(|| "built-in".to_string(), |path| path.display().to_string()),
        DogColumn::Running => String::new(),
    }
}

/// The dogs table's header line, indented to match every row's own
/// mark-and-gap prefix ([`super::flock::mark`]'s own two columns).
fn dog_header_line(columns: &[DogColumn], width: u16, palette: Palette) -> Line<'static> {
    let name_width = dog_name_width(width, columns);
    let mut text = String::from("  ");
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let cell_width = if *column == DogColumn::Name {
            name_width
        } else {
            column.width()
        };
        text.push_str(&fit(column.header(), cell_width));
    }
    Line::from(Span::styled(text, palette.muted()))
}

/// One dog's row.
fn dog_line(dog: &DogView, columns: &[DogColumn], width: u16, selected: bool) -> Line<'static> {
    let name_width = dog_name_width(width, columns);
    let mut text = format!("{} ", mark(selected));
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let cell_width = if *column == DogColumn::Name {
            name_width
        } else {
            column.width()
        };
        text.push_str(&fit(&dog_cell(dog, *column), cell_width));
    }
    Line::from(Span::raw(text))
}

/// A `[section]` header, indented to match every scalar row's own
/// mark-and-gap prefix, same as [`dog_header_line`]'s own reasoning.
fn section_header(label: &str, palette: Palette) -> Line<'static> {
    Line::from(Span::styled(format!("  {label}"), palette.muted()))
}

/// Which of the six scalars a [`SettingField`] names.
fn scalar_view(snapshot: &SettingsSnapshot, field: SettingField) -> &ScalarView {
    match field {
        SettingField::LogLevel => &snapshot.log_level,
        SettingField::LogJson => &snapshot.log_json,
        SettingField::Socket => &snapshot.socket,
        SettingField::MaxCronSleep => &snapshot.max_cron_sleep,
        SettingField::AllowControl => &snapshot.allow_control,
        SettingField::StyleLevel => &snapshot.style_level,
    }
}

/// The name printed in the NAME cell -- the document's own key, except
/// `[style] level`, which drops the `style_` a section header already says.
/// `pub(super)`: `status::status_line`'s own editor-line branch names the
/// field being typed with this same word, so the status bar and the body
/// pane never disagree about what to call `socket` or `max_cron_sleep`.
pub(super) const fn field_label(field: SettingField) -> &'static str {
    match field {
        SettingField::LogLevel => "log_level",
        SettingField::LogJson => "log_json",
        SettingField::Socket => "socket",
        SettingField::MaxCronSleep => "max_cron_sleep",
        SettingField::AllowControl => "allow_control",
        SettingField::StyleLevel => "level",
    }
}

/// What applying this field costs, decision 6 of the design spec's own
/// table. `log_level`, `log_json` and `max_cron_sleep` all need a daemon
/// reload; `socket` needs a full stop and start, since a reload never moves
/// the listening socket; `allow_control` needs whistle restarted rather
/// than the daemon, because a dog toggle and a whistle setting are both
/// read by a process the daemon itself never touches; `style level` costs
/// nothing because lookout re-resolves it on its own next command.
const fn apply_cost(field: SettingField) -> &'static str {
    match field {
        SettingField::LogLevel | SettingField::LogJson | SettingField::MaxCronSleep => {
            "needs shep daemon reload"
        }
        SettingField::Socket => "needs the shepherd stopped and started",
        SettingField::AllowControl => "needs shep whistle restarted",
        SettingField::StyleLevel => "the next command reads it",
    }
}

/// NAME column width: fits `max_cron_sleep`, the longest field name, with a
/// column of padding.
const SCALAR_NAME_W: u16 = 15;
/// VALUE column width: fits `/home/ada/.shep/run/shep.sock` (29 columns),
/// an ordinary absolute socket path, whole -- a value already past this
/// budget still truncates through [`fit`] rather than growing the column.
const SCALAR_VALUE_W: u16 = 30;
/// SOURCE column width: `$SHEP_STYLE` and `the default` are both 11 columns,
/// the widest two words [`crate::style::StyleSource::Display`] ever prints.
const SCALAR_SOURCE_W: u16 = 11;
/// The floor on the apply-cost column once `width` is too narrow to give it
/// the remainder: enough for `needs`, never a whole sentence.
const SCALAR_COST_MIN: u16 = 8;

/// One scalar row: name, value, source, apply cost.
fn scalar_line(
    field: SettingField,
    view: &ScalarView,
    selected: bool,
    width: u16,
) -> Line<'static> {
    let fixed = 1 + 1 + SCALAR_NAME_W + 2 + SCALAR_VALUE_W + 2 + SCALAR_SOURCE_W + 2;
    let cost_width = width.saturating_sub(fixed).max(SCALAR_COST_MIN);
    let text = format!(
        "{} {}  {}  {}  {}",
        mark(selected),
        fit(field_label(field), SCALAR_NAME_W),
        fit(&view.value, SCALAR_VALUE_W),
        fit(&view.source.to_string(), SCALAR_SOURCE_W),
        fit(apply_cost(field), cost_width),
    );
    Line::from(Span::raw(text))
}

/// Which `[section]` a scalar field's row lives under.
const fn section_for(field: SettingField) -> &'static str {
    match field {
        SettingField::LogLevel
        | SettingField::LogJson
        | SettingField::Socket
        | SettingField::MaxCronSleep => "[daemon]",
        SettingField::AllowControl => "[whistle]",
        SettingField::StyleLevel => "[style]",
    }
}

/// Every line of the screen's body, top to bottom: `[daemon]`'s four rows,
/// `[whistle]`'s one, `[style]`'s one, then `[dogs]`'s caption and table.
///
/// Walks [`Settings::rows`] itself rather than hand-listing the six scalars
/// a second time in this function's own order: two lists that happen to
/// agree today would silently desync the moment either one reordered, and
/// the cursor -- which [`Settings::rows`] alone defines -- would then land
/// on a row this function drew somewhere else.
fn content_lines(settings: &Settings, palette: Palette, width: u16) -> Vec<Line<'static>> {
    let snapshot = settings.snapshot();
    let cursor = settings.cursor();

    let mut lines = Vec::new();
    let mut current_section: Option<&'static str> = None;

    // The six scalar rows always sort first in `Settings::rows`, ahead of
    // every `SettingsRow::Dog` -- see its own doc -- so this loop's `break`
    // on the first non-scalar row is exactly "stop once the scalars end",
    // never "stop at the first dog that happens to come early".
    for row in settings.rows() {
        let SettingsRow::Scalar(field) = row else {
            break;
        };
        let section = section_for(field);
        if current_section != Some(section) {
            if current_section.is_some() {
                lines.push(Line::default());
            }
            lines.push(section_header(section, palette));
            current_section = Some(section);
        }
        lines.push(scalar_line(
            field,
            scalar_view(snapshot, field),
            cursor == Some(row),
            width,
        ));
    }
    lines.push(Line::default());

    lines.push(section_header("[dogs]", palette));
    lines.push(Line::from(Span::styled(
        format!("  {DOGS_CAPTION}"),
        palette.muted(),
    )));
    // `Running` is dropped from what actually draws -- see its own doc --
    // so NAME reclaims the columns it would have taken rather than leaving
    // them blank.
    let rendered_columns: Vec<DogColumn> = columns_for(width)
        .iter()
        .copied()
        .filter(|column| *column != DogColumn::Running)
        .collect();
    lines.push(dog_header_line(&rendered_columns, width, palette));
    for (index, dog) in snapshot.dogs.iter().enumerate() {
        let selected = cursor == Some(SettingsRow::Dog(index));
        lines.push(dog_line(dog, &rendered_columns, width, selected));
    }

    // One prompt line under the table, in the shape the status bar's own
    // `confirm_prompt`/`in_flight_text` use for a sheep confirm: a question
    // styled `attention` while it waits on `Enter`, an in-flight sentence
    // once it has gone out. Drawn here rather than in the status bar itself
    // -- the settings screen owns its own body between the title and that
    // bar, and this prompt is about one row in the table above it, not
    // about the whole dashboard.
    if let Some(prompt) = settings.pending() {
        lines.push(Line::default());
        let text = if prompt.sent {
            format!("{}  sent, waiting for the shepherd", prompt.text)
        } else {
            format!("{}  enter confirms, any other key cancels", prompt.text)
        };
        lines.push(Line::from(Span::styled(
            format!("  {text}"),
            palette.attention(),
        )));
    } else if let Some((field, buffer)) = settings.typing() {
        // The free-text editor's own line, in the same slot the prompt
        // above uses -- the two never coexist ([`Settings::pending`]
        // returns `None` for `Pending::Typing`), so this is an `else if`
        // rather than a second, always-checked block. The cursor is a
        // character rather than a style, the same call the status bar's
        // own filter box already makes: the ANSI gallery renders
        // foregrounds only, so a reversed cell would come out unstyled
        // there.
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!(
                "  editing {}: {buffer}\u{258f}   enter applies   esc cancels",
                field_label(*field)
            ),
            palette.attention(),
        )));
    }

    lines
}

/// Draws the settings screen into `area`, straight into `buffer`.
///
/// `app` supplies the palette; every other fact this screen shows, including
/// its one armed or in-flight prompt line, comes off `settings`.
pub fn draw_settings(app: &App, settings: &Settings, area: Rect, buffer: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let palette = app.palette();
    for (offset, line) in content_lines(settings, palette, area.width)
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
    use crate::style::StyleSource;

    /// The whole screen at a comfortable width. The snapshot is the
    /// assertion: it pins the section order, every row's four columns and
    /// the dogs table underneath them.
    #[test]
    fn settings_at_a_comfortable_width() {
        let app = fixtures::app_in_settings();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| super::super::draw(&app, frame))
            .unwrap();
        insta::assert_snapshot!(render_text(terminal.backend().buffer()));
    }

    /// SOURCE is the widest column and the first to go, the same reasoning
    /// `flock::TIERS` gives for SMIT.
    #[test]
    fn the_dogs_source_column_drops_before_the_rest() {
        assert!(columns_for(120).contains(&DogColumn::Source));
        assert!(!columns_for(60).contains(&DogColumn::Source));
        assert!(
            columns_for(60).contains(&DogColumn::Running),
            "RUNNING is the diagnostic half and outlives SOURCE"
        );
    }

    /// fails if a dogs-table tier can render wider than the terminal it was
    /// chosen for -- the same claim `flock`'s own
    /// `every_tier_fits_the_width_it_claims` makes for the flock table.
    #[test]
    fn every_dogs_tier_fits_the_width_it_claims() {
        for width in DOG_MIN_WIDTH..=200 {
            let columns = columns_for(width);
            let fixed: u16 = columns.iter().map(|c| c.width()).sum();
            let gaps = u16::try_from(columns.len() - 1).unwrap() * 2;
            assert!(
                fixed + gaps + DOG_NAME_MIN <= width,
                "width {width} chose {} columns needing {}",
                columns.len(),
                fixed + gaps + DOG_NAME_MIN
            );
        }
    }

    /// fails if the dogs caption starts claiming space applies rather than
    /// arms. This screen arms and confirms like every other action in
    /// lookout, and a caption that says otherwise is wrong on screen.
    #[test]
    fn the_dogs_caption_says_arms_not_applies() {
        assert_eq!(
            DOGS_CAPTION,
            "space arms, Enter applies; a dog needs no reload"
        );
    }

    /// fails if the cursor mark stops landing on the row `Settings::cursor`
    /// actually points at, or lands on more than one.
    #[test]
    fn the_cursor_mark_sits_on_exactly_the_selected_row() {
        let app = fixtures::app_in_settings();
        let settings = app.settings().unwrap();
        let palette = app.palette();
        let lines = content_lines(settings, palette, 120);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let marked: Vec<&String> = rendered.iter().filter(|l| l.starts_with('>')).collect();
        assert_eq!(marked.len(), 1, "exactly one row is marked: {rendered:?}");
        assert!(
            marked[0].contains("log_level"),
            "the cursor opens on the first row: {rendered:?}"
        );
    }

    /// fails if `SCALAR_SOURCE_W` stops fitting the widest word this column
    /// ever prints. `"the default"` is what a fresh `$SHEP_HOME` shows for
    /// every scalar -- the state most operators open this screen in -- and
    /// no fixture anywhere in this tree renders it: `settings_snapshot`
    /// gives every scalar `StyleSource::Config`, which is one column
    /// shorter (`"shep.toml"`) and would not catch the column shrinking out
    /// from under the longer word.
    #[test]
    fn the_default_source_label_fits_the_column_it_was_sized_for() {
        let rendered = fit(&StyleSource::Default.to_string(), SCALAR_SOURCE_W);
        assert_eq!(
            rendered.chars().count(),
            usize::from(SCALAR_SOURCE_W),
            "fit always pads or truncates to the exact column width"
        );
        assert!(
            !rendered.contains('…'),
            "SCALAR_SOURCE_W must fit \"the default\" whole: got {rendered:?}"
        );
    }

    /// fails if the free-text editor stops showing what is being typed, or
    /// stops naming the field it belongs to -- the same claim
    /// `the_cursor_mark_sits_on_exactly_the_selected_row` makes for the
    /// cursor, aimed at task 8's own line instead.
    #[test]
    fn the_editor_line_names_its_field_and_shows_the_buffer() {
        let mut app = fixtures::app_in_settings_on(SettingField::Socket);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let settings = app.settings().unwrap();
        let palette = app.palette();
        let lines = content_lines(settings, palette, 120);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            rendered.iter().any(|line| line.contains("editing socket:")
                && line.contains("/home/ada/.shep/run/shep.sock")),
            "got: {rendered:?}"
        );
    }
}
