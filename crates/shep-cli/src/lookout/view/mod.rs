//! `draw`: one `App`, one `Frame`, six regions of arithmetic.
//!
//! No `Layout`, no `Constraint`, no widget. The upstream surface this whole
//! phase touches is six items wide — `Frame::area`, `Frame::buffer_mut`,
//! `Buffer::set_line`, `Line`, `Span`, `Style` — which is what makes the
//! render path both testable and cheap to keep working across a ratatui
//! release. See the phase plan's design decision 5b for the argument.

pub mod flock;
pub mod status;

use ratatui::Frame;
use ratatui::text::{Line, Span};

use self::flock::{MIN_HEIGHT, MIN_WIDTH};
use super::app::App;

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

    if width < MIN_WIDTH || height < MIN_HEIGHT {
        // Two nine-character lines, not one 39-character sentence.
        // `Buffer::set_line` truncates at `max_width` in silence, and this
        // branch exists for terminals narrower than `MIN_WIDTH` — so a
        // refusal that does not fit inside `MIN_WIDTH` loses its own numbers
        // at exactly the widths that need them. Nine columns is the floor at
        // which both lines are still whole, and below that nothing helps.
        if width == 0 || height == 0 {
            return;
        }
        let first = Line::from(Span::raw("too small"));
        frame.buffer_mut().set_line(area.x, area.y, &first, width);
        if height >= 2 {
            let second = Line::from(Span::raw(format!("need {MIN_WIDTH}x{MIN_HEIGHT}")));
            frame
                .buffer_mut()
                .set_line(area.x, area.y + 1, &second, width);
        }
        return;
    }

    let mut y = area.y;
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

    let columns = flock::columns_for(width);
    buffer.set_line(
        area.x,
        y,
        &flock::header_line(columns, width, palette.muted()),
        width,
    );
    y += 1;
    buffer.set_line(area.x, y, &status::rule_line(palette.muted(), width), width);
    y += 1;

    // Everything from here to the line above the status bar.
    let viewport = usize::from(area.y + height - 1 - y);
    let rows = app.rows();
    if rows.is_empty() {
        let line = Line::from(Span::styled("the flock is empty", palette.muted()));
        buffer.set_line(area.x, y, &line, width);
    } else {
        let offset = flock::scroll_offset(app.scroll(), viewport, rows.len());
        for (slot, row) in rows.iter().skip(offset).take(viewport).enumerate() {
            let line = flock::row_line(app, row, columns, width);
            let slot = u16::try_from(slot).unwrap_or(0);
            buffer.set_line(area.x, y + slot, &line, width);
        }
    }

    buffer.set_line(
        area.x,
        area.y + height - 1,
        &status::status_line(app, width),
        width,
    );
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use super::*;
    use crate::lookout::app::{App, Control, Msg};
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
    /// for terminals narrower than 31 columns — so a refusal written as one
    /// 39-character sentence loses `31x6` off the right-hand edge at exactly
    /// the widths that need to read it. Both assertions below are on the
    /// WHOLE line, trimmed, rather than on `contains`, because `contains`
    /// passes on a truncated line as happily as on a whole one.
    #[test]
    fn a_terminal_below_the_floor_says_so_instead_of_drawing() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let frame = draw_to(&app, 28, 8);
        let mut lines = frame.lines();
        assert_eq!(lines.next().unwrap().trim_end(), "too small");
        assert_eq!(lines.next().unwrap().trim_end(), "need 31x6");
        assert!(!frame.contains("STATUS"), "no header was drawn");

        // Narrower still, and taller than the floor in rows: the numbers
        // must survive here too, because a 12-column terminal is precisely
        // the case this message exists for.
        let cramped = draw_to(&app, 12, 8);
        assert!(
            cramped.lines().nth(1).unwrap().trim_end() == "need 31x6",
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
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let frame = draw_to(&app, 100, 12);
        assert!(frame.contains("STATUS"));
        assert!(frame.contains("the flock is empty"));
    }

    /// fails if a frozen dashboard does not say so where it cannot be
    /// missed. This is the whole of Rin's ruling made visible: last values
    /// on screen, and a sentence admitting they are stale.
    #[test]
    fn a_frozen_link_puts_the_banner_under_the_title() {
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
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
            "/home/rin/.shep".to_string(),
            now,
        );
        assert!(draw_to(&read_only, 100, 12).contains("read-only"));

        let allowed = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/rin/.shep".to_string(),
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
            "/home/rin/.shep".to_string(),
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
}
