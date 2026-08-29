//! The sheep detail pane: four lines about the selected sheep.
//!
//! Three of the four come from the `ProcessInfo` the flock table's own rows
//! are built from. The lamb line alone is different: it comes from a
//! `Request::Describe` fetched on selection change and on `r`, never on the
//! two-second poll — `ListFlock` never populates `ProcessInfo::lambs`, and its
//! own doc says why, so this pane asks separately for the one thing the table
//! cannot answer.
//!
//! What it adds over the row above it: the UNTRUNCATED name (the NAME column
//! ends in `…`, and a truncated name is one an operator types into
//! `shep stop`), both log paths (the first thing anyone wants once the feed
//! shows them a crash), the lamb line, and whichever fields the current width
//! tier has dropped.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use shep_core::protocol::DogSource;

use super::super::app::{App, LambWalk, RowKey};
use super::super::theme::Palette;
use super::flock::fit;
use crate::output::{human_bytes, human_duration};

/// The pane's four content lines. Its rule is [`super::draw`]'s.
#[must_use]
pub fn detail_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let palette = app.palette();
    match app.selected() {
        None => empty_lines(app, width, palette),
        Some(RowKey::Group(name)) => group_lines(app, &name, width, palette),
        Some(RowKey::Sheep(_)) => sheep_lines(app, width, palette),
    }
}

/// The pane's four lines when nothing is selected. Names the CAUSE, not the
/// fact: an operator can see the pane is empty; what they cannot see is
/// whether that is a broken dashboard or a shepherd with nothing registered.
fn empty_lines(app: &App, width: u16, palette: Palette) -> Vec<Line<'static>> {
    let why = if app.flock_len() == 0 {
        "no sheep selected: the flock is empty".to_string()
    } else {
        format!("no sheep selected: no name contains \"{}\"", app.filter())
    };
    vec![
        Line::from(Span::styled(fit(&why, width), palette.muted())),
        Line::from(Span::raw(String::new())),
        Line::from(Span::raw(String::new())),
        Line::from(Span::raw(String::new())),
    ]
}

/// An app's four lines when a [`RowKey::Group`] is selected: the rollup
/// [`App::group_totals`] computes, in place of one sheep's own fields. No
/// lamb line and no log paths -- a group has no single process to walk or
/// tail, and reading either for one arbitrarily chosen instance would
/// describe a sheep the operator did not select.
fn group_lines(app: &App, name: &str, width: u16, palette: Palette) -> Vec<Line<'static>> {
    let totals = app.group_totals(name);
    let head = format!("app {name} \u{d7}{}  ", totals.count);
    let status = app.group_status_text(name);
    let rest = format!(
        "   restarts {}   uptime {}   cpu {}   mem {}",
        totals.restarts,
        totals
            .uptime_ms
            .map_or_else(|| "-".to_string(), human_duration),
        totals
            .cpu
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        totals.memory.map_or_else(|| "-".to_string(), human_bytes),
    );
    let used = head.chars().count() + status.chars().count();
    let status_style = app
        .group_uniform_status(name)
        .map_or(Style::default(), |status| palette.status(status));

    vec![
        Line::from(vec![
            Span::raw(head),
            Span::styled(status, status_style),
            Span::raw(fit(
                &rest,
                width.saturating_sub(u16::try_from(used).unwrap_or(width)),
            )),
        ]),
        Line::from(Span::styled(
            fit("lambs  not shown for a group; select one instance", width),
            palette.muted(),
        )),
        Line::from(Span::raw(String::new())),
        Line::from(Span::raw(String::new())),
    ]
}

/// A real sheep's four lines. `app.selected_row()` is `None` here only when
/// the selection has just gone stale between messages; that frame reuses the
/// empty pane's own sentence rather than inventing a fifth state.
fn sheep_lines(app: &App, width: u16, palette: Palette) -> Vec<Line<'static>> {
    let Some(row) = app.selected_row() else {
        return empty_lines(app, width, palette);
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
        lamb_line(app, info.id, width, palette),
        path_line("out", info.out_file.as_deref(), width, palette),
        path_line("err", info.err_file.as_deref(), width, palette),
    ]
}

