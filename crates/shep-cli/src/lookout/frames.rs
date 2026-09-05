//! Renders a `Buffer` to plain text or ANSI, and holds the scene list the
//! pinned snapshots and the gallery share.
//!
//! `docs/lookout/frames.txt` and `docs/lookout/frames.ansi` are generated
//! from this module's output, doubling as a rendered layout reference.
//!
//! Gated at the `mod` declaration with `#[cfg(test)]` rather than
//! `pub mod`: `lib.rs` exposes only three entry points, so an ordinary
//! `pub mod` here is unreachable from outside the crate and fails
//! `dead_code`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use shep_client::RequestError;
use shep_core::protocol::{
    DogSource, ExitInfo, Lamb, ProcessInfo, Response, RpcError, RpcErrorCode,
};
use shep_core::status::ProcStatus;

use super::app::{ActionVerb, App, Control, KeyPress, Msg, RowKey, Sent, SettingsRow};
use super::source::HostSample;
use super::tail::{Stream, Tail, TailLine};
use super::theme::Palette;
use super::view::{body_rows, draw};
use crate::commands::settings::{
    DogView, ScalarView, SettingField, SettingsSnapshot, load_settings,
};
use crate::commands::shep_toml::ShepToml;
use crate::style::{StyleLevel, StyleSource};

/// One rendered buffer as plain text: one line per row, trailing spaces
/// kept, no escapes.
///
/// Trailing spaces stay because a frame is a fixed-size grid: trimming
/// would make a right-aligned cell look like it moved.
///
/// Cells are read by their rendered symbol, not by byte length, so a
/// multi-byte cell round-trips exactly as drawn. Indexed with
/// `Buffer[(x, y)]`, not the deprecated `Buffer::get`.
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
/// Every line ends with a reset before its newline, so an unreset colour
/// does not bleed into the rest of the file.
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
/// Foreground only: no pane uses bold, reverse, or any other modifier, since
/// the selected row is a marker character rather than a style.
/// `no_scene_uses_a_modifier_the_ansi_renderer_would_drop` pins it.
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
    /// Three instances of one app under a group header, with the cursor on
    /// the header.
    Grouped,
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
    /// The settings screen on a fresh `$SHEP_HOME`: only `[interpreters]` on
    /// disk, so every scalar reads its compiled default. The state most
    /// operators open the screen in.
    SettingsFresh,
    /// The settings screen with some scalars declared, so `shep.toml` and
    /// the default sit side by side.
    SettingsSet,
    /// The settings screen with a `[daemon]` confirm armed, naming the
    /// variable and the flag it cannot see.
    SettingsConfirm,
    /// The settings screen with the `socket` editor open mid-path.
    SettingsTyping,
    /// The settings screen's dogs table, showing the drift it exists to
    /// make visible.
    SettingsDogs,
    /// The settings screen on a narrow terminal, where both of its tables
    /// have dropped a column.
    SettingsNarrow,
    /// The settings screen on a terminal too short to hold every row, with
    /// the cursor on the last one, so the view has scrolled.
    SettingsShort,
}

