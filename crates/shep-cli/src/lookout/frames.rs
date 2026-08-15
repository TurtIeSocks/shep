//! `Buffer` -> text, and `Buffer` -> ANSI, plus the scene list both the
//! pinned snapshots and the gallery writer are built from.
//!
//! **Why this exists at all.** `TestBackend` renders a frame into a plain
//! text buffer with no terminal involved, which is what makes a TUI testable
//! headlessly. It is also, for exactly the same reason, what lets a reviewer
//! SEE the dashboard without running it — so this module's output is a
//! deliverable (`docs/lookout/frames.txt`, `docs/lookout/frames.ansi`) and
//! not only test scaffolding. That is the whole point of Phase 12a: Rin
//! decides what Phase 12b's layout looks like from these frames, not from a
//! spec sentence.
//!
//! **Why not `TestBackend`'s own `Display`.** Two reasons, both practical:
//! its exact framing is an upstream presentation detail that can change
//! between ratatui releases, and it carries no colour, while one of the two
//! renderers here has to.
//!
//! **Why one scene list.** The gallery and the snapshot tests both read
//! [`Scene::ALL`], so the gallery cannot silently drift from what the suite
//! checks: a layout change reddens the snapshots in the ordinary run, and
//! regenerating the gallery is one command.
//!
//! **`#[cfg(test)]` at the `mod` declaration in `super::mod`, not a plain
//! `pub mod`.** `shep-cli` is `[[bin]]`-only, so in a normal build the only
//! reachability root is `main` — nothing here is called outside this
//! module's own tests, so a plain `pub mod` fails the task gate on
//! `dead_code`. Gating the whole module means these items simply do not
//! exist in a non-test build, and cost nothing when they run under
//! `cargo test`.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

use shep_core::protocol::ProcessInfo;
use shep_core::status::ProcStatus;

use super::app::{App, Control, KeyPress, Msg};
use super::source::HostSample;
use super::tail::{Stream, Tail, TailLine};
use super::theme::Palette;
use super::view::draw;

/// One rendered buffer as plain text: one line per row, trailing spaces
/// kept, no escapes.
///
/// Trailing spaces are kept on purpose. A frame is a fixed-size grid, and
/// trimming makes a right-aligned cell — the flock count in the title, the
/// control state in the status bar — look as though it moved.
///
/// Cells are read by their rendered symbol, not by byte length, so a
/// multi-byte cell (an ellipsis, a multi-byte name) round-trips exactly as
/// it was drawn. Indexed with `Buffer[(x, y)]` rather than the deprecated
/// `Buffer::get`, per ratatui-core 0.1.2.
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

