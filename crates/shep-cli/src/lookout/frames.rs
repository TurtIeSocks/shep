//! `Buffer` -> text, and `Buffer` -> ANSI, plus the scene list both the
//! pinned snapshots and the gallery writer are built from.
//!
//! **Why this exists at all.** `TestBackend` renders a frame into a plain
//! text buffer with no terminal involved, which is what makes a TUI testable
//! headlessly. It is also, for exactly the same reason, what lets a reviewer
//! SEE the dashboard without running it — so this module's output is a
//! deliverable (`docs/lookout/frames.txt`, `docs/lookout/frames.ansi`) and
//! not only test scaffolding. That is the whole point of this module: Rin
//! decides what a layout looks like from these frames, not from a spec
//! sentence — the way Phase 12a decided 12b's and a later phase will decide
//! search/filter's and the actions'.
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
//! `pub mod`.** The package (`shep`) has had a `[lib]` target since Phase 14,
//! but that does not exempt this module from `dead_code`: `mod lookout` in
//! `lib.rs` is private, and `lib.rs`'s own doc comment states the crate's
//! whole public API as three entry points — `main`, `main_runtime`,
//! `main_dev` — with every other item private. A `pub mod frames` nested
//! inside a private module is unreachable from outside this crate regardless
//! of the keyword, so nothing here is called outside this module's own
//! tests, and a plain `pub mod` fails the task gate on `dead_code`. Gating
//! the whole module means these items simply do not exist in a non-test
//! build, and cost nothing when they run under `cargo test`.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

use shep_client::RequestError;
use shep_core::protocol::{ExitInfo, Lamb, ProcessInfo, Response, RpcError, RpcErrorCode};
use shep_core::status::ProcStatus;

