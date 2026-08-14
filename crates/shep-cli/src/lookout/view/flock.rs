//! The flock table: which columns fit, and what each row's cells say.
//!
//! The visual contract is "the table `shep flock` prints, live", so this
//! builds `Line`s itself rather than handing the job to
//! `ratatui::widgets::Table`. `crate::output::table::render_table` already
//! owns the house column algorithm — widest cell, two spaces between, no
//! box-drawing — and a second, independent algorithm beside it would drift on
//! the first multi-byte name. Cell values come from the same
//! `crate::output::{human_bytes, human_duration}` the CLI's own rows use, so a
//! number reads identically in both surfaces.
//!
//! Widths here are FIXED per column rather than measured from content, which
//! is the one deliberate departure from `render_table`: a live table whose
//! columns resize as a pid gains a digit is a table that shivers.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{App, Row};
use crate::output::{human_bytes, human_duration};

/// The narrowest terminal the pane will draw into.
///
/// `ID` + `NAME` (floor 8) + `STATUS` (15, the width of `waiting-restart`)
/// plus two separators. Below this the whole draw becomes one line saying
/// so — see [`super::draw`].
pub const MIN_WIDTH: u16 = 31;

/// The shortest terminal the pane will draw into: title, banner, header,
/// rule, one data row, status bar.
pub const MIN_HEIGHT: u16 = 6;

/// The floor on the NAME column, which takes whatever the fixed columns
/// leave.
pub const NAME_MIN: u16 = 8;

/// One column of the flock table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// The sheep's stable numeric id.
    Id,
    /// Its name. The flexible column.
    Name,
    /// Its lifecycle status — the one coloured cell.
    Status,
    /// Its OS pid while running.
    Pid,
    /// Restarts since registration.
    Restarts,
    /// Tree CPU as a percentage of one core.
    Cpu,
    /// Tree resident set size.
    Mem,
    /// Time since its last successful start.
    Uptime,
    /// Fold membership.
    Fold,
}

impl Column {
    /// The header text, matching `output::rows::FlockRows::headers` exactly
    /// — one vocabulary across both surfaces.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Id => "ID",
            Self::Name => "NAME",
            Self::Status => "STATUS",
            Self::Pid => "PID",
            Self::Restarts => "RESTARTS",
            Self::Cpu => "CPU",
            Self::Mem => "MEM",
            Self::Uptime => "UPTIME",
            Self::Fold => "FOLD",
        }
    }

    /// The fixed width of this column's cells. `Name` reports `0` — it is
    /// the column that takes the remainder, and [`name_width`] computes it.
    #[must_use]
    pub const fn width(self) -> u16 {
        match self {
            Self::Id => 4,
            Self::Name => 0,
            // 15: `waiting-restart`, the longest of the six statuses. A
            // status is never truncated — it is the pane.
            Self::Status => 15,
            Self::Pid => 7,
            Self::Restarts => 8,
            Self::Cpu => 6,
            Self::Mem => 8,
            Self::Uptime => 8,
            Self::Fold => 10,
        }
    }
}

const ALL: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Restarts,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
    Column::Fold,
];
const NO_FOLD: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Restarts,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
];
const NO_RESTARTS: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
];
const NO_PID: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
];
const NO_MEM: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Cpu,
    Column::Uptime,
];
const NO_CPU: &[Column] = &[Column::Id, Column::Name, Column::Status, Column::Uptime];
const FLOOR: &[Column] = &[Column::Id, Column::Name, Column::Status];

/// Width thresholds, widest first. Each entry is the narrowest terminal that
/// still gets that column set.
///
/// The drop order is least-diagnostic first and it is a decision, not an
/// accident of ordering: FOLD is grouping metadata rather than health;
/// RESTARTS and PID answer follow-up questions rather than "is it up"; CPU
/// and MEM are the last two numbers to go because they are the ones that
/// explain WHY something is wrong. `ID NAME STATUS` is the floor because
/// those three are the pane.
const TIERS: &[(u16, &[Column])] = &[
    (90, ALL),
    (78, NO_FOLD),
    (68, NO_RESTARTS),
    (59, NO_PID),
    (49, NO_MEM),
    (41, NO_CPU),
    (MIN_WIDTH, FLOOR),
];

/// The widest column set that fits `width`.
#[must_use]
pub fn columns_for(width: u16) -> &'static [Column] {
    TIERS
        .iter()
        .find(|(threshold, _)| width >= *threshold)
        .map_or(FLOOR, |(_, columns)| *columns)
}