/// The same buffer with SGR escapes, for reading through `less -R`.
///
/// Every line ends with a reset before its newline. A frame that set a
/// colour and never reset it would bleed into whatever came next — which,
/// in a file, is the rest of the file.
#[must_use]
pub fn render_ansi(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for row in 0..area.height {
        let mut current = String::new();
        for col in 0..area.width {
            let cell = &buffer[(area.x + col, area.y + row)];
            let wanted = sgr(cell.fg);
            if wanted != current {
                out.push_str("\u{1b}[0m");
                out.push_str(&wanted);
                current = wanted;
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\u{1b}[0m");
        out.push('\n');
    }
    out
}

/// The SGR sequence for one cell's foreground.
///
/// Foreground only, because a foreground is the only thing 12a's palette
/// sets — there is no selected row and nothing is bold. A modifier a 12b
/// pane introduces renders unstyled here rather than as a wrong style, and
/// this function grows a case for it then.
fn sgr(fg: Color) -> String {
    let mut out = String::new();
    match fg {
        Color::Reset => {}
        Color::Indexed(index) => {
            let _ = write!(out, "\u{1b}[38;5;{index}m");
        }
        Color::Red => out.push_str("\u{1b}[31m"),
        Color::Green => out.push_str("\u{1b}[32m"),
        Color::Yellow => out.push_str("\u{1b}[33m"),
        Color::DarkGray => out.push_str("\u{1b}[90m"),
        _ => {}
    }
    out
}

/// The scenes the frame snapshots pin and the gallery renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// A healthy flock at a comfortable width, all three panes up.
    HealthyWide,
    /// One sheep errored, one waiting to restart, one stopped.
    Errored,
    /// Nothing registered.
    Empty,
    /// A narrow terminal: four columns dropped.
    Narrow,
    /// Below the floor.
    TooNarrow,
    /// Mid-reconnect.
    Retrying,
    /// The shepherd is gone and the values are frozen.
    Frozen,
    /// The read-only refusal.
    Refused,
    /// 20 rows: the 18-tier. The detail pane is gone; the strip and the feed
    /// are not.
    NoDetail,
    /// 12 rows: below every optional-pane threshold. 12a's frame.
    TableOnly,
    /// The feed under a burst: lines dropped and bytes never read.
    FeedGap,
    /// The selected sheep has never written a log in this `$SHEP_HOME`.
    FeedMissing,
    /// 33x26: the narrowest terminal that still draws all three panes.
    Cramped,
    /// `sysinfo` reports this platform unsupported.
    HostUnknown,
}

impl Scene {
    /// Every scene, in the order they appear in the gallery.
    pub const ALL: &'static [Self] = &[
        Self::HealthyWide,
        Self::Errored,
        Self::Empty,
        Self::Narrow,
        Self::TooNarrow,
        Self::Retrying,
        Self::Frozen,
        Self::Refused,
        Self::NoDetail,
        Self::TableOnly,
        Self::FeedGap,
        Self::FeedMissing,
        Self::Cramped,
        Self::HostUnknown,
    ];

    /// The snapshot name and the gallery heading.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::HealthyWide => "healthy_wide",
            Self::Errored => "errored",
            Self::Empty => "empty",
            Self::Narrow => "narrow",
            Self::TooNarrow => "too_narrow",
            Self::Retrying => "retrying",
            Self::Frozen => "frozen",
            Self::Refused => "refused",
            Self::NoDetail => "no_detail",
            Self::TableOnly => "table_only",
            Self::FeedGap => "feed_gap",
            Self::FeedMissing => "feed_missing",
            Self::Cramped => "cramped",
            Self::HostUnknown => "host_unknown",
        }
    }

    /// One sentence saying what this frame is for, printed above it in the
    /// gallery so Rin does not have to hold fourteen of them in her head.
    ///
    /// Every clause here is pinned by an assertion in
    /// `every_scene_shows_the_thing_it_is_named_for` — a caption may not say
    /// a thing the frame is not asserted to show.
    #[must_use]
    pub const fn caption(self) -> &'static str {
        match self {
            Self::HealthyWide => {
                "All three panes at 120x30: the host strip under the title, the detail pane and the bleats feed under the table. `>` marks the selected sheep, and every pane below the table describes it."
            }
            Self::Errored => {
                "One errored, one waiting to restart, one stopped, with the selection parked on the errored sheep. Each row's own STATUS cell is the only coloured cell in that row."
            }
            Self::Empty => {
                "No sheep registered. Each of the three panes says why it is empty, and the three sentences are different because the three reasons are."
            }
            Self::Narrow => {
                "51 columns: FOLD, RESTARTS, PID and MEM are gone, in that order. CPU and UPTIME survive because they explain WHY. The host strip fits; the detail pane and the feed do not, at 14 rows."
            }
            Self::TooNarrow => {
                "28 columns: below the floor, the pane refuses rather than drawing overlapping garbage. Two short lines, so the refusal still fits the terminal it is refusing about."
            }
            Self::Retrying => {
                "The shepherd stopped answering. Five attempts over about eight seconds before this becomes the next frame. Every pane below the table keeps describing the selected sheep from the last listing."
            }
            Self::Frozen => {
                "The ladder ran out. Last known values stay, the uptime clock has stopped, and so has the host strip — one line ticking over on a frozen screen is a contradiction on the same frame."
            }
            Self::Refused => {
                "`x` with actions gated off. Both refusals are literal — nothing about damage gets charming — and the panes below carry on."
            }
            Self::NoDetail => {
                "20 rows: the detail pane is the first to go, because every number on it but the log paths is already in the row above it."
            }
            Self::TableOnly => {
                "12 rows: no optional panes at all. This is 12a's frame, and the only thing that changed is the two-column gutter the marker sits in."
            }
            Self::FeedGap => {
                "The feed under a burst: four megabytes were never read and some hundreds of lines were read and dropped. The pane counts both, and counts them separately, because it knows the second exactly and cannot know how many lines are in the first."
            }
            Self::FeedMissing => {
                "The selected sheep has never written a log in this $SHEP_HOME. The feed names that cause rather than sitting blank."
            }
            Self::Cramped => {
                "33 columns, 26 rows: the narrowest terminal that draws, with all three panes up. Everything truncates with an ellipsis; nothing overlaps."
            }
            Self::HostUnknown => {
                "`sysinfo` reports this platform unsupported. The strip says so and keeps the flock's own totals, which lookout can always compute."
            }
        }
    }

    /// The terminal size this scene is rendered at.
    #[must_use]
    pub const fn size(self) -> (u16, u16) {
        match self {
            Self::Empty => (100, 28),
            // 51, not 46 and not 49. `columns_for` runs on `width - GUTTER`
            // (Task 2: the selection marker's two-column gutter, phase plan
            // design decision 6), so the table only sees `width - 2` — a
            // scene asked for the raw `NO_MEM` threshold (49) would land two
            // columns short of it and drop into the `41` tier, which has
            // already dropped CPU, contradicting this scene's own caption.
            // 51 - GUTTER == 49, the `NO_MEM` tier: four columns gone, CPU
            // and UPTIME still there.
            Self::Narrow => (51, 14),
            Self::TooNarrow => (28, 8),
            Self::NoDetail => (120, 20),
            Self::TableOnly => (120, 12),
            Self::Cramped => (33, 26),
            // HealthyWide, Errored, Retrying, Frozen, Refused, FeedGap,
            // FeedMissing, HostUnknown: every scene that carries all three
            // optional panes at their ordinary rows.
            _ => (120, 30),
        }
    }
}

