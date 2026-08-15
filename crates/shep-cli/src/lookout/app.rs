//! The lookout's state and its reducer: `Msg` in, `Effect` out.
//!
//! Everything about this module is chosen so it can be tested without a
//! terminal, a runtime, a socket, or a sleep:
//!
//! - **No I/O.** [`App::update`] is a synchronous function of `&mut self` and
//!   one [`Msg`]. Work that has to happen outside comes back out as an
//!   [`Effect`] for the caller in `super::mod` to run.
//! - **No terminal types.** Nothing here imports `ratatui` beyond
//!   [`super::theme::Palette`] (which is a style value, not a widget) or
//!   `crossterm` at all — `super::input` maps a `KeyEvent` to a [`KeyPress`]
//!   before it reaches this module.
//! - **No clock.** Every `Instant` arrives on the message
//!   ([`Msg::Tick`], [`Msg::Snapshot`]). A test asserts on uptime arithmetic
//!   exactly rather than sleeping and hoping.
//!
//! **The flock map is keyed by sheep id, and the poll is the truth.** The bus
//! is lossy by construction (`tokio::sync::broadcast` drops for a lagging
//! subscriber), so a flock view built from events alone WILL drift.
//! [`Msg::Event`] upserts for latency; [`Msg::Snapshot`] replaces the whole map
//! and wins every conflict. The cursor this module carries is a **selected
//! sheep id**, not a row index — the flock map is replaced wholesale every two
//! seconds, so an index cursor would silently point at a different sheep the
//! moment an earlier row is deleted. [`App::reseat`] is what puts the
//! selection back on a real sheep after the map changes; the viewport offset
//! the flock table scrolls to is derived from the selection rather than
//! stored beside it ([`super::view::flock::scroll_offset`]).

use core::fmt;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
use shep_core::status::ProcStatus;

use super::theme::Palette;

/// Whether this lookout may act on a sheep.
///
/// Default is [`Self::ReadOnly`], per Rin's ruling, mirroring the
/// `allow_control` precedent spec §9 sets for whistle. Turned on by
/// `--allow-control` or by `lookout.allow_control` in the KV store.
///
/// **This is a fat-finger catch, not a security boundary.** lookout runs as the
/// operator's own process under the operator's own uid; anyone who can run it
/// can run `shep stop`. The gate exists so a keystroke in a dashboard someone
/// is reading does not become an action they did not intend.
// Every public item below this point is wired together by `super::mod`'s
// `lookout` and `run_ui` — the real caller for `App`, `Msg` and friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Actions refuse. The default.
    ReadOnly,
    /// Actions are permitted — in 12a there are none, so they refuse with a
    /// different sentence.
    Allowed,
}

/// The keys lookout binds, named by what they mean rather than by which key
/// produces them.
///
/// A plain enum, rather than `crossterm::event::KeyEvent`, so this module and
/// its tests never touch a terminal crate: `super::input::map_key` does the
/// translation at the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    /// `q`, `Esc`, or `Ctrl-C`.
    Quit,
    /// `k` or `Up` — the selection moves up one row.
    SelectUp,
    /// `j` or `Down` — the selection moves down one row.
    SelectDown,
    /// `g` or `Home` — the first sheep in the flock.
    SelectFirst,
    /// `G` or `End` — the last one.
    SelectLast,
    /// `r` — poll now.
    Refresh,
    /// `x` — the one action key. Refuses in both control states in 12a; see
    /// the plan's design decision 2.
    Stop,
}

/// Everything that can change the dashboard.
#[derive(Debug, Clone)]
pub enum Msg {
    /// A `Request::ListFlock` reply landed. `at` is when it was received, and
    /// becomes every row's uptime anchor.
    Snapshot {
        /// The flock as the shepherd reported it.
        rows: Vec<ProcessInfo>,
        /// When the reply was received.
        at: Instant,
    },
    /// One frame off the bus.
    Event(BusEvent),
    /// This client's own receiver fell behind and discarded frames — the
    /// local half of the drop problem, distinct from
    /// [`BusEvent::Dropped`], which is the shepherd's own queue.
    BusLagged {
        /// How many frames this process lost.
        count: u64,
    },
    /// The link task is re-dialling; `attempt` is 1-based.
    Retrying {
        /// Which attempt is in flight.
        attempt: u32,
    },
    /// The link task reconnected and re-subscribed.
    Relinked,
    /// The reconnect ladder is exhausted. Everything on screen is now frozen.
    ///
    /// `at_local` is a pre-formatted local timestamp rather than an instant to
    /// format here: this module holds no clock and no formatter, and a frozen
    /// banner whose text is supplied is a banner a snapshot test can pin.
    Frozen {
        /// When the link was declared lost, already rendered for display.
        at_local: String,
    },
    /// One key.
    Key(KeyPress),
    /// The 1s heartbeat. `now` is what advances every running sheep's uptime.
    Tick {
        /// The current instant, read by the caller.
        now: Instant,
    },
    /// The terminal changed size; nothing to update but the frame is stale.
    Resize,
    /// One reading of the machine this lookout is running on, off the 1-second
    /// heartbeat. `None` means `sysinfo` does not support this platform —
    /// which is a real, expected case, not a failure.
    ///
    /// Refused once the link is lost: see this arm in [`App::update`].
    Host {
        /// What the sampler saw, or `None` on an unsupported platform.
        sample: Option<super::source::HostSample>,
    },
    /// One refresh of the selected sheep's log files, in answer to an
    /// [`Effect::RefreshFeed`] this reducer asked for.
    ///
    /// Returns [`Effect::None`] unconditionally; see its arm.
    Bleats {
        /// What the read found, including what it could not show.
        tail: super::tail::Tail,
    },
}

