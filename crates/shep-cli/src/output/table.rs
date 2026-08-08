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
/// Not called outside this module's own tests yet: `emit`'s `Format::Table`
/// arm is its only real caller, and `emit` itself has no caller until Tasks
/// 7-11 land. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
#[allow(dead_code)]
pub fn render_table<T: Render>(data: &T) -> String {
    let headers = T::headers();
    let rows = data.rows();

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
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
}
