//! The host-usage strip: one line, five self-labelled segments.
//!
//! Half of it is read from this machine ([`super::super::source::HostSample`])
//! and half is summed from the flock the dashboard already holds. Every
//! segment names which half it belongs to, because a strip truncated by a
//! narrow terminal must never leave a bare `mem 12.4G` beside a bare
//! `mem 706.0M`.
//!
//! The flock half sums [`super::super::app::App::all_rows`], the WHOLE
//! flock, never [`super::super::app::App::rows`], the table's current view.
//! A strip that named itself `flock cpu`/`flock mem` is a claim about the
//! machine's total load, and a name filter narrowing the table must not
//! quietly narrow that claim along with it; the
//! title bar is where the filtered-vs-total split already lives
//! (`2 of 6 in the flock`), one row above this one.
//!
//! Segments are joined **in the drop order** — `up` last, then the flock's
//! memory, its CPU, the host's memory, with the load average first — and the
//! line is fitted with [`super::flock::fit`], the same call every other line
//! on this screen goes through. Truncating from the right therefore IS the
//! drop order, with no second mechanism to maintain, and the `…` says so the
//! way it does on every truncated name in the table above. See the phase
//! plan's design decision 9 for the machinery that was cut and why.

use ratatui::text::{Line, Span};

use super::super::app::App;
use crate::output::{human_bytes, human_duration};

/// The strip, fitted to `width`.
#[must_use]
pub fn strip_line(app: &App, width: u16) -> Line<'static> {
    // One `Span`, muted: nothing on this line is damage, and nothing on it is
    // a status word. Colour here would be decoration with no meaning behind
    // it, which is the one thing the palette module forbids.
    Line::from(Span::styled(
        super::flock::fit(&segments(app).join("   "), width),
        app.palette().muted(),
    ))
}

