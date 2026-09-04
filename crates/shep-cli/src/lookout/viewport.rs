//! A cursor that knows what is on screen.
//!
//! lookout's screens used to hold a bare `cursor: usize` and draw the first
//! `height` lines, which works while every screen fits a terminal. A config
//! pane does not: a sheep has 39 fields under four headers, and a 30-line
//! terminal shows a quarter of them. This is the offset and the
//! scroll-into-view that a bare index never had.
//!
//! A viewport that does not know its height (`rows == 0`) never scrolls,
//! so a screen built in a test with no terminal behaves exactly as it did
//! before this existed. That is deliberate: it is what lets the settings
//! screen's seven snapshots stay byte-identical without every fixture
//! learning a height.

/// A cursor, an offset, and the number of rows the terminal shows.
///
/// `Debug` is derived (IR-41): three integers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewport {
    cursor: usize,
    offset: usize,
    rows: usize,
}

impl Viewport {
    /// At the top, height unknown.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The selected row's index into the list.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The first row that is drawn.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// How many rows the terminal shows, or zero if nobody has said.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Records the terminal's height, and pulls the cursor back into view
    /// if the terminal shrank under it.
    pub fn set_rows(&mut self, rows: usize, len: usize) {
        self.rows = rows;
        self.ensure_visible(len);
    }

    /// Moves by `delta`, clamped to `0..len` rather than wrapping, the same
    /// rule the flock table follows.
    pub fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.cursor = 0;
            self.offset = 0;
            return;
        }
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, len as isize - 1) as usize;
        self.ensure_visible(len);
    }

    /// Jumps to `index`, clamped.
    pub fn move_to(&mut self, index: usize, len: usize) {
        if len == 0 {
            self.cursor = 0;
            self.offset = 0;
            return;
        }
        self.cursor = index.min(len - 1);
        self.ensure_visible(len);
    }

    /// Clamps to a list that may have shrunk since the last move.
    pub fn clamp(&mut self, len: usize) {
        self.move_by(0, len);
    }

    /// Rows above the first drawn one.
    #[must_use]
    pub fn hidden_above(&self) -> usize {
        self.offset
    }

    /// Rows below the last drawn one. Zero when the height is unknown.
    #[must_use]
    pub fn hidden_below(&self, len: usize) -> usize {
        if self.rows == 0 {
            return 0;
        }
        len.saturating_sub(self.offset + self.rows)
    }

    /// Pulls the offset back to whatever keeps the cursor visible, and caps
    /// it to `len - rows` -- every entry point above routes through here
    /// with its own `len`, so a list that shrank between two calls (a
    /// `move_by` after keys were removed from underneath, with no `clamp`
    /// in between) cannot leave a stale offset past the end of the new,
    /// shorter list, the same way `move_by` and `move_to`'s own cursor
    /// clamp already cannot leave a stale cursor.
    fn ensure_visible(&mut self, len: usize) {
        if self.rows == 0 {
            return;
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.rows {
            self.offset = self.cursor + 1 - self.rows;
        }
        self.offset = self.offset.min(len.saturating_sub(self.rows));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewport_that_does_not_know_its_height_never_scrolls() {
        let mut v = Viewport::new();
        v.move_by(50, 100);
        assert_eq!(v.cursor(), 50);
        assert_eq!(v.offset(), 0);
    }

    #[test]
    fn moving_past_the_bottom_pulls_the_offset_so_the_cursor_is_the_last_visible_row() {
        let mut v = Viewport::new();
        v.set_rows(10, 100);
        v.move_by(15, 100);
        assert_eq!(v.cursor(), 15);
        assert_eq!(v.offset(), 6, "rows 6..=15 are visible");
    }

    #[test]
    fn moving_back_above_the_top_pulls_the_offset_to_the_cursor() {
        let mut v = Viewport::new();
        v.set_rows(10, 100);
        v.move_by(30, 100);
        v.move_by(-28, 100);
        assert_eq!(v.cursor(), 2);
        assert_eq!(v.offset(), 2);
    }

    #[test]
    fn the_cursor_clamps_to_the_list_rather_than_wrapping() {
        let mut v = Viewport::new();
        v.set_rows(10, 100);
        v.move_by(-5, 100);
        assert_eq!(v.cursor(), 0);
        v.move_by(500, 100);
        assert_eq!(v.cursor(), 99);
        assert_eq!(v.offset(), 90);
    }

    #[test]
    fn an_empty_list_leaves_the_cursor_and_offset_at_zero() {
        let mut v = Viewport::new();
        v.set_rows(10, 100);
        v.move_by(3, 0);
        assert_eq!((v.cursor(), v.offset()), (0, 0));
    }

    #[test]
    fn hidden_counts_say_how_much_is_off_screen_either_side() {
        let mut v = Viewport::new();
        v.set_rows(10, 100);
        v.move_to(45, 100);
        assert_eq!(v.hidden_above(), v.offset());
        assert_eq!(v.hidden_below(100), 100 - v.offset() - 10);
        assert_eq!(v.hidden_above() + 10 + v.hidden_below(100), 100);
    }

    #[test]
    fn shrinking_the_list_under_the_cursor_clamps_it_back() {
        let mut v = Viewport::new();
        v.set_rows(10, 100);
        v.move_to(45, 100);
        v.clamp(20);
        assert_eq!(v.cursor(), 19);
        assert_eq!(v.offset(), 10);
    }

    #[test]
    fn a_shorter_terminal_brings_the_cursor_back_into_view() {
        let mut v = Viewport::new();
        v.set_rows(30, 100);
        v.move_to(25, 100);
        assert_eq!(v.offset(), 0);
        v.set_rows(10, 100);
        assert_eq!(v.offset(), 16, "the cursor is still the last visible row");
    }

    /// A list that shrinks between two `move_by` calls with no `clamp` in
    /// between -- the shape `content_lines`' env sub-screen exercises once
    /// it can remove a key out from under the cursor. `move_by` must catch
    /// the stale offset on its own; nothing here ever calls `clamp`.
    #[test]
    fn a_move_after_the_list_shrinks_catches_the_stale_offset_without_a_clamp() {
        let mut v = Viewport::new();
        v.set_rows(10, 100);
        v.move_to(90, 100);
        v.move_by(0, 20);
        assert_eq!(v.offset(), 10);
        assert_eq!(v.cursor(), 19);
    }
}