impl Scene {
    /// Every scene, in the order they appear in the gallery.
    pub const ALL: &'static [Self] = &[
        Self::HealthyWide,
        Self::Errored,
        Self::Grouped,
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
        Self::SettingsFresh,
        Self::SettingsSet,
        Self::SettingsConfirm,
        Self::SettingsTyping,
        Self::SettingsDogs,
        Self::SettingsNarrow,
        Self::SettingsShort,
    ];

    /// The snapshot name and the gallery heading.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::HealthyWide => "healthy_wide",
            Self::Errored => "errored",
            Self::Grouped => "grouped",
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
            Self::SettingsFresh => "settings_fresh",
            Self::SettingsSet => "settings_set",
            Self::SettingsConfirm => "settings_confirm",
            Self::SettingsTyping => "settings_typing",
            Self::SettingsDogs => "settings_dogs",
            Self::SettingsNarrow => "settings_narrow",
            Self::SettingsShort => "settings_short",
        }
    }

    /// One sentence saying what this frame is for, printed above it in the
    /// gallery.
    ///
    /// `every_scene_shows_the_thing_it_is_named_for` pins each clause: a
    /// caption may not claim what the frame is not asserted to show.
    #[must_use]
    pub const fn caption(self) -> &'static str {
        match self {
            Self::HealthyWide => {
                "All three panes at 120x30: the host strip under the title, the detail pane and the bleats feed under the table. `>` marks the selected sheep, and every pane below the table describes it."
            }
            Self::Errored => {
                "One errored, one waiting to restart, one stopped, with the selection parked on the errored sheep. Each row's own STATUS cell is the only coloured cell in that row, and EXIT carries why each of the three stopped: a code for the two that crashed, a signal name for the one shep stopped itself."
            }
            Self::Grouped => {
                "Three instances of one app under a group header, with the cursor parked on the header. The header sums their restarts, CPU and memory and takes the SHORTEST of their uptimes, so a group reads as time since the app was last disturbed rather than as the age of its luckiest instance. The detail pane repeats that rollup and says lambs are per-instance; the feed will not guess which instance to tail."
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
            Self::SettingsFresh => {
                "The settings screen on a fresh $SHEP_HOME: shep.toml holds only [interpreters], so every scalar's SOURCE column reads `the default` and its VALUE is the compiled fallback. The state most operators open this screen in."
            }
            Self::SettingsSet => {
                "The settings screen with some scalars declared. `shep.toml` and `the default` sit side by side in the SOURCE column, so what the operator wrote and what shep assumes are both visible at once."
            }
            Self::SettingsConfirm => {
                "A [daemon] confirm armed on log_level, naming the variable and the flag it cannot see: SHEP_LOG_LEVEL and --log-level, and the shep daemon reload the edit needs."
            }
            Self::SettingsTyping => {
                "The socket editor open mid-path. The status bar names the field being typed, not the dashboard's own filter box, which is what it showed here before the fix this frame now pins."
            }
            Self::SettingsDogs => {
                "The dogs table showing the drift it exists to reveal: otel running while disabled in the file, ledger enabled and absent, and bark enabled, running, and silent."
            }
            Self::SettingsNarrow => {
                "The same screen at 45 columns. Both of its tables have dropped a column rather than clipping: the scalar rows have lost the apply cost and kept SOURCE, and the dogs table has lost SOURCE and kept RUNNING. Each keeps whichever half is not said anywhere else."
            }
            Self::SettingsShort => {
                "The same screen at 14 rows, which is fewer than it has to draw. The cursor is on the last dog, so the view has scrolled to reach it and `... 5 above` says how much is off the top. The scroll is counted in LINES rather than in rows: a section header and the dogs caption cost the same height a row does."
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
            | Self::Lambs
            | Self::SettingsConfirm
            | Self::SettingsTyping => Control::Allowed,
            _ => Control::ReadOnly,
        }
    }

    /// The terminal size this scene is rendered at.
    #[must_use]
    pub const fn size(self) -> (u16, u16) {
        match self {
            Self::Empty => (100, 28),
            // 51: `columns_for` runs on `width - GUTTER` (the two-column
            // selection marker), so 51 - GUTTER = 49, the `NO_MEM` tier:
            // four columns gone, CPU and UPTIME still there.
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
            // 180: the log_level confirm's sentence runs to 170 columns,
            // past every other scene's width. Narrower would truncate the
            // flag it shows.
            Self::SettingsConfirm => (180, 30),
            // 45: the middle tier of both `SCALAR_TIERS` and `DOG_TIERS`,
            // so both tables have dropped one column without losing a row.
            Self::SettingsNarrow => (45, 24),
            // 14 rows: twelve of body, against a screen that wants
            // eighteen lines. Short enough that the cursor cannot be
            // reached without scrolling, tall enough that what survives is
            // a legible section rather than a single row.
            Self::SettingsShort => (120, 14),
            // HealthyWide, Errored, Grouped, Retrying, Frozen, Refused, FeedGap,
            // FeedMissing, HostUnknown, Lambs, LambsUnknown: every scene that
            // carries all three optional panes at their ordinary rows.
            _ => (120, 30),
        }
    }
}

/// Builds one scene and returns its label with the buffer it drew into.
///
/// Renders at ten minutes of dashboard age, the same age the pinned
/// snapshots and `docs/lookout/frames.txt` use.
#[must_use]
pub fn scene(which: Scene) -> (&'static str, Buffer) {
    (which.label(), scene_with(which, Duration::from_secs(600)))
}

/// Parks the gallery's cursor on the sheep with id `id`.
///
/// Walks by name since `SelectDown` moves one visible row and the table
/// reads by name, not id.
///
/// # Panics
///
/// If `id` is not in the flock, or is hidden by a filter.
#[track_caller]
fn select_id(app: &mut App, id: u32) {
    select_row(app, &RowKey::Sheep(id));
}

/// Parks the gallery's cursor on `name`'s group header.
///
/// # Panics
///
/// If `name` has no group header: an app with one instance, or an
/// instance that reports no slot.
#[track_caller]
fn select_group(app: &mut App, name: &str) {
    select_row(app, &RowKey::Group(name.to_string()));
}

