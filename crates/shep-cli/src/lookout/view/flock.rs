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

use super::super::app::{App, GroupTotals, Row, RowKey};
use crate::output::width::char_columns;
use crate::output::{cfg_cell, exit_cell, human_bytes, human_duration};

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
    /// Whether a config load has parked a change for this sheep's next
    /// spawn, or an operator has overridden a field its Flockfile no longer
    /// declares -- task 12. Rendered by [`crate::output::cfg_cell`], the
    /// same function `output::rows::FlockRows`'s own CFG column calls.
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
    /// `SetSmit` request -- task 7's own column, last in the header order to
    /// match `output::rows::FlockRows`'s. shep paints what a dog wrote and
    /// never parses it.
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
            Self::Cfg => "CFG",
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
            // 15: `waiting-restart`, the longest word `Reported::word`
            // returns. That vocabulary is the six lifecycle statuses plus
            // `silent`, which is 6 columns and so does not move this. A
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
            // 4: `!12`/`*12` -- a `cfg_cell`'s own longest realistic value.
            // `AppConfig` has well under a hundred fields, so two digits
            // covers it with room to spare, and `-` is one character.
            Self::Cfg => 4,
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
    Column::Cfg,
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
    Column::Cfg,
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
    Column::Cfg,
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
/// column, so it is the first one to go. That is the same reasoning
/// `output::rows::FlockRows::PRIORITIES` gives for its own priority 8, the
/// highest number in that table.
///
/// CFG (task 12) drops in the same breath as EXIT rather than getting a
/// tier of its own: it is absent from `NO_EXIT` and every narrower set, so
/// a terminal that has already lost EXIT has lost CFG too. Both answer "why
/// does this row need a second look", and `output::rows::FlockRows::PRIORITIES`
/// gives CFG the exact number it gives EXIT for the same reason.
const TIERS: &[(u16, &[Column])] = &[
    (122, ALL),
    (107, NO_SMIT),
    (95, NO_FOLD),
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

/// One line for a row the table draws: a real sheep, or the header above an
/// app's grouped instances.
///
/// Every production caller sources `key` from [`App::visible_rows`], whose
/// `Sheep` ids always name a row still in `app`'s own flock, so the blank
/// fallback below is never drawn in practice. It exists anyway rather than
/// as an `expect`, on the same "no honest value" rule this table already
/// applies to a missing pid or a missing cpu reading: a caller that manages
/// to hand this a stale id gets a blank row instead of a dead dashboard.
///
/// A `Sheep` row under a group header is drawn as a slot rather than as a
/// standalone sheep ([`App::is_grouped`] is the one test for which).
/// Without that, a header reading `web ×3` was followed by three rows each
/// reading `web` again with the app's FOLD and SMIT repeated down all three,
/// which is the "several rows sharing one name with nothing tying them
/// together" this whole feature exists to end, reproduced one level down.
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
/// `output::rows::FlockRows`'s own group row (task 9) so the two surfaces
/// never disagree about what an app's instances add up to.
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
            // to be silent about -- see `App::group_uniform_status`'s own
            // doc for the argument.
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
/// ID, PID, EXIT and CFG are blank -- not `-`: there is no single id, pid,
/// exit or config-drift state for a group row to have "no honest value"
/// about, the way a real sheep's absent pid does -- a load can park a
/// different set of fields on each slot, so CFG joins the per-instance
/// facts rather than the per-app ones. FOLD and SMIT read the first
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
/// gutter column ([`mark`]), not by a REVERSED modifier on the row's own
/// text — this function has no notion of "selected" to key one off at all.
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
/// `-` rather than an empty cell for every unknown, exactly as
/// `output::rows::FlockRows::rows` does and for the same stated reason: an
/// empty cell in a padded table is indistinguishable from a rendering bug,
/// and `0.0%` would claim a measurement the shepherd never made.
///
/// `grouped` changes three cells, matching `output::rows::slot_row` cell for
/// cell so an operator meets one shape for an app whether they typed
/// `shep flock` or opened the lookout. NAME becomes `↳ :2`, teaching the
/// `web:2` selector by sitting under the name the header already printed;
/// FOLD and SMIT go blank rather than `-`, because the group row above
/// carries both and the daemon keys a smit by name, so repeating either down
/// every slot is noise about an app-level fact.
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
        // in `shep flock`'s own table -- see `Row::reported`'s own doc.
        Column::Status => row.reported().word(),
        Column::Pid => info
            .pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        Column::Restarts => info.restarts.to_string(),
        // `crate::output::exit_cell`, not a second implementation of the
        // code/signal split -- see `Column::Exit`'s own doc.
        Column::Exit => exit_cell(info.pid, info.last_exit),
        // `crate::output::cfg_cell`, not a second implementation of the
        // pending-over-overridden precedence -- see `Column::Cfg`'s own doc.
        Column::Cfg => cfg_cell(info.pending.as_deref(), info.overridden.as_deref()),
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
        assert_eq!(columns_for(300).len(), 12);
        assert_eq!(columns_for(122).len(), 12);
        assert!(!columns_for(121).contains(&Column::Smit));
        assert!(columns_for(121).contains(&Column::Fold));
        assert_eq!(columns_for(107).len(), 11);
        assert!(!columns_for(106).contains(&Column::Fold));
        assert!(columns_for(106).contains(&Column::Exit));
        assert!(columns_for(106).contains(&Column::Cfg));
        assert_eq!(columns_for(95).len(), 10);
        // CFG (task 12) drops in the same breath as EXIT: both are absent
        // from `NO_EXIT` and every narrower tier.
        assert!(!columns_for(88).contains(&Column::Exit));
        assert!(!columns_for(88).contains(&Column::Cfg));
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
    /// `cfg(unix)` because this case's fixture carries a SIGNALLED exit, and
    /// `output::rows::signal_label` resolves a signal number against the
    /// running platform's own table on purpose — its doc argues that a
    /// `ProcessInfo` is always rendered by a binary on the same OS as the
    /// daemon that produced it, and `shep_core::signals::OperatorSignal`
    /// deliberately refuses to map numbers to names at all ("a number means
    /// different signals on different platforms, and shep will not guess").
    ///
    /// So Windows renders `15` where unix renders `SIGTERM`, and that is the
    /// designed behaviour rather than a gap: a Windows `ExitOutcome` never
    /// carries a signal in the first place (`tokio_runner`'s `wait` sets it
    /// `None` unconditionally), so this arm is only ever reached by a
    /// synthetic fixture like this one. The pinned artifacts under
    /// `docs/lookout/` are unix renderings for the same reason.
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

    /// fails if a group row's cells stop showing the app's own rollup:
    /// `web ×3` in NAME, memory SUMMED across instances, ID/PID/EXIT blank
    /// (a group has no single one of any of the three to report), and
    /// UPTIME the MINIMUM rather than any one instance's own reading.
    /// Asserted on the rendered [`Line`], not on `App::group_totals`
    /// directly -- a change in either the arithmetic or the rendering that
    /// reads it has to redden this.
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

        // The exact row, column by column, rather than a substring search:
        // a substring check on a run of blank cells can pass by accident on
        // neighbouring padding. `name_width` is the same helper `key_line`
        // itself calls for the NAME column's own width, not a re-derivation
        // of the arithmetic under test.
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

    /// fails if a slot row under a group header renders as a standalone
    /// sheep.
    ///
    /// `key_line`'s `Sheep` arm drew every row the same way, so a header
    /// reading `web x3` was followed by three rows each repeating `web` with
    /// the app's FOLD and SMIT down all three, and the slot number appeared
    /// nowhere in the dashboard at all. `shep flock` had shown `↳ :1` since
    /// task 9, so one app read as two different shapes in two views.
    ///
    /// Asserted on the rendered line rather than on `cell`, because the
    /// defect was in which caller `key_line` picked and a helper-level test
    /// passes straight over that.
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

    /// fails if a single-instance app loses its name to the slot rendering.
    ///
    /// The guard on the test above: `is_grouped` is what keeps an ungrouped
    /// sheep drawing exactly as it did before group rows existed, and an app
    /// with one instance never gets a header to sit under.
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

    /// fails if a dog that has never handshook reads `online` in this pane
    /// -- the same defect task 4 fixed in `shep flock`'s own table, this
    /// time in the dashboard that never got routed through the fix. The
    /// process IS alive, so nothing but `handshook` can catch this.
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

    /// fails if this pane starts calling a dog silent once it has actually
    /// handshook -- passing for the wrong reason is exactly what a test that
    /// never drives the new lookup would do, so this is the guard on
    /// [`a_silent_dog_reads_silent_not_online`] above.
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

    /// fails if a sheep -- which has no handshake at all, `handshook` is
    /// always `None` -- ever gets caught by the same rule. A sheep's own
    /// STATUS cell must render exactly as it did before this task.
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

    /// fails if the `dog.is_none()` guard in `Row::reported` is ever
    /// removed. The daemon never sends a sheep `handshook: Some(false)` --
    /// a sheep has no handshake and no version relationship with the
    /// shepherd at all, it is a supervised process, not a peer -- so this
    /// is an input the guard exists for and no other test drives. Same
    /// precedent as `output::rows`' own `a_sheep_never_reads_as_silent`.
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
