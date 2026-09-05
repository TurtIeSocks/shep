//! The bleats feed: the selected sheep's newest output, re-read from its log
//! files on every listing rather than a live subscription to `log.*`.

use ratatui::text::{Line, Span};

use super::super::app::{App, RowKey};
use super::super::tail::Stream;
use super::flock::fit;
use crate::output::human_bytes;

/// The feed's lines: one header, then the newest lines that fit.
///
/// `rows` excludes the pane's rule. The header takes one line always: the
/// gap notice replaces the ordinary header rather than sitting beside it.
#[must_use]
pub fn feed_lines(app: &App, width: u16, rows: usize) -> Vec<Line<'static>> {
    let palette = app.palette();
    let mut out = Vec::with_capacity(rows);
    let feed = app.feed();
    let body = rows.saturating_sub(1);

    // Two of three loss sources summed here: lines the reader discarded
    // above the cap, and lines this pane has no room to show. Bytes below
    // the window are reported separately, since they cannot be counted in
    // lines.
    let lost_lines = feed.missed_lines + feed.lines.len().saturating_sub(body);

    let header = match app.selected() {
        None => Line::from(Span::styled(
            fit("bleats  no sheep is selected", width),
            palette.muted(),
        )),
        // A group has no single log to re-read, so the header replaces "no
        // sheep is selected" rather than falling through to stale lines
        // from the previously selected sheep. Only visible while frozen,
        // since selecting a group otherwise triggers a refresh.
        Some(RowKey::Group(name)) => {
            out.push(Line::from(Span::styled(
                fit(
                    &format!("bleats  {name}  follows one instance; select one to see its log"),
                    width,
                ),
                palette.muted(),
            )));
            return out;
        }
        Some(RowKey::Sheep(_)) => {
            let row = app
                .selected_row()
                .expect("a selected sheep is in the flock");
            match gap_notice(lost_lines, feed.missed_bytes) {
                Some(notice) => Line::from(Span::styled(
                    fit(&format!("bleats  {}  {notice}", row.info.name), width),
                    // Attention, not alarm: a sheep writing faster than a
                    // two-second poll is busy, not broken. `--bark` means
                    // errored, refused and destructive.
                    palette.attention(),
                )),
                // `out then err`, not `out+err`: a log line carries no
                // timestamp, so there is no key to interleave the two files
                // on.
                None => Line::from(Span::styled(
                    fit(
                        &format!(
                            "bleats  {}  out then err  from the log files, re-read with each listing",
                            row.info.name
                        ),
                        width,
                    ),
                    palette.muted(),
                )),
            }
        }
    };
    out.push(header);

    if feed.lines.is_empty() {
        if let Some(note) = feed.note.as_deref() {
            out.push(Line::from(Span::styled(fit(note, width), palette.muted())));
        }
        return out;
    }
    // The last lines that fit. `err` comes after `out` in `Tail::lines`, so
    // a crash on stderr survives a chatty stdout.
    let skip = feed.lines.len().saturating_sub(body);
    for line in feed.lines.iter().skip(skip) {
        let tag = match line.stream {
            Stream::Out => "out",
            Stream::Err => "err",
        };
        out.push(Line::from(vec![
            // Muted, both of them. The word carries the whole meaning, and
            // a red `err` would say a stderr line is damage.
            Span::styled(format!("{tag}  "), palette.muted()),
            Span::raw(fit(&line.text, width.saturating_sub(5))),
        ]));
    }
    out
}

/// What the header says about what is not on screen, or `None` when
/// everything is.
///
/// Two separate quantities, not one merged number: `lines` is exact, counted
/// by the reader and the pane. `bytes` is exact as bytes but unknowable as
/// lines, since reading them is what the window exists to avoid.
fn gap_notice(lines: usize, bytes: u64) -> Option<String> {
    match (lines, bytes) {
        (0, 0) => None,
        (0, bytes) => Some(format!(
            "… {} written before these lines was never read",
            human_bytes(bytes)
        )),
        (lines, 0) => Some(format!("… {}", earlier_lines(lines))),
        (lines, bytes) => Some(format!(
            "… {}, and {} before them never read",
            earlier_lines(lines),
            human_bytes(bytes)
        )),
    }
}

/// `1 earlier line` / `25 earlier lines`. A sentence with the wrong plural on
/// it reads as a rendering bug, and this one is on screen during an
/// incident.
fn earlier_lines(count: usize) -> String {
    if count == 1 {
        "1 earlier line not shown".to_string()
    } else {
        format!("{count} earlier lines not shown")
    }
}

#[cfg(test)]
mod tests {
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use super::super::super::app::{KeyPress, Msg, RowKey};
    use super::super::super::tail::{Stream, Tail};
    use super::super::fixtures::{
        app_with, coloured, line, plain, render_all, with_feed, with_feed_and_palette,
        with_feed_and_selection, with_no_selection,
    };
    use super::feed_lines;

