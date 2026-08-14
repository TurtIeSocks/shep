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
    /// A healthy flock at a comfortable width.
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
        }
    }

    /// One sentence saying what this frame is for, printed above it in the
    /// gallery so Rin does not have to hold eight of them in her head.
    #[must_use]
    pub const fn caption(self) -> &'static str {
        match self {
            Self::HealthyWide => "A healthy flock at 120 columns: all nine columns fit.",
            Self::Errored => {
                "One errored, one waiting to restart, one stopped. Colour is on the STATUS word and nowhere else."
            }
            Self::Empty => {
                "No sheep registered. The header row still prints, and a sentence says why the pane is empty."
            }
            Self::Narrow => {
                "49 columns: FOLD, RESTARTS, PID and MEM are gone, in that order. CPU and UPTIME survive because they explain WHY."
            }
            Self::TooNarrow => {
                "28 columns: below the floor, the pane refuses rather than drawing overlapping garbage. The refusal is two short lines so it still fits."
            }
            Self::Retrying => {
                "The shepherd stopped answering. Five attempts over about eight seconds before this becomes the next frame."
            }
            Self::Frozen => {
                "The ladder ran out. Last known values stay; the uptime clock has stopped; lookout does not exit."
            }
            Self::Refused => {
                "`x` with actions gated off. Both refusals are literal — nothing about damage gets charming."
            }
        }
    }

    /// The terminal size this scene is rendered at.
    #[must_use]
    pub const fn size(self) -> (u16, u16) {
        match self {
            Self::Empty => (100, 12),
            // 49, not 46. `columns_for` picks the first tier whose threshold
            // is <= the width, and 46 lands on the `41` tier — which has
            // already dropped CPU, so a scene rendered there would
            // contradict its own caption in the gallery Rin reads. 49 is the
            // `NO_MEM` tier: four columns gone, CPU and UPTIME still there.
            Self::Narrow => (49, 14),
            Self::TooNarrow => (28, 8),
            _ => (120, 20),
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

/// One row's worth of shepherd reply, spelled out so each scene reads as a
/// plausible flock rather than as six copies of one sheep.
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
        .build()
}

/// The header both gallery files open with.
///
/// Not a doc comment on the test: this text is read by a person opening
/// `docs/lookout/frames.txt` with no context at all, and it is the only
/// place that says where those frames came from.
const GALLERY_PREAMBLE: &str = "shep lookout — Phase 12a frames
================================

These are real frames, rendered headlessly through ratatui's TestBackend by

    cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery

Nothing here is a mockup.

frames.ansi is the same eight frames with colour; read it with `less -R`.

They are here to be looked at BEFORE Phase 12b's layout is decided. 12a
builds the shell and one pane on purpose — the bleats feed, the sheep detail
pane and the host-usage strip are 12b, and how those three sit beside this
one is the decision these frames exist to inform.
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
        assert_eq!(text.lines().count(), 20);
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
    #[test]
    fn every_scene_shows_the_thing_it_is_named_for() {
        assert!(render_text(&scene(Scene::Empty).1).contains("the flock is empty"));
        assert!(render_text(&scene(Scene::TooNarrow).1).contains("need 31x6"));
        assert!(render_text(&scene(Scene::Frozen).1).contains("the shepherd has died"));
        assert!(render_text(&scene(Scene::Retrying).1).contains("reconnecting"));
        assert!(render_text(&scene(Scene::Refused).1).contains("--allow-control"));
        assert!(render_text(&scene(Scene::Errored).1).contains("errored"));
        assert!(render_text(&scene(Scene::HealthyWide).1).contains("FOLD"));

        // The narrow scene's caption in the gallery makes four specific
        // claims about which columns survive at this width. Each is
        // asserted here, so a scene rendered at a width that contradicts
        // its own caption reddens the suite rather than shipping to Rin —
        // `STATUS` alone would not, since STATUS is in the floor tier and
        // present at every width the pane draws at.
        let narrow = render_text(&scene(Scene::Narrow).1);
        assert!(narrow.contains("CPU"), "CPU survives the narrow tier");
        assert!(narrow.contains("UPTIME"), "and so does UPTIME");
        for gone in ["FOLD", "RESTARTS", "PID", "MEM"] {
            assert!(!narrow.contains(gone), "the narrow tier dropped {gone}");
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