/// Walks the cursor down until it lands on `key`.
///
/// Budgeted by [`App::visible_rows`]'s length, not the flock count: a
/// grouped app's header adds a visible row beyond its own slots.
///
/// # Panics
///
/// If `key` is not a visible row.
#[track_caller]
fn select_row(app: &mut App, key: &RowKey) {
    for _ in 0..=app.visible_rows().len() {
        if app.selected().as_ref() == Some(key) {
            return;
        }
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    panic!("the gallery cannot park its cursor on {key:?}");
}

/// One scene, `age` after its opening snapshot.
///
/// Deterministic: a forced palette, an explicit `Instant` advanced by exact
/// `Duration`s, and a literal frozen timestamp, so the gallery never
/// depends on this machine's clock or environment.
///
/// `age` exists for
/// `the_frozen_frame_does_not_move_however_long_the_link_stays_gone`,
/// which renders the frozen scene at two ages and checks for identical
/// frames.
#[must_use]
fn scene_with(which: Scene, age: Duration) -> Buffer {
    use std::ffi::OsStr;

    let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
    let t0 = Instant::now();
    let mut app = App::new(palette, which.control(), "/home/ada/.shep".to_string(), t0);

    let flock = match which {
        Scene::Empty => Vec::new(),
        // The only flock in the gallery whose rows carry a slot, and so the
        // only one that draws a group header at all. `api` is here so the
        // frame shows a grouped app beside an ungrouped one rather than
        // implying every app gets a header.
        Scene::Grouped => vec![
            instance(0, "web", 0, 0, 3.4, 182 << 20, 4_512_000),
            // The youngest of the three, and the one carrying restarts: the
            // group row's uptime is a minimum and its restarts are a sum.
            instance(1, "web", 1, 2, 2.9, 178 << 20, 300_000),
            instance(2, "web", 2, 1, 3.1, 180 << 20, 9_000_000),
            sheep(
                3,
                "api",
                ProcStatus::Online,
                Some(48_219),
                1,
                Some(7.1),
                Some(241 << 20),
                Some("edge"),
            ),
        ],
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
        // Two dog processes: `otel` up and healthy, `bark` up but never
        // handshook. `ledger` has no row here, which is what "enabled and
        // absent" means in the settings snapshot below.
        Scene::SettingsDogs | Scene::SettingsNarrow | Scene::SettingsShort => vec![
            dog_sheep(90, "otel", None),
            dog_sheep(91, "bark", Some(false)),
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

    // Selects `api` (id 2) so the panes below describe a fixed sheep,
    // walked by id since the table sorts by name. Skipped where there is
    // no flock, no pane below the table, or the cursor belongs elsewhere
    // (`Grouped`, and the three settings scenes with no id 2).
    if !matches!(
        which,
        Scene::Empty
            | Scene::Narrow
            | Scene::TooNarrow
            | Scene::TableOnly
            | Scene::Grouped
            | Scene::SettingsDogs
            | Scene::SettingsNarrow
            | Scene::SettingsShort
    ) {
        select_id(&mut app, 2);
    }

    // Onto `web`'s group header, which is the one row in the gallery that is
    // not a sheep. Every pane below the table renders its own group state
    // from it: the rollup line, the per-instance lamb sentence, and the
    // feed's refusal to pick an instance to tail.
    if which == Scene::Grouped {
        select_group(&mut app, "web");
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
                app.update(Msg::Key(KeyPress::TextChar(typed)));
            }
            if which == Scene::FilterActive {
                app.update(Msg::Key(KeyPress::TextApply));
            }
        }
        _ => {}
    }

    // Every live scene gets a host sample, frozen included: without a
    // baseline sample the strip would read "not read yet" regardless of
    // the freeze guard. `Scene::Frozen` below sends a second, older
    // sample that exercises the guard itself.
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

    // Applied while the link is still `Live`: `on_lambs` refuses once it
    // is `Lost`, the same guard `Msg::Bleats` carries.
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

    // `cron` (id 4) has no pid, so the shepherd's walk never ran and
    // `lambs_for(4)` stays `None`.
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

    // `Msg::Host` and the `SelectDown`s run before `Msg::Frozen`: the
    // reducer refuses both once frozen.
    match which {
        Scene::Retrying => {
            app.update(Msg::Retrying { attempt: 3 });
        }
        Scene::Frozen => {
            app.update(Msg::Frozen {
                at_local: "2026-08-14 14:32:07".to_string(),
            });
            // Sent after `Msg::Frozen`, with a load average that varies
            // with `age`: the guard's refusal keeps
            // `the_frozen_frame_does_not_move_however_long_the_link_stays_gone`
            // byte-identical across ages.
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

    // The last tick, `age` after the opening snapshot: advances the
    // uptime column on a live scene, and does nothing on the frozen one,
    // since the reducer stops accepting `now` once the link is lost.
    app.update(Msg::Tick { now: t0 + age });

    // Applied after the last tick: `scene()` renders at `age` = 600s past
    // `CONFIRM_EXPIRY` (10s), so an armed confirm built before the tick
    // would already show expired.
    match which {
        Scene::ActionRefusedOffline => {
            // The link must stop being live before the key is pressed, or
            // `arm` would accept it.
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
                        target: RowKey::Sheep(2),
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
                        target: RowKey::Sheep(2),
                        name: "api".to_string(),
                    },
                    result: Err(RequestError::Rpc(RpcError {
                        code: RpcErrorCode::NotFound,
                        message: "selector matched no registered sheep".to_string(),
                        daemon_version: None,
                    })),
                });
            }
        }
        _ => {}
    }

    // Applied last, for the same reason: `SettingsConfirm` arms a
    // candidate that expires on the same `CONFIRM_EXPIRY`, so it must be
    // armed after the tick at `age`.
    match which {
        Scene::SettingsFresh => {
            // A fresh document, not a hand-edited snapshot: first run
            // leaves only `[interpreters]`, and `load_settings` is the
            // same reader `run_ui` calls.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("shep.toml");
            ShepToml::edit(&path, ShepToml::write_starter_interpreters).unwrap();
            let snapshot = load_settings(
                &path,
                std::path::Path::new("/home/ada/.shep/run/shep.sock"),
                (StyleLevel::Full, StyleSource::Default),
            )
            .unwrap();
            app.update(Msg::Key(KeyPress::Settings));
            app.update(Msg::Settings {
                result: Ok(snapshot),
            });
        }
        Scene::SettingsSet => {
            app.update(Msg::Key(KeyPress::Settings));
            app.update(Msg::Settings {
                result: Ok(settings_snapshot_for_gallery()),
            });
        }
        Scene::SettingsConfirm => {
            app.update(Msg::Key(KeyPress::Settings));
            app.update(Msg::Settings {
                result: Ok(settings_snapshot_for_gallery()),
            });
            // The cursor already sits on `log_level`, `Settings::rows`'s
            // first row, so arming it needs no `SelectDown` at all.
            app.update(Msg::Key(KeyPress::Cycle));
        }
        Scene::SettingsTyping => {
            app.update(Msg::Key(KeyPress::Settings));
            app.update(Msg::Settings {
                result: Ok(settings_snapshot_for_gallery()),
            });
            move_settings_cursor_to(&mut app, SettingField::Socket);
            // Opens the editor, seeded with the on-disk value.
            app.update(Msg::Key(KeyPress::Confirm));
            // Trims the seeded value back to a partial path, so the frame
            // shows the editor genuinely mid-type rather than holding the
            // whole, untouched value it opened with.
            for _ in 0..8 {
                app.update(Msg::Key(KeyPress::TextBackspace));
            }
        }
        Scene::SettingsDogs | Scene::SettingsNarrow => {
            app.update(Msg::Key(KeyPress::Settings));
            app.update(Msg::Settings {
                result: Ok(settings_snapshot_with_dog_drift()),
            });
        }
        Scene::SettingsShort => {
            app.update(Msg::Key(KeyPress::Settings));
            app.update(Msg::Settings {
                result: Ok(settings_snapshot_with_dog_drift()),
            });
            // Onto the last row, which is the one a body this short cannot
            // reach without scrolling.
            app.update(Msg::Key(KeyPress::SelectLast));
        }
        _ => {}
    }

    let (width, height) = which.size();
    // The same call `run_ui` makes before every draw. Without it a gallery
    // frame renders with `Viewport::rows` still zero, which means unlimited
    // -- so every guard on a scrolled screen sat on a branch no snapshot
    // here exercised, and the settings screen shipped drawing its cursor
    // off the bottom of a short terminal with seven green frames behind it.
    app.note_body_rows(body_rows(Rect::new(0, 0, width, height)));
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(&app, frame)).unwrap();
    terminal.backend().buffer().clone()
}