/// The lamb line: what the last walk found, and how old it is.
///
/// The age comes first. This file's own rule is that the rarest field goes
/// last so a narrow terminal truncates it first, and here that rule inverts:
/// a truncated list is still honest, while a list whose stamp was truncated
/// away is a stale reading presented as current.
///
/// It does not repeat the CLI's "not exactly the set a stop kills" clause.
/// "parent-pid descendants" is already precisely true, and forty characters of
/// warning on every frame trains an operator to stop reading the pane (A16).
fn lamb_line(
    app: &App,
    id: u32,
    width: u16,
    palette: super::super::theme::Palette,
) -> Line<'static> {
    let text = match app.lambs_for(id) {
        None => "lambs  not read yet".to_string(),
        Some((LambWalk::Failed, _)) => {
            "lambs  the shepherd did not answer that request".to_string()
        }
        Some((LambWalk::NotWalked, _)) => {
            "lambs  this sheep is not running, so there is no tree to walk".to_string()
        }
        Some((LambWalk::Walked(lambs), age)) if lambs.is_empty() => {
            format!("lambs  none found, read {} ago", human_duration(age))
        }
        Some((LambWalk::Walked(lambs), age)) => {
            let noun = if lambs.len() == 1 {
                "descendant"
            } else {
                "descendants"
            };
            let list = lambs
                .iter()
                .map(|lamb| format!("{} {}", lamb.pid, lamb.name))
                .collect::<Vec<_>>()
                .join("   ");
            format!(
                "lambs  {} parent-pid {noun}, read {} ago   {list}",
                lambs.len(),
                human_duration(age)
            )
        }
    };
    Line::from(Span::styled(fit(&text, width), palette.muted()))
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
    use std::time::Duration;

    use shep_core::protocol::{Lamb, ProcessInfo};
    use shep_core::status::ProcStatus;

    use super::super::fixtures::{
        app_with_lamb_reading_at, coloured, lamb_line_of, render_all, rendered, sheep_with_lambs,
        with_lamb_reading, with_lamb_reading_for, with_selection, with_selection_and_palette,
    };
    use super::*;
    use crate::lookout::app::{App, Control, LambWalk, Msg};
    use crate::lookout::theme::Palette;

    /// fails if the pane collapses any two of the five states it can be in.
    /// Three of them are distinctions `ProcessInfo::lambs` was built to keep
    /// (walked and non-empty, walked and empty, not walked at all) and the CLI
    /// has wording for only the first, so the other four sentences are this
    /// pane's own.
    #[test]
    fn the_pane_says_which_lamb_state_it_is_in() {
        let cases: [(LambWalk, &str); 3] = [
            (
                LambWalk::Walked(vec![Lamb::new(48_220, "node"), Lamb::new(48_221, "node")]),
                "lambs  2 parent-pid descendants, read ",
            ),
            (LambWalk::Walked(Vec::new()), "lambs  none found, read "),
            (
                LambWalk::NotWalked,
                "lambs  this sheep is not running, so there is no tree to walk",
            ),
        ];
        for (walk, expected) in cases {
            let app = with_lamb_reading(walk);
            let rendered = render_all(&detail_lines(&app, 200));
            assert!(
                rendered.contains(expected),
                "expected {expected:?} in {rendered:?}"
            );
        }

        let failed = with_lamb_reading(LambWalk::Failed);
        assert!(
            render_all(&detail_lines(&failed, 200))
                .contains("lambs  the shepherd did not answer that request")
        );

        let unread = with_selection(sheep_with_lambs());
        assert!(render_all(&detail_lines(&unread, 200)).contains("lambs  not read yet"));
    }

    /// fails if a single lamb reads as "1 parent-pid descendants".
    #[test]
    fn one_lamb_is_a_descendant_and_not_descendants() {
        let app = with_lamb_reading(LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(
            rendered.contains("1 parent-pid descendant, read "),
            "got {rendered:?}"
        );
    }

    /// fails if the staleness stamp moves after the list, or goes away.
    /// `detail.rs`'s standing rule is that the rarest field goes last so a
    /// narrow terminal truncates it first; here that rule inverts, because a
    /// truncated list is still honest and a list whose "read 4m ago" was
    /// truncated away is a stale reading presented as current.
    #[test]
    fn the_lamb_line_carries_its_age_before_its_list() {
        let app = with_lamb_reading(LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        let line = rendered(&detail_lines(&app, 200)[1]);
        let stamp = line.find("read ").expect("a stamp");
        let list = line.find("48220").expect("a list");
        assert!(stamp < list, "the caveat must survive truncation: {line:?}");
    }

    /// fails if the pane starts showing a reading taken for another sheep.
    #[test]
    fn a_reading_for_another_sheep_is_not_drawn_here() {
        // with_lamb_reading pins its reading to the selected sheep's id;
        // this one pins it to a different one and expects the unread sentence.
        let app = with_lamb_reading_for(11, LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        assert!(render_all(&detail_lines(&app, 200)).contains("lambs  not read yet"));
    }

    /// fails if the stamp reads a live clock instead of `App::now`, and fails
    /// again if a frozen dashboard's stamp creeps. Two halves, and BOTH are
    /// needed: the first proves the stamp moves at all, the second proves it
    /// stops when the banner says the values did.
    ///
    /// This is a unit test rather than a two-age frame comparison, and that is
    /// the point. Rendering the frozen scene at two ages cannot fail for this
    /// mutation: both renders happen at the same wall-clock instant, so a live
    /// clock produces the same string in both and the frames stay identical.
    /// Here the two ages differ by construction, because they are `Msg::Tick`
    /// arithmetic rather than elapsed time. No sleep (IR-33).
    #[test]
    fn the_stamp_ages_on_a_live_dashboard_and_stops_on_a_frozen_one() {
        let (mut app, t0) =
            app_with_lamb_reading_at(LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(120),
        });
        let live = lamb_line_of(&app);
        assert!(live.contains("read 2m ago"), "the stamp aged: {live:?}");

        app.update(Msg::Frozen {
            at_local: "2026-08-16 09:00:00".to_string(),
        });
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(3_600),
        });
        assert_eq!(
            lamb_line_of(&app),
            live,
            "a frozen dashboard's reading must not age"
        );
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
                .out_file(Some("/home/ada/.shep/logs/payments-out.log".to_string()))
                .err_file(Some("/home/ada/.shep/logs/payments-err.log".to_string()))
                .build(),
        );
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(
            rendered.contains("payments-reconciliation-worker"),
            "the whole name"
        );
        assert!(rendered.contains("out  /home/ada/.shep/logs/payments-out.log"));
        assert!(rendered.contains("err  /home/ada/.shep/logs/payments-err.log"));
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
            "/home/ada/.shep".to_string(),
            std::time::Instant::now(),
        );
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(
            rendered.contains("no sheep selected: the flock is empty"),
            "got {rendered:?}"
        );
    }
}
