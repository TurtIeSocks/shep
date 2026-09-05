//! The settings screen: `[daemon]`, `[whistle]`, `[style]`, then `[dogs]`,
//! drawn straight into the buffer since this screen owns the whole body
//! between the title and the status bar.
//!
//! Every row goes through [`fit`]: a value cut short ends in `…` rather
//! than spilling into the next column.
//!
//! Four scalars (`log_level`, `log_json`, `allow_control`, `style level`)
//! cycle; `socket` and `max_cron_sleep` are free text via
//! [`Settings::typing`], the only two fields an empty buffer can unset. The
//! dogs table's RUNNING column ([`dog_rows`]) joins the file's own
//! `enabled`/`SOURCE` against the live flock.

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::super::app::{App, Settings, SettingsRow};
use super::super::theme::Palette;
use super::flock::{fit, mark};
use super::scroll::Attempt;
use crate::commands::settings::{ScalarView, SettingField, SettingsSnapshot};
use crate::style::StyleSource;
use crate::vocabulary::Reported;

/// The dogs caption, verbatim: arms and confirms like every other lookout
/// action (`x`, `R`, `L`).
const DOGS_CAPTION: &str = "space arms, Enter applies; a dog needs no reload";

/// The floor on the dogs table's NAME column, mirroring
/// [`super::flock::NAME_MIN`]: never shrinks below a name worth reading.
const DOG_NAME_MIN: u16 = 8;

/// Columns spent on the selection mark and the space after it, before any
/// cell is drawn.
///
/// Both the dogs table and the scalar rows draw [`mark`] plus a space, so
/// the width a tier is chosen for, and its cells fitted into, is the
/// terminal minus this, never the terminal itself. Mirrors
/// [`super::flock::GUTTER`].
const GUTTER: u16 = 2;

/// The width the rows themselves are laid out in: the terminal minus
/// [`GUTTER`].
const fn body_width(width: u16) -> u16 {
    width.saturating_sub(GUTTER)
}

