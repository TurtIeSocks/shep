//! The bleats feed: the selected sheep's newest output, re-read from its log
//! files on every listing. See the phase plan's design decision 1 for why
//! this reads files rather than subscribing to `log.*`.

use ratatui::text::{Line, Span};

use super::super::app::{App, RowKey};
use super::super::tail::Stream;
use super::flock::fit;
use crate::output::human_bytes;

/// The feed's lines: one header, then the newest lines that fit.
///
/// `rows` is how many lines the pane has, excluding its rule. The header
/// takes one of them, always — either the ordinary one naming the sheep and
/// the cadence, or the gap notice, which REPLACES it rather than sitting
/// beside it. Two header rows would cost a fifth of the pane.
#[must_use]
pub fn feed_lines(app: &App, width: u16, rows: usize) -> Vec<Line<'static>> {
    let palette = app.palette();
    let mut out = Vec::with_capacity(rows);
    let feed = app.feed();
    let body = rows.saturating_sub(1);

    // Lines go missing in three places and this is where two of them are
    // added up: the ones the READER discarded (above the forty-line cap,
    // plus the line the window boundary cut) and the ones this PANE holds
    // and has no room for. Both exact. The third — bytes below the window —
    // cannot be counted in lines at all and is reported separately, in
    // bytes. See the phase plan's design decision 1.
    let lost_lines = feed.missed_lines + feed.lines.len().saturating_sub(body);

    let header = match app.selected() {
        None => Line::from(Span::styled(
            fit("bleats  no sheep is selected", width),
            palette.muted(),
        )),
        // A group row is selected, but there is no single log to re-read for
        // it -- the same reason `view::detail`'s own group pane has no lamb
        // line -- so the header says as much rather than repeating "no
        // sheep is selected" about a row that plainly is.
        //
        // And it RETURNS rather than falling through to the body, because
        // `feed` still holds whatever the previously selected sheep wrote.
        // Live that is invisible: moving onto a group raises a refresh and
        // the coalesced read applies an empty `Tail` in the same iteration.
        // Once the link is lost that repair stops -- `Msg::Bleats` returns
        // early while frozen and the cursor still moves -- so a fall-through
        // prints one sheep's lines, unattributed, under a header naming
        // none.
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
                // `out then err`, not `out+err`: `+` reads as one merged
                // stream, and there is no merge — a log line carries no
                // timestamp, so there is no key to interleave two files on.
                // This header is the only place on screen that can say so.
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
    // The LAST lines that fit: a feed that showed the beginning of a burst
    // and hid its end is the opposite of what a dashboard is for. `err`
    // comes after `out` in `Tail::lines`, so a crash on stderr survives a
    // chatty stdout for free.
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
/// **Two quantities, because they are two different facts, and merging them
/// would mean inventing one of them.** `lines` is exact — the reader counted
/// what it discarded and the pane counts what it has no room for. `bytes` is
/// exact as bytes and *unknowable as lines*: reading them is precisely what
/// the 64 KiB window exists to avoid. So the wording for the byte half says
/// what the reader DID — it never read them — rather than putting a line
/// count on them that nothing measured.
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

    /// fails if the pane prints one sheep's log lines underneath a group
    /// header, attributed to nothing.
    ///
    /// Live this never shows: moving onto a group row raises a refresh and
    /// the coalesced read applies an empty `Tail` in the same iteration.
    /// Once the link is lost that repair stops -- `Msg::Bleats` returns
    /// early while frozen -- and the cursor still moves, so the pane would
    /// read "select one to see its log" with the previous sheep's lines
    /// under it.
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

    /// fails if the BYTE half of the gap notice stops reaching the screen.
    /// Task 5 makes the number exact; this is the half that makes it visible,
    /// and without it the feed silently shows five lines of a four-megabyte
    /// burst.
    ///
    /// "was never read", not "is not shown": the pane cannot say how many
    /// lines are in those bytes — reading them is what the window exists to
    /// avoid — so it says what the reader DID rather than inventing a count.
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

    /// fails if the pane claims completeness in the ORDINARY case. **This is
    /// the test the first draft of this plan did not have**, and its absence
    /// was the phase's worst defect: thirty lines fit inside one 64 KiB
    /// window with room to spare, so `missed_bytes` is zero and the byte
    /// notice never fires — while twenty-five of those thirty lines are not
    /// on screen. A feed that lies is worse than no feed, and it would have
    /// lied exactly when the flock was busy, which is when someone is
    /// watching it.
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

    /// fails if the two kinds of loss get merged into one number. They are
    /// different facts: the reader COUNTED the lines it dropped, and it
    /// never looked at the bytes below the window at all. Adding an invented
    /// line count for the second, or dropping the first because the second
    /// is bigger, would both be the pane claiming to know something it does
    /// not.
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

    /// fails if the ordinary header stops naming the sheep, the streams, or
    /// the fact that this is a re-read rather than a live stream. An
    /// operator who reads this pane as `tail -f` will draw wrong conclusions
    /// from a two-second gap in a log, and the pane is the only place that
    /// can say so.
    ///
    /// `out then err`, not `out+err`. `+` reads as one merged stream, and
    /// this is two files rendered end to end with no interleaving at all —
    /// a log line carries the time it was written, but nothing here merges
    /// on it. A
    /// sheep with forty stdout lines and one old stderr line shows the stale
    /// stderr line UNDER the fresh stdout ones, and the header is the only
    /// place on screen that can say why.
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

    /// fails if the pane stops showing the NEWEST lines, **or stops saying
    /// that the older ones went**. A feed that scrolled off the bottom
    /// would show an operator the beginning of a burst and hide its end,
    /// which is the opposite of what a dashboard is for; a feed that showed
    /// the end and said nothing about the beginning would look complete,
    /// which is worse.
    ///
    /// The first draft of this test asserted only the ordering, and so
    /// certified the silence as correct.
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

    /// fails if `err` stops being distinguishable from `out` by TEXT, or
    /// starts being `--bark` red. A sheep writing to stderr is not a sheep
    /// in trouble — most runtimes log there by default — and `--bark` means
    /// errored, refused and destructive and nothing else.
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

    /// fails if an empty feed stops saying why. Task 5 produces the
    /// sentence; this asserts it survives to the screen instead of being
    /// swallowed by a blank pane — the exact caption 12a got wrong, one
    /// layer up.
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

        // No sheep selected at all is a fourth reason, and it is the pane's
        // own to state — Task 5 never runs in that case.
        let empty = with_no_selection();
        let rendered = render_all(&feed_lines(&empty, 120, 6));
        assert!(
            rendered.contains("no sheep is selected"),
            "got {rendered:?}"
        );
    }
}
