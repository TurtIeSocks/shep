//! The padded table renderer and human-readable duration formatting — the
//! two pieces of `--format table` that have nothing to do with any one
//! payload type.

use super::Render;
use super::width::visible_width;

/// Renders any payload as the padded table, returned rather than printed so
/// a test can read it. [`emit`](super::emit) calls this for `Format::Table`.
///
/// Column widths come from the widest cell in each column, header included;
/// cells are padded and separated by two spaces. No box-drawing characters —
/// a table a user can `awk` over beats one that looks nice. An empty payload
/// still prints the header row: a bare blank line would not tell the user
/// whether the command worked.
///
/// Widths are counted in `char`s, not bytes: `{:<w$}` pads by character
/// count, so measuring in bytes would over-pad any column holding a
/// multi-byte name (CJK, emoji) relative to `headers()`'s own char-counted
/// width.
///
/// # Panics
/// If any row `T::rows()` returns has a different number of cells than
/// `T::headers()`. Every real `Render` impl keeps the two in lockstep —
/// rows.rs's own anti-drift tests police that — so this only fires if a
/// future impl breaks that invariant; better a loud, type-named panic here
/// than a silent `index out of bounds` two lines down in [`write_row`].
///
/// Not called outside this module's own tests yet: `emit`'s `Format::Table`
/// arm is its only real caller, and `emit` itself has no caller until Tasks
/// 7-11 land. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
#[allow(dead_code)]
#[track_caller]
pub fn render_table<T: Render>(data: &T) -> String {
    let headers = T::headers();
    let rows = data.rows();

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

/// Appends one row: every cell but the last padded to its column's width and
/// followed by two spaces, the last cell unpadded so no line carries
/// trailing whitespace.
///
/// Padded by hand rather than with `{cell:<width$}`, and the difference is
/// the point: that format spec counts `char`s, so a CJK name gets half the
/// spaces it needs and every column after it slides left. `boxed_row` below
/// pads by [`visible_width`] for the same reason — this is the plain
/// renderer catching up with the boxed one.
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
/// `uptime_ms` instead (no formatted duplicate — see `rows`'s own test).
///
/// Not called outside this module's own tests and `FlockRows::rows` yet, and
/// `FlockRows::rows` itself has no real caller until Tasks 7-11 land.
/// `#[allow(dead_code)]` says so explicitly rather than inventing a call
/// site nothing needs yet.
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

/// Renders `at_ms` (unix millis, UTC by construction — [`Bark::at_ms`]'s own
/// doc) as a local timestamp for a table cell: `shep barks`' `WHEN` column.
///
/// A local rendering, not UTC: `at_ms` is read during an incident, at a
/// terminal, by an operator who thinks in wall-clock time — the same reason
/// `human_duration`/`human_bytes` exist instead of a raw `uptime_ms`/
/// `memory_bytes` echo. The raw millis stay in `--format json`'s `at_ms`
/// field for a consumer that wants to do its own arithmetic; this is table
/// output only.
///
/// `%Y-%m-%d %H:%M:%S` rather than RFC3339: no `T`, no offset suffix — this
/// is a column meant to be read at a glance, not parsed back, and the
/// operator's own local zone is implied by every other clock in the room.
///
/// A millis value too large to fit `i64` (`u64::MAX`, a corrupt or
/// far-future record) renders as the raw number rather than failing the
/// whole row: [`shep_core::barks::read`]'s own doc already tolerates a bad
/// *line*; a good line with one unrenderable field should not be dropped
/// for a reason narrower than that.
///
/// [`Bark::at_ms`]: shep_core::barks::Bark::at_ms
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
/// value EXACTLY and so prints a live RSS of 50 462 720 bytes as
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

/// Columns that identify a sheep, and so are never dropped -- which three
/// survive is entirely the caller's own choice (leave them at priority 0);
/// this is only the floor below which [`render_boxed`] refuses to go.
const FLOOR_COLUMNS: usize = 3;

/// [`render_boxed`]'s rendered string, paired with exactly which headers it
/// hid.
///
/// `table_of`'s two-pass STATUS-word retry (spec §2: the word drops before
/// any whole column does) needs to know whether the first pass hid
/// anything, without either scraping the footer string for the same answer
/// or re-deriving this function's own `sum(w + 3) + 1` fit arithmetic a
/// second time -- both of those are the kind of duplicated knowledge that
/// drifts silently. [`render_boxed`] itself is a thin wrapper over
/// [`render_boxed_ex`], kept so its own callers here (the property test and
/// the unit tests below) see no difference.
pub(crate) struct BoxedTable {
    pub(crate) rendered: String,
    /// Headers hidden this render, sorted -- the same order the footer
    /// names them in. Empty when everything fit.
    pub(crate) dropped: Vec<String>,
}

/// Renders `rows` as a box-drawn table that fits `term_width`.
///
/// Columns are dropped by descending priority until the table fits, never
/// below [`FLOOR_COLUMNS`] -- a table that cannot say which sheep a row is
/// about has stopped being a table. What was dropped is named in a footer,
/// because a column that vanishes silently is worse than one that is
/// missing loudly.
///
/// Every width is computed with [`crate::output::width::visible_width`], so
/// a styled cell pads by what it shows rather than by what it stores. This
/// module's own property test is the real specification: any rows, any
/// terminal width, any mix of styled and plain cells, every line of the
/// table the same width, and that width inside the terminal unless the
/// floor itself does not fit.
///
/// `output::mod`'s `table_of` -- every real caller in this crate -- reaches
/// for [`render_boxed_ex`] instead, for the dropped-column list this
/// function's own footer only renders as prose. Only this module's own
/// tests call this form directly, so a plain (non-test) build has no
/// caller at all. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
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
/// `commands/` goes through — `emit`, `emit_flock` and `emit_described` all
/// reach it, never `render_boxed`/`render_boxed_ex` directly.
pub(crate) fn render_boxed_ex(
    headers: &[&str],
    rows: &[Vec<String>],
    priorities: &[u8],
    term_width: usize,
) -> BoxedTable {
    // Sanitised once, here, rather than inside `column_widths` and
    // `boxed_row` separately: a cell born from operator-chosen data (a
    // sheep name, a bark message, an adopted dog's path) reaches this
    // function raw, and `crate::output::width::visible_width`'s own doc
    // names this as the box-drawn renderer's job, not its own. Sanitising
    // once and reusing the result for both the width pass below and the
    // print pass is also what keeps the two in agreement -- see
    // `width::sanitize_cell`'s own doc for why sanitising twice risked
    // drifting.
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
        // The kept column with the highest priority number goes first --
        // priority 0 is the caller's way of saying "never", so a priority-0
        // column reaching this point means nothing droppable is left, and
        // the table stays wider than the terminal rather than losing an
        // identity column.
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
/// rather than by length or byte count -- a styled cell must pad by what it
/// shows, the same reason [`boxed_row`] measures it the same way.
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

    /// The day arm (`units[0]` in `human_duration`) is otherwise untouched
    /// by the test above, which never reaches a value >= 24h. Both cases
    /// also pin the "skip a zero middle unit" rule: 1d 5m has no hours, 1h
    /// 2s has no minutes, and each still takes exactly its two largest
    /// nonzero units rather than pairing adjacent slots regardless of value.
    #[test]
    fn human_duration_day_arm_skips_a_zero_middle_unit() {
        assert_eq!(human_duration(86_700_000), "1d 5m"); // 1 day + 5 minutes, 0 hours
        assert_eq!(human_duration(3_602_000), "1h 2s"); // 1 hour + 2 seconds, 0 minutes
    }

    /// Round-trips a real millis value through [`local_timestamp`] and back,
    /// rather than pinning a fixed string — this test must pass on any
    /// machine in any `$TZ`, and `std::env::set_var` is `unsafe` in edition
    /// 2024 (this crate is `#![forbid(unsafe_code)]`), so there is no way to
    /// pin the host's own zone from inside the test. Parsing the rendered
    /// cell back as a *local* naive datetime and converting it back to UTC
    /// is what actually proves the cell names the same instant `at_ms`
    /// does, whatever zone rendered it.
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

    /// fails if a millis value this crate cannot render (too large for
    /// `i64`, or in-range for `i64` but outside chrono's representable
    /// calendar) panics or silently drops the row, rather than falling back
    /// to the raw number — [`shep_core::barks::read`]'s own doc already
    /// tolerates a bad *line*; a good line with one unrenderable field
    /// should not be dropped for a narrower reason than that.
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
    /// (rows.rs) — that gate polices every real `Render` impl's `rows()`
    /// against its own `Serialize` output, but says nothing about a `rows()`
    /// that is simply wrong by construction. This type exists only to
    /// reach that panic and pin its message.
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

    /// fails if `human_bytes` renders a live RSS as raw digits. `MemSize`'s
    /// own Display only names a unit that divides the value exactly, and a
    /// resident set is never an exact number of MiB — so a column built on
    /// it would show "50462720" where an operator expects "48.1M".
    #[test]
    fn bytes_render_with_a_unit_a_reader_can_scan() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(50_462_720), "48.1M");
        assert_eq!(human_bytes(3 << 30), "3.0G");
        assert_eq!(human_bytes(u64::MAX), "16.0E");
    }

    /// "羊" is one character, three bytes in UTF-8, and **two columns** on a
    /// terminal. All three numbers differ, and only one of them is the width
    /// of the column it needs.
    ///
    /// This test used to assert the middle one — that two names with equal
    /// *character* counts render to equal-width rows — and that assertion was
    /// the bug, pinned. A six-character CJK name draws twice as wide as a
    /// six-character ASCII one, so a table that gives them the same column
    /// hangs the CJK name over its own border and shoves every column after
    /// it left. `docs/specs/deferred.md` recorded the same fault in
    /// `lookout`'s `fit`; both are measured by
    /// [`crate::output::width::char_columns`] now.
    ///
    /// So the property is stated in columns, and on the HEADER line as well
    /// as the row. The two fixtures differ only in their name, so both lines
    /// have to widen by exactly what that name draws wider — and only the
    /// header line proves the *padding* moved, since the row's own NAME cell
    /// is that name and would widen whatever the column did. Asserting on
    /// one line alone would leave [`visible_width`] free to return a
    /// constant.
    ///
    /// Not asserted: that a table's lines are all one width. `write_row`
    /// leaves the last cell unpadded on purpose so no line carries trailing
    /// whitespace, so they are not, and the boxed renderer's own
    /// `every_line_of_a_boxed_table_has_the_same_visible_width` is where
    /// that property belongs.
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

    /// The lines that make up the table itself -- top rule, header, the
    /// separator, each row, bottom rule -- rather than every line
    /// `render_boxed` returns. The footer's width has nothing to do with
    /// the table's (it is prose, not a box), so a same-width check over the
    /// raw output would fail the moment any column drops. Every box-drawn
    /// line starts with one of these four characters and nothing else does.
    fn table_lines(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| l.starts_with(['┌', '├', '│', '└']))
            .collect()
    }

    /// One cell's raw text before the `styled` wrapper below decides whether
    /// to also wrap it in a colour span: plain words most of the time, and
    /// -- whole-branch review item 3 -- occasionally a control character
    /// (`\n`/`\r`/`\t`, the exact class `sanitize_cell` exists to escape) or
    /// an unterminated CSI introducer with no final byte, the exact class it
    /// exists to drop. Before this task the strategy only ever generated
    /// `[a-z(). -]`, so this property test could hold even though the
    /// renderer had never actually been asked to sanitise a cell -- it
    /// stepped around the reachable case (`normalize()` rejects only `/`,
    /// `\`, `.` and `..` in a name) rather than covering it.
    fn dirty_cell_text() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;

        prop_oneof![
            3 => "[a-z(). -]{0,12}".prop_map(String::from),
            1 => ("[a-z]{0,4}", prop_oneof![Just('\n'), Just('\r'), Just('\t')], "[a-z]{0,4}")
                .prop_map(|(a, c, b)| format!("{a}{c}{b}")),
            // Digits and `;` only after the introducer, never a letter --
            // the same trap `an_unterminated_or_bare_escape_is_dropped_whole`
            // (`width.rs`'s own test module) avoids for the same reason: a
            // letter here is itself a valid final byte and would close the
            // sequence the case means to leave open.
            1 => "[a-z]{0,4}".prop_map(|a| format!("{a}\u{1b}[3;1")),
        ]
    }

    /// The invariant the whole feature rests on. Any rows, any width, any
    /// mix of styled and plain cells: every line of the table itself is the
    /// same visible width, and that width is either inside the terminal or
    /// the table has already been reduced to the floor of three columns --
    /// below that width the floor wins over fitting (spec assumption 2:
    /// three columns of worst-case cells cannot always fit a 20-column
    /// terminal, and dropping an identity column is not the answer).
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
        // The footer legitimately names a dropped column by its header text
        // (`the_footer_names_every_column_it_hid` below pins that), so a
        // `narrow.contains("FOLD")` check over the whole render would fail
        // on the footer's own announcement. What this assertion needs is
        // that the *column* is gone from the table itself.
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
    /// `term_width = 20`, not 30: at 30 the arithmetic (ID 5 + NAME 7 +
    /// STATUS 9 + CPU 6 + FOLD 7 + 1 = 35) only needs FOLD's drop to fit
    /// (35 -> 28 <= 30), so CPU would never reach the footer. At 20, FOLD's
    /// drop (35 -> 28) still does not fit, CPU's drop (28 -> 22) still does
    /// not fit either, and the floor of three stops the loop there -- both
    /// names reach the footer.
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

    /// No em dashes in copy a user reads -- the dropped-column footer is
    /// prose, same discipline `welcome.rs`'s
    /// `the_welcome_copy_has_no_em_dashes` and `status.rs`'s
    /// `the_status_lines_have_no_em_dashes` already pin for their own copy.
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

    /// `render_boxed_ex`'s own dropped list matches the footer it renders --
    /// `table_of`'s two-pass retry (`output/mod.rs`) trusts this list
    /// instead of re-deriving it, so this pins that the two never disagree.
    /// `render_boxed` (its thin wrapper) renders the identical string.
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

    /// The concrete bug whole-branch review item 3 named: `shep-core`'s
    /// `normalize()` rejects only `/`, `\`, `.` and `..` in a name, so a
    /// name carrying an embedded newline reaches this renderer -- reachable,
    /// not theoretical. Before `render_boxed_ex` sanitised every cell, a
    /// literal `\n` inside `boxed_row`'s output split that one row across
    /// two printed lines and misaligned every border beneath it.
    #[test]
    fn a_name_with_an_embedded_newline_does_not_split_its_own_row() {
        let headers = ["ID", "NAME", "STATUS"];
        let rows = vec![vec!["0".into(), "web\nworker".into(), "online".into()]];
        let out = render_boxed(&headers, &rows, &[0, 0, 0], 80);

        let box_lines: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with(['┌', '├', '│', '└']))
            .collect();
        // Top rule, header, separator, exactly one data row, bottom rule --
        // five lines. A literal newline surviving into the cell would have
        // split that one data row into two, making it six.
        assert_eq!(box_lines.len(), 5, "{out}");
        assert!(out.contains("web\\nworker"), "escaped, visible: {out}");
        assert!(!out.contains("web\nworker"), "no literal newline: {out:?}");
    }

    // --- Task 7: pin every level, through the real rendering seam ---------
    //
    // Every snapshot below goes through `crate::output::table_of`, the seam
    // `emit`/`emit_flock`/`emit_described` all call, over a `FlockRows` built
    // from real `ProcessInfo` values -- never `render_boxed` called on
    // hand-written cells. That is the whole difference between pinning box
    // drawing and pinning the feature this branch built: the face comes from
    // `vocabulary::face`, the colour from `output::paint::style_for` keyed
    // off `vocabulary::role_of`, and the STATUS-word retry from `table_of`
    // itself, and a snapshot that hand-writes `"(o.o) online"` walks past all
    // three.

    use std::ffi::OsStr;

    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use crate::output::table_of;
    use crate::style::{Presentation, StyleLevel};

    /// Four sheep, one per role [`crate::vocabulary::role_of`] maps a status
    /// to -- Meadow (Online), Butter (`butter`), Bark (Errored), Ink3
    /// (Stopped). Every row the same status would pin one face and say
    /// nothing about the other three (this task's own brief's Correction 2).
    ///
    /// `butter` is a parameter rather than fixed at `Starting`, because the
    /// two narrower snapshots below need two different Butter-role words at
    /// the same fallback terminal width: the narrow one wants
    /// `WaitingRestart` (`"waiting-restart"`, 15) specifically because it is
    /// the longest status word -- see that test's own doc for the width
    /// arithmetic this drives -- and the two `full_under_no_color`/`plain`
    /// snapshots want `Starting` for no reason narrower than "some Butter
    /// row has to exist and this one's doc comments already describe it".
    ///
    /// Every other column is sized to fit a width of 80 -- the value every
    /// `Presentation::new` call in this module's narrower tests passes
    /// explicitly as `width`, injected rather than measured. `table_of`
    /// reads `presentation.width`, never the process's own controlling
    /// terminal, so this arithmetic holds on any machine this suite runs
    /// on, a real developer's tty included (see [`crate::style::Presentation`]'s
    /// own doc for why the field exists).
    ///
    /// One exception: `full_wide_pins_face_word_and_colour_for_a_mixed_flock`
    /// stopped fitting at 80 the moment task 49 added an `EXIT` column (a
    /// 4-wide column costs 7 -- its own width plus the renderer's 3-per-column
    /// padding), and there was no free width left to give it. `EXIT` reads
    /// `-` on every row here (none of these four sheep carries a real
    /// `last_exit`), so the column itself is header-bound, not content-bound,
    /// and there is nothing left to shrink in `NAME`/`PID` that would recover
    /// seven columns without resorting to cryptic abbreviations. That test
    /// widens its own `Presentation` instead -- see its own doc for the
    /// number and the reasoning.
    ///
    /// A second exception, the same shape: task 7's `SMIT` column costs
    /// another seven columns here too (`"SMIT"` is also 4-wide, and none of
    /// these four sheep carries one, so it reads `-` the same way `EXIT`
    /// does). `full_narrow_drops_the_status_word_before_a_whole_column`
    /// moved off 80 for the same reason -- see its own doc for the new
    /// number.
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

    /// Not a behaviour test: a MEASUREMENT, recorded so the two-width tests
    /// below rest on a number rather than an assumption. `unicode-width`
    /// classifies these two by East Asian Width, which is ambiguous for
    /// some symbols and has moved between Unicode revisions, so what shep
    /// thinks a smit occupies is worth writing down.
    #[test]
    fn how_wide_the_real_smits_actually_are() {
        assert_eq!(visible_width("\u{25b2} main@a1b2c3"), 13);
        assert_eq!(visible_width("\u{23f8} main@f6e5d4"), 13);
    }

    /// A deep (256-colour) terminal, `xterm-256color` -- the same string
    /// `output/mod.rs`'s own Task 5b tests use to exercise
    /// `output::paint::style_for`'s deep tier rather than the 16-colour
    /// fallback, since a snapshot pinned at the shallow tier would not catch
    /// a regression in the tier most terminals in the wild actually use.
    fn deep_terminal() -> Option<&'static OsStr> {
        Some(OsStr::new("xterm-256color"))
    }

    /// A `Full`, deep-colour `Presentation` at `width`, the shape every test
    /// below wants and the only thing that varies between them.
    fn full_at(width: usize) -> Presentation {
        Presentation::new(StyleLevel::Full, None, deep_terminal(), None, width)
    }

    /// [`mixed_flock`], with two of its four rows carrying the real smit
    /// strings a deploy dog paints -- not a hand-built `Some("x")`, since
    /// the requirement under test is about a real smit at a real terminal
    /// width. Taken verbatim from `~/GitHub/shep-deploy/src/smit.rs`.
    fn mixed_flock_with_smits() -> FlockRows {
        let mut flock = mixed_flock(ProcStatus::Starting);
        flock.0[0].smit = Some("\u{25b2} main@a1b2c3".to_string());
        flock.0[2].smit = Some("\u{23f8} main@f6e5d4".to_string());
        flock
    }

    /// `full`, comfortably wide enough: face, word and colour all present,
    /// nothing dropped.
    ///
    /// Width 97, not this module's usual 80 (`mixed_flock`'s own doc has the
    /// exception and the arithmetic): the `EXIT` and `SMIT` columns each
    /// cost the fixture seven columns they had no slack left to give up, so
    /// 80 no longer fits `Starting`'s word
    /// (`"starting"`, the second-longest Butter-role word after
    /// `WaitingRestart`) alongside them. This test's own job was never
    /// "prove it fits at exactly the realistic fallback" -- that boundary
    /// belongs to the narrow snapshot below, which moved off 80 for the
    /// same reason -- it is "prove nothing drops when there is room", and
    /// 97 is still an ordinary terminal width, comfortably proving that.
    #[test]
    fn full_wide_pins_face_word_and_colour_for_a_mixed_flock() {
        let presentation = Presentation::new(StyleLevel::Full, None, deep_terminal(), None, 97);
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

    /// `full`, narrow enough that the STATUS word has dropped but no whole
    /// column has -- spec §2's own ordering, and the one width-driven
    /// behaviour a hand-written `render_boxed` call cannot exercise, since
    /// only `table_of`'s two-pass retry knows to ask [`Render::rows_for`]
    /// again with the word turned off.
    ///
    /// Swapping `mixed_flock`'s Butter row from `Starting` to
    /// `WaitingRestart` grows its STATUS content from `"(o~o) starting"`
    /// (14 columns) to `"(o~o) waiting-restart"` (21) -- seven columns more.
    /// Width 87, not the module's usual 80: task 7's `SMIT` column (empty
    /// here, same as `EXIT`) costs the fixture another seven columns it had
    /// no slack left to give up, the same arithmetic `mixed_flock`'s own
    /// doc records. `render_boxed_ex`'s own priority order
    /// (`FlockRows::PRIORITIES`) drops SMIT first on the word-included
    /// pass, being the highest priority number, landing back under budget
    /// without a second column needing to go. The retry itself asks for
    /// every column again with the word off; every face is exactly 5
    /// columns regardless of status (`vocabulary::face`'s own invariant),
    /// so STATUS falls back to its 6-column header width and the whole
    /// table fits with room for SMIT to return too -- word gone, SMIT and
    /// FOLD both back, no footer.
    #[test]
    fn full_narrow_drops_the_status_word_before_a_whole_column() {
        let presentation = Presentation::new(StyleLevel::Full, None, deep_terminal(), None, 87);
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
    /// smit. Asserted rather than assumed: `table.rs`'s own note at :875
    /// records that adding EXIT cost 7 columns and forced the wide fixture
    /// from 80 to 90, and a later column will move this too. When it moves,
    /// that is a decision about the maintainer's full-width condition, not a number to
    /// quietly update.
    const FULL_WIDTH: usize = 93;

    /// fails if a smit is dropped at full width. The maintainer's permission to drop
    /// it on a narrow terminal was conditional on it being seen regularly
    /// at a wide one, so a later column that crowded it out here would
    /// reopen a decision that was already made. This is the half of that
    /// condition her permission does not state outright.
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

    /// `plain`: boxes and colour survive, the word rides alone -- `plain` is
    /// "no sheep", not "no colour" (spec §2, and `output/mod.rs`'s own
    /// `the_three_levels_render_the_status_column_differently_and_look_right`
    /// asserts the same thing without pinning the exact render).
    #[test]
    fn plain_pins_the_boxed_table_with_words_and_colour_but_no_face() {
        let presentation = Presentation::new(StyleLevel::Plain, None, deep_terminal(), None, 80);
        let rendered = table_of(&mixed_flock(ProcStatus::Starting), presentation);
        assert!(!rendered.contains("(o.o)"), "no face at plain: {rendered}");
        insta::assert_snapshot!(rendered);
    }

    /// `bare`: the hard rule made visible. Byte-identical to what
    /// `render_table` printed before this whole feature existed -- no box,
    /// no face, no escape -- so a border or an escape byte reaching this
    /// file in review is the regression `cli_e2e.rs`'s piped-output assertion
    /// exists to catch at the process boundary, seen here at the unit level
    /// instead.
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
    /// vetoed -- the gap Correction 2 names explicitly: the spec asks for
    /// this case and the earlier tasks' plan had no snapshot for it, only
    /// `output/mod.rs`'s own assertion-based
    /// `no_color_at_full_keeps_sheep_and_boxes_but_drops_colour`.
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