/// One column of the dogs table.
///
/// `Debug` is derived, not redacted: a bare variant name leaks nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DogColumn {
    /// The dog's name. The flexible column.
    Name,
    /// Whether `[daemon] enabled_dogs` names it, not whether the shepherd
    /// has started it.
    InFile,
    /// `built-in`, or the adopted binary's path.
    Source,
    /// Whether the dog is up: [`dog_rows`] joins it against the live
    /// flock, reading `not running` when no dog by that name is running.
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

    /// The fixed width of this column's cells. `Name` reports `0`; see
    /// [`super::flock::Column::width`].
    #[must_use]
    const fn width(self) -> u16 {
        match self {
            Self::Name => 0,
            // `IN FILE` header; yes/no cells are shorter.
            Self::InFile => 7,
            // budget for an adopted binary's path before `fit` truncates.
            Self::Source => 24,
            // matches `super::flock::Column::Status`'s width.
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

/// The narrowest the dogs table will draw into: [`DogColumn::Running`] plus
/// [`DOG_NAME_MIN`] plus one gap.
///
/// A table width, not a terminal width, like [`super::flock::MIN_WIDTH`]:
/// the narrowest terminal this table fits is `DOG_MIN_WIDTH + GUTTER`.
/// [`DOG_TIERS`]'s thresholds compare against [`body_width`], never the raw
/// terminal.
const DOG_MIN_WIDTH: u16 = DogColumn::Running.width() + DOG_NAME_MIN + 2;

/// Width thresholds, widest first, mirroring [`super::flock::TIERS`]:
/// least-diagnostic column drops first. SOURCE (widest) drops before IN
/// FILE; NAME and RUNNING are the floor.
const DOG_TIERS: &[(u16, &[DogColumn])] = &[
    (61, ALL_DOG_COLUMNS),
    (34, NO_SOURCE),
    (DOG_MIN_WIDTH, FLOOR_DOG_COLUMNS),
];

/// The widest dogs-table column set that fits `width`.
///
/// Includes [`DogColumn::Running`] in every tier, alongside NAME: it is the
/// floor, and `draw_settings` renders every column this returns.
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

/// One row of the dogs table: the file joined against the live flock, by
/// name. [`DogView`](crate::commands::settings::DogView) supplies `enabled`
/// and `adopted_path`; [`dog_rows`] adds `running`.
///
/// `Debug` is derived, not redacted: name, bool, word and path, none of it
/// a secret. A dog's own config lives in `dogs.toml`, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogRow {
    /// The dog's name.
    pub name: String,
    /// Whether `[daemon] enabled_dogs` names it, not whether the shepherd
    /// has started it.
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
/// Takes `app` rather than `Settings` alone: the file half
/// ([`SettingsSnapshot::dogs`]) and the live flock (`App::all_rows`) live on
/// two different types. Returns an empty vector when the settings screen
/// is not open.
#[must_use]
pub fn dog_rows(app: &App, width: u16) -> Vec<DogRow> {
    // unused: every dogs-table tier keeps `DogColumn::Running`, so no width
    // leaves the join unrendered. Kept for signature parity with the other
    // view functions.
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

/// The name printed in the NAME cell: the document's own key, except
/// `[style] level`, which drops the `style_` a section header already says.
///
/// `pub(super)`: `status::status_line` names the field being typed with
/// this same word, so the status bar and body pane agree.
pub(super) const fn field_label(field: SettingField) -> &'static str {
    field.key()
}

/// What applying this field costs. `log_level`, `log_json` and
/// `max_cron_sleep` need a daemon reload; `socket` needs a stop and start,
/// since reload never moves the listening socket; `allow_control` needs
/// whistle restarted, not the daemon.
///
/// `style level` reads `source` too: costs nothing when the file decides,
/// since lookout re-resolves it next command, but `--style` and
/// `$SHEP_STYLE` outrank the file and make that untrue.
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
/// VALUE column width where the terminal can afford it: fits
/// `/home/ada/.shep/run/shep.sock` (29 columns) whole. A cap, not a fixed
/// width: a narrow terminal gets [`SCALAR_VALUE_MIN`] instead, and anything
/// wider truncates through [`fit`].
const SCALAR_VALUE_W: u16 = 30;
/// The floor on VALUE: enough for `not set`, `waiting`, a level name or the
/// tail of a path, never a whole socket path. Below this the row stops
/// saying anything and there is nothing left to trade.
const SCALAR_VALUE_MIN: u16 = 10;
/// SOURCE column width: `$SHEP_STYLE` and `the default` are both 11 columns,
/// the widest two words [`crate::style::StyleSource::Display`] ever prints.
const SCALAR_SOURCE_W: u16 = 11;
/// The floor on the apply-cost column once the terminal is too narrow to
/// give it the remainder: enough for `needs`, never a whole sentence.
const SCALAR_COST_MIN: u16 = 8;

/// One column of a scalar row.
///
/// `Debug` is derived, not redacted: a bare variant name leaks nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarColumn {
    /// The document's own key for this field.
    Name,
    /// What the file says, or the compiled fallback when it says nothing.
    /// The flexible column, between [`SCALAR_VALUE_MIN`] and
    /// [`SCALAR_VALUE_W`].
    Value,
    /// Which layer the value came from.
    Source,
    /// What applying an edit to this field costs. Takes whatever the other
    /// three leave.
    Cost,
}

const SCALAR_ALL: &[ScalarColumn] = &[
    ScalarColumn::Name,
    ScalarColumn::Value,
    ScalarColumn::Source,
    ScalarColumn::Cost,
];
const SCALAR_NO_COST: &[ScalarColumn] = &[
    ScalarColumn::Name,
    ScalarColumn::Value,
    ScalarColumn::Source,
];
const SCALAR_FLOOR: &[ScalarColumn] = &[ScalarColumn::Name, ScalarColumn::Value];

/// The narrowest the scalar rows will draw into, as a body width: NAME, a
/// gap and VALUE at its floor. The terminal also pays [`GUTTER`], the same
/// split [`DOG_MIN_WIDTH`] documents.
const SCALAR_MIN_WIDTH: u16 = SCALAR_NAME_W + 2 + SCALAR_VALUE_MIN;

/// Width thresholds for the scalar rows, widest first, the same shape
/// [`DOG_TIERS`] and [`super::flock::TIERS`] both use.
///
/// The apply cost drops first, then SOURCE, the reverse of the dogs
/// table's order: the cost also shows in the confirm and the status bar,
/// while SOURCE appears nowhere else. NAME and VALUE are the floor.
const SCALAR_TIERS: &[(u16, &[ScalarColumn])] = &[
    (
        SCALAR_NAME_W + 2 + SCALAR_VALUE_MIN + 2 + SCALAR_SOURCE_W + 2 + SCALAR_COST_MIN,
        SCALAR_ALL,
    ),
    (
        SCALAR_NAME_W + 2 + SCALAR_VALUE_MIN + 2 + SCALAR_SOURCE_W,
        SCALAR_NO_COST,
    ),
    (SCALAR_MIN_WIDTH, SCALAR_FLOOR),
];

/// The widest scalar column set that fits `width`, a BODY width.
fn scalar_columns_for(width: u16) -> &'static [ScalarColumn] {
    SCALAR_TIERS
        .iter()
        .find(|(threshold, _)| width >= *threshold)
        .map_or(SCALAR_FLOOR, |(_, columns)| *columns)
}

