//! The one scroll driver every screen taller than its terminal shares.
//!
//! [`Viewport`](crate::lookout::viewport::Viewport) scrolls in data rows,
//! while the height a screen is drawn into counts lines. A screen with
//! chrome (a section header, a blank separator, a caption, a column
//! header, two hidden-count markers) cannot ask the viewport how much
//! fits and get an answer about its own body.
//!
//! The fix is the same shape on every screen: treat the viewport's offset
//! as a starting point, lay the body out from there, and walk down one
//! row at a time until the cursor's row is among the lines that came
//! back. The walk and its last resort live here.
//!
//! What the two screens share is the invariant this module holds: the
//! cursor is drawn at every height a screen claims it can be drawn at.

use ratatui::text::Line;

/// One attempt at a body, laid out from a given data row.
///
/// No `Debug`: it is built and consumed inside [`to_cursor`], and a
/// `Vec<Line>` prints as noise rather than as anything a test would assert
/// on.
pub(super) struct Attempt {
    /// The body, never longer than the budget it was built against.
    pub lines: Vec<Line<'static>>,
    /// Whether the cursor's own row was one of them. `false` means the
    /// chrome ate the budget and the walk must scroll further.
    pub cursor_drawn: bool,
}

/// Lays the body out from the first offset at or below `start` whose
/// attempt draws the cursor's row, and falls back when none does.
///
/// The walk can run out. A four-line body has no room for a section's own
/// header, blank separator and both markers above a single row, and at
/// `view::MIN_HEIGHT` that is the whole budget. When it runs out it gives up
/// on the layout rather than on the cursor: `fallback` draws the cursor's
/// row without its section chrome. The cursor is therefore drawn at every
/// height the screen claims to support, and the frame at the floor looks
/// unlike every other frame on purpose.
///
/// At most one pass per row, and each pass is bounded by the budget.
pub(super) fn to_cursor<B, F>(
    cursor_row: usize,
    start: usize,
    mut body: B,
    fallback: F,
) -> Vec<Line<'static>>
where
    B: FnMut(usize) -> Attempt,
    F: FnOnce() -> Vec<Line<'static>>,
{
    let mut offset = start.min(cursor_row);
    loop {
        let attempt = body(offset);
        if attempt.cursor_drawn {
            return attempt.lines;
        }
        if offset >= cursor_row {
            return fallback();
        }
        offset += 1;
    }
}
