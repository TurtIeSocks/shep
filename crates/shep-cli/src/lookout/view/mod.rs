//! `draw`: one `App`, one `Frame`, six regions of arithmetic.
//!
//! No `Layout`, no `Constraint`, no widget. The upstream surface this whole
//! phase touches is six items wide — `Frame::area`, `Frame::buffer_mut`,
//! `Buffer::set_line`, `Line`, `Span`, `Style` — which is what makes the
//! render path both testable and cheap to keep working across a ratatui
//! release. See the phase plan's design decision 5b for the argument.

pub mod bleats;
pub mod detail;
pub mod flock;
pub mod host;
pub mod status;

// `pub`, not private: Task 8's `a_heartbeat_puts_the_host_strip_on_the_frame`
// lives in `super::super::mod`'s own `mod tests` (it drives `run_ui`, and
// that is where `run_ui`'s other tests and `FakeLocal` already are), and it
// needs `fixtures::sample()` from there. `#[cfg(test)]` still keeps every
// item out of the ordinary build, the same shape `lookout::frames` uses.
#[cfg(test)]
pub mod fixtures;

use ratatui::Frame;
use ratatui::text::{Line, Span};

use self::flock::MIN_HEIGHT;
use super::app::App;

/// The narrowest terminal the dashboard draws into.
///
/// The table's own floor ([`flock::MIN_WIDTH`], 31) plus the selection
/// marker's gutter ([`flock::GUTTER`], 2). Below this the whole draw becomes
/// two short lines saying so.
pub const MIN_TERM_WIDTH: u16 = flock::MIN_WIDTH + flock::GUTTER;

/// Rows the chrome always takes: title, column header, rule, status bar.
///
/// The banner is deliberately not in this count — it is one row only when
/// the link is not live, and every caller that needs the worst case adds it
/// separately, the way `every_pane_tier_fits_the_height_it_claims` does.
///
/// `#[cfg(test)]`: `draw` lays the four chrome rows out one `y += 1` at a
/// time rather than summing them first, so this has no production call
/// site — its only reader is the test that proves `PANE_TIERS` leaves
/// enough room for chrome, a banner and a table, the same shape
/// `lookout::frames` already uses for a module read only by tests.
#[cfg(test)]
const CHROME_ROWS: u16 = 4;

/// The host strip is one line.
const HOST_ROWS: u16 = 1;

/// The detail pane: one rule and four lines.
const DETAIL_ROWS: u16 = 5;

/// The bleats feed: one rule, one header, five lines.
const FEED_ROWS: u16 = 7;

/// Which optional panes a terminal of a given height gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    /// The host-usage strip, under the title.
    pub host: bool,
    /// The sheep detail pane, under the table.
    pub detail: bool,
    /// The bleats feed, under that.
    pub feed: bool,
}

impl Panes {
    /// The flock table alone — 12a's frame, and what every terminal shorter
    /// than [`PANE_TIERS`]' last threshold gets.
    pub const NONE: Self = Self {
        host: false,
        detail: false,
        feed: false,
    };

    /// How many rows these panes take together.
    ///
    /// `#[cfg(test)]`: `draw` claims each pane's rows off `floor` one
    /// constant at a time as it lays the bottom stack out, so it never needs
    /// the sum — only `every_pane_tier_fits_the_height_it_claims` does, to
    /// check that sum against `PANE_TIERS`' own thresholds.
    #[cfg(test)]
    #[must_use]
    pub const fn rows(self) -> u16 {
        let mut rows = 0;
        if self.host {
            rows += HOST_ROWS;
        }
        if self.detail {
            rows += DETAIL_ROWS;
        }
        if self.feed {
            rows += FEED_ROWS;
        }
        rows
    }
}

/// Height thresholds, tallest first. Each entry is the shortest terminal that
/// still gets that pane set.
///
/// The drop order is least-diagnostic-first and it is a decision, not an
/// accident of ordering — see this module's own doc and the phase plan's
/// design decision 8. 24 is not arbitrary either: it is the classic terminal
/// height, and the table is chosen so a plain 80×24 gets all three panes with
/// a flock table worth reading.
const PANE_TIERS: &[(u16, Panes)] = &[
    (
        24,
        Panes {
            host: true,
            detail: true,
            feed: true,
        },
    ),
    (
        18,
        Panes {
            host: true,
            detail: false,
            feed: true,
        },
    ),
    (
        14,
        Panes {
            host: true,
            detail: false,
            feed: false,
        },
    ),
    (MIN_HEIGHT, Panes::NONE),
];

