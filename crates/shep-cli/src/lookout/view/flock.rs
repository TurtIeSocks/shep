//! The flock table: which columns fit, and what each row's cells say.
//!
//! Builds `Line`s directly rather than through `ratatui::widgets::Table`,
//! sourcing cell values from the same `crate::output::{human_bytes,
//! human_duration}` `shep flock` uses, so a number reads identically in
//! both surfaces.
//!
//! Column widths are fixed rather than measured from content: a live table
//! whose columns resize as a pid gains a digit is a table that shivers.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{App, GroupTotals, Row, RowKey};
use crate::output::width::char_columns;
use crate::output::{cfg_cell, exit_cell, human_bytes, human_duration};

/// The narrowest terminal the table will draw into.
///
/// `ID` + `NAME` (floor 8) + `STATUS` (15, the width of `waiting-restart`)
/// plus two separators. This is the table's own floor, not the terminal's:
/// the terminal also needs room for [`GUTTER`], the selection marker's
/// column.
pub const MIN_WIDTH: u16 = 31;

/// The shortest terminal the pane will draw into: title, banner, header,
/// rule, one data row, status bar.
pub const MIN_HEIGHT: u16 = 6;

/// The floor on the NAME column, which takes whatever the fixed columns
/// leave.
pub const NAME_MIN: u16 = 8;

/// The columns the selection marker takes, to the left of the table.
///
/// One for the marker, one for the gap. The table itself is rendered into
/// `width - GUTTER` starting at `x + GUTTER`, so every threshold in
/// [`TIERS`] and every arithmetic in [`name_width`] is untouched by the
/// marker.
pub const GUTTER: u16 = 2;

/// The marker for the selected row, or a blank for every other row.
///
/// A plain ASCII `>`, not a colour and not a `REVERSED` modifier: every
/// signal on this screen survives `NO_COLOR` and a 16-colour terminal, and a
/// decoration-only cursor does not. `▸` is East-Asian *Ambiguous* width, and a
/// terminal that renders it double-wide would shift every column of that one
/// row by a cell.
#[must_use]
pub const fn mark(selected: bool) -> &'static str {
    if selected { ">" } else { " " }
}

/// One column of the flock table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// The sheep's stable numeric id.
    Id,
    /// Its name. The flexible column.
    Name,
    /// Its lifecycle status, the one coloured cell.
    Status,
    /// Its OS pid while running.
    Pid,
    /// Restarts since registration.
    Restarts,
    /// Its last exit, once it is not running. Rendered by
    /// [`crate::output::exit_cell`], the same function
    /// `output::rows::FlockRows`'s own EXIT column calls.
    Exit,
    /// Whether a config load has parked a change for this sheep's next
    /// spawn, or an operator has overridden a field its Flockfile no longer
    /// declares. Rendered by [`crate::output::cfg_cell`], the same function
    /// `output::rows::FlockRows`'s own CFG column calls.
    Cfg,
    /// Tree CPU as a percentage of one core.
    Cpu,
    /// Tree resident set size.
    Mem,
    /// Time since its last successful start.
    Uptime,
    /// Fold membership.
    Fold,
    /// A short marker a dog attaches to a sheep over the client protocol's
    /// `SetSmit` request, last in the header order to match
    /// `output::rows::FlockRows`'s. shep paints what a dog wrote and never
    /// parses it.
    Smit,
}