/// What VALUE and the apply cost get out of `width`, once NAME, SOURCE and
/// the separators are paid for.
///
/// VALUE takes the remainder up to [`SCALAR_VALUE_W`] and never below
/// [`SCALAR_VALUE_MIN`]; the cost takes what is left after that, so a wide
/// terminal spends its extra columns on the sentence rather than on padding
/// a value that has already been shown whole. The cost's number is zero
/// when [`ScalarColumn::Cost`] is not in `columns`, and nothing reads it
/// then.
fn scalar_widths(width: u16, columns: &[ScalarColumn]) -> (u16, u16) {
    let fixed = SCALAR_NAME_W
        + if columns.contains(&ScalarColumn::Source) {
            SCALAR_SOURCE_W
        } else {
            0
        };
    let gaps = u16::try_from(columns.len().saturating_sub(1)).unwrap_or(0) * 2;
    let remainder = width.saturating_sub(fixed).saturating_sub(gaps);
    if columns.contains(&ScalarColumn::Cost) {
        let value = remainder
            .saturating_sub(SCALAR_COST_MIN)
            .clamp(SCALAR_VALUE_MIN, SCALAR_VALUE_W);
        (value, remainder.saturating_sub(value).max(SCALAR_COST_MIN))
    } else {
        (remainder.clamp(SCALAR_VALUE_MIN, SCALAR_VALUE_W), 0)
    }
}

/// One scalar cell's text.
fn scalar_cell(field: SettingField, view: &ScalarView, column: ScalarColumn) -> String {
    match column {
        ScalarColumn::Name => field_label(field).to_owned(),
        ScalarColumn::Value => view.value.clone(),
        ScalarColumn::Source => view.source.to_string(),
        ScalarColumn::Cost => apply_cost(field, view.source).to_string(),
    }
}

/// One scalar row: name, value, source, apply cost, as many of those as
/// `width` (a BODY width) can pay for.
fn scalar_line(
    field: SettingField,
    view: &ScalarView,
    selected: bool,
    width: u16,
) -> Line<'static> {
    let columns = scalar_columns_for(width);
    let (value_width, cost_width) = scalar_widths(width, columns);
    let mut text = format!("{} ", mark(selected));
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let cell_width = match column {
            ScalarColumn::Name => SCALAR_NAME_W,
            ScalarColumn::Value => value_width,
            ScalarColumn::Source => SCALAR_SOURCE_W,
            ScalarColumn::Cost => cost_width,
        };
        text.push_str(&fit(&scalar_cell(field, view, *column), cell_width));
    }
    Line::from(Span::raw(text))
}

/// Which `[section]` a scalar field's row lives under.
fn section_for(settings: &Settings, field: SettingField) -> &str {
    settings
        .fields()
        .by_key(field.key())
        .and_then(|f| f.group.as_deref())
        .unwrap_or("")
}