/// The widest pane set that fits `height`.
#[must_use]
pub fn panes_for(height: u16) -> Panes {
    PANE_TIERS
        .iter()
        .find(|(threshold, _)| height >= *threshold)
        .map_or(Panes::NONE, |(_, panes)| *panes)
}

/// Renders the whole dashboard.
///
/// Synchronous and total: every branch below draws something, and the
/// degenerate cases draw a sentence rather than nothing. A blank pane cannot
/// tell an operator whether the shepherd has nothing to run or whether the
/// dashboard is broken — the same reason `output::table::render_table`
/// prints its header row for an empty payload.
///
/// `draw`'s real caller is `super::mod`'s `run_ui`, once per frame.
pub fn draw(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area();
    let (width, height) = (area.width, area.height);
    let palette = app.palette();

    if width < MIN_TERM_WIDTH || height < MIN_HEIGHT {
        // Two nine-character lines, not one 39-character sentence.
        // `Buffer::set_line` truncates at `max_width` in silence, and this
        // branch exists for terminals narrower than `MIN_TERM_WIDTH` — so a
        // refusal that does not fit inside `MIN_TERM_WIDTH` loses its own
        // numbers at exactly the widths that need them. Nine columns is the
        // floor at which both lines are still whole, and below that nothing
        // helps.
        if width == 0 || height == 0 {
            return;
        }
        let first = Line::from(Span::raw("too small"));
        frame.buffer_mut().set_line(area.x, area.y, &first, width);
        if height >= 2 {
            let second = Line::from(Span::raw(format!("need {MIN_TERM_WIDTH}x{MIN_HEIGHT}")));
            frame
                .buffer_mut()
                .set_line(area.x, area.y + 1, &second, width);
        }
        return;
    }

    let panes = panes_for(height);
    let mut y = area.y;
    // The status bar's own row. Held once, up front: the bottom stack below
    // is laid out UPWARD from it, so nothing has to know the flock table's
    // length before deciding where the table ends.
    let bottom = area.y + height - 1;
    let buffer = frame.buffer_mut();

    buffer.set_line(
        area.x,
        y,
        &status::title_line(app, app.home(), width),
        width,
    );
    y += 1;

    if let Some(banner) = status::banner_line(app) {
        buffer.set_line(area.x, y, &banner, width);
        y += 1;
    }

    if panes.host {
        buffer.set_line(area.x, y, &host::strip_line(app, width), width);
        y += HOST_ROWS;
    }

    // width >= MIN_TERM_WIDTH, checked above, so this never underflows.
    let table_width = width - flock::GUTTER;
    let columns = flock::columns_for(table_width);
    buffer.set_line(
        area.x + flock::GUTTER,
        y,
        &flock::header_line(columns, table_width, palette.muted()),
        table_width,
    );
    y += 1;
    // The rule stays full width — it is chrome, and a rule that stopped two
    // columns short of the left edge would look like a rendering bug.
    buffer.set_line(area.x, y, &status::rule_line(palette.muted(), width), width);
    y += 1;

    // The bottom stack, laid out UPWARD from the status bar: whichever of
    // the detail pane and the feed are up claim their rows off `bottom`
    // first, and the table gets whatever is left between `y` and `floor`.
    let mut floor = bottom;
    let feed_at = panes.feed.then(|| {
        floor -= FEED_ROWS;
        floor
    });
    let detail_at = panes.detail.then(|| {
        floor -= DETAIL_ROWS;
        floor
    });

    // Everything from `y` up to `floor`, exactly as Task 2 left it, save for
    // the viewport now stopping at `floor` rather than at the status bar.
    let viewport = usize::from(floor - y);
    let rows = app.rows();
    if rows.is_empty() {
        // Two sentences, because there are two reasons and an operator cannot
        // tell them apart from a blank table. `the flock is empty` stays for
        // the case it describes and no other.
        let text = if app.flock_len() == 0 {
            "the flock is empty".to_string()
        } else {
            format!("no sheep's name contains \"{}\"", app.filter())
        };
        let line = Line::from(Span::styled(text, palette.muted()));
        buffer.set_line(area.x, y, &line, width);
    } else {
        let offset = flock::scroll_offset(app.selected_index().unwrap_or(0), viewport, rows.len());
        for (slot, row) in rows.iter().skip(offset).take(viewport).enumerate() {
            let slot = u16::try_from(slot).unwrap_or(0);
            let selected = app.selected() == Some(row.info.id);
            buffer.set_line(
                area.x,
                y + slot,
                &Line::from(Span::raw(flock::mark(selected))),
                1,
            );
            buffer.set_line(
                area.x + flock::GUTTER,
                y + slot,
                &flock::row_line(app, row, columns, table_width),
                table_width,
            );
        }
    }

    if let Some(top) = detail_at {
        buffer.set_line(
            area.x,
            top,
            &status::rule_line(palette.muted(), width),
            width,
        );
        for (offset, line) in detail::detail_lines(app, width).iter().enumerate() {
            let offset = u16::try_from(offset).unwrap_or(0);
            buffer.set_line(area.x, top + 1 + offset, line, width);
        }
    }
    if let Some(top) = feed_at {
        buffer.set_line(
            area.x,
            top,
            &status::rule_line(palette.muted(), width),
            width,
        );
        let rows = usize::from(FEED_ROWS - 1);
        for (offset, line) in bleats::feed_lines(app, width, rows).iter().enumerate() {
            let offset = u16::try_from(offset).unwrap_or(0);
            buffer.set_line(area.x, top + 1 + offset, line, width);
        }
    }

    buffer.set_line(area.x, bottom, &status::status_line(app, width), width);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use super::*;
    use crate::lookout::app::{App, Control, KeyPress, Msg};
    use crate::lookout::theme::Palette;

    fn draw_to(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(app, frame)).unwrap();
        crate::lookout::frames::render_text(terminal.backend().buffer())
    }

    /// fails if a terminal too small to hold the pane is drawn into anyway,
    /// **or if the refusal stops fitting in the terminal it is refusing
    /// about.** Overlapping garbage in a 20-column terminal reads as a crash,
    /// and the operator's next move is to kill the process rather than
    /// resize.
    ///
    /// The second half is the one that regresses silently. `Buffer::set_line`
    /// truncates at `max_width` without complaint, and this branch exists
    /// for terminals narrower than 33 columns — so a refusal written as one
    /// 39-character sentence loses `33x6` off the right-hand edge at exactly
    /// the widths that need to read it. Both assertions below are on the
    /// WHOLE line, trimmed, rather than on `contains`, because `contains`
    /// passes on a truncated line as happily as on a whole one.
    #[test]
    fn a_terminal_below_the_floor_says_so_instead_of_drawing() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            Instant::now(),
        );
        let frame = draw_to(&app, 28, 8);
        let mut lines = frame.lines();
        assert_eq!(lines.next().unwrap().trim_end(), "too small");
        assert_eq!(lines.next().unwrap().trim_end(), "need 33x6");
        assert!(!frame.contains("STATUS"), "no header was drawn");

        // Narrower still, and taller than the floor in rows: the numbers
        // must survive here too, because a 12-column terminal is precisely
        // the case this message exists for.
        let cramped = draw_to(&app, 12, 8);
        assert!(
            cramped.lines().nth(1).unwrap().trim_end() == "need 33x6",
            "the dimensions were cut off in the terminal that needed them"
        );

        // One row to write into: the second line has nowhere to go, and the
        // draw must not reach past the buffer for it.
        let single = draw_to(&app, 20, 1);
        assert_eq!(single.lines().next().unwrap().trim_end(), "too small");
        assert_eq!(single.lines().count(), 1);
    }

    /// fails if an empty flock renders as a blank pane. A bare empty screen
    /// does not tell an operator whether the shepherd has nothing to run or
    /// whether the dashboard is broken — the same reason
    /// `output::table::render_table` prints its header row for an empty
    /// payload.
    #[test]
    fn an_empty_flock_still_prints_the_header_and_says_it_is_empty() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            Instant::now(),
        );
        let frame = draw_to(&app, 100, 12);
        assert!(frame.contains("STATUS"));
        assert!(frame.contains("the flock is empty"));
    }

    /// fails if a filter that matches nothing says the flock is empty. It is
    /// not empty, and that sentence belongs to the case it describes. Three
    /// panes say three different things here because there are three different
    /// reasons, which is the `empty` scene's own principle.
    #[test]
    fn a_filter_matching_nothing_does_not_say_the_flock_is_empty() {
        let app = fixtures::filtered_app("zzz");
        let frame = draw_to(&app, 120, 30);
        assert!(
            frame.contains("no sheep's name contains \"zzz\""),
            "the table body names the query: {frame:?}"
        );
        assert!(
            !frame.contains("the flock is empty"),
            "and does not claim the flock is: {frame:?}"
        );
        assert!(
            frame.contains("no sheep selected: no name contains \"zzz\""),
            "the detail pane says its own reason: {frame:?}"
        );
        assert!(
            frame.contains("bleats  no sheep is selected"),
            "the feed's sentence is already true and is unchanged: {frame:?}"
        );
    }

    /// fails if the genuinely empty flock loses its own sentence. The mirror
    /// of the test above: one of the two branches getting the other's text is
    /// the failure, and only asserting both catches it.
    #[test]
    fn an_empty_flock_still_says_the_flock_is_empty() {
        let app = fixtures::filtered_app_of(Vec::new(), "");
        let frame = draw_to(&app, 120, 30);
        assert!(frame.contains("the flock is empty"), "got {frame:?}");
        assert!(!frame.contains("no sheep's name contains"), "got {frame:?}");
    }

    /// fails if the table stops leaving room for the marker, or if the marker
    /// stops landing on the selected row. Asserted on the WHOLE line rather
    /// than with `contains`, because a `>` somewhere in a log path would
    /// satisfy `contains` and prove nothing.
    #[test]
    fn the_marker_sits_in_the_gutter_of_the_selected_row_and_nowhere_else() {
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            Instant::now(),
        );
        app.update(Msg::Snapshot {
            rows: (0..4)
                .map(|id| {
                    ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
                })
                .collect(),
            at: Instant::now(),
        });
        app.update(Msg::Key(KeyPress::SelectDown));

        let frame = draw_to(&app, 100, 12);
        let rows: Vec<&str> = frame.lines().skip(3).take(4).collect();
        assert!(
            rows[0].starts_with("  0 "),
            "unselected rows keep a blank gutter: {:?}",
            rows[0]
        );
        assert!(
            rows[1].starts_with("> 1 "),
            "the marker is on row 1: {:?}",
            rows[1]
        );
        assert!(
            rows[2].starts_with("  2 "),
            "and on no other row: {:?}",
            rows[2]
        );
        assert_eq!(
            frame.lines().filter(|line| line.starts_with('>')).count(),
            1,
            "exactly one marker on the frame"
        );
    }

    /// fails if a frozen dashboard does not say so where it cannot be
    /// missed. This is the whole of the maintainer's ruling made visible: last values
    /// on screen, and a sentence admitting they are stale.
    #[test]
    fn a_frozen_link_puts_the_banner_under_the_title() {
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            Instant::now(),
        );
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        let frame = draw_to(&app, 100, 12);
        let banner = frame.lines().nth(1).expect("a second line").to_string();
        assert!(banner.contains("the shepherd has died"));
        assert!(banner.contains("2026-08-14 14:32:07"));
    }

    /// fails if the control state stops being visible. An operator who does
    /// not know whether their dashboard can act is one keystroke from
    /// finding out the wrong way.
    #[test]
    fn the_status_bar_always_says_which_control_state_is_in_force() {
        let now = Instant::now();
        let read_only = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            now,
        );
        assert!(draw_to(&read_only, 100, 12).contains("read-only"));

        let allowed = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/ada/.shep".to_string(),
            now,
        );
        let frame = draw_to(&allowed, 100, 12);
        assert!(frame.contains("control enabled"));
        assert!(!frame.contains("read-only"));
    }

    /// fails if a draw panics at a degenerate or an enormous size. IR-40's
    /// boundary sweep: the failure mode this catches is an arithmetic
    /// underflow on `height - 1` in a one-row terminal.
    #[test]
    fn drawing_never_panics_across_the_size_sweep() {
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            Instant::now(),
        );
        app.update(Msg::Snapshot {
            rows: (0..200)
                .map(|id| {
                    ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
                })
                .collect(),
            at: Instant::now(),
        });
        for (width, height) in [(1, 1), (20, 3), (31, 6), (80, 24), (250, 60), (400, 200)] {
            let _ = draw_to(&app, width, height);
        }
    }

    /// fails if a tier can render taller than the terminal it was chosen for.
    /// The height twin of `every_tier_fits_the_width_it_claims`, and the check
    /// that makes the tier table a claim rather than a wish: every tier's
    /// fixed rows, plus a banner, plus a flock table worth having, must fit in
    /// that tier's own threshold.
    #[test]
    fn every_pane_tier_fits_the_height_it_claims() {
        for height in flock::MIN_HEIGHT..=200 {
            let panes = panes_for(height);
            let fixed = CHROME_ROWS + 1 /* banner */ + panes.rows();
            // A tier that shows a pane must leave the table at least three
            // rows; the floor tier, which shows none, only has to leave one —
            // which is what MIN_HEIGHT has always meant.
            let floor = if panes.rows() == 0 { 1 } else { 3 };
            assert!(
                fixed + floor <= height,
                "height {height} chose {panes:?}, needing {} rows",
                fixed + floor
            );
        }
    }

    /// fails if the pane grew a line and the tier table did not grow with it.
    /// `every_pane_tier_fits_the_height_it_claims` picks `DETAIL_ROWS` up
    /// automatically and should stay green; if it does not, the tier table is
    /// wrong, not the test. At 24 rows the fixed cost becomes chrome 4, banner
    /// 1, host 1, detail 5, feed 7, which is 18, leaving 6 for the table
    /// against the tier test's floor of 3.
    #[test]
    fn the_detail_pane_claims_the_rows_it_draws() {
        let app = fixtures::with_selection(fixtures::sheep_with_lambs());
        assert_eq!(
            detail::detail_lines(&app, 120).len(),
            usize::from(DETAIL_ROWS - 1),
            "one rule plus its content lines"
        );
    }

    /// fails if the drop order changes without someone re-arguing it. The
    /// DETAIL pane goes first because it is the most redundant thing on the
    /// screen — every number on it but the log paths is in the row above it.
    /// The FEED goes second: its content exists nowhere else, but five lines
    /// of a busy log is thin. The HOST STRIP goes last: one row, and nothing
    /// else on the dashboard says anything about the machine.
    #[test]
    fn panes_drop_in_a_fixed_order_as_the_terminal_shortens() {
        assert_eq!(
            panes_for(60),
            Panes {
                host: true,
                detail: true,
                feed: true
            }
        );
        assert_eq!(
            panes_for(24),
            Panes {
                host: true,
                detail: true,
                feed: true
            }
        );
        assert_eq!(
            panes_for(23),
            Panes {
                host: true,
                detail: false,
                feed: true
            }
        );
        assert_eq!(
            panes_for(18),
            Panes {
                host: true,
                detail: false,
                feed: true
            }
        );
        assert_eq!(
            panes_for(17),
            Panes {
                host: true,
                detail: false,
                feed: false
            }
        );
        assert_eq!(
            panes_for(14),
            Panes {
                host: true,
                detail: false,
                feed: false
            }
        );
        assert_eq!(panes_for(13), Panes::NONE);
        assert_eq!(
            panes_for(flock::MIN_HEIGHT),
            Panes::NONE,
            "12a's frame, untouched"
        );
    }

    /// fails if a pane ever draws over the status bar, over the flock table,
    /// or off the bottom of the buffer. `Buffer::set_line` outside the area is
    /// a panic in debug and a silent no-op otherwise, and the arithmetic here
    /// has four moving parts.
    ///
    /// A live sweep, not a `timeout`: `draw` is synchronous, so a timer around
    /// it would complete on its first poll and bound nothing.
    #[test]
    fn every_pane_lands_inside_its_own_rows_across_the_size_sweep() {
        let mut app = fixtures::full_app();
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        for height in flock::MIN_HEIGHT..=60 {
            for width in [MIN_TERM_WIDTH, 40, 51, 80, 120, 200] {
                let frame = draw_to(&app, width, height);
                let lines: Vec<&str> = frame.lines().collect();
                let panes = panes_for(height);

                // The table's own row band, recomputed independently of
                // `draw` rather than trusted from it — this is what makes
                // the test's name true. Title (1) + banner (1, always: this
                // fixture is frozen, so `banner_line` is always `Some`) +
                // the host strip if it is up + the table's own header and
                // rule (2) is where the table's rows START; `floor`, walked
                // up from the status row exactly as `draw` walks it, is
                // where they STOP. A bottom-stack pane drawn downward from
                // the table instead of upward from the status bar — the
                // exact regression a snapshot test caught here before this
                // check existed — would put its content inside
                // `table_body_start..table_body_end` and this loop would
                // catch it on the next line.
                let table_body_start = 2 + if panes.host { HOST_ROWS } else { 0 } + 2;
                let mut floor = height - 1;
                if panes.feed {
                    floor -= FEED_ROWS;
                }
                if panes.detail {
                    floor -= DETAIL_ROWS;
                }
                let table_body_end = floor;
                for (i, line) in lines.iter().enumerate() {
                    let i = u16::try_from(i).unwrap_or(u16::MAX);
                    if i < table_body_start || i >= table_body_end {
                        continue;
                    }
                    assert!(
                        !line.starts_with("bleats  "),
                        "the feed header sits inside the table's own rows at \
                         {width}x{height}, row {i}"
                    );
                    assert!(
                        !line.starts_with("out  /home/ada/.shep/logs/"),
                        "the detail pane's out path sits inside the table's \
                         own rows at {width}x{height}, row {i}"
                    );
                }

                // NOT `lines.len() == height`: `frames::render_text` maps
                // `(0..area.height)` over `(0..area.width)` by construction,
                // so that holds for any `draw` whatsoever — including one that
                // drew nothing at all. It is a property of the renderer, not
                // of this layout, and asserting it here would be a check that
                // cannot fail.
                let last = lines.last().unwrap();
                assert!(
                    last.contains("read-only"),
                    "the status bar survived at {width}x{height}: {last:?}"
                );
                // The row above the status bar belongs to the bottom-most pane
                // that is up, so it is never blank — a blank one means the
                // upward layout left a hole, which is the failure mode the
                // arithmetic here actually has.
                if panes.feed || panes.detail {
                    let above = lines[lines.len() - 2];
                    assert!(
                        !above.trim().is_empty(),
                        "a blank row above the status bar at {width}x{height}"
                    );
                }
                // And every pane that is up appears exactly once, AND sits in
                // its own band, so nothing overlapped anything else and
                // nothing landed in the wrong place while still appearing
                // once.
                if panes.host {
                    let positions: Vec<usize> = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| l.starts_with("host  "))
                        .map(|(i, _)| i)
                        .collect();
                    assert_eq!(positions.len(), 1, "the strip at {width}x{height}");
                    assert!(
                        u16::try_from(positions[0]).unwrap_or(u16::MAX) < table_body_start,
                        "the strip at {width}x{height} sits at row {}, at or below the table",
                        positions[0]
                    );
                }
                if panes.feed {
                    let positions: Vec<usize> = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| l.starts_with("bleats  "))
                        .map(|(i, _)| i)
                        .collect();
                    assert_eq!(positions.len(), 1, "the feed header at {width}x{height}");
                    assert!(
                        u16::try_from(positions[0]).unwrap_or(0) >= table_body_end,
                        "the feed header at {width}x{height} sits at row {}, inside or above the table",
                        positions[0]
                    );
                }
                if panes.detail {
                    // The PATH prefix, not a bare `out  ` — the feed's own
                    // body lines are tagged `out  ` too, and counting those
                    // would make this assertion depend on how many log lines
                    // the fixture happens to carry.
                    let positions: Vec<usize> = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| l.starts_with("out  /home/ada/.shep/logs/"))
                        .map(|(i, _)| i)
                        .collect();
                    assert_eq!(
                        positions.len(),
                        1,
                        "the detail pane's out path at {width}x{height}"
                    );
                    assert!(
                        u16::try_from(positions[0]).unwrap_or(0) >= table_body_end,
                        "the detail pane's out path at {width}x{height} sits at row {}, inside or above the table",
                        positions[0]
                    );
                }
            }
        }
    }

    /// fails if the flock table stops being the spine. The maintainer's ruling in one
    /// test: whatever else is on screen, the table gets the remainder, and at
    /// the tier where all three panes are up it still has room for more than
    /// a couple of rows.
    #[test]
    fn the_flock_table_keeps_the_middle_of_the_screen() {
        let app = fixtures::full_app(); // twelve sheep
        let frame = draw_to(&app, 120, 24);
        let data_rows = frame
            .lines()
            .filter(|line| line.starts_with("  ") || line.starts_with("> "))
            .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .count();
        assert!(data_rows >= 5, "the table got {data_rows} rows at 120x24");
    }
}