impl Column {
    /// The header text, matching `output::rows::FlockRows::headers` exactly:
    /// one vocabulary across both surfaces, enforced by
    /// `the_full_column_set_matches_flock_rows_headers_exactly` below.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Id => "ID",
            Self::Name => "NAME",
            Self::Status => "STATUS",
            Self::Pid => "PID",
            Self::Restarts => "RESTARTS",
            Self::Exit => "EXIT",
            Self::Cfg => "CFG",
            Self::Cpu => "CPU",
            Self::Mem => "MEM",
            Self::Uptime => "UPTIME",
            Self::Fold => "FOLD",
            Self::Smit => "SMIT",
        }
    }

    /// The fixed width of this column's cells. `Name` reports `0`: it is
    /// the column that takes the remainder, and [`name_width`] computes it.
    #[must_use]
    pub const fn width(self) -> u16 {
        match self {
            Self::Id => 4,
            Self::Name => 0,
            // 15: `waiting-restart`, the longest word `Reported::word`
            // returns.
            Self::Status => 15,
            Self::Pid => 7,
            Self::Restarts => 8,
            // 9: `SIGVTALRM`/`SIGSTKFLT`, the longest names
            // `nix::sys::signal::Signal::as_str` returns.
            Self::Exit => 9,
            // 4: `!12`/`*12`, a `cfg_cell`'s own longest realistic value.
            Self::Cfg => 4,
            Self::Cpu => 6,
            Self::Mem => 8,
            Self::Uptime => 8,
            Self::Fold => 10,
            // 13: `visible_width("▲ main@a1b2c3")`, the measured width of the
            // real strings a deploy dog paints. A longer smit truncates via
            // [`fit`] rather than growing the column.
            Self::Smit => 13,
        }
    }
}

