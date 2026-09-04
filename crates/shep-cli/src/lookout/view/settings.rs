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
//! enable/disable is editable as well, and its RUNNING column
//! ([`dog_rows`]) joins the file's own `enabled`/`SOURCE` pair against the
//! live flock, which is the only one of the three that can drift from it.

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::super::app::{App, Settings, SettingsRow};
use super::super::theme::Palette;
use super::flock::{fit, mark};
use crate::commands::settings::{ScalarView, SettingField, SettingsSnapshot};
use crate::style::StyleSource;
use crate::vocabulary::Reported;

/// The dogs caption, verbatim. Not "space applies": this screen arms and
/// confirms like every other action in lookout (`x`, `R`, `L`), and a
/// caption claiming otherwise would be wrong on screen rather than merely
/// wrong in a doc.
const DOGS_CAPTION: &str = "space arms, Enter applies; a dog needs no reload";

/// The floor on the dogs table's NAME column, matching
/// [`super::flock::NAME_MIN`]'s own reasoning: whatever the fixed columns
/// leave, NAME never shrinks below a name worth reading.
const DOG_NAME_MIN: u16 = 8;

/// The columns every line on this screen spends on the selection mark and
/// the space after it, before any cell is drawn.
///
/// [`super::flock::GUTTER`]'s twin, and it exists for the reason that one
/// does: a budget that forgets it is a budget every line overruns. Both the
/// dogs table and the scalar rows draw [`mark`] plus a space, so the width
/// a tier is chosen for and the width its cells are fitted into is the
/// terminal MINUS this, never the terminal itself. Leaving it out put every
/// dogs line two columns over its budget at every width -- 122 columns
/// drawn into a 120-column terminal -- with `Buffer::set_line` clipping the
/// overflow in silence. RUNNING is last, so what got clipped was always the
/// diagnostic half: `waiting-restart` is exactly 15 characters in a 15-wide
/// column, so it drew as `waiting-resta` with no ellipsis to say so.
const GUTTER: u16 = 2;

/// The width the rows themselves are laid out in: the terminal minus
/// [`GUTTER`].
const fn body_width(width: u16) -> u16 {
    width.saturating_sub(GUTTER)
}

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
    /// [`dog_rows`] supplies the word this cell shows -- the same one
    /// [`super::flock::Column::Status`] would print for a sheep of that
    /// name, off [`crate::vocabulary::Reported`] -- or [`None`] when no
    /// dog of this name is running, which reads `not running`.
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

/// The narrowest the dogs TABLE will draw into: [`DogColumn::Running`] plus
/// [`DOG_NAME_MIN`] plus one gap.
///
/// The table's floor, not the terminal's, exactly as
/// [`super::flock::MIN_WIDTH`] is: a terminal also has to pay [`GUTTER`]
/// for the selection mark, so the narrowest TERMINAL this table fits is
/// `DOG_MIN_WIDTH + GUTTER`. Every threshold in [`DOG_TIERS`] is a table
/// width and is compared against [`body_width`], never against the raw
/// terminal.
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
/// Includes [`DogColumn::Running`] in every tier -- it is the floor,
/// alongside NAME, and `draw_settings` renders every column this returns.
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

/// One row of the dogs table, with the file and the live flock joined by
/// name. Declared here rather than in the [`SettingsSnapshot`] task's own
/// module because the join is this one's whole subject: that snapshot
/// carries [`crate::commands::settings::DogView`]'s `enabled` and
/// `adopted_path` alone, and [`Self::running`] is what [`dog_rows`] adds.
///
/// `Debug` is derived rather than redacted (IR-41): a name, a bool, a
/// rendered word and a path, none of which is a secret -- a dog's own
/// config, which can be, lives in `dogs.toml` and neither this type nor the
/// [`DogView`](crate::commands::settings::DogView) it is built from ever
/// touches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogRow {
    /// The dog's name.
    pub name: String,
    /// Whether `[daemon] enabled_dogs` names it -- a fact about the file,
    /// never about whether the shepherd has actually started it.
    pub enabled: bool,
    /// The word the flock table would show for this dog's own row, or
    /// `None` when no dog of this name is running.
    pub running: Option<String>,
    /// `None` for a built-in dog; the adopted binary's path otherwise.
    pub adopted_path: Option<PathBuf>,
}