/// Every line of the screen's body, top to bottom: `[daemon]`'s four rows,
/// `[whistle]`'s one, `[style]`'s one, then `[dogs]`'s caption and table.
///
/// Walks [`Settings::rows`] rather than hand-listing the six scalars again:
/// the cursor, which [`Settings::rows`] alone defines, would otherwise land
/// on a row this function drew somewhere else.
///
/// Takes `app` as well as `settings`: [`dog_rows`] needs the live flock,
/// which lives on `App`, not on `Settings`.
///
/// `height` is the body's row budget; zero means unlimited, for a test
/// with no real terminal. [`Viewport`] scrolls in data rows while height
/// counts lines, so its offset is only a starting point: this walks
/// forward from it until the cursor's row fits, through
/// [`super::scroll::to_cursor`], shared with the config pane.
///
/// [`cursor_only`] is that walk's last resort, for a body too short for
/// even one row's own chrome.
///
/// [`Viewport`]: crate::lookout::viewport::Viewport
fn content_lines(
    app: &App,
    settings: &Settings,
    palette: Palette,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let total_rows = settings.rows().len();
    let cursor_row = settings.view().cursor().min(total_rows.saturating_sub(1));
    let budget = if height == 0 {
        usize::MAX
    } else {
        usize::from(height)
    };
    // Six scalars are unconditional, so an empty screen is unreachable
    // today. Handled rather than indexed on faith, because the fallback
    // below has a row to draw and this does not.
    if total_rows == 0 {
        return body_from(app, settings, palette, width, budget, 0).lines;
    }
    super::scroll::to_cursor(
        cursor_row,
        settings.view().offset(),
        |offset| body_from(app, settings, palette, width, budget, offset),
        || cursor_only(app, settings, palette, width, budget, cursor_row),
    )
}