/// What the caller has to do after an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing.
    None,
    /// Ask the link task for a `ListFlock` now, rather than at the next tick.
    PollNow,
    /// Re-read the selected sheep's log files and hand the result back as
    /// [`Msg::Bleats`].
    ///
    /// The feed has no timer of its own. It rides this: a snapshot produces
    /// one (so the two-second listing, a drop repair, a lag repair and `r`
    /// all refresh it), and so does a selection that actually moved. See the
    /// phase plan's design decision 2.
    RefreshFeed,
    /// Leave.
    Quit,
}

/// The connection's state, as the dashboard reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// Connected and subscribed.
    Live,
    /// Re-dialling. `attempt` is 1-based and bounded by
    /// `super::link::RECONNECT_ATTEMPTS`.
    Retrying {
        /// Which attempt is in flight.
        attempt: u32,
    },
    /// The ladder is exhausted. Terminal: nothing moves this state, and the
    /// values on screen stay exactly as they were.
    Lost {
        /// When it was declared lost, already rendered for display.
        at_local: String,
    },
}

/// One sheep's row: what the shepherd said, and when it said it.
#[derive(Debug, Clone)]
pub struct Row {
    /// The shepherd's own snapshot of this sheep.
    pub info: ProcessInfo,
    /// When [`Self::info`] was received — the origin for this row's live
    /// uptime, so a value two seconds old is never rendered as current.
    pub anchor: Instant,
}

/// A short line the status bar shows instead of the key hints, cleared by the
/// next keypress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    text: String,
    /// True for a refusal or a damage report — the status bar picks
    /// [`Palette::refusal`] over [`Palette::attention`].
    grave: bool,
}

