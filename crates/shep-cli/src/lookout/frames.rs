//! `Buffer` -> text. Task 5 grows this module with `render_ansi`, the scene
//! list and the gallery writer (`docs/lookout/frames.txt` / `.ansi`); this is
//! the one function Task 4's own tests need in the meantime — `view::draw`'s
//! tests render into a `TestBackend` and read the result back as plain text.
//!
//! A function of ours rather than `TestBackend`'s own `Display`: the upstream
//! `Display` impl is a presentation detail that can change between ratatui
//! releases, and Task 5's second renderer (`render_ansi`) needs to carry
//! colour, which `Display` never will.

use ratatui::buffer::Buffer;

/// Every row of `buffer`, joined by `\n`. Cells are read by their rendered
/// symbol, not by byte length, so a multi-byte cell (an ellipsis, a
/// multi-byte name) round-trips exactly as it was drawn.
///
/// Not called outside this module's own tests yet — `view::draw`'s tests are
/// its real caller. See this module's own doc for why.
#[allow(dead_code)]
#[must_use]
pub fn render_text(buffer: &Buffer) -> String {
    let area = buffer.area;
    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|col| buffer[(area.x + col, area.y + row)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