/// The bleats feed each scene is given, before `Msg::Bleats` carries it in.
///
/// Most scenes: six ordinary `Stream::Out` lines, no missed lines or
/// bytes.
///
/// `FeedGap`: thirty lines, `missed_lines: 500`, `missed_bytes:
/// 4_012_000`, so the header reads both a dropped-lines and a
/// never-read-bytes count.
///
/// `FeedMissing`: no lines, no counts, and a `note` mirroring
/// [`super::tail::read`]'s wording for a log file that was never created.
fn feed_for(which: Scene) -> Tail {
    match which {
        // Mirrors `run_ui`: an empty flock has no selected row, so no
        // `tail::read` call, just the pane's own "no sheep is selected"
        // header.
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

/// One row's worth of shepherd reply, spelled out so each scene reads as
/// a plausible flock rather than six copies of one sheep.
///
/// The two log paths are derived from `name` and `id` rather than taken
/// as parameters: this already carries
/// `#[allow(clippy::too_many_arguments)]` at eight.
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
        .out_file(Some(format!("/home/ada/.shep/logs/{name}-{id}-out.log")))
        .err_file(Some(format!("/home/ada/.shep/logs/{name}-{id}-err.log")))
        // Derived from `status`, not a ninth parameter: a sheep that is
        // not running always has a reason it stopped, so no scene can
        // depict one with a blank EXIT column.
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

/// One instance of a clustered app: the row a shepherd reports for slot
/// `slot` of `name`.
///
/// Takes an explicit `uptime_ms` rather than deriving it from the id like
/// [`sheep`] does: a group row's uptime is the minimum across its
/// members, and grouping should not depend on id arithmetic.
fn instance(
    id: u32,
    name: &str,
    slot: u32,
    restarts: u32,
    cpu: f32,
    memory: u64,
    uptime_ms: u64,
) -> ProcessInfo {
    ProcessInfo::builder(id, name, ProcStatus::Online)
        .instance(Some(slot))
        .pid(Some(48_400 + id))
        .restarts(restarts)
        .uptime_ms(uptime_ms)
        .cpu_percent(Some(cpu))
        .memory_bytes(Some(memory))
        .fold(Some("edge".to_string()))
        .out_file(Some(format!("/home/ada/.shep/logs/{name}-{id}-out.log")))
        .err_file(Some(format!("/home/ada/.shep/logs/{name}-{id}-err.log")))
        .build()
}

/// The row `ActionAccepted`'s reply carries: `api` at id 2, restarted.
///
/// Pid 48299, not the listing's 48219: a matching pid would pass whether
/// the reply's row was upserted or silently ignored.
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

/// One dog process: `id` and `name` as `dog_rows`'s join key. `handshook`
/// carries the three-state signal a real listing does: `None` reads
/// `online`, `Some(false)` reads `silent`, per
/// [`crate::vocabulary::Reported::of`].
fn dog_sheep(id: u32, name: &str, handshook: Option<bool>) -> ProcessInfo {
    ProcessInfo::builder(id, name, ProcStatus::Online)
        .pid(Some(90_000 + id))
        .dog(Some(DogSource::BuiltIn))
        .handshook(handshook)
        .build()
}

/// Walks the settings screen's cursor onto `field`'s row with real
/// `SelectDown` keypresses.
///
/// # Panics
///
/// If the settings screen is not open.
#[track_caller]
fn move_settings_cursor_to(app: &mut App, field: SettingField) {
    let target = app
        .settings()
        .expect("the settings screen must already be open")
        .rows()
        .iter()
        .position(|row| *row == SettingsRow::Scalar(field))
        .expect("field is one of the six scalar rows Settings::rows always carries");
    for _ in 0..target {
        app.update(Msg::Key(KeyPress::SelectDown));
    }
}

/// A settings snapshot with `log_level`, `socket`, `max_cron_sleep` and
/// `style_level` declared in `shep.toml`, and `log_json`/`allow_control`
/// left at their compiled defaults: the mixed state
/// [`Scene::SettingsSet`] shows, reused by [`Scene::SettingsConfirm`] and
/// [`Scene::SettingsTyping`].
fn settings_snapshot_for_gallery() -> SettingsSnapshot {
    let config = |value: &str| ScalarView {
        value: value.to_string(),
        source: StyleSource::Config,
    };
    let default = |value: &str| ScalarView {
        value: value.to_string(),
        source: StyleSource::Default,
    };
    SettingsSnapshot {
        log_level: config("warn"),
        log_json: default("false"),
        socket: config("/home/ada/.shep/run/shep.sock"),
        max_cron_sleep: config("30s"),
        allow_control: default("false"),
        style_level: config("full"),
        // The document declares it, so the file and resolved value agree.
        style_level_in_file: Some("full".to_string()),
        dogs: vec![
            DogView {
                name: "bark".to_string(),
                enabled: false,
                adopted_path: None,
            },
            DogView {
                name: "metrics".to_string(),
                enabled: true,
                adopted_path: None,
            },
        ],
    }
}

/// [`settings_snapshot_for_gallery`]'s scalars, with `dogs` replaced by
/// the three-way drift [`Scene::SettingsDogs`] shows: `otel` running
/// while the file disables it, `ledger` enabled and absent from the
/// flock, and `bark` enabled with `handshook: Some(false)`, so the join
/// reads it `silent`.
fn settings_snapshot_with_dog_drift() -> SettingsSnapshot {
    SettingsSnapshot {
        dogs: vec![
            // Both carry a real path: a non-built-in dog with
            // `adopted_path: None` is not a row `load_settings` can
            // produce from a document shep wrote.
            DogView {
                name: "otel".to_string(),
                enabled: false,
                adopted_path: Some(PathBuf::from("/usr/local/bin/shep-otel")),
            },
            DogView {
                name: "ledger".to_string(),
                enabled: true,
                adopted_path: Some(PathBuf::from("/opt/ledger/bin/dog")),
            },
            DogView {
                name: "bark".to_string(),
                enabled: true,
                adopted_path: None,
            },
        ],
        ..settings_snapshot_for_gallery()
    }
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

frames.ansi is the same thirty-two frames with colour; read it with `less -R`.

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

The last six frames are the settings screen, `s` from the dashboard. It owns
the whole body between the title and the status bar rather than sharing it
with the flock table, so a fresh $SHEP_HOME, some scalars declared, an armed
confirm, the socket editor mid-type, the dogs table's own drift and the same
screen at 45 columns each get a frame of their own.
";

#[cfg(test)]
mod tests {
    use super::*;

    /// Nine other tests read frames through this renderer: a regression
    /// here silently changes what they assert.
    #[test]
    fn the_plain_renderer_is_one_line_per_row_and_no_escapes() {
        let text = render_text(&scene(Scene::HealthyWide).1);
        assert_eq!(text.lines().count(), 30);
        assert!(!text.contains('\u{1b}'), "plain means plain");
        for line in text.lines() {
            assert_eq!(line.chars().count(), 120, "every row is the full width");
        }
    }

    /// An unreset colour bleeds into whatever prints next, which for a
    /// file read through `less -R` is the rest of the file.
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
    /// Strips the leading `>` selection marker first, so a marked and
    /// unmarked row share the same token index.
    ///
    /// The numeric-id guard on token 0 is load bearing: the status bar's
    /// own lines also open `{verb} {name} (id {id})`, so without it this
    /// could match the bar line instead of a table row.
    #[cfg_attr(windows, allow(dead_code))]
    fn row_for<'a>(frame: &'a str, name: &str) -> Option<&'a str> {
        frame.lines().find(|line| {
            let mut tokens = line.trim_start_matches('>').split_whitespace();
            tokens.next().is_some_and(|id| id.parse::<u32>().is_ok()) && tokens.next() == Some(name)
        })
    }

    /// The dogs table's own row lookup: unlike [`row_for`], a dog row opens
    /// with `mark` and a name, never a numeric id, so the same "first two
    /// tokens" shape does not apply.
    fn dog_row_for<'a>(frame: &'a str, name: &str) -> Option<&'a str> {
        frame
            .lines()
            .find(|line| line.trim_start_matches('>').split_whitespace().next() == Some(name))
    }

    /// Whether the marked row's name starts with `prefix`.
    ///
    /// Handles truncation: the NAME column's truncated string depends on
    /// terminal width, so `prefix` only needs to fit the eight-column
    /// floor `name_width` never shrinks below.
    #[cfg_attr(windows, allow(dead_code))]
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

    /// fails if a scene stops rendering what it is named for.
    ///
    /// Each caption clause in [`Scene::caption`] is pinned by one
    /// assertion here.
    #[test]
    /// `cfg(unix)`: one fixture carries a synthetic signalled exit, and
    /// `signal_label` resolves it against the running platform's table.
    /// Windows never sets a signal on `ExitOutcome`, so this arm only
    /// runs against a synthetic fixture like this one; the pinned
    /// artifacts under `docs/lookout/` are unix renderings for the same
    /// reason.
    #[cfg(unix)]
    #[allow(clippy::too_many_lines)] // thirty-two captions, each pinned clause by clause
    fn every_scene_shows_the_thing_it_is_named_for() {
        // HealthyWide: all three panes at 120x30.
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

        // Grouped: cursor on the group header, which rolls up restarts,
        // CPU and memory and takes the shortest uptime.
        let grouped = render_text(&scene(Scene::Grouped).1);
        assert!(
            grouped.contains("web \u{d7}3"),
            "the group header names the app and how many instances it has: {grouped:?}"
        );
        assert_eq!(
            grouped
                .lines()
                .filter(|line| line.contains("web \u{d7}3"))
                .count(),
            2,
            "one in the table, one in the detail pane, and nowhere else"
        );
        assert!(
            grouped
                .lines()
                .any(|line| line.starts_with('>') && line.contains("web \u{d7}3")),
            "the cursor is on the header, not on one of its slots: {grouped:?}"
        );
        assert!(
            row_for(&grouped, "api").is_some(),
            "an ungrouped app sits beside the grouped one: {grouped:?}"
        );
        let rollup = grouped
            .lines()
            .find(|line| line.starts_with("app web "))
            .expect("the detail pane's rollup line");
        // Summed restarts, CPU and memory; uptime is the minimum (300s
        // plus the 600s this frame renders at), not the oldest member's.
        assert!(rollup.contains("restarts 3"), "summed restarts: {rollup:?}");
        assert!(rollup.contains("cpu 9.4%"), "summed cpu: {rollup:?}");
        assert!(rollup.contains("mem 540.0M"), "summed memory: {rollup:?}");
        assert!(rollup.contains("uptime 15m"), "the shortest: {rollup:?}");
        assert!(
            !rollup.contains("2h 40m"),
            "not the longest, which is what a max or a first-member read would show: {rollup:?}"
        );
        assert!(
            grouped.contains("lambs  not shown for a group; select one instance"),
            "the detail pane says lambs are per-instance: {grouped:?}"
        );
        assert!(
            grouped.contains("bleats  web  follows one instance; select one to see its log"),
            "and the feed will not guess which instance to tail: {grouped:?}"
        );
        assert!(
            !grouped.contains("GET /healthz 200 3ms"),
            "with no instance's lines under that sentence: {grouped:?}"
        );

        // Empty: each of the three panes gives its own reason.
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

        // Narrow: 51 columns drops FOLD, EXIT, RESTARTS, PID and MEM but
        // keeps CPU and UPTIME.
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

        // TooNarrow: below the floor, refuses rather than overlapping.
        let too_narrow = render_text(&scene(Scene::TooNarrow).1);
        let mut lines = too_narrow.lines();
        assert_eq!(lines.next().unwrap().trim_end(), "too small");
        assert_eq!(lines.next().unwrap().trim_end(), "need 33x6");

        // FeedGap: dropped lines and never-read bytes, counted separately.
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

        // FeedMissing: names the cause rather than sitting blank.
        let missing = render_text(&scene(Scene::FeedMissing).1);
        assert!(missing.contains("has not written a log in this $SHEP_HOME"));

        // NoDetail: the detail pane is the first to go at 20 rows.
        let no_detail = render_text(&scene(Scene::NoDetail).1);
        assert!(
            no_detail.contains("bleats  api"),
            "the feed stayed, on the selection"
        );
        assert!(no_detail.contains("host  load"), "and so did the strip");
        // The log-path prefix is the detail pane's alone: the feed's body
        // lines are tagged `out  ` too, but carry log text, not a path.
        assert!(
            !no_detail.contains("out  /home/ada/.shep/logs/"),
            "the detail pane went"
        );

        // TableOnly: 12 rows, no optional panes.
        let table_only = render_text(&scene(Scene::TableOnly).1);
        assert!(!table_only.contains("host  load"));
        assert!(!table_only.contains("bleats  "));
        assert!(table_only.contains("STATUS"), "the table is still there");

        // Cramped: 33 columns, the narrowest terminal that draws.
        let cramped = render_text(&scene(Scene::Cramped).1);
        assert!(cramped.contains('…'), "something truncated, visibly");
        // Not a row-width check, which `render_text` satisfies trivially:
        // "nothing overlaps" means each pane's marker appears exactly once.
        for marker in ["host  ", "bleats  ", "out  /home/ada/.shep/logs/"] {
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

        // Retrying: every pane keeps describing the last listing.
        let retrying = render_text(&scene(Scene::Retrying).1);
        assert!(retrying.contains("reconnecting"));
        assert!(
            retrying.contains("sheep 2  api"),
            "the detail pane is still up"
        );
        assert!(retrying.contains("host  load"), "and so is the strip");

        // Frozen: last known values stay, nothing keeps ticking.
        let frozen = render_text(&scene(Scene::Frozen).1);
        assert!(frozen.contains("the shepherd has died"));
        assert!(
            frozen.contains("host  load 2.31 4.10 3.88 / 10 cores"),
            "the strip kept its LAST values rather than blanking"
        );

        // Errored: selection parked on the errored sheep.
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
        // Only the ANSI rendering carries colour to check STATUS against.
        //
        // Asserted per row, not on the whole frame: `errored.contains("1")`
        // would pass on any digit anywhere, blank column included.
        let row_of = |name: &str| {
            errored
                .lines()
                .find(|line| line.contains(name))
                .unwrap_or_else(|| panic!("no row for {name}:\n{errored}"))
                .to_string()
        };
        for (name, want) in [
            ("api", "1"),
            // NAME truncates at this width, landing one syllable earlier.
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

        // Refused: `x` with actions gated off.
        let refused = render_text(&scene(Scene::Refused).1);
        assert!(refused.contains("--allow-control"));
        assert!(
            refused.contains("bleats  api"),
            "a refusal does not blank the screen"
        );

        // FilterEditing: mid-type, table already narrowed.
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

        // FilterActive: the same query, box closed.
        let active = render_text(&scene(Scene::FilterActive).1);
        assert!(active.contains("filter \"web\""), "the box is closed");
        assert!(!active.contains("enter applies"), "and its keys are gone");
        assert!(active.contains("2 of 6 in the flock"), "still narrowed");
        assert!(active.contains("/ edit") && active.contains("esc clear"));

        // FilterNoMatch: names the query rather than claiming empty.
        let none = render_text(&scene(Scene::FilterNoMatch).1);
        assert!(none.contains("no sheep's name contains \"zzz\""));
        assert!(!none.contains("the flock is empty"));
        assert!(none.contains("0 of 6 in the flock"));

        // HostUnknown: the strip keeps the flock's own totals.
        let unknown = render_text(&scene(Scene::HostUnknown).1);
        assert!(unknown.contains("host  usage is not available on this platform"));
        assert!(
            unknown.contains("flock cpu"),
            "the half lookout can compute survives"
        );

        // Lambs: the stamp sits before the list.
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

        // LambsUnknown: no pid to walk from, unset rather than empty.
        let unknown = render_text(&scene(Scene::LambsUnknown).1);
        assert!(unknown.contains("lambs  this sheep is not running, so there is no tree to walk"));
        assert!(
            !unknown.contains("none found"),
            "which is the other sentence"
        );
        assert!(unknown.contains("sheep 4  cron"), "on the stopped sheep");

        // Confirm: `R` pressed, nothing sent yet.
        let confirm = render_text(&scene(Scene::Confirm).1);
        assert!(confirm.contains("restart api (id 2)? enter confirms, any other key cancels"));
        assert!(confirm.contains("control enabled"), "the gate is open");
        assert!(
            row_for(&confirm, "api").is_some_and(|row| row.contains("online")),
            "nothing was sent, so api is still online: {confirm:?}"
        );

        // Acting: request out, table unchanged.
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

        // ActionAccepted: the reply's own row reaches the table at once.
        let accepted = render_text(&scene(Scene::ActionAccepted).1);
        assert!(accepted.contains("restart api (id 2): the shepherd restarted it"));
        assert!(
            row_for(&accepted, "api").is_some_and(|row| row.contains("48299")),
            "the reply's own row reached the table without waiting for a poll"
        );

        // ActionRefused: the shepherd's own sentence is forwarded as is.
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

        // ActionRefusedOffline: names the same reconnect attempt as the
        // banner above it, not the exhausted-ladder sentence.
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

        // The only frame with the left bar slot empty while the gate is
        // open, so it is the only one showing the control hint.
        let lambs_bar = render_text(&scene(Scene::Lambs).1);
        for key in ["x stop", "R restart", "L reload"] {
            assert!(lambs_bar.contains(key), "the control hint names {key}");
        }

        // SettingsFresh: a bare `shep.toml`, every scalar reads the default.
        let fresh = render_text(&scene(Scene::SettingsFresh).1);
        assert_eq!(
            fresh.matches("the default").count(),
            6,
            "all six scalars read the default: {fresh:?}"
        );
        assert!(
            !fresh.contains("shep.toml  "),
            "a fresh home has declared nothing: {fresh:?}"
        );

        // SettingsSet: `shep.toml` and the default sit side by side.
        let set = render_text(&scene(Scene::SettingsSet).1);
        assert!(set.contains("shep.toml"), "some scalars are declared");
        assert!(set.contains("the default"), "and some are not: {set:?}");

        // SettingsConfirm: names the env var and flag it cannot see.
        let confirm = render_text(&scene(Scene::SettingsConfirm).1);
        assert!(confirm.contains("shep daemon reload"), "got: {confirm:?}");
        assert!(confirm.contains("SHEP_LOG_LEVEL"), "got: {confirm:?}");
        assert!(confirm.contains("--log-level"), "got: {confirm:?}");

        // SettingsTyping: names the field being typed, not the filter box.
        let typing = render_text(&scene(Scene::SettingsTyping).1);
        assert!(
            typing.contains("editing socket"),
            "names the field being typed: {typing:?}"
        );
        assert!(
            !typing.contains("filter "),
            "must not read as the dashboard's own filter box: {typing:?}"
        );

        // SettingsDogs: the drift the table exists to reveal.
        let dogs = render_text(&scene(Scene::SettingsDogs).1);
        assert!(
            dog_row_for(&dogs, "otel")
                .is_some_and(|row| row.contains("no") && row.contains("online")),
            "otel: disabled in the file, running: {dogs:?}"
        );
        assert!(
            dog_row_for(&dogs, "ledger")
                .is_some_and(|row| row.contains("yes") && row.contains("not running")),
            "ledger: enabled, absent from the flock: {dogs:?}"
        );
        assert!(
            dog_row_for(&dogs, "bark")
                .is_some_and(|row| row.contains("yes") && row.contains("silent")),
            "bark: enabled, running, never handshook: {dogs:?}"
        );

        // SettingsNarrow: both tables drop a column rather than clip.
        let narrow = render_text(&scene(Scene::SettingsNarrow).1);
        assert!(
            narrow.contains("shep.toml"),
            "the scalar rows keep SOURCE: {narrow:?}"
        );
        assert!(
            !narrow.contains("needs shep daemon reload"),
            "and lose the apply cost: {narrow:?}"
        );
        assert!(
            dog_row_for(&narrow, "otel").is_some_and(|row| row.contains("online")),
            "the dogs table keeps RUNNING: {narrow:?}"
        );
        assert!(!narrow.contains("built-in"), "and loses SOURCE: {narrow:?}");

        // "The same screen at 14 rows, which is fewer than it has to draw.
        //  The cursor is on the last dog, so the view has scrolled to reach
        //  it and `... 5 above` says how much is off the top. The scroll is
        //  counted in LINES rather than in rows: a section header and the
        //  dogs caption cost the same height a row does."
        let short = render_text(&scene(Scene::SettingsShort).1);
        assert!(
            short.contains("... 5 above"),
            "the marker names how many rows are off the top: {short:?}"
        );
        assert!(
            short
                .lines()
                .any(|line| line.starts_with("> bark")
                    || line.starts_with('>') && line.contains("bark")),
            "the cursor's own row is drawn: {short:?}"
        );
        assert!(
            !short.contains("log_level"),
            "and the rows above it are the ones that went: {short:?}"
        );
        assert!(
            short.contains("[style]") && short.contains("[dogs]"),
            "what survives is whole sections, headers and all: {short:?}"
        );
    }

    /// Two grouped apps, four sheep, six visible rows: a `0..=flock_len()`
    /// budget only reaches index 4, short of the last row at index 5. One
    /// group would not show this, since its header lands the budget
    /// exactly on the last row.
    #[test]
    fn the_cursor_walk_budgets_by_visible_rows_not_by_sheep() {
        use std::ffi::OsStr;

        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, Some(OsStr::new("xterm-256color")), None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                instance(0, "web", 0, 0, 3.4, 182 << 20, 4_512_000),
                instance(1, "web", 1, 0, 2.9, 178 << 20, 4_512_000),
                instance(2, "api", 0, 0, 7.1, 241 << 20, 4_512_000),
                instance(3, "api", 1, 0, 6.8, 239 << 20, 4_512_000),
            ],
            at: t0,
        });
        assert_eq!(
            app.visible_rows().len(),
            6,
            "four sheep and two group headers"
        );

        // `web`'s second slot: the last visible row, and the one a
        // sheep-counted budget cannot reach.
        select_id(&mut app, 1);
        assert_eq!(app.selected(), Some(RowKey::Sheep(1)));
    }

    /// Only checks that every scene has a caption, since whether the
    /// caption is pinned by an assertion is not machine-checkable.
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
        // The literal 32 catches a scene added to the enum but not to
        // `ALL`, or the reverse; `labels.len()` would not, since `insert`
        // above already guarantees it.
        assert_eq!(Scene::ALL.len(), 32);
    }

    /// `sgr` renders foregrounds only, so a modifier would come out
    /// unstyled. The selection marker is a character rather than a
    /// reversed row for exactly this reason.
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

    /// Rendered twice at two different ages and compared to each other,
    /// not to the healthy scene, since a live-versus-frozen diff would
    /// pass either way. The live pair at the bottom catches a renderer
    /// that drops the uptime column entirely.
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

    /// Frame pins, not wire fixtures: re-accepting these after a layout
    /// change is expected, unlike the rule for shep-core's protocol
    /// snapshots.
    ///
    /// `cfg(unix)`: one fixture carries a synthetic signalled exit, and
    /// `signal_label` resolves it against the running platform's table.
    /// Windows never sets a signal on `ExitOutcome`, so this only runs
    /// against a synthetic fixture; the pinned artifacts under
    /// `docs/lookout/` are unix renderings for the same reason.
    #[cfg(unix)]
    #[test]
    fn frames_are_pinned() {
        for which in Scene::ALL {
            let (label, buffer) = scene(*which);
            insta::assert_snapshot!(label, render_text(&buffer));
        }
    }

    /// Writes `docs/lookout/frames.txt` and `docs/lookout/frames.ansi`.
    ///
    /// `#[ignore]`: writes into the repository, so it only runs on request.
    ///
    /// ```text
    /// cargo test -p shep --lib --all-features -- --ignored write_the_gallery
    /// ```
    ///
    /// Cannot rot: it renders the same `Scene::ALL` the pinned snapshots
    /// read, so a layout change reddens the ordinary suite first.
    #[test]
    #[ignore = "writes into docs/lookout; run it deliberately"]
    fn write_the_gallery() {
        // Absolute, derived from the manifest, so it lands in the same
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