impl Notice {
    /// Whether this notice is a refusal or a damage report rather than an
    /// informational one.
    #[must_use]
    pub fn is_grave(&self) -> bool {
        self.grave
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// The whole dashboard's state.
#[derive(Debug)]
pub struct App {
    flock: BTreeMap<u32, Row>,
    /// Which sheep the detail pane and the bleats feed describe.
    ///
    /// An **id**, not an index. The flock map is replaced wholesale every two
    /// seconds, so an index survives a `shep delete` of an earlier row by
    /// silently pointing at a different sheep — and every pane below the table
    /// would then describe that different sheep with nothing on screen
    /// changing. `None` only for an empty flock.
    ///
    /// The viewport offset is derived from this rather than stored beside it
    /// ([`super::view::flock::scroll_offset`]), which is what makes a
    /// disagreement between a stored offset and a stored cursor impossible
    /// rather than merely unlikely.
    selected: Option<u32>,
    link: Link,
    notice: Option<Notice>,
    palette: Palette,
    control: Control,
    /// The `$SHEP_HOME` this lookout watches, for the title line. Held here
    /// rather than threaded through [`super::view::draw`]: it never changes for
    /// the life of the process, and a render function taking it as an argument
    /// would make every call site — including eight scene fixtures — carry it.
    home: String,
    /// The clock the view reads. Advanced by [`Msg::Tick`] — and deliberately
    /// NOT advanced once the link is [`Link::Lost`], which is what stops a
    /// frozen dashboard's uptime column from counting up for a sheep nothing
    /// can see.
    now: Instant,
    /// The last host reading, or `None` before the first heartbeat and on a
    /// platform `sysinfo` does not support. [`Self::host_unsupported`] tells
    /// the strip which of the two it is looking at.
    host: Option<super::source::HostSample>,
    /// True once a sample has come back `None` — the two `None`s mean
    /// different things and the strip says different sentences for them.
    host_unsupported: bool,
    /// The selected sheep's most recent output, as of the last refresh.
    /// [`Tail::default`](super::tail::Tail::default) before the first one —
    /// an empty, unlabelled tail, which
    /// [`super::view::bleats::feed_lines`] reads the same way it reads a
    /// sheep that has genuinely written nothing.
    feed: super::tail::Tail,
}

impl App {
    /// A dashboard with an empty flock, a live link, and no notice.
    #[must_use]
    pub fn new(palette: Palette, control: Control, home: String, now: Instant) -> Self {
        Self {
            flock: BTreeMap::new(),
            selected: None,
            link: Link::Live,
            notice: None,
            palette,
            control,
            home,
            now,
            host: None,
            host_unsupported: false,
            feed: super::tail::Tail::default(),
        }
    }

    /// Applies one message and reports what the caller must do next.
    pub fn update(&mut self, msg: Msg) -> Effect {
        match msg {
            Msg::Snapshot { rows, at } => {
                // A snapshot cannot arrive after a freeze — the link task has
                // ended by then, so there is nothing left to produce one. If
                // one does, it is a message from a task that should not exist,
                // and accepting it would silently un-freeze a dashboard whose
                // banner says otherwise.
                if matches!(self.link, Link::Lost { .. }) {
                    return Effect::None;
                }
                let previous = self.selected_index();
                self.flock = rows
                    .into_iter()
                    .map(|info| (info.id, Row { info, anchor: at }))
                    .collect();
                self.reseat(previous);
                // Unconditional, and NOT `if reseat(..)`: the paths on the
                // selected row may have changed even when the selection did
                // not, and this is the whole of the feed's cadence. See design
                // decision 2.
                Effect::RefreshFeed
            }
            Msg::Event(event) => self.on_event(event),
            Msg::BusLagged { count } => {
                self.notice = Some(Notice {
                    text: format!(
                        "lookout fell behind and lost {count} events; re-reading the flock"
                    ),
                    grave: false,
                });
                Effect::PollNow
            }
            Msg::Retrying { attempt } => {
                if !matches!(self.link, Link::Lost { .. }) {
                    self.link = Link::Retrying { attempt };
                }
                Effect::None
            }
            Msg::Relinked => {
                if !matches!(self.link, Link::Lost { .. }) {
                    self.link = Link::Live;
                }
                Effect::None
            }
            Msg::Frozen { at_local } => {
                self.link = Link::Lost { at_local };
                Effect::None
            }
            Msg::Tick { now } => {
                // The one line that keeps a frozen dashboard honest.
                if !matches!(self.link, Link::Lost { .. }) {
                    self.now = now;
                }
                Effect::None
            }
            Msg::Resize => Effect::None,
            Msg::Key(key) => self.on_key(key),
            Msg::Host { sample } => {
                // The one line that keeps a frozen dashboard honest, for the
                // second time. `Msg::Tick` stops advancing `now` once the link
                // is lost; this stops the strip for the same reason. A single
                // line ticking over on a screen whose banner says the values
                // are frozen is a contradiction on one frame.
                if matches!(self.link, Link::Lost { .. }) {
                    return Effect::None;
                }
                self.host_unsupported = sample.is_none();
                self.host = sample;
                Effect::None
            }
            // Always `Effect::None`. A reducer that answered its own feed
            // update with another refresh request would spin the UI task at
            // full tilt; the `let _ =` at the call site in `run_ui` is
            // deliberate rather than lazy.
            Msg::Bleats { tail } => {
                self.feed = tail;
                Effect::None
            }
        }
    }

    fn on_event(&mut self, event: BusEvent) -> Effect {
        match event {
            BusEvent::Process { event, info, .. } => {
                if matches!(event, ProcessEventKind::Delete) {
                    let previous = self.selected_index();
                    self.flock.remove(&info.id);
                    return if self.reseat(previous) {
                        Effect::RefreshFeed
                    } else {
                        Effect::None
                    };
                }
                // An upsert cannot orphan the selection — the row it names
                // either already existed (so the id is still in the map) or
                // is new (so it cannot be the one the selection pointed at).
                // The only case that needs a reseat is the flock going from
                // empty to non-empty, which is when `selected` is `None` and
                // ought not to be.
                let was_empty = self.flock.is_empty();
                let anchor = self.now;
                self.flock.insert(info.id, Row { info, anchor });
                if was_empty && self.reseat(None) {
                    return Effect::RefreshFeed;
                }
                Effect::None
            }
            // The shepherd's own outbound queue overflowed for this
            // subscriber. Deliberately worded differently from
            // `Msg::BusLagged` above: the two failures live on opposite ends of
            // the connection, and an operator cannot tell which end to
            // investigate if they read the same. `bleats` pins the identical
            // distinction.
            BusEvent::Dropped { count } => {
                self.notice = Some(Notice {
                    text: format!("the shepherd dropped {count} events; re-reading the flock"),
                    grave: false,
                });
                Effect::PollNow
            }
            // A notice, not an exit. `bleats` prints this and then leaves,
            // which is right for a follow; a standing dashboard that vanished
            // when the shepherd went down would take the last known state with
            // it. The link task will find the socket gone, climb the reconnect
            // ladder, and freeze if it runs out.
            BusEvent::DaemonShutdown => {
                self.notice = Some(Notice {
                    text: "the shepherd is shutting down".to_string(),
                    grave: true,
                });
                Effect::None
            }
            // `BusEvent` is `#[non_exhaustive]`: a variant a newer shepherd
            // added must not take the dashboard down, and must not be reported
            // as anything. `Dropped` and `DaemonShutdown` are NOT in this arm
            // — both are named variants this binary understands.
            _ => Effect::None,
        }
    }

    fn on_key(&mut self, key: KeyPress) -> Effect {
        self.notice = None;
        match key {
            KeyPress::Quit => Effect::Quit,
            // The one key that does I/O, and it refuses honestly once the
            // link task has ended: its poll receiver is gone by then, so an
            // `Effect::PollNow` would be a `try_send` into a closed channel
            // and the operator would get silence with no reason for it.
            KeyPress::Refresh => {
                if matches!(self.link, Link::Lost { .. }) {
                    self.notice = Some(Notice {
                        text: "the shepherd is gone — nothing left to ask".to_string(),
                        grave: true,
                    });
                    return Effect::None;
                }
                Effect::PollNow
            }
            KeyPress::SelectUp => self.select_by(-1),
            KeyPress::SelectDown => self.select_by(1),
            KeyPress::SelectFirst => self.select_at(0),
            KeyPress::SelectLast => self.select_at(self.flock.len().saturating_sub(1)),
            KeyPress::Stop => {
                // Both texts are literal. The design language's standing rule
                // is that nothing about damage gets charming, and a stop is
                // damage; the house rule says destructive operations and error
                // text stay plain.
                let text = match self.control {
                    Control::ReadOnly => "read-only: actions need --allow-control".to_string(),
                    Control::Allowed => "stop is not built yet".to_string(),
                };
                self.notice = Some(Notice { text, grave: true });
                Effect::None
            }
        }
    }

    /// Puts the selection back on a real sheep after the flock changed.
    ///
    /// `previous_index` is where the selection sat **before** the change, read
    /// while the old map was still in place. A selection whose id survived is
    /// left alone, whatever row it now occupies. One that did not falls to
    /// whatever occupies the same position, clamped to the last row — not to
    /// row 0, which would throw an operator back to the top of a
    /// two-hundred-sheep flock every time an unrelated sheep was deleted.
    ///
    /// Returns whether the selection changed, which is what decides between
    /// [`Effect::RefreshFeed`] and [`Effect::None`].
    fn reseat(&mut self, previous_index: Option<usize>) -> bool {
        let before = self.selected;
        if self.flock.is_empty() {
            self.selected = None;
            return before != self.selected;
        }
        if self.selected.is_some_and(|id| self.flock.contains_key(&id)) {
            return false;
        }
        let index = previous_index.unwrap_or(0).min(self.flock.len() - 1);
        self.selected = self.flock.keys().nth(index).copied();
        before != self.selected
    }

    /// Moves the selection by `delta` rows and reports whether it moved.
    ///
    /// Clamped rather than wrapping: wrapping a two-hundred-sheep flock from
    /// the last row to the first on one keypress loses the operator's place
    /// with nothing to undo it.
    fn select_by(&mut self, delta: isize) -> Effect {
        let Some(index) = self.selected_index() else {
            return Effect::None;
        };
        let next = index.saturating_add_signed(delta);
        self.select_at(next)
    }

    /// Selects the row at `index`, clamped to the flock, and reports whether
    /// that changed anything.
    ///
    /// `Effect::None` when it did not: [`Effect::RefreshFeed`] reads two files
    /// off disk, and a held `k` at the top of the flock must not do that once
    /// per keypress.
    fn select_at(&mut self, index: usize) -> Effect {
        if self.flock.is_empty() {
            return Effect::None;
        }
        let index = index.min(self.flock.len() - 1);
        let next = self.flock.keys().nth(index).copied();
        if next == self.selected {
            return Effect::None;
        }
        self.selected = next;
        // The cursor moved; whether anything is READ is a separate question.
        // A frozen dashboard re-reading live log files would put content on
        // screen newer than the banner saying the values are frozen, which is
        // the contradiction design decision 7 refuses for the host strip. The
        // detail pane still re-renders, from the frozen listing — that is data
        // already on the frame.
        if matches!(self.link, Link::Lost { .. }) {
            return Effect::None;
        }
        Effect::RefreshFeed
    }

    /// The flock, in id order.
    #[must_use]
    pub fn rows(&self) -> Vec<&Row> {
        self.flock.values().collect()
    }

    /// The selected sheep's id, or `None` for an empty flock.
    #[must_use]
    pub fn selected(&self) -> Option<u32> {
        self.selected
    }

    /// Which row of [`Self::rows`] the selection sits on.
    ///
    /// Derived every call rather than stored: see [`Self::selected`].
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.flock.keys().position(|key| *key == id)
    }

    /// The selected sheep's row, which the detail pane and the feed read.
    #[must_use]
    pub fn selected_row(&self) -> Option<&Row> {
        self.flock.get(&self.selected?)
    }

    /// The link state, as the status bar reports it.
    #[must_use]
    pub fn link(&self) -> &Link {
        &self.link
    }

    /// The current notice, if the last message left one.
    #[must_use]
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// The resolved palette.
    #[must_use]
    pub fn palette(&self) -> Palette {
        self.palette
    }

    /// Whether actions are permitted.
    #[must_use]
    pub fn control(&self) -> Control {
        self.control
    }

    /// The `$SHEP_HOME` this lookout watches.
    #[must_use]
    pub fn home(&self) -> &str {
        &self.home
    }

    /// The last host reading, or `None` if there has not been one.
    #[must_use]
    pub fn host(&self) -> Option<super::source::HostSample> {
        self.host
    }

    /// Whether [`Self::host`] is `None` because the platform cannot be read,
    /// rather than because no heartbeat has fired yet.
    ///
    /// The strip says `usage is not available on this platform` for the first
    /// and `not read yet` for the second. They are different facts and an
    /// operator seeing the wrong one waits for numbers that are never coming.
    #[must_use]
    pub fn host_unsupported(&self) -> bool {
        self.host_unsupported
    }

    /// The selected sheep's most recent output, as of the last refresh.
    #[must_use]
    pub fn feed(&self) -> &super::tail::Tail {
        &self.feed
    }

    /// One sheep's uptime as of this dashboard's own clock, in milliseconds.
    ///
    /// A **running** sheep's uptime advances between polls, from the anchor its
    /// row carries — the alternative, showing a number that only moves every
    /// two seconds, reads as a frozen dashboard when it is not. A sheep that is
    /// not running does not advance: its `uptime_ms` is a historical fact about
    /// how long it ran, and animating it would invent one.
    ///
    /// While the link is [`Link::Lost`], nothing advances at all, because
    /// `self.now` stops.
    #[must_use]
    pub fn uptime_ms(&self, id: u32) -> Option<u64> {
        let row = self.flock.get(&id)?;
        if !matches!(row.info.status, ProcStatus::Online | ProcStatus::Starting) {
            return Some(row.info.uptime_ms);
        }
        let elapsed = self.now.saturating_duration_since(row.anchor);
        Some(row.info.uptime_ms.saturating_add(millis(elapsed)))
    }
}

/// Saturating `Duration` -> milliseconds. A lookout left open for 580 million
/// years is not the failure this guards; the cast is what clippy's
/// `cast_possible_truncation` would otherwise deny.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::ProcessEventKind;

