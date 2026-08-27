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
use crate::output::width::char_columns;
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
    /// A dog's marker, painted over `shep whisper` -- task 7's own column,
    /// last in the header order to match `output::rows::FlockRows`'s. shep
    /// paints what a dog wrote and never parses it.
    Smit,
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
            Self::Smit => "SMIT",
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
            // 13: `visible_width("▲ main@a1b2c3")` -- the measured width of
            // the real strings a deploy dog paints, pinned by
            // `output::table`'s own `how_wide_the_real_smits_actually_are`.
            // A smit's own cap is 48 characters (`Smit::MAX_CHARS`), but the
            // pane is fixed-width like every other column here, not
            // variable like NAME, so a longer one truncates via [`fit`]
            // rather than growing the column.
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
///
/// SMIT (task 7) sits above FOLD, at the very top: it is by far the widest
/// column, so it is the first one to go, and the only one whose content is
/// recoverable another way -- asking the deploy dog again. That is the
/// same reasoning `output::rows::FlockRows::PRIORITIES` gives for its own
/// priority 8, the highest number in that table.
const TIERS: &[(u16, &[Column])] = &[
    (116, ALL),
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
/// Counted in terminal columns, never bytes and never `char`s. Bytes
/// over-pad every multi-byte name. `char`s under-pad every double-width one:
/// a CJK name or an emoji counts as one `char` and draws in two columns, so
/// a row built by `char` count runs past its cell and shoves every column
/// after it out of line — which for the last column means running off the
/// end and losing the `…` that says the text was cut. That was this
/// function's recorded limitation until [`crate::output::width::char_columns`]
/// existed to fix it in one place; `output::table` pads by the same rule.
///
/// A truncated name that looked whole would be a name an operator types into
/// `shep stop`, so the ellipsis is not cosmetic.
///
/// **Escapes are not discounted here, deliberately** — this is the one place
/// the two width questions in this crate part company.
/// [`crate::output::width::visible_width`] skips an ANSI sequence because its
/// callers write raw bytes to a terminal that will interpret one. Nothing on
/// this path does: a `Span` hands ratatui text, ratatui draws it, and a log
/// line carrying `\x1b[32m` puts a literal `32m` on screen occupying three
/// columns. Measuring it as zero would under-count exactly the cell it was
/// meant to protect.
///
/// The result is exactly `width` columns in **both** branches. A
/// double-width character that will not fit the last column before the `…`
/// is dropped and the gap is padded, so a cell never comes out short and the
/// two-space separators between cells stay where the header put them.
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
    // One column pays for the `…`; the rest is filled with as much of `text`
    // as fits whole. A double-width character straddling the boundary is
    // dropped rather than split — there is no half of it to draw — and the
    // column it would have used is padded below so the cell still measures
    // `width`.
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
        assert_eq!(columns_for(300).len(), 11);
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
    /// branch, where a nine-byte name asks for nine columns of padding
    /// budget it does not need and comes out short, and at the exactly-fits
    /// boundary, where a byte count truncates a string that already fits.
    #[test]
    fn fit_counts_columns_not_bytes_when_it_pads_and_when_it_truncates() {
        // Pad branch. `日本語` is 9 bytes and 6 columns: measured properly
        // it exactly fills a 6-wide cell, byte-counted it falls into the
        // truncate branch.
        assert_eq!(fit("日本語", 6), "日本語");
        // Exactly fits. 7 columns / 11 bytes — a byte count cuts it to
        // `ünïcöd…`.
        assert_eq!(fit("ünïcödé", 7), "ünïcödé");
    }

    /// fails if `fit` goes back to counting `char`s — the bug
    /// `docs/specs/deferred.md` recorded and this change closes. A `char`
    /// count gives `日本語` three and lets it into a 3-wide cell it draws
    /// six columns in, and gives `日本語アプリ` a five-`char` prefix of
    /// `日本語ア…` that draws nine.
    ///
    /// Every case asserts on CONTENT and on measured columns, for the same
    /// reason the byte test above does: `chars().count()` alone is blind to
    /// the mutation, and here so is `len()`.
    #[test]
    fn a_double_width_name_is_cut_to_the_columns_it_draws_in() {
        // Truncate branch. Budget is 4 columns plus the `…`: two characters
        // fit, the third would draw past the cell.
        assert_eq!(fit("日本語アプリ", 5), "日本…");
        assert_eq!(columns_of(&fit("日本語アプリ", 5)), 5);
        // A `char` count would call this a pad — 3 chars into 3 columns —
        // and emit `日本語`, six columns wide, with no `…` to say it was cut.
        assert_eq!(fit("日本語", 3), "日…");
        // The odd-width case the padding exists for: `日` fits the 2-column
        // budget, `本` does not, and half a character is not drawable — so
        // the leftover column is a space rather than a short cell.
        assert_eq!(fit("日本語", 4), "日… ");
        assert_eq!(columns_of(&fit("日本語", 4)), 4);
        // Nothing but the marker fits, at either width.
        assert_eq!(fit("日本語", 1), "…");
        assert_eq!(fit("日本語", 2), "… ");
    }

    /// fails if a cell can come out narrower or wider than the width it was
    /// asked for. Every caller concatenates cells with two-space separators
    /// and no caller re-measures, so one short cell shifts every column
    /// after it on that row alone — the drift is invisible in a single
    /// cell's own test and obvious in a rendered table.
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

    /// An ANSI escape is text here, not styling — see [`fit`]'s own doc. A
    /// `Span` is not a terminal, so ratatui draws `\x1b[32m` as the literal
    /// `32m` it is; measuring it as zero (which
    /// `crate::output::width::visible_width` would, correctly, on its own
    /// path) would let a hostile log line claim more columns than the cell
    /// it was cut to fit.
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
