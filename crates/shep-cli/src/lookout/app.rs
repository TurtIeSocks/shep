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

use shep_client::RequestError;
use shep_core::config::LogLevel;
use shep_core::protocol::{
    BusEvent, Lamb, ProcessEventKind, ProcessInfo, Request, Response, SelectorSpec,
};
use shep_core::status::ProcStatus;

use super::theme::Palette;
use crate::commands::settings::{SettingEdit, SettingField, SettingsSnapshot};
use crate::style::{StyleLevel, StyleSource};
use crate::vocabulary::Reported;

/// Whether this lookout may act on a sheep.
///
/// Default is [`Self::ReadOnly`], per the maintainer's ruling, mirroring the
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
    /// Actions are permitted. Three keys arm a confirm — `x` (stop), `R`
    /// (restart) and `L` (reload) — and Enter sends it.
    Allowed,
}

/// Which keymap is in force.
///
/// Held by the reducer and passed to [`super::input::map_key`] at the call
/// site, so the crossterm edge stays in one file and this module keeps
/// holding no terminal types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// The ordinary dashboard keys.
    Normal,
    /// The filter box is open and every printable key is text.
    Text,
}

/// The keys lookout binds, named by what they mean rather than by which key
/// produces them.
///
/// A plain enum, rather than `crossterm::event::KeyEvent`, so this module and
/// its tests never touch a terminal crate: `super::input::map_key` does the
/// translation at the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    /// `q` in normal mode, or `Ctrl-C` in either.
    Quit,
    /// `Esc` in normal mode. Cancels an armed confirm if there is one, else
    /// clears the filter if there is one, else quits. The reducer decides,
    /// because the keymap cannot see any of those three states.
    Escape,
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
    /// `x`, `R` or `L` — arms a confirm for the given verb, or refuses and
    /// says why. See [`ActionVerb`].
    Action(ActionVerb),
    /// `Enter` in normal mode. Sends an armed confirm; does nothing
    /// otherwise.
    Confirm,
    /// `/` — open the filter box, carrying whatever query is already set.
    FilterStart,
    /// One printable character typed into whichever text field is open.
    /// The reducer decides which that is, the same division `Escape`'s own
    /// doc argues for, because the keymap cannot see it.
    TextChar(char),
    /// `Backspace` in whichever text field is open.
    TextBackspace,
    /// `Enter` in whichever text field is open: apply and leave.
    TextApply,
    /// `Esc` in whichever text field is open: abandon the edit and leave.
    TextAbandon,
    /// `s`: open the settings screen from the dashboard, or close it again
    /// from inside the screen. The reducer decides which, the same division
    /// [`Self::Escape`]'s own doc argues for.
    Settings,
    /// `space`: cycle the value under the settings screen's own cursor.
    /// Meaningless from the dashboard; refuses the same way an action key
    /// does when the control gate is closed.
    Cycle,
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
    /// A request this dashboard asked for came back. `sent` is the echo tag.
    Replied {
        /// What was asked.
        sent: Sent,
        /// What the shepherd said, or why it could not be asked.
        result: Result<Response, RequestError>,
    },
    /// A request the caller could not hand to the link task.
    ///
    /// The reducer has already entered the in-flight state by the time
    /// `run_ui` tries to send, so a `try_send` that fails has to come back:
    /// otherwise the bar keeps saying "sent, waiting for the shepherd" about a
    /// request nobody has, and the one-action-at-a-time guard refuses every
    /// later action for the life of the process.
    Unsent {
        /// What could not be sent.
        sent: Sent,
    },
    /// The settings screen's read of `shep.toml` landed, in answer to an
    /// [`Effect::LoadSettings`] this reducer asked for.
    ///
    /// `Result<_, String>` rather than `commands::settings::SettingError`,
    /// because this reducer holds no error types from `commands` and a
    /// notice needs a rendered sentence anyway: `super::run_ui` is what
    /// calls `to_string()` on the way in.
    Settings {
        /// The rendered snapshot, or why it could not be read.
        result: Result<SettingsSnapshot, String>,
    },
    /// An [`Effect::WriteSetting`] this reducer asked for has landed.
    ///
    /// `Result<(), String>` for the same reason [`Self::Settings`]'s own doc
    /// gives: this reducer holds no error types from `commands`, and
    /// `super::run_ui` is what calls `to_string()` on a
    /// `commands::settings::SettingError` on the way in.
    SettingWritten {
        /// The edit that was sent, echoed back so the reducer can update the
        /// right row without re-deriving it from whatever is armed now: the
        /// cursor, and what is armed, can both have moved on while the write
        /// was in flight.
        edit: SettingEdit,
        /// Whether the write landed, or why it did not.
        result: Result<(), String>,
    },
}

/// What the caller has to do after an update.
///
/// Not `Copy`: [`Self::Send`] carries a [`Sent`], whose `Sent::Action`
/// variant carries a `String`. Nothing matches on an `Effect` by reference,
/// so no call site changes for it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Re-read the selected sheep's log files AND ask the shepherd for its
    /// lambs.
    ///
    /// Held apart from [`Self::RefreshFeed`] because the two triggers differ:
    /// a snapshot must refresh the feed, since the selected row's log paths
    /// can have changed, and must not fetch lambs, since it fires every two
    /// seconds. Returned by `select_at` when the selection actually moved, and
    /// by the callers of `reseat` on the same condition they already use to
    /// choose between `RefreshFeed` and `None`.
    RefreshSelected,
    /// Send a request to the shepherd. Raised by [`App::confirm`] once an
    /// armed action's Enter lands; `super::run_ui` is what actually sends it.
    Send(Sent),
    /// Leave.
    Quit,
    /// Read `shep.toml`'s settings snapshot; the result lands as
    /// [`Msg::Settings`].
    ///
    /// Raised by the dashboard's own `s`, never by the settings screen's:
    /// once the screen is open, `s` closes it instead. See [`App::on_key`].
    LoadSettings,
    /// Apply one edit to `shep.toml`; the result lands as
    /// [`Msg::SettingWritten`].
    ///
    /// Raised by [`App::confirm_setting`] once an armed scalar's Enter
    /// lands. `super::run_ui` runs it on `spawn_blocking`, and that is
    /// load-bearing rather than a convention: `commands::settings`'s
    /// `ConfigLock::acquire` blocks with no deadline
    /// (`FlockArg::LockExclusive`), so a concurrent `shep adopt` on the same
    /// file would freeze the UI task's redraw, its tick and its bus drain
    /// right along with the write.
    WriteSetting(SettingEdit),
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

impl Row {
    /// What this row's STATUS cell reports: [`Self::info`]'s lifecycle
    /// status, or [`Reported::Silent`] for a dog whose process is up and
    /// which has never handshook this shepherd.
    ///
    /// Mirrors `output::rows::reported` exactly, guard included: the `dog`
    /// check is read here rather than left to [`Reported::of`] alone, so a
    /// sheep -- which has no handshake and no version relationship with the
    /// shepherd at all -- can never be painted silent by a future
    /// daemon-side bug that leaves `handshook` set on a non-dog row. One
    /// method for both panes that need it ([`super::view::flock`] and
    /// [`super::view::detail`]), so they cannot drift on what a dog's
    /// STATUS cell says the way the table and the dashboard did before this
    /// existed.
    ///
    /// `pub(crate)` rather than `pub(super)`: `output::rows`' own test
    /// module drives this method alongside `output::rows::reported` to pin
    /// the agreement between the two copies (see
    /// `the_flock_table_and_the_lookout_read_a_dogs_silence_the_same_way`),
    /// and that test lives outside `lookout` entirely.
    #[must_use]
    pub(crate) fn reported(&self) -> Reported {
        if self.info.dog.is_none() {
            return Reported::Live(self.info.status);
        }
        Reported::of(self.info.status, self.info.handshook)
    }
}

/// One app's rolled-up numbers, computed from its own instances.
///
/// The same fields `output::rows`'s own `GroupTotals` sums for `shep
/// flock`'s table (task 9), kept here so the two surfaces sum the exact same
/// fields off the exact same [`ProcessInfo`]s -- an operator seeing
/// different numbers in `shep flock` and `shep lookout` for the same app
/// would be right to distrust both. Restarts, cpu and memory are summed;
/// uptime is the MINIMUM, so a group reads as time since the app was last
/// disturbed rather than as the age of its luckiest instance.
#[derive(Debug, Clone)]
pub struct GroupTotals {
    /// How many instances make up this group.
    pub count: usize,
    /// Every instance's restarts, added up.
    pub restarts: u32,
    /// Every instance's CPU reading summed, `None` only when not one
    /// instance has a live reading.
    pub cpu: Option<f32>,
    /// Every instance's memory reading summed, `None` for the same reason.
    pub memory: Option<u64>,
    /// The MINIMUM live uptime across instances, `None` only when the group
    /// has none.
    pub uptime_ms: Option<u64>,
}

/// One request the dashboard asked the link task to send, carried back on the
/// reply so it can be routed.
///
/// An echo tag rather than a correlation id: the answer to a request can be an
/// `Err` that carries no shape of its own, so the only thing that reliably
/// says which request a reply belongs to is the request itself, handed along
/// beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// The selected sheep's process tree.
    Lambs {
        /// Which sheep was asked about.
        id: u32,
    },
    /// One action against a target: one sheep, or every instance of a named
    /// app. `name` rides along so a reply can be reported by name even after
    /// the target has left the flock.
    Action {
        /// Which verb.
        verb: ActionVerb,
        /// The pinned target.
        target: RowKey,
        /// Its name at arm time.
        name: String,
    },
}

impl Sent {
    /// The wire request this asks for.
    #[must_use]
    pub fn request(&self) -> Request {
        match self {
            Self::Lambs { id } => Request::Describe {
                selector: SelectorSpec::Id(*id),
            },
            Self::Action { verb, target, .. } => {
                let selector = match target {
                    RowKey::Sheep(id) => SelectorSpec::Id(*id),
                    RowKey::Group(name) => SelectorSpec::Name(name.clone()),
                };
                match verb {
                    ActionVerb::Stop => Request::Stop { selector },
                    ActionVerb::Restart => Request::Restart { selector },
                    ActionVerb::Reload => Request::Reload { selector },
                }
            }
        }
    }
}

/// What the cursor can sit on: one sheep, or the header above an app's
/// instances.
///
/// Grouping is [`App::visible_rows`]'s own call, made the same way
/// `output::rows::FlockRows`'s own `name_groups`/`slotted` rule makes it
/// (task 9): more than one instance of a name, every one of them reporting
/// its slot. An app whose listing carries no slot at all (an older shepherd)
/// never earns a [`Self::Group`], and renders exactly as it always has.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowKey {
    /// One app's group header, carrying its name.
    Group(String),
    /// One sheep, by id.
    Sheep(u32),
}

/// What one lamb fetch came back with.
///
/// Three variants because `ProcessInfo::lambs` distinguishes three states and
/// the pane says three different sentences. The CLI has wording for only one
/// of them: `output::emit_described` skips the lamb caption for `None` and for
/// an empty vector alike, so there is nothing to borrow for the other two and
/// the pane says its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LambWalk {
    /// The shepherd walked the process table. Possibly to no descendants.
    Walked(Vec<Lamb>),
    /// The reply carried no walk at all, which for a `Describe` means this
    /// sheep has no pid to walk from.
    NotWalked,
    /// The request did not come back, or came back as something this binary
    /// does not understand.
    Failed,
}

/// One lamb reading, and which sheep it was taken for.
#[derive(Debug, Clone)]
pub struct LambReading {
    id: u32,
    at: Instant,
    walk: LambWalk,
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

/// One row the settings screen's cursor can sit on.
///
/// `Debug` is derived rather than redacted (IR-41): a bare field name or a
/// bare index, nothing a `{:?}` could leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    /// One of the six scalar fields, in [`Settings::rows`]'s fixed order.
    Scalar(SettingField),
    /// Index into [`SettingsSnapshot::dogs`].
    Dog(usize),
}

/// The settings screen's own state. `None` on [`App`] is the dashboard.
///
/// `Debug` is derived rather than redacted (IR-41): the snapshot underneath
/// carries no secret ([`SettingsSnapshot`]'s own note says why) and the
/// cursor is a bare index.
#[derive(Debug, Clone)]
pub struct Settings {
    snapshot: SettingsSnapshot,
    /// An index into [`Self::rows`], clamped on every read rather than kept
    /// pre-clamped: a later task's refresh can shrink the dog list out from
    /// under a cursor already sitting past its new end.
    cursor: usize,
    /// The screen's one in-flight edit, or `None`. One field rather than
    /// several `Option`s, so typing, armed and sent cannot overlap -- the
    /// same claim [`Action`]'s own doc makes for the sheep confirm, made in
    /// the type instead of in a guard.
    pending: Option<Pending>,
}

/// The settings screen's own in-flight edit.
///
/// `Debug` is derived rather than redacted (IR-41): a field name, a
/// candidate value and the rendered prompt sentence built from it -- none of
/// the four scalars this task reaches carries a secret.
#[derive(Debug, Clone)]
enum Pending {
    /// A free-text edit under construction. Only [`SettingField::Socket`]
    /// and [`SettingField::MaxCronSleep`] ever reach this: `Enter` on
    /// either row opens it, seeded with that field's own on-disk value
    /// ([`App::confirm_setting`]), and [`App::on_settings_text_key`] is
    /// the only place a [`KeyPress::TextChar`] or [`KeyPress::TextBackspace`]
    /// ever reaches it while [`Settings`] owns the keyboard.
    Typing {
        /// Which scalar.
        field: SettingField,
        /// What the operator has typed so far.
        buffer: String,
    },
    /// Armed: waiting for the operator's `Enter`. Nothing has gone out yet.
    Armed {
        /// The candidate, ready to send.
        edit: SettingEdit,
        /// The question this candidate reads as, rendered once at arm time
        /// so [`Settings::pending`] can hand back a borrowed `&str` without
        /// re-rendering it on every read.
        text: String,
        /// When it was armed. Only an armed edit expires -- see the
        /// `Msg::Tick` arm.
        at: Instant,
    },
    /// Sent: [`Effect::WriteSetting`] is in flight, waiting on
    /// [`Msg::SettingWritten`]. Carries no `edit`: nothing reads the sent
    /// candidate back off `Pending` -- `Msg::SettingWritten`'s own `edit`
    /// (the one [`Effect::WriteSetting`] round-trips) is what every match
    /// site actually uses, so this variant only ever needs the rendered
    /// question.
    Sent {
        /// The same rendered question, so the prompt line does not change
        /// wording between the question and its own answer.
        text: String,
    },
}

/// What the settings screen's status line shows for its one in-flight edit.
///
/// `Debug` is derived rather than redacted (IR-41): a rendered sentence and
/// a bool, nothing a `{:?}` could leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsPrompt<'a> {
    /// The confirm sentence: what will change, and what applying it does
    /// and does not do.
    pub text: &'a str,
    /// False while it is a question, true once it has gone out.
    pub sent: bool,
}

impl Settings {
    /// A freshly opened screen, cursor on the first row.
    fn new(snapshot: SettingsSnapshot) -> Self {
        Self {
            snapshot,
            cursor: 0,
            pending: None,
        }
    }

    /// The armed candidate and its prompt, or `None`.
    #[must_use]
    pub fn pending(&self) -> Option<SettingsPrompt<'_>> {
        match &self.pending {
            Some(Pending::Armed { text, .. }) => Some(SettingsPrompt { text, sent: false }),
            Some(Pending::Sent { text, .. }) => Some(SettingsPrompt { text, sent: true }),
            Some(Pending::Typing { .. }) | None => None,
        }
    }

    /// The field and buffer of an in-flight free-text edit, or `None` --
    /// including while nothing is armed, while a cycled candidate is
    /// `Armed` or `Sent`, and once the screen is closed. The view's own
    /// window into [`Pending::Typing`], the same way [`Self::pending`] is
    /// its window into `Armed`/`Sent`; the two never overlap; see
    /// [`Self::pending`]'s own `Typing` arm.
    #[must_use]
    pub fn typing(&self) -> Option<(&SettingField, &str)> {
        match &self.pending {
            Some(Pending::Typing { field, buffer }) => Some((field, buffer.as_str())),
            _ => None,
        }
    }

    /// The next candidate for `field`, or `None` for a field this task does
    /// not cycle ([`SettingField::Socket`], [`SettingField::MaxCronSleep`]
    /// -- free text, task 8's own `Pending::Typing`) and for a
    /// [`SettingsRow::Dog`] row, which is never a [`SettingField`] at all.
    ///
    /// Advances from whatever is already armed for THIS field, so a second
    /// `space` walks one step further along the cycle rather than
    /// re-deriving the same next value the file itself would produce --
    /// without that, six log levels behind one cycle key would leave the
    /// fourth unreachable without a cancel in between. An armed candidate
    /// for a DIFFERENT field (the cursor moved after arming) is not a base:
    /// this starts fresh from the snapshot instead.
    fn next_candidate(&self, field: SettingField) -> Option<String> {
        let armed_here = match &self.pending {
            Some(Pending::Armed {
                edit:
                    SettingEdit::Set {
                        field: armed_field,
                        value,
                    },
                ..
            }) if *armed_field == field => Some(value.as_str()),
            _ => None,
        };
        let base = match armed_here {
            Some(value) => value,
            None => self.current_value(field)?,
        };
        Some(match field {
            SettingField::LogLevel => next_log_level(base),
            SettingField::LogJson | SettingField::AllowControl => next_bool(base),
            SettingField::StyleLevel => next_style_level(base),
            SettingField::Socket | SettingField::MaxCronSleep => return None,
        })
    }

    /// The snapshot's own rendered value for one of the four scalars this
    /// task cycles. `None` for the two this task does not.
    fn current_value(&self, field: SettingField) -> Option<&str> {
        Some(match field {
            SettingField::LogLevel => self.snapshot.log_level.value.as_str(),
            SettingField::LogJson => self.snapshot.log_json.value.as_str(),
            SettingField::AllowControl => self.snapshot.allow_control.value.as_str(),
            SettingField::StyleLevel => self.snapshot.style_level.value.as_str(),
            SettingField::Socket | SettingField::MaxCronSleep => return None,
        })
    }

    /// The snapshot's own rendered value for one of the two free-text
    /// fields [`Self::current_value`] does not cover -- what
    /// [`App::confirm_setting`] seeds [`Pending::Typing`]'s buffer with.
    /// Only ever called with [`SettingField::Socket`] or
    /// [`SettingField::MaxCronSleep`]: the other four are cycled, not
    /// typed, and never open an editor.
    fn text_seed(&self, field: SettingField) -> &str {
        match field {
            SettingField::Socket => self.snapshot.socket.value.as_str(),
            SettingField::MaxCronSleep => self.snapshot.max_cron_sleep.value.as_str(),
            SettingField::LogLevel
            | SettingField::LogJson
            | SettingField::AllowControl
            | SettingField::StyleLevel => {
                unreachable!("text_seed only ever reaches the two free-text fields")
            }
        }
    }

    /// What the screen reads off disk. `view::settings::content_lines` is
    /// the real caller, reading it to render every row's value and source.
    /// A landed write does not update this in place any more: `App`'s own
    /// `Msg::SettingWritten` `Ok` arm raises a fresh [`Effect::LoadSettings`]
    /// instead, the same read `r` and the initial `s` both already go
    /// through, so `Set` and `Unset` land the same way and neither can
    /// drift from whatever else changed in the document meanwhile.
    #[must_use]
    pub fn snapshot(&self) -> &SettingsSnapshot {
        &self.snapshot
    }

    /// Every row the cursor can sit on: the six scalars in their fixed
    /// order, then one row per candidate dog.
    #[must_use]
    pub fn rows(&self) -> Vec<SettingsRow> {
        let mut rows = vec![
            SettingsRow::Scalar(SettingField::LogLevel),
            SettingsRow::Scalar(SettingField::LogJson),
            SettingsRow::Scalar(SettingField::Socket),
            SettingsRow::Scalar(SettingField::MaxCronSleep),
            SettingsRow::Scalar(SettingField::AllowControl),
            SettingsRow::Scalar(SettingField::StyleLevel),
        ];
        rows.extend((0..self.snapshot.dogs.len()).map(SettingsRow::Dog));
        rows
    }

    /// The row the cursor sits on. `None` only if [`Self::rows`] is somehow
    /// empty, which cannot happen today (the six scalars are unconditional),
    /// but the type stays honest about it rather than asserting.
    ///
    /// `view::settings::content_lines` is the real caller, reading it to
    /// highlight the selected row -- and [`App::cycle_setting`] reads it
    /// through this same accessor to decide which field, if any, `space`
    /// arms.
    #[must_use]
    pub fn cursor(&self) -> Option<SettingsRow> {
        let rows = self.rows();
        rows.get(self.cursor.min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// Moves the cursor by `delta` rows, clamped to [`Self::rows`] rather
    /// than wrapping, the same rule the flock table's own cursor follows.
    fn move_by(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, len as isize - 1) as usize;
    }

    /// Moves the cursor to the first row.
    fn move_to_first(&mut self) {
        self.cursor = 0;
    }

    /// Moves the cursor to the last row.
    fn move_to_last(&mut self) {
        self.cursor = self.rows().len().saturating_sub(1);
    }
}