/// Builds one scene and returns its label with the buffer it drew into.
///
/// Ten minutes of dashboard age: long enough that a frozen frame whose
/// clock had kept running would be obvious in the gallery at a glance, and
/// it is what both the pinned snapshots and `docs/lookout/frames.txt`
/// render at.
#[must_use]
pub fn scene(which: Scene) -> (&'static str, Buffer) {
    (which.label(), scene_with(which, Duration::from_secs(600)))
}

/// One scene, `age` after its opening snapshot.
///
/// The parameter exists for one test:
/// `the_frozen_frame_does_not_move_however_long_the_link_stays_gone` renders
/// the frozen scene at two ages and asserts the two frames are identical,
/// which is the only shape in which "the clock stopped" is a falsifiable
/// claim about a whole frame.
///
/// Deterministic by construction: the palette is forced to the 256-colour
/// set regardless of this machine's `TERM`, the clock is an explicit
/// `Instant` advanced by exact `Duration`s, and the frozen timestamp is a
/// literal. A scene that read the environment or the wall clock would
/// produce a different gallery on every machine and a snapshot that could
/// never be pinned.
#[must_use]
fn scene_with(which: Scene, age: Duration) -> Buffer {
    use std::ffi::OsStr;

    let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
    let t0 = Instant::now();
    let mut app = App::new(
        palette,
        Control::ReadOnly,
        "/home/rin/.shep".to_string(),
        t0,
    );

    let flock = match which {
        Scene::Empty => Vec::new(),
        Scene::Errored | Scene::Frozen => vec![
            sheep(
                0,
                "web",
                ProcStatus::Online,
                Some(48_211),
                0,
                Some(3.4),
                Some(182 << 20),
                Some("edge"),
            ),
            sheep(
                1,
                "web",
                ProcStatus::Online,
                Some(48_212),
                0,
                Some(2.9),
                Some(178 << 20),
                Some("edge"),
            ),
            sheep(
                2,
                "api",
                ProcStatus::Errored,
                None,
                14,
                None,
                None,
                Some("edge"),
            ),
            sheep(
                3,
                "billing-reconciliation-worker",
                ProcStatus::WaitingRestart,
                None,
                3,
                None,
                None,
                None,
            ),
            sheep(4, "cron", ProcStatus::Stopped, None, 0, None, None, None),
            sheep(
                5,
                "metrics",
                ProcStatus::Online,
                Some(48_240),
                0,
                Some(0.4),
                Some(11 << 20),
                None,
            ),
        ],
        _ => vec![
            sheep(
                0,
                "web",
                ProcStatus::Online,
                Some(48_211),
                0,
                Some(3.4),
                Some(182 << 20),
                Some("edge"),
            ),
            sheep(
                1,
                "web",
                ProcStatus::Online,
                Some(48_212),
                0,
                Some(2.9),
                Some(178 << 20),
                Some("edge"),
            ),
            sheep(
                2,
                "api",
                ProcStatus::Online,
                Some(48_219),
                1,
                Some(7.1),
                Some(241 << 20),
                Some("edge"),
            ),
            sheep(
                3,
                "billing-reconciliation-worker",
                ProcStatus::Online,
                Some(48_230),
                0,
                Some(0.8),
                Some(96 << 20),
                None,
            ),
            sheep(
                4,
                "cron",
                ProcStatus::Online,
                Some(48_233),
                0,
                Some(0.1),
                Some(8 << 20),
                None,
            ),
            sheep(
                5,
                "metrics",
                ProcStatus::Online,
                Some(48_240),
                0,
                Some(0.4),
                Some(11 << 20),
                None,
            ),
        ],
    };
    app.update(Msg::Snapshot {
        rows: flock,
        at: t0,
    });
    app.update(Msg::Tick {
        now: t0 + Duration::from_secs(7),
    });

    // Onto `api`, id 2, in both flocks — the third row of each. A fresh
    // snapshot selects the FIRST id, so without this every "sheep 2  api"
    // and "bleats  api" assertion below is asserting about `web` at id 0 and
    // failing for a reason that has nothing to do with the pane.
    //
    // The four excluded scenes have either no flock (`Empty`) or no pane
    // below the table to describe (`Narrow`, `TooNarrow`, `TableOnly`), so
    // moving the cursor in them would change a snapshot for no reason.
    if !matches!(
        which,
        Scene::Empty | Scene::Narrow | Scene::TooNarrow | Scene::TableOnly
    ) {
        app.update(Msg::Key(KeyPress::SelectDown));
        app.update(Msg::Key(KeyPress::SelectDown));
    }

    // Every live scene gets a host sample. The FROZEN one gets it too, and
    // that is load-bearing: `the_frozen_frame_does_not_move_however_long_the
    // _link_stays_gone` renders the frozen scene at two clock ages and
    // compares the frames byte for byte, so a host strip that kept updating
    // after the link was lost reddens it with no new assertion written. A
    // frozen scene with no host sample would leave that mutation uncaught —
    // Task 4's own mutation step says so out loud.
    if which == Scene::HostUnknown {
        app.update(Msg::Host { sample: None });
    } else {
        app.update(Msg::Host {
            sample: Some(HostSample {
                load: (2.31, 4.10, 3.88),
                cores: Some(10),
                memory_total_bytes: 32 << 30,
                memory_used_bytes: 12 * (1 << 30) + (410 << 20),
                uptime_seconds: 6 * 86_400 + 3 * 3_600,
            }),
        });
    }

    app.update(Msg::Bleats {
        tail: feed_for(which),
    });

    // The `Msg::Host` above and the two `SelectDown`s are applied BEFORE
    // `Msg::Frozen` below, because the reducer refuses both after — which is
    // the property, and which the two-age comparison in
    // `the_frozen_frame_does_not_move_however_long_the_link_stays_gone` then
    // pins.
    match which {
        Scene::Retrying => {
            app.update(Msg::Retrying { attempt: 3 });
        }
        Scene::Frozen => {
            app.update(Msg::Frozen {
                at_local: "2026-08-14 14:32:07".to_string(),
            });
        }
        Scene::Refused => {
            app.update(Msg::Key(KeyPress::Stop));
        }
        _ => {}
    }

    // The last tick, `age` after the opening snapshot. For every live scene
    // this is what advances the UPTIME column; for the frozen one it must
    // change nothing at all, because the reducer stopped accepting `now`
    // when the link was lost. That asymmetry is the whole of design
    // decision 8, and rendering the same scene at two ages is how it
    // becomes testable.
    app.update(Msg::Tick { now: t0 + age });

    let (width, height) = which.size();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(&app, frame)).unwrap();
    terminal.backend().buffer().clone()
}