/// The segments, widest set first.
fn segments(app: &App) -> Vec<String> {
    let mut out = Vec::with_capacity(5);
    match app.host() {
        Some(host) => {
            let (one, five, fifteen) = host.load;
            out.push(match host.cores {
                Some(cores) => {
                    format!("host  load {one:.2} {five:.2} {fifteen:.2} / {cores} cores")
                }
                // No denominator: the numbers alone are not readable, so they
                // are shown without a claim about how many cores they are
                // spread over rather than with a guessed one.
                None => format!("host  load {one:.2} {five:.2} {fifteen:.2}"),
            });
            out.push(format!(
                "host mem {} / {}",
                human_bytes(host.memory_used_bytes),
                human_bytes(host.memory_total_bytes)
            ));
        }
        None if app.host_unsupported() => {
            out.push("host  usage is not available on this platform".to_string());
        }
        // Reachable for at most one redraw: `tokio::time::interval`'s first
        // tick is immediate, so the heartbeat samples before the second frame.
        // It still gets a sentence and a gallery scene — an untested string is
        // where this project's claims rot.
        None => out.push("host  not read yet".to_string()),
    }

    // Summed from the WHOLE flock, `all_rows`, not the table's current
    // `rows`: a filter that narrows what's on screen must not also narrow
    // what this strip claims about the machine, and `-` here must keep
    // meaning "no reading", never "the filter matched nothing" — see the
    // module doc and `App::all_rows`. Never requested on its own: this is
    // the same `ListFlock` reply the table already has. `-` and not `0.0%`
    // when nothing reported: `ProcessInfo::cpu_percent`'s own doc is
    // explicit that `None` is unknown, and rendering unknown as zero claims a
    // measurement the shepherd never made.
    let rows = app.all_rows();
    let cpu: Option<f32> = rows
        .iter()
        .filter_map(|row| row.info.cpu_percent)
        .fold(None, |sum, value| Some(sum.unwrap_or(0.0) + value));
    let mem: Option<u64> = rows
        .iter()
        .filter_map(|row| row.info.memory_bytes)
        .fold(None, |sum, value| Some(sum.unwrap_or(0) + value));
    out.push(cpu.map_or_else(
        || "flock cpu -".to_string(),
        |cpu| format!("flock cpu {cpu:.1}%"),
    ));
    out.push(mem.map_or_else(
        || "flock mem -".to_string(),
        |mem| format!("flock mem {}", human_bytes(mem)),
    ));

    // Last, and therefore the first thing a narrow terminal loses: a host that
    // has been up six days explains nothing about right now.
    if let Some(host) = app.host() {
        out.push(format!(
            "up {}",
            human_duration(host.uptime_seconds * 1_000)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::MIN_TERM_WIDTH;
    use super::super::fixtures::{flock_of, rendered, sample, with_host, with_host_none};
    use super::*;
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    /// fails if a segment stops saying whose number it is. A strip truncated
    /// by a narrow terminal must never leave a bare `mem 12.4G` beside a bare
    /// `mem 706.0M` — the two are the host's and the flock's, and an operator
    /// reading an incident cannot afford to guess which is which.
    #[test]
    fn every_segment_names_whose_number_it_is() {
        let app = with_host(sample(), flock_of(4, 1));
        let line = rendered(&strip_line(&app, 200));
        for segment in ["host  load", "host mem", "flock cpu", "flock mem", "up "] {
            assert!(line.contains(segment), "missing {segment:?} in {line:?}");
        }
    }

    /// fails if the strip stops truncating visibly, or if the load average
    /// stops being the segment that survives a narrow terminal.
    ///
    /// There is no drop loop and no width table. The segments are joined in
    /// the drop order — least useful last — and `flock::fit` truncates from
    /// the right, so truncating IS the drop order and this is the whole of the
    /// fitting behaviour. An earlier draft built a second mechanism for it and
    /// a test that walked every width from 200 down to 10 recording the order
    /// things vanished; Rin's ruling for this phase is "as plain as the flock
    /// table", and the ellipsis on every other line of the screen is the
    /// precedent. Three widths, not a hundred and ninety.
    #[test]
    fn a_narrow_strip_truncates_visibly_and_keeps_the_load_average() {
        let app = with_host(sample(), flock_of(4, 1));

        let narrow = rendered(&strip_line(&app, 40));
        assert!(narrow.starts_with("host  load"), "got {narrow:?}");
        assert!(
            narrow.ends_with('…'),
            "a truncation the operator can see: {narrow:?}"
        );
        assert!(
            !narrow.contains("up "),
            "`up` is the first thing off the end"
        );

        // At the floor the strip still says whose number it is quoting, which
        // is the reason every segment carries its own label.
        let floor = rendered(&strip_line(&app, MIN_TERM_WIDTH));
        assert!(floor.starts_with("host  load"), "got {floor:?}");

        // And where it fits, nothing is cut.
        let full = rendered(&strip_line(&app, 200));
        assert!(!full.contains('…'));
        assert!(
            full.contains("up 6d"),
            "the last segment is there: {full:?}"
        );
    }

    /// fails if an unknown flock reading renders as zero. `ProcessInfo`'s own
    /// doc is explicit that `None` covers three cases — not running, under one
    /// sampling window, or a shepherd predating the field — and that a reader
    /// renders all three as unknown and never as zero. `0.0%` claims a
    /// measurement the shepherd never made.
    #[test]
    fn a_flock_with_no_readings_shows_a_dash_and_not_a_zero() {
        let app = with_host(
            sample(),
            vec![ProcessInfo::builder(1, "web", ProcStatus::Errored).build()],
        );
        let line = rendered(&strip_line(&app, 200));
        assert!(line.contains("flock cpu -"), "got {line:?}");
        assert!(line.contains("flock mem -"), "got {line:?}");
        assert!(!line.contains("0.0%"));
    }

    /// fails if an unsupported platform stops saying so. `None` from the
    /// sampler is a real case, and a strip that silently dropped its host half
    /// would look like a strip whose numbers had not arrived yet.
    #[test]
    fn an_unread_host_says_which_of_the_two_reasons_it_is() {
        let unsupported = with_host_none(flock_of(4, 1), true);
        assert!(
            rendered(&strip_line(&unsupported, 200))
                .contains("host  usage is not available on this platform")
        );

        let not_yet = with_host_none(flock_of(4, 1), false);
        assert!(rendered(&strip_line(&not_yet, 200)).contains("host  not read yet"));

        // Both keep the flock half, which lookout can always compute.
        assert!(rendered(&strip_line(&unsupported, 200)).contains("flock cpu"));
    }

    /// fails if the strip's flock totals move when a filter narrows the
    /// table. The strip is a claim about the whole machine, not about
    /// whatever the table currently shows: `flock cpu`/`flock mem` must
    /// never silently sum the FILTERED set while staying labelled `flock`
    /// — a filter matching nothing would make a running flock's strip
    /// print `-`, the same cell this dashboard reserves for "no reading
    /// arrived yet".
    #[test]
    fn the_flock_totals_ignore_the_filter() {
        let mut app = with_host(sample(), flock_of(4, 1));
        let full = rendered(&strip_line(&app, 200));
        assert!(full.contains("flock cpu 3.5%"), "sanity: got {full:?}");

        // A filter matching nothing empties the table (`rows()`) without
        // touching the flock itself (`all_rows()`, what the strip reads).
        app.set_filter_for_tests("zzz");
        assert!(app.rows().is_empty(), "sanity: the filter matched nothing");
        let filtered = rendered(&strip_line(&app, 200));

        assert_eq!(
            full, filtered,
            "the strip must not change when the table's filter does: {filtered:?}"
        );
        assert!(
            !filtered.contains("flock cpu -"),
            "a filter matching nothing is not the same as no reading arriving: {filtered:?}"
        );
    }
}
