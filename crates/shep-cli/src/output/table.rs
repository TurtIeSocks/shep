//! The padded table renderer and human-readable duration formatting, the
//! two pieces of `--format table` independent of any payload type.

use super::Render;
use super::width::{sanitize_cell_without_ansi, visible_width};

/// Renders any payload as the padded table, returned rather than printed so
/// a test can read it. [`emit`](super::emit) calls this for `Format::Table`.
///
/// Column widths come from the widest cell in each column, header included,
/// measured in display columns via [`visible_width`] so a CJK or emoji cell
/// pads correctly. Cells are separated by two spaces with no box-drawing
/// characters. An empty payload still prints the header row. Every cell goes
/// through [`sanitize_cell_without_ansi`] first.
///
/// # Panics
/// If any row `T::rows()` returns has a different number of cells than
/// `T::headers()`.
#[allow(dead_code)]
#[track_caller]
pub fn render_table<T: Render>(data: &T) -> String {
    let headers = T::headers();
    // Sanitised once, ahead of the width pass, so measuring and printing see
    // the same bytes. This surface never colours, so a well-formed CSI
    // sequence drops here where the boxed renderer keeps its own.
    let rows: Vec<Vec<String>> = data
        .rows()
        .iter()
        .map(|row| {
            row.iter()
                .map(String::as_str)
                .map(sanitize_cell_without_ansi)
                .collect()
        })
        .collect();

    for row in &rows {
        assert_eq!(
            row.len(),
            headers.len(),
            "{}::rows() returned a row with {} cells, but headers() has {}",
            std::any::type_name::<T>(),
            row.len(),
            headers.len(),
        );
    }

    let mut widths: Vec<usize> = headers.iter().copied().map(visible_width).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(visible_width(cell));
        }
    }

    let mut out = String::new();
    write_row(&mut out, headers.iter().copied(), &widths);
    for row in &rows {
        write_row(&mut out, row.iter().map(String::as_str), &widths);
    }
    out
}

/// Appends one row: every cell but the last padded to its column's width
/// and followed by two spaces; the last cell is unpadded so no line
/// carries trailing whitespace.
///
/// Padded by hand via [`visible_width`] rather than `{cell:<width$}`,
/// which counts chars: a CJK name would get half the spaces it needs and
/// every column after it would slide left.
fn write_row<'a>(out: &mut String, cells: impl Iterator<Item = &'a str>, widths: &[usize]) {
    let cells: Vec<&str> = cells.collect();
    let last = cells.len().saturating_sub(1);
    for (i, cell) in cells.into_iter().enumerate() {
        out.push_str(cell);
        if i != last {
            let pad = widths[i].saturating_sub(visible_width(cell));
            out.extend(core::iter::repeat_n(' ', pad));
            out.push_str("  ");
        }
    }
    out.push('\n');
}

/// `uptime_ms` as the two largest non-zero units (`1h 2m`, `3m 4s`, `5s`,
/// `0s`). The table surface is for a human; the JSON surface keeps the raw
/// `uptime_ms` instead.
#[allow(dead_code)]
#[must_use]
pub fn human_duration(ms: u64) -> String {
    const SECOND_MS: u64 = 1_000;
    const MINUTE_MS: u64 = 60 * SECOND_MS;
    const HOUR_MS: u64 = 60 * MINUTE_MS;
    const DAY_MS: u64 = 24 * HOUR_MS;

    let units: [(u64, &str); 4] = [
        (ms / DAY_MS, "d"),
        ((ms % DAY_MS) / HOUR_MS, "h"),
        ((ms % HOUR_MS) / MINUTE_MS, "m"),
        ((ms % MINUTE_MS) / SECOND_MS, "s"),
    ];
    let mut nonzero = units.iter().filter(|(value, _)| *value > 0);

    match (nonzero.next(), nonzero.next()) {
        (Some(&(a, au)), Some(&(b, bu))) => format!("{a}{au} {b}{bu}"),
        (Some(&(a, au)), None) => format!("{a}{au}"),
        (None, _) => "0s".to_string(),
    }
}