/// The dogs table's rows: `app`'s settings snapshot joined against its own
/// live flock, by name.
///
/// `app` rather than `Settings` alone: the file half
/// ([`SettingsSnapshot::dogs`]) and the join's other half
/// (`App::all_rows`, the live flock) live on two different types, and this
/// is the one function that reads both. Returns the empty vector when the
/// settings screen is not open -- `draw_settings` never calls this while it
/// is closed, but a caller that did gets no rows rather than a panic.
#[must_use]
pub fn dog_rows(app: &App, width: u16) -> Vec<DogRow> {
    // Not read: every dogs-table tier keeps `DogColumn::Running`, so there
    // is no width where the join this function does would go unrendered --
    // see `columns_for`'s own doc. Kept in the signature to match every
    // other view function in this file, which takes the render width it is
    // about to be fit into.
    let _ = width;
    let Some(settings) = app.settings() else {
        return Vec::new();
    };
    let running_by_name: std::collections::BTreeMap<&str, String> = app
        .all_rows()
        .into_iter()
        .filter(|row| row.info.dog.is_some())
        .map(|row| {
            (
                row.info.name.as_str(),
                Reported::of(row.info.status, row.info.handshook).word(),
            )
        })
        .collect();
    settings
        .snapshot()
        .dogs
        .iter()
        .map(|dog| DogRow {
            name: dog.name.clone(),
            enabled: dog.enabled,
            running: running_by_name.get(dog.name.as_str()).cloned(),
            adopted_path: dog.adopted_path.clone(),
        })
        .collect()
}