/// What NAME gets, once the fixed columns and the separators are paid for.
#[must_use]
pub fn name_width(width: u16, columns: &[Column]) -> u16 {
    let fixed: u16 = columns.iter().map(|column| column.width()).sum();
    let gaps = u16::try_from(columns.len().saturating_sub(1)).unwrap_or(0) * 2;
    width
        .saturating_sub(fixed)
        .saturating_sub(gaps)
        .max(NAME_MIN)
}

/// `text` in exactly `width` characters: padded on the right, or truncated
/// with a trailing `…`.
///
/// Counted in `char`s, never bytes — `{:<w$}` pads by character count, so a
/// byte measurement over-pads every multi-byte name. `output::table` records
/// having made the same choice for the same reason.
///
/// A truncated name that looked whole would be a name an operator types into
/// `shep stop`, so the ellipsis is not cosmetic.
#[must_use]
pub fn fit(text: &str, width: u16) -> String {
    let width = usize::from(width);
    let count = text.chars().count();
    if count <= width {
        let mut out = String::from(text);
        out.extend(core::iter::repeat_n(' ', width - count));
        return out;
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// The header line: every column name, muted.
#[must_use]
pub fn header_line(columns: &[Column], width: u16, style: Style) -> Line<'static> {
    let name = name_width(width, columns);
    let mut text = String::new();
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let cell_width = if *column == Column::Name {
            name
        } else {
            column.width()
        };
        text.push_str(&fit(column.header(), cell_width));
    }
    Line::from(Span::styled(text, style))
}