/// The cursor's own row, alone, for a body too short to hold the chrome
/// that row's section costs.
///
/// The last resort, reached only when every offset down to the cursor's own
/// left it undrawn. `MIN_HEIGHT` is six rows, four of body, and a dog row
/// needs the blank line, the `[dogs]` header, the caption and the column
/// header above it before it may be drawn at all: six lines for one row.
/// A screen that declares a minimum height should draw something at it,
/// and the selected row is the something.
///
/// Markers are added around it while they fit, the cursor's row first: it
/// is the one line this function exists to guarantee.
fn cursor_only(
    app: &App,
    settings: &Settings,
    palette: Palette,
    width: u16,
    budget: usize,
    cursor_row: usize,
) -> Vec<Line<'static>> {
    let rows = settings.rows();
    let table_width = body_width(width);
    let mut lines = Vec::new();
    match rows.get(cursor_row) {
        Some(SettingsRow::Scalar(field)) => lines.push(scalar_line(
            *field,
            scalar_view(settings.snapshot(), *field),
            true,
            table_width,
        )),
        Some(SettingsRow::Dog(index)) => {
            let dogs = dog_rows(app, table_width);
            if let Some(dog) = dogs.get(*index) {
                lines.push(dog_line(dog, columns_for(table_width), table_width, true));
            }
        }
        None => {}
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

/// Lays the body out from data row `offset`, spending at most `budget`
/// lines.
///
/// Every line pushed is counted, including the ones no data row owns. The
/// `... N above` marker and the `... N below` marker are both reserved for
/// before a row is admitted rather than appended afterwards, so a height
/// that binds cuts a row instead of cutting the sentence that says a row
/// was cut.
fn body_from(
    app: &App,
    settings: &Settings,
    palette: Palette,
    width: u16,
    budget: usize,
    offset: usize,
) -> Attempt {
    let snapshot = settings.snapshot();
    let cursor = settings.cursor();
    let rows = settings.rows();
    let total_rows = rows.len();
    let cursor_row = settings.view().cursor().min(total_rows.saturating_sub(1));
    // The `... N above` marker is inserted at the top once everything under
    // it is laid out, so its line is held back from the very first check.
    let above = usize::from(offset > 0);
    // Whether a row at `index` still leaves room for `need` more lines. The
    // `... N below` marker is only owed when a row follows this one: a row
    // that fills the last line and has nothing under it needs no marker.
    let room = |taken: usize, need: usize, index: usize| {
        taken + need + above + usize::from(index + 1 < total_rows) <= budget
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_section: Option<&str> = None;
    // A section's header, and the blank line ahead of it for every
    // section after the first, held here rather than pushed straight
    // away: pushed alongside the first row of its section that survives
    // the offset skip, so the view still names a section it opens in.
    let mut pending_header: Vec<Line<'static>> = Vec::new();
    let mut drawn = 0usize;
    let mut row_index = 0usize;
    let mut full = false;

    // The scalar rows sort first in `Settings::rows`, ahead of every
    // `SettingsRow::Dog`, so `break` here stops once the scalars end.
    for row in rows.iter().copied() {
        let SettingsRow::Scalar(field) = row else {
            break;
        };
        let index = row_index;
        row_index += 1;
        let section = section_for(settings, field);
        if current_section != Some(section) {
            let mut header = Vec::new();
            if current_section.is_some() {
                header.push(Line::default());
            }
            header.push(section_header(section, palette));
            pending_header = header;
            current_section = Some(section);
        }
        if index < offset {
            continue;
        }
        if !room(lines.len(), pending_header.len() + 1, index) {
            full = true;
            break;
        }
        lines.append(&mut pending_header);
        lines.push(scalar_line(
            field,
            scalar_view(snapshot, field),
            cursor == Some(row),
            body_width(width),
        ));
        drawn += 1;
    }

    // `body_width`, not `width`: every line below draws `mark`'s own two
    // columns before its first cell.
    let table_width = body_width(width);
    if !full {
        let mut dogs_header = vec![Line::default(), section_header("[dogs]", palette)];
        // fitted: the caption is 48 columns and the screen draws from
        // `view::MIN_TERM_WIDTH` (33) up.
        dogs_header.push(Line::from(Span::styled(
            format!("  {}", fit(DOGS_CAPTION, table_width)),
            palette.muted(),
        )));
        let rendered_columns = columns_for(table_width);
        dogs_header.push(dog_header_line(rendered_columns, table_width, palette));

        let dogs = dog_rows(app, table_width);
        if dogs.is_empty() {
            // Nothing to gate a header this function will never draw
            // otherwise: the empty flock still gets to say "[dogs]", same
            // as `flock::render_table`'s own header for an empty payload.
            if lines.len() + dogs_header.len() + above <= budget {
                lines.append(&mut dogs_header);
            }
        } else {
            let mut pending_dogs_header = dogs_header;
            for (index, dog) in dogs.iter().enumerate() {
                let global_index = row_index + index;
                if global_index < offset {
                    continue;
                }
                if !room(lines.len(), pending_dogs_header.len() + 1, global_index) {
                    break;
                }
                lines.append(&mut pending_dogs_header);
                let selected = cursor == Some(SettingsRow::Dog(index));
                lines.push(dog_line(dog, rendered_columns, table_width, selected));
                drawn += 1;
            }
        }
    }

    // Counted off what this pass actually drew, not off `Viewport`'s own
    // arithmetic: the viewport hides rows against a line budget it cannot
    // see spent, so its answer and this one disagree once the chrome costs
    // anything.
    let hidden_below = total_rows.saturating_sub(offset + drawn);
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

    // One prompt line under the table, echoing the status bar's Slot 1
    // (`view::status::status_line`): `attention` styled while it waits on
    // `Enter`, an in-flight sentence once it has gone out. Both branches
    // cost two lines, so neither is pushed unless the budget has them.
    if lines.len() + 2 <= budget {
        if let Some(prompt) = settings.pending() {
            lines.push(Line::default());
            let text = if prompt.sent {
                format!("{}  sent, waiting for the shepherd", prompt.text)
            } else {
                format!("{}  enter confirms, any other key cancels", prompt.text)
            };
            lines.push(Line::from(Span::styled(
                format!("  {}", fit(&text, table_width)),
                palette.attention(),
            )));
        } else if let Some((field, buffer)) = settings.typing() {
            // The cursor is a character, not a style: the ANSI gallery
            // renders foregrounds only, so a reversed cell would come out
            // unstyled.
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    fit(
                        &format!(
                            "editing {}: {buffer}\u{258f}   enter applies   esc cancels",
                            field_label(*field)
                        ),
                        table_width,
                    )
                ),
                palette.attention(),
            )));
        }
    }

    Attempt {
        cursor_drawn: drawn > 0 && (offset..offset + drawn).contains(&cursor_row),
        lines,
    }
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
    for (offset, line) in content_lines(app, settings, palette, area.width, area.height)
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

    /// The snapshot pins the section order, every row's four columns, and
    /// the dogs table underneath.
    #[test]
    fn settings_at_a_comfortable_width() {
        let app = fixtures::app_in_settings();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| super::super::draw(&app, frame))
            .unwrap();
        insta::assert_snapshot!(render_text(terminal.backend().buffer()));
    }

    /// SOURCE is the widest column, so it drops first.
    #[test]
    fn the_dogs_source_column_drops_before_the_rest() {
        assert!(columns_for(120).contains(&DogColumn::Source));
        assert!(!columns_for(60).contains(&DogColumn::Source));
        assert!(
            columns_for(60).contains(&DogColumn::Running),
            "RUNNING is the diagnostic half and outlives SOURCE"
        );
    }

    /// Measures the rendered lines, not the declared widths: each line
    /// carries `mark`'s own two-column prefix, which a width sum would miss.
    #[test]
    fn every_dogs_tier_fits_the_width_it_claims() {
        let app = fixtures::app_in_settings_with_dog_drift();
        let settings = app.settings().unwrap();
        let palette = app.palette();
        for width in (DOG_MIN_WIDTH + GUTTER)..=200 {
            let rendered: Vec<String> = content_lines(&app, settings, palette, width, 0)
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

    /// Widths here are BODY widths, [`GUTTER`] columns narrower than the
    /// terminal.
    #[test]
    fn the_scalar_apply_cost_drops_before_its_source() {
        assert!(scalar_columns_for(118).contains(&ScalarColumn::Cost));
        assert!(!scalar_columns_for(43).contains(&ScalarColumn::Cost));
        assert!(
            scalar_columns_for(43).contains(&ScalarColumn::Source),
            "SOURCE is the screen's own subject and outlives the cost"
        );
        assert!(!scalar_columns_for(33).contains(&ScalarColumn::Source));
        assert!(
            scalar_columns_for(33).contains(&ScalarColumn::Value),
            "NAME and VALUE are the floor"
        );
    }

    #[test]
    fn every_settings_line_fits_the_terminal_it_was_drawn_for() {
        let app = fixtures::app_in_settings_with_dog_drift();
        let settings = app.settings().unwrap();
        let palette = app.palette();
        for width in super::super::MIN_TERM_WIDTH..=200 {
            for line in content_lines(&app, settings, palette, width, 0) {
                let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(
                    visible_width(&rendered) <= usize::from(width),
                    "width {width} drew {}: {rendered:?}",
                    visible_width(&rendered)
                );
            }
        }
    }

    #[test]
    fn the_dogs_caption_says_arms_not_applies() {
        assert_eq!(
            DOGS_CAPTION,
            "space arms, Enter applies; a dog needs no reload"
        );
    }

    #[test]
    fn the_cursor_mark_sits_on_exactly_the_selected_row() {
        let app = fixtures::app_in_settings();
        let settings = app.settings().unwrap();
        let palette = app.palette();
        let lines = content_lines(&app, settings, palette, 120, 0);
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

    /// The body as strings, at the height the terminal really has.
    fn body_at(app: &App, height: u16) -> Vec<String> {
        let settings = app.settings().unwrap();
        content_lines(app, settings, app.palette(), 120, height)
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    }

    /// `Viewport` scrolls in data rows while the height it is handed counts
    /// lines, so the section headers, blank separators, `[dogs]` caption
    /// and dog column header all have to be counted here too.
    #[test]
    fn a_short_terminal_scrolls_and_says_what_it_hid() {
        let mut app = fixtures::app_in_settings();
        let height = 10;
        app.note_body_rows(height);
        // Walk to the last row. Six scalars plus however many dogs the
        // fixture carries; `SelectLast` lands on the last one whatever the
        // count.
        app.update(Msg::Key(KeyPress::SelectLast));
        let text = body_at(&app, height);
        assert!(
            text.len() <= usize::from(height),
            "the body fits the height it was given: {text:?}"
        );
        assert!(text[0].contains("above"), "{text:?}");
        assert!(
            text.iter().any(|l| l.contains("[dogs]")),
            "the visible section is labelled"
        );
        assert_eq!(
            text.iter().filter(|l| l.starts_with('>')).count(),
            1,
            "the cursor's own row is drawn: {text:?}"
        );
        let settings = app.settings().unwrap();
        assert_eq!(settings.view().cursor(), settings.rows().len() - 1);
        // `Viewport` sees ten rows of room for nine rows of data and scrolls
        // nothing. The scroll in the frame above is the renderer's, which is
        // the only layer that knows what the chrome costs.
        assert_eq!(settings.view().offset(), 0);
    }

    /// The whole screen at `height`, through the same `note_body_rows` and
    /// `draw` the event loop runs before every frame.
    fn screen_at(app: &mut App, height: u16) -> String {
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

    /// Six is `view::MIN_HEIGHT`: four lines of body, where a dog row
    /// costs six lines to draw in place and falls back to drawing alone.
    #[test]
    fn the_cursor_survives_every_step_of_a_walk_down_and_back_up() {
        for height in [6u16, 7, 8, 10, 14, 20] {
            let mut app = fixtures::app_in_settings_with_dog_drift();
            let total = app.settings().unwrap().rows().len();
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

    /// `settings_short` exercises this only in its mild form: it opens at
    /// `[style]`, whose one row is the section, so the header sits where it
    /// would anyway. This opens inside `[daemon]`, three rows past that
    /// header, which still has to be drawn.
    #[test]
    fn a_window_opening_mid_section_still_draws_the_header_it_scrolled_past() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let text = screen_at(&mut app, 6);
        let body: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert!(
            body.iter().any(|line| line.trim() == "[daemon]"),
            "the section above the window is named: {text}"
        );
        assert!(
            !text.contains("log_level"),
            "and the rows between its header and the window are gone: {text}"
        );
        assert_eq!(marked(&text), 1, "{text}");
        assert!(
            body.iter().any(|line| line.starts_with("> max_cron_sleep")),
            "the marked row is the cursor's: {text}"
        );
    }

    /// The marker is pushed last, so a clip drops it first before it can
    /// say anything was cut.
    #[test]
    fn the_below_marker_survives_the_height_that_made_it_necessary() {
        let mut app = fixtures::app_in_settings();
        let height = 6;
        app.note_body_rows(height);
        // Cursor left on the first row, so everything hidden is hidden
        // below it.
        let text = body_at(&app, height);
        assert!(
            text.len() <= usize::from(height),
            "the body fits the height it was given: {text:?}"
        );
        assert!(
            text.last().is_some_and(|l| l.contains("below")),
            "the last line says how many rows were cut: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.starts_with("> log_level")),
            "the cursor's own row is drawn: {text:?}"
        );
    }

    /// No fixture in this file renders `Default`: `settings_snapshot` gives
    /// every scalar `StyleSource::Config`, one column shorter.
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

    #[test]
    fn the_style_cost_cell_stops_promising_the_next_command_when_it_is_outranked() {
        let app = fixtures::app_in_settings_with_shadowed_style(StyleSource::Env);
        let settings = app.settings().unwrap();
        let lines = content_lines(&app, settings, app.palette(), 120, 0);
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

    #[test]
    fn the_editor_line_names_its_field_and_shows_the_buffer() {
        let mut app = fixtures::app_in_settings_on(SettingField::Socket);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let settings = app.settings().unwrap();
        let palette = app.palette();
        let lines = content_lines(&app, settings, palette, 120, 0);
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

    /// A dog that never completed a handshake reads `silent`, not `online`.
    #[test]
    fn a_dog_that_never_handshook_reads_silent_here_too() {
        let app = fixtures::app_in_settings_with_silent_dog();
        let rows = dog_rows(&app, 120);
        let bark = rows.iter().find(|r| r.name == "bark").unwrap();
        assert_eq!(bark.running.as_deref(), Some("silent"));
    }
}
