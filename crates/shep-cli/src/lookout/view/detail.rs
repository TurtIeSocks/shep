//! The sheep detail pane: three lines about the selected sheep.
//!
//! **Everything here comes from the `ProcessInfo` the flock table's own rows
//! are built from.** No second request, no `Request::Describe`, and therefore
//! no lamb list — `ProcessInfo::lambs` is `None` on a `ListFlock` reply by
//! construction, and its doc says why: the walk costs a second pass over the
//! machine's process table, and a flock listing is the thing an operator
//! leaves running in a loop.
//!
//! What it adds over the row above it: the UNTRUNCATED name (the NAME column
//! ends in `…`, and a truncated name is one an operator types into
//! `shep stop`), both log paths (the first thing anyone wants once the feed
//! shows them a crash), and whichever fields the current width tier has
//! dropped.

use ratatui::text::{Line, Span};
use shep_core::protocol::DogSource;

use super::super::app::App;
use super::flock::fit;
use crate::output::{human_bytes, human_duration};

/// The pane's three content lines. Its rule is [`super::draw`]'s.
#[must_use]
pub fn detail_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let palette = app.palette();
    let Some(row) = app.selected_row() else {
        // Names the CAUSE, not the fact. An operator can see the pane is
        // empty; what they cannot see is whether that is a broken
        // dashboard or a shepherd with nothing registered.
        let why = if app.flock_len() == 0 {
            "no sheep selected: the flock is empty".to_string()
        } else {
            format!("no sheep selected: no name contains \"{}\"", app.filter())
        };
        return vec![
            Line::from(Span::styled(fit(&why, width), palette.muted())),
            Line::from(Span::raw(String::new())),
            Line::from(Span::raw(String::new())),
        ];
    };
    let info = &row.info;

    // Everything except the status word, which is the one coloured cell —
    // exactly the table's rule, for exactly the table's reason.
    let head = format!("sheep {}  {}   ", info.id, info.name);
    let status = info.status.to_string();
    let rest = format!(
        "   pid {}   restarts {}   uptime {}   cpu {}   mem {}   fold {}{}",
        info.pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        info.restarts,
        app.uptime_ms(info.id)
            .map_or_else(|| "-".to_string(), human_duration),
        info.cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        info.memory_bytes
            .map_or_else(|| "-".to_string(), human_bytes),
        info.fold.as_deref().unwrap_or("-"),
        // Last, so it is the first thing a narrow terminal truncates: a dog is
        // a rare row, and every field before it is true of every row.
        match &info.dog {
            None => String::new(),
            Some(DogSource::BuiltIn) => "   dog built-in".to_string(),
            Some(DogSource::Adopted { path }) => format!("   dog adopted {path}"),
            // `DogSource` is `#[non_exhaustive]`: a source a newer shepherd
            // added must not take the pane down, and must not be reported as
            // anything it is not.
            _ => "   dog (unrecognised source)".to_string(),
        }
    );
    let used = head.chars().count() + status.chars().count();

    vec![
        Line::from(vec![
            Span::raw(head),
            Span::styled(status, palette.status(info.status)),
            Span::raw(fit(
                &rest,
                width.saturating_sub(u16::try_from(used).unwrap_or(width)),
            )),
        ]),
        path_line("out", info.out_file.as_deref(), width, palette),
        path_line("err", info.err_file.as_deref(), width, palette),
    ]
}

/// One log-path line, or a sentence saying why there is none.
///
/// `None` means the shepherd predates the field — `ProcessInfo::out_file`'s own
/// doc — which is a fact about the peer, not about this sheep, and the
/// sentence says so rather than leaving a bare `-` that reads like a missing
/// file.
fn path_line(
    label: &str,
    path: Option<&str>,
    width: u16,
    palette: super::super::theme::Palette,
) -> Line<'static> {
    let text = match path {
        Some(path) => format!("{label}  {path}"),
        None => format!("{label}  this shepherd did not report a path"),
    };
    Line::from(Span::styled(fit(&text, width), palette.muted()))
}

#[cfg(test)]
mod tests {
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use super::super::fixtures::{
        coloured, render_all, sheep_with_lambs, with_selection, with_selection_and_palette,
    };
    use super::*;
    use crate::lookout::app::{App, Control};
    use crate::lookout::theme::Palette;

    /// fails if the pane starts claiming a lamb list. `ProcessInfo::lambs` is
    /// `None` on a `ListFlock` reply by construction, and its own doc says
    /// why: the walk costs a second pass over the machine's process table, and
    /// a flock listing is the thing an operator leaves running in a loop. A
    /// dashboard polling `Describe` every two seconds would put that walk on a
    /// timer.
    ///
    /// Asserted on the RENDERED pane rather than on the source, because the
    /// failure this guards is a caption or a heading promising something the
    /// pane cannot show.
    #[test]
    fn the_detail_pane_never_mentions_lambs() {
        let app = with_selection(sheep_with_lambs());
        let rendered = render_all(&detail_lines(&app, 200));
        for forbidden in ["lamb", "LAMB", "children", "tree"] {
            assert!(
                !rendered.contains(forbidden),
                "found {forbidden:?} in {rendered:?}"
            );
        }
    }

    /// fails if the pane stops showing what the ROW above it cannot. Three
    /// things justify four rows of screen: the untruncated name (the NAME
    /// column ends in `…`, and a truncated name is one an operator types into
    /// `shep stop`), and both log paths (the first thing anyone wants after
    /// the feed shows them a crash).
    #[test]
    fn the_pane_adds_the_full_name_and_both_log_paths() {
        let app = with_selection(
            ProcessInfo::builder(7, "payments-reconciliation-worker", ProcStatus::Errored)
                .out_file(Some("/home/rin/.shep/logs/payments-out.log".to_string()))
                .err_file(Some("/home/rin/.shep/logs/payments-err.log".to_string()))
                .build(),
        );
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(
            rendered.contains("payments-reconciliation-worker"),
            "the whole name"
        );
        assert!(rendered.contains("out  /home/rin/.shep/logs/payments-out.log"));
        assert!(rendered.contains("err  /home/rin/.shep/logs/payments-err.log"));
    }

    /// fails if the STATUS word stops carrying its own colour, or if anything
    /// else on the pane starts carrying one. Same rule as the table's: the
    /// coloured cell is the cell whose text already says the same thing.
    #[test]
    fn only_the_status_word_is_coloured() {
        let palette = coloured();
        let app = with_selection_and_palette(
            ProcessInfo::builder(2, "api", ProcStatus::Errored).build(),
            palette,
        );
        let lines = detail_lines(&app, 200);
        let coloured: Vec<&str> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style.fg == palette.alarm().fg)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(coloured, vec!["errored"], "got {coloured:?}");
    }

    /// fails if an unselectable pane stops saying WHY it is empty. "no sheep
    /// selected" alone restates what the operator can already see; the cause
    /// is that the flock is empty, and that is what the sentence has to carry.
    /// 12a shipped a caption claiming a sentence said why when it only stated
    /// the fact — this is the same mistake, refused one layer down.
    #[test]
    fn an_empty_flock_says_why_the_pane_has_nothing_to_describe() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            std::time::Instant::now(),
        );
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(
            rendered.contains("no sheep selected: the flock is empty"),
            "got {rendered:?}"
        );
    }
}
