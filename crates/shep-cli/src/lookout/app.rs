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
//! and wins every conflict. The only cursor 12a carries is a **scroll offset**
//! — which row of the flock sits first on screen — re-clamped against the map
//! every time it is replaced, because an offset that was valid for six sheep
//! outlives four of them by two seconds. A *selected* row, and the reseat rule
//! a wholesale replacement would need for it, arrive in 12b together with the
//! detail pane that reads them; the phase plan's "What 12b gets" section
//! records that as a decision rather than an omission.

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
// Every public item below this point is not yet constructed or called
// outside this module's own tests: Task 8 (`mod.rs`, the verb and the event
// loop) is the real caller that wires `App`, `Msg` and friends together, and
// it has not landed yet. `#[allow(dead_code)]` says so explicitly, same
// convention `theme::Palette` already carries for the identical reason.
#[allow(dead_code)]
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
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    /// `q`, `Esc`, or `Ctrl-C`.
    Quit,
    /// `k` or `Up` — the viewport moves up one row.
    ScrollUp,
    /// `j` or `Down` — the viewport moves down one row.
    ScrollDown,
    /// `g` or `Home` — the top of the flock.
    ScrollTop,
    /// `G` or `End` — the bottom of it.
    ScrollBottom,
    /// `r` — poll now.
    Refresh,
    /// `x` — the one action key. Refuses in both control states in 12a; see
    /// the plan's design decision 2.
    Stop,
}

/// Everything that can change the dashboard.
#[allow(dead_code)]
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
}

/// What the caller has to do after an update.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing.
    None,
    /// Ask the link task for a `ListFlock` now, rather than at the next tick.
    PollNow,
    /// Leave.
    Quit,
}

/// The connection's state, as the dashboard reports it.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    text: String,
    /// True for a refusal or a damage report — the status bar picks
    /// [`Palette::refusal`] over [`Palette::attention`].
    grave: bool,
}

#[allow(dead_code)]
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
#[allow(dead_code)]
#[derive(Debug)]
pub struct App {
    flock: BTreeMap<u32, Row>,
    /// Which row of [`Self::flock`] is first on screen. An offset, not a
    /// selection: nothing in 12a reads "the sheep the operator is pointing
    /// at", and the view clamps this against its own viewport height as well
    /// (`super::view::flock::scroll_offset`, Task 4 — not an intra-doc link
    /// yet, since that module does not exist until then).
    scroll: usize,
    link: Link,
    notice: Option<Notice>,
    palette: Palette,
    control: Control,
    /// The `$SHEP_HOME` this lookout watches, for the title line. Held here
    /// rather than threaded through `super::view::draw`: it never changes for
    /// the life of the process, and a render function taking it as an argument
    /// would make every call site — including eight scene fixtures — carry it.
    home: String,
    /// The clock the view reads. Advanced by [`Msg::Tick`] — and deliberately
    /// NOT advanced once the link is [`Link::Lost`], which is what stops a
    /// frozen dashboard's uptime column from counting up for a sheep nothing
    /// can see.
    now: Instant,
}

#[allow(dead_code)]
impl App {
    /// A dashboard with an empty flock, a live link, and no notice.
    #[must_use]
    pub fn new(palette: Palette, control: Control, home: String, now: Instant) -> Self {
        Self {
            flock: BTreeMap::new(),
            scroll: 0,
            link: Link::Live,
            notice: None,
            palette,
            control,
            home,
            now,
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
                self.flock = rows
                    .into_iter()
                    .map(|info| (info.id, Row { info, anchor: at }))
                    .collect();
                self.clamp_scroll();
                Effect::None
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
        }
    }

    fn on_event(&mut self, event: BusEvent) -> Effect {
        match event {
            BusEvent::Process { event, info, .. } => {
                if matches!(event, ProcessEventKind::Delete) {
                    self.flock.remove(&info.id);
                } else {
                    let anchor = self.now;
                    self.flock.insert(info.id, Row { info, anchor });
                }
                self.clamp_scroll();
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
            KeyPress::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
                Effect::None
            }
            KeyPress::ScrollDown => {
                self.scroll = (self.scroll + 1).min(self.last_row());
                Effect::None
            }
            KeyPress::ScrollTop => {
                self.scroll = 0;
                Effect::None
            }
            KeyPress::ScrollBottom => {
                self.scroll = self.last_row();
                Effect::None
            }
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

    /// The index of the last row that exists, or `0` for an empty flock.
    ///
    /// The ceiling on [`Self::scroll`]. Clamped rather than wrapping:
    /// wrapping a two-hundred-sheep flock from the last row to the first on
    /// one keypress loses the operator's place with nothing to undo it.
    fn last_row(&self) -> usize {
        self.flock.len().saturating_sub(1)
    }

    /// Pulls the scroll offset back inside a flock that just got smaller.
    ///
    /// [`Msg::Snapshot`] replaces the map wholesale, so an offset that was
    /// valid two seconds ago can now point past the end. A pane scrolled past
    /// its own last row draws nothing at all, which an operator reads as a
    /// crash rather than as a small flock.
    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.last_row());
    }

    /// The flock, in id order.
    #[must_use]
    pub fn rows(&self) -> Vec<&Row> {
        self.flock.values().collect()
    }

    /// Which row of the flock is first on screen.
    ///
    /// A request, not a result: the view clamps it again against its own
    /// viewport height, because this module does not know how tall the
    /// terminal is. See `super::view::flock::scroll_offset` (Task 4 — not an
    /// intra-doc link yet, since that module does not exist until then).
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
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
#[allow(dead_code)]
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

    /// fails if a snapshot that shrinks the flock leaves the viewport scrolled
    /// past the end of it. The map is REPLACED wholesale every two seconds, so
    /// an offset that was valid for six sheep can outlive four of them — and a
    /// pane scrolled past its own last row draws nothing at all, which reads
    /// as a crash rather than as a small flock.
    #[test]
    fn a_snapshot_that_shrinks_the_flock_pulls_the_scroll_back() {
        let (mut app, t0) = started();
        app.update(Msg::Key(KeyPress::ScrollBottom));
        assert_eq!(app.scroll(), 2);

        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.scroll(), 0, "the offset came back with the flock");

        app.update(Msg::Snapshot {
            rows: vec![],
            at: t0,
        });
        assert_eq!(app.scroll(), 0, "an empty flock scrolls nowhere");
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

    /// fails if scrolling up at the first row or down at the last one wraps or
    /// panics. Clamping is the choice: wrapping a two-hundred-sheep flock from
    /// the last row to the first on one keypress loses the operator's place
    /// with nothing to undo it.
    #[test]
    fn the_scroll_offset_clamps_at_both_ends() {
        let (mut app, _) = started();
        for _ in 0..10 {
            app.update(Msg::Key(KeyPress::ScrollUp));
        }
        assert_eq!(app.scroll(), 0, "up past the first row stays on it");
        for _ in 0..10 {
            app.update(Msg::Key(KeyPress::ScrollDown));
        }
        assert_eq!(app.scroll(), 2, "down past the last row stays on it");
        app.update(Msg::Key(KeyPress::ScrollTop));
        assert_eq!(app.scroll(), 0);
        app.update(Msg::Key(KeyPress::ScrollBottom));
        assert_eq!(app.scroll(), 2);
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
        app.update(Msg::Key(KeyPress::ScrollDown));
        assert!(app.notice().is_none());
    }
}