use super::app::{ActionVerb, App, Control, KeyPress, Msg, Sent};
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
/// Foreground only, because a foreground is the only thing this palette
/// sets anywhere on screen — no pane uses bold, reversed, or any other
/// modifier; the selected row is shown by a marker character, not a style.
/// `no_scene_uses_a_modifier_the_ansi_renderer_would_drop` is the standing
/// check. A future pane that introduces one renders unstyled here rather
/// than as a wrong style, and this function grows a case for it then.
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
    /// Mid-type: the table has already narrowed and the box is still open.
    FilterEditing,
    /// Applied and no longer editing.
    FilterActive,
    /// A query nothing matches.
    FilterNoMatch,
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
    /// The detail pane with a lamb list.
    Lambs,
    /// The detail pane on a sheep with no pid, where the shepherd had no tree
    /// to walk.
    LambsUnknown,
    /// An action key pressed with the gate open. Nothing has been sent.
    Confirm,
    /// Enter pressed. The request is out.
    Acting,
    /// The shepherd refused, in its own words.
    ActionRefused,
    /// The shepherd did it, and the bar says so in the non-grave style.
    ActionAccepted,
    /// An action key pressed while the link is coming back.
    ActionRefusedOffline,
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
        Self::FilterEditing,
        Self::FilterActive,
        Self::FilterNoMatch,
        Self::NoDetail,
        Self::TableOnly,
        Self::FeedGap,
        Self::FeedMissing,
        Self::Cramped,
        Self::HostUnknown,
        Self::Lambs,
        Self::LambsUnknown,
        Self::Confirm,
        Self::Acting,
        Self::ActionRefused,
        Self::ActionAccepted,
        Self::ActionRefusedOffline,
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
            Self::FilterEditing => "filter_editing",
            Self::FilterActive => "filter_active",
            Self::FilterNoMatch => "filter_no_match",
            Self::NoDetail => "no_detail",
            Self::TableOnly => "table_only",
            Self::FeedGap => "feed_gap",
            Self::FeedMissing => "feed_missing",
            Self::Cramped => "cramped",
            Self::HostUnknown => "host_unknown",
            Self::Lambs => "lambs",
            Self::LambsUnknown => "lambs_unknown",
            Self::Confirm => "confirm",
            Self::Acting => "acting",
            Self::ActionRefused => "action_refused",
            Self::ActionAccepted => "action_accepted",
            Self::ActionRefusedOffline => "action_refused_offline",
        }
    }

    /// One sentence saying what this frame is for, printed above it in the
    /// gallery so Rin does not have to hold twenty-four of them in her head.
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
                "One errored, one waiting to restart, one stopped, with the selection parked on the errored sheep. Each row's own STATUS cell is the only coloured cell in that row, and EXIT carries why each of the three stopped: a code for the two that crashed, a signal name for the one shep stopped itself."
            }
            Self::Empty => {
                "No sheep registered. Each of the three panes says why it is empty, and the three sentences are different because the three reasons are."
            }
            Self::Narrow => {
                "51 columns: FOLD, EXIT, RESTARTS, PID and MEM are gone, in that order. CPU and UPTIME survive because they explain WHY a RUNNING sheep is behaving badly, a question EXIT cannot even ask. The host strip fits; the detail pane and the feed do not, at 14 rows."
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
                "`x` with actions gated off. The refusal is literal, nothing about damage gets charming, and the panes below carry on."
            }
            Self::FilterEditing => {
                "Mid-type at 100x14. The table has already narrowed to the two sheep whose names contain the query, the title counts the narrowed set and the whole flock, and the status bar carries the query, a cursor, and the three keys that mean anything while the box is open."
            }
            Self::FilterActive => {
                "The same query applied. The box is closed, the table is still narrowed, and the bar has changed to name the two keys that now touch the filter."
            }
            Self::FilterNoMatch => {
                "A query nothing matches. The table names the query rather than claiming the flock is empty, and the title keeps the flock's real size on screen."
            }
            Self::NoDetail => {
                "20 rows: the detail pane is the first to go, because every number on it but the log paths is already in the row above it."
            }
            Self::TableOnly => {
                "12 rows: no optional panes at all. This is 12a's frame, and the only thing that changed is the two-column gutter the marker sits in."
            }
            Self::FeedGap => {
                "The feed under a burst: 3.8 megabytes were never read and some hundreds of lines were read and dropped. The pane counts both, and counts them separately, because it knows the second exactly and cannot know how many lines are in the first."
            }
            Self::FeedMissing => {
                "The selected sheep has never written a log in this $SHEP_HOME. The feed names that cause rather than sitting blank."
            }
            Self::Cramped => {
                "33 columns: the narrowest terminal that draws. 26 rows — a couple more than the 24-row floor for all three panes being up, so this frame has a little breathing room rather than sitting exactly on the edge. Everything truncates with an ellipsis; nothing overlaps."
            }
            Self::HostUnknown => {
                "`sysinfo` reports this platform unsupported. The strip says so and keeps the flock's own totals, which lookout can always compute."
            }
            Self::Lambs => {
                "The detail pane with a lamb list: how many descendants the shepherd's walk found, how old that reading is, and each lamb's pid and executable name. The stamp sits before the list so a narrow terminal truncates lambs rather than the caveat."
            }
            Self::LambsUnknown => {
                "The same pane on a stopped sheep. The shepherd had no pid to walk from and left the field unset rather than empty, and the line says which of the two it is looking at rather than reporting none found."
            }
            Self::Confirm => {
                "`R` pressed with the gate open. Nothing has been sent: the bar asks a question naming the verb and the exact sheep, and `api` is still online in the table behind it."
            }
            Self::Acting => {
                "Enter pressed. The request is out and nothing on the table has changed, because nothing the shepherd has said has changed: `api` is still online and the cursor has not moved."
            }
            Self::ActionAccepted => {
                "The shepherd answered. The bar says what it did, in the non-grave style a refusal does not get, and the table shows the row the reply carried rather than waiting for the next poll."
            }
            Self::ActionRefused => {
                "The shepherd refused while the request was out, and its own sentence is forwarded rather than rewritten. The sheep has left the flock in the listing behind it, so the table is one row shorter and the cursor has moved to the row below."
            }
            Self::ActionRefusedOffline => {
                "An action key pressed while the link is coming back. The refusal names the same reconnect attempt the banner above it does, rather than the exhausted-ladder sentence — Phase 16 review Minor #8 caught the two disagreeing on one frame."
            }
        }
    }

    /// Whether this scene's dashboard may act.
    ///
    /// `Lambs` is in here as well as the three action scenes: its bar has
    /// nothing in the left slot, so it is the one frame in the gallery that
    /// shows the control-enabled key hint.
    #[must_use]
    pub const fn control(self) -> Control {
        match self {
            Self::Confirm
            | Self::Acting
            | Self::ActionRefused
            | Self::ActionAccepted
            | Self::ActionRefusedOffline
            | Self::Lambs => Control::Allowed,
            _ => Control::ReadOnly,
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
            Self::FilterEditing | Self::FilterActive | Self::FilterNoMatch => (100, 14),
            Self::NoDetail => (120, 20),
            Self::TableOnly => (120, 12),
            Self::Cramped => (33, 26),
            Self::Confirm
            | Self::Acting
            | Self::ActionRefused
            | Self::ActionAccepted
            | Self::ActionRefusedOffline => (100, 14),
            // HealthyWide, Errored, Retrying, Frozen, Refused, FeedGap,
            // FeedMissing, HostUnknown, Lambs, LambsUnknown: every scene that
            // carries all three optional panes at their ordinary rows.
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

/// Parks the gallery's cursor on the sheep with id `id`.
///
/// `SelectDown` moves one VISIBLE row, and the table reads by name, so the
/// number of presses a given sheep needs is a fact about the fixture's names
/// rather than about its ids. Walking until the id matches keeps a scene
/// describing the sheep its assertions name, whatever the ordering rule is.
///
/// # Panics
///
/// If `id` is not in the flock, or is hidden by a filter. Gallery scaffolding:
/// a scene that silently described a different sheep than the one its own
/// assertions name is the failure this exists to make loud.
#[track_caller]
fn select_id(app: &mut App, id: u32) {
    for _ in 0..=app.flock_len() {
        if app.selected() == Some(id) {
            return;
        }
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    panic!("the gallery cannot park its cursor on id {id}");
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
    let mut app = App::new(palette, which.control(), "/home/rin/.shep".to_string(), t0);

    let flock = match which {
        Scene::Empty => Vec::new(),
        Scene::Errored | Scene::Frozen | Scene::LambsUnknown => vec![
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

    // Onto `api`, id 2, in both flocks. A fresh snapshot selects the first
    // VISIBLE row, so without this every "sheep 2  api" and "bleats  api"
    // assertion below is asserting about whichever sheep the table happens to
    // draw first, and failing for a reason that has nothing to do with the
    // pane.
    //
    // Walked by id rather than by a fixed number of `j`s. The table reads by
    // name, so which row `api` occupies is decided by what the other five
    // sheep are called; two `SelectDown`s used to land on it only because the
    // table read by id and `api` held id 2.
    //
    // The four excluded scenes have either no flock (`Empty`) or no pane
    // below the table to describe (`Narrow`, `TooNarrow`, `TableOnly`), so
    // moving the cursor in them would change a snapshot for no reason.
    if !matches!(
        which,
        Scene::Empty | Scene::Narrow | Scene::TooNarrow | Scene::TableOnly
    ) {
        select_id(&mut app, 2);
    }

    // `LambsUnknown` wants `cron`, id 4, instead.
    if which == Scene::LambsUnknown {
        select_id(&mut app, 4);
    }

    match which {
        Scene::FilterEditing | Scene::FilterNoMatch | Scene::FilterActive => {
            app.update(Msg::Key(KeyPress::FilterStart));
            let query = if which == Scene::FilterNoMatch {
                "zzz"
            } else {
                "web"
            };
            for typed in query.chars() {
                app.update(Msg::Key(KeyPress::FilterChar(typed)));
            }
            if which == Scene::FilterActive {
                app.update(Msg::Key(KeyPress::FilterApply));
            }
        }
        _ => {}
    }

    // Every live scene gets a host sample, including the FROZEN one — a
    // strip with no host sample at all would render "host  not read yet"
    // whether or not the freeze guard existed, so this baseline sample is
    // what gives that guard something to protect. The regression coverage
    // itself is the SECOND, age-varying sample sent after `Msg::Frozen`
    // below, in the `Scene::Frozen` arm: this one only establishes what a
    // live dashboard would have shown first.
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

    // `Scene::Frozen` gets a reading too, applied here — while the link is
    // still `Live` — because `on_lambs` refuses once it is `Lost`, the same
    // guard `Msg::Bleats` carries. It is not there to support a test: the
    // property that a reading does not age once frozen is pinned in
    // `detail.rs`'s own unit test, where the two ages differ by construction
    // rather than by elapsed time. This frame is a picture, and pictures are
    // what Rin reads.
    if matches!(which, Scene::Lambs | Scene::Frozen) {
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 2 },
            result: Ok(Response::Described(vec![
                ProcessInfo::builder(2, "api", ProcStatus::Online)
                    .pid(Some(48_219))
                    .lambs(Some(vec![
                        Lamb::new(48_220, "node"),
                        Lamb::new(48_221, "node"),
                        Lamb::new(48_222, "node"),
                    ]))
                    .build(),
            ])),
        });
    }

    // `LambsUnknown`'s own reading: `cron` (id 4) has no pid, so the
    // shepherd's walk never ran. The plan's own code block for this step
    // never applied a reply for this scene, which leaves `lambs_for(4)`
    // `None` and renders "not read yet" rather than the caption's own
    // "this sheep is not running" sentence the assertions below pin — a gap
    // reported alongside this task rather than silently worked around.
    if which == Scene::LambsUnknown {
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 4 },
            result: Ok(Response::Described(vec![
                ProcessInfo::builder(4, "cron", ProcStatus::Stopped)
                    .lambs(None)
                    .build(),
            ])),
        });
    }

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
            // The frame-level regression test for the `Msg::Host` freeze
            // guard: a sample sent AFTER `Msg::Frozen`, with a load average
            // that varies with `age` so it cannot coincide with the
            // baseline sample above by accident. With the guard in place
            // this is refused and changes nothing, so the ten-minute and
            // sixteen-hour renders in
            // `the_frozen_frame_does_not_move_however_long_the_link_stays_gone`
            // stay byte-identical, as they already do. Remove the guard
            // and this is what makes that test catch it: the two ages
            // would then paint two different load averages onto a banner
            // that claims neither should move.
            app.update(Msg::Host {
                sample: Some(HostSample {
                    load: (2.31 + age.as_secs_f64(), 4.10, 3.88),
                    cores: Some(10),
                    memory_total_bytes: 32 << 30,
                    memory_used_bytes: 12 * (1 << 30) + (410 << 20),
                    uptime_seconds: 6 * 86_400 + 3 * 3_600,
                }),
            });
        }
        Scene::Refused => {
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
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

    // The five action scenes, applied AFTER the last tick rather than
    // alongside `Retrying`/`Frozen`/`Refused` above. `scene()` renders at
    // `age` = 600 seconds, and `CONFIRM_EXPIRY` is 10: an armed confirm built
    // before that tick would already have expired by the time this function
    // draws it, which is exactly the defect `Confirm`'s own frame exists to
    // show the ABSENCE of. A `Sent` action never expires (only `Stage::Armed`
    // does), so `Acting`, `ActionAccepted` and `ActionRefused` would have been
    // safe either side of the tick; `Confirm` is the one that is not, so all
    // five sit here together rather than splitting the rule across two
    // places.
    match which {
        Scene::ActionRefusedOffline => {
            // The link has to stop being live BEFORE the key is pressed, or
            // `arm` would accept it — the order is the whole state this
            // scene shows.
            app.update(Msg::Retrying { attempt: 3 });
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        }
        Scene::Confirm | Scene::Acting | Scene::ActionRefused | Scene::ActionAccepted => {
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
            if which != Scene::Confirm {
                app.update(Msg::Key(KeyPress::Confirm));
            }
            if which == Scene::ActionAccepted {
                app.update(Msg::Replied {
                    sent: Sent::Action {
                        verb: ActionVerb::Restart,
                        id: 2,
                        name: "api".to_string(),
                    },
                    result: Ok(Response::Restarted(vec![restarted_api()])),
                });
            }
            if which == Scene::ActionRefused {
                // The sheep leaves the flock while the request is out, which
                // is what makes the daemon's own sentence the true one.
                app.update(Msg::Snapshot {
                    rows: flock_without_api(),
                    at: t0,
                });
                app.update(Msg::Replied {
                    sent: Sent::Action {
                        verb: ActionVerb::Restart,
                        id: 2,
                        name: "api".to_string(),
                    },
                    result: Err(RequestError::Rpc(RpcError {
                        code: RpcErrorCode::NotFound,
                        message: "selector matched no registered sheep".to_string(),
                    })),
                });
            }
        }
        _ => {}
    }

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
        // flock has no selected row, and `run_ui` never calls `tail::read`
        // at all in that case — its own header, "bleats  no sheep is
        // selected", is the pane's complete sentence. A note here would
        // show stale feed content, or the wrong sentence, under a header
        // that already says nobody is selected — a real inconsistency this
        // gallery must not ship.
        Scene::Empty => Tail::default(),
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
        // Derived from `status` rather than taken as a ninth parameter, and
        // not just to keep the argument count down: a sheep that is not
        // running always has a reason it stopped, and deriving it means no
        // scene can accidentally depict an errored sheep with nothing in its
        // EXIT column. Before this, every pinned frame showed `-` there,
        // including the errored scene -- so the frames documented none of
        // what that column is for, and a regression blanking it entirely
        // would have passed all of them.
        .last_exit(match status {
            // Crashed on its own, and a restart is either pending or spent.
            ProcStatus::Errored | ProcStatus::WaitingRestart => Some(ExitInfo {
                code: Some(1),
                signal: None,
            }),
            // Stopped because shep asked it to, which is a signal.
            ProcStatus::Stopped => Some(ExitInfo {
                code: None,
                signal: Some(15),
            }),
            // Running, or on its way in or out: nothing has exited yet.
            ProcStatus::Online | ProcStatus::Starting | ProcStatus::Stopping => None,
        })
        .build()
}