const ALL: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Restarts,
    Column::Exit,
    Column::Cfg,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
    Column::Fold,
    Column::Smit,
];
// The full set minus CFG, the first column dropped. See `TIERS`'s own doc
// for the drop order.
const NO_CFG: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Restarts,
    Column::Exit,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
    Column::Fold,
    Column::Smit,
];
const NO_SMIT: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Restarts,
    Column::Exit,
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
    Column::Exit,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
];
const NO_EXIT: &[Column] = &[
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
/// The drop order is least-diagnostic first: FOLD is grouping metadata,
/// RESTARTS and PID answer follow-up questions, CPU and MEM explain why a
/// running sheep is behaving badly, and EXIT renders `-` for every running
/// sheep, the common case. `ID NAME STATUS` is the floor.
///
/// CFG has its own drop tier, one tier before SMIT's, keeping `NO_CFG`'s
/// threshold at 116 so a 120-column terminal, the gallery's fixture width,
/// still shows SMIT.
const TIERS: &[(u16, &[Column])] = &[
    (122, ALL),
    (116, NO_CFG),
    (101, NO_SMIT),
    (89, NO_FOLD),
    (78, NO_EXIT),
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

/// `text` in exactly `width` display columns: padded on the right, or
/// truncated with a trailing `…`.
///
/// Counted in terminal columns, never bytes or `char`s: bytes over-pad a
/// multi-byte name, and `char`s under-pad a double-width one, shoving every
/// column after it out of line. A truncated name looking whole would be one
/// an operator types into `shep stop`.
///
/// An ANSI escape counts as the literal text a `Span` draws it as, unlike
/// [`crate::output::width::visible_width`], since nothing here writes to a
/// real terminal that would interpret it.
#[must_use]
pub fn fit(text: &str, width: u16) -> String {
    let width = usize::from(width);
    let columns: usize = text.chars().map(char_columns).sum();
    if columns <= width {
        let mut out = String::from(text);
        out.extend(core::iter::repeat_n(' ', width - columns));
        return out;
    }
    if width == 0 {
        return String::new();
    }
    // One column pays for the `…`. A double-width character straddling the
    // boundary is dropped rather than split, and its column is padded below
    // so the cell still measures `width`.
    let budget = width - 1;
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let c_width = char_columns(c);
        if used + c_width > budget {
            break;
        }
        out.push(c);
        used += c_width;
    }
    out.push('…');
    out.extend(core::iter::repeat_n(' ', budget - used));
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

/// One line for a row the table draws: a real sheep, or the header above an
/// app's grouped instances.
///
/// `key`'s `Sheep` ids always name a row still in the flock in practice, so
/// the blank fallback below is never drawn; it exists rather than an
/// `expect`, on the same "no honest value" rule this table applies to a
/// missing pid.
///
/// A `Sheep` row under a group header draws as a slot rather than a
/// standalone sheep ([`App::is_grouped`]), so a header reading `web ×3` is
/// not followed by three rows each repeating `web`.
#[must_use]
pub fn key_line(app: &App, key: &RowKey, columns: &[Column], width: u16) -> Line<'static> {
    match key {
        RowKey::Sheep(id) => app.row(*id).map_or_else(
            || Line::from(Span::raw(" ".repeat(usize::from(width)))),
            |row| row_line(app, row, columns, width, app.is_grouped(&row.info.name)),
        ),
        RowKey::Group(name) => group_line(app, name, columns, width),
    }
}

/// An app's group header row: [`App::group_totals`]'s own rollup, in the
/// same columns [`row_line`] uses for a real sheep. Mirrors
/// `output::rows::FlockRows`'s own group row so the two surfaces never
/// disagree about what an app's instances add up to.
///
/// No row style beyond STATUS, the same rule [`row_line`] follows: the
/// selected row is shown by the marker in the gutter column ([`mark`]).
fn group_line(app: &App, name: &str, columns: &[Column], width: u16) -> Line<'static> {
    let palette = app.palette();
    let totals = app.group_totals(name);
    let name_width = self::name_width(width, columns);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(columns.len() * 2);
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let cell_width = if *column == Column::Name {
            name_width
        } else {
            column.width()
        };
        let text = fit(&group_cell(app, name, *column, &totals), cell_width);
        let style = if *column == Column::Status {
            // `palette.status`, not `palette.reported`: a group row is
            // always an app's own instances, never a dog, so it has nothing
            // to be silent about.
            app.group_uniform_status(name)
                .map_or(Style::default(), |status| palette.status(status))
        } else {
            Style::default()
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// One cell of an app's group header row.
///
/// ID, PID, EXIT and CFG are blank, not `-`: there is no single value for a
/// group row to have "no honest value" about, since a load can park a
/// different set of fields on each slot. FOLD and SMIT read the first
/// member's, since both are per-app facts every instance shares.
fn group_cell(app: &App, name: &str, column: Column, totals: &GroupTotals) -> String {
    match column {
        Column::Id | Column::Pid | Column::Exit | Column::Cfg => String::new(),
        Column::Name => format!("{name} \u{d7}{}", totals.count),
        Column::Status => app.group_status_text(name),
        Column::Restarts => totals.restarts.to_string(),
        Column::Cpu => totals
            .cpu
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        Column::Mem => totals.memory.map_or_else(|| "-".to_string(), human_bytes),
        Column::Uptime => totals
            .uptime_ms
            .map_or_else(|| "-".to_string(), human_duration),
        Column::Fold => app
            .group_members(name)
            .first()
            .and_then(|row| row.info.fold.clone())
            .unwrap_or_else(|| "-".to_string()),
        Column::Smit => app
            .group_members(name)
            .first()
            .and_then(|row| row.info.smit.clone())
            .unwrap_or_else(|| "-".to_string()),
    }
}

/// One sheep's line. The STATUS cell is the only one that carries colour.
///
/// No row style beyond that: the selected row is shown by the marker in the
/// gutter column ([`mark`]), not by a `REVERSED` modifier on the row's own
/// text.
///
/// `grouped` says whether a group header sits above this row, which is the
/// only thing that changes NAME, FOLD and SMIT. See [`cell`].
#[must_use]
pub fn row_line(
    app: &App,
    row: &Row,
    columns: &[Column],
    width: u16,
    grouped: bool,
) -> Line<'static> {
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
        let text = fit(&cell(app, row, *column, grouped), cell_width);
        let style = if *column == Column::Status {
            palette.reported(row.reported())
        } else {
            Style::default()
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// One cell's text.
///
/// `-` rather than an empty cell for every unknown: an empty cell in a
/// padded table is indistinguishable from a rendering bug, and `0.0%` would
/// claim a measurement the shepherd never made.
///
/// `grouped` changes three cells, matching `output::rows::slot_row`. NAME
/// becomes `↳ :2`, teaching the `web:2` selector by sitting under the name
/// the header already printed; FOLD and SMIT go blank rather than `-`,
/// since the group row above carries both.
fn cell(app: &App, row: &Row, column: Column, grouped: bool) -> String {
    let info = &row.info;
    match column {
        Column::Id => info.id.to_string(),
        Column::Name if grouped => info
            .instance
            .map_or_else(String::new, |slot| format!(" \u{21b3} :{slot}")),
        Column::Name => info.name.clone(),
        // `Row::reported`, not `info.status.to_string()`: a dog that has
        // never handshook must not read `online` here any more than it does
        // in `shep flock`'s own table.
        Column::Status => row.reported().word(),
        Column::Pid => info
            .pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        Column::Restarts => info.restarts.to_string(),
        // `crate::output::exit_cell`, not a second implementation of the
        // code/signal split.
        Column::Exit => exit_cell(info.pid, info.last_exit),
        // `crate::output::cfg_cell`, not a second implementation of the
        // pending-over-overridden precedence.
        Column::Cfg => cfg_cell(info.pending.as_deref(), info.overridden.as_deref()),
        Column::Cpu => info
            .cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        Column::Mem => info
            .memory_bytes
            .map_or_else(|| "-".to_string(), human_bytes),
        // The live value, not the snapshot's: `App::uptime_ms` advances a
        // running sheep between polls and stops once the link is lost.
        Column::Uptime => app
            .uptime_ms(info.id)
            .map_or_else(|| "-".to_string(), human_duration),
        Column::Fold | Column::Smit if grouped => String::new(),
        Column::Fold => info.fold.clone().unwrap_or_else(|| "-".to_string()),
        Column::Smit => info.smit.clone().unwrap_or_else(|| "-".to_string()),
    }
}

/// Which slice of the flock is on screen, given where the cursor is.
///
/// Derived every frame from [`super::super::app::App::selected_index`] rather
/// than stored beside it: a stored offset and a stored cursor can disagree,
/// and this way they cannot. The selection is centred where the flock is long
/// enough to allow it and pinned at both ends where it is not, so the last row
/// of the flock is always the last row of the pane.
#[must_use]
pub fn scroll_offset(selected: usize, viewport: usize, total: usize) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    let last = total - viewport;
    selected.saturating_sub(viewport / 2).min(last)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures;
    use super::*;

    /// FOLD goes first (grouping metadata), then EXIT (silent for a running
    /// sheep, the common case), then RESTARTS and PID, then CPU and MEM (the
    /// last to explain why a running sheep misbehaves). ID/NAME/STATUS is
    /// the floor.
    #[test]
    fn columns_drop_in_a_fixed_order_as_the_terminal_narrows() {
        assert_eq!(columns_for(300).len(), 12);
        assert_eq!(columns_for(122).len(), 12);
        // CFG is the first column gone, ahead of even SMIT. See `TIERS`'s
        // own doc for the reasoning.
        assert!(!columns_for(121).contains(&Column::Cfg));
        assert!(columns_for(121).contains(&Column::Smit));
        assert_eq!(columns_for(116).len(), 11);
        assert!(!columns_for(115).contains(&Column::Smit));
        assert!(columns_for(115).contains(&Column::Fold));
        assert_eq!(columns_for(101).len(), 10);
        assert!(!columns_for(100).contains(&Column::Fold));
        assert!(columns_for(100).contains(&Column::Exit));
        assert!(!columns_for(88).contains(&Column::Exit));
        assert!(columns_for(88).contains(&Column::Restarts));
        assert!(!columns_for(77).contains(&Column::Restarts));
        assert!(!columns_for(67).contains(&Column::Pid));
        assert!(!columns_for(58).contains(&Column::Mem));
        assert!(!columns_for(48).contains(&Column::Cpu));
        assert_eq!(columns_for(31), &[Column::Id, Column::Name, Column::Status]);
        // Every tier keeps the three that are the pane.
        for width in [31u16, 40, 48, 58, 67, 77, 88, 100, 300] {
            let cols = columns_for(width);
            for required in [Column::Id, Column::Name, Column::Status] {
                assert!(
                    cols.contains(&required),
                    "width {width} dropped {required:?}"
                );
            }
        }
    }

    /// The gallery's own fixtures render at 120 columns, so this pins the
    /// one width a future column's drop-order choice must not narrow past.
    #[test]
    fn smit_survives_the_gallerys_own_wide_fixtures() {
        assert!(columns_for(120).contains(&Column::Smit));
    }

    /// Enforces `Column::header`'s claim of one vocabulary across both
    /// surfaces, rather than leaving it aspirational.
    #[test]
    fn the_full_column_set_matches_flock_rows_headers_exactly() {
        use crate::output::Render;

        let headers: Vec<&str> = ALL.iter().map(|column| column.header()).collect();
        assert_eq!(headers, crate::output::FlockRows::headers());
    }

    /// `cfg(unix)`: the fixture carries a signalled exit, which
    /// `output::rows::signal_label` resolves against the running platform's
    /// own signal table. A Windows `ExitOutcome` never carries a signal at
    /// all, so this arm is only ever reached by a synthetic fixture.
    #[cfg(unix)]
    #[test]
    fn the_exit_cell_reuses_the_same_rendering_flock_rows_uses() {
        use shep_core::protocol::{ExitInfo, ProcessInfo};
        use shep_core::status::ProcStatus;

        let crashed = ProcessInfo::builder(1, "crashed", ProcStatus::Errored)
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();
        let killed = ProcessInfo::builder(2, "killed", ProcStatus::Stopped)
            .last_exit(Some(ExitInfo {
                code: None,
                signal: Some(9),
            }))
            .build();
        let running = ProcessInfo::builder(3, "running", ProcStatus::Online)
            .pid(Some(4_242))
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();

        let app = fixtures::app_with(vec![crashed, killed, running], fixtures::plain());
        let rows = app.rows();
        let cell_for = |id: u32| {
            let row = rows.iter().find(|row| row.info.id == id).unwrap();
            cell(&app, row, Column::Exit, false)
        };

        assert_eq!(cell_for(1), "1");
        assert_eq!(cell_for(2), "SIGKILL");
        assert_eq!(
            cell_for(3),
            "-",
            "a running sheep has nothing for EXIT to say"
        );
    }

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

    #[test]
    fn a_name_too_long_for_its_column_ends_in_an_ellipsis() {
        let cut = fit("payments-reconciliation-worker", 12);
        assert_eq!(cut.chars().count(), 12);
        assert!(cut.ends_with('…'));
        assert!(cut.starts_with("payments"));
        assert_eq!(fit("web", 12), "web         ");
    }

    /// Asserts on content, not length: `fit(..).chars().count() == width`
    /// alone is `width` either way, blind to a byte-vs-char mutation. The
    /// pad branch under-pads a multi-byte name; the truncate branch cuts a
    /// string that already fits.
    #[test]
    fn fit_counts_columns_not_bytes_when_it_pads_and_when_it_truncates() {
        // Pad branch. `日本語` is 9 bytes and 6 columns: measured correctly
        // it fills a 6-wide cell, byte-counted it falls into the truncate
        // branch.
        assert_eq!(fit("日本語", 6), "日本語");
        // Exactly fits: 7 columns, 11 bytes. A byte count would cut it to
        // `ünïcöd…`.
        assert_eq!(fit("ünïcödé", 7), "ünïcödé");
    }

    /// A `char` count gives `日本語` three and lets it into a 3-wide cell it
    /// draws six columns in, and gives `日本語アプリ` a five-`char` prefix
    /// of `日本語ア…` that draws nine.
    ///
    /// Asserts on content and on measured columns: `chars().count()` alone
    /// is blind to the mutation, and here so is `len()`.
    #[test]
    fn a_double_width_name_is_cut_to_the_columns_it_draws_in() {
        // Truncate branch. Budget is 4 columns plus the `…`: two characters
        // fit, the third would draw past the cell.
        assert_eq!(fit("日本語アプリ", 5), "日本…");
        assert_eq!(columns_of(&fit("日本語アプリ", 5)), 5);
        // A `char` count would call this a pad (3 chars into 3 columns) and
        // emit `日本語`, six columns wide, with no `…` to say it was cut.
        assert_eq!(fit("日本語", 3), "日…");
        // The odd-width case the padding exists for: `日` fits the 2-column
        // budget, `本` does not, and half a character is not drawable, so
        // the leftover column is a space rather than a short cell.
        assert_eq!(fit("日本語", 4), "日… ");
        assert_eq!(columns_of(&fit("日本語", 4)), 4);
        // Nothing but the marker fits, at either width.
        assert_eq!(fit("日本語", 1), "…");
        assert_eq!(fit("日本語", 2), "… ");
    }

    /// Every caller concatenates cells with two-space separators and no
    /// caller re-measures, so one short cell shifts every column after it
    /// on that row, invisibly in a single cell's own test.
    #[test]
    fn every_cell_measures_exactly_the_width_it_was_given() {
        let names = [
            "web",
            "payments-reconciliation-worker",
            "日本語アプリ",
            "café",
            "cafe\u{301}",
            "羊",
            "",
        ];
        for name in names {
            for width in 0..=12u16 {
                let cell = fit(name, width);
                assert_eq!(
                    columns_of(&cell),
                    usize::from(width),
                    "fit({name:?}, {width}) == {cell:?}"
                );
            }
        }
    }

    /// An ANSI escape is text here, not styling. Measuring it as zero, which
    /// [`crate::output::width::visible_width`] does on its own path, would
    /// let a hostile log line claim more columns than the cell it was cut
    /// to fit.
    #[test]
    fn an_escape_sequence_is_measured_as_the_text_it_will_be_drawn_as() {
        let styled = "\u{1b}[32mup";
        // ESC is zero-width; `[32mup` is six columns.
        assert_eq!(columns_of(styled), 6);
        assert_eq!(columns_of(&fit(styled, 4)), 4);
        assert!(fit(styled, 4).ends_with('…'));
    }

    /// The columns a rendered cell actually draws in, by the same rule
    /// [`fit`] pads by. Not `chars().count()`: that is the measurement
    /// under test.
    fn columns_of(s: &str) -> usize {
        s.chars().map(char_columns).sum()
    }

    /// Colour and a `REVERSED` modifier are both rejected: every signal on
    /// this screen has to survive `NO_COLOR` and a 16-colour terminal. `>`
    /// rather than `▸`, since `▸` is East-Asian Ambiguous width and a
    /// terminal rendering it double-wide shifts every column of that row.
    #[test]
    fn the_marker_is_one_ascii_column_wide_in_both_states() {
        // The two literals are the test: `">"` is one ASCII char by
        // inspection, so a separate length/`is_ascii()` check is redundant.
        assert_eq!(
            mark(true),
            ">",
            "not `▸`: East-Asian Ambiguous width would shift the row"
        );
        assert_eq!(mark(false), " ");
    }

    #[test]
    fn the_offset_keeps_the_selection_visible_and_centred_where_it_can() {
        // Everything fits: no scrolling, wherever the cursor is.
        assert_eq!(scroll_offset(0, 10, 6), 0);
        assert_eq!(scroll_offset(5, 10, 6), 0);
        // Taller than the viewport: centred in the middle, pinned at the ends.
        assert_eq!(scroll_offset(0, 5, 20), 0);
        assert_eq!(scroll_offset(2, 5, 20), 0);
        assert_eq!(scroll_offset(3, 5, 20), 1);
        assert_eq!(scroll_offset(10, 5, 20), 8);
        assert_eq!(scroll_offset(19, 5, 20), 15, "the last page, not past it");
        assert_eq!(scroll_offset(usize::MAX, 5, 20), 15);
        // Degenerate: a viewport of zero rows scrolls nowhere.
        assert_eq!(scroll_offset(3, 0, 20), 0);
        // And the selection is always inside the window it returns.
        for total in [1usize, 2, 7, 40, 200] {
            for viewport in [1usize, 3, 8, 25] {
                for selected in 0..total {
                    let offset = scroll_offset(selected, viewport, total);
                    assert!(
                        selected >= offset && selected < offset + viewport,
                        "selected {selected} fell outside [{offset}, {}) for total {total}",
                        offset + viewport
                    );
                }
            }
        }
    }

    /// `web x3` in NAME, memory summed across instances, ID/PID/EXIT blank,
    /// and UPTIME the minimum rather than any one instance's own reading.
    /// Asserted on the rendered [`Line`], not on `App::group_totals`
    /// directly, so a change in either the arithmetic or the rendering has
    /// to redden this.
    #[test]
    fn a_group_rows_cells_show_the_apps_rollup() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let app = fixtures::app_with(
            vec![
                ProcessInfo::builder(1, "web", ProcStatus::Online)
                    .instance(Some(0))
                    .memory_bytes(Some(100 << 20))
                    .uptime_ms(120_000)
                    .build(),
                ProcessInfo::builder(2, "web", ProcStatus::Online)
                    .instance(Some(1))
                    .memory_bytes(Some(150 << 20))
                    .uptime_ms(30_000)
                    .build(),
                ProcessInfo::builder(3, "web", ProcStatus::Online)
                    .instance(Some(2))
                    .memory_bytes(Some(50 << 20))
                    .uptime_ms(600_000)
                    .build(),
            ],
            fixtures::plain(),
        );

        let line = key_line(&app, &RowKey::Group("web".to_string()), ALL, 200);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        // The exact row, column by column: a substring check on a run of
        // blank cells can pass by accident on neighbouring padding.
        let name = name_width(200, ALL);
        let expected = [
            fit("", Column::Id.width()),           // ID: blank, no single id
            fit("web \u{d7}3", name),              // NAME: app x instance count
            fit("online", Column::Status.width()), // STATUS: every instance agrees
            fit("", Column::Pid.width()),          // PID: blank, no single pid
            fit("0", Column::Restarts.width()),    // RESTARTS: summed, all zero
            fit("", Column::Exit.width()),         // EXIT: blank, no single exit
            fit("", Column::Cfg.width()),          // CFG: blank, per-instance fact
            fit("-", Column::Cpu.width()),         // CPU: no reading on any instance
            // 100 + 150 + 50 = 300 MiB, summed rather than averaged.
            fit("300.0M", Column::Mem.width()),
            // The MINIMUM across the three instances (30s), not the first
            // one's (120s) or the last one's (600s).
            fit("30s", Column::Uptime.width()),
            fit("-", Column::Fold.width()),
            fit("-", Column::Smit.width()),
        ]
        .join("  ");

        assert_eq!(rendered, expected, "got {rendered:?}");
    }

    /// A slot row drawn like a standalone sheep repeats FOLD and SMIT down
    /// every row and shows no slot number, unlike `shep flock`'s own `↳ :1`.
    ///
    /// Asserted on the rendered line rather than on `cell`, since the
    /// defect was in which caller `key_line` picked.
    #[test]
    fn a_slot_row_under_a_group_header_renders_as_a_slot() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let member = |id: u32, slot: u32| {
            ProcessInfo::builder(id, "web", ProcStatus::Online)
                .instance(Some(slot))
                .pid(Some(4_000 + id))
                .fold(Some("edge".to_string()))
                .smit(Some("web".to_string()))
                .uptime_ms(30_000)
                .build()
        };
        let app = fixtures::app_with(vec![member(1, 0), member(2, 1)], fixtures::plain());

        let line = key_line(&app, &RowKey::Sheep(2), ALL, 200);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        let name = name_width(200, ALL);
        let expected = [
            fit("2", Column::Id.width()),
            // NAME: the slot alone, indented under the name the header
            // above already printed.
            fit(" \u{21b3} :1", name),
            fit("online", Column::Status.width()),
            fit("4002", Column::Pid.width()),
            fit("0", Column::Restarts.width()),
            fit("-", Column::Exit.width()),
            fit("-", Column::Cfg.width()),
            fit("-", Column::Cpu.width()),
            fit("-", Column::Mem.width()),
            fit("30s", Column::Uptime.width()),
            // FOLD and SMIT blank, not `-`: the group row carries both.
            fit("", Column::Fold.width()),
            fit("", Column::Smit.width()),
        ]
        .join("  ");

        assert_eq!(rendered, expected, "got {rendered:?}");
    }

    /// The guard on the test above: an app with one instance never gets a
    /// header, so `is_grouped` keeps it drawing as it always did.
    #[test]
    fn an_ungrouped_sheep_still_shows_its_own_name_and_fold() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let app = fixtures::app_with(
            vec![
                ProcessInfo::builder(7, "solo", ProcStatus::Online)
                    .instance(Some(0))
                    .fold(Some("edge".to_string()))
                    .build(),
            ],
            fixtures::plain(),
        );

        let line = key_line(&app, &RowKey::Sheep(7), ALL, 200);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(rendered.contains("solo"), "got {rendered:?}");
        assert!(rendered.contains("edge"), "got {rendered:?}");
        assert!(!rendered.contains('\u{21b3}'), "got {rendered:?}");
    }

    /// The process is alive, so nothing but `handshook` can catch this.
    #[test]
    fn a_silent_dog_reads_silent_not_online() {
        use shep_core::protocol::{DogSource, ProcessInfo};
        use shep_core::status::ProcStatus;

        let dog = ProcessInfo::builder(9, "log-rotate", ProcStatus::Online)
            .pid(Some(4_242))
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            }))
            .handshook(Some(false))
            .build();
        let app = fixtures::app_with(vec![dog], fixtures::plain());
        let row = app.row(9).unwrap();

        let line = row_line(&app, row, ALL, 200, false);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("silent"),
            "expected silent, got {rendered:?}"
        );
        assert!(
            !rendered.contains("online"),
            "must not say online: {rendered:?}"
        );
    }

    /// The guard on [`a_silent_dog_reads_silent_not_online`] above.
    #[test]
    fn a_dog_that_has_handshook_still_reads_online() {
        use shep_core::protocol::{DogSource, ProcessInfo};
        use shep_core::status::ProcStatus;

        let dog = ProcessInfo::builder(9, "log-rotate", ProcStatus::Online)
            .pid(Some(4_242))
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            }))
            .handshook(Some(true))
            .build();
        let app = fixtures::app_with(vec![dog], fixtures::plain());
        let row = app.row(9).unwrap();

        let line = row_line(&app, row, ALL, 200, false);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("online"), "got {rendered:?}");
        assert!(!rendered.contains("silent"), "got {rendered:?}");
    }

    /// A sheep's `handshook` is always `None`; it must never get caught by
    /// the same silent rule a dog does.
    #[test]
    fn a_sheep_still_reads_online_and_has_no_handshake() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let sheep = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .pid(Some(4_000))
            .build();
        assert_eq!(sheep.handshook, None, "a sheep is never sent one");
        let app = fixtures::app_with(vec![sheep], fixtures::plain());
        let row = app.row(1).unwrap();

        let line = row_line(&app, row, ALL, 200, false);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("online"), "got {rendered:?}");
        assert!(!rendered.contains("silent"), "got {rendered:?}");
    }

    /// The daemon never sends a sheep `handshook: Some(false)`; a sheep has
    /// no version relationship with the shepherd to fail. Exercises the
    /// `dog.is_none()` guard in `Row::reported` with an input no other test
    /// drives.
    #[test]
    fn a_sheep_never_reads_as_silent() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let mut impossible = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .pid(Some(4_000))
            .build();
        impossible.handshook = Some(false);
        let app = fixtures::app_with(vec![impossible], fixtures::plain());
        let row = app.row(1).unwrap();

        let line = row_line(&app, row, ALL, 200, false);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("online"),
            "the sheep table has no dogs in it, and no silence rule either: {rendered:?}"
        );
        assert!(!rendered.contains("silent"), "got {rendered:?}");
    }
}
