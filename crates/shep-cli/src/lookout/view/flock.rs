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
use crate::output::{exit_cell, human_bytes, human_duration};

/// The narrowest terminal the TABLE will draw into.
///
/// `ID` + `NAME` (floor 8) + `STATUS` (15, the width of `waiting-restart`)
/// plus two separators. This is the table's own floor, not the terminal's —
/// the terminal also needs room for [`GUTTER`], the selection marker's
/// column; see [`super::MIN_TERM_WIDTH`] and [`super::draw`].
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
/// `width - GUTTER` starting at `x + GUTTER`, which is why every threshold in
/// [`TIERS`] and every arithmetic in [`name_width`] is untouched by the
/// marker's arrival — see the phase plan's design decision 6.
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
    /// Its lifecycle status — the one coloured cell.
    Status,
    /// Its OS pid while running.
    Pid,
    /// Restarts since registration.
    Restarts,
    /// Its last exit, once it is not running -- task 49, the CLI parity gap
    /// this variant closes. Rendered by [`crate::output::exit_cell`], the
    /// same function `output::rows::FlockRows`'s own EXIT column calls, so
    /// the two surfaces cannot drift on what a code or a signal name looks
    /// like.
    Exit,
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
    /// — one vocabulary across both surfaces. Enforced, not just asserted:
    /// `the_full_column_set_matches_flock_rows_headers_exactly` (below)
    /// compares the two lists directly, which is what makes this claim
    /// something a future column can't quietly break (task 49 found it
    /// already had).
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Id => "ID",
            Self::Name => "NAME",
            Self::Status => "STATUS",
            Self::Pid => "PID",
            Self::Restarts => "RESTARTS",
            Self::Exit => "EXIT",
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
            // 9: `SIGVTALRM`/`SIGSTKFLT`, the longest names
            // `nix::sys::signal::Signal::as_str` returns. An exit code is at
            // most a few digits and `-` is one character, both comfortably
            // inside it -- like STATUS above, sized so its longest real
            // value is never truncated.
            Self::Exit => 9,
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
/// The drop order is least-diagnostic first and it is a decision, not an
/// accident of ordering: FOLD is grouping metadata rather than health;
/// RESTARTS and PID answer follow-up questions rather than "is it up"; CPU
/// and MEM are the last two numbers to go because they are the ones that
/// explain WHY a RUNNING sheep is behaving badly. `ID NAME STATUS` is the
/// floor because those three are the pane.
///
/// EXIT (task 49) sits directly below FOLD rather than beside CPU/MEM, and
/// that placement is deliberate even though EXIT is arguably the most
/// diagnostic column of all -- it is the only one that says anything at all
/// once a sheep is dead. But it says nothing else: for every sheep that is
/// still running, which is what this pane spends most of its time showing,
/// EXIT renders `-` in every row, the same silent cell FOLD's own
/// "grouping metadata" reasoning already earns a place near the front of
/// the drop order for. CPU and MEM keep their spot because they answer a
/// question EXIT cannot even ask while a sheep is up: whether a RUNNING
/// sheep is in trouble. This matches where EXIT landed in
/// `output::rows::FlockRows::PRIORITIES` -- the CLI table reasons through
/// the exact same tension in its own comment and reaches the same answer --
/// so an operator who has learned one table's drop order is not surprised
/// by the other's.
const TIERS: &[(u16, &[Column])] = &[
    (101, ALL),
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

/// `text` in exactly `width` characters: padded on the right, or truncated
/// with a trailing `…`.
///
/// Counted in `char`s, never bytes — `{:<w$}` pads by character count, so a
/// byte measurement over-pads every multi-byte name. `output::table` records
/// having made the same choice for the same reason.
///
/// A truncated name that looked whole would be a name an operator types into
/// `shep stop`, so the ellipsis is not cosmetic.
///
/// **Known limitation: `char`s, not display columns.** A double-width
/// character (CJK, many emoji) counts as one `char` here but occupies two
/// columns in the terminal, so a name or a log line built from them can run
/// past `width` and lose its `…` truncation marker. Confirmed cosmetic, not
/// a security issue: ratatui's own `Buffer::set_line` clips at the render
/// area rather than bleeding into a neighbouring pane, and no ESC or CR byte
/// ever reaches a buffer cell, so a hostile log line has no escape-injection
/// path through this function. Fixing it means measuring display width
/// (`unicode-width` or equivalent) instead of `char` count; not done here —
/// see `docs/specs/deferred.md`'s "Known debt" section.
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
/// No row style beyond that: the selected row is shown by the marker in the
/// gutter column ([`mark`]), not by a REVERSED modifier on the row's own
/// text — this function has no notion of "selected" to key one off at all.
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
        // `crate::output::exit_cell`, not a second implementation of the
        // code/signal split -- see `Column::Exit`'s own doc.
        Column::Exit => exit_cell(info.pid, info.last_exit),
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

    /// fails if the drop order changes without someone re-arguing it. FOLD is
    /// grouping metadata and goes first; EXIT (task 49) goes second, right
    /// behind it -- diagnostic only for a dead sheep, and silent (`-`) for
    /// every row while the flock is healthy, which is the common case this
    /// pane spends most of its time showing; RESTARTS and PID answer
    /// follow-up questions rather than "is it up"; CPU and MEM are the last
    /// two numbers to go because they are the ones that explain WHY a
    /// RUNNING sheep is behaving badly; ID/NAME/STATUS is the floor because
    /// those three are the pane.
    #[test]
    fn columns_drop_in_a_fixed_order_as_the_terminal_narrows() {
        assert_eq!(columns_for(300).len(), 10);
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
        // Every tier keeps the three that ARE the pane.
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

    /// fails if lookout's own column list drifts from
    /// `output::rows::FlockRows`'s -- `Column::header`'s own doc claims "one
    /// vocabulary across both surfaces"; this is what makes that claim
    /// enforceable rather than aspirational. Task 49 is the defect that let
    /// it go stale once already: `FlockRows` grew an EXIT header and this
    /// enum did not, and nothing here would have caught it.
    #[test]
    fn the_full_column_set_matches_flock_rows_headers_exactly() {
        use crate::output::Render;

        let headers: Vec<&str> = ALL.iter().map(|column| column.header()).collect();
        assert_eq!(headers, crate::output::FlockRows::headers());
    }

    /// fails if a dead sheep's EXIT cell stops matching what `shep flock`
    /// itself prints, or a running sheep's cell shows anything but `-`. This
    /// pins the WIRING -- that `cell` reaches `crate::output::exit_cell` at
    /// all -- not the rule itself: `output::rows`'s own
    /// `the_exit_column_shows_the_last_exit_only_for_a_sheep_that_is_not_running`
    /// already pins the code/signal split and the "no honest value" `-`.
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
            cell(&app, row, Column::Exit)
        };

        assert_eq!(cell_for(1), "1");
        assert_eq!(cell_for(2), "SIGKILL");
        assert_eq!(
            cell_for(3),
            "-",
            "a running sheep has nothing for EXIT to say"
        );
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

    /// fails if the selection marker stops being a plain character. Colour
    /// and a `REVERSED` modifier are both rejected here: every signal on this
    /// screen has to survive `NO_COLOR` and a 16-colour terminal, and a
    /// decoration-only cursor does not. `>` rather than `▸` because `▸` is
    /// East-Asian *Ambiguous* width — a terminal rendering it double-wide
    /// shifts every column of that one row by a cell, which is worse than
    /// plain.
    #[test]
    fn the_marker_is_one_ascii_column_wide_in_both_states() {
        // The two literals ARE the test. `chars().count() == 1` and
        // `is_ascii()` were in the first draft and cannot fail once these two
        // have passed — `">"` is one ASCII char by inspection — so they were
        // three assertions dressed as five.
        assert_eq!(
            mark(true),
            ">",
            "not `▸`: East-Asian Ambiguous width would shift the row"
        );
        assert_eq!(mark(false), " ");
    }

    /// fails if the viewport stops keeping the selection on screen, or stops
    /// clamping at either end. A pane whose cursor has walked off the bottom
    /// draws a page the operator is not pointing at, and a detail pane
    /// describing a sheep no row on screen shows is worse than either.
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
}