/// The row `ActionAccepted`'s reply carries: `api` at id 2, restarted.
///
/// **Pid 48299, not the listing's 48219.** A different pid is what makes
/// "the reply's own row reached the table" falsifiable — the same pid would
/// pass whether the reply's row was upserted or silently ignored in favour
/// of what the last poll already had.
fn restarted_api() -> ProcessInfo {
    sheep(
        2,
        "api",
        ProcStatus::Online,
        Some(48_299),
        2,
        Some(7.1),
        Some(241 << 20),
        Some("edge"),
    )
}

/// The default six-sheep flock `scene_with`'s `_` arm builds, with id 2
/// (`api`) removed: five rows, ids 0, 1, 3, 4, 5.
fn flock_without_api() -> Vec<ProcessInfo> {
    vec![
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
    ]
}

/// The header both gallery files open with.
///
/// Not a doc comment on the test: this text is read by a person opening
/// `docs/lookout/frames.txt` with no context at all, and it is the only
/// place that says where those frames came from.
const GALLERY_PREAMBLE: &str = "shep lookout — Phase 16 frames
================================

These are real frames, rendered headlessly through ratatui's TestBackend by

    cargo test -p shep --lib --all-features -- --ignored write_the_gallery

Nothing here is a mockup.

frames.ansi is the same twenty-four frames with colour; read it with `less -R`.

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

    /// The table row for `name`, or `None` if the table does not draw one.
    ///
    /// The selection marker (`>`) sits on the row itself, in its own
    /// one-character gutter column ahead of the id, so a marked row's tokens
    /// are shifted one place right of an unmarked row's: `nth(1)` is the id,
    /// not the name. Stripping the marker first keeps both cases on the same
    /// token index, and the marker never appears anywhere else on a line, so
    /// `trim_start_matches` cannot eat anything else.
    ///
    /// The `nth(0)` numeric-id guard is load-bearing, not decoration: the
    /// status bar's own in-flight and outcome lines both open `{verb} {name}
    /// (id {id})`, so `restart api (id 2): ...` has `"api"` at token index 1
    /// too. Without the guard this finds the BAR line naming the sheep
    /// rather than a table row (or, worse, `None` of them) — exactly the
    /// case `ActionRefused`'s own assertions exercise, where the bar and the
    /// table disagree about `api` on purpose.
    fn row_for<'a>(frame: &'a str, name: &str) -> Option<&'a str> {
        frame.lines().find(|line| {
            let mut tokens = line.trim_start_matches('>').split_whitespace();
            tokens.next().is_some_and(|id| id.parse::<u32>().is_ok()) && tokens.next() == Some(name)
        })
    }

    /// Whether the MARKED row's name starts with `prefix`. For a name the
    /// NAME column has truncated, the exact truncated string depends on
    /// terminal width, so a literal expected value would be wrong at any
    /// width this test was not written against. `prefix` only needs to fit
    /// inside the eight-column floor `name_width` never shrinks below to be
    /// safe here.
    fn marked_row_name_starts_with(frame: &str, prefix: &str) -> bool {
        frame.lines().any(|line| {
            line.starts_with('>')
                && line
                    .trim_start_matches('>')
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|name| name.starts_with(prefix))
        })
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
    #[allow(clippy::too_many_lines)] // twenty-four captions, each pinned clause by clause
    fn every_scene_shows_the_thing_it_is_named_for() {
        // "All three panes at 120x30: the host strip under the title, the
        //  detail pane and the bleats feed under the table. `>` marks the
        //  selected sheep, and every pane below the table describes it."
        let wide = render_text(&scene(Scene::HealthyWide).1);
        assert!(
            wide.contains("FOLD") && wide.contains("EXIT"),
            "every column fits at 120 columns"
        );
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

        // "51 columns: FOLD, EXIT, RESTARTS, PID and MEM are gone, in that
        //  order. CPU and UPTIME survive because they explain WHY a RUNNING
        //  sheep is behaving badly, a question EXIT cannot even ask. The
        //  host strip fits; the detail pane and the feed do not, at 14
        //  rows."
        let narrow = render_text(&scene(Scene::Narrow).1);
        assert!(narrow.contains("CPU") && narrow.contains("UPTIME"));
        for gone in ["FOLD", "EXIT", "RESTARTS", "PID", "MEM"] {
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

        // "The feed under a burst: 3.8 megabytes were never read and some
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

        // "33 columns: the narrowest terminal that draws. 26 rows — a couple
        //  more than the 24-row floor for all three panes being up, so this
        //  frame has a little breathing room rather than sitting exactly on
        //  the edge. Everything truncates with an ellipsis; nothing
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
        //  is the only coloured cell in that row, and EXIT carries why each
        //  of the three stopped: a code for the two that crashed, a signal
        //  name for the one shep stopped itself."
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
        // EXIT carries why each of the three stopped. Asserted on the ROWS
        // rather than on the whole frame, because `errored.contains("1")`
        // would pass on any digit anywhere -- a restart count, a pid, a
        // timestamp -- and pass just as happily if the column were blank.
        let row_of = |name: &str| {
            errored
                .lines()
                .find(|line| line.contains(name))
                .unwrap_or_else(|| panic!("no row for {name}:\n{errored}"))
                .to_string()
        };
        for (name, want) in [
            ("api", "1"),
            // "billing-r", not the fuller prefix this used before task 7:
            // the SMIT column added at this frame's width narrows NAME
            // enough that the truncation lands one syllable earlier.
            ("billing-r", "1"),
            ("cron", "SIGTERM"),
        ] {
            let row = row_of(name);
            assert!(
                row.contains(want),
                "{name}'s EXIT cell must read {want}, not a dash: {row}"
            );
        }
        assert!(
            row_of("metrics").contains(" -   "),
            "a running sheep has no exit to report: {}",
            row_of("metrics")
        );

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

        // "`x` with actions gated off. The refusal is literal, nothing about
        //  damage gets charming, and the panes below carry on."
        let refused = render_text(&scene(Scene::Refused).1);
        assert!(refused.contains("--allow-control"));
        assert!(
            refused.contains("bleats  api"),
            "a refusal does not blank the screen"
        );

        // "Mid-type at 100x14. The table has already narrowed to the two
        //  sheep whose names contain the query, the title counts the
        //  narrowed set and the whole flock, and the status bar carries the
        //  query, a cursor, and the three keys that mean anything while the
        //  box is open."
        let editing = render_text(&scene(Scene::FilterEditing).1);
        assert_eq!(
            editing
                .lines()
                .filter(|line| line.contains("  web  "))
                .count(),
            2,
            "two rows survived the query"
        );
        assert!(!editing.contains("billing"), "and the rest did not");
        assert!(editing.contains("2 of 6 in the flock"), "got {editing:?}");
        assert!(
            editing.contains("filter  web\u{258f}"),
            "the query and the cursor"
        );
        for named in ["enter applies", "esc cancels", "ctrl-c quits"] {
            assert!(editing.contains(named), "the box names {named}");
        }

        // "The same query applied. The box is closed, the table is still
        //  narrowed, and the bar has changed to name the two keys that now
        //  touch the filter."
        let active = render_text(&scene(Scene::FilterActive).1);
        assert!(active.contains("filter \"web\""), "the box is closed");
        assert!(!active.contains("enter applies"), "and its keys are gone");
        assert!(active.contains("2 of 6 in the flock"), "still narrowed");
        assert!(active.contains("/ edit") && active.contains("esc clear"));

        // "A query nothing matches. The table names the query rather than
        //  claiming the flock is empty, and the title keeps the flock's real
        //  size on screen."
        let none = render_text(&scene(Scene::FilterNoMatch).1);
        assert!(none.contains("no sheep's name contains \"zzz\""));
        assert!(!none.contains("the flock is empty"));
        assert!(none.contains("0 of 6 in the flock"));

        // "sysinfo reports this platform unsupported. The strip says so and
        //  keeps the flock's own totals, which lookout can always compute."
        let unknown = render_text(&scene(Scene::HostUnknown).1);
        assert!(unknown.contains("host  usage is not available on this platform"));
        assert!(
            unknown.contains("flock cpu"),
            "the half lookout can compute survives"
        );

        // "The detail pane with a lamb list: how many descendants the
        //  shepherd's walk found, how old that reading is, and each lamb's
        //  pid and executable name. The stamp sits before the list so a
        //  narrow terminal truncates lambs rather than the caveat."
        let lambs = render_text(&scene(Scene::Lambs).1);
        assert!(lambs.contains("lambs  3 parent-pid descendants, read "));
        assert!(lambs.contains("48220 node"), "each lamb's pid and name");
        let line = lambs
            .lines()
            .find(|line| line.starts_with("lambs  "))
            .expect("the lamb line");
        assert!(
            line.find("read ").unwrap() < line.find("48220").unwrap(),
            "the stamp comes before the list"
        );

        // "The same pane on a stopped sheep. The shepherd had no pid to walk
        //  from and left the field unset rather than empty, and the line
        //  says which of the two it is looking at rather than reporting none
        //  found."
        let unknown = render_text(&scene(Scene::LambsUnknown).1);
        assert!(unknown.contains("lambs  this sheep is not running, so there is no tree to walk"));
        assert!(
            !unknown.contains("none found"),
            "which is the other sentence"
        );
        assert!(unknown.contains("sheep 4  cron"), "on the stopped sheep");

        // "`R` pressed with the gate open. Nothing has been sent: the bar
        //  asks a question naming the verb and the exact sheep, and `api` is
        //  still online in the table behind it."
        let confirm = render_text(&scene(Scene::Confirm).1);
        assert!(confirm.contains("restart api (id 2)? enter confirms, any other key cancels"));
        assert!(confirm.contains("control enabled"), "the gate is open");
        assert!(
            row_for(&confirm, "api").is_some_and(|row| row.contains("online")),
            "nothing was sent, so api is still online: {confirm:?}"
        );

        // "Enter pressed. The request is out and nothing on the table has
        //  changed, because nothing the shepherd has said has changed:
        //  `api` is still online and the cursor has not moved."
        let acting = render_text(&scene(Scene::Acting).1);
        assert!(acting.contains("restart api (id 2): sent, waiting for the shepherd"));
        assert!(
            row_for(&acting, "api").is_some_and(|row| row.starts_with('>')),
            "the table is untouched: the marker is still on api"
        );
        assert!(
            row_for(&acting, "api").is_some_and(|row| row.contains("online")),
            "and the row still says what the shepherd last said"
        );

        // "The shepherd answered. The bar says what it did, in the non-grave
        //  style a refusal does not get, and the table shows the row the
        //  reply carried rather than waiting for the next poll."
        let accepted = render_text(&scene(Scene::ActionAccepted).1);
        assert!(accepted.contains("restart api (id 2): the shepherd restarted it"));
        assert!(
            row_for(&accepted, "api").is_some_and(|row| row.contains("48299")),
            "the reply's own row reached the table without waiting for a poll"
        );

        // "The shepherd refused while the request was out, and its own
        //  sentence is forwarded rather than rewritten. The sheep has left
        //  the flock in the listing behind it, so the table is one row
        //  shorter and the cursor has moved to the row below."
        let refused = render_text(&scene(Scene::ActionRefused).1);
        assert!(refused.contains("restart api (id 2): selector matched no registered sheep"));
        assert!(
            !refused.contains("NotFound"),
            "no Rust identifiers on the bar"
        );
        assert!(refused.contains("5 in the flock"), "one row shorter");
        assert!(
            row_for(&refused, "api").is_none(),
            "api is the row that went"
        );
        assert!(
            marked_row_name_starts_with(&refused, "billing"),
            "and the cursor has moved to the row below: {refused:?}"
        );

        // "An action key pressed while the link is coming back. The refusal
        //  names the same reconnect attempt the banner above it does, rather
        //  than the exhausted-ladder sentence."
        let offline = render_text(&scene(Scene::ActionRefusedOffline).1);
        assert_eq!(
            offline.matches("reconnecting (attempt 3)").count(),
            2,
            "the banner and the refusal under it agree, rather than one \
             saying reconnecting and the other saying gone: {offline:?}"
        );
        assert!(
            !offline.contains("nothing left to ask"),
            "the ladder has not run out yet, so the refusal must not claim it has: {offline:?}"
        );

        // The one frame in the gallery whose left slot is empty while the
        // gate is open, which makes it the only one that shows the control
        // hint.
        let lambs_bar = render_text(&scene(Scene::Lambs).1);
        for key in ["x stop", "R restart", "L reload"] {
            assert!(lambs_bar.contains(key), "the control hint names {key}");
        }
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
        assert_eq!(Scene::ALL.len(), 24);
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
        assert!(
            ten_minutes.lines().any(|line| line.starts_with("lambs  ")),
            "the frozen frame has a lamb line for the comparison above to cover"
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
    /// cargo test -p shep --lib --all-features -- --ignored write_the_gallery
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