/// The bleats feed each scene is given, before `Msg::Bleats` carries it in.
///
/// **most scenes** — six ordinary `Stream::Out` lines, `missed_lines: 0`,
/// `missed_bytes: 0`, so the ordinary header is what the gallery shows.
///
/// **`FeedGap`** — thirty lines with `missed_lines: 500` and
/// `missed_bytes: 4_012_000`: the both-kinds case. The header reads
/// `… 525 earlier lines not shown, and 3.8M before them never read` — 500
/// the reader dropped, plus 25 the five-row pane has no room for, and
/// `human_bytes(4_012_000)` is `3.8M`. This is the frame that has to be
/// legible, because it is the one an operator sees during an incident.
///
/// **`FeedMissing`** — no lines, no counts, and a `note` naming the cause,
/// mirroring [`super::tail::read`]'s own wording for a log file that was
/// never created.
fn feed_for(which: Scene) -> Tail {
    match which {
        // Mirrors what `super::mod`'s `run_ui` would actually send: an empty
        // flock has no selected row, so `out`/`err` are both `None` and
        // `tail::read` takes its own early return with this exact note.
        // Anything else here would show stale feed content under a header
        // that says nobody is selected, which is a real inconsistency this
        // gallery must not ship.
        Scene::Empty => Tail {
            lines: Vec::new(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 0,
            note: Some("the shepherd did not report a log path for this sheep".to_string()),
        },
        Scene::FeedGap => Tail {
            lines: (0..30)
                .map(|n| TailLine {
                    stream: Stream::Out,
                    text: format!("GET /v1/orders/{n} 200 {}ms", 8 + n % 40),
                })
                .collect(),
            missed_lines: 500,
            missed_bytes: 4_012_000,
            read_bytes: 65_536,
            note: None,
        },
        Scene::FeedMissing => Tail {
            lines: Vec::new(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 0,
            note: Some("this sheep has not written a log in this $SHEP_HOME".to_string()),
        },
        _ => Tail {
            lines: [
                "listening on 0.0.0.0:8080",
                "GET /healthz 200 3ms",
                "GET /v1/orders 200 44ms",
                "POST /v1/orders 201 88ms",
                "GET /v1/orders/8821 200 9ms",
                "connection pool: 14/50 in use",
            ]
            .into_iter()
            .map(|text| TailLine {
                stream: Stream::Out,
                text: text.to_string(),
            })
            .collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 512,
            note: None,
        },
    }
}

/// One row's worth of shepherd reply, spelled out so each scene reads as a
/// plausible flock rather than as six copies of one sheep.
///
/// The two log paths are DERIVED from `name` and `id` rather than taken as
/// two more parameters — this already carries
/// `#[allow(clippy::too_many_arguments)]` at eight, and ten would be worse.
/// Deterministic, and it gives the detail pane something to render and
/// `no_detail` something to assert the absence of.
#[allow(clippy::too_many_arguments)]
fn sheep(
    id: u32,
    name: &str,
    status: ProcStatus,
    pid: Option<u32>,
    restarts: u32,
    cpu: Option<f32>,
    memory: Option<u64>,
    fold: Option<&str>,
) -> ProcessInfo {
    ProcessInfo::builder(id, name, status)
        .pid(pid)
        .restarts(restarts)
        .uptime_ms(4_512_000 + u64::from(id) * 91_000)
        .cpu_percent(cpu)
        .memory_bytes(memory)
        .fold(fold.map(str::to_string))
        .out_file(Some(format!("/home/rin/.shep/logs/{name}-{id}-out.log")))
        .err_file(Some(format!("/home/rin/.shep/logs/{name}-{id}-err.log")))
        .build()
}

/// The header both gallery files open with.
///
/// Not a doc comment on the test: this text is read by a person opening
/// `docs/lookout/frames.txt` with no context at all, and it is the only
/// place that says where those frames came from.
const GALLERY_PREAMBLE: &str = "shep lookout — Phase 12b frames
================================

These are real frames, rendered headlessly through ratatui's TestBackend by

    cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery

Nothing here is a mockup.

frames.ansi is the same fourteen frames with colour; read it with `less -R`.

All four panes are here: the flock table (the spine), the host-usage strip,
the sheep detail pane and the bleats feed. `>` marks the selected sheep, and
every pane below the table describes that one sheep.

The feed reads the selected sheep's log files from disk and re-reads them with
each flock listing. It is not a live subscription, and it says so on its own
header line: `out then err` because the two files are shown end to end with no
interleaving, and `re-read with each listing` because a two-second gap in this
pane is the refresh, not the sheep.

When the pane cannot show everything, the header says what went instead. Lines
it read and dropped are counted exactly; bytes below its 64 KiB window were
never read at all, so those are reported in bytes, because nothing counted the
lines in them and guessing would be worse than saying so.
";

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the plain renderer starts carrying escape bytes, or stops
    /// producing one line per buffer row. This is the renderer the view's
    /// own assertions read through, so a change here silently changes what
    /// nine other tests are asserting on.
    #[test]
    fn the_plain_renderer_is_one_line_per_row_and_no_escapes() {
        let text = render_text(&scene(Scene::HealthyWide).1);
        assert_eq!(text.lines().count(), 30);
        assert!(!text.contains('\u{1b}'), "plain means plain");
        for line in text.lines() {
            assert_eq!(line.chars().count(), 120, "every row is the full width");
        }
    }

    /// fails if the ANSI renderer stops emitting colour, or stops
    /// resetting. A frame that sets a colour and never resets it bleeds
    /// into whatever the operator's terminal prints next — which for a file
    /// read through `less -R` is the rest of the file.
    #[test]
    fn the_ansi_renderer_colours_the_errored_row_and_always_resets() {
        let ansi = render_ansi(&scene(Scene::Errored).1);
        assert!(
            ansi.contains("\u{1b}[38;5;166m"),
            "bark, on the errored status"
        );
        for line in ansi.lines() {
            assert!(
                line.is_empty() || line.ends_with("\u{1b}[0m"),
                "every line resets before its newline"
            );
        }
    }

    /// fails if a scene stops rendering what it is named for. Each
    /// assertion is the one sentence that scene exists to show Rin — if one
    /// of these stops being true, the frame she is looking at is not the
    /// frame this plan promised her.
    ///
    /// Every clause of every caption in [`Scene::caption`] is pinned by one
    /// assertion here — the rule this task adds, stated in its own words in
    /// the plan: "every clause of every caption is one assertion here, or it
    /// is deleted from the caption."
    #[test]
    #[allow(clippy::too_many_lines)] // fourteen captions, each pinned clause by clause
    fn every_scene_shows_the_thing_it_is_named_for() {
        // "All three panes at 120x30: the host strip under the title, the
        //  detail pane and the bleats feed under the table. `>` marks the
        //  selected sheep, and every pane below the table describes it."
        let wide = render_text(&scene(Scene::HealthyWide).1);
        assert!(wide.contains("FOLD"), "every column fits at 120 columns");
        assert!(
            wide.contains("host  load 2.31 4.10 3.88 / 10 cores"),
            "the host strip"
        );
        assert!(
            wide.contains("sheep 2  api"),
            "the detail pane, on the selected sheep"
        );
        assert!(
            wide.contains("bleats  api"),
            "and the feed, on the same one"
        );
        assert_eq!(
            wide.lines().filter(|line| line.starts_with('>')).count(),
            1,
            "exactly one selection marker"
        );

        // "No sheep registered. Each of the three panes says why it is empty,
        //  and the three sentences are different because the three reasons
        //  are."
        let empty = render_text(&scene(Scene::Empty).1);
        assert!(
            empty.contains("the flock is empty"),
            "the table's own sentence"
        );
        assert!(
            empty.contains("no sheep selected: the flock is empty"),
            "the detail pane's"
        );
        assert!(empty.contains("bleats  no sheep is selected"), "the feed's");
        assert!(
            empty.contains("flock cpu -"),
            "and the strip shows no reading, not zero"
        );

        // "51 columns: FOLD, RESTARTS, PID and MEM are gone, in that order.
        //  CPU and UPTIME survive because they explain WHY. The host strip
        //  fits; the detail pane and the feed do not, at 14 rows."
        let narrow = render_text(&scene(Scene::Narrow).1);
        assert!(narrow.contains("CPU") && narrow.contains("UPTIME"));
        for gone in ["FOLD", "RESTARTS", "PID", "MEM"] {
            assert!(!narrow.contains(gone), "the narrow tier dropped {gone}");
        }
        assert!(narrow.contains("host  load"), "the strip is up at 14 rows");
        assert!(!narrow.contains("bleats  "), "the feed is not");
        assert!(
            !narrow.contains("sheep 0  "),
            "and neither is the detail pane"
        );

        // "28 columns: below the floor, the pane refuses rather than drawing
        //  overlapping garbage. Two short lines, so the refusal still fits the
        //  terminal it is refusing about."
        let too_narrow = render_text(&scene(Scene::TooNarrow).1);
        let mut lines = too_narrow.lines();
        assert_eq!(lines.next().unwrap().trim_end(), "too small");
        assert_eq!(lines.next().unwrap().trim_end(), "need 33x6");

        // "The feed under a burst: four megabytes were never read and some
        //  hundreds of lines were read and dropped. The pane counts both, and
        //  counts them separately, because it knows the second exactly and
        //  cannot know how many lines are in the first."
        let gap = render_text(&scene(Scene::FeedGap).1);
        assert!(
            gap.contains("earlier lines not shown"),
            "the lines it dropped"
        );
        assert!(gap.contains("3.8M"), "the exact figure, not a vague one");
        assert!(gap.contains("never read"), "and what it never looked at");
        assert!(
            !gap.contains("re-read with each listing"),
            "the gap replaces the header"
        );

        // "The selected sheep has never written a log in this $SHEP_HOME. The
        //  feed names that cause rather than sitting blank."
        let missing = render_text(&scene(Scene::FeedMissing).1);
        assert!(missing.contains("has not written a log in this $SHEP_HOME"));

        // "20 rows: the detail pane is the first to go, because every number
        //  on it but the log paths is already in the row above it."
        let no_detail = render_text(&scene(Scene::NoDetail).1);
        assert!(
            no_detail.contains("bleats  api"),
            "the feed stayed, on the selection"
        );
        assert!(no_detail.contains("host  load"), "and so did the strip");
        // The ABSENCE, pinned to something only the detail pane can emit. The
        // first draft asserted `!contains("sheep 2  api")`, which passes just
        // as well when the selection is on sheep 0 and the pane drew
        // perfectly — a check that cannot fail for the reason it exists. The
        // log-path prefix is the detail pane's alone: the feed's body lines
        // are tagged `out  ` too, but they carry log TEXT, not a path.
        assert!(
            !no_detail.contains("out  /home/rin/.shep/logs/"),
            "the detail pane went"
        );

        // "12 rows: no optional panes at all. This is 12a's frame, and the
        //  only thing that changed is the two-column gutter the marker sits in."
        let table_only = render_text(&scene(Scene::TableOnly).1);
        assert!(!table_only.contains("host  load"));
        assert!(!table_only.contains("bleats  "));
        assert!(table_only.contains("STATUS"), "the table is still there");

        // "33 columns, 26 rows: the narrowest terminal that draws, with all
        //  three panes up. Everything truncates with an ellipsis; nothing
        //  overlaps."
        let cramped = render_text(&scene(Scene::Cramped).1);
        assert!(cramped.contains('…'), "something truncated, visibly");
        // NOT `line.chars().count() == 33` on every row: `render_text` maps
        // `(0..area.width)` for every row by construction, so that is true of
        // any frame at any width, including a blank one. What "nothing
        // overlaps" actually means is that each pane's own marker appears
        // exactly once, which is a claim about this layout.
        for marker in ["host  ", "bleats  ", "out  /home/rin/.shep/logs/"] {
            assert_eq!(
                cramped
                    .lines()
                    .filter(|line| line.starts_with(marker))
                    .count(),
                1,
                "{marker:?} appears once at 33 columns"
            );
        }
        assert!(
            cramped.lines().last().unwrap().contains("read-only"),
            "and the status bar is still the last row"
        );

        // The four scenes carried over from 12a all changed meaning this
        // phase — three panes, a marker, a strip, and 20 rows becoming 30 —
        // so their captions were rewritten and each new clause is pinned here
        // rather than left as prose nobody checked.

        // "The shepherd stopped answering. Five attempts over about eight
        //  seconds before this becomes the next frame. Every pane below the
        //  table keeps describing the selected sheep from the last listing."
        let retrying = render_text(&scene(Scene::Retrying).1);
        assert!(retrying.contains("reconnecting"));
        assert!(
            retrying.contains("sheep 2  api"),
            "the detail pane is still up"
        );
        assert!(retrying.contains("host  load"), "and so is the strip");

        // "The ladder ran out. Last known values stay, the uptime clock has
        //  stopped, and so has the host strip — one line ticking over on a
        //  frozen screen is a contradiction on the same frame."
        let frozen = render_text(&scene(Scene::Frozen).1);
        assert!(frozen.contains("the shepherd has died"));
        assert!(
            frozen.contains("host  load 2.31 4.10 3.88 / 10 cores"),
            "the strip kept its LAST values rather than blanking"
        );

        // "One errored, one waiting to restart, one stopped, with the
        //  selection parked on the errored sheep. Each row's own STATUS cell
        //  is the only coloured cell in that row."
        let errored = render_text(&scene(Scene::Errored).1);
        assert!(errored.contains("errored"));
        assert!(
            errored.contains("sheep 2  api"),
            "the selection is on the errored sheep"
        );
        assert_eq!(
            errored.lines().filter(|line| line.starts_with('>')).count(),
            1,
            "exactly one marker, on that row"
        );
        // Each row's own STATUS cell is the only coloured cell in that row:
        // `online`, `errored` and `waiting-restart` each get their own
        // status colour, and `stopped` happens to share the chrome's muted
        // grey rather than standing out from it. Only the ANSI rendering
        // carries colour to check this against.
        let errored_ansi = render_ansi(&scene(Scene::Errored).1);
        assert!(
            errored_ansi.contains("\u{1b}[38;5;29monline"),
            "online's STATUS cell gets meadow"
        );
        assert!(
            errored_ansi.contains("\u{1b}[38;5;166merrored"),
            "errored's STATUS cell gets bark"
        );
        assert!(
            errored_ansi.contains("\u{1b}[38;5;221mwaiting-restart"),
            "waiting-restart's STATUS cell gets butter"
        );
        assert!(
            errored_ansi.contains("\u{1b}[38;5;245mID"),
            "the header row is muted grey, the same token stopped's STATUS uses"
        );
        assert!(
            errored_ansi.contains("\u{1b}[38;5;245mstopped"),
            "stopped's STATUS cell shares the chrome's muted grey rather than standing out"
        );

        // "`x` with actions gated off. Both refusals are literal — nothing
        //  about damage gets charming — and the panes below carry on."
        let refused = render_text(&scene(Scene::Refused).1);
        assert!(refused.contains("--allow-control"));
        assert!(
            refused.contains("bleats  api"),
            "a refusal does not blank the screen"
        );

        // "sysinfo reports this platform unsupported. The strip says so and
        //  keeps the flock's own totals, which lookout can always compute."
        let unknown = render_text(&scene(Scene::HostUnknown).1);
        assert!(unknown.contains("host  usage is not available on this platform"));
        assert!(
            unknown.contains("flock cpu"),
            "the half lookout can compute survives"
        );
    }

    /// fails if a scene is added to [`Scene::ALL`] without a caption, or with
    /// a caption nobody pinned. The second half cannot be checked by a
    /// machine — but the first half can, and a scene with no caption is how
    /// an unpinned one gets in.
    #[test]
    fn every_scene_has_a_caption_and_a_distinct_label() {
        let mut labels = std::collections::BTreeSet::new();
        for which in Scene::ALL {
            assert!(
                labels.insert(which.label()),
                "two scenes share {}",
                which.label()
            );
            let caption = which.caption();
            assert!(caption.len() > 30, "{} has a stub caption", which.label());
            assert!(
                caption.ends_with('.'),
                "{}'s caption is not a sentence",
                which.label()
            );
        }
        // `labels.len() == Scene::ALL.len()` is not asserted: the `insert`
        // above already guarantees it, so it would be a line that cannot
        // fail. The literal can — it is what catches a scene added to the
        // enum and not to `ALL`, or the reverse.
        assert_eq!(Scene::ALL.len(), 14);
    }

    /// fails if a 12b pane introduced a text MODIFIER. `sgr` renders
    /// foregrounds only — its own doc says a modifier would come out unstyled
    /// — and this phase deliberately introduced none: the selection marker is
    /// a character, not a `REVERSED` row, precisely so `NO_COLOR` and a
    /// 16-colour terminal lose nothing. If this ever reddens, `sgr` needs a
    /// case before the gallery is regenerated, or the modifier needs
    /// removing.
    #[test]
    fn no_scene_uses_a_modifier_the_ansi_renderer_would_drop() {
        for which in Scene::ALL {
            let buffer = scene(*which).1;
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let cell = &buffer[(buffer.area.x + x, buffer.area.y + y)];
                    assert!(
                        cell.modifier.is_empty(),
                        "{} has a modifier at {x},{y}",
                        which.label()
                    );
                }
            }
        }
    }

    /// fails if a frozen frame keeps counting. This is the one thing the
    /// frozen scene exists to show Rin, and it is the property design
    /// decision 8 is about.
    ///
    /// **Rendered twice at two different clock ages and compared**, rather
    /// than compared against the healthy scene: those two frames differ by
    /// a banner line, five statuses and a row shift, so an `assert_ne!`
    /// between them holds whether or not the clock stopped and cannot
    /// detect the regression its name claims. The live pair at the bottom
    /// is what keeps the frozen pair honest — without it, a `render_text`
    /// that emitted no UPTIME column at all would satisfy the first
    /// assertion perfectly.
    #[test]
    fn the_frozen_frame_does_not_move_however_long_the_link_stays_gone() {
        let ten_minutes = render_text(&scene_with(Scene::Frozen, Duration::from_secs(600)));
        let sixteen_hours = render_text(&scene_with(Scene::Frozen, Duration::from_secs(60_000)));
        assert_eq!(
            ten_minutes, sixteen_hours,
            "the frozen frame's uptime column advanced after the link was lost"
        );

        let live_ten = render_text(&scene_with(Scene::HealthyWide, Duration::from_secs(600)));
        let live_sixteen =
            render_text(&scene_with(Scene::HealthyWide, Duration::from_secs(60_000)));
        assert_ne!(
            live_ten, live_sixteen,
            "a LIVE frame's uptime column must advance, or the assertion above passes for the wrong reason"
        );
    }

    /// The frame pins. NOT wire fixtures: re-accepting these after a
    /// deliberate layout change is correct and expected, which is the
    /// opposite of IR-35's rule for the protocol snapshots in shep-core.
    /// Nobody may apply wire discipline to a border glyph.
    #[test]
    fn frames_are_pinned() {
        for which in Scene::ALL {
            let (label, buffer) = scene(*which);
            insta::assert_snapshot!(label, render_text(&buffer));
        }
    }

    /// Writes `docs/lookout/frames.txt` and `docs/lookout/frames.ansi`.
    ///
    /// `#[ignore]` because it writes into the repository, which no ordinary
    /// test run may do. Run it deliberately:
    ///
    /// ```text
    /// cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery
    /// ```
    ///
    /// This is the ONE ignored test this phase adds — the `ignored` count
    /// in the workspace summary goes 3 -> 4, exactly once.
    ///
    /// It cannot rot: every frame it writes is `render_text`/`render_ansi`
    /// over the same `Scene::ALL` the pinned snapshots above read, so a
    /// layout change reddens the ordinary suite first.
    #[test]
    #[ignore = "writes into docs/lookout; run it deliberately"]
    fn write_the_gallery() {
        // Absolute, derived from the manifest — so it lands in the same
        // place whatever directory the run started in.
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/lookout"));
        std::fs::create_dir_all(dir).unwrap();

        let mut plain = String::from(GALLERY_PREAMBLE);
        let mut ansi = String::from(GALLERY_PREAMBLE);
        for which in Scene::ALL {
            let (label, buffer) = scene(*which);
            let (width, height) = which.size();
            let heading = format!(
                "\n\n=== {label}  ({width}x{height}) ===\n{}\n\n",
                which.caption()
            );
            plain.push_str(&heading);
            plain.push_str(&render_text(&buffer));
            ansi.push_str(&heading);
            ansi.push_str(&render_ansi(&buffer));
        }
        std::fs::write(dir.join("frames.txt"), &plain).unwrap();
        std::fs::write(dir.join("frames.ansi"), &ansi).unwrap();

        // A live assertion, not a `timeout`: this function is synchronous,
        // so a `tokio::time::timeout` around it would complete on its first
        // poll and bound nothing at all. What can actually go wrong here is
        // a scene rendering empty, and that is what these two check.
        assert!(
            plain.lines().count() > 100,
            "eight frames is more than a hundred lines"
        );
        assert_eq!(plain.matches("=== ").count(), Scene::ALL.len());
    }
}