/// One sheep's line. The STATUS cell is the only one that carries colour.
///
/// No row style beyond that: 12a has no selected row (see the phase plan's
/// "What 12b gets"), so there is nothing here for a REVERSED modifier to
/// mean.
#[must_use]
pub fn row_line(app: &App, row: &Row, columns: &[Column], width: u16) -> Line<'static> {
    let palette = app.palette();
    let name = name_width(width, columns);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(columns.len() * 2);
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let cell_width = if *column == Column::Name {
            name
        } else {
            column.width()
        };
        let text = fit(&cell(app, row, *column), cell_width);
        let style = if *column == Column::Status {
            palette.status(row.info.status)
        } else {
            Style::default()
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// One cell's text.
///
/// `-` rather than an empty cell for every unknown, exactly as
/// `output::rows::FlockRows::rows` does and for the same stated reason: an
/// empty cell in a padded table is indistinguishable from a rendering bug,
/// and `0.0%` would claim a measurement the shepherd never made.
fn cell(app: &App, row: &Row, column: Column) -> String {
    let info = &row.info;
    match column {
        Column::Id => info.id.to_string(),
        Column::Name => info.name.clone(),
        Column::Status => info.status.to_string(),
        Column::Pid => info
            .pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        Column::Restarts => info.restarts.to_string(),
        Column::Cpu => info
            .cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        Column::Mem => info
            .memory_bytes
            .map_or_else(|| "-".to_string(), human_bytes),
        // The live value, not the snapshot's — `App::uptime_ms` advances a
        // RUNNING sheep between polls and stops entirely once the link is
        // lost.
        Column::Uptime => app
            .uptime_ms(info.id)
            .map_or_else(|| "-".to_string(), human_duration),
        Column::Fold => info.fold.clone().unwrap_or_else(|| "-".to_string()),
    }
}

/// Which slice of the flock is on screen: the first row to draw, given what
/// `App` was asked for and how many rows there is room for.
///
/// `requested` is [`super::super::app::App::scroll`] — the reducer's own
/// offset, which it clamps to the flock's length but cannot clamp to the
/// terminal's height, because it does not know it. This is the second
/// clamp, and it is the one that stops a scrolled pane leaving blank rows
/// above the status bar: once the flock is taller than the viewport, the
/// furthest down it can go is the page that ends on the last row.
///
/// Derived every frame rather than stored: the flock map is replaced
/// wholesale every two seconds, and a stored *result* would have to be
/// reconciled against a list that changed underneath it.
#[must_use]
pub fn scroll_offset(requested: usize, viewport: usize, total: usize) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    requested.min(total - viewport)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the drop order changes without someone re-arguing it. FOLD is
    /// grouping metadata and goes first; CPU and MEM are the last two numbers
    /// to go because they are the ones that explain WHY something is wrong;
    /// ID/NAME/STATUS is the floor because those three are the pane.
    #[test]
    fn columns_drop_in_a_fixed_order_as_the_terminal_narrows() {
        assert_eq!(columns_for(200).len(), 9);
        assert_eq!(columns_for(90).len(), 9);
        assert!(!columns_for(89).contains(&Column::Fold));
        assert!(columns_for(89).contains(&Column::Restarts));
        assert!(!columns_for(67).contains(&Column::Restarts));
        assert!(!columns_for(58).contains(&Column::Pid));
        assert!(!columns_for(48).contains(&Column::Mem));
        assert!(!columns_for(40).contains(&Column::Cpu));
        assert_eq!(columns_for(31), &[Column::Id, Column::Name, Column::Status]);
        // Every tier keeps the three that ARE the pane.
        for width in [31u16, 40, 48, 58, 67, 89, 200] {
            let cols = columns_for(width);
            for required in [Column::Id, Column::Name, Column::Status] {
                assert!(
                    cols.contains(&required),
                    "width {width} dropped {required:?}"
                );
            }
        }
    }

    /// fails if a tier can render wider than the terminal it was chosen for.
    /// This is the check that makes the table above a claim rather than a
    /// wish: every tier's fixed widths plus its separators plus the minimum
    /// NAME must fit in that tier's own threshold.
    #[test]
    fn every_tier_fits_the_width_it_claims() {
        for width in MIN_WIDTH..=200 {
            let cols = columns_for(width);
            let fixed: u16 = cols.iter().map(|c| c.width()).sum();
            let gaps = u16::try_from(cols.len() - 1).unwrap() * 2;
            assert!(
                fixed + gaps + NAME_MIN <= width,
                "width {width} chose {} columns needing {}",
                cols.len(),
                fixed + gaps + NAME_MIN
            );
        }
    }

    /// fails if a long name is cut without saying so. A truncated name that
    /// looks whole is a name an operator will type into `shep stop`.
    #[test]
    fn a_name_too_long_for_its_column_ends_in_an_ellipsis() {
        let cut = fit("payments-reconciliation-worker", 12);
        assert_eq!(cut.chars().count(), 12);
        assert!(cut.ends_with('…'));
        assert!(cut.starts_with("payments"));
        assert_eq!(fit("web", 12), "web         ");
    }

    /// fails if `fit` starts counting bytes. `output::table::render_table`'s
    /// own doc records having avoided the same bug for the same reason.
    ///
    /// **These assert on CONTENT, not on length**, and that is the whole
    /// point: an earlier draft of this test only checked
    /// `fit(..).chars().count() == width`, which is `width` in *both*
    /// branches under either measurement — so the byte-vs-char mutation
    /// could not redden it. The observable difference lives in the PAD
    /// branch, where a three-character nine-byte name asks for nine columns
    /// of padding budget it does not need and comes out short, and at the
    /// exactly-fits boundary, where a byte count truncates a string that
    /// already fits.
    #[test]
    fn fit_counts_characters_not_bytes_when_it_pads_and_when_it_truncates() {
        // Pad branch. `日本語` is 3 chars / 9 bytes: char-counted it gets 3
        // trailing spaces, byte-counted it falls into the truncate branch.
        assert_eq!(fit("日本語", 6), "日本語   ");
        // Exactly fits. 7 chars / 11 bytes — a byte count cuts it to
        // `ünïcöd…`.
        assert_eq!(fit("ünïcödé", 7), "ünïcödé");
        // Truncate branch, pinning which prefix survives.
        assert_eq!(fit("日本語アプリ", 5), "日本語ア…");
    }

    /// fails if the viewport stops being clamped to the rows that exist. An
    /// offset past the last row draws an empty pane, which reads as a crash
    /// rather than as a short flock; an offset inside a flock that fits on
    /// screen would scroll rows off the top for no reason.
    #[test]
    fn the_scroll_offset_never_leaves_a_gap_at_the_bottom() {
        // Everything fits: no scrolling, whatever was asked for.
        assert_eq!(scroll_offset(0, 10, 6), 0);
        assert_eq!(scroll_offset(4, 10, 6), 0);
        // Taller than the viewport: the last page is the ceiling, so the
        // bottom row is always the flock's own last row.
        assert_eq!(scroll_offset(0, 5, 20), 0);
        assert_eq!(scroll_offset(7, 5, 20), 7);
        assert_eq!(scroll_offset(19, 5, 20), 15);
        assert_eq!(scroll_offset(usize::MAX, 5, 20), 15);
        // Degenerate: a viewport of zero rows scrolls nowhere.
        assert_eq!(scroll_offset(3, 0, 20), 0);
    }
}