/// `Off, Error, Warn, Info, Debug, Trace`, [`LogLevel`]'s own declared
/// order, wrapping from `Trace` back to `Off`.
const LOG_LEVEL_ORDER: [LogLevel; 6] = [
    LogLevel::Off,
    LogLevel::Error,
    LogLevel::Warn,
    LogLevel::Info,
    LogLevel::Debug,
    LogLevel::Trace,
];

/// One step along [`LOG_LEVEL_ORDER`] from `current`. A string this crate
/// did not itself render (should not happen -- every candidate and every
/// snapshot value comes from [`LogLevel::as_str`]) reads as
/// [`LogLevel::default`]'s own place in the ladder (`Warn`), so a corrupt
/// value still produces a legal next one rather than panicking.
fn next_log_level(current: &str) -> String {
    let index = LogLevel::from_name(current)
        .and_then(|level| {
            LOG_LEVEL_ORDER
                .iter()
                .position(|candidate| *candidate == level)
        })
        .unwrap_or(2);
    LOG_LEVEL_ORDER[(index + 1) % LOG_LEVEL_ORDER.len()]
        .as_str()
        .to_string()
}

/// Flips `"true"`/`"false"`. Anything else (should not happen, the same
/// reason [`next_log_level`] states) reads as `false` and flips to `true`.
fn next_bool(current: &str) -> String {
    (current != "true").to_string()
}

/// `Full, Plain, Bare`, [`StyleLevel`]'s own declared order, wrapping from
/// `Bare` back to `Full`.
const STYLE_LEVEL_ORDER: [StyleLevel; 3] = [StyleLevel::Full, StyleLevel::Plain, StyleLevel::Bare];

/// One step along [`STYLE_LEVEL_ORDER`] from `current`, the same fallback
/// [`next_log_level`] takes for an unparseable value.
fn next_style_level(current: &str) -> String {
    let index = StyleLevel::parse(current)
        .and_then(|level| {
            STYLE_LEVEL_ORDER
                .iter()
                .position(|candidate| *candidate == level)
        })
        .unwrap_or(0);
    STYLE_LEVEL_ORDER[(index + 1) % STYLE_LEVEL_ORDER.len()].to_string()
}

/// The confirm sentence for `field`'s candidate `value` -- verbatim, per the
/// task 7 spec's own table. `value` is always the candidate a
/// `next_*` function above just produced, never re-derived here: this
/// function only ever renders it into the sentence.
///
/// Called only with a field [`Settings::next_candidate`] returned `Some`
/// for, so [`SettingField::Socket`] and [`SettingField::MaxCronSleep`]
/// never reach the two arms below -- named rather than wildcarded so a
/// future field added to the match cannot fall through unnoticed.
fn confirm_text(field: SettingField, value: &str) -> String {
    match field {
        SettingField::LogLevel => format!(
            "set log_level to {value}? needs shep daemon reload, and will not apply if the shepherd was booted with SHEP_LOG_LEVEL or --log-level"
        ),
        SettingField::LogJson => format!(
            "set log_json to {value}? needs shep daemon reload, and will not apply if the shepherd was booted with SHEP_LOG_JSON or --log-json"
        ),
        SettingField::AllowControl => {
            let word = if value == "true" { "on" } else { "off" };
            format!("turn whistle control tools {word}? needs shep whistle restarted")
        }
        SettingField::StyleLevel => {
            format!("set style level to {value}? the next command reads it")
        }
        SettingField::Socket | SettingField::MaxCronSleep => unreachable!(
            "Settings::next_candidate never arms these two -- they are task 8's Pending::Typing"
        ),
    }
}

/// The confirm sentence for a free-text edit -- verbatim, per the design
/// spec's own table, and the pair this repository's voice review already
/// signed off on. Only ever built from [`App::on_settings_text_key`]'s
/// `TextApply` arm, and only ever with an edit naming
/// [`SettingField::Socket`] or [`SettingField::MaxCronSleep`]: the other
/// four fields build their sentence through [`confirm_text`] instead,
/// which never sees an [`SettingEdit::Unset`] because none of the four is
/// optional. The `Socket` sentence says both halves on purpose: that a
/// reload will not move it, and that an env var or a boot flag may shadow
/// it anyway even after this edit lands.
fn confirm_text_for_edit(edit: &SettingEdit) -> String {
    match edit {
        SettingEdit::Set {
            field: SettingField::Socket,
            value,
        } => format!(
            "set socket to {value}? needs the shepherd stopped and started; a reload will not move it, and it will not apply if the shepherd was booted with SHEP_SOCKET or --socket"
        ),
        SettingEdit::Set {
            field: SettingField::MaxCronSleep,
            value,
        } => format!(
            "set max_cron_sleep to {value}? needs shep daemon reload, and will not apply if the shepherd was booted with SHEP_MAX_CRON_SLEEP or --max-cron-sleep"
        ),
        SettingEdit::Unset {
            field: SettingField::Socket,
        } => "unset socket? it goes back to the default under $SHEP_HOME, and needs the shepherd stopped and started"
            .to_string(),
        SettingEdit::Unset {
            field: SettingField::MaxCronSleep,
        } => "unset max_cron_sleep? it goes back to the daemon's own default, and needs shep daemon reload"
            .to_string(),
        SettingEdit::Set { .. } | SettingEdit::Unset { .. } => unreachable!(
            "on_settings_text_key only ever builds an edit for socket or max_cron_sleep"
        ),
    }
}

/// What [`App`]'s `Msg::SettingWritten` `Err` arm reopens
/// [`Pending::Typing`] with, for the two free-text fields: the field and
/// the text the operator typed, recovered from the edit
/// [`Effect::WriteSetting`] carried. `None` for the four cycled fields,
/// which reopen nothing -- a refusal there clears the row's pending state
/// and raises the notice on its own, same as before this task.
fn typed_text_of(edit: &SettingEdit) -> Option<(SettingField, String)> {
    match edit {
        SettingEdit::Set {
            field: field @ (SettingField::Socket | SettingField::MaxCronSleep),
            value,
        } => Some((*field, value.clone())),
        SettingEdit::Unset {
            field: field @ (SettingField::Socket | SettingField::MaxCronSleep),
        } => Some((*field, String::new())),
        _ => None,
    }
}

/// What an action key does.
///
/// Three verbs, and deliberately not four: `start` is whistle's and the CLI's,
/// by the maintainer's ruling. Delete, scale, signal and whisper stay CLI-only for
/// whistle's own reasons: each takes a parameter a dashboard has nowhere to
/// put, or removes an app from the registry, which is the one action no
/// keypress should be one Enter away from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionVerb {
    /// `x`. Stops the sheep; it stays registered.
    Stop,
    /// `R`, on shift because `r` is refresh.
    Restart,
    /// `L`, on shift for symmetry with `R`.
    Reload,
}

impl ActionVerb {
    /// The word the prompt and every outcome sentence begin with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Reload => "reload",
        }
    }
}

/// Whether an action has been sent yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Armed, waiting for the operator's Enter. Nothing has gone out.
    Armed,
    /// Sent, waiting for the shepherd.
    Sent,
}

/// The one action this dashboard is in the middle of.
///
/// The target is captured HERE, by [`RowKey`] and by name, and never re-read
/// from the selection: a snapshot can land between the arming keypress and
/// the Enter, and a confirmation that re-read the cursor could act on a
/// sheep the operator never pointed at.
///
/// One field on [`App`] rather than two `Option`s, so "armed" and "in flight"
/// cannot both be true. That is the same claim the one-action-at-a-time rule
/// makes, made in the type instead of in a guard.
#[derive(Debug, Clone)]
struct Action {
    verb: ActionVerb,
    target: RowKey,
    name: String,
    /// How many processes [`Self::target`] reaches: 1 for a sheep, the
    /// group's own size for a [`RowKey::Group`]. Captured at arm time, the
    /// same reason [`Self::name`] is: the confirm prompt states the blast
    /// radius before the operator commits, and the group could gain or lose
    /// an instance between arming and the Enter.
    count: usize,
    /// When it was armed. Only an armed action expires.
    at: Instant,
    stage: Stage,
}

/// What the status bar needs to know about the action in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionState<'a> {
    /// Which verb.
    pub verb: ActionVerb,
    /// The pinned target.
    pub target: &'a RowKey,
    /// The pinned target's name, as it was when the key was pressed.
    pub name: &'a str,
    /// How many processes [`Self::target`] reaches.
    pub count: usize,
    /// False while it is a question, true once it has gone out.
    pub sent: bool,
}

/// How long an armed confirm waits for its Enter.
///
/// Ten seconds. A prompt left armed while the operator walks away, followed by
/// an Enter typed at what they think is a shell, is the same fat finger
/// arriving by a slower route. It rides `Msg::Tick`, so it costs one `Instant`
/// comparison and no timer (A11).
pub const CONFIRM_EXPIRY: Duration = Duration::from_secs(10);

/// The sentence `r` gives when the link is gone. The action keys refuse with
/// the same one, so the two cannot drift apart.
const LINK_GONE: &str = "the shepherd is gone — nothing left to ask";

/// The whole dashboard's state.
#[derive(Debug)]
pub struct App {
    flock: BTreeMap<u32, Row>,
    /// Which row the detail pane and the bleats feed describe.
    ///
    /// A [`RowKey`], not an index. The flock map is replaced wholesale every
    /// two seconds, so an index survives a `shep delete` of an earlier row by
    /// silently pointing at a different sheep — and every pane below the table
    /// would then describe that different sheep with nothing on screen
    /// changing. `None` only for an empty flock.
    ///
    /// The viewport offset is derived from this rather than stored beside it
    /// ([`super::view::flock::scroll_offset`]), which is what makes a
    /// disagreement between a stored offset and a stored cursor impossible
    /// rather than merely unlikely.
    selected: Option<RowKey>,
    /// The live substring filter over sheep NAMES, empty when there is none.
    ///
    /// Case-insensitive `contains`, and nothing else: not the CLI's selector
    /// grammar, not a regex, no understanding of `fold:`, `all` or ids. The
    /// grammar is exact-match on both the variants an operator would type
    /// while narrowing, so it cannot narrow as you type, and a half-typed
    /// `/re` parses as a search for a sheep literally named `/re` rather than
    /// refusing. See the design's feature 1 and assumption A1.
    ///
    /// Taken literally, spaces included, with no trimming (A6): this repo does
    /// not widen an accepted input format without a basis in the spec.
    filter: String,
    /// Which keymap [`super::input::map_key`] is called with. Normal until
    /// `/` opens the box; the reducer, not the keymap, owns this state.
    mode: InputMode,
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
    /// The last lamb reading, or `None` before there has been one.
    ///
    /// Keyed by the id it was taken for, so a reading for a sheep that is no
    /// longer selected, and a request a full channel dropped, both read as
    /// "not read yet" without a second field tracking them.
    lambs: Option<LambReading>,
    /// The one action this dashboard is in the middle of, or `None`.
    action: Option<Action>,
    /// The settings screen's own state. `None` is the dashboard; `Some` is
    /// the screen, open over it.
    settings: Option<Settings>,
    /// The resolved style level and which layer chose it, from
    /// `run_argv`'s own `resolve_style`. `App::new` defaults it to
    /// `(StyleLevel::Full, StyleSource::Default)`; `run_argv`'s `lookout`
    /// dispatch immediately overrides it with the real resolution through
    /// `Self::set_style`, so the settings screen's own STYLE LEVEL row
    /// reads the same answer the rest of the CLI does rather than a second,
    /// independently derived one.
    style: (StyleLevel, StyleSource),
}

