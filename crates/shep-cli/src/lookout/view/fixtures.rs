//! Fixtures the pane test modules share. See the phase plan for why they live
//! in one file rather than three.

use std::ffi::OsStr;
use std::time::Instant;

use ratatui::text::Line;
use shep_client::RequestError;
use shep_core::protocol::{Lamb, ProcessInfo, Response};
use shep_core::status::ProcStatus;

use super::super::app::{App, Control, KeyPress, LambWalk, Msg, Sent};
use super::super::source::HostSample;
use super::super::tail::{Stream, Tail, TailLine};
use super::super::theme::Palette;

/// No colour at all: the palette every fixture uses unless the test is about
/// colour.
pub fn plain() -> Palette {
    Palette::detect(None, None, None)
}

/// The 256-colour palette, for the two tests that assert on a specific
/// foreground.
pub fn coloured() -> Palette {
    Palette::detect(None, Some(OsStr::new("xterm-256color")), None)
}

/// A dashboard with `flock` listed and nothing else applied.
pub fn app_with(flock: Vec<ProcessInfo>, palette: Palette) -> App {
    let t0 = Instant::now();
    let mut app = App::new(
        palette,
        Control::ReadOnly,
        "/home/rin/.shep".to_string(),
        t0,
    );
    app.update(Msg::Snapshot {
        rows: flock,
        at: t0,
    });
    app
}

/// `count` online sheep, the first `with_readings` of which report cpu and
/// memory. The rest report neither, which is the case the `-` assertions need.
pub fn flock_of(count: u32, with_readings: u32) -> Vec<ProcessInfo> {
    (0..count)
        .map(|id| {
            let reports = id < with_readings;
            ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online)
                .pid(Some(48_000 + id))
                .uptime_ms(4_512_000)
                .cpu_percent(reports.then_some(3.5))
                .memory_bytes(reports.then_some(182 << 20))
                .out_file(Some(format!("/home/rin/.shep/logs/sheep-{id}-out.log")))
                .err_file(Some(format!("/home/rin/.shep/logs/sheep-{id}-err.log")))
                .build()
        })
        .collect()
}

/// One plausible host reading: the same numbers the gallery's scenes use, so a
/// failure here and a frame Rin is looking at name the same figures.
pub fn sample() -> HostSample {
    HostSample {
        load: (2.31, 4.10, 3.88),
        cores: Some(10),
        memory_total_bytes: 32 << 30,
        memory_used_bytes: 12 * (1 << 30) + (410 << 20),
        uptime_seconds: 6 * 86_400 + 3 * 3_600,
    }
}

/// A dashboard that has had one host sample applied.
pub fn with_host(sample: HostSample, flock: Vec<ProcessInfo>) -> App {
    let mut app = app_with(flock, plain());
    app.update(Msg::Host {
        sample: Some(sample),
    });
    app
}

/// A dashboard with no host reading — **and the two ways there are of having
/// none are not the same state.**
///
/// `unsupported: true` applies `Msg::Host { sample: None }`, which is what a
/// platform `sysinfo` does not support produces; the reducer sets
/// `host_unsupported` from it and the strip says so.
/// `unsupported: false` applies **no `Msg::Host` at all**, which is the state
/// before the first heartbeat, and the strip says `not read yet` instead.
/// Passing a `None` sample for the second would produce the first, and the
/// test asserting the two sentences differ would pass by rendering one of them
/// twice.
pub fn with_host_none(flock: Vec<ProcessInfo>, unsupported: bool) -> App {
    let mut app = app_with(flock, plain());
    if unsupported {
        app.update(Msg::Host { sample: None });
    }
    app
}

/// A dashboard with a flock of three sheep, the first one selected, and
/// `tail` applied as this refresh's feed.
pub fn with_feed(tail: Tail) -> App {
    let mut app = app_with(flock_of(3, 0), plain());
    app.update(Msg::Bleats { tail });
    app
}