/// Renders `at_ms` (unix millis) as a local timestamp for a table cell:
/// `shep barks`' `WHEN` column.
///
/// Local, not UTC: read during an incident by an operator who thinks in
/// wall-clock time. `%Y-%m-%d %H:%M:%S`, not RFC3339: meant to be read at
/// a glance, not parsed back.
///
/// A millis value too large to fit `i64` renders as the raw number rather
/// than failing the whole row.
#[must_use]
pub fn local_timestamp(at_ms: u64) -> String {
    let Ok(millis) = i64::try_from(at_ms) else {
        return at_ms.to_string();
    };
    let Some(utc) = chrono::DateTime::from_timestamp_millis(millis) else {
        return at_ms.to_string();
    };
    utc.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Formats a byte count for a table cell: the largest binary unit that
/// leaves at least one significant digit, one decimal place under 10.
///
/// Not `MemSize`'s `Display`, which renders the largest unit dividing the
/// value exactly and so prints a live RSS of 50 462 720 bytes as
/// "50462720". A resident-set reading is never a round number of MiB.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 6] = [
        (1 << 60, "E"),
        (1 << 50, "P"),
        (1 << 40, "T"),
        (1 << 30, "G"),
        (1 << 20, "M"),
        (1 << 10, "K"),
    ];
    for (unit, suffix) in UNITS {
        if bytes >= unit {
            #[allow(clippy::cast_precision_loss)] // display only, a table cell
            return format!("{:.1}{suffix}", bytes as f64 / unit as f64);
        }
    }
    format!("{bytes}B")
}

/// Columns that identify a sheep, and so are never dropped: which three
/// survive is the caller's choice (leave them at priority 0). This is only
/// the floor below which [`render_boxed`] refuses to go.
const FLOOR_COLUMNS: usize = 3;

/// [`render_boxed`]'s rendered string, paired with exactly which headers it
/// hid.
///
/// `table_of`'s two-pass STATUS-word retry needs to know whether the first
/// pass hid anything, without re-deriving this function's own fit
/// arithmetic a second time. [`render_boxed`] is a thin wrapper over
/// [`render_boxed_ex`] so its callers see no difference.
pub(crate) struct BoxedTable {
    pub(crate) rendered: String,
    /// Headers hidden this render, sorted the same order the footer names
    /// them in. Empty when everything fit.
    pub(crate) dropped: Vec<String>,
}

/// Renders `rows` as a box-drawn table that fits `term_width`.
///
/// Columns are dropped by descending priority until the table fits, never
/// below [`FLOOR_COLUMNS`]: a table that cannot say which sheep a row is
/// about has stopped being a table. What was dropped is named in a footer.
///
/// Every width is computed with [`crate::output::width::visible_width`], so
/// a styled cell pads by what it shows rather than by what it stores.
///
/// `output::mod`'s `table_of` reaches for [`render_boxed_ex`] instead, for
/// the dropped-column list; only this module's own tests call this form.
#[allow(dead_code)]
pub(crate) fn render_boxed(
    headers: &[&str],
    rows: &[Vec<String>],
    priorities: &[u8],
    term_width: usize,
) -> String {
    render_boxed_ex(headers, rows, priorities, term_width).rendered
}

/// [`render_boxed`], returning the dropped-column list alongside the
/// string rather than only a footer naming them in prose. See
/// [`BoxedTable`] for why a caller would want that.
///
/// Called by [`super::table_of`], which every table-rendering command in
/// `commands/` goes through.
pub(crate) fn render_boxed_ex(
    headers: &[&str],
    rows: &[Vec<String>],
    priorities: &[u8],
    term_width: usize,
) -> BoxedTable {
    // Sanitised once, here: a cell born from operator-chosen data reaches
    // this raw, and reusing the result for both the width pass and the
    // print pass keeps them in agreement.
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| crate::output::width::sanitize_cell(cell))
                .collect()
        })
        .collect();
    let rows = &rows;

    let mut keep: Vec<usize> = (0..headers.len()).collect();
    let mut dropped: Vec<&str> = Vec::new();

    loop {
        let widths = column_widths(headers, rows, &keep);
        let total: usize = widths.iter().map(|w| w + 3).sum::<usize>() + 1;
        if total <= term_width || keep.len() <= FLOOR_COLUMNS {
            break;
        }
        // The kept column with the highest priority number goes first.
        // Priority 0 means never drop: reaching one here means nothing
        // droppable is left, so the table stays wider than the terminal.
        let worst = keep
            .iter()
            .enumerate()
            .max_by_key(|&(_, &col)| priorities.get(col).copied().unwrap_or(0))
            .map(|(at, _)| at);
        let Some(at) = worst else { break };
        if priorities.get(keep[at]).copied().unwrap_or(0) == 0 {
            break;
        }
        dropped.push(headers[keep[at]]);
        keep.remove(at);
    }

    let widths = column_widths(headers, rows, &keep);
    let rule = |left: &str, mid: &str, right: &str| {
        let mut line = String::from(left);
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                line.push_str(mid);
            }
            line.push_str(&"─".repeat(w + 2));
        }
        line.push_str(right);
        line.push('\n');
        line
    };

    let mut out = rule("┌", "┬", "┐");
    out.push_str(&boxed_row(
        &keep
            .iter()
            .map(|&c| headers[c].to_string())
            .collect::<Vec<_>>(),
        &widths,
    ));
    out.push_str(&rule("├", "┼", "┤"));
    for row in rows {
        out.push_str(&boxed_row(
            &keep
                .iter()
                .map(|&c| row.get(c).cloned().unwrap_or_default())
                .collect::<Vec<_>>(),
            &widths,
        ));
    }
    out.push_str(&rule("└", "┴", "┘"));

    // Sorted unconditionally, not only inside the `if` below: `BoxedTable`
    // documents `dropped` as sorted, and sorting an empty `Vec` is a no-op
    // anyway.
    dropped.sort_unstable();
    if !dropped.is_empty() {
        out.push_str(&format!(
            "  {} hidden. Widen the window, or use --format json.\n",
            dropped.join(", ")
        ));
    }
    BoxedTable {
        rendered: out,
        dropped: dropped.into_iter().map(str::to_string).collect(),
    }
}