impl App {
    /// A dashboard with an empty flock, a live link, and no notice.
    #[must_use]
    pub fn new(palette: Palette, control: Control, home: String, now: Instant) -> Self {
        Self {
            flock: BTreeMap::new(),
            selected: None,
            filter: String::new(),
            mode: InputMode::Normal,
            link: Link::Live,
            notice: None,
            palette,
            control,
            home,
            now,
            host: None,
            host_unsupported: false,
            feed: super::tail::Tail::default(),
            lambs: None,
            action: None,
            settings: None,
            style: (StyleLevel::Full, StyleSource::Default),
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
                self.forget_missing_target();
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
                    self.disarm_on_link_change();
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
                self.disarm_on_link_change();
                Effect::None
            }
            Msg::Tick { now } => {
                // The one line that keeps a frozen dashboard honest.
                if !matches!(self.link, Link::Lost { .. }) {
                    self.now = now;
                    let expired = self.action.as_ref().is_some_and(|action| {
                        action.stage == Stage::Armed
                            && now.saturating_duration_since(action.at) >= CONFIRM_EXPIRY
                    });
                    if expired {
                        self.action = None;
                    }
                }
                // The settings screen's own expiry sits OUTSIDE the guard
                // above, and compares against `now` -- the tick's own
                // instant -- rather than `self.now`, which that guard stops
                // advancing once the link is lost. The sheep confirm freezes
                // with everything else on a dead link because every word of
                // it describes the shepherd, which the dashboard can no
                // longer see; a settings edit describes a local file that is
                // not stale just because the shepherd stopped answering, so
                // it keeps its own clock running.
                if let Some(settings) = self.settings.as_mut() {
                    let expired = matches!(
                        settings.pending,
                        Some(Pending::Armed { at, .. })
                            if now.saturating_duration_since(at) >= CONFIRM_EXPIRY
                    );
                    if expired {
                        settings.pending = None;
                    }
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
            //
            // The `Link::Lost` guard is load-bearing, not decorative: `run_ui`
            // arms its coalesced read from a `feed_dirty` flag set BEFORE a
            // freeze can land, so a read requested a moment earlier can still
            // be in flight when `Msg::Frozen` arrives and this arm sees the
            // stale read after it. Without the guard that read reaches the
            // rendered frame under a banner saying the values are frozen —
            // the same contradiction-on-one-frame design decision 7 already
            // refuses for the host strip and the uptime clock.
            Msg::Bleats { tail } => {
                if matches!(self.link, Link::Lost { .. }) {
                    return Effect::None;
                }
                self.feed = tail;
                Effect::None
            }
            Msg::Replied { sent, result } => match sent {
                Sent::Lambs { id } => self.on_lambs(id, result),
                Sent::Action { verb, target, name } => {
                    self.on_action_reply(verb, target, &name, result)
                }
            },
            Msg::Unsent { sent } => match sent {
                Sent::Action { verb, target, name } => {
                    self.action = None;
                    self.notice = Some(Notice {
                        // "it was not sent", and no cause. The reducer does
                        // not know one: the channel holds 2, it is shared with
                        // lamb fetches, and `run_connected` awaits each
                        // request inline, so `Full` is reachable while the
                        // shepherd is perfectly reachable and merely slow.
                        // Naming a cause nothing observed is the failure the
                        // `-` CPU cell exists to prevent.
                        text: format!("{}: it was not sent", target_prefix(verb, &target, &name)),
                        grave: true,
                    });
                    Effect::None
                }
                // A dropped lamb fetch already reads as "not read yet", which
                // is what the pane says. Nothing to report and nothing to
                // clear.
                Sent::Lambs { .. } => Effect::None,
            },
            // The file can have changed since `s` asked for it, and the
            // screen opens on whatever this read found -- never on stale or
            // empty state, which is exactly why `s` asks first rather than
            // opening immediately. A failed read leaves the dashboard up: an
            // empty settings screen would say nothing about why it has
            // nothing to show.
            //
            // This is also where a landed write's own re-read lands
            // (`Msg::SettingWritten`'s `Ok` arm raises `Effect::LoadSettings`
            // rather than hand-updating one row) and where `r` lands. Both
            // arrive with `self.settings` already `Some`, and the cursor is
            // preserved across them rather than reset: `opening` below is
            // true only the first time, when `s` itself is what raised the
            // read.
            Msg::Settings { result } => {
                let opening = self.settings.is_none();
                match result {
                    Ok(snapshot) => {
                        // Clears an action that armed while the read was in
                        // flight (`s`, then `x`, then this landing): once
                        // this branch runs, `on_key`'s settings short
                        // circuit intercepts every key ahead of the
                        // armed-confirm cancel block, and `on_settings_key`
                        // no-ops `Confirm`. Without this, the prompt would
                        // sit on screen, unreachable by Enter or by any
                        // other key, until `CONFIRM_EXPIRY`. This is the
                        // same closing-by-construction `on_key`'s own
                        // comment already argues for `/` and the filter
                        // box: a sheep confirm and the settings screen can
                        // never coexist, and this is the one place
                        // `self.settings` becomes `Some`, so clearing here
                        // covers both the keypress and the race.
                        self.action = None;
                        // The sibling race: `s`, then `/`, then a snapshot
                        // landing while the box is still open. `on_key`
                        // checks `self.mode == InputMode::Text` ahead of the
                        // settings check, and `on_text_key` never consults
                        // `self.settings`, so an open box would survive the
                        // screen opening and keep eating every keystroke the
                        // settings keymap was meant to own. Leaving text
                        // mode here makes that state unrepresentable, the
                        // same way clearing `self.action` above does for a
                        // confirm.
                        //
                        // The query itself is kept, not cleared: this is
                        // `TextApply`'s reading (Enter), not
                        // `TextAbandon`'s (Esc). The operator was watching
                        // the dashboard filter live while `/` was open --
                        // the read had not landed yet -- so the characters
                        // they typed are a real query they chose to build,
                        // not a stray keystroke. Discarding it on the way
                        // out would be the one filter edit in this whole
                        // screen that vanishes without an Esc.
                        self.mode = InputMode::Normal;
                        // The cursor: reset to the first row only while
                        // `opening`. `Settings::cursor` clamps on every
                        // read, so a preserved cursor sitting past a
                        // shorter dogs list still lands somewhere real
                        // rather than out of bounds.
                        let cursor = self.settings.as_ref().map_or(0, |settings| settings.cursor);
                        let mut settings = Settings::new(snapshot);
                        if !opening {
                            settings.cursor = cursor;
                        }
                        self.settings = Some(settings);
                    }
                    Err(message) => {
                        self.notice = Some(Notice {
                            text: message,
                            grave: true,
                        });
                    }
                }
                Effect::None
            }
            // An [`Effect::WriteSetting`] landed. `Ok` clears the prompt and
            // raises a fresh [`Effect::LoadSettings`] rather than folding
            // the write into the row by hand: that covers `Set` and
            // `Unset` uniformly (an `Unset` has no local value to fold in,
            // only the document does), picks up anything else that changed
            // on disk in the meantime, and is the same read `r` and the
            // initial `s` already go through -- see [`Msg::Settings`]'s own
            // doc for how the cursor survives it. `Err` leaves the row
            // exactly as it read before the write, and raises a grave
            // notice with the refusal's own words rather than a generic
            // one -- the same "say why" rule `arm`'s refusal ladder follows.
            //
            // For the two free-text fields, `Err` also reopens
            // [`Pending::Typing`] with the text the operator typed
            // ([`typed_text_of`]) and switches [`InputMode::Text`] back on:
            // the refusal is discovered under `apply_setting`'s own lock,
            // after the confirm, so it has to land as a re-opened editor
            // rather than a blank row -- an operator who typed a long path
            // must not have to retype it to fix one character. The four
            // cycled fields have nothing to reopen; their `Err` matches the
            // behaviour this arm already had.
            Msg::SettingWritten { edit, result } => match result {
                Ok(()) => {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.pending = None;
                    }
                    Effect::LoadSettings
                }
                Err(message) => {
                    // `typed_text_of` names the field, so the two branches
                    // below never share a borrow of `self.settings` that
                    // would fight the `self.notice` assignment beneath
                    // them.
                    if let Some((field, buffer)) = typed_text_of(&edit) {
                        if let Some(settings) = self.settings.as_mut() {
                            settings.pending = Some(Pending::Typing { field, buffer });
                        }
                        self.mode = InputMode::Text;
                    } else if let Some(settings) = self.settings.as_mut() {
                        settings.pending = None;
                    }
                    self.notice = Some(Notice {
                        text: message,
                        grave: true,
                    });
                    Effect::None
                }
            },
        }
    }

    /// Records one lamb reading. Always [`Effect::None`]: a reducer that
    /// answered a reading with another request would spin the UI task.
    fn on_lambs(&mut self, id: u32, result: Result<Response, RequestError>) -> Effect {
        // The same guard `Msg::Bleats` carries, for the same reason: this
        // fetch is armed before a freeze can land, and a reading that reached
        // the frame afterwards would be newer than the banner saying the
        // values are frozen.
        if matches!(self.link, Link::Lost { .. }) {
            return Effect::None;
        }
        let walk = match result {
            Ok(Response::Described(rows)) => rows
                .into_iter()
                .find(|info| info.id == id)
                .map_or(LambWalk::Failed, |info| {
                    info.lambs.map_or(LambWalk::NotWalked, LambWalk::Walked)
                }),
            // An `Err`, or a reply this binary does not recognise. Neither is
            // an empty walk, and reporting either as one would say "none
            // found" about a process table nobody read.
            _ => LambWalk::Failed,
        };
        self.lambs = Some(LambReading {
            id,
            at: self.now,
            walk,
        });
        Effect::None
    }