/// Like [`with_feed`], but selects sheep `id` first — for the tests that
/// need the header to name a specific sheep.
pub fn with_feed_and_selection(tail: Tail, id: u32) -> App {
    let mut app = app_with(flock_of(3, 0), plain());
    for _ in 0..id {
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    app.update(Msg::Bleats { tail });
    app
}

/// Like [`with_feed`], but with an explicit palette — for the one test that
/// asserts on a specific foreground colour.
pub fn with_feed_and_palette(tail: Tail, palette: Palette) -> App {
    let mut app = app_with(flock_of(3, 0), palette);
    app.update(Msg::Bleats { tail });
    app
}

/// A dashboard with an empty flock: nothing is selected, so the feed's own
/// "no sheep is selected" line is what renders.
pub fn with_no_selection() -> App {
    app_with(Vec::new(), plain())
}

/// A TWO-sheep dashboard with the selection on `info`, which sorts second.
///
/// Two, and the selection not on row 0, on purpose: Task 7's first mutation
/// replaces `selected_row()` with `rows().first()`, and a one-sheep fixture
/// would make that mutation invisible.
pub fn with_selection(info: ProcessInfo) -> App {
    with_selection_and_palette(info, plain())
}

/// The same, at a given palette.
pub fn with_selection_and_palette(info: ProcessInfo, palette: Palette) -> App {
    assert!(
        info.id > 0,
        "the decoy takes id 0, so the sheep under test cannot"
    );
    let decoy = ProcessInfo::builder(0, "decoy", ProcStatus::Online).build();
    let mut app = app_with(vec![decoy, info], palette);
    app.update(Msg::Key(KeyPress::SelectDown));
    app
}

/// A sheep whose listing DID carry lambs.
///
/// `ListFlock` never populates this field — that is the whole of design
/// decision 4 — so this fixture is deliberately impossible. The pane must not
/// mention lambs even when handed some, because the failure being guarded is a
/// heading or a caption promising a list, not a missing `if let`.
pub fn sheep_with_lambs() -> ProcessInfo {
    ProcessInfo::builder(9, "gateway", ProcStatus::Online)
        .pid(Some(48_301))
        .lambs(Some(vec![
            Lamb::new(48_302, "node"),
            Lamb::new(48_303, "sh"),
        ]))
        .build()
}

/// [`with_selection`] over [`sheep_with_lambs`] (id 9), with one lamb reading
/// applied for that sheep.
pub fn with_lamb_reading(walk: LambWalk) -> App {
    with_lamb_reading_for(9, walk)
}

/// The same, with the reading pinned to `id` instead, so a test can hand the
/// pane a reading that belongs to a different sheep.
pub fn with_lamb_reading_for(id: u32, walk: LambWalk) -> App {
    let mut app = with_selection(sheep_with_lambs());
    app.update(Msg::Replied {
        sent: Sent::Lambs { id },
        result: reply_for(id, &walk),
    });
    app
}

/// [`with_lamb_reading`] plus the `Instant` the dashboard started at, for the
/// one test that needs to tick the clock forward itself.
pub fn app_with_lamb_reading_at(walk: LambWalk) -> (App, Instant) {
    let t0 = Instant::now();
    let mut app = with_selection(sheep_with_lambs());
    app.update(Msg::Tick { now: t0 });
    app.update(Msg::Replied {
        sent: Sent::Lambs { id: 9 },
        result: reply_for(9, &walk),
    });
    (app, t0)
}

/// The reply that makes the reducer record `walk`. There is no way to set a
/// `LambWalk` directly and there should not be: a fixture that reached past
/// `on_lambs` would stop testing the mapping this pane depends on.
///
/// `Failed` is produced by an `Err` rather than by an unrecognised `Ok`,
/// because the two are the same state and `Err` is the one an operator
/// actually meets.
fn reply_for(id: u32, walk: &LambWalk) -> Result<Response, RequestError> {
    let lambs = match walk {
        LambWalk::Failed => return Err(RequestError::Closed),
        LambWalk::NotWalked => None,
        LambWalk::Walked(lambs) => Some(lambs.clone()),
    };
    Ok(Response::Described(vec![
        ProcessInfo::builder(id, "gateway", ProcStatus::Online)
            .pid(Some(48_301))
            .lambs(lambs)
            .build(),
    ]))
}

/// The pane's lamb line alone, for the tests that compare two renderings of
/// it. Panics if the pane has none, so a regression that dropped the line
/// entirely cannot pass by comparing two absences.
pub fn lamb_line_of(app: &App) -> String {
    render_all(&super::detail::detail_lines(app, 200))
        .lines()
        .find(|line| line.starts_with("lambs  "))
        .map(str::to_string)
        .expect("the pane has a lamb line")
}

/// A dashboard with twelve sheep and a full bleats feed — Task 8's own
/// fixture, for the checks that need every pane to have more than it can
/// show: the flock table needs rows to prove it kept the middle of the
/// screen, and the feed needs enough lines to fill its reserved rows so a
/// short terminal's layout sweep can tell a genuine hole in the arithmetic
/// apart from a pane that is up but has nothing to say.
pub fn full_app() -> App {
    let mut app = app_with(flock_of(12, 12), plain());
    app.update(Msg::Bleats {
        tail: Tail {
            lines: (0..10)
                .map(|n| line(Stream::Out, &format!("line-{n}")))
                .collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 1_024,
            note: None,
        },
    });
    app
}

/// One tail line, tagged with the stream it came from.
pub fn line(stream: Stream, text: &str) -> TailLine {
    TailLine {
        stream,
        text: text.to_string(),
    }
}

/// One rendered line, styles discarded.
pub fn rendered(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Several rendered lines, newline-joined. Newline-joined and not
/// concatenated, so an assertion can anchor on a line boundary.
pub fn render_all(lines: &[Line<'static>]) -> String {
    lines.iter().map(rendered).collect::<Vec<_>>().join("\n")
}

/// Four sheep at ids 1..=4 named `web`, `api`, `web-worker`, `cron`, with
/// `query` typed into the filter box and applied. An empty `query` leaves the
/// dashboard unfiltered, which is what the "nothing changed" assertions need.
///
/// Two of the four contain `web`, with `api` between them, so a fixture that
/// stepped over hidden rows would show up as a wrong count rather than as a
/// passing test.
pub fn filtered_app(query: &str) -> App {
    filtered_app_of(named_flock(), query)
}

/// [`filtered_app`] over an explicit flock, for the empty-flock mirror.
pub fn filtered_app_of(flock: Vec<ProcessInfo>, query: &str) -> App {
    let mut app = app_with(flock, plain());
    if !query.is_empty() {
        app.update(Msg::Key(KeyPress::FilterStart));
        for typed in query.chars() {
            app.update(Msg::Key(KeyPress::FilterChar(typed)));
        }
        app.update(Msg::Key(KeyPress::FilterApply));
    }
    app
}

/// The same four sheep with `query` half-typed and the box still OPEN: no
/// `FilterApply`, which is the whole difference between this and
/// [`filtered_app`].
pub fn editing_app(query: &str) -> App {
    let mut app = app_with(named_flock(), plain());
    app.update(Msg::Key(KeyPress::FilterStart));
    for typed in query.chars() {
        app.update(Msg::Key(KeyPress::FilterChar(typed)));
    }
    app
}

/// The four named sheep the filter fixtures share. `flock_of` names its sheep
/// `sheep-0`..`sheep-N`, which every query would match or miss together.
fn named_flock() -> Vec<ProcessInfo> {
    [(1, "web"), (2, "api"), (3, "web-worker"), (4, "cron")]
        .into_iter()
        .map(|(id, name)| {
            ProcessInfo::builder(id, name, ProcStatus::Online)
                .pid(Some(48_000 + id))
                .uptime_ms(4_512_000)
                .build()
        })
        .collect()
}