    fn sheep(id: u32, name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(id, name, status)
            .pid(Some(1000 + id))
            .uptime_ms(60_000)
            .build()
    }

    fn started() -> (App, Instant) {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(2, "api", ProcStatus::Errored),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        (app, t0)
    }

    /// fails if the poll stops being the truth. A bus event upserts, but a
    /// snapshot REPLACES: the bus is lossy by construction, so a sheep the
    /// events invented — or one they never learned had been deleted — must not
    /// survive the next listing. Event-sourcing a lossy bus is the exact drift
    /// this reducer exists to prevent.
    #[test]
    fn a_snapshot_replaces_the_flock_wholesale() {
        let (mut app, t0) = started();
        app.update(Msg::Event(BusEvent::Process {
            event: ProcessEventKind::Start,
            info: sheep(9, "ghost", ProcStatus::Starting),
            manually: true,
            at_ms: 0,
        }));
        assert_eq!(app.rows().len(), 4, "the bus event upserted");

        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.rows().len(), 1);
        assert!(app.rows().iter().all(|row| row.info.id == 1));
    }

    /// fails if a snapshot that shrinks the flock leaves the selection pointing
    /// at a row that no longer exists. The map is REPLACED wholesale every two
    /// seconds, so a selection that was valid for six sheep can outlive four of
    /// them — and this is the reseat rule, not the clamp `last_row` used to be.
    #[test]
    fn a_snapshot_that_shrinks_the_flock_pulls_the_selection_back() {
        let (mut app, t0) = started();
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected_index(), Some(2));

        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(
            app.selected_index(),
            Some(0),
            "the selection came back with the flock"
        );

        app.update(Msg::Snapshot {
            rows: vec![],
            at: t0,
        });
        assert_eq!(app.selected_index(), None, "an empty flock selects nothing");
    }

    /// fails if the selection stops being stored as an ID. The flock map is
    /// replaced wholesale every two seconds, so an INDEX cursor silently
    /// points at a different sheep the moment an earlier row is deleted —
    /// and the detail pane and the feed would then describe that different
    /// sheep with nothing on screen changing. This is the whole reason
    /// `selected` is a `u32` and not a `usize`.
    #[test]
    fn the_selection_follows_the_sheep_and_not_the_row_number() {
        let (mut app, t0) = started();
        app.update(Msg::Key(KeyPress::SelectDown));
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(app.selected(), Some(3), "the third row, worker");

        // Sheep 1 goes away. `worker` is now row 1 rather than row 2 — an
        // index cursor would now be pointing at `api`.
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(2, "api", ProcStatus::Errored),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        assert_eq!(app.selected(), Some(3), "still worker");
        assert_eq!(app.selected_index(), Some(1), "which is now row 1");
    }

    /// fails if a selection whose sheep was deleted jumps to the top of the
    /// flock. It falls to whatever now occupies the same POSITION, clamped —
    /// throwing an operator back to row 0 of a two-hundred-sheep flock every
    /// time an unrelated sheep is deleted is the behaviour this rejects.
    #[test]
    fn a_deleted_selection_falls_to_the_row_that_took_its_place() {
        let (mut app, t0) = started();
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(app.selected(), Some(2), "api, at index 1");

        // api dies; web and worker remain. Index 1 is now worker.
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        assert_eq!(app.selected(), Some(3), "the row that took index 1");

        // The LAST row dying clamps rather than leaving the cursor past the end.
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected(), Some(3));
        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.selected(), Some(1));

        // An empty flock selects nothing at all, rather than an id that is gone.
        app.update(Msg::Snapshot {
            rows: vec![],
            at: t0,
        });
        assert_eq!(app.selected(), None);
        assert_eq!(app.selected_index(), None);
    }

    /// fails if moving the selection stops asking for a feed refresh, or
    /// starts asking for one when the selection did not move. The second half
    /// is the one that matters: `RefreshFeed` reads two files off disk, and a
    /// held `k` at the top of the flock must not do that once per keypress.
    #[test]
    fn a_selection_that_moves_refreshes_the_feed_and_one_that_cannot_does_not() {
        let (mut app, _) = started();
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::RefreshFeed
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectFirst)),
            Effect::RefreshFeed
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectUp)),
            Effect::None,
            "already at the top: nothing moved, so nothing is re-read"
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectLast)),
            Effect::RefreshFeed
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::None,
            "already at the bottom"
        );
    }

    /// fails if a snapshot stops refreshing the feed. This is the whole
    /// cadence: the feed has no timer of its own, and inherits the two-second
    /// listing, the drop repair, the lag repair and `r` from this one line.
    /// It must NOT fire once the link is lost — a frozen dashboard re-reading
    /// log files would be the one thing on screen still moving.
    #[test]
    fn a_snapshot_refreshes_the_feed_unless_the_link_is_frozen() {
        let (mut app, t0) = started();
        assert_eq!(
            app.update(Msg::Snapshot {
                rows: vec![sheep(1, "web", ProcStatus::Online)],
                at: t0
            }),
            Effect::RefreshFeed
        );
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        assert_eq!(
            app.update(Msg::Snapshot {
                rows: vec![sheep(1, "web", ProcStatus::Online)],
                at: t0
            }),
            Effect::None,
            "a frozen dashboard does not re-read anything"
        );
    }

    /// fails if a frozen dashboard reads a log file. The snapshot path
    /// freezes for free — no snapshots arrive after `Msg::Frozen` — but the
    /// KEY path does not, and `j` on a frozen screen would repaint the feed
    /// with content newer than the banner saying the values are frozen as of
    /// 14:32:07. That is the contradiction on one frame that design decision 7
    /// refuses for the host strip, and 12a's
    /// `the_frozen_frame_does_not_move_however_long_the_link_stays_gone`
    /// cannot catch it, because it presses no keys.
    ///
    /// The cursor still MOVES: the detail pane re-rendering from the frozen
    /// listing is the operator reading data already on the frame, which is
    /// allowed. It is touching the disk that is not.
    #[test]
    fn a_frozen_dashboard_moves_the_cursor_without_touching_a_file() {
        let (mut app, _) = started();
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });

        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::None,
            "no file is read once the link is lost"
        );
        assert_eq!(app.selected(), Some(2), "but the cursor moved anyway");
        assert_eq!(app.update(Msg::Key(KeyPress::SelectLast)), Effect::None);
        assert_eq!(app.selected(), Some(3));
    }

    /// fails if a dropped or lagged frame stops triggering an immediate poll.
    /// The drop carries no information about what was lost; the only repair
    /// is to ask the shepherd what things look like now — bark's own reason,
    /// and the reason this dashboard does not go silently wrong under load.
    #[test]
    fn a_drop_and_a_lag_both_ask_for_an_immediate_poll() {
        let (mut app, _) = started();
        assert_eq!(
            app.update(Msg::Event(BusEvent::Dropped { count: 12 })),
            Effect::PollNow
        );
        assert_eq!(app.update(Msg::BusLagged { count: 3 }), Effect::PollNow);
        assert_eq!(
            app.update(Msg::Event(BusEvent::Process {
                event: ProcessEventKind::Online,
                info: sheep(1, "web", ProcStatus::Online),
                manually: false,
                at_ms: 0,
            })),
            Effect::None,
            "an ordinary event needs no repair"
        );
    }

    /// fails if the two drop conditions stop being told apart. `Dropped` is the
    /// SHEPHERD's outbound queue overflowing; `Lagged` is this process failing
    /// to read its own socket fast enough. They live on opposite ends of the
    /// connection, and an operator cannot tell which end to investigate if the
    /// notice reads the same. `bleats` pins the same distinction.
    #[test]
    fn a_shepherd_side_drop_and_a_local_lag_read_differently() {
        let (mut app, _) = started();
        app.update(Msg::Event(BusEvent::Dropped { count: 12 }));
        let shepherd_side = app.notice().expect("a drop leaves a notice").to_string();
        app.update(Msg::BusLagged { count: 3 });
        let local = app.notice().expect("a lag leaves a notice").to_string();

        assert!(shepherd_side.contains("the shepherd dropped"));
        assert!(local.contains("lookout fell behind"));
        assert_ne!(shepherd_side, local);
    }

    /// fails if the uptime column stops advancing between polls. A dashboard
    /// whose uptime only moves on the 2s poll reads as frozen when it is not.
    #[test]
    fn a_running_sheeps_uptime_advances_with_the_heartbeat() {
        let (mut app, t0) = started();
        assert_eq!(app.uptime_ms(app.rows()[0].info.id), Some(60_000));
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(5),
        });
        assert_eq!(app.uptime_ms(1), Some(65_000));
    }

    /// fails if a FROZEN dashboard keeps counting. This is the specific lie
    /// this whole state exists to avoid: the shepherd is gone, so nothing on
    /// screen is known to still be true, and an UPTIME column that keeps
    /// ticking asserts second by second that a process is still running when
    /// nothing can see it.
    #[test]
    fn a_frozen_dashboard_stops_the_uptime_clock() {
        let (mut app, t0) = started();
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(5),
        });
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        let at_freeze = app.uptime_ms(1);
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(400),
        });
        assert_eq!(
            app.uptime_ms(1),
            at_freeze,
            "the clock stopped with the link"
        );
        assert_eq!(at_freeze, Some(65_000));
    }

    /// fails if a sheep that is not running has its uptime animated. A stopped
    /// sheep's `uptime_ms` is a historical fact — how long it ran — and
    /// advancing it invents one.
    #[test]
    fn a_stopped_sheeps_uptime_does_not_advance() {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Stopped)],
            at: t0,
        });
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(30),
        });
        assert_eq!(app.uptime_ms(1), Some(60_000));
    }

    /// fails if the control gate stops refusing, or starts refusing with
    /// whimsy. Both refusals are literal: the design language's own standing
    /// rule is that nothing about damage gets charming, and a stop is damage.
    #[test]
    fn the_stop_key_refuses_in_both_control_states() {
        let (mut app, _) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::Stop)), Effect::None);
        let read_only = app.notice().expect("a refusal is a notice").to_string();
        assert!(read_only.contains("read-only"));
        assert!(read_only.contains("--allow-control"));

        let t0 = Instant::now();
        let mut allowed = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/rin/.shep".to_string(),
            t0,
        );
        allowed.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(allowed.update(Msg::Key(KeyPress::Stop)), Effect::None);
        let not_built = allowed.notice().expect("a refusal is a notice").to_string();
        assert!(not_built.contains("not built yet"));
        assert_ne!(read_only, not_built);
    }

    /// fails if lookout learns to exit on its own. A `DaemonShutdown` is a
    /// notice here, where in `bleats` it precedes a clean exit — the whole
    /// point of Rin's ruling is that a standing dashboard admits it is stale
    /// rather than vanishing. Only `q` quits.
    #[test]
    fn nothing_but_a_keypress_quits() {
        let (mut app, _) = started();
        for msg in [
            Msg::Event(BusEvent::DaemonShutdown),
            Msg::Event(BusEvent::Dropped { count: 1 }),
            Msg::BusLagged { count: 1 },
            Msg::Retrying { attempt: 5 },
            Msg::Frozen {
                at_local: "2026-08-14 14:32:07".to_string(),
            },
        ] {
            assert_ne!(app.update(msg), Effect::Quit);
        }
        assert_eq!(app.update(Msg::Key(KeyPress::Quit)), Effect::Quit);
    }

    /// fails if a reconnect leaves the dashboard in a state that says nothing.
    /// `Retrying` has to be visible while it is happening — an operator
    /// watching a shepherd restart should see it, and should see it clear.
    #[test]
    fn the_link_state_walks_live_to_retrying_to_lost_and_back() {
        let (mut app, t0) = started();
        assert_eq!(app.link(), &Link::Live);

        app.update(Msg::Retrying { attempt: 1 });
        assert_eq!(app.link(), &Link::Retrying { attempt: 1 });

        app.update(Msg::Relinked);
        assert_eq!(app.link(), &Link::Live);

        app.update(Msg::Retrying { attempt: 5 });
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        assert_eq!(
            app.link(),
            &Link::Lost {
                at_local: "2026-08-14 14:32:07".to_string()
            }
        );

        // A late snapshot after the freeze must not silently unfreeze it: the
        // link task has ended, so there is nothing left to produce one, and
        // accepting it would be accepting a message that cannot exist.
        app.update(Msg::Snapshot {
            rows: vec![],
            at: t0,
        });
        assert!(matches!(app.link(), Link::Lost { .. }));
    }

    /// fails if selecting up at the first row or down at the last one wraps or
    /// panics. Clamping is the choice: wrapping a two-hundred-sheep flock from
    /// the last row to the first on one keypress loses the operator's place
    /// with nothing to undo it.
    #[test]
    fn the_selection_clamps_at_both_ends() {
        let (mut app, _) = started();
        for _ in 0..10 {
            app.update(Msg::Key(KeyPress::SelectUp));
        }
        assert_eq!(
            app.selected_index(),
            Some(0),
            "up past the first row stays on it"
        );
        for _ in 0..10 {
            app.update(Msg::Key(KeyPress::SelectDown));
        }
        assert_eq!(
            app.selected_index(),
            Some(2),
            "down past the last row stays on it"
        );
        app.update(Msg::Key(KeyPress::SelectFirst));
        assert_eq!(app.selected_index(), Some(0));
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected_index(), Some(2));
    }

    /// fails if `r` stops asking for a poll while the link is live, or starts
    /// pretending to ask once it is frozen. It is the one key that does I/O,
    /// and it is what an operator presses when they do not believe the screen
    /// — so it has to be honest in both directions. The link task ENDS at the
    /// freeze (design decision 8), which drops its poll receiver, so an
    /// `Effect::PollNow` after that is a `try_send` into a closed channel:
    /// the operator presses the key and gets silence with no reason given.
    #[test]
    fn refresh_polls_while_live_and_says_why_it_cannot_once_frozen() {
        let (mut app, _) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::Refresh)), Effect::PollNow);

        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        assert_eq!(
            app.update(Msg::Key(KeyPress::Refresh)),
            Effect::None,
            "there is no link task left to ask"
        );
        let notice = app.notice().expect("a refusal is a notice").to_string();
        assert!(notice.contains("the shepherd is gone"));
        assert!(notice.contains("nothing left to ask"));
    }

    /// fails if a notice outlives the keypress after it. A stale refusal still
    /// on screen a minute later is read as a live one.
    #[test]
    fn the_next_keypress_clears_the_notice() {
        let (mut app, _) = started();
        app.update(Msg::Key(KeyPress::Stop));
        assert!(app.notice().is_some());
        app.update(Msg::Key(KeyPress::SelectDown));
        assert!(app.notice().is_none());
    }

    /// fails if a frozen dashboard accepts a host reading. The strip reads
    /// THIS machine, which lookout can still see after the shepherd dies — so
    /// this is the one pane that could keep ticking, and one line ticking over
    /// on a screen whose banner says the values are frozen as of 14:32:07 is a
    /// contradiction on the same frame.
    ///
    /// Asserted in the reducer, which is the single place the rule lives:
    /// `run_ui`'s heartbeat samples unconditionally and this arm decides
    /// whether the dashboard is allowed to believe it, exactly as
    /// `Msg::Tick` and the uptime clock already work.
    #[test]
    fn a_frozen_dashboard_ignores_a_host_sample() {
        let (mut app, _) = started();
        app.update(Msg::Host {
            sample: Some(super::super::source::HostSample {
                load: (2.31, 4.10, 3.88),
                cores: Some(10),
                memory_total_bytes: 32 << 30,
                memory_used_bytes: 12 << 30,
                uptime_seconds: 600,
            }),
        });
        assert!(app.host().is_some(), "a live dashboard takes the sample");

        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        let frozen = app.host();
        assert_eq!(app.update(Msg::Host { sample: None }), Effect::None);
        assert_eq!(app.host(), frozen, "the last values stay, unchanged");
        assert!(
            !app.host_unsupported(),
            "and a refused sample changes no flag"
        );
    }

    /// fails if `Msg::Bleats` starts asking for another refresh. A reducer
    /// that answered its own feed update with `Effect::RefreshFeed` would
    /// spin the UI task at full tilt, re-reading two files as fast as the
    /// loop can go — the one recursion this design can have.
    #[test]
    fn applying_a_tail_does_not_ask_for_another_one() {
        let (mut app, _) = started();
        assert_eq!(
            app.update(Msg::Bleats {
                tail: super::super::tail::Tail::default()
            }),
            Effect::None
        );
    }
}