    /// Needs `Msg::Frozen`: without it, selecting a group triggers a
    /// refresh that clears the stale lines, so this stays invisible live.
    #[test]
    fn a_group_row_prints_no_body_lines_on_a_frozen_dashboard() {
        let flock: Vec<ProcessInfo> = (0..2)
            .map(|slot| {
                ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                    .instance(Some(slot))
                    .build()
            })
            .collect();
        let mut app = app_with(flock, plain());
        // Row 0 is the group header, so one `j` lands on `web`'s first slot,
        // which is the sheep whose lines the feed is about to hold.
        app.update(Msg::Key(KeyPress::SelectDown));
        app.update(Msg::Bleats {
            tail: Tail {
                lines: vec![line(Stream::Out, "slot-0 wrote this")],
                missed_lines: 0,
                missed_bytes: 0,
                read_bytes: 18,
                note: None,
            },
        });
        app.update(Msg::Frozen {
            at_local: "12:00:00".to_string(),
        });
        app.update(Msg::Key(KeyPress::SelectUp));
        assert_eq!(
            app.selected(),
            Some(RowKey::Group("web".to_string())),
            "the cursor has to be parked on the group row for this to test anything"
        );

        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(
            rendered.contains("select one to see its log"),
            "got {rendered:?}"
        );
        assert!(
            !rendered.contains("slot-0 wrote this"),
            "a group header with one instance's lines under it: {rendered:?}"
        );
    }

    /// Without this, the feed would silently show five lines of a
    /// four-megabyte burst with no notice.
    #[test]
    fn a_byte_gap_replaces_the_header_and_says_how_much_was_never_read() {
        let app = with_feed(Tail {
            lines: vec![line(Stream::Out, "still here")],
            missed_lines: 0,
            missed_bytes: 4_000_000,
            read_bytes: 65_536,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(
            rendered.contains("3.8M written before these lines was never read"),
            "got {rendered:?}"
        );
        // And the ordinary header is gone while the gap notice is up: two
        // header lines would cost one of the five content rows.
        assert!(!rendered.contains("re-read with each listing"));
    }

    /// Thirty lines fit inside one window, so `missed_bytes` stays zero, but
    /// only five fit on screen: the gap notice must still fire.
    #[test]
    fn a_pane_that_cannot_show_every_line_it_holds_says_how_many() {
        let app = with_feed(Tail {
            lines: (0..30)
                .map(|n| line(Stream::Out, &format!("line-{n}")))
                .collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 4_096,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(
            rendered.contains("… 25 earlier lines not shown"),
            "got {rendered:?}"
        );
        assert!(
            !rendered.contains("never read"),
            "no bytes were skipped, so nothing may claim any were: {rendered:?}"
        );
    }

    #[test]
    fn both_kinds_of_gap_are_named_separately_in_one_line() {
        let app = with_feed(Tail {
            lines: (0..30)
                .map(|n| line(Stream::Out, &format!("line-{n}")))
                .collect(),
            missed_lines: 500,
            missed_bytes: 4_000_000,
            read_bytes: 131_072,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 200, 6));
        assert!(
            rendered.contains("… 525 earlier lines not shown, and 3.8M before them never read"),
            "500 the reader dropped plus 25 the pane has no room for: {rendered:?}"
        );
    }

    /// An operator reading this pane as `tail -f` would draw wrong
    /// conclusions from the poll gap without this header.
    #[test]
    fn the_header_says_which_sheep_and_that_it_is_a_re_read() {
        let app = with_feed_and_selection(
            Tail {
                lines: vec![line(Stream::Out, "hello")],
                missed_lines: 0,
                missed_bytes: 0,
                read_bytes: 6,
                note: None,
            },
            1,
        );
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(rendered.contains("bleats  sheep-1"), "got {rendered:?}");
        assert!(rendered.contains("out then err"), "got {rendered:?}");
        assert!(
            !rendered.contains("out+err"),
            "`+` reads as a merge: {rendered:?}"
        );
        assert!(
            rendered.contains("re-read with each listing"),
            "got {rendered:?}"
        );
    }

    /// Showing the end while saying nothing about the gap would look
    /// complete, which is worse than showing neither.
    #[test]
    fn the_pane_shows_the_last_lines_that_fit_and_says_so() {
        let app = with_feed(Tail {
            lines: (0..40)
                .map(|n| line(Stream::Out, &format!("line-{n}")))
                .collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 4_096,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        for n in 35..40 {
            assert!(
                rendered.contains(&format!("out  line-{n}")),
                "line-{n} is on screen"
            );
        }
        assert!(
            !rendered.contains("out  line-34"),
            "and line-34 is not: {rendered:?}"
        );
        assert!(
            rendered.contains("… 35 earlier lines not shown"),
            "and the pane says the other thirty-five went: {rendered:?}"
        );
    }

    /// Most runtimes log to stderr by default, so it is not `--bark`.
    #[test]
    fn the_stream_tag_is_a_word_and_stderr_is_not_bark() {
        let palette = coloured();
        let app = with_feed_and_palette(
            Tail {
                lines: vec![line(Stream::Out, "fine"), line(Stream::Err, "warning")],
                missed_lines: 0,
                missed_bytes: 0,
                read_bytes: 32,
                note: None,
            },
            palette,
        );
        let lines = feed_lines(&app, 120, 6);
        let rendered = render_all(&lines);
        assert!(rendered.contains("out  fine"));
        assert!(rendered.contains("err  warning"));
        let bark = palette.alarm().fg;
        for line in &lines {
            for span in &line.spans {
                assert_ne!(
                    span.style.fg, bark,
                    "nothing in this pane is bark: {span:?}"
                );
            }
        }
    }

    /// Confirms the note reaches the screen rather than a blank pane.
    #[test]
    fn an_empty_feed_prints_the_reason_rather_than_nothing() {
        let app = with_feed(Tail {
            lines: Vec::new(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 0,
            note: Some("this sheep has written nothing yet".to_string()),
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(rendered.contains("this sheep has written nothing yet"));

        // No sheep selected is a fourth reason, stated by the pane itself.
        let empty = with_no_selection();
        let rendered = render_all(&feed_lines(&empty, 120, 6));
        assert!(
            rendered.contains("no sheep is selected"),
            "got {rendered:?}"
        );
    }
}