/// The visible width each kept column needs: the widest of its header and
/// every cell in it, measured by [`crate::output::width::visible_width`]
/// rather than by length or byte count, so a styled cell pads by what it
/// shows.
fn column_widths(headers: &[&str], rows: &[Vec<String>], keep: &[usize]) -> Vec<usize> {
    keep.iter()
        .map(|&col| {
            let mut w = visible_width(headers[col]);
            for row in rows {
                if let Some(cell) = row.get(col) {
                    w = w.max(visible_width(cell));
                }
            }
            w
        })
        .collect()
}

/// One `│ a │ b │` row. Padding is computed from
/// [`crate::output::width::visible_width`] rather than `cell.len()`, so an
/// ANSI-styled cell lines its border up with a plain cell beside it instead
/// of pushing every border after it to the right.
fn boxed_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::from("│");
    for (cell, w) in cells.iter().zip(widths) {
        let pad = w.saturating_sub(visible_width(cell));
        line.push(' ');
        line.push_str(cell);
        line.push_str(&" ".repeat(pad));
        line.push_str(" │");
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::FlockRows;
    use crate::output::rows::tests::info_with_uptime_ms;

    #[test]
    fn an_empty_payload_renders_headers_rather_than_a_bare_blank() {
        let out = render_table(&FlockRows(vec![]));
        assert!(
            out.contains("NAME"),
            "an empty flock still tells the user what it would show"
        );
        assert_eq!(out.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    }

    #[test]
    fn uptime_is_a_duration_in_the_table_and_a_number_in_json() {
        let rows = FlockRows(vec![info_with_uptime_ms(3_723_000)]); // 1h 2m 3s
        let table = render_table(&rows);
        assert!(table.contains("1h"), "table uptime is for a human: {table}");

        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json[0]["uptime_ms"], serde_json::json!(3_723_000u64));
        assert!(
            json[0].get("uptime").is_none(),
            "no formatted duplicate on the machine surface"
        );
    }

    #[test]
    fn human_duration_takes_the_two_largest_nonzero_units() {
        assert_eq!(human_duration(3_723_000), "1h 2m");
        assert_eq!(human_duration(184_000), "3m 4s");
        assert_eq!(human_duration(5_000), "5s");
        assert_eq!(human_duration(0), "0s");
    }

    /// The day arm (`units[0]`) is untouched by the test above. Both cases
    /// also pin skipping a zero middle unit.
    #[test]
    fn human_duration_day_arm_skips_a_zero_middle_unit() {
        assert_eq!(human_duration(86_700_000), "1d 5m"); // 1 day + 5 minutes, 0 hours
        assert_eq!(human_duration(3_602_000), "1h 2s"); // 1 hour + 2 seconds, 0 minutes
    }

    /// Round-trips through the host's own zone rather than pinning a fixed
    /// string: `std::env::set_var` is `unsafe` in this
    /// `#![forbid(unsafe_code)]` crate, so `$TZ` cannot be pinned from
    /// inside the test.
    #[test]
    fn local_timestamp_round_trips_through_the_hosts_own_zone() {
        let at_ms: u64 = 1_700_000_000_000; // 2023-11-14T22:13:20Z, an arbitrary real moment
        let rendered = local_timestamp(at_ms);
        assert_eq!(
            rendered.len(),
            19,
            "shape is `YYYY-MM-DD HH:MM:SS`: {rendered}"
        );
        let parsed = chrono::NaiveDateTime::parse_from_str(&rendered, "%Y-%m-%d %H:%M:%S")
            .unwrap_or_else(|e| {
                panic!("local_timestamp produced something unparseable: {rendered}: {e}")
            });
        let resolved_utc = parsed
            .and_local_timezone(chrono::Local)
            .single()
            .unwrap_or_else(|| panic!("{rendered} does not resolve to one local instant"))
            .with_timezone(&chrono::Utc);
        assert_eq!(
            resolved_utc.timestamp_millis(),
            i64::try_from(at_ms).unwrap(),
            "the rendered cell must name the same instant at_ms does, in whatever zone \
             this machine runs"
        );
    }

    #[test]
    fn local_timestamp_falls_back_to_the_raw_number_when_it_will_not_render() {
        assert_eq!(
            local_timestamp(u64::MAX),
            u64::MAX.to_string(),
            "too large to fit i64 at all"
        );
        assert_eq!(
            local_timestamp(u64::try_from(i64::MAX).unwrap()),
            i64::MAX.to_string(),
            "fits i64, but names a calendar date far outside what chrono can represent"
        );
    }

    /// `render_table`'s own defensive check, not `assert_no_drift`'s
    /// (rows.rs): that gate polices `rows()` against `Serialize`, but says
    /// nothing about a `rows()` that is wrong by construction.
    struct MalformedRow;

    impl serde::Serialize for MalformedRow {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_unit()
        }
    }

    impl Render for MalformedRow {
        fn headers() -> &'static [&'static str] {
            &["A", "B"]
        }

        fn rows(&self) -> Vec<Vec<String>> {
            vec![vec!["1".to_string(), "2".to_string(), "3".to_string()]]
        }

        fn json_key_for(header: &str) -> &'static str {
            match header {
                "A" => "a",
                "B" => "b",
                other => panic!("MalformedRow::headers() does not include {other:?}"),
            }
        }

        const JSON_ONLY: &'static [&'static str] = &[];
    }

    #[test]
    #[should_panic(
        expected = "MalformedRow::rows() returned a row with 3 cells, but headers() has 2"
    )]
    fn render_table_panics_on_a_row_whose_arity_does_not_match_headers() {
        render_table(&MalformedRow);
    }

    fn info_with_name(name: &str) -> shep_core::protocol::ProcessInfo {
        shep_core::protocol::ProcessInfo::builder(1, name, shep_core::status::ProcStatus::Online)
            .build()
    }

    /// fails if a forged colour reaches the bare table. `shep-core`'s
    /// `normalize()` rejects only `/`, `\`, `.` and `..` in a name, so an
    /// escape gets this far from an app an operator started.
    #[test]
    fn a_name_carrying_an_escape_leaves_no_escape_in_the_table() {
        let out = render_table(&FlockRows(vec![info_with_name("web\u{1b}[31mworker")]));
        assert!(!out.contains('\u{1b}'), "escape survived: {out:?}");
        assert!(out.contains("webworker"), "the printable text stays: {out}");
    }

    /// `MemSize`'s own Display only names a unit that divides the value
    /// exactly, and a resident set is never an exact number of MiB.
    #[test]
    fn bytes_render_with_a_unit_a_reader_can_scan() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(50_462_720), "48.1M");
        assert_eq!(human_bytes(3 << 30), "3.0G");
        assert_eq!(human_bytes(u64::MAX), "16.0E");
    }

    /// "羊" is one character, three bytes, two display columns. Asserts on
    /// both the header line and the row: only the header line proves the
    /// padding moved, since the row's own NAME cell is that name and would
    /// widen regardless.
    ///
    /// Not asserted: that a table's lines are all one width. `write_row`
    /// leaves the last cell unpadded, so they are not; that property
    /// belongs to `every_line_of_a_boxed_table_has_the_same_visible_width`.
    #[test]
    fn column_widths_count_display_columns_not_characters_or_bytes() {
        let ascii_name = "wwwwww".to_string(); // 6 chars, 6 bytes, 6 columns
        let cjk_name = "羊".repeat(6); // 6 chars, 18 bytes, 12 columns
        assert_eq!(ascii_name.chars().count(), cjk_name.chars().count());

        let lines = |name: &str| -> (usize, usize) {
            let table = render_table(&FlockRows(vec![info_with_name(name)]));
            let mut lines = table.lines();
            let header = visible_width(lines.next().expect("a header line"));
            let row = visible_width(lines.next().expect("a row line"));
            (header, row)
        };
        let (ascii_header, ascii_row) = lines(&ascii_name);
        let (cjk_header, cjk_row) = lines(&cjk_name);

        assert_eq!(
            cjk_header - ascii_header,
            6,
            "six `羊` draw six columns wider than six `w`, so the NAME column \
             is padded six wider — a character count makes this 0 and a byte \
             count makes it 12"
        );
        assert_eq!(
            cjk_row - ascii_row,
            6,
            "and the row moves with its own header, or the two disagree about \
             where the second column starts"
        );
    }

    /// The lines that make up the table itself: top rule, header,
    /// separator, each row, bottom rule, rather than every line
    /// `render_boxed` returns. The footer is prose, not a box, so a
    /// same-width check over the raw output would fail once a column drops.
    fn table_lines(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| l.starts_with(['┌', '├', '│', '└']))
            .collect()
    }

    /// One cell's raw text before the `styled` wrapper below decides
    /// whether to also wrap it in a colour span: plain words most of the
    /// time, and occasionally a control character (`\n`/`\r`/`\t`, the
    /// class `sanitize_cell` escapes) or an unterminated CSI introducer
    /// with no final byte, the class it drops.
    fn dirty_cell_text() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;

        prop_oneof![
            3 => "[a-z(). -]{0,12}".prop_map(String::from),
            1 => ("[a-z]{0,4}", prop_oneof![Just('\n'), Just('\r'), Just('\t')], "[a-z]{0,4}")
                .prop_map(|(a, c, b)| format!("{a}{c}{b}")),
            // digits and `;` only after the introducer, never a letter: a
            // letter is itself a valid final byte and would close the
            // sequence this case means to leave open.
            1 => "[a-z]{0,4}".prop_map(|a| format!("{a}\u{1b}[3;1")),
        ]
    }

    /// Any rows, any width, any mix of styled and plain cells: every line
    /// of the table is the same visible width, either inside the terminal
    /// or reduced to the floor of three columns.
    #[test]
    fn every_line_of_a_boxed_table_has_the_same_visible_width() {
        use proptest::prelude::*;

        proptest!(|(
            cells in proptest::collection::vec(
                proptest::collection::vec(
                    (dirty_cell_text(), any::<bool>()).prop_map(|(s, styled)| {
                        if styled {
                            format!("\u{1b}[32m{s}\u{1b}[0m")
                        } else {
                            s
                        }
                    }),
                    3..6),
                0..5),
            term in 20usize..200,
        )| {
            let headers = ["ID", "NAME", "STATUS", "PID", "MEM"];
            let n = cells.first().map_or(3, Vec::len);
            let headers = &headers[..n];
            let priorities: Vec<u8> = (0..n).map(|i| u8::try_from(i).unwrap_or(u8::MAX)).collect();
            let out = render_boxed(headers, &cells, &priorities, term);

            let lines = table_lines(&out);
            let widths: Vec<usize> = lines
                .iter()
                .map(|l| visible_width(l))
                .collect();
            if let Some(&first) = widths.first() {
                prop_assert!(
                    widths.iter().all(|&w| w == first),
                    "ragged table at term={term}: widths {widths:?}\n{out}"
                );

                let columns_kept = lines
                    .first()
                    .map_or(0, |top_rule| top_rule.matches('┬').count() + 1);
                prop_assert!(
                    first <= term || columns_kept == FLOOR_COLUMNS,
                    "table {first} columns wide exceeds term={term} with {columns_kept} kept \
                     (floor is {FLOOR_COLUMNS}):\n{out}"
                );
            }
        });
    }

    /// Columns drop by priority until the table fits, and the floor is the
    /// three that identify a sheep.
    #[test]
    fn columns_drop_by_priority_and_never_below_three() {
        let headers = ["ID", "NAME", "STATUS", "PID", "FOLD"];
        let rows = vec![vec![
            "0".into(),
            "zeus-auth".into(),
            "(o.o) online".into(),
            "24963".into(),
            "backend".into(),
        ]];
        let priorities = [0, 0, 0, 2, 6];

        let wide = render_boxed(&headers, &rows, &priorities, 200);
        assert!(wide.contains("FOLD"), "everything fits at 200:\n{wide}");

        let narrow = render_boxed(&headers, &rows, &priorities, 46);
        // The footer legitimately names a dropped column by header text, so
        // a whole-render `contains("FOLD")` check would fail on the
        // footer's own announcement.
        let narrow_table = table_lines(&narrow).join("\n");
        assert!(
            !narrow_table.contains("FOLD"),
            "FOLD drops first:\n{narrow}"
        );
        assert!(
            narrow.contains("NAME"),
            "identity columns survive:\n{narrow}"
        );
        assert!(
            narrow.contains("hidden"),
            "and the footer says so:\n{narrow}"
        );

        let tiny = render_boxed(&headers, &rows, &priorities, 10);
        for keep in ["ID", "NAME", "STATUS"] {
            assert!(tiny.contains(keep), "{keep} is a floor column:\n{tiny}");
        }
    }

    /// A dropped column is named, so nothing vanishes silently.
    ///
    /// `term_width = 20`, not 30: at 30, FOLD's drop alone fits, so CPU
    /// never reaches the footer. At 20, both drops are needed before the
    /// floor of three stops the loop, so both names reach the footer.
    #[test]
    fn the_footer_names_every_column_it_hid() {
        let headers = ["ID", "NAME", "STATUS", "CPU", "FOLD"];
        let rows = vec![vec![
            "0".into(),
            "a".into(),
            "(o.o)".into(),
            "0%".into(),
            "b".into(),
        ]];
        let out = render_boxed(&headers, &rows, &[0, 0, 0, 5, 6], 20);
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("CPU"), "{footer}");
        assert!(footer.contains("FOLD"), "{footer}");
        assert!(
            footer.contains("--format json"),
            "and the way to see them: {footer}"
        );
    }

    /// The dropped-column footer is prose a user reads.
    #[test]
    fn the_dropped_column_footer_has_no_em_dashes() {
        let headers = ["ID", "NAME", "STATUS", "CPU", "FOLD"];
        let rows = vec![vec![
            "0".into(),
            "a".into(),
            "(o.o)".into(),
            "0%".into(),
            "b".into(),
        ]];
        let out = render_boxed(&headers, &rows, &[0, 0, 0, 5, 6], 20);
        let footer = out.lines().last().unwrap();
        assert!(!footer.contains('\u{2014}'), "em dash in footer: {footer}");
        assert!(!footer.contains('\u{2013}'), "en dash in footer: {footer}");
    }

    /// `render_boxed_ex`'s dropped list matches the footer it renders:
    /// `table_of`'s two-pass retry trusts this list instead of re-deriving
    /// it. `render_boxed` renders the identical string.
    #[test]
    fn render_boxed_ex_reports_exactly_what_it_dropped() {
        let headers = ["ID", "NAME", "STATUS", "CPU", "FOLD"];
        let rows = vec![vec![
            "0".into(),
            "a".into(),
            "(o.o)".into(),
            "0%".into(),
            "b".into(),
        ]];
        let priorities = [0, 0, 0, 5, 6];

        let fits = render_boxed_ex(&headers, &rows, &priorities, 200);
        assert!(fits.dropped.is_empty(), "everything fits at 200");
        assert_eq!(
            fits.rendered,
            render_boxed(&headers, &rows, &priorities, 200)
        );

        let narrow = render_boxed_ex(&headers, &rows, &priorities, 20);
        assert_eq!(narrow.dropped, vec!["CPU".to_string(), "FOLD".to_string()]);
        assert_eq!(
            narrow.rendered,
            render_boxed(&headers, &rows, &priorities, 20)
        );
    }

    /// `shep-core`'s `normalize()` rejects only `/`, `\`, `.` and `..` in a
    /// name, so an embedded newline reaches this renderer: reachable, not
    /// theoretical.
    #[test]
    fn a_name_with_an_embedded_newline_does_not_split_its_own_row() {
        let headers = ["ID", "NAME", "STATUS"];
        let rows = vec![vec!["0".into(), "web\nworker".into(), "online".into()]];
        let out = render_boxed(&headers, &rows, &[0, 0, 0], 80);

        let box_lines: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with(['┌', '├', '│', '└']))
            .collect();
        // Top rule, header, separator, exactly one data row, bottom rule:
        // five lines. A literal newline surviving into the cell would have
        // split that one data row into two, making it six.
        assert_eq!(box_lines.len(), 5, "{out}");
        assert!(out.contains("web\\nworker"), "escaped, visible: {out}");
        assert!(!out.contains("web\nworker"), "no literal newline: {out:?}");
    }

    // --- Every level, through the real rendering seam ----------------------

    // Every snapshot below goes through `crate::output::table_of` over a
    // `FlockRows` built from real `ProcessInfo` values, never `render_boxed`
    // on hand-written cells: the face, colour and STATUS-word retry all
    // come from the real rendering path.

    use std::ffi::OsStr;

    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use crate::output::table_of;
    use crate::style::{Presentation, StyleLevel};

    /// Four sheep, one per role [`crate::vocabulary::role_of`] maps a
    /// status to: Meadow (Online), Butter (`butter`), Bark (Errored), Ink3
    /// (Stopped). Every row the same status would pin one face only.
    ///
    /// `butter` is a parameter: the narrower snapshots below need different
    /// Butter-role words at the same width, and `WaitingRestart`
    /// (`"waiting-restart"`, 15 characters) is the longest.
    ///
    /// Every column is sized to a width of 80, injected as
    /// `Presentation::new`'s `width` rather than measured. `EXIT`, `SMIT`
    /// and `CFG` all read `-` here yet still cost columns, so tests that
    /// add them widen their own `Presentation` past 80.
    fn mixed_flock(butter: ProcStatus) -> FlockRows {
        FlockRows(vec![
            ProcessInfo::builder(0, "web", ProcStatus::Online)
                .pid(Some(1234))
                .uptime_ms(3_723_000) // 1h 2m
                .build(),
            ProcessInfo::builder(1, "worker", butter).build(),
            ProcessInfo::builder(2, "api", ProcStatus::Errored)
                .restarts(4)
                .build(),
            ProcessInfo::builder(3, "cron", ProcStatus::Stopped).build(),
        ])
    }

    /// Not a behaviour test: a measurement, recorded so the two-width tests
    /// below rest on a number rather than an assumption. `unicode-width`
    /// classifies these two by East Asian Width, which is ambiguous for
    /// some symbols and has moved between Unicode revisions.
    #[test]
    fn how_wide_the_real_smits_actually_are() {
        assert_eq!(visible_width("\u{25b2} main@a1b2c3"), 13);
        assert_eq!(visible_width("\u{23f8} main@f6e5d4"), 13);
    }

    /// A deep (256-colour) terminal: exercises `output::paint::style_for`'s
    /// deep tier rather than the 16-colour fallback, since most terminals
    /// in the wild use it.
    fn deep_terminal() -> Option<&'static OsStr> {
        Some(OsStr::new("xterm-256color"))
    }

    /// A `Full`, deep-colour `Presentation` at `width`, the shape every test
    /// below wants and the only thing that varies between them.
    fn full_at(width: usize) -> Presentation {
        Presentation::new(StyleLevel::Full, None, deep_terminal(), None, width)
    }

    /// [`mixed_flock`], with two of its four rows carrying the real smit
    /// strings a deploy dog paints, not a hand-built `Some("x")`: the
    /// requirement under test is about a real smit at a real terminal
    /// width.
    fn mixed_flock_with_smits() -> FlockRows {
        let mut flock = mixed_flock(ProcStatus::Starting);
        flock.0[0].smit = Some("\u{25b2} main@a1b2c3".to_string());
        flock.0[2].smit = Some("\u{23f8} main@f6e5d4".to_string());
        flock
    }

    /// `full`, comfortably wide enough: face, word and colour all present,
    /// nothing dropped.
    ///
    /// Width 103, not this module's usual 80: `EXIT`, `SMIT` and `CFG` each
    /// cost extra columns their `-` content never fills, leaving no slack
    /// for `Starting`'s word. This proves nothing drops when there is room;
    /// the narrow snapshot below proves the boundary itself.
    #[test]
    fn full_wide_pins_face_word_and_colour_for_a_mixed_flock() {
        let presentation = Presentation::new(StyleLevel::Full, None, deep_terminal(), None, 103);
        let rendered = table_of(&mixed_flock(ProcStatus::Starting), presentation);
        assert!(
            !rendered.contains("hidden"),
            "this fixture must fit without dropping a column: {rendered}"
        );
        assert!(
            rendered.contains("starting"),
            "the word must survive at a width with room to spare: {rendered}"
        );
        insta::assert_snapshot!(rendered);
    }

    /// `full`, narrow enough that the STATUS word drops but no whole column
    /// does: the one width-driven behaviour a hand-written `render_boxed`
    /// call cannot exercise, since only `table_of`'s two-pass retry asks
    /// [`Render::rows_for`] again with the word turned off.
    ///
    /// Width 93: `SMIT` and `CFG` cost the fixture extra columns the same
    /// way they do in `full_wide_pins_face_word_and_colour_for_a_mixed_flock`.
    /// `render_boxed_ex`'s priority order drops SMIT first on the
    /// word-included pass; the retry then asks again with the word off,
    /// every face is a fixed 5 columns, and the table fits with SMIT back
    /// too.
    #[test]
    fn full_narrow_drops_the_status_word_before_a_whole_column() {
        let presentation = Presentation::new(StyleLevel::Full, None, deep_terminal(), None, 93);
        let rendered = table_of(&mixed_flock(ProcStatus::WaitingRestart), presentation);
        assert!(
            !rendered.contains("waiting-restart"),
            "the word should have dropped: {rendered}"
        );
        assert!(
            !rendered.contains("hidden"),
            "and no whole column should have needed to: {rendered}"
        );
        insta::assert_snapshot!(rendered);
    }

    /// The narrowest terminal that still shows every column, including the
    /// smit. A later column that changes this width needs this number
    /// updated, not left stale.
    const FULL_WIDTH: usize = 99;

    /// Guards the narrow-terminal drop below: dropping the smit there is
    /// acceptable only if it still shows up regularly at full width.
    #[test]
    fn a_smit_is_never_dropped_at_full_width() {
        let rendered = table_of(&mixed_flock_with_smits(), full_at(FULL_WIDTH));
        assert!(
            rendered.contains("\u{25b2} main@a1b2c3"),
            "the smit must survive a full-width render: {rendered}"
        );
        assert!(
            !rendered.contains("hidden. Widen the window"),
            "and nothing else may be dropped either, or FULL_WIDTH is wrong: {rendered}"
        );
        insta::assert_snapshot!(rendered);
    }

    /// fails if a smit stops yielding first on a narrow terminal. It is by
    /// far the widest column, so giving it up buys back the most room for
    /// one column lost.
    #[test]
    fn a_smit_is_the_first_column_dropped_when_the_window_narrows() {
        let rendered = table_of(&mixed_flock_with_smits(), full_at(FULL_WIDTH - 1));
        assert!(
            !rendered.contains("main@a1b2c3"),
            "the smit must be gone one column below full width: {rendered}"
        );
        assert!(
            rendered.contains("SMIT hidden.") || rendered.contains("SMIT, "),
            "and the footer must name it, so an operator knows to widen: {rendered}"
        );
        // FOLD outlasts it, which is the placement decision itself.
        assert!(rendered.contains("FOLD"), "{rendered}");
        insta::assert_snapshot!(rendered);
    }

    /// `plain`: boxes and colour survive, the word rides alone. `plain` is
    /// "no sheep", not "no colour".
    #[test]
    fn plain_pins_the_boxed_table_with_words_and_colour_but_no_face() {
        let presentation = Presentation::new(StyleLevel::Plain, None, deep_terminal(), None, 80);
        let rendered = table_of(&mixed_flock(ProcStatus::Starting), presentation);
        assert!(!rendered.contains("(o.o)"), "no face at plain: {rendered}");
        insta::assert_snapshot!(rendered);
    }

    /// `bare`: byte-identical to `render_table`'s own output. No box, no
    /// face, no escape.
    #[test]
    fn bare_pins_the_byte_identical_plain_table() {
        let rendered = table_of(&mixed_flock(ProcStatus::Starting), Presentation::BARE);
        assert!(
            !rendered.contains('\u{1b}'),
            "bare must never emit an escape: {rendered:?}"
        );
        assert!(
            !rendered.contains('┌') && !rendered.contains('│') && !rendered.contains('└'),
            "bare must never draw a box: {rendered}"
        );
        insta::assert_snapshot!(rendered);
    }

    /// `full` under `NO_COLOR`: sheep and boxes survive, colour alone is
    /// vetoed.
    #[test]
    fn full_under_no_color_pins_sheep_and_boxes_without_colour() {
        let presentation = Presentation::new(
            StyleLevel::Full,
            Some(OsStr::new("1")),
            deep_terminal(),
            None,
            80,
        );
        let rendered = table_of(&mixed_flock(ProcStatus::Starting), presentation);
        assert!(
            !rendered.contains('\u{1b}'),
            "NO_COLOR must leave no escape byte: {rendered:?}"
        );
        insta::assert_snapshot!(rendered);
    }
}
