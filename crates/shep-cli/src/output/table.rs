//! The padded table renderer and human-readable duration formatting — the
//! two pieces of `--format table` that have nothing to do with any one
//! payload type.

use super::Render;

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

    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
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
fn write_row<'a>(out: &mut String, cells: impl Iterator<Item = &'a str>, widths: &[usize]) {
    let cells: Vec<&str> = cells.collect();
    let last = cells.len().saturating_sub(1);
    for (i, cell) in cells.into_iter().enumerate() {
        if i == last {
            out.push_str(cell);
        } else {
            out.push_str(&format!("{cell:<width$}", width = widths[i]));
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
        shep_core::protocol::ProcessInfo {
            id: 1,
            name: name.to_string(),
            status: shep_core::status::ProcStatus::Online,
            pid: None,
            restarts: 0,
            uptime_ms: 0,
            fold: None,
            out_file: None,
            err_file: None,
        }
    }

    /// "羊" is one character but three bytes in UTF-8. If column width were
    /// computed with `str::len()` (bytes) instead of char count, a
    /// six-character CJK name (18 bytes) would produce a far wider NAME
    /// column than a six-character ASCII name (6 bytes) — even though
    /// `{:<w$}` pads both to the same *character* width either way.
    #[test]
    fn column_widths_count_characters_not_bytes() {
        let ascii_name = "wwwwww".to_string(); // 6 chars, 6 bytes
        let cjk_name = "羊".repeat(6); // 6 chars, 18 bytes
        assert_eq!(ascii_name.chars().count(), cjk_name.chars().count());

        let ascii_line = render_table(&FlockRows(vec![info_with_name(&ascii_name)]))
            .lines()
            .nth(1)
            .unwrap()
            .chars()
            .count();
        let cjk_line = render_table(&FlockRows(vec![info_with_name(&cjk_name)]))
            .lines()
            .nth(1)
            .unwrap()
            .chars()
            .count();

        assert_eq!(
            ascii_line, cjk_line,
            "two names with equal character counts must produce equal-width rendered rows, \
             regardless of how many UTF-8 bytes either one takes"
        );
    }
}