    /// One action's answer: the shepherd's rows upserted, and one sentence.
    ///
    /// No provisional row state is invented anywhere on this path. The three
    /// replies carry `Vec<ProcessInfo>`, so the table updates from the
    /// shepherd's own words; an `online, restart sent...` in the STATUS column
    /// would be a guess printed in the one column whose whole job is to be
    /// true, and it would have to negotiate with the narrow-terminal column
    /// drop order for the privilege.
    fn on_action_reply(
        &mut self,
        verb: ActionVerb,
        target: RowKey,
        name: &str,
        result: Result<Response, RequestError>,
    ) -> Effect {
        self.action = None;
        let prefix = target_prefix(verb, &target, name);
        // Each verb accepts its own reply and no other. A `Stopped` answering
        // a `Restart` carries rows and would upsert perfectly happily, which
        // is why the guards are on the arms rather than a single
        // rows-carrying match.
        let rows = match result {
            Ok(Response::Stopped(rows)) if verb == ActionVerb::Stop => rows,
            Ok(Response::Restarted(rows)) if verb == ActionVerb::Restart => rows,
            Ok(Response::Reloading(rows)) if verb == ActionVerb::Reload => rows,
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{prefix}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
                return Effect::None;
            }
            // The daemon's own message, not `RequestError`'s full `Display`:
            // the latter interpolates the code with `{:?}` and produces
            // "the daemon reported NotFound: ...", which puts a Rust
            // identifier on an operator's screen. The message alone is
            // already a sentence a human wrote.
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: {}", err.message),
                    grave: true,
                });
                return Effect::None;
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: {other}"),
                    grave: true,
                });
                return Effect::None;
            }
        };
        let anchor = self.now;
        let was_empty = self.flock.is_empty();
        for info in rows {
            self.flock.insert(info.id, Row { info, anchor });
        }
        self.notice = Some(Notice {
            text: format!("{prefix}: {}", outcome(verb)),
            grave: false,
        });
        if was_empty && self.reseat(None) {
            return Effect::RefreshSelected;
        }
        Effect::None
    }

    fn on_event(&mut self, event: BusEvent) -> Effect {
        match event {
            BusEvent::Process { event, info, .. } => {
                if matches!(event, ProcessEventKind::Delete) {
                    let previous = self.selected_index();
                    self.flock.remove(&info.id);
                    self.forget_missing_target();
                    return if self.reseat(previous) {
                        Effect::RefreshSelected
                    } else {
                        Effect::None
                    };
                }
                // An upsert cannot orphan the selection from `self.flock` —
                // the row it names either already existed (so the id is
                // still in the map) or is new (so it cannot be the one the
                // selection pointed at) — but it CAN orphan it from the
                // VISIBLE sequence `reseat` and `j`/`k` actually walk: a
                // rename that moves the selected row out of the current
                // filter leaves its id in `self.flock` while dropping it out
                // of `visible_ids()`. A `was_empty`-only guard misses that
                // case: the cursor stays pinned to an id the table has
                // stopped drawing, and `j`/`k` do nothing until the next
                // snapshot repairs it. Reading
                // `previous` before the insert and always reseating covers
                // both that case and the empty-to-non-empty one below with
                // the same call — `reseat` itself is a no-op read when the
                // selection is still seated, so the common case costs one
                // cheap check.
                let previous = self.selected_index();
                let anchor = self.now;
                self.flock.insert(info.id, Row { info, anchor });
                if self.reseat(previous) {
                    return Effect::RefreshSelected;
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
        // While the box is open every KEY is text. That is not the same as
        // nothing being able to raise a notice: three of them arrive on
        // messages that are not keys at all, and the status bar rather than
        // this branch is what keeps them off the query. See `on_text_key`.
        if self.mode == InputMode::Text {
            return self.on_text_key(key);
        }
        // The settings screen owns its own keymap while it is open, ahead of
        // the armed-confirm check below: no sheep action can be armed from
        // in here (`s` reaches the dashboard only after the screen closes),
        // so the ordering is a documentation choice, not a correctness one.
        if self.settings.is_some() {
            return self.on_settings_key(key);
        }
        // Checked BEFORE the ordinary dispatch, and a cancelling keypress is
        // CONSUMED. A stray `j` during a pending confirm cancels it and does
        // not also move the selection: if it did both, the operator would see
        // the prompt vanish and the cursor move, and the next reflexive Enter
        // would act on a target they had already lost track of.
        //
        // Cancelling is silent. Nothing happened, and reporting nothing as if
        // it were something trains an operator to ignore the status bar.
        //
        // One consequence worth naming: `/` cancels before it opens the filter
        // box, so a confirm and a filter edit can never coexist. That closes
        // the whole interaction between the two features by construction
        // rather than by rule.
        if self
            .action
            .as_ref()
            .is_some_and(|action| action.stage == Stage::Armed)
        {
            if key == KeyPress::Confirm {
                return self.confirm();
            }
            // The one key the cancel does not consume. `input.rs`'s own doc
            // says why: dropping Ctrl-C would leave the most reflexive way out
            // of a terminal program doing nothing, and the operator's next
            // move is `kill -9` from another window, past every restore path
            // `super::term` has. Quitting DISCARDS the confirm rather than
            // acting on it, so the property this rule exists for is untouched;
            // that property is about a cancelling key also doing its ordinary
            // job on a target the operator has lost track of. Text mode makes
            // the same carve-out. See the phase plan's "Shapes the design
            // named" #4.
            if key == KeyPress::Quit {
                return Effect::Quit;
            }
            self.action = None;
            return Effect::None;
        }
        self.notice = None;
        match key {
            KeyPress::Quit => Effect::Quit,
            // The one key whose meaning depends on state, and the screen says
            // which meaning is in force: the bar reads `esc clear` for
            // exactly as long as clearing is what it does.
            KeyPress::Escape => {
                if self.filter.is_empty() {
                    Effect::Quit
                } else {
                    self.set_filter(String::new())
                }
            }
            // The one key that does I/O, and it refuses honestly once the
            // link task has ended: its poll receiver is gone by then, so an
            // `Effect::PollNow` would be a `try_send` into a closed channel
            // and the operator would get silence with no reason for it.
            KeyPress::Refresh => {
                if matches!(self.link, Link::Lost { .. }) {
                    self.notice = Some(Notice {
                        text: LINK_GONE.to_string(),
                        grave: true,
                    });
                    return Effect::None;
                }
                Effect::PollNow
            }
            KeyPress::SelectUp => self.select_by(-1),
            KeyPress::SelectDown => self.select_by(1),
            KeyPress::SelectFirst => self.select_at(0),
            KeyPress::SelectLast => self.select_at(self.visible_len().saturating_sub(1)),
            KeyPress::Action(verb) => self.arm(verb),
            // Enter means nothing outside an armed confirm. It reaches this
            // match whenever nothing is armed, which includes while an action
            // is IN FLIGHT, because the routing rule above only fires on
            // `Stage::Armed`. Named rather than swept into a wildcard: on the
            // one key whose job is to confirm a stop, an unspecified arm is
            // one edit away from being the wrong arm.
            KeyPress::Confirm => Effect::None,
            KeyPress::FilterStart => {
                self.mode = InputMode::Text;
                Effect::None
            }
            // `map_key` produces these only in text mode, which the branch at
            // the top of this function has already taken. Named rather than
            // wildcarded so a future variant does not fall silently into an
            // arm that ignores it.
            KeyPress::TextChar(_)
            | KeyPress::TextBackspace
            | KeyPress::TextApply
            | KeyPress::TextAbandon => Effect::None,
            // The read, not the open: the screen opens only once
            // `Msg::Settings` lands. See that arm's own doc for why.
            KeyPress::Settings => Effect::LoadSettings,
            // Meaningless from the dashboard; the settings screen is the
            // only thing `space` does anything for, and this branch is only
            // reached when that screen is not open.
            KeyPress::Cycle => Effect::None,
        }
    }

    /// The settings screen's own keymap, in force for as long as
    /// [`Self::settings`] is `Some`. Everything not named here is ignored,
    /// in particular an action key: no sheep action can arm while this
    /// screen owns the keyboard.
    fn on_settings_key(&mut self, key: KeyPress) -> Effect {
        self.notice = None;
        match key {
            KeyPress::Quit => return Effect::Quit,
            // Both close -- but an armed confirm eats the FIRST one instead,
            // the same cancel-before-act rule `on_key`'s own dashboard
            // branch already follows for `x`/`R`/`L`. Without this, `space`
            // then a reflexive `Escape` would close the whole screen on top
            // of a question the operator only meant to back out of.
            //
            // `s` toggling shut is the dashboard's own `s` meaning something
            // different once the screen is open, the same division
            // `Escape`'s doc argues for; `Escape` closing rather than
            // quitting is the one place this screen swaps the dashboard's
            // own cascade.
            KeyPress::Settings | KeyPress::Escape => {
                let armed = self.settings.as_ref().is_some_and(|settings| {
                    matches!(settings.pending, Some(Pending::Armed { .. }))
                });
                if armed {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.pending = None;
                    }
                } else {
                    self.settings = None;
                }
            }
            // An armed candidate eats the FIRST movement key rather than
            // also moving the cursor -- the same cancel-before-act rule
            // `on_key`'s own dashboard branch already follows for
            // `x`/`R`/`L` (see its "A stray `j` during a pending confirm"
            // comment): without it, the operator would see the prompt
            // vanish and the cursor move on the same keypress, and the
            // next reflexive Enter would apply an edit to a row they had
            // already lost track of. `Sent` is untouched, same as the
            // dashboard's own guard, which only fires on `Stage::Armed`:
            // a request already in flight is not cancellable by a keypress.
            KeyPress::SelectUp
            | KeyPress::SelectDown
            | KeyPress::SelectFirst
            | KeyPress::SelectLast => {
                if let Some(settings) = self.settings.as_mut() {
                    if matches!(settings.pending, Some(Pending::Armed { .. })) {
                        settings.pending = None;
                    } else {
                        match key {
                            KeyPress::SelectUp => settings.move_by(-1),
                            KeyPress::SelectDown => settings.move_by(1),
                            KeyPress::SelectFirst => settings.move_to_first(),
                            KeyPress::SelectLast => settings.move_to_last(),
                            _ => unreachable!(),
                        }
                    }
                }
            }
            KeyPress::Cycle => return self.cycle_setting(),
            KeyPress::Confirm => return self.confirm_setting(),
            // Re-reads `shep.toml`, so another process's write shows up --
            // the design spec's own table entry for `r`. `Msg::Settings`'s
            // own doc is what keeps the cursor from resetting to the first
            // row on the way back, the same as a landed write's reload.
            KeyPress::Refresh => return Effect::LoadSettings,
            // Unreachable from here, and named rather than wildcarded so a
            // future variant does not fall silently into an arm that
            // ignores it: an action key in particular must never be
            // mistaken for one this screen answers.
            KeyPress::Action(_)
            | KeyPress::FilterStart
            | KeyPress::TextChar(_)
            | KeyPress::TextBackspace
            | KeyPress::TextApply
            | KeyPress::TextAbandon => {}
        }
        Effect::None
    }

    /// `space` on the settings screen. Arms a candidate for the cursor's
    /// row, or refuses and says why the same way an action key does; the
    /// gate is the same read-only check `arm` makes for a sheep action,
    /// because a keystroke that mutates `shep.toml` needs the same fat-finger
    /// catch a keystroke that stops a sheep does. Re-arms rather than
    /// no-opping when a candidate is already armed for this row, so a
    /// second `space` walks one step further along the cycle -- see
    /// [`Settings::next_candidate`]'s own doc.
    ///
    /// Silently does nothing on [`SettingField::Socket`],
    /// [`SettingField::MaxCronSleep`] and a [`SettingsRow::Dog`] row: none
    /// of the three is this task's job, and a screen the operator can
    /// already read gives no sign that `space` was ever going to do
    /// anything there.
    fn cycle_setting(&mut self) -> Effect {
        if self.control == Control::ReadOnly {
            self.notice = Some(Notice {
                text: "read-only: actions need --allow-control".to_string(),
                grave: true,
            });
            return Effect::None;
        }
        let Some(settings) = self.settings.as_mut() else {
            return Effect::None;
        };
        let Some(SettingsRow::Scalar(field)) = settings.cursor() else {
            return Effect::None;
        };
        let Some(value) = settings.next_candidate(field) else {
            return Effect::None;
        };
        let text = confirm_text(field, &value);
        settings.pending = Some(Pending::Armed {
            edit: SettingEdit::Set { field, value },
            text,
            at: self.now,
        });
        Effect::None
    }

    /// The operator's `Enter` on the settings screen. Three meanings,
    /// picked in this order:
    ///
    /// - Nothing is pending and the cursor sits on [`SettingField::Socket`]
    ///   or [`SettingField::MaxCronSleep`]: opens [`Pending::Typing`],
    ///   seeded with that field's own on-disk value, and switches
    ///   [`InputMode::Text`] on -- [`Self::on_settings_text_key`] is what
    ///   answers every key from here on, not this function again.
    /// - Something is [`Pending::Armed`]: sends it and moves it to
    ///   [`Pending::Sent`], same as before this task.
    /// - Anything else (nothing pending on a cycled row, or already
    ///   `Sent`): untouched.
    fn confirm_setting(&mut self) -> Effect {
        let Some(settings) = self.settings.as_mut() else {
            return Effect::None;
        };
        if settings.pending.is_none()
            && let Some(SettingsRow::Scalar(
                field @ (SettingField::Socket | SettingField::MaxCronSleep),
            )) = settings.cursor()
        {
            let buffer = settings.text_seed(field).to_string();
            settings.pending = Some(Pending::Typing { field, buffer });
            self.mode = InputMode::Text;
            return Effect::None;
        }
        match settings.pending.take() {
            Some(Pending::Armed { edit, text, .. }) => {
                settings.pending = Some(Pending::Sent { text });
                Effect::WriteSetting(edit)
            }
            other => {
                settings.pending = other;
                Effect::None
            }
        }
    }

    /// Arms a confirm, or refuses and says why.
    ///
    /// Every refusal happens HERE rather than at confirm time, so an operator
    /// never answers a question that was never going to be honoured. Every
    /// sentence is literal: the standing rule is that nothing about damage
    /// gets charming, and a stop is damage.
    ///
    /// The ladder's order is the design's error table, read top to bottom:
    /// gate, link, nothing selected, one already in flight. It only shows when
    /// two conditions hold at once, and there is no reason to reorder an
    /// approved table.
    fn arm(&mut self, verb: ActionVerb) -> Effect {
        let refusal = if self.control == Control::ReadOnly {
            Some("read-only: actions need --allow-control".to_string())
        } else if let Link::Retrying { attempt } = self.link {
            // NOT `LINK_GONE` here: that sentence says the shepherd is gone,
            // which is false while it is still being redialled, and a
            // refusal saying so under a banner that says "reconnecting"
            // would be two contradictory claims on one frame. This is the
            // status bar's own sentence for the state (`view/status.rs`),
            // so the refusal agrees with the banner above it instead of
            // overriding it.
            Some(format!(
                "the shepherd stopped answering — reconnecting (attempt {attempt})"
            ))
        } else if matches!(self.link, Link::Lost { .. }) {
            // The same sentence `r` gives once the ladder is exhausted: at
            // that point the shepherd really is gone, so `LINK_GONE` is
            // literally true rather than merely reused for convenience.
            Some(LINK_GONE.to_string())
        } else if self.selected.is_none() {
            Some("no sheep is selected".to_string())
        } else if self.action.is_some() {
            Some("one action is already in flight".to_string())
        } else {
            None
        };
        if let Some(text) = refusal {
            self.notice = Some(Notice { text, grave: true });
            return Effect::None;
        }
        let key = self.selected.clone().expect("checked just above");
        let (target, name, count) = match &key {
            RowKey::Sheep(id) => {
                let row = self
                    .flock
                    .get(id)
                    .expect("a selected sheep is in the flock");
                (RowKey::Sheep(*id), row.info.name.clone(), 1)
            }
            RowKey::Group(group_name) => {
                let count = self
                    .flock
                    .values()
                    .filter(|row| &row.info.name == group_name)
                    .count();
                (RowKey::Group(group_name.clone()), group_name.clone(), count)
            }
        };
        self.action = Some(Action {
            verb,
            target,
            name,
            count,
            at: self.now,
            stage: Stage::Armed,
        });
        Effect::None
    }

    /// The operator's Enter. Sends, or refuses because the target left.
    fn confirm(&mut self) -> Effect {
        let Some(action) = self.action.take() else {
            return Effect::None;
        };
        // The whole flock, not the visible set: a filter typed after arming
        // hides a sheep, it does not remove it.
        if !self.target_present(&action.target) {
            self.notice = Some(Notice {
                text: format!(
                    "{}: it is no longer in the flock",
                    target_prefix(action.verb, &action.target, &action.name)
                ),
                grave: true,
            });
            return Effect::None;
        }
        let sent = Sent::Action {
            verb: action.verb,
            target: action.target.clone(),
            name: action.name.clone(),
        };
        self.action = Some(Action {
            stage: Stage::Sent,
            ..action
        });
        Effect::Send(sent)
    }

    /// Whether `target` still has at least one process in the flock: a
    /// single sheep by id, or a group by whether any instance of its name
    /// remains.
    fn target_present(&self, target: &RowKey) -> bool {
        match target {
            RowKey::Sheep(id) => self.flock.contains_key(id),
            RowKey::Group(name) => self.flock.values().any(|row| &row.info.name == name),
        }
    }

    /// Takes an armed prompt off the screen once its target is gone, rather
    /// than leaving a question about nothing. Called from the `Snapshot` arm
    /// and from the `Delete` arm; an action already in flight keeps its line.
    fn forget_missing_target(&mut self) {
        let gone = self.action.as_ref().is_some_and(|action| {
            action.stage == Stage::Armed && !self.target_present(&action.target)
        });
        if gone {
            self.action = None;
        }
    }

    /// Takes an armed prompt off the screen when the link stops being live.
    ///
    /// Called from the `Msg::Retrying` and `Msg::Frozen` arms. A9 refuses to
    /// ARM unless the link is `Live`; without this, a prompt armed a moment
    /// earlier would outlive the connection it was going to be sent over, and
    /// on a frozen dashboard it would never expire either, because `now` stops
    /// advancing and the expiry check rides it. Silent, like every other
    /// cancel: nothing happened, and the banner appearing on the same frame
    /// already says why.
    ///
    /// An action already SENT keeps its line. It is a real request, and
    /// `run_connected` answers it with an `Err` before its loop ends, so the
    /// in-flight line always resolves rather than hanging.
    fn disarm_on_link_change(&mut self) {
        if self
            .action
            .as_ref()
            .is_some_and(|action| action.stage == Stage::Armed)
        {
            self.action = None;
        }
    }

    /// The one text keymap's own router: the filter box while the
    /// settings screen is closed, [`Self::on_settings_text_key`]'s editor
    /// while it is open. A previous task closed the window where the two
    /// could both own [`InputMode::Text`] at once -- see `Msg::Settings`'s
    /// own arm -- so this split is total: exactly one of the two ever
    /// answers a given keypress.
    fn on_text_key(&mut self, key: KeyPress) -> Effect {
        if self.settings.is_some() {
            return self.on_settings_text_key(key);
        }
        self.on_filter_text_key(key)
    }

    /// The filter box's keymap.
    ///
    /// Ctrl-C still quits: in raw mode it is a key event and not a signal,
    /// and a text box that swallowed it would leave the operator reaching for
    /// `kill -9` from another window, past every restore path
    /// [`super::term`] has.
    ///
    /// Deliberately does NOT clear [`Self::notice`], unlike the normal-mode
    /// branch above it. A notice can be raised while this box is open, by
    /// `Msg::BusLagged`, `BusEvent::Dropped` or `BusEvent::DaemonShutdown`,
    /// none of which is a keypress; clearing here would destroy it because
    /// somebody was mid-word. The status bar hides it under the box instead
    /// and shows it when the box closes. See the phase plan's "Shapes the
    /// design named" #2.
    fn on_filter_text_key(&mut self, key: KeyPress) -> Effect {
        match key {
            KeyPress::Quit => Effect::Quit,
            KeyPress::TextChar(typed) => {
                let mut query = self.filter.clone();
                query.push(typed);
                self.set_filter(query)
            }
            KeyPress::TextBackspace => {
                let mut query = self.filter.clone();
                query.pop();
                self.set_filter(query)
            }
            KeyPress::TextApply => {
                self.mode = InputMode::Normal;
                Effect::None
            }
            KeyPress::TextAbandon => {
                self.mode = InputMode::Normal;
                self.set_filter(String::new())
            }
            _ => Effect::None,
        }
    }

    /// The settings editor's own text keymap, in force for as long as a
    /// [`Pending::Typing`] owns [`InputMode::Text`].
    ///
    /// Does not trim the buffer, on `TextChar` or on `TextBackspace`
    /// alike: this repository does not widen an accepted input grammar
    /// without a basis in the spec, the same rule
    /// [`Self::on_filter_text_key`]'s own filter buffer carries.
    ///
    /// `TextApply` arms rather than writes: an empty buffer becomes
    /// [`SettingEdit::Unset`], anything else becomes [`SettingEdit::Set`],
    /// and either way the edit moves to [`Pending::Armed`] rather than
    /// going out -- the operator's next `Enter`, on the now-closed editor,
    /// is what sends it, the same second-`Enter` shape every other confirm
    /// on this screen already uses.
    ///
    /// `TextAbandon` drops [`Pending::Typing`] back to `None` and leaves
    /// the screen open, matching [`KeyPress::Escape`]'s own doc: the
    /// cascade backs out of the innermost thing first.
    fn on_settings_text_key(&mut self, key: KeyPress) -> Effect {
        let now = self.now;
        let Some(settings) = self.settings.as_mut() else {
            return Effect::None;
        };
        match key {
            KeyPress::Quit => return Effect::Quit,
            KeyPress::TextChar(typed) => {
                if let Some(Pending::Typing { buffer, .. }) = settings.pending.as_mut() {
                    buffer.push(typed);
                }
            }
            KeyPress::TextBackspace => {
                if let Some(Pending::Typing { buffer, .. }) = settings.pending.as_mut() {
                    buffer.pop();
                }
            }
            KeyPress::TextApply => {
                if let Some(Pending::Typing { field, buffer }) = settings.pending.take() {
                    let edit = if buffer.is_empty() {
                        SettingEdit::Unset { field }
                    } else {
                        SettingEdit::Set {
                            field,
                            value: buffer,
                        }
                    };
                    let text = confirm_text_for_edit(&edit);
                    settings.pending = Some(Pending::Armed {
                        edit,
                        text,
                        at: now,
                    });
                }
                self.mode = InputMode::Normal;
            }
            KeyPress::TextAbandon => {
                settings.pending = None;
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
        Effect::None
    }

    /// The rows the table draws, in `(name, instance, id)` order: the whole
    /// flock, or whatever the filter leaves of it, as [`RowKey`]s rather than
    /// as a flat list of ids.
    ///
    /// An app earns a [`RowKey::Group`] header, immediately before its own
    /// [`RowKey::Sheep`] entries, under the same condition
    /// `output::rows::FlockRows`'s own `name_groups`/`slotted` rule uses
    /// (task 9): more than one instance of the name, every one of them
    /// reporting a slot. The listing is sorted by `(name, instance, id)`
    /// here for exactly that reason -- `sort_flock`'s own doc is why an
    /// app's instances arrive adjacent and in slot order upstream, and this
    /// sequence keeps that ordering rather than the plainer `(name, id)`
    /// this function used before grouping existed.
    ///
    /// The order is otherwise the one every operator-facing shep listing
    /// takes. `flock` is a `BTreeMap<u32, Row>`, so iterating it is id
    /// order, which is why the sequence is materialised and re-keyed here
    /// rather than taken from the map. That matters more in this pane than
    /// anywhere else: the table repolls every two seconds, so a key that is
    /// not total would let two instances of one app swap places under the
    /// operator's cursor between refreshes.
    ///
    /// [`Self::select_at`], [`Self::select_by`], [`Self::reseat`] and
    /// [`Self::selected_index`] all read this sequence and nothing else.
    /// That is the whole point of it: a filter that hid rows `j` and `k`
    /// still stepped over would move the cursor onto a sheep nobody can see,
    /// and every pane below the table would then describe that sheep with
    /// nothing on screen saying so. A query narrows by NAME, so it can never
    /// split one app's instances across the filter boundary, which is what
    /// keeps a [`RowKey::Group`] and its own slots always adjacent here too.
    #[must_use]
    pub fn visible_rows(&self) -> Vec<RowKey> {
        let needle = self.filter.to_lowercase();
        let mut visible: Vec<(&str, Option<u32>, u32)> = self
            .flock
            .iter()
            .filter(|(_, row)| needle.is_empty() || row.info.name.to_lowercase().contains(&needle))
            .map(|(id, row)| (row.info.name.as_str(), row.info.instance, *id))
            .collect();
        visible.sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

        let mut out = Vec::with_capacity(visible.len());
        let mut at = 0;
        while at < visible.len() {
            let name = visible[at].0;
            let end = visible[at..]
                .iter()
                .position(|entry| entry.0 != name)
                .map_or(visible.len(), |offset| at + offset);
            let group = &visible[at..end];
            if self.is_grouped(name) {
                out.push(RowKey::Group(name.to_string()));
            }
            out.extend(group.iter().map(|entry| RowKey::Sheep(entry.2)));
            at = end;
        }
        out
    }

    /// How many rows the table draws.
    fn visible_len(&self) -> usize {
        self.visible_rows().len()
    }

    /// Puts the selection back on a real row after the flock changed.
    ///
    /// `previous_index` is where the selection sat **before** the change, read
    /// while the old map was still in place. A selection whose key survived is
    /// left alone, whatever row it now occupies. One that did not falls to
    /// whatever occupies the same position, clamped to the last row — not to
    /// row 0, which would throw an operator back to the top of a
    /// two-hundred-sheep flock every time an unrelated sheep was deleted.
    ///
    /// Returns whether the selection changed, which is what decides between
    /// [`Effect::RefreshSelected`] and [`Effect::None`].
    fn reseat(&mut self, previous_index: Option<usize>) -> bool {
        // `selected_index`, NOT `flock.contains_key`: a selection the filter
        // is hiding is not seated, however present its id still is in the map.
        // This one line is the difference between a cursor that follows the
        // filter and one that wanders behind it.
        //
        // It must come BEFORE the `flock.is_empty()` check below: with the
        // emptiness test above it instead, reverting this line changes
        // nothing when the query matches no sheep, because the early
        // return has already fired -- so this order is what makes the
        // nothing-matches case reachable at all. The order is otherwise
        // behaviour-preserving (a seated selection implies at least one
        // visible row).
        if self.selected_index().is_some() {
            return false;
        }
        let before = self.selected.clone();
        let visible = self.visible_rows();
        if visible.is_empty() {
            self.selected = None;
            return before != self.selected;
        }
        let index = previous_index.unwrap_or(0).min(visible.len() - 1);
        self.selected = Some(visible[index].clone());
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
    /// `Effect::None` when it did not: [`Effect::RefreshSelected`] reads two
    /// files off disk and asks the shepherd for lambs, and a held `k` at the
    /// top of the flock must not do that once per keypress.
    fn select_at(&mut self, index: usize) -> Effect {
        let visible = self.visible_rows();
        if visible.is_empty() {
            return Effect::None;
        }
        let next = visible[index.min(visible.len() - 1)].clone();
        if Some(&next) == self.selected.as_ref() {
            return Effect::None;
        }
        self.selected = Some(next);
        // The cursor moved; whether anything is READ is a separate question.
        // A frozen dashboard re-reading live log files would put content on
        // screen newer than the banner saying the values are frozen, which is
        // the contradiction design decision 7 refuses for the host strip. The
        // detail pane still re-renders, from the frozen listing — that is data
        // already on the frame.
        if matches!(self.link, Link::Lost { .. }) {
            return Effect::None;
        }
        Effect::RefreshSelected
    }

    /// Every sheep the table's rows are drawn from, in name-then-id order:
    /// the whole flock, or whatever the filter leaves of it. See
    /// [`Self::all_rows`] for the unfiltered sequence a pane describing the
    /// machine as a whole, not the table's current view, needs instead.
    ///
    /// A flat sheep list, not [`Self::visible_rows`]'s own [`RowKey`]
    /// sequence: this is what the title bar and the host-strip tests count,
    /// and a group header is not a sheep to count twice against.
    #[must_use]
    pub fn rows(&self) -> Vec<&Row> {
        let needle = self.filter.to_lowercase();
        let mut visible: Vec<&Row> = self
            .flock
            .values()
            .filter(|row| needle.is_empty() || row.info.name.to_lowercase().contains(&needle))
            .collect();
        visible.sort_unstable_by(|a, b| {
            (a.info.name.as_str(), a.info.id).cmp(&(b.info.name.as_str(), b.info.id))
        });
        visible
    }

    /// Every sheep the shepherd last reported, in id order, whatever the
    /// filter hides.
    ///
    /// Id order, unlike [`Self::rows`]: nothing renders this sequence as a
    /// list. The host strip sums it, and a sum does not care what order it
    /// arrives in.
    ///
    /// The host strip reads this rather than [`Self::rows`] so a name filter
    /// cannot narrow what `flock cpu`/`flock mem` sum while the strip stays
    /// labelled `flock` -- see `view::host`'s own module doc for why. The
    /// title bar already carries the filtered-vs-total distinction
    /// (`2 of 6 in the flock`) so the strip does not have to.
    #[must_use]
    pub fn all_rows(&self) -> Vec<&Row> {
        self.flock.values().collect()
    }

    /// Replaces the filter and puts the selection back on a visible sheep.
    ///
    /// Private, and its only production caller is [`Self::on_key`]'s text-mode
    /// arm (Task 2). The reseat is the whole reason this is not a plain field
    /// assignment: a keystroke that narrows the query can hide the selected
    /// sheep, and the selection then falls to whatever occupies the same
    /// position, clamped to the last visible row, which is `reseat`'s shipped
    /// rule applied to a new cause.
    fn set_filter(&mut self, query: String) -> Effect {
        if self.filter == query {
            return Effect::None;
        }
        let previous = self.selected_index();
        self.filter = query;
        if self.reseat(previous) && !matches!(self.link, Link::Lost { .. }) {
            // The cursor moved, so the feed and the lambs are about to
            // describe a different sheep. A frozen dashboard reads nothing,
            // for the reason `select_at` already states.
            return Effect::RefreshSelected;
        }
        Effect::None
    }

    /// The filter as typed, empty when there is none.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Which keymap is currently in force, for `super::run_ui`'s call to
    /// [`super::input::map_key`].
    #[must_use]
    pub fn mode(&self) -> InputMode {
        self.mode
    }

    /// How many sheep the shepherd last reported, whatever the filter hides.
    ///
    /// The title reads this beside `rows().len()`. A title that could only
    /// read the narrowed count would understate the flock while a filter is
    /// on, which is the confident wrong number this dashboard's `-` cells and
    /// its frozen uptime clock both exist to avoid.
    #[must_use]
    pub fn flock_len(&self) -> usize {
        self.flock.len()
    }

    /// The selected row, or `None` for an empty flock.
    #[must_use]
    pub fn selected(&self) -> Option<RowKey> {
        self.selected.clone()
    }

    /// Which row of [`Self::visible_rows`] the selection sits on.
    ///
    /// Derived every call rather than stored: see [`Self::selected`].
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let key = self.selected.clone()?;
        self.visible_rows().iter().position(|row| *row == key)
    }

    /// The selected sheep's row, which the detail pane and the feed read.
    ///
    /// `None` for a [`RowKey::Group`] selection as well as for no selection
    /// at all: a group has no single sheep to describe, and the panes that
    /// call this read the `None` case as their own "nothing to show" state.
    #[must_use]
    pub fn selected_row(&self) -> Option<&Row> {
        match &self.selected {
            Some(RowKey::Sheep(id)) => self.flock.get(id),
            _ => None,
        }
    }

    /// One sheep by id, whatever the filter hides -- the lookup a
    /// [`RowKey::Sheep`] row's own rendering needs.
    #[must_use]
    pub fn row(&self, id: u32) -> Option<&Row> {
        self.flock.get(&id)
    }

    /// Every instance of `name`, sorted by slot -- the members a
    /// [`RowKey::Group`] row summarises.
    #[must_use]
    pub fn group_members(&self, name: &str) -> Vec<&Row> {
        let mut members: Vec<&Row> = self
            .flock
            .values()
            .filter(|row| row.info.name == name)
            .collect();
        members.sort_by_key(|row| row.info.instance.unwrap_or(u32::MAX));
        members
    }

    /// Whether `name`'s instances draw under a [`RowKey::Group`] header.
    ///
    /// The one condition, stated once: more than one instance of the name,
    /// every one of them reporting a slot. [`Self::visible_rows`] decides
    /// whether to emit the header from it and the table decides how to render
    /// a slot row underneath from the same call, so the header and its rows
    /// cannot disagree about which shape they are in.
    ///
    /// Read over the whole flock rather than over the filtered sequence, and
    /// that is not a discrepancy: a query narrows by NAME, so it either keeps
    /// every instance of an app or none of them.
    #[must_use]
    pub fn is_grouped(&self, name: &str) -> bool {
        let members = self.group_members(name);
        members.len() > 1 && members.iter().all(|row| row.info.instance.is_some())
    }

    /// `name`'s rolled-up numbers. See [`GroupTotals`]'s own doc for the
    /// rule each field follows.
    #[must_use]
    pub fn group_totals(&self, name: &str) -> GroupTotals {
        let members = self.group_members(name);
        GroupTotals {
            count: members.len(),
            restarts: members.iter().map(|row| row.info.restarts).sum(),
            cpu: members
                .iter()
                .filter_map(|row| row.info.cpu_percent)
                .fold(None, |acc, cpu| Some(acc.unwrap_or(0.0) + cpu)),
            memory: members
                .iter()
                .filter_map(|row| row.info.memory_bytes)
                .fold(None, |acc, mem| Some(acc.unwrap_or(0) + mem)),
            uptime_ms: members
                .iter()
                .filter_map(|row| self.uptime_ms(row.info.id))
                .min(),
        }
    }

    /// `name`'s STATUS cell: the shared status word when every instance
    /// agrees, else a count per state -- the same rule
    /// `output::rows::group_status` applies for `shep flock` (task 9), kept
    /// here so the two surfaces read a mixed group the same way.
    ///
    /// Reads `ProcStatus` directly, never [`Row::reported`]: `is_grouped`
    /// requires every member's `instance` to be `Some`, and a dog is never
    /// stocked to several instances, so no group this method can see has a
    /// handshake to report. Same argument `output::rows::group_paint`
    /// already makes for the table's own group row (task 5's own plan entry
    /// spells this out rather than leaving it to be re-derived).
    #[must_use]
    pub fn group_status_text(&self, name: &str) -> String {
        let members = self.group_members(name);
        let Some(first) = members.first().map(|row| row.info.status) else {
            return String::new();
        };
        if members.iter().all(|row| row.info.status == first) {
            return first.to_string();
        }
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for row in &members {
            *counts.entry(row.info.status.to_string()).or_default() += 1;
        }
        counts
            .into_iter()
            .map(|(status, n)| format!("{n} {status}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `name`'s status, when every instance agrees on one -- what
    /// [`view::flock`](super::view::flock)'s STATUS colouring and
    /// [`view::detail`](super::view::detail)'s own status word key off,
    /// mirroring `output::rows::group_paint`'s "a mixed group's plain count
    /// text wears no colour" rule.
    ///
    /// `Option<ProcStatus>`, not `Option<Reported>`, for the same reason
    /// [`Self::group_status_text`] reads `ProcStatus` directly above: a dog
    /// is never stocked to several instances, so a group row has no
    /// handshake to report and nothing here is ever silent.
    #[must_use]
    pub fn group_uniform_status(&self, name: &str) -> Option<ProcStatus> {
        let members = self.group_members(name);
        let first = members.first()?.info.status;
        members
            .iter()
            .all(|row| row.info.status == first)
            .then_some(first)
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

    /// The lamb reading for sheep `id`, with its age in milliseconds as of
    /// this dashboard's own clock.
    ///
    /// `None` when there is no reading, or when the one there is was taken for
    /// a different sheep. The age comes from [`Self::now`], the same clock the
    /// uptime column reads, which means it stops when the dashboard freezes,
    /// for the same reason.
    #[must_use]
    pub fn lambs_for(&self, id: u32) -> Option<(&LambWalk, u64)> {
        let reading = self.lambs.as_ref().filter(|reading| reading.id == id)?;
        Some((
            &reading.walk,
            millis(self.now.saturating_duration_since(reading.at)),
        ))
    }

    /// The action in progress, for the status bar.
    #[must_use]
    pub fn action(&self) -> Option<ActionState<'_>> {
        let action = self.action.as_ref()?;
        Some(ActionState {
            verb: action.verb,
            target: &action.target,
            name: &action.name,
            count: action.count,
            sent: action.stage == Stage::Sent,
        })
    }

    /// The settings screen's own state, or `None` while the dashboard is
    /// showing.
    #[must_use]
    pub fn settings(&self) -> Option<&Settings> {
        self.settings.as_ref()
    }

    /// The resolved style level and which layer chose it. What
    /// `Effect::LoadSettings` hands `commands::settings::load_settings`
    /// for the STYLE LEVEL row, so the screen never re-resolves on its own.
    #[must_use]
    pub fn style(&self) -> (StyleLevel, StyleSource) {
        self.style
    }

    /// Sets the resolved style level and its source. Called exactly once,
    /// by `run_argv`'s `lookout` dispatch, right after construction: `App`
    /// has no way to resolve this itself (it holds no `GlobalArgs` and
    /// reads no files), so the caller that already computed it hands it
    /// over rather than this type inventing a second, divergent way to get
    /// one.
    pub(crate) fn set_style(&mut self, style: (StyleLevel, StyleSource)) {
        self.style = style;
    }

    /// Overrides the control gate a fixture built with. `App::new` takes
    /// [`Control`] and every shipped fixture hard-codes [`Control::ReadOnly`];
    /// this is the one line that lets the action-key tests build a dashboard
    /// with the gate open without a second copy of `app_with`.
    #[cfg(test)]
    pub(crate) fn set_control_for_tests(&mut self, control: Control) {
        self.control = control;
    }

    /// Sets the filter directly, bypassing the reseat [`Self::set_filter`]
    /// does. `set_filter` is private to this module; this is the one line
    /// that lets `view::host`'s tests (a sibling module, not a descendant)
    /// build a dashboard with a filter already applied, the same shape
    /// [`Self::set_control_for_tests`] takes for the control gate.
    #[cfg(test)]
    pub(crate) fn set_filter_for_tests(&mut self, query: &str) {
        self.filter = query.to_string();
    }

    /// Selects `key` directly, without walking the cursor. No production
    /// caller needs this: every real selection change arrives by walking
    /// (`select_by`/`select_at`) or by a snapshot's own [`Self::reseat`].
    /// This exists only so a test can point the cursor at a specific group
    /// or sheep without simulating keypresses.
    #[cfg(test)]
    fn select(&mut self, key: RowKey) {
        self.selected = Some(key);
    }
}

/// The prefix every action's notice shares: which verb, and which target.
///
/// A single sheep keeps the `(id N)` form unchanged from before this feature;
/// a group names the app instead of an id, since there is no one id for the
/// notice to name.
fn target_prefix(verb: ActionVerb, target: &RowKey, name: &str) -> String {
    match target {
        RowKey::Sheep(id) => format!("{} {name} (id {id})", verb.label()),
        RowKey::Group(_) => format!("{} all instances of {name}", verb.label()),
    }
}

/// Saturating `Duration` -> milliseconds. A lookout left open for 580 million
/// years is not the failure this guards; the cast is what clippy's
/// `cast_possible_truncation` would otherwise deny.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What the bar says once the shepherd has answered.
///
/// Reload's wording is not decoration. `Response::Reloading` is an acceptance
/// and not a result, and the swaps arrive afterwards on the bus, which the
/// table already consumes.
const fn outcome(verb: ActionVerb) -> &'static str {
    match verb {
        ActionVerb::Stop => "the shepherd stopped it",
        ActionVerb::Restart => "the shepherd restarted it",
        ActionVerb::Reload => "accepted, the swaps report themselves as they happen",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::{ProcessEventKind, RpcError, RpcErrorCode};

    use super::super::view::fixtures;
    use crate::commands::settings::ScalarView;

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
            "/home/ada/.shep".to_string(),
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

    /// `started()`'s three sheep with the gate open and the cursor parked in
    /// the middle, on `web` at id 1.
    ///
    /// The table reads by name (`App::visible_ids`), so the middle row is the
    /// middle NAME: `api` 2, `web` 1, `worker` 3. The ids are deliberately
    /// left disagreeing with the display order, so a test that walks the
    /// cursor and then asserts an id cannot pass by reading the map.
    ///
    /// Mid-list on purpose. Half the tests below assert that a stray `j` did
    /// NOT move the cursor, and a cursor already clamped at either end would
    /// pass those tests whether the routing rule consumed the key or not.
    fn allowed() -> App {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(2, "api", ProcStatus::Online),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        app.update(Msg::Tick { now: t0 });
        app.update(Msg::Key(KeyPress::SelectDown));
        app
    }

    /// `allowed()`'s shape, but with three instances of one app instead of
    /// three distinct ones: `web` at slots 0, 1 and 2, ids 1 through 3.
    /// Nothing is selected here -- which row a `RowKey::Group` occupies is
    /// exactly what the group tests are checking, so each one calls
    /// `App::select` itself rather than trusting a walk to land there.
    fn allowed_with_instances() -> App {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: instanced_rows(),
            at: t0,
        });
        app
    }

    /// `web`'s three instances, at slots 0, 1 and 2 and ids 1 through 3.
    /// Shared by [`allowed_with_instances`] and the reseat test's own second
    /// `Msg::Snapshot`, so a poll that repeats the same listing is the one
    /// under test rather than the flock actually changing shape.
    fn instanced_rows() -> Vec<ProcessInfo> {
        (0..3)
            .map(|slot| {
                ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                    .instance(Some(slot))
                    .build()
            })
            .collect()
    }

    /// The status bar's own rendered text, for the tests that assert on the
    /// confirm prompt's wording rather than on the reducer's internal state.
    fn status_line_text(app: &App) -> String {
        super::super::view::fixtures::rendered(&super::super::view::status::status_line(app, 200))
    }

    /// fails if an app with more than one instance does not get a group
    /// header above its own slots.
    #[test]
    fn a_multi_instance_app_shows_a_group_row_above_its_slots() {
        let app = allowed_with_instances();
        assert_eq!(
            app.visible_rows().len(),
            4,
            "three slots and the group row above them"
        );
        assert!(matches!(app.visible_rows()[0], RowKey::Group(ref n) if n == "web"));
    }

    /// fails if an action armed on a group row sends against one instance
    /// instead of the whole app. This is the test that would redden if
    /// targeting regressed to `SelectorSpec::Id` for a group: the request
    /// this asserts on can only be built from `RowKey::Group`, never from a
    /// single pinned id.
    #[test]
    fn an_action_on_a_group_row_targets_the_whole_app_by_name() {
        let mut app = allowed_with_instances();
        app.select(RowKey::Group("web".to_string()));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter sends");
        };
        assert_eq!(
            sent.request(),
            Request::Stop {
                selector: SelectorSpec::Name("web".to_string())
            }
        );
    }

    /// fails if the confirm prompt does not say how many processes a group
    /// action reaches before the operator commits. The one place a keypress
    /// reaches several processes is the one place the prompt has to say so.
    #[test]
    fn a_group_confirm_states_how_many_processes_it_reaches() {
        let mut app = allowed_with_instances();
        app.select(RowKey::Group("web".to_string()));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        let prompt = status_line_text(&app);
        assert!(prompt.contains('3'), "names the blast radius: {prompt}");
    }

    /// fails if a group selection does not survive the two-second poll. The
    /// cursor has to reseat onto the SAME group, not fall back to whatever
    /// row now occupies its old position.
    #[test]
    fn selection_survives_a_poll_on_both_row_kinds() {
        let mut app = allowed_with_instances();
        app.select(RowKey::Group("web".to_string()));
        app.update(Msg::Snapshot {
            rows: instanced_rows(),
            at: Instant::now(),
        });
        assert_eq!(app.selected(), Some(RowKey::Group("web".to_string())));
    }

    /// fails if `arm`'s read-only refusal stops firing for a group row. The
    /// gate exists so a keystroke in a dashboard somebody is reading does
    /// not become an action, and that has to hold for a keypress that would
    /// reach three processes exactly as much as for one that reaches one.
    #[test]
    fn arming_a_group_action_refuses_when_read_only() {
        let mut app = allowed_with_instances();
        app.set_control_for_tests(Control::ReadOnly);
        app.select(RowKey::Group("web".to_string()));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert!(app.action().is_none());
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("read-only: actions need --allow-control")
        );
    }

    /// fails if a group action can be armed while the link is not live. The
    /// mirror of `every_action_key_refuses_while_the_link_is_not_live`, for
    /// a `RowKey::Group` selection: A9's reasoning (an action typed during
    /// the reconnect ladder would queue and land on a connection the
    /// operator has stopped watching) does not know or care which kind of
    /// row is selected.
    #[test]
    fn arming_a_group_action_refuses_while_the_link_is_not_live() {
        for link in [
            Msg::Retrying { attempt: 2 },
            Msg::Frozen {
                at_local: "2026-08-16 09:00:00".to_string(),
            },
        ] {
            let mut app = allowed_with_instances();
            app.select(RowKey::Group("web".to_string()));
            app.update(link);
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
            assert!(app.action().is_none());
            assert!(app.notice().is_some_and(Notice::is_grave));
        }
    }

    /// fails if a second action can be armed on a group row while one is
    /// already in flight. The mirror of
    /// `a_second_action_refuses_while_one_is_in_flight` for a
    /// `RowKey::Group` target: the in-flight line names one action, and a
    /// second one racing it would make it ambiguous which (A12).
    #[test]
    fn arming_a_group_action_refuses_while_one_is_already_in_flight() {
        let mut app = allowed_with_instances();
        app.select(RowKey::Group("web".to_string()));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        assert!(app.action().is_some_and(|action| action.sent));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("one action is already in flight")
        );
        let action = app.action().expect("the first one is untouched");
        assert_eq!(action.verb, ActionVerb::Stop);
        assert!(action.sent);
    }

    /// fails if an action key acts. It arms, and nothing has been sent: the
    /// whole point of the gate is that one keystroke in a dashboard somebody
    /// is reading does not become an action.
    #[test]
    fn an_action_key_arms_a_confirm_and_sends_nothing() {
        let mut app = allowed();
        assert_eq!(
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop))),
            Effect::None
        );
        let armed = app.action().expect("armed");
        assert_eq!(armed.verb, ActionVerb::Stop);
        assert_eq!(armed.target, &RowKey::Sheep(1));
        assert_eq!(armed.name, "web");
        assert!(!armed.sent, "nothing has gone out");
    }

    /// fails if the action key itself confirms, or if only `Esc` cancels. A
    /// confirm the action key could complete is the double-tap the gate exists
    /// to catch, on a keyboard that may be repeating.
    #[test]
    fn only_enter_confirms_and_every_other_key_cancels() {
        for key in [
            KeyPress::SelectDown,
            KeyPress::SelectUp,
            KeyPress::SelectFirst,
            KeyPress::Refresh,
            KeyPress::Escape,
            KeyPress::FilterStart,
            KeyPress::Action(ActionVerb::Stop),
            KeyPress::Action(ActionVerb::Restart),
        ] {
            let mut app = allowed();
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
            assert!(app.action().is_some(), "armed before {key:?}");
            assert_eq!(
                app.update(Msg::Key(key)),
                Effect::None,
                "{key:?} sent something"
            );
            assert!(app.action().is_none(), "{key:?} did not cancel");
        }

        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert!(
            matches!(app.update(Msg::Key(KeyPress::Confirm)), Effect::Send(_)),
            "and Enter is the one key that sends"
        );
    }

    /// fails if a cancelling keypress ALSO does its ordinary job. This is the
    /// failure mode the whole feature is about: the operator would see the
    /// prompt vanish and the cursor move, and the next reflexive Enter would
    /// act on a target they had already lost track of.
    ///
    /// Three assertions, because each catches a different half of the bug: the
    /// prompt is gone, the cursor did NOT move, and no effect leaked out. The
    /// selection is parked mid-list by `allowed()` so a `j` genuinely could
    /// move it.
    #[test]
    fn a_cancelling_key_is_consumed_and_does_not_also_move_the_selection() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        let before = app.selected();
        let effect = app.update(Msg::Key(KeyPress::SelectDown));
        assert!(app.action().is_none(), "the stray j cancelled the confirm");
        assert_eq!(app.selected(), before, "and did not also move the cursor");
        assert_eq!(effect, Effect::None, "nor ask for a feed read or a walk");
    }

    /// fails if the confirm re-reads the cursor at Enter time. A snapshot can
    /// land between the arming keypress and the Enter, and a confirmation
    /// built from `self.selected` would then act on a sheep the operator never
    /// pointed at.
    ///
    /// Arming on id 2 and having a snapshot delete id 2's NEIGHBOUR does
    /// NOT exercise this: `reseat`'s own rule is "an id that survived is
    /// left alone, whatever row it now occupies" — id 2 survives every
    /// such snapshot, so `self.selected` stays 2 right alongside
    /// `action.id`, and the mutation this test exists to catch (reading
    /// `self.selected` instead of the pinned `action.id`) could not redden
    /// it.
    ///
    /// This version applies a filter before arming, then has the snapshot
    /// RENAME the armed sheep out of that filter while a second sheep enters
    /// it. The armed id (2) still survives in `self.flock` — a rename is not
    /// a delete, so `confirm`'s "did the target leave" check must not fire —
    /// but it drops out of `visible_ids()`, so `reseat` moves the cursor to
    /// the other match. That genuinely separates `self.selected` (9) from
    /// `action.id` (2), which is what makes the pin observable: the fixed
    /// code sends id 2 under its arm-time name "api", and the mutation Task 7
    /// tried would instead send id 9 under whatever the snapshot named it.
    #[test]
    fn the_confirm_is_pinned_to_the_id_it_was_armed_on() {
        let mut app = allowed();
        app.set_filter("api".to_string());
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(2, "gateway", ProcStatus::Online),
                sheep(9, "api-new", ProcStatus::Online),
            ],
            at: Instant::now(),
        });
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(9)),
            "sanity: the cursor followed the filter off the armed id"
        );
        let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter sends");
        };
        assert_eq!(
            sent,
            Sent::Action {
                verb: ActionVerb::Stop,
                target: RowKey::Sheep(2),
                name: "api".to_string()
            }
        );
    }

    /// fails if a confirm whose sheep left the flock sends anyway. The one
    /// refusal that has to happen at confirm time rather than at arm time.
    #[test]
    fn a_confirm_whose_sheep_left_the_flock_refuses_instead_of_sending() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Event(BusEvent::Process {
            event: ProcessEventKind::Delete,
            info: sheep(1, "web", ProcStatus::Stopped),
            manually: true,
            at_ms: 0,
        }));
        // The prompt came off the screen as soon as the reducer learned the
        // sheep was gone, rather than sitting there as a question about
        // nothing.
        assert!(app.action().is_none());
        assert_eq!(app.update(Msg::Key(KeyPress::Confirm)), Effect::None);
    }

    /// fails if a prompt left armed while the operator walks away never
    /// expires. Driven by `Msg::Tick`, so there is no sleep here (IR-33).
    #[test]
    fn a_confirm_expires_after_ten_seconds_of_ticks() {
        let mut app = allowed();
        let t0 = Instant::now();
        app.update(Msg::Tick { now: t0 });
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Reload)));
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(9),
        });
        assert!(app.action().is_some(), "nine seconds is still armed");
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(10),
        });
        assert!(app.action().is_none(), "ten is not");
    }

    /// fails if arming is allowed while the link is not live. An action typed
    /// during the eight second reconnect ladder would otherwise queue and land
    /// seconds later, on a connection the operator has stopped watching (A9).
    #[test]
    fn every_action_key_refuses_while_the_link_is_not_live() {
        for link in [
            Msg::Retrying { attempt: 2 },
            Msg::Frozen {
                at_local: "2026-08-16 09:00:00".to_string(),
            },
        ] {
            let mut app = allowed();
            app.update(link);
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
            assert!(app.action().is_none());
            assert!(app.notice().is_some_and(Notice::is_grave));
        }
    }

    /// fails if a second action can be armed while one is in flight. The
    /// in-flight line names one action; a second one would make it ambiguous
    /// which (A12).
    #[test]
    fn a_second_action_refuses_while_one_is_in_flight() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        assert!(app.action().is_some_and(|action| action.sent));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("one action is already in flight")
        );
        let action = app.action().expect("the first one is untouched");
        assert_eq!(action.verb, ActionVerb::Stop);
        assert!(action.sent);
    }

    /// fails if the in-flight line is stored as a notice. A notice is cleared
    /// by the next keypress, and an in-flight action whose only sign on screen
    /// could be wiped by a stray `j` is a dashboard hiding something it knows.
    #[test]
    fn an_in_flight_line_survives_a_keypress() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Key(KeyPress::SelectDown));
        assert!(
            app.action().is_some_and(|action| action.sent),
            "the keypress moved the cursor and left the in-flight state alone"
        );
    }

    /// fails if `q` or Ctrl-C stops quitting while a prompt is up. The routing
    /// rule consumes every other key; this is the one carve-out, and
    /// `input.rs`'s own doc says why: an operator whose most reflexive way out
    /// of a terminal program stops working reaches for `kill -9` from another
    /// window, past every restore path `term` has. Quitting discards the
    /// confirm rather than acting on it, so nothing this rule protects is
    /// weakened. See the phase plan's "Shapes the design named" #4.
    #[test]
    fn quit_still_quits_while_a_confirm_is_armed() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert_eq!(app.update(Msg::Key(KeyPress::Quit)), Effect::Quit);
    }

    /// fails if Enter does something when nothing is armed. It reaches the
    /// ordinary match in two states an operator can be in: nothing armed at
    /// all, and one action already in flight. Neither may send.
    #[test]
    fn enter_outside_an_armed_confirm_does_nothing() {
        let mut app = allowed();
        assert_eq!(app.update(Msg::Key(KeyPress::Confirm)), Effect::None);
        assert!(app.action().is_none());

        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        assert!(app.action().is_some_and(|action| action.sent), "in flight");
        assert_eq!(
            app.update(Msg::Key(KeyPress::Confirm)),
            Effect::None,
            "a second Enter does not re-send"
        );
    }

    /// fails if a request the channel could not take leaves the dashboard
    /// claiming an action is in flight forever, which would also refuse every
    /// later action for the life of the process.
    #[test]
    fn a_request_that_could_not_be_sent_says_so_and_clears_the_state() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter sends");
        };
        app.update(Msg::Unsent { sent });
        assert!(app.action().is_none());
        assert!(app.notice().is_some_and(Notice::is_grave));
    }

    /// fails if an armed prompt outlives the connection it was going to be
    /// sent over. Both halves matter: `Retrying` because A9 says an action
    /// typed during the reconnect ladder is refused rather than queued, and
    /// `Frozen` because a frozen dashboard's `now` stops advancing, so an
    /// armed prompt there would never expire either.
    #[test]
    fn a_link_that_stops_being_live_takes_an_armed_prompt_down() {
        for link in [
            Msg::Retrying { attempt: 2 },
            Msg::Frozen {
                at_local: "2026-08-16 09:00:00".to_string(),
            },
        ] {
            let mut app = allowed();
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
            assert!(app.action().is_some(), "armed while live");
            app.update(link);
            assert!(app.action().is_none(), "and gone once the link is not");
            assert_eq!(
                app.update(Msg::Key(KeyPress::Confirm)),
                Effect::None,
                "so Enter has nothing to send"
            );
        }
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
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(3)),
            "the third row, worker"
        );

        // Sheep 1 goes away. `worker` is now row 1 rather than row 2 — an
        // index cursor would now be pointing at `api`.
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(2, "api", ProcStatus::Errored),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        assert_eq!(app.selected(), Some(RowKey::Sheep(3)), "still worker");
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
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(1)),
            "web, at index 1 by name"
        );

        // web dies; api and worker remain. Index 1 is now worker.
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(2, "api", ProcStatus::Online),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(3)),
            "the row that took index 1"
        );

        // The LAST row dying clamps rather than leaving the cursor past the end.
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected(), Some(RowKey::Sheep(3)));
        app.update(Msg::Snapshot {
            rows: vec![sheep(2, "api", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.selected(), Some(RowKey::Sheep(2)));

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
    /// is the one that matters: `RefreshSelected` reads two files off disk
    /// and asks the shepherd for lambs, and a held `k` at the top of the
    /// flock must not do that once per keypress.
    #[test]
    fn a_selection_that_moves_refreshes_the_feed_and_one_that_cannot_does_not() {
        let (mut app, _) = started();
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::RefreshSelected
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectFirst)),
            Effect::RefreshSelected
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectUp)),
            Effect::None,
            "already at the top: nothing moved, so nothing is re-read"
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectLast)),
            Effect::RefreshSelected
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::None,
            "already at the bottom"
        );
    }

    /// fails if a moved selection stops asking for lambs. The pane describes
    /// the selected sheep, so the trigger is the selection changing and
    /// nothing else.
    #[test]
    fn moving_the_selection_asks_for_lambs() {
        let (mut app, _t0) = started();
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::RefreshSelected
        );
    }

    /// fails if the two-second listing starts asking for lambs, which is the
    /// whole cost decision inverted. `with_lambs`'s own doc says `ListFlock`
    /// declines the walk because a flock listing is what an operator leaves
    /// running in a loop; a dashboard putting a full machine enumeration on a
    /// fixed 2s clock, times however many lookout windows are open, is the
    /// daemon paying exactly the cost its own code was written to avoid.
    #[test]
    fn a_snapshot_refreshes_the_feed_and_does_not_ask_for_lambs() {
        let (mut app, t0) = started();
        assert_eq!(
            app.update(Msg::Snapshot {
                rows: vec![sheep(1, "web", ProcStatus::Online)],
                at: t0,
            }),
            Effect::RefreshFeed
        );
    }

    /// fails if a frozen dashboard asks the shepherd for anything. Inherited
    /// from `select_at`'s shipped rule rather than restated: the link is gone,
    /// so there is nothing to ask.
    #[test]
    fn nothing_is_requested_while_the_link_is_lost() {
        let (mut app, _t0) = started();
        app.update(Msg::Frozen {
            at_local: "2026-08-16 09:00:00".to_string(),
        });
        assert_eq!(app.update(Msg::Key(KeyPress::SelectDown)), Effect::None);
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
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(1)),
            "but the cursor moved anyway"
        );
        assert_eq!(app.update(Msg::Key(KeyPress::SelectLast)), Effect::None);
        assert_eq!(app.selected(), Some(RowKey::Sheep(3)));
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
            "/home/ada/.shep".to_string(),
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

    /// fails if the gate is checked for one key and not the others. Replaces
    /// `the_stop_key_refuses_in_both_control_states`: that test's whole
    /// subject was that `x` refuses in BOTH gate states, and half of that
    /// stopped being true this task — behind an open gate `x` now arms. Its
    /// read-only half is strictly subsumed here, which covers all three
    /// verbs; its control-enabled half moved out and became
    /// `an_action_key_arms_a_confirm_and_sends_nothing` below.
    #[test]
    fn every_action_key_refuses_while_the_gate_is_closed() {
        for verb in [ActionVerb::Stop, ActionVerb::Restart, ActionVerb::Reload] {
            let (mut app, _t0) = started();
            app.update(Msg::Key(KeyPress::Action(verb)));
            assert!(
                app.action().is_none(),
                "{verb:?} armed behind a closed gate"
            );
            assert_eq!(
                app.notice().map(ToString::to_string).as_deref(),
                Some("read-only: actions need --allow-control"),
                "{verb:?}"
            );
        }
    }

    /// fails if lookout learns to exit on its own. A `DaemonShutdown` is a
    /// notice here, where in `bleats` it precedes a clean exit — the whole
    /// point of the maintainer's ruling is that a standing dashboard admits it is stale
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

    /// fails if `Esc` starts quitting out from under a filter. It is the one
    /// key whose meaning depends on state, which is acceptable only because
    /// the status bar reads `esc clear` for as long as that is in force and
    /// the title keeps `2 of 4 in the flock` on screen.
    ///
    /// **Deviation from the plan's literal (Task 2, Step 2.2):** the plan
    /// asserts `Effect::RefreshFeed` here. Traced against `reseat`/
    /// `set_filter` (both shipped, unchanged, in Task 1): clearing a filter
    /// only WIDENS the visible set, so a selection valid under the narrower
    /// filter is — by construction — still valid under the wider one.
    /// `reseat`'s very first line (`if self.selected_index().is_some() {
    /// return false; }`) therefore always takes the no-reseat path on any
    /// widening change, for every fixture, not just this one — there is no
    /// selected sheep for which clearing a filter could report a move. Empty
    /// filter's own doc says `RefreshFeed` rides "a selection that actually
    /// moved"; here it correctly did not, so `Effect::None` is the honest
    /// answer, not a bug this task should paper over by nudging `reseat`.
    #[test]
    fn esc_clears_the_filter_instead_of_quitting_while_one_is_set() {
        let mut app = filtered("web");
        assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::None);
        assert_eq!(app.filter(), "");
        assert_eq!(app.rows().len(), 4);
    }

    /// fails if the clear becomes unconditional, which is the mirror bug: `q`
    /// and Ctrl-C quit from every non-editing state, and so does `Esc` when
    /// there is no filter to take away first.
    #[test]
    fn esc_still_quits_with_no_filter_set() {
        let (mut app, _t0) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::Quit);
    }

    /// fails if typing into the box stops narrowing the table live, or if
    /// `Enter` narrows it a second time. The design's `filter_editing` frame
    /// is mid-type with the table already narrowed; applying only changes
    /// which keys mean what.
    #[test]
    fn the_table_narrows_while_the_query_is_still_being_typed() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        assert_eq!(app.mode(), InputMode::Text);
        for letter in ['w', 'e', 'b'] {
            app.update(Msg::Key(KeyPress::TextChar(letter)));
        }
        assert_eq!(app.rows().len(), 1, "narrowed before Enter");
        app.update(Msg::Key(KeyPress::TextApply));
        assert_eq!(app.mode(), InputMode::Normal);
        assert_eq!(
            app.rows().len(),
            1,
            "and applying changed nothing but the mode"
        );
    }

    /// fails if backspace stops widening the table again.
    #[test]
    fn backspace_widens_the_table_back_out() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        app.update(Msg::Key(KeyPress::TextChar('w')));
        app.update(Msg::Key(KeyPress::TextChar('z')));
        assert_eq!(app.rows().len(), 0);
        app.update(Msg::Key(KeyPress::TextBackspace));
        assert_eq!(
            app.rows().len(),
            2,
            "wz became w, which matches web and worker"
        );
    }

    /// fails if abandoning the box leaves the filter behind. `Esc` while
    /// editing clears and leaves; the two halves are one action.
    #[test]
    fn esc_while_editing_clears_the_filter_and_leaves_the_box() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        app.update(Msg::Key(KeyPress::TextChar('w')));
        app.update(Msg::Key(KeyPress::TextAbandon));
        assert_eq!(app.mode(), InputMode::Normal);
        assert_eq!(app.filter(), "");
        assert_eq!(app.rows().len(), 3);
    }

    /// fails if a notice survives into the filter box. Two things keep the
    /// query visible and this is one of them: opening the box takes any
    /// standing notice down (the `self.notice = None` at the top of
    /// `on_key`'s normal branch, which `FilterStart` goes through), and slot
    /// 2 of the bar keeps a notice raised LATER, while the box is open, from
    /// covering it. The second is what actually matters, because
    /// `Msg::BusLagged`, `BusEvent::Dropped` and `BusEvent::DaemonShutdown`
    /// all raise notices with no keypress involved and keep arriving while
    /// somebody types; see the phase plan's "Shapes the design named" #2.
    /// This test pins the first, which is what stops a stale notice
    /// reappearing the instant the box closes.
    #[test]
    fn opening_the_filter_takes_a_notice_off_the_bar() {
        let (mut app, _t0) = started();
        app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
        assert!(app.notice().is_some());
        app.update(Msg::Key(KeyPress::FilterStart));
        assert!(app.notice().is_none(), "the box is what the bar shows now");
    }

    /// fails if a notice raised WHILE the box is open is destroyed rather
    /// than deferred. The rejected fix for the same problem was to clear
    /// `self.notice` at the top of `on_text_key`; it would lose a
    /// `DaemonShutdown` because the operator happened to be mid-word. The bar
    /// hides the notice under the box (slot 2) and this pins that the notice
    /// itself is still there to be shown when the box closes.
    #[test]
    fn a_notice_raised_while_typing_is_deferred_and_not_destroyed() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        app.update(Msg::Key(KeyPress::TextChar('w')));
        app.update(Msg::Event(BusEvent::DaemonShutdown));
        app.update(Msg::Key(KeyPress::TextChar('e')));
        assert!(
            app.notice().is_some(),
            "typing did not wipe the shepherd's announcement"
        );
        assert_eq!(app.filter(), "we", "and the box kept the query");
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
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
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

    /// fails if a frozen dashboard applies a coalesced feed read that landed
    /// after the freeze. `refresh_feed` already refuses to ISSUE a read once
    /// the link is `Link::Lost`, but `run_ui`'s coalesced read is armed by a
    /// `feed_dirty` flag set BEFORE the freeze — a read requested a moment
    /// before `Msg::Frozen` can still be in flight when it lands, and without
    /// a guard here the `Msg::Bleats` arm would apply it anyway. That is the
    /// same contradiction-on-one-frame design decision 7 already refuses for
    /// the host strip and the uptime clock; the single enforcement point for
    /// the feed has to live in this arm; nothing upstream can catch a read
    /// that was already in flight when the freeze landed.
    #[test]
    fn a_frozen_dashboard_ignores_a_bleats_tail_in_flight_at_the_freeze() {
        let (mut app, _) = started();
        let live_tail = super::super::tail::Tail {
            lines: vec![super::super::tail::TailLine {
                stream: super::super::tail::Stream::Out,
                text: "read before the freeze".to_string(),
            }],
            ..Default::default()
        };
        app.update(Msg::Bleats {
            tail: live_tail.clone(),
        });
        assert_eq!(app.feed(), &live_tail, "a live dashboard takes the tail");

        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });

        let in_flight_tail = super::super::tail::Tail {
            lines: vec![super::super::tail::TailLine {
                stream: super::super::tail::Stream::Out,
                text: "read after the freeze".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(
            app.update(Msg::Bleats {
                tail: in_flight_tail
            }),
            Effect::None
        );
        assert_eq!(
            app.feed(),
            &live_tail,
            "the tail read after the freeze must not reach the rendered frame"
        );
    }

    /// A dashboard whose filter is set without any keymap involved. Task 2
    /// wires `/` to this; this task proves the sequence underneath it.
    ///
    /// Four sheep, two of which contain `web`: `api-web` at id 1 and
    /// `web-worker` at id 4, with `cron` and `queue` sitting BETWEEN them. The
    /// gap is deliberate. It is what makes `j` stepping over a hidden row a
    /// falsifiable claim rather than one a contiguous fixture would pass by
    /// accident.
    ///
    /// The names are what they are because the table reads by NAME. The old
    /// fixture was `web` 1, `api` 2, `web-worker` 3, `cron` 4, which put the
    /// gap in the id order alone: sorted by name, `web` and `web-worker` are
    /// adjacent and nothing can ever sit between them that the query `web`
    /// does not also match. Every "stepped over a hidden row" test here would
    /// have gone on passing over a contiguous pair. `api-web` sorts before
    /// `cron`, which is what puts two hidden rows back in the middle.
    fn filtered(query: &str) -> App {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "api-web", ProcStatus::Online),
                sheep(2, "cron", ProcStatus::Online),
                sheep(3, "queue", ProcStatus::Online),
                sheep(4, "web-worker", ProcStatus::Online),
            ],
            at: t0,
        });
        app.set_filter(query.to_string());
        app
    }

    /// fails if the table draws in id order, or if two instances of one app
    /// are left in an order the map decides.
    ///
    /// `flock` is a `BTreeMap<u32, Row>`, so a build that simply iterates it
    /// draws by id. The fixture makes those two answers impossible to
    /// confuse: by id it is `web` 0, `api` 1, `web` 2; by name and then id it
    /// is `api` 1, `web` 0, `web` 2.
    ///
    /// The two `web` rows carry the clustered-app case, but be clear about
    /// what they can and cannot catch here, because it is not what the
    /// equivalent test in `shep-core` catches. Measured with the tiebreak
    /// removed (`sort_by(|a, b| a.0.cmp(b.0))`, a STABLE name-only key): this
    /// test still passed. `flock` is a `BTreeMap<u32, Row>`, so the rows
    /// arrive in id order already and a stable name sort leaves them in it.
    /// The tiebreak is unfalsifiable from this pane, and pretending otherwise
    /// is how a test comes to assert something nothing could break. What this
    /// DOES catch, measured with the sort deleted outright, is the whole
    /// defect: the rows came back `web` 0, `api` 1, `web` 2, straight off the
    /// map.
    ///
    /// The key this test exercises is `rows()`'s own `(name, id)`, not
    /// `visible_rows`'s `(name, instance, id)` -- `rows()` is the flat sheep
    /// list, with no group headers and no slots in it, and nothing here
    /// calls the sequence the table draws. Both keys are TOTAL, which is the
    /// property that matters either way: this pane repolls every two
    /// seconds, and a key that is not total is what would let two rows swap
    /// places under the operator's cursor between refreshes.
    #[test]
    fn the_table_draws_by_name_then_by_id() {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(0, "web", ProcStatus::Online),
                sheep(1, "api", ProcStatus::Online),
                sheep(2, "web", ProcStatus::Online),
            ],
            at: t0,
        });

        let drawn: Vec<(&str, u32)> = app
            .rows()
            .iter()
            .map(|row| (row.info.name.as_str(), row.info.id))
            .collect();
        assert_eq!(drawn, vec![("api", 1), ("web", 0), ("web", 2)]);
    }

    /// fails if the table stops narrowing, or if the flock's real size stops
    /// being available beside the narrowed one. The title reads both numbers
    /// and a title that could only read the narrowed one would understate the
    /// flock, which is the same confident wrong number the `-` CPU cell and
    /// the frozen uptime rule exist to prevent.
    #[test]
    fn a_filter_narrows_the_rows_and_leaves_the_real_size_readable() {
        let app = filtered("web");
        assert_eq!(app.rows().len(), 2, "api-web and web-worker");
        assert_eq!(app.flock_len(), 4, "the flock did not get smaller");
    }

    /// fails if the filter matches whole names instead of substrings, which is
    /// precisely the failure the CLI's selector grammar would have had:
    /// `ProcessSelector`'s `Name` compares with `==`, so typing `w`, `we`,
    /// `web` toward `web-worker` matches nothing at every step.
    #[test]
    fn the_filter_matches_a_substring_and_not_a_whole_name() {
        assert_eq!(filtered("wor").rows().len(), 1, "web-worker, by its middle");
        assert_eq!(filtered("w").rows().len(), 2, "api-web, by its own middle");
    }

    /// fails if either `to_lowercase` is dropped. Both directions, because
    /// dropping one of the two leaves the other test passing.
    #[test]
    fn the_filter_ignores_case_in_both_directions() {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "WebEdge", ProcStatus::Online)],
            at: t0,
        });
        app.set_filter("webedge".to_string());
        assert_eq!(
            app.rows().len(),
            1,
            "a lowercase query against a mixed name"
        );
        app.set_filter("WEBEDGE".to_string());
        assert_eq!(app.rows().len(), 1, "and an uppercase one");
    }

    /// fails if `select_by` walks the whole flock again. This is the whole
    /// point of the task: `j` from the first visible row must land on the
    /// second VISIBLE row, not on whatever id happens to sit next in the map.
    #[test]
    fn j_and_k_step_only_over_visible_rows() {
        let mut app = filtered("web");
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(1)),
            "api-web, the first visible sheep"
        );
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(4)),
            "web-worker, skipping the hidden cron and queue"
        );
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(4)),
            "clamped at the last visible row"
        );
        app.update(Msg::Key(KeyPress::SelectUp));
        assert_eq!(app.selected(), Some(RowKey::Sheep(1)));
    }

    /// fails if `SelectLast` measures the flock rather than the visible set.
    #[test]
    fn select_last_lands_on_the_last_visible_row() {
        let mut app = filtered("web");
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(4)),
            "web-worker, not queue at id 3"
        );
    }

    /// fails if a filter that hides the selection snaps to row 0, or drops the
    /// selection entirely while rows are still visible. `reseat`'s shipped
    /// rule is that a lost selection falls to whatever now occupies the same
    /// POSITION, clamped: snapping to the top would throw an operator to the
    /// start of a two hundred sheep flock for typing one more character.
    #[test]
    fn a_filter_that_hides_the_selection_clamps_to_the_nearest_visible_row() {
        let mut app = filtered("");
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(4)),
            "web-worker, position 3 of 4"
        );
        app.set_filter("web".to_string());
        assert_eq!(
            app.selected(),
            Some(RowKey::Sheep(4)),
            "position 3 clamps to the last visible row, which is web-worker"
        );
    }

    /// fails if nothing-matches leaves the selection pointing at a hidden
    /// sheep. Every pane below the table describes the selection, so a
    /// selection nobody can see is four panes describing a sheep that is not
    /// on screen.
    #[test]
    fn nothing_visible_means_nothing_selected() {
        let app = filtered("zzz");
        assert_eq!(app.rows().len(), 0);
        assert_eq!(app.selected(), None);
        assert!(app.selected_row().is_none());
        assert_eq!(app.flock_len(), 4, "the flock is still four sheep");
    }

    /// fails if a snapshot clears the filter, or rebuilds the table from the
    /// unfiltered map. The two-second `ListFlock` reply REPLACES `self.flock`
    /// wholesale and is by far the most frequent message this reducer sees, so
    /// a regression here would make the filter appear to work for two seconds
    /// and then silently widen the table under an operator who is still
    /// reading it, with the title's `2 of 4` the only thing left saying a
    /// filter is on.
    #[test]
    fn a_filter_survives_the_two_second_snapshot() {
        let mut app = filtered("web");
        let t1 = Instant::now();
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "api-web", ProcStatus::Online),
                sheep(2, "cron", ProcStatus::Online),
                sheep(3, "queue", ProcStatus::Online),
                sheep(4, "web-worker", ProcStatus::Online),
            ],
            at: t1,
        });
        assert_eq!(app.filter(), "web", "the snapshot did not clear it");
        assert_eq!(app.rows().len(), 2, "and did not widen the table");
        assert_eq!(app.flock_len(), 4);
    }

    /// fails if clearing the filter does not bring the whole flock back, or
    /// leaves the selection unseated. An empty query is the same as no filter,
    /// which is also what `Enter` on an empty box has to mean.
    #[test]
    fn an_empty_query_is_the_same_as_no_filter() {
        let mut app = filtered("zzz");
        app.set_filter(String::new());
        assert_eq!(app.rows().len(), 4);
        assert_eq!(app.selected(), Some(RowKey::Sheep(1)), "seated again");
    }

    /// fails if the three states `ProcessInfo::lambs` distinguishes get
    /// collapsed. The wire type keeps them apart on purpose: `None` means this
    /// reply did not walk, `Some(vec![])` means it walked and found nothing,
    /// and the pane says different sentences for each because they are
    /// different facts about the machine.
    #[test]
    fn a_lamb_reply_records_which_of_the_three_states_it_saw() {
        let (mut app, t0) = started();
        let walked = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .lambs(Some(vec![Lamb::new(48_220, "node")]))
            .build();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![walked])),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Walked(lambs), _)) if lambs.len() == 1));

        let empty = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .lambs(Some(Vec::new()))
            .build();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![empty])),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Walked(lambs), _)) if lambs.is_empty()));

        let unwalked = ProcessInfo::builder(1, "web", ProcStatus::Stopped).build();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![unwalked])),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::NotWalked, _))));
        let _ = t0;
    }

    /// fails if a reading taken for one sheep is shown against another. The
    /// reading carries the id it was taken for and the pane asks by id, so a
    /// request dropped by a full channel and a reply for the previous
    /// selection both read as "not read yet" with no second field to track
    /// them.
    #[test]
    fn a_reading_for_another_sheep_reads_as_not_read_yet() {
        let (mut app, _t0) = started();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![
                ProcessInfo::builder(1, "web", ProcStatus::Online)
                    .lambs(Some(vec![Lamb::new(48_220, "node")]))
                    .build(),
            ])),
        });
        assert!(app.lambs_for(1).is_some());
        assert!(app.lambs_for(2).is_none(), "not this sheep's reading");
    }

    /// fails if a failed lamb fetch steals the status bar. It is a decoration
    /// on a pane, not an operator's action, and the pane already says what it
    /// does not know (A17).
    #[test]
    fn a_failed_lamb_fetch_says_so_in_the_pane_and_raises_no_notice() {
        let (mut app, _t0) = started();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Err(RequestError::Closed),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Failed, _))));
        assert!(app.notice().is_none(), "no notice for a decoration");
    }

    /// fails if an unrecognised reply is recorded as a successful walk.
    /// `Response` is `#[non_exhaustive]`; a variant this binary predates is
    /// not a lamb list and must not read as an empty one, which would say
    /// "none found" about a machine nobody looked at.
    #[test]
    fn an_unrecognised_lamb_reply_is_a_failure_and_not_an_empty_walk() {
        let (mut app, _t0) = started();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Pong),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Failed, _))));
    }

    /// fails if a reading landing after a freeze reaches the frame. Same guard
    /// and same reason as `Msg::Bleats`: the fetch is armed before the freeze
    /// can land, so a reply can still be in flight when `Msg::Frozen` arrives,
    /// and content newer than a banner saying the values are frozen is the
    /// contradiction-on-one-frame this dashboard refuses everywhere else.
    #[test]
    fn a_lamb_reply_after_a_freeze_is_refused() {
        let (mut app, _t0) = started();
        app.update(Msg::Frozen {
            at_local: "2026-08-16 09:00:00".to_string(),
        });
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![
                ProcessInfo::builder(1, "web", ProcStatus::Online)
                    .lambs(Some(vec![Lamb::new(48_220, "node")]))
                    .build(),
            ])),
        });
        assert!(
            app.lambs_for(1).is_none(),
            "the frozen frame learned nothing"
        );
    }

    /// fails if the reply's rows are thrown away and the table left to wait
    /// for the next poll. The shepherd's own rows are right there.
    #[test]
    fn an_accepted_stop_upserts_the_rows_the_shepherd_returned() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Replied {
            sent: Sent::Action {
                verb: ActionVerb::Stop,
                target: RowKey::Sheep(2),
                name: "api".to_string(),
            },
            result: Ok(Response::Stopped(vec![sheep(
                2,
                "api",
                ProcStatus::Stopped,
            )])),
        });
        assert_eq!(
            app.rows()
                .iter()
                .find(|row| row.info.id == 2)
                .map(|row| row.info.status),
            Some(ProcStatus::Stopped),
            "the table shows what the shepherd said, without waiting for a poll"
        );
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("stop api (id 2): the shepherd stopped it")
        );
        assert!(app.action().is_none(), "the in-flight state cleared");
    }

    /// fails if a reload reply claims the swap finished. `Response::Reloading`
    /// is an ACCEPTANCE, its own doc says so, and the swaps arrive afterwards
    /// on the bus as `process.reload` / `process.reloaded` /
    /// `process.reload_abandoned`, which the table already consumes. A
    /// sentence saying "reloaded" would be the one lie this reply makes easy
    /// to tell.
    #[test]
    fn a_reload_reply_does_not_claim_the_swap_finished() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Reload)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Replied {
            sent: Sent::Action {
                verb: ActionVerb::Reload,
                target: RowKey::Sheep(2),
                name: "api".to_string(),
            },
            result: Ok(Response::Reloading(vec![sheep(
                2,
                "api",
                ProcStatus::Online,
            )])),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert_eq!(
            said,
            "reload api (id 2): accepted, the swaps report themselves as they happen"
        );
        assert!(!said.contains("reloaded"), "got {said:?}");
    }

    /// fails if the daemon's own words are replaced with a canned string. The
    /// message is a sentence a human wrote; `RequestError`'s full `Display`
    /// interpolates the code with `{:?}` and would put a Rust identifier on an
    /// operator's screen.
    #[test]
    fn a_daemon_refusal_reaches_the_bar_in_the_daemons_own_words() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        app.update(Msg::Key(KeyPress::Confirm));
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
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert_eq!(
            said,
            "restart api (id 2): selector matched no registered sheep"
        );
        assert!(!said.contains("NotFound"), "no Rust identifiers: {said:?}");
        assert!(app.notice().is_some_and(Notice::is_grave));
    }

    /// fails if a connection that died mid-request reports as anything else.
    #[test]
    fn a_connection_that_died_mid_request_says_so_under_the_same_prefix() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Replied {
            sent: Sent::Action {
                verb: ActionVerb::Stop,
                target: RowKey::Sheep(2),
                name: "api".to_string(),
            },
            result: Err(RequestError::Closed),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert!(said.starts_with("stop api (id 2): "), "got {said:?}");
        assert!(said.contains(&RequestError::Closed.to_string()));
    }

    /// fails if a reply this binary does not understand reads as success.
    /// `Response` is `#[non_exhaustive]`, and swallowing an unrecognised
    /// variant into `Ok` is what `flock()` does and what this must not: a
    /// stop that silently reported success while the sheep kept running is the
    /// worst outcome this feature has.
    ///
    /// The second half is the sharper case: the RIGHT SHAPE for the wrong
    /// verb. A `Stopped` answering a `Restart` carries rows and would upsert
    /// happily.
    #[test]
    fn an_unrecognised_reply_says_so_rather_than_reading_as_success() {
        for reply in [
            Response::Pong,
            Response::Stopped(vec![sheep(2, "api", ProcStatus::Stopped)]),
        ] {
            let mut app = allowed();
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
            app.update(Msg::Key(KeyPress::Confirm));
            app.update(Msg::Replied {
                sent: Sent::Action {
                    verb: ActionVerb::Restart,
                    target: RowKey::Sheep(2),
                    name: "api".to_string(),
                },
                result: Ok(reply),
            });
            assert_eq!(
                app.notice().map(ToString::to_string).as_deref(),
                Some(
                    "restart api (id 2): the shepherd answered something this lookout does not understand"
                )
            );
            assert!(app.notice().is_some_and(Notice::is_grave));
        }
    }

    /// `s` asks for the read rather than opening on stale or empty state: the
    /// file can have changed since the last look, and an empty screen while the
    /// read is in flight is a screen that lies for one frame.
    #[test]
    fn s_asks_for_the_file_before_the_screen_opens() {
        let mut app = fixtures::full_app();
        assert_eq!(
            app.update(Msg::Key(KeyPress::Settings)),
            Effect::LoadSettings
        );
        assert!(
            app.settings().is_none(),
            "nothing opens until the read lands"
        );
    }

    #[test]
    fn the_screen_opens_when_the_read_lands() {
        let mut app = fixtures::full_app();
        let _ = app.update(Msg::Key(KeyPress::Settings));
        let _ = app.update(Msg::Settings {
            result: Ok(fixtures::settings_snapshot()),
        });
        assert!(app.settings().is_some());
    }

    #[test]
    fn a_read_that_failed_says_so_and_leaves_the_dashboard_up() {
        let mut app = fixtures::full_app();
        let _ = app.update(Msg::Key(KeyPress::Settings));
        let _ = app.update(Msg::Settings {
            result: Err("no such file".into()),
        });
        assert!(app.settings().is_none());
        let notice = app.notice().expect("a failed read has to say so");
        assert!(notice.is_grave());
        assert!(notice.to_string().contains("no such file"));
    }

    #[test]
    fn s_closes_the_screen_again() {
        let mut app = fixtures::app_in_settings();
        let _ = app.update(Msg::Key(KeyPress::Settings));
        assert!(app.settings().is_none());
    }

    /// The one arm of the `Escape` cascade this screen swaps. From the dashboard
    /// with no filter, `Esc` quits; from here it must not.
    #[test]
    fn escape_closes_the_screen_and_never_quits() {
        let mut app = fixtures::app_in_settings();
        assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::None);
        assert!(app.settings().is_none());
    }

    #[test]
    fn the_flock_cursor_and_the_filter_survive_the_swap() {
        let mut app = fixtures::full_app();
        let _ = app.update(Msg::Key(KeyPress::FilterStart));
        for c in "web".chars() {
            let _ = app.update(Msg::Key(KeyPress::TextChar(c)));
        }
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        // `selected()` hands back an owned `Option<RowKey>` (app.rs:1436), so
        // there is nothing to clone.
        let selected = app.selected();
        let filter = app.filter().to_string();

        let _ = app.update(Msg::Key(KeyPress::Settings));
        let _ = app.update(Msg::Settings {
            result: Ok(fixtures::settings_snapshot()),
        });
        let _ = app.update(Msg::Key(KeyPress::Settings));

        assert_eq!(app.selected(), selected);
        assert_eq!(app.filter(), filter);
    }

    #[test]
    fn the_settings_cursor_starts_at_the_first_row_on_every_open() {
        let mut app = fixtures::app_in_settings();
        let _ = app.update(Msg::Key(KeyPress::SelectDown));
        let _ = app.update(Msg::Key(KeyPress::SelectDown));
        let _ = app.update(Msg::Key(KeyPress::Settings));
        let _ = app.update(Msg::Key(KeyPress::Settings));
        let _ = app.update(Msg::Settings {
            result: Ok(fixtures::settings_snapshot()),
        });

        let first = app.settings().unwrap().rows()[0];
        assert_eq!(app.settings().unwrap().cursor(), Some(first));
    }

    #[test]
    fn the_cursor_moves_through_the_scalars_and_into_the_dogs() {
        let mut app = fixtures::app_in_settings();
        let rows = app.settings().unwrap().rows();
        for _ in 0..rows.len() - 1 {
            let _ = app.update(Msg::Key(KeyPress::SelectDown));
        }
        assert_eq!(
            app.settings().unwrap().cursor(),
            Some(*rows.last().unwrap())
        );
        // and it stops rather than wrapping, the way the flock table does
        let _ = app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(
            app.settings().unwrap().cursor(),
            Some(*rows.last().unwrap())
        );
    }

    #[test]
    fn an_action_key_from_the_dashboard_is_unreachable_while_the_screen_is_up() {
        let mut app = fixtures::app_in_settings_with_control();
        // `x` is the stop key on the dashboard. In here it is not an action at all.
        let _ = app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        // The accessor is `App::action()` (app.rs:1662).
        assert!(app.action().is_none(), "no sheep confirm can arm from here");
    }

    #[test]
    fn a_read_only_lookout_opens_the_screen_and_refuses_the_edit_key() {
        let mut app = fixtures::app_in_settings(); // Control::ReadOnly
        assert!(app.settings().is_some(), "reading shep.toml is not gated");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let notice = app.notice().expect("the refusal has to say why");
        assert!(notice.is_grave());
    }

    #[test]
    fn space_arms_a_candidate_without_changing_the_row() {
        let mut app = fixtures::app_in_settings_with_control();
        let before = app.settings().unwrap().snapshot().log_level.value.clone();

        assert_eq!(app.update(Msg::Key(KeyPress::Cycle)), Effect::None);

        assert_eq!(
            app.settings().unwrap().snapshot().log_level.value,
            before,
            "arming is a question, so the row still shows what the file says"
        );
        assert!(app.settings().unwrap().pending().is_some());
    }

    /// Six log levels and one cycle key. Without re-arming, the fourth is
    /// unreachable without cancelling in between.
    #[test]
    fn space_advances_the_candidate_rather_than_needing_a_cancel() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let first = app.settings().unwrap().pending().unwrap().text.to_string();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let second = app.settings().unwrap().pending().unwrap().text.to_string();
        assert_ne!(first, second);
    }

    #[test]
    fn the_daemon_confirm_names_both_layers_lookout_cannot_see() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(text.contains("shep daemon reload"), "got: {text}");
        assert!(text.contains("SHEP_LOG_LEVEL"), "got: {text}");
        assert!(text.contains("--log-level"), "got: {text}");
    }

    #[test]
    fn the_whistle_confirm_names_a_whistle_restart_and_not_a_reload() {
        let mut app = fixtures::app_in_settings_on(SettingField::AllowControl);
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(text.contains("shep whistle restarted"), "got: {text}");
        assert!(
            !text.contains("daemon reload"),
            "a whistle key needs no reload: {text}"
        );
    }

    #[test]
    fn the_style_confirm_promises_nothing_beyond_the_next_command() {
        let mut app = fixtures::app_in_settings_on(SettingField::StyleLevel);
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(text.contains("the next command reads it"), "got: {text}");
    }

    #[test]
    fn enter_sends_the_armed_edit() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let effect = app.update(Msg::Key(KeyPress::Confirm));
        assert!(matches!(
            effect,
            Effect::WriteSetting(SettingEdit::Set {
                field: SettingField::LogLevel,
                ..
            })
        ));
        assert!(app.settings().unwrap().pending().unwrap().sent);
    }

    #[test]
    fn a_written_edit_updates_the_row_and_its_source() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send");
        };
        let SettingEdit::Set {
            value: candidate, ..
        } = edit.clone()
        else {
            panic!("cycling only ever arms Set");
        };

        let effect = app.update(Msg::SettingWritten {
            edit,
            result: Ok(()),
        });
        assert_eq!(
            effect,
            Effect::LoadSettings,
            "a landed write re-reads rather than hand-folding the row"
        );
        assert!(app.settings().unwrap().pending().is_none());

        // The re-read itself: `run_ui` drives this through `spawn_blocking`
        // and `load_settings`; this test drives the landing message
        // directly, the same way the fixtures that open the screen already
        // do for the initial `s`.
        let mut updated = fixtures::settings_snapshot();
        updated.log_level = ScalarView {
            value: candidate,
            source: StyleSource::Config,
        };
        let _ = app.update(Msg::Settings {
            result: Ok(updated.clone()),
        });

        assert_eq!(app.settings().unwrap().snapshot(), &updated);
    }

    /// The other half of the row-update story: an `Unset` has no local
    /// value to fold in (only the document does), which is exactly why the
    /// fix routes both through the same re-read rather than growing a
    /// second, `Unset`-shaped folding path.
    #[test]
    fn an_unset_write_returns_the_row_to_the_default() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for _ in 0..8 {
            let _ = app.update(Msg::Key(KeyPress::TextBackspace));
        }
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send");
        };
        assert!(matches!(
            edit,
            SettingEdit::Unset {
                field: SettingField::MaxCronSleep
            }
        ));

        let effect = app.update(Msg::SettingWritten {
            edit,
            result: Ok(()),
        });
        assert_eq!(effect, Effect::LoadSettings);

        let mut updated = fixtures::settings_snapshot();
        updated.max_cron_sleep = ScalarView {
            value: "30s".to_string(),
            source: StyleSource::Default,
        };
        let _ = app.update(Msg::Settings {
            result: Ok(updated.clone()),
        });

        assert_eq!(app.settings().unwrap().snapshot(), &updated);
    }

    /// fails if a landed write's own reload throws the cursor back to the
    /// first row -- `Msg::Settings`'s `opening` check is what this pins.
    #[test]
    fn the_cursor_survives_a_landed_writes_reload() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let before = app.settings().unwrap().cursor();
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send");
        };
        let _ = app.update(Msg::SettingWritten {
            edit,
            result: Ok(()),
        });
        let _ = app.update(Msg::Settings {
            result: Ok(fixtures::settings_snapshot()),
        });
        assert_eq!(app.settings().unwrap().cursor(), before);
    }

    /// fails if `r` throws the cursor back to the first row the same way a
    /// landed write's own reload almost did.
    #[test]
    fn the_cursor_survives_a_refresh() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let before = app.settings().unwrap().cursor();
        assert_eq!(
            app.update(Msg::Key(KeyPress::Refresh)),
            Effect::LoadSettings
        );
        let _ = app.update(Msg::Settings {
            result: Ok(fixtures::settings_snapshot()),
        });
        assert_eq!(app.settings().unwrap().cursor(), before);
    }

    #[test]
    fn a_refused_write_says_why_and_leaves_the_row_alone() {
        let mut app = fixtures::app_in_settings_with_control();
        let before = app.settings().unwrap().snapshot().log_level.clone();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send");
        };
        let _ = app.update(Msg::SettingWritten {
            edit,
            result: Err("max_cron_sleep is 500ms, below the 1s floor".into()),
        });

        assert_eq!(app.settings().unwrap().snapshot().log_level, before);
        let notice = app.notice().unwrap();
        assert!(notice.is_grave());
        assert!(notice.to_string().contains("below the 1s floor"));
    }

    /// The divergence from the sheep confirm, which `disarm_on_link_change`
    /// clears. A settings edit is local file I/O over a file that is not
    /// stale.
    #[test]
    fn a_lost_link_leaves_a_scalar_confirm_armed() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let _ = app.update(Msg::Frozen {
            at_local: "12:00:00".into(),
        });
        assert!(
            app.settings().unwrap().pending().is_some(),
            "a scalar never leaves the machine, so a dead shepherd is irrelevant to it"
        );
    }

    /// And it still expires, off the raw tick rather than `self.now`, which
    /// stops advancing once the link is lost.
    #[test]
    fn a_settings_confirm_expires_on_a_frozen_dashboard() {
        let (mut app, start) = fixtures::app_in_settings_at();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let _ = app.update(Msg::Frozen {
            at_local: "12:00:00".into(),
        });
        let _ = app.update(Msg::Tick {
            now: start + CONFIRM_EXPIRY,
        });
        assert!(app.settings().unwrap().pending().is_none());
    }

    #[test]
    fn escape_cancels_the_confirm_before_it_closes_the_screen() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let _ = app.update(Msg::Key(KeyPress::Escape));
        assert!(app.settings().unwrap().pending().is_none());
        assert!(
            app.settings().is_some(),
            "the first Esc cancels, it does not close"
        );
        let _ = app.update(Msg::Key(KeyPress::Escape));
        assert!(app.settings().is_none());
    }

    /// fails if an action armed while the read is still in flight survives
    /// the screen opening. `s` raises `Effect::LoadSettings` while
    /// `self.settings` is still `None`, so `x` reaches `arm()` normally and
    /// succeeds; once the read lands, `on_key`'s settings branch runs ahead
    /// of the armed-confirm cancel block and `on_settings_key` no-ops
    /// `Confirm`, so nothing would ever reach the code that resolves an
    /// armed action. The fix is the same closing-by-construction `on_key`'s
    /// own comment already argues for `/` and the filter box.
    #[test]
    fn opening_the_screen_clears_an_action_armed_while_the_read_was_in_flight() {
        let mut app = fixtures::allowed_app();
        let _ = app.update(Msg::Key(KeyPress::Settings));
        let _ = app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert!(
            app.action().is_some(),
            "the arm must still succeed before the read lands"
        );
        let _ = app.update(Msg::Settings {
            result: Ok(fixtures::settings_snapshot()),
        });
        assert!(
            app.action().is_none(),
            "no armed action may survive the screen opening"
        );
    }

    /// fails if the filter box survives the screen opening the same way an
    /// armed action almost did. `s` raises `Effect::LoadSettings` while
    /// `self.settings` is still `None`, so `/` reaches `on_key`'s ordinary
    /// dispatch and opens the box; once the read lands, `on_key`'s
    /// `self.mode == InputMode::Text` check runs ahead of the settings
    /// branch, so every key after this would keep landing in
    /// `on_text_key` and never reach `on_settings_key` at all. The query
    /// is kept rather than cleared -- `TextApply`'s reading, argued at the
    /// fix's own call site.
    #[test]
    fn opening_the_screen_closes_a_filter_box_left_open_while_the_read_was_in_flight() {
        let mut app = fixtures::allowed_app();
        let _ = app.update(Msg::Key(KeyPress::Settings));
        let _ = app.update(Msg::Key(KeyPress::FilterStart));
        let _ = app.update(Msg::Key(KeyPress::TextChar('w')));
        let _ = app.update(Msg::Key(KeyPress::TextChar('e')));
        assert_eq!(
            app.mode(),
            InputMode::Text,
            "the box is open before the read lands"
        );
        let _ = app.update(Msg::Settings {
            result: Ok(fixtures::settings_snapshot()),
        });
        assert!(app.settings().is_some(), "the screen opened");
        assert_eq!(
            app.mode(),
            InputMode::Normal,
            "the box must not survive the screen opening"
        );
        assert_eq!(app.filter(), "we", "the typed query is kept, not discarded");
    }

    /// fails if `App::set_style` does not round trip exactly: the flag
    /// source in particular, since that is the layer the settings screen's
    /// own STYLE LEVEL row was silently dropping before this was wired
    /// through `App` at all.
    #[test]
    fn set_style_round_trips_exactly() {
        let mut app = fixtures::full_app();
        assert_eq!(
            app.style(),
            (StyleLevel::Full, StyleSource::Default),
            "the default before anyone calls set_style"
        );
        app.set_style((StyleLevel::Bare, StyleSource::Flag));
        assert_eq!(app.style(), (StyleLevel::Bare, StyleSource::Flag));
    }

    /// fails if the settings screen's own STYLE LEVEL row can disagree with
    /// the style `App` was told to carry. Drives the exact call
    /// `Effect::LoadSettings` makes (`load_settings(path, socket_default,
    /// app.style())`) against a real file on disk whose own `[style]
    /// level` names a THIRD, different level -- proving the row reports
    /// the value threaded onto `App`, not one re-derived from the file,
    /// which is what a flag "reaching the row" rather than being dropped
    /// actually means.
    #[test]
    fn the_style_set_on_the_app_reaches_the_settings_row_undropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[style]\nlevel = \"bare\"\n").unwrap();
        let socket_default = dir.path().join("run").join("shep.sock");

        let mut app = fixtures::full_app();
        app.set_style((StyleLevel::Plain, StyleSource::Flag));

        let result = crate::commands::settings::load_settings(&path, &socket_default, app.style())
            .map_err(|err| err.to_string());
        let _ = app.update(Msg::Settings { result });

        let row = &app.settings().unwrap().snapshot().style_level;
        assert_eq!(
            row.source,
            StyleSource::Flag,
            "the flag beats the file rather than being dropped by it"
        );
        assert_eq!(row.value, "plain");
    }

    #[test]
    fn enter_on_a_text_row_opens_the_editor_seeded_with_the_current_value() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let (field, buffer) = app.settings().unwrap().typing().expect("the editor opens");
        assert_eq!(*field, SettingField::MaxCronSleep);
        assert_eq!(buffer, "30s");
    }

    #[test]
    fn typing_then_enter_arms_rather_than_writing() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for _ in 0..3 {
            let _ = app.update(Msg::Key(KeyPress::TextBackspace));
        }
        for c in "45s".chars() {
            let _ = app.update(Msg::Key(KeyPress::TextChar(c)));
        }
        assert_eq!(app.update(Msg::Key(KeyPress::TextApply)), Effect::None);
        let prompt = app.settings().unwrap().pending().unwrap();
        assert!(
            !prompt.sent,
            "the editor arms; a second Enter is what sends"
        );
        assert!(prompt.text.contains("45s"), "got: {}", prompt.text);
        assert!(
            prompt.text.contains("SHEP_MAX_CRON_SLEEP"),
            "got: {}",
            prompt.text
        );
    }

    #[test]
    fn an_empty_editor_arms_an_unset() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for _ in 0..8 {
            let _ = app.update(Msg::Key(KeyPress::TextBackspace));
        }
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(text.starts_with("unset max_cron_sleep?"), "got: {text}");
    }

    #[test]
    fn the_socket_confirm_rules_out_the_reload_it_would_otherwise_imply() {
        let mut app = fixtures::app_in_settings_on(SettingField::Socket);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(text.contains("stopped and started"), "got: {text}");
        assert!(text.contains("a reload will not move it"), "got: {text}");
    }

    /// A refusal is discovered under the lock, so it lands after the confirm.
    /// The typed text has to survive it, or the operator retypes a path to fix
    /// one character.
    #[test]
    fn a_refused_write_reopens_the_editor_with_the_text_intact() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for _ in 0..3 {
            let _ = app.update(Msg::Key(KeyPress::TextBackspace));
        }
        for c in "500ms".chars() {
            let _ = app.update(Msg::Key(KeyPress::TextChar(c)));
        }
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send");
        };
        let _ = app.update(Msg::SettingWritten {
            edit,
            result: Err("max_cron_sleep is 500ms, below the 1s floor".into()),
        });

        let (_, buffer) = app
            .settings()
            .unwrap()
            .typing()
            .expect("the editor reopens");
        assert_eq!(buffer, "500ms");
        assert!(
            app.notice()
                .unwrap()
                .to_string()
                .contains("below the 1s floor")
        );
    }

    #[test]
    fn escape_abandons_the_editor_and_keeps_the_screen_open() {
        let mut app = fixtures::app_in_settings_on(SettingField::Socket);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::TextAbandon));
        assert!(app.settings().unwrap().typing().is_none());
        assert!(app.settings().is_some());
    }

    #[test]
    fn a_closed_scalar_has_no_editor() {
        let mut app = fixtures::app_in_settings_with_control(); // on log_level
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        assert!(
            app.settings().unwrap().typing().is_none(),
            "log_level is a cycle, not a text field"
        );
    }

    /// fails if an armed candidate survives a movement key. `space` on
    /// `log_level` arms it; `j` must cancel that arm instead of also
    /// moving the cursor to `log_json` -- the same cancel-before-act rule
    /// the dashboard's `x`/`R`/`L` already follow (task 7 review finding
    /// A).
    #[test]
    fn movement_cancels_an_armed_candidate_rather_than_also_moving() {
        let mut app = fixtures::app_in_settings_with_control(); // cursor on log_level
        let before = app.settings().unwrap().cursor();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        assert!(
            app.settings().unwrap().pending().is_some(),
            "space must arm before this test means anything"
        );
        let _ = app.update(Msg::Key(KeyPress::SelectDown));
        assert!(
            app.settings().unwrap().pending().is_none(),
            "the armed candidate must not survive the movement key"
        );
        assert_eq!(
            app.settings().unwrap().cursor(),
            before,
            "the cursor must not also move on the same keypress"
        );
    }
}
