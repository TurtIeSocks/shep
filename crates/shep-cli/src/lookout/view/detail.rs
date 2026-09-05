//! The sheep detail pane: four lines about the selected sheep.
//!
//! Three of the four come from the same `ProcessInfo` the flock table's rows
//! are built from. The lamb line is different: it comes from a
//! `Request::Describe` fetched on selection change and on `r`, never on the
//! poll, since `ListFlock` never populates `ProcessInfo::lambs`.
//!
//! Adds over the row above it: the untruncated name, both log paths, the
//! lamb line, and whichever fields the current width tier dropped.

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

/// The pane's four lines when nothing is selected. Names the cause, not the
/// fact: whether the flock is empty or the filter matched nothing.
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
/// lamb line and no log paths, since a group has no single process to walk
/// or tail.
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
    // `palette.status`, not `palette.reported`: a selected group is always
    // an app's own instances, never a dog.
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

    // Everything except the status word, which is the one coloured cell,
    // same as the table.
    let head = format!("sheep {}  {}   ", info.id, info.name);
    // `Row::reported`, not `info.status.to_string()`: this pane must agree
    // with the flock table's own STATUS cell for the same row, and a dog
    // that has never handshook reads `silent` there.
    let status = row.reported().word();
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
            Span::styled(status, palette.reported(row.reported())),
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
/// The age comes first: a truncated list is still honest, but a list whose
/// stamp truncated away is a stale reading presented as current.
///
/// Omits the CLI's "not exactly the set a stop kills" caveat, since
/// "parent-pid descendants" is already precise.
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
/// `None` means the shepherd predates the field, a fact about the peer, not
/// about this sheep. The sentence says so rather than leaving a bare `-`
/// that reads like a missing file.
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
        app_with, app_with_lamb_reading_at, coloured, lamb_line_of, plain, render_all, rendered,
        sheep_with_lambs, with_lamb_reading, with_lamb_reading_for, with_selection,
        with_selection_and_palette,
    };
    use super::*;
    use crate::lookout::app::{App, Control, LambWalk, Msg, RowKey};
    use crate::lookout::theme::Palette;

    /// Five states, and the CLI's own wording covers only one of them: the
    /// other four sentences belong to this pane.
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

    #[test]
    fn one_lamb_is_a_descendant_and_not_descendants() {
        let app = with_lamb_reading(LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(
            rendered.contains("1 parent-pid descendant, read "),
            "got {rendered:?}"
        );
    }

    #[test]
    fn the_lamb_line_carries_its_age_before_its_list() {
        let app = with_lamb_reading(LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        let line = rendered(&detail_lines(&app, 200)[1]);
        let stamp = line.find("read ").expect("a stamp");
        let list = line.find("48220").expect("a list");
        assert!(stamp < list, "the caveat must survive truncation: {line:?}");
    }

    #[test]
    fn a_reading_for_another_sheep_is_not_drawn_here() {
        // with_lamb_reading pins its reading to the selected sheep's id;
        // this one pins it to a different one and expects the unread sentence.
        let app = with_lamb_reading_for(11, LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        assert!(render_all(&detail_lines(&app, 200)).contains("lambs  not read yet"));
    }

    /// A two-age frame comparison can't fail here, since both renders share
    /// one wall-clock instant. Ages come from `Msg::Tick` arithmetic
    /// instead.
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

    /// The untruncated name matters: an operator types it into `shep stop`,
    /// and the name column truncates.
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

    /// Same rule as the table's: the coloured cell is the cell whose text
    /// already says the same thing.
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

    /// Drives `detail_lines` through a real `App` built from a real
    /// `Msg::Snapshot`, the same door the production render loop walks
    /// through, rather than calling `group_lines` directly.
    #[test]
    fn a_selected_group_row_shows_the_apps_rollup_and_no_lambs_or_paths() {
        let app = app_with(
            vec![
                ProcessInfo::builder(1, "web", ProcStatus::Online)
                    .instance(Some(0))
                    .memory_bytes(Some(100 << 20))
                    .uptime_ms(120_000)
                    .out_file(Some("/home/ada/.shep/logs/web-0-out.log".to_string()))
                    .build(),
                ProcessInfo::builder(2, "web", ProcStatus::Online)
                    .instance(Some(1))
                    .memory_bytes(Some(150 << 20))
                    .uptime_ms(30_000)
                    .build(),
            ],
            plain(),
        );
        // Sanity: the group is the whole flock here, so the default
        // selection lands on it with no keypress.
        assert!(
            matches!(app.selected(), Some(RowKey::Group(ref name)) if name == "web"),
            "sanity: the group is selected by default, got {:?}",
            app.selected()
        );

        let rendered = render_all(&detail_lines(&app, 200));
        assert!(rendered.contains("app web \u{d7}2"), "got {rendered:?}");
        assert!(
            rendered.contains("uptime 30s"),
            "the MINIMUM uptime (30s), not the first instance's 120s: {rendered:?}"
        );
        assert!(
            rendered.contains("mem 250.0M"),
            "memory summed (100 + 150 MiB): {rendered:?}"
        );
        assert!(
            rendered.contains("lambs  not shown for a group; select one instance"),
            "got {rendered:?}"
        );
        assert!(
            !rendered.contains("web-0-out.log"),
            "no arbitrarily-chosen instance's log path: {rendered:?}"
        );
    }

    /// Drives both panes off the same [`App`] built from the same row, the
    /// way an operator with both open sees them.
    #[test]
    fn a_silent_dogs_status_word_and_colour_agree_with_the_flock_pane() {
        use shep_core::protocol::DogSource;
        use shep_core::status::ProcStatus;

        use super::super::flock::{columns_for, row_line};

        let dog = ProcessInfo::builder(9, "log-rotate", ProcStatus::Online)
            .pid(Some(4_242))
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            }))
            .handshook(Some(false))
            .build();
        let app = with_selection_and_palette(dog, coloured());
        let palette = app.palette();

        let detail_rendered = render_all(&detail_lines(&app, 200));
        assert!(
            detail_rendered.contains("silent"),
            "detail pane must say silent: {detail_rendered:?}"
        );
        assert!(!detail_rendered.contains("online"), "{detail_rendered:?}");

        let row = app.row(9).unwrap();
        let flock_line = row_line(&app, row, columns_for(200), 200, false);
        let flock_rendered: String = flock_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(flock_rendered.contains("silent"), "{flock_rendered:?}");

        // Both panes colour the word `--butter`: a gap the operator can
        // close, not `--bark`.
        let detail_colour = detail_lines(&app, 200)[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "silent")
            .map(|span| span.style.fg);
        assert_eq!(detail_colour, Some(palette.attention().fg));
    }
}