/// One dog's cell text.
fn dog_cell(dog: &DogRow, column: DogColumn) -> String {
    match column {
        DogColumn::Name => dog.name.clone(),
        DogColumn::InFile => if dog.enabled { "yes" } else { "no" }.to_string(),
        DogColumn::Source => dog
            .adopted_path
            .as_ref()
            .map_or_else(|| "built-in".to_string(), |path| path.display().to_string()),
        DogColumn::Running => dog
            .running
            .clone()
            .unwrap_or_else(|| "not running".to_string()),
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
fn dog_line(dog: &DogRow, columns: &[DogColumn], width: u16, selected: bool) -> Line<'static> {
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
/// read by a process the daemon itself never touches.
///
/// `style level` is the one cell that reads `source` as well as the field.
/// It costs nothing when the file is what decides -- lookout re-resolves it
/// on its own next command -- but `--style` and `$SHEP_STYLE` outrank the
/// file, and under either of those "the next command reads it" is simply
/// untrue. The SOURCE cell two columns left already names the layer, so
/// this cell says what that layer does rather than repeating its name. The
/// confirm says it at length; see `app::style_confirm_text`.
const fn apply_cost(field: SettingField, source: StyleSource) -> &'static str {
    match field {
        SettingField::LogLevel | SettingField::LogJson | SettingField::MaxCronSleep => {
            "needs shep daemon reload"
        }
        SettingField::Socket => "needs the shepherd stopped and started",
        SettingField::AllowControl => "needs shep whistle restarted",
        SettingField::StyleLevel => match source {
            StyleSource::Config | StyleSource::Default => "the next command reads it",
            StyleSource::Env | StyleSource::Flag => "written, but outranked",
        },
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
        fit(apply_cost(field, view.source), cost_width),
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
///
/// Takes `app` as well as `settings`, unlike every scalar row above it:
/// [`dog_rows`] is the one read here that needs the live flock, which lives
/// on `App` and not on `Settings` alone.
fn content_lines(
    app: &App,
    settings: &Settings,
    palette: Palette,
    width: u16,
) -> Vec<Line<'static>> {
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
    // `body_width`, not `width`: every line below draws `mark`'s own two
    // columns before its first cell, so the table is laid out in what is
    // left after them. `view::mod`'s own `draw` does the same for the flock
    // table, and this pane had not.
    let table_width = body_width(width);
    // Fitted rather than printed raw: the caption is 48 columns and the
    // screen draws from `view::MIN_TERM_WIDTH` (33) up, so on a narrow
    // terminal it was cut mid-word by `Buffer::set_line` with nothing
    // saying it had been cut.
    lines.push(Line::from(Span::styled(
        format!("  {}", fit(DOGS_CAPTION, table_width)),
        palette.muted(),
    )));
    let rendered_columns = columns_for(table_width);
    lines.push(dog_header_line(rendered_columns, table_width, palette));
    for (index, dog) in dog_rows(app, table_width).iter().enumerate() {
        let selected = cursor == Some(SettingsRow::Dog(index));
        lines.push(dog_line(dog, rendered_columns, table_width, selected));
    }

    // One prompt line under the table, echoing the status bar's own Slot 1
    // (`view::status::status_line`'s own doc): a question styled
    // `attention` while it waits on `Enter`, an in-flight sentence once it
    // has gone out. The status bar is the line of record now -- it is a
    // fixed row `draw_settings`'s own `.take(area.height)` never reaches,
    // where this body echo used to be the ONLY place an armed candidate
    // showed at all, and the first thing a short terminal cut. Kept here
    // too, for the same reason the free-text editor line below is kept in
    // both places: when there is room, seeing the confirm sit right under
    // the row it names is worth the redundancy.
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
/// `app` supplies the palette and the live flock [`dog_rows`] joins against;
/// every other fact this screen shows, including its one armed or in-flight
/// prompt line, comes off `settings`.
pub fn draw_settings(app: &App, settings: &Settings, area: Rect, buffer: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let palette = app.palette();
    for (offset, line) in content_lines(app, settings, palette, area.width)
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
    ///
    /// Measures the RENDERED lines rather than summing the declared widths,
    /// which is what the arithmetic version of this test did and why it
    /// could not fail. Every row and the header carry `mark`'s own
    /// two-column prefix, and the sum left it out exactly as the code did,
    /// so the test agreed with the bug: every dogs line came out two
    /// columns over its budget at every width, `Buffer::set_line` clipped
    /// the overflow in silence, and RUNNING is the last column, so
    /// `waiting-restart` (15 characters in a 15-wide column) always drew as
    /// `waiting-resta` with no ellipsis to say it had been cut.
    #[test]
    fn every_dogs_tier_fits_the_width_it_claims() {
        let app = fixtures::app_in_settings_with_dog_drift();
        let settings = app.settings().unwrap();
        let palette = app.palette();
        for width in (DOG_MIN_WIDTH + GUTTER)..=200 {
            let rendered: Vec<String> = content_lines(&app, settings, palette, width)
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            // Everything from the `[dogs]` header down: the caption, the
            // column header and one line per dog.
            let table = rendered
                .iter()
                .position(|line| line.contains("[dogs]"))
                .expect("the dogs section is always drawn");
            for line in &rendered[table..] {
                assert!(
                    visible_width(line) <= usize::from(width),
                    "width {width} drew {} columns: {line:?}",
                    visible_width(line)
                );
            }
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
        let lines = content_lines(&app, settings, palette, 120);
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

    /// fails if the style row keeps claiming the next command reads it
    /// while a layer above the file is set.
    ///
    /// The row's own SOURCE cell already says `$SHEP_STYLE`, so the cost
    /// cell beside it saying "the next command reads it" is the same frame
    /// contradicting itself: the next command reads `$SHEP_STYLE`.
    #[test]
    fn the_style_cost_cell_stops_promising_the_next_command_when_it_is_outranked() {
        let app = fixtures::app_in_settings_with_shadowed_style(StyleSource::Env);
        let settings = app.settings().unwrap();
        let lines = content_lines(&app, settings, app.palette(), 120);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let row = rendered
            .iter()
            .find(|line| {
                line.get(2..)
                    .is_some_and(|cells| cells.starts_with("level "))
            })
            .unwrap_or_else(|| panic!("the style row is drawn: {rendered:?}"));
        assert!(row.contains("$SHEP_STYLE"), "got: {row:?}");
        assert!(row.contains("written, but outranked"), "got: {row:?}");
        assert!(!row.contains("the next command reads it"), "got: {row:?}");
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
        let lines = content_lines(&app, settings, palette, 120);
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

    /// `otel` runs while the file has it disabled, which is what "a removed
    /// name keeps running" looks like. `ledger` is enabled and absent,
    /// which is a dog that failed to start.
    #[test]
    fn the_dogs_table_joins_the_file_against_the_running_flock() {
        let app = fixtures::app_in_settings_with_dog_drift();
        let rows = dog_rows(&app, 120);

        let otel = rows.iter().find(|r| r.name == "otel").unwrap();
        assert!(!otel.enabled);
        assert_eq!(otel.running.as_deref(), Some("online"));

        let ledger = rows.iter().find(|r| r.name == "ledger").unwrap();
        assert!(ledger.enabled);
        assert_eq!(ledger.running, None);
    }

    /// Phase 3b: a dog that never completed a handshake reads `silent`, not
    /// `online`, and this table must not undo that.
    #[test]
    fn a_dog_that_never_handshook_reads_silent_here_too() {
        let app = fixtures::app_in_settings_with_silent_dog();
        let rows = dog_rows(&app, 120);
        let bark = rows.iter().find(|r| r.name == "bark").unwrap();
        assert_eq!(bark.running.as_deref(), Some("silent"));
    }
}
