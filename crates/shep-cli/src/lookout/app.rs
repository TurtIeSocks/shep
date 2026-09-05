//! The lookout's state and its reducer: `Msg` in, `Effect` out.
//!
//! No I/O, no terminal types, no clock. [`App::update`] is synchronous, work
//! for the outside comes back as an [`Effect`] the caller runs, and every
//! `Instant` arrives on a message.
//!
//! The bus is lossy, so [`Msg::Event`] upserts and [`Msg::Snapshot`] replaces
//! the whole flock map. The cursor is a [`RowKey`], not a row index: the map is
//! replaced wholesale every two seconds, and [`App::reseat`] puts the cursor
//! back on a real row.

use core::fmt;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use shep_client::RequestError;
use shep_core::config::LogLevel;
use shep_core::protocol::{
    BusEvent, DogSectionToml, DogSource, EnvValue, Lamb, ProcessEventKind, ProcessInfo, Request,
    Response, SelectorSpec,
};
use shep_core::status::ProcStatus;

use super::field::{FieldKind, FieldSet};
use super::pane::{ConfigPane, FieldValue, Lock, PaneEdit, PanePending, PaneTarget};
use super::theme::Palette;
use super::viewport::Viewport;
use crate::commands::settings::{SettingEdit, SettingField, SettingsSnapshot, settings_field_set};
use crate::style::{StyleLevel, StyleSource};
use crate::vocabulary::Reported;

/// Whether this lookout may act on a sheep.
///
/// Turned on by `--allow-control` or `lookout.allow_control` in the KV store.
/// A fat-finger catch, not a security boundary: anyone who can run lookout can
/// run `shep stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Actions refuse. The default.
    ReadOnly,
    /// Actions are permitted: `x`, `R` and `L` arm a confirm, Enter sends it.
    Allowed,
}

/// Which keymap is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// The ordinary dashboard keys.
    Normal,
    /// The filter box is open and every printable key is text.
    Text,
}

/// The keys lookout binds, named by meaning rather than by keystroke.
///
/// `super::input::map_key` builds these at the edge, so this module never
/// touches a terminal crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    /// `q` in normal mode, or `Ctrl-C` in either.
    Quit,
    /// `Esc` in normal mode: cancels an armed confirm, else clears the filter,
    /// else quits. The reducer decides; the keymap sees none of those states.
    Escape,
    /// `k` or `Up`: the selection moves up one row.
    SelectUp,
    /// `j` or `Down`: the selection moves down one row.
    SelectDown,
    /// `g` or `Home`: the first sheep in the flock.
    SelectFirst,
    /// `G` or `End`: the last one.
    SelectLast,
    /// `r`: poll now.
    Refresh,
    /// `x`, `R` or `L`: arms a confirm for the verb, or refuses and says why.
    Action(ActionVerb),
    /// `Enter` in normal mode. Sends an armed confirm; does nothing
    /// otherwise.
    Confirm,
    /// `/`: open the filter box, carrying whatever query is already set.
    FilterStart,
    /// One printable character typed into whichever text field is open.
    TextChar(char),
    /// `Backspace` in whichever text field is open.
    TextBackspace,
    /// `Enter` in whichever text field is open: apply and leave.
    TextApply,
    /// `Esc` in whichever text field is open: abandon the edit and leave.
    TextAbandon,
    /// `s`: opens the settings screen, or closes it from inside.
    Settings,
    /// `space`: cycles the value under the settings screen's cursor. Nothing on
    /// the dashboard; refuses like an action key when the gate is closed.
    Cycle,
    /// `e`: open the config pane for the selected sheep on the dashboard,
    /// and close it again from inside it. The reducer decides which, the
    /// same division [`Self::Settings`]'s own doc argues for.
    Edit,
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
    /// This client's own receiver fell behind and discarded frames.
    /// [`BusEvent::Dropped`] is the shepherd's queue instead.
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
    /// One reading of the machine this lookout runs on, off the 1s heartbeat.
    /// `None` means `sysinfo` does not support this platform. Refused once the
    /// link is lost.
    Host {
        /// What the sampler saw, or `None` on an unsupported platform.
        sample: Option<super::source::HostSample>,
    },
    /// One refresh of the selected sheep's log files, answering an
    /// [`Effect::RefreshFeed`]. Always yields [`Effect::None`].
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
    /// The reducer is already in the in-flight state when `run_ui` tries to
    /// send, so a failed `try_send` has to come back, or the
    /// one-action-at-a-time guard refuses everything from then on.
    Unsent {
        /// What could not be sent.
        sent: Sent,
    },
    /// The settings screen's read of `shep.toml` landed, answering an
    /// [`Effect::LoadSettings`]. A `String` error, since this reducer holds no
    /// error types from `commands`.
    Settings {
        /// The rendered snapshot, or why it could not be read.
        result: Result<SettingsSnapshot, String>,
    },
    /// An [`Effect::WriteSetting`] has landed.
    SettingWritten {
        /// The edit that was sent, echoed back: the cursor can have moved on
        /// while the write was in flight.
        edit: SettingEdit,
        /// Whether the write landed, or why it did not.
        result: Result<(), String>,
    },
    /// An [`Effect::LoadDogPane`]'s schema probe has answered.
    ///
    /// The schema half only: the section arrives separately over the wire
    /// as [`Sent::DogSection`], since it comes from the shepherd rather
    /// than the dog's own binary. This arm parks the schema and raises the
    /// request for the section; the pane is built once that lands.
    ///
    /// `Result<_, String>`, like [`Self::Settings`]: this reducer holds no
    /// error types from `commands`.
    DogPane {
        /// The dog.
        name: String,
        /// The adopted binary, or [`None`] for a built-in, echoed back so
        /// the pane records what it probed.
        adopted_path: Option<PathBuf>,
        /// The dog's schema, or why there is no pane for it.
        result: Result<serde_json::Value, String>,
    },
    /// An [`Effect::WriteDog`] has landed. `Ok` carries the [`DogSource`] the
    /// write resolved, which [`Sent::Dog`] then rides to the shepherd, so the
    /// request cannot disagree with the file.
    DogWritten {
        /// The toggle that was sent, echoed back.
        edit: DogEdit,
        /// Whether the write landed and what it resolved to, or why it did
        /// not.
        result: Result<DogSource, String>,
    },
}

/// The one gate on writing `shep.toml` from the settings screen.
///
/// [`WriteAuthority`]'s field is private, so [`WriteAuthority::granted`] is the
/// only way to build one, and it hands back [`None`] under
/// [`Control::ReadOnly`]. [`Effect::WriteSetting`] and [`Effect::WriteDog`]
/// each carry one, so a handler cannot name a write without the check.
mod authority {
    use super::App;

    /// Proof that `--allow-control` was on when a settings write was built.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct WriteAuthority(());

    impl WriteAuthority {
        /// A token when `app`'s [`Control`](super::Control) permits writing,
        /// [`None`] otherwise.
        ///
        /// The sole constructor, taking `&App` rather than a `Control` so the
        /// answer can only be the app's own gate.
        #[must_use]
        pub fn granted(app: &App) -> Option<Self> {
            match app.control {
                super::Control::ReadOnly => None,
                super::Control::Allowed => Some(Self(())),
            }
        }
    }
}

pub use authority::WriteAuthority;

/// What the caller has to do after an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Nothing.
    None,
    /// Ask the link task for a `ListFlock` now, rather than at the next tick.
    PollNow,
    /// Re-read the selected sheep's log files and hand the result back as
    /// [`Msg::Bleats`]. The feed has no timer of its own; it rides this.
    RefreshFeed,
    /// Re-read the selected sheep's log files and ask the shepherd for its
    /// lambs. Raised only when the selection moved: a snapshot refreshes the
    /// feed alone, since it fires every two seconds.
    RefreshSelected,
    /// Send a request to the shepherd. Raised by [`App::confirm`] once an
    /// armed action's Enter lands; `super::run_ui` sends it.
    Send(Sent),
    /// Leave.
    Quit,
    /// Read `shep.toml`'s settings snapshot; the result lands as
    /// [`Msg::Settings`]. Raised by the dashboard's `s`; once the screen is
    /// open, `s` closes it instead.
    LoadSettings,
    /// Apply one edit to `shep.toml`; the result lands as
    /// [`Msg::SettingWritten`].
    ///
    /// Must run on `spawn_blocking`: `ConfigLock::acquire` blocks with no
    /// deadline, and the UI task's redraw, tick and bus drain would block with
    /// the write.
    WriteSetting(SettingEdit, WriteAuthority),
    /// Probe one dog for its config schema; the answer lands as
    /// [`Msg::DogPane`].
    ///
    /// A dog's schema is not persisted anywhere, so the pane asks at open:
    /// `shep adopt` uses the answer for the vet and records only the path.
    /// A built-in dog IS this binary, so it is asked in-process
    /// (`crate::dog::builtin_schema`); an adopted one is spawned with the
    /// schema flag, which is why `super::run_ui` runs this on
    /// `spawn_blocking` -- it costs up to `VERSION_BUDGET` of somebody
    /// else's binary starting up, and the redraw task cannot wait for that.
    ///
    /// No [`WriteAuthority`]: this reads. The pane it opens is gated on
    /// every keystroke that writes, the same as a sheep pane.
    LoadDogPane {
        /// The dog.
        name: String,
        /// The adopted binary, or [`None`] for a built-in.
        adopted_path: Option<PathBuf>,
    },
    /// Apply one dog's file half; the result lands as [`Msg::DogWritten`].
    ///
    /// Its own effect, not [`Self::WriteSetting`]: it ends in a request to the
    /// shepherd ([`Sent::Dog`]) where a scalar write ends in a notice.
    /// `spawn_blocking`, for [`Self::WriteSetting`]'s reason.
    WriteDog(DogEdit, WriteAuthority),
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
    /// values on screen stay as they were.
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
    /// When [`Self::info`] was received: the origin for this row's live
    /// uptime, so a value two seconds old is never rendered as current.
    pub anchor: Instant,
}

impl Row {
    /// What this row's STATUS cell reports: [`Self::info`]'s status, or
    /// [`Reported::Silent`] for a dog whose process is up and which has never
    /// handshook this shepherd.
    ///
    /// Mirrors `output::rows::reported`. The `dog` check is read here rather
    /// than left to [`Reported::of`], so a non-dog row can never be painted
    /// silent by a stray `handshook`.
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
/// The fields `output::rows`'s own `GroupTotals` sums for `shep flock`.
/// Restarts, cpu and memory are summed; uptime is the minimum, so a group reads
/// as time since the app was last disturbed.
#[derive(Debug, Clone)]
pub struct GroupTotals {
    /// How many instances make up this group.
    pub count: usize,
    /// Every instance's restarts, added up.
    pub restarts: u32,
    /// Every instance's CPU reading summed, `None` only when not one
    /// instance has a live reading.
    pub cpu: Option<f32>,
    /// Every instance's memory reading summed, `None` only when not one
    /// instance has a live reading.
    pub memory: Option<u64>,
    /// The minimum live uptime across instances, `None` only when the group
    /// has none.
    pub uptime_ms: Option<u64>,
}

/// One request the dashboard asked the link task to send, carried back on the
/// reply so it can be routed.
///
/// An echo tag rather than a correlation id: an `Err` reply carries no shape of
/// its own.
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
    /// One dog's daemon half, after its file half landed. `source` is what
    /// the write returned, so the request cannot disagree with the file.
    Dog {
        /// The dog's name.
        name: String,
        /// `true` to start it, `false` to stop and deregister it.
        enable: bool,
        /// Where the binary comes from, exactly as the config write
        /// answered.
        source: DogSource,
    },
    /// One sheep's effective config, for the config pane. Raised by `e` on
    /// the dashboard and by `r` from inside an open pane.
    ///
    /// A name rather than a [`RowKey`], because [`Request::SheepConfig`]
    /// takes one: the pane is about an app's stored spec, which every
    /// instance of a multi-instance app shares, so an id would name a
    /// narrower thing than the config it would come back with.
    SheepConfig {
        /// Which sheep was asked about.
        name: String,
    },
    /// One field of one sheep's config, off the config pane's own `Enter`.
    ///
    /// [`Self::SetEnv`]'s twin, and it is a `Request::SetSheepField`
    /// rather than a one-key `Request::ApplyConfig` for the reason that
    /// variant's own doc gives: an `ApplyConfig` at `ResetDepth::File`
    /// moves the field and then spends the operator's override for it,
    /// because that merge is a template load. This pane's value IS the
    /// operator's, so the sheep still differs from its file and the `*`
    /// marker has to keep saying so.
    ///
    /// The [`WriteAuthority`] is not decoration, for exactly the reason
    /// [`Effect::WriteSetting`]'s own doc gives: this variant cannot be
    /// named without having passed the gate, so a fifth door added
    /// tomorrow cannot walk around it.
    ApplyField {
        /// The sheep.
        name: String,
        /// Which write this is, so a reply settles its own in-flight line
        /// and no other. See [`PanePending::Sent`].
        ticket: u64,
        /// The field that moves.
        key: String,
        /// Its new value, in the shape that field serializes as. Wrapped,
        /// so a `{:?}` of this enum cannot print it: `cwd` and `script`
        /// hold a home directory and `args` holds a token (IR-41).
        value: FieldValue,
        /// Proof the control gate was open.
        authority: WriteAuthority,
    },
    /// One dog's `[<name>]` section, for the dog config pane. Raised once
    /// the pane's schema probe has answered, and again by `r` from inside an
    /// open dog pane and after a landed write.
    ///
    /// The SCHEMA is not asked for here and never travels the wire: it comes
    /// off the dog's own binary ([`Effect::LoadDogPane`]), because nothing
    /// records one -- `shep adopt` uses it for the vet and writes down only
    /// the path.
    DogSection {
        /// Which dog was asked about.
        name: String,
    },
    /// One dog's whole section, off the dog pane's own `Enter`.
    ///
    /// The whole section rather than one key, because
    /// `Request::SetDogConfig` replaces the table: `ConfigPane::edited_section`
    /// applies the edit through `toml_edit`, so the operator's comments and
    /// key order survive a write shep did not author.
    ///
    /// NOT a `Request::ApplyConfig`, and not for [`Self::ApplyField`]'s
    /// reason either -- a dog has no override store and no Flockfile. Its
    /// section is the operator's outright, and `dogs.toml` is the one copy
    /// of it.
    ///
    /// The [`WriteAuthority`] is not decoration, for the reason
    /// [`Effect::WriteSetting`]'s own doc gives.
    SetDogSection {
        /// The dog.
        name: String,
        /// Which write this is, so a reply settles its own in-flight line
        /// and no other. See [`PanePending::Sent`].
        ticket: u64,
        /// The section, edit applied. [`DogSectionToml`] rather than a
        /// `String` so a `{:?}` of this enum cannot print the webhook
        /// credentials a dog's section routinely holds (IR-41).
        toml: DogSectionToml,
        /// Proof the control gate was open.
        authority: WriteAuthority,
    },
    /// One env key of one sheep, off the env sub-screen's own `Enter`.
    ///
    /// Its own variant beside [`Self::ApplyField`] rather than a value of
    /// it, because the two are different requests: a whole env map is
    /// never sent (a pane is not told the values), so `SetSheepField`
    /// refuses the `env` key outright and `SetSheepEnv` takes one key at a
    /// time.
    SetEnv {
        /// The sheep.
        name: String,
        /// Which write this is, for [`Self::ApplyField`]'s reason.
        ticket: u64,
        /// The env key.
        key: String,
        /// The value, or `None` to remove the key. [`EnvValue`] rather than
        /// a `String` so a `{:?}` of this enum cannot print it (IR-41).
        value: Option<EnvValue>,
        /// Proof the control gate was open.
        authority: WriteAuthority,
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
            Self::Dog {
                name,
                enable: true,
                source,
            } => Request::EnableDog {
                name: name.clone(),
                source: source.clone(),
            },
            Self::Dog {
                name,
                enable: false,
                ..
            } => Request::DisableDog { name: name.clone() },
            Self::SheepConfig { name } => Request::SheepConfig { name: name.clone() },
            // One field, recorded as an operator override. NOT an
            // `ApplyConfig`: see this variant's own doc for the marker that
            // route silently spent.
            Self::ApplyField {
                name, key, value, ..
            } => Request::SetSheepField {
                name: name.clone(),
                key: key.clone(),
                // Unwrapped only here, at the wire. See `FieldValue` for
                // why the protocol's own field is a bare `Value` while
                // everything above it is not.
                value: value.as_value().clone(),
            },
            Self::DogSection { name } => Request::DogConfig { name: name.clone() },
            Self::SetDogSection { name, toml, .. } => Request::SetDogConfig {
                name: name.clone(),
                toml: toml.clone(),
            },
            Self::SetEnv {
                name, key, value, ..
            } => Request::SetSheepEnv {
                name: name.clone(),
                key: key.clone(),
                value: value.clone(),
            },
        }
    }
}

/// One dog toggle, ready for the file half: [`Effect::WriteDog`] carries one
/// and [`Msg::DogWritten`] echoes it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogEdit {
    /// The dog's name.
    pub name: String,
    /// `true` to enable, `false` to disable.
    pub enable: bool,
}

/// A dog's schema, and the binary it was probed from.
///
/// Held on [`App`] between the probe answering ([`Msg::DogPane`]) and the
/// section landing ([`Sent::DogSection`]), and then for as long as the pane
/// is open, so a refresh re-reads the section without respawning the dog.
///
/// `Debug` is derived rather than redacted (IR-41): a name, a binary's path
/// and a JSON Schema. A schema describes values without carrying any -- the
/// same argument `super::field::Field`'s own derived `Debug` makes, and a
/// dog's defaults come from its binary describing itself rather than from
/// this flock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogProbe {
    /// The dog.
    pub name: String,
    /// The adopted binary, or [`None`] for a built-in.
    pub adopted_path: Option<PathBuf>,
    /// What it answered the schema flag with.
    pub schema: serde_json::Value,
}

/// What the cursor can sit on: one sheep, or the header above an app's
/// instances.
///
/// A name earns a [`Self::Group`] only with more than one instance, every one
/// of them reporting its slot ([`App::is_grouped`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowKey {
    /// One app's group header, carrying its name.
    Group(String),
    /// One sheep, by id.
    Sheep(u32),
}

/// What one lamb fetch came back with. The pane says a different sentence for
/// each variant.
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
    /// True for a refusal or a damage report: the status bar picks
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    /// One of the six scalar fields, in [`Settings::rows`]'s fixed order.
    Scalar(SettingField),
    /// Index into [`SettingsSnapshot::dogs`].
    Dog(usize),
}

/// The settings screen's own state. `None` on [`App`] is the dashboard.
#[derive(Debug, Clone)]
pub struct Settings {
    snapshot: SettingsSnapshot,
    /// The six scalars' shape: which they are, in what order, under which
    /// section. The screen reads its rows, labels and section headers off
    /// this rather than off a `match` per question, so a config pane and
    /// this screen answer them the same way.
    fields: FieldSet,
    /// The cursor and, once a terminal has said how tall the body is, the
    /// scroll offset. Clamped on every read rather than kept pre-clamped: a
    /// refresh can shrink the dog list out from under a cursor already
    /// sitting past its new end.
    view: Viewport,
    /// The screen's one in-flight edit, or `None`. One field rather than
    /// several `Option`s, so typing, armed and sent cannot overlap.
    pending: Option<Pending>,
}

/// The settings screen's own in-flight edit.
#[derive(Debug, Clone)]
enum Pending {
    /// A free-text edit under construction. Only [`SettingField::Socket`] and
    /// [`SettingField::MaxCronSleep`] reach this, seeded with the field's
    /// on-disk value.
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
        /// The question this candidate reads as, rendered once at arm time.
        text: String,
        /// When it was armed. Only an armed edit expires.
        at: Instant,
    },
    /// [`Self::Armed`] for a [`DogEdit`] on a [`SettingsRow::Dog`] row, which
    /// [`App::confirm_setting`] sends through [`Effect::WriteDog`].
    DogArmed {
        /// The candidate toggle, ready to send.
        edit: DogEdit,
        /// The question this candidate reads as, rendered once at arm time.
        text: String,
        /// When it was armed. Only an armed edit expires.
        at: Instant,
    },
    /// [`Effect::WriteSetting`] or [`Effect::WriteDog`] is in flight. Carries
    /// no `edit`: every match site reads the landing message's own copy.
    Sent {
        /// The same rendered question, so the prompt line does not change
        /// wording between the question and its own answer.
        text: String,
    },
}

/// What the settings screen's status line shows for its one in-flight edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsPrompt<'a> {
    /// The confirm sentence: what will change, and what applying it does and
    /// does not do.
    pub text: &'a str,
    /// False while it is a question, true once it has gone out.
    pub sent: bool,
}

impl Settings {
    /// A freshly opened screen, cursor on the first row.
    fn new(snapshot: SettingsSnapshot) -> Self {
        Self {
            snapshot,
            fields: settings_field_set(),
            view: Viewport::new(),
            pending: None,
        }
    }

    /// The armed candidate and its prompt, or `None`.
    #[must_use]
    pub fn pending(&self) -> Option<SettingsPrompt<'_>> {
        match &self.pending {
            Some(Pending::Armed { text, .. } | Pending::DogArmed { text, .. }) => {
                Some(SettingsPrompt { text, sent: false })
            }
            Some(Pending::Sent { text, .. }) => Some(SettingsPrompt { text, sent: true }),
            Some(Pending::Typing { .. }) | None => None,
        }
    }

    /// Whether a candidate is waiting on `Enter`: the one state a stray key
    /// (movement, `Escape`, `Settings`, `Refresh`) eats rather than also doing
    /// its ordinary job.
    fn is_armed(&self) -> bool {
        matches!(
            self.pending,
            Some(Pending::Armed { .. } | Pending::DogArmed { .. })
        )
    }

    /// The field and buffer of an in-flight free-text edit, or `None`.
    #[must_use]
    pub fn typing(&self) -> Option<(&SettingField, &str)> {
        match &self.pending {
            Some(Pending::Typing { field, buffer }) => Some((field, buffer.as_str())),
            _ => None,
        }
    }

    /// The next candidate for `field`, or `None` for the two free-text fields.
    ///
    /// Advances from a candidate already armed for this field, so a second
    /// `space` walks one step further along the cycle. From nothing armed the
    /// base is what the file says, which for `[style] level` is
    /// [`SettingsSnapshot::style_level_in_file`] rather than the level in
    /// force: cycling the resolved level could propose a write that changes
    /// nothing.
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
        let in_file = (field == SettingField::StyleLevel)
            .then_some(self.snapshot.style_level_in_file.as_deref())
            .flatten();
        let base: String = match (armed_here, in_file) {
            (Some(value), _) | (None, Some(value)) => value.to_string(),
            // A `[style]` document declaring nothing falls back to
            // `StyleLevel`'s compiled default, as `style::resolve` does.
            (None, None) if field == SettingField::StyleLevel => STYLE_LEVEL_ORDER[0].to_string(),
            (None, None) => self.current_value(field)?.to_string(),
        };
        Some(match field {
            SettingField::LogLevel => next_log_level(&base),
            SettingField::LogJson | SettingField::AllowControl => next_bool(&base),
            SettingField::StyleLevel => next_style_level(&base),
            SettingField::Socket | SettingField::MaxCronSleep => return None,
        })
    }

    /// The snapshot's own rendered value for one of the four cycled scalars.
    /// `None` for the two free-text ones.
    fn current_value(&self, field: SettingField) -> Option<&str> {
        Some(match field {
            SettingField::LogLevel => self.snapshot.log_level.value.as_str(),
            SettingField::LogJson => self.snapshot.log_json.value.as_str(),
            SettingField::AllowControl => self.snapshot.allow_control.value.as_str(),
            SettingField::StyleLevel => self.snapshot.style_level.value.as_str(),
            SettingField::Socket | SettingField::MaxCronSleep => return None,
        })
    }

    /// Which layer `field`'s value came from. Only [`confirm_text`]'s `[style]`
    /// arm acts on it.
    fn source_of(&self, field: SettingField) -> StyleSource {
        match field {
            SettingField::LogLevel => self.snapshot.log_level.source,
            SettingField::LogJson => self.snapshot.log_json.source,
            SettingField::Socket => self.snapshot.socket.source,
            SettingField::MaxCronSleep => self.snapshot.max_cron_sleep.source,
            SettingField::AllowControl => self.snapshot.allow_control.source,
            SettingField::StyleLevel => self.snapshot.style_level.source,
        }
    }

    /// The rendered value [`App::confirm_setting`] seeds [`Pending::Typing`]'s
    /// buffer with. Only the two free-text fields reach it.
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

    /// What the screen reads off disk, and renders every row's value and source
    /// from.
    ///
    /// A landed write does not update this in place: it raises a fresh
    /// [`Effect::LoadSettings`], so `Set` and `Unset` land the same way and
    /// neither can drift from the rest of the document.
    #[must_use]
    pub fn snapshot(&self) -> &SettingsSnapshot {
        &self.snapshot
    }

    /// Every row the cursor can sit on: the six scalars in their fixed
    /// order, then one row per candidate dog.
    #[must_use]
    pub fn rows(&self) -> Vec<SettingsRow> {
        let mut rows: Vec<SettingsRow> = self
            .fields
            .fields()
            .iter()
            .filter_map(|f| SettingField::from_key(&f.key))
            .map(SettingsRow::Scalar)
            .collect();
        rows.extend((0..self.snapshot.dogs.len()).map(SettingsRow::Dog));
        rows
    }

    /// The field model behind the scalar rows.
    #[must_use]
    pub fn fields(&self) -> &FieldSet {
        &self.fields
    }

    /// The row the cursor sits on. `None` only if [`Self::rows`] is empty,
    /// which the six unconditional scalars make unreachable.
    #[must_use]
    pub fn cursor(&self) -> Option<SettingsRow> {
        let rows = self.rows();
        rows.get(self.view.cursor().min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// Moves the cursor by `delta` rows, clamped to [`Self::rows`], never
    /// wrapping.
    fn move_by(&mut self, delta: isize) {
        let len = self.rows().len();
        self.view.move_by(delta, len);
    }

    fn move_to_first(&mut self) {
        let len = self.rows().len();
        self.view.move_to(0, len);
    }

    fn move_to_last(&mut self) {
        let len = self.rows().len();
        self.view.move_to(len.saturating_sub(1), len);
    }

    /// The viewport, for the renderer.
    #[must_use]
    pub fn view(&self) -> &Viewport {
        &self.view
    }

    /// Records the terminal's height.
    pub fn set_rows(&mut self, rows: usize) {
        let len = self.rows().len();
        self.view.set_rows(rows, len);
    }
}

/// [`LogLevel`]'s own declared order, wrapping from `Trace` back to `Off`.
pub(crate) const LOG_LEVEL_ORDER: [LogLevel; 6] = [
    LogLevel::Off,
    LogLevel::Error,
    LogLevel::Warn,
    LogLevel::Info,
    LogLevel::Debug,
    LogLevel::Trace,
];

/// One step along [`LOG_LEVEL_ORDER`] from `current`. An unparseable value
/// reads as `Warn`, so it still produces a legal next one.
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

/// Flips `"true"`/`"false"`. Anything else reads as `false`.
fn next_bool(current: &str) -> String {
    (current != "true").to_string()
}

/// [`StyleLevel`]'s own declared order, wrapping from `Bare` back to `Full`.
pub(crate) const STYLE_LEVEL_ORDER: [StyleLevel; 3] =
    [StyleLevel::Full, StyleLevel::Plain, StyleLevel::Bare];

/// One step along [`STYLE_LEVEL_ORDER`] from `current`. An unparseable value
/// reads as `Full`.
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

/// The confirm sentence for `field`'s candidate `value`, verbatim. `value` is
/// what a `next_*` function produced, never re-derived here.
///
/// Only [`SettingField::StyleLevel`] reads `source`: the other three can only
/// warn about the shepherd's env and flags, which lookout cannot see.
fn confirm_text(field: SettingField, value: &str, source: StyleSource) -> String {
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
        SettingField::StyleLevel => style_confirm_text(value, source),
        SettingField::Socket | SettingField::MaxCronSleep => unreachable!(
            "Settings::next_candidate never arms these two -- they are task 8's Pending::Typing"
        ),
    }
}

/// The `[style] level` half of [`confirm_text`].
///
/// Under `Env` or `Flag` the write lands and the level in force does not move,
/// so the sentence names the layer that keeps winning.
fn style_confirm_text(value: &str, source: StyleSource) -> String {
    match source {
        StyleSource::Config | StyleSource::Default => {
            format!("set style level to {value}? the next command reads it")
        }
        StyleSource::Env => format!(
            "set style level to {value}? it goes in the file, but $SHEP_STYLE is set and keeps winning until it is unset"
        ),
        StyleSource::Flag => format!(
            "set style level to {value}? it goes in the file, but --style was passed to this lookout and keeps winning for as long as it runs"
        ),
    }
}

/// The confirm sentence for a free-text edit, verbatim.
///
/// Only [`SettingField::Socket`] and [`SettingField::MaxCronSleep`] reach it;
/// the other four go through [`confirm_text`] and, not being optional, are
/// never [`SettingEdit::Unset`].
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

/// What `Msg::SettingWritten`'s `Err` arm reopens [`Pending::Typing`] with: the
/// field and the text the operator typed. `None` for the four cycled fields,
/// which have no editor to reopen.
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
/// The target is captured at arm time and never re-read from the selection: a
/// snapshot can land between the keypress and the Enter.
///
/// One field on [`App`] rather than two `Option`s, so "armed" and "in flight"
/// cannot both be true.
#[derive(Debug, Clone)]
struct Action {
    verb: ActionVerb,
    target: RowKey,
    name: String,
    /// How many processes [`Self::target`] reaches, captured at arm time: 1 for
    /// a sheep, the group's own size for a [`RowKey::Group`].
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
/// Ten seconds: a prompt left armed while the operator walks away is the same
/// fat finger by a slower route. Rides `Msg::Tick`, so it needs no timer.
pub const CONFIRM_EXPIRY: Duration = Duration::from_secs(10);

/// The sentence `r` and the action keys both give when the link is gone.
const LINK_GONE: &str = "the shepherd is gone — nothing left to ask";

/// The sentence every closed-gate refusal gives, dashboard and settings alike.
const READ_ONLY_REFUSAL: &str = "read-only: actions need --allow-control";

/// The whole dashboard's state.
#[derive(Debug)]
pub struct App {
    flock: BTreeMap<u32, Row>,
    /// Which row the detail pane and the bleats feed describe. `None` only for
    /// an empty flock.
    ///
    /// A [`RowKey`], not an index: the flock map is replaced wholesale every
    /// two seconds, and an index would silently start pointing at a different
    /// sheep. The viewport offset is derived from this
    /// ([`super::view::flock::scroll_offset`]).
    selected: Option<RowKey>,
    /// The live substring filter over sheep names, empty when there is none.
    ///
    /// Case-insensitive `contains`, taken literally with no trimming. Not the
    /// CLI's selector grammar, which is exact-match and so cannot narrow as you
    /// type.
    filter: String,
    /// Which keymap [`super::input::map_key`] is called with. Normal until `/`
    /// opens the box; the reducer, not the keymap, owns this state.
    mode: InputMode,
    /// The next config-write ticket. Monotonic and never reused, so a reply
    /// can only settle the write it belongs to -- see [`PanePending::Sent`].
    next_write_ticket: u64,
    link: Link,
    notice: Option<Notice>,
    palette: Palette,
    control: Control,
    /// The `$SHEP_HOME` this lookout watches, for the title line.
    home: String,
    /// The clock the view reads. Advanced by [`Msg::Tick`], and never once the
    /// link is [`Link::Lost`], so a frozen dashboard's uptime column stops.
    now: Instant,
    /// The last host reading, or `None` before the first heartbeat and on a
    /// platform `sysinfo` does not support. [`Self::host_unsupported`] tells
    /// the strip which of the two it is looking at.
    host: Option<super::source::HostSample>,
    /// True once a sample has come back `None`, which the strip says a
    /// different sentence for than a heartbeat that has not fired yet.
    host_unsupported: bool,
    /// The selected sheep's most recent output, as of the last refresh. An
    /// empty, unlabelled tail before the first one, which the feed reads the
    /// same way as a sheep that has written nothing.
    feed: super::tail::Tail,
    /// The last lamb reading, or `None` before there has been one. Keyed by the
    /// id it was taken for, so a stale reading and a dropped request both read
    /// as "not read yet".
    lambs: Option<LambReading>,
    /// The one action this dashboard is in the middle of, or `None`.
    action: Option<Action>,
    /// The settings screen's own state. `None` is the dashboard.
    settings: Option<Settings>,
    /// Which sheep a config pane is open for, or wanted for.
    ///
    /// Set when the read goes out, cleared when the pane closes, checked
    /// when a reply lands. Without it, two `e` presses and an `Esc` could
    /// reopen a closed pane on the late reply. [`Self::config_pane`] alone
    /// cannot tell those two `None` states apart.
    config_target: Option<String>,
    /// The dog a config pane is open for, or wanted for, and the schema its
    /// binary answered with.
    ///
    /// [`Self::config_target`]'s twin. Carries a schema because it is
    /// probed once at open and reused on every re-read, so `r` never
    /// respawns the dog's binary. Cleared alongside `config_target`.
    dog_target: Option<DogProbe>,
    /// The open config pane, or `None`. Opened by `e` on a selected sheep,
    /// and closed by `e` or `Escape` from inside it.
    ///
    /// A sibling of [`Self::settings`] rather than a field on it: the two
    /// screens open from different places and neither is reachable from the
    /// other. `on_key` checks this one first, so a pane opened over the
    /// dashboard owns the keyboard for as long as it is up.
    config_pane: Option<ConfigPane>,
    /// The resolved style level and which layer chose it. Defaulted here and
    /// overridden through [`Self::set_style`], so the STYLE LEVEL row reads the
    /// same answer the rest of the CLI does.
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
            next_write_ticket: 0,
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
            config_target: None,
            dog_target: None,
            config_pane: None,
            style: (StyleLevel::Full, StyleSource::Default),
        }
    }

    /// Applies one message and reports what the caller must do next.
    pub fn update(&mut self, msg: Msg) -> Effect {
        match msg {
            Msg::Snapshot { rows, at } => {
                // The link task has ended, so nothing is left to produce a
                // snapshot. Accepting one would un-freeze the dashboard.
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
                // Unconditional: the selected row's log paths can change even
                // when the selection does not, and this is the feed's cadence.
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
                // Against the tick's own `now`, not `self.now`, which stops on
                // a dead link: a settings edit describes a local file that is
                // no staler for the shepherd being gone.
                if let Some(settings) = self.settings.as_mut() {
                    let expired = matches!(
                        settings.pending,
                        Some(Pending::Armed { at, .. } | Pending::DogArmed { at, .. })
                            if now.saturating_duration_since(at) >= CONFIRM_EXPIRY
                    );
                    if expired {
                        settings.pending = None;
                    }
                }
                // The pane's own expiry, and it sits OUTSIDE the link guard
                // for a different reason than the settings screen's: the
                // question describes the shepherd, which a lost link froze,
                // but an armed edit that can never be sent is not a
                // question worth leaving on screen either. `now` rather
                // than `self.now`, which that guard stops advancing.
                if let Some(pane) = self.config_pane.as_mut()
                    && pane
                        .armed_at()
                        .is_some_and(|at| now.saturating_duration_since(at) >= CONFIRM_EXPIRY)
                {
                    pane.cancel();
                }
                Effect::None
            }
            Msg::Resize => Effect::None,
            Msg::Key(key) => self.on_key(key),
            Msg::Host { sample } => {
                // A strip ticking over under a banner saying the values are
                // frozen contradicts it on one frame.
                if matches!(self.link, Link::Lost { .. }) {
                    return Effect::None;
                }
                self.host_unsupported = sample.is_none();
                self.host = sample;
                Effect::None
            }
            // Always `Effect::None`: answering a feed update with another
            // refresh would spin the UI task. The guard catches a read `run_ui`
            // armed before the freeze landed.
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
                Sent::Dog { name, enable, .. } => self.on_dog_reply(name, enable, result),
                Sent::SheepConfig { name } => self.on_sheep_config(&name, result),
                Sent::DogSection { name } => self.on_dog_section(&name, result),
                Sent::SetDogSection { name, ticket, .. } => {
                    self.on_dog_section_set(&name, ticket, result)
                }
                Sent::ApplyField {
                    name, ticket, key, ..
                } => self.on_field_applied(&name, ticket, &key, result),
                Sent::SetEnv {
                    name,
                    ticket,
                    key,
                    value,
                    ..
                } => self.on_env_set(&name, ticket, &key, value.is_some(), result),
            },
            Msg::Unsent { sent } => match sent {
                Sent::Action { verb, target, name } => {
                    self.action = None;
                    self.notice = Some(Notice {
                        // No cause: `Full` is reachable while the shepherd is
                        // merely slow, so naming one would invent it.
                        text: format!("{}: it was not sent", target_prefix(verb, &target, &name)),
                        grave: true,
                    });
                    Effect::None
                }
                // A dropped lamb fetch already reads as "not read yet".
                Sent::Lambs { .. } => Effect::None,
                // A config read nobody took, reported rather than
                // swallowed: silence here looks like a key that is not
                // bound. Nothing was armed, so this is the whole report.
                Sent::SheepConfig { name } => {
                    self.notice = Some(Notice {
                        text: format!("{name}: its config was not asked for"),
                        grave: true,
                    });
                    Effect::None
                }
                // Both write arms clear the pane's own `Sent` line, the
                // same shape `Sent::Action`'s arm above uses: a bar still
                // saying "sent, waiting for the shepherd" about a request
                // nobody has is the failure `Msg::Unsent` exists for.
                Sent::ApplyField {
                    name, ticket, key, ..
                } => {
                    if let Some(pane) = self.config_pane.as_mut() {
                        pane.settle(ticket);
                    }
                    self.notice = Some(Notice {
                        text: format!("{name}: {key} was not sent"),
                        grave: true,
                    });
                    Effect::None
                }
                Sent::SetEnv {
                    name, ticket, key, ..
                } => {
                    if let Some(pane) = self.config_pane.as_mut() {
                        pane.settle(ticket);
                    }
                    self.notice = Some(Notice {
                        text: format!("{name}: env {key} was not sent"),
                        grave: true,
                    });
                    Effect::None
                }
                // The dog twins of the two arms above: a read nobody took
                // is reported, and a write nobody took clears the pane's
                // "sent, waiting" line.
                Sent::DogSection { name } => {
                    self.notice = Some(Notice {
                        text: format!("{name}: its config was not asked for"),
                        grave: true,
                    });
                    Effect::None
                }
                Sent::SetDogSection { name, ticket, .. } => {
                    if let Some(pane) = self.config_pane.as_mut() {
                        pane.settle(ticket);
                    }
                    self.notice = Some(Notice {
                        text: format!("{name}: its config was not sent"),
                        grave: true,
                    });
                    Effect::None
                }
                // The arm above, against the settings screen's pending line.
                Sent::Dog { name, enable, .. } => {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.pending = None;
                    }
                    let verb = if enable { "enable" } else { "disable" };
                    self.notice = Some(Notice {
                        text: format!("{verb} {name}: it was not sent"),
                        grave: true,
                    });
                    Effect::None
                }
            },
            // The screen opens on what this read found; a failed read leaves
            // the dashboard up. A landed write's re-read and `r` land here too,
            // with `self.settings` already `Some`, so `opening` is false and
            // the cursor survives.
            Msg::Settings { result } => {
                let opening = self.settings.is_none();
                match result {
                    Ok(snapshot) => {
                        // An action armed while the read was in flight: once
                        // the screen is up, `on_settings_key` no-ops `Confirm`
                        // and the prompt would be unreachable.
                        self.action = None;
                        // A filter box left open would keep eating every
                        // keystroke the settings keymap owns: `on_key` checks
                        // the mode first. The query itself is kept.
                        self.mode = InputMode::Normal;
                        // Reset to the top only while `opening`.
                        // `Settings::cursor` clamps on every read, so a
                        // preserved `Viewport` past a shorter dogs list
                        // still lands somewhere real.
                        let view = self.settings.as_ref().map(|settings| settings.view.clone());
                        let mut settings = Settings::new(snapshot);
                        if !opening {
                            if let Some(view) = view {
                                settings.view = view;
                            }
                            let len = settings.rows().len();
                            settings.view.clamp(len);
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
            // `Ok` re-reads rather than folding the write into the row, which
            // covers `Unset` too. `Err` reopens the editor for the two
            // free-text fields, so a long path need not be retyped.
            Msg::SettingWritten { edit, result } => match result {
                Ok(()) => {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.pending = None;
                    }
                    Effect::LoadSettings
                }
                Err(message) => {
                    // Split so no borrow of `self.settings` is held across the
                    // `self.notice` assignment below.
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
            // A dog's schema probe answered. `Ok` parks the schema and asks
            // the shepherd for the section; the pane is built once that
            // lands. `Err` gets no pane, and the refusal names the file to
            // edit instead. The settings screen stays open until then.
            Msg::DogPane {
                name,
                adopted_path,
                result,
            } => match result {
                Ok(schema) => {
                    self.dog_target = Some(DogProbe {
                        name: name.clone(),
                        adopted_path,
                        schema,
                    });
                    Effect::Send(Sent::DogSection { name })
                }
                Err(message) => {
                    self.notice = Some(Notice {
                        text: message,
                        grave: true,
                    });
                    Effect::None
                }
            },
            // `Ok` raises the daemon half: `Cycle` arms, `Confirm` writes the
            // file, this arm asks the shepherd. `Err` never reaches it, since
            // there is nothing for the daemon half to agree with.
            Msg::DogWritten { edit, result } => match result {
                Ok(source) => Effect::Send(Sent::Dog {
                    name: edit.name,
                    enable: edit.enable,
                    source,
                }),
                Err(message) => {
                    if let Some(settings) = self.settings.as_mut() {
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
        // Armed before the freeze could land: a reading reaching the frame now
        // would be newer than the banner over it.
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
            // Neither an `Err` nor an unrecognised reply is an empty walk:
            // reporting one would say "none found" about nothing read.
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
    /// Nothing provisional is invented; all three replies carry the rows.
    fn on_action_reply(
        &mut self,
        verb: ActionVerb,
        target: RowKey,
        name: &str,
        result: Result<Response, RequestError>,
    ) -> Effect {
        self.action = None;
        let prefix = target_prefix(verb, &target, name);
        // Each verb accepts its own reply and no other: a `Stopped` answering
        // a `Restart` carries rows and would upsert happily.
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
            // The daemon's own message: `RequestError`'s `Display` would put a
            // Rust identifier on an operator's screen.
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

    /// One dog toggle's answer: the `Pending::Sent` line clears, one sentence
    /// lands in the status bar, and the screen re-reads `shep.toml`.
    ///
    /// [`Effect::LoadSettings`] on every arm, `Err` included: the file half has
    /// already landed, so `DogView.enabled` is stale whatever the shepherd
    /// said. No row is upserted; the next `ListFlock` repairs RUNNING.
    fn on_dog_reply(
        &mut self,
        name: String,
        enable: bool,
        result: Result<Response, RequestError>,
    ) -> Effect {
        if let Some(settings) = self.settings.as_mut() {
            settings.pending = None;
        }
        let verb = if enable { "enable" } else { "disable" };
        let prefix = format!("{verb} {name}");
        // `EnableDog` answers `Response::DogStarted`; `DisableDog` answers
        // `Response::Deleted`, the same reply `Delete` gives.
        match result {
            Ok(Response::DogStarted(_)) if enable => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: the shepherd started it"),
                    grave: false,
                });
            }
            Ok(Response::Deleted(_)) if !enable => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: the shepherd stopped and deregistered it"),
                    grave: false,
                });
            }
            // Also a mismatched guard above: an `EnableDog` answered by
            // `Response::Deleted`, or the reverse.
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{prefix}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
            }
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: {}", err.message),
                    grave: true,
                });
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: {other}"),
                    grave: true,
                });
            }
        }
        Effect::LoadSettings
    }

    /// One `Request::SheepConfig` reply. Opens the pane, or refreshes an
    /// open one in place.
    ///
    /// The cursor and offset are carried across a refresh rather than reset
    /// to the first field, the same rule `Msg::Settings` follows and for the
    /// same reason: `r` from inside the pane, and every later re-read, must
    /// not throw an operator who was reading `cron_restart` back to `name`.
    /// [`ConfigPane::adopt_view`] clamps what it adopts, so a field list
    /// that came back shorter cannot leave the cursor past its end.
    ///
    /// A failed read leaves whatever is on screen exactly as it was and
    /// raises a grave notice, so a refusal is reported rather than
    /// swallowed, and a refresh that fails does not blank a pane that was
    /// showing something real.
    fn on_sheep_config(&mut self, name: &str, result: Result<Response, RequestError>) -> Effect {
        // A reply nobody is waiting for any more: two `e` presses against a
        // slow shepherd, then `Esc`, and the second answer used to re-open
        // the pane over a dashboard the operator had gone back to. Dropped
        // in silence, because nothing went wrong and the operator asked for
        // nothing that is still outstanding.
        if self.config_target.as_deref() != Some(name) {
            return Effect::None;
        }
        match result {
            Ok(Response::SheepConfig(view)) => {
                let carried = self.config_pane.as_ref().map(|pane| pane.view().clone());
                // The env sub-screen is carried too, and it has to be: a
                // set re-reads the whole config, so without this the
                // sub-screen would close on the operator's own keystroke --
                // on the one screen where that keystroke ADDS a row.
                // The cursor's KEY rides across, not its index: a removal
                // shortens the list, so the same index names the NEIGHBOUR
                // afterwards and a reflexive second `Enter` would arm a
                // write against a key nobody chose. See `EnvPane::adopt_view`.
                let carried_env = self
                    .config_pane
                    .as_ref()
                    .and_then(ConfigPane::env)
                    .map(|env| (env.view().clone(), env.cursor_key().map(str::to_owned)));
                // A question the operator has not answered, or a write
                // still out, survives the rebuild. Only `Typing` is
                // dropped -- see `ConfigPane::adopt_pending_edit`.
                let carried_edit = self
                    .config_pane
                    .as_ref()
                    .and_then(|pane| pane.pending_edit().cloned());
                let mut pane = ConfigPane::sheep(*view);
                pane.adopt_pending_edit(carried_edit);
                if let Some(carried) = carried {
                    pane.adopt_view(carried);
                }
                if let Some((carried, cursor_key)) = carried_env {
                    pane.adopt_env_view(carried, cursor_key.as_deref());
                }
                self.config_pane = Some(pane);
                // The rebuilt pane carries no editor, so the keyboard must
                // not still think one is open.
                self.release_text_mode_if_unowned();
            }
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{name}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
            }
            // Prefixed, like every other reply handler in this file, and
            // NOT stripped on the theory that the shepherd's refusals name
            // their own subject. Two of the three do -- `no sheep named
            // {name}` and the `IsADog` sentence -- and the third,
            // `EngineStopped`, renders as `the supervisor engine has
            // stopped`, which names neither the sheep nor the screen it
            // came from. Stripping unconditionally traded a doubled name in
            // two cases for an unattributable notice in the third.
            //
            // The doubling that motivated the strip is fixed in the right
            // place instead: `Self::ask_for_config` refuses a dog off the
            // local row and never sends, so `IsADog` no longer reaches this
            // arm by the door an operator actually uses. It is not
            // structurally unreachable -- that guard reads a flock snapshot
            // up to two seconds old, and `r` from inside the pane re-sends
            // by name without re-checking -- so a sheep deleted and a dog
            // adopted under the same name in between would still land here
            // and read `web: web is a dog, ...`. That is a cosmetic double
            // in a case needing a name collision across a delete; the
            // alternative was silence about which sheep every time the
            // engine stops.
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {}", err.message),
                    grave: true,
                });
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {other}"),
                    grave: true,
                });
            }
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
                // An upsert can orphan the selection from `visible_rows()`
                // without touching `self.flock`: a rename can move the selected
                // row out of the filter. `reseat` is a no-op read while the
                // selection is still seated.
                let previous = self.selected_index();
                let anchor = self.now;
                self.flock.insert(info.id, Row { info, anchor });
                if self.reseat(previous) {
                    return Effect::RefreshSelected;
                }
                Effect::None
            }
            // The shepherd's own outbound queue overflowed. Worded differently
            // from `Msg::BusLagged`: an operator cannot tell which end of the
            // connection to investigate if the two read the same.
            BusEvent::Dropped { count } => {
                self.notice = Some(Notice {
                    text: format!("the shepherd dropped {count} events; re-reading the flock"),
                    grave: false,
                });
                Effect::PollNow
            }
            // A notice, not an exit: a dashboard that vanished would take the
            // last known state with it.
            BusEvent::DaemonShutdown => {
                self.notice = Some(Notice {
                    text: "the shepherd is shutting down".to_string(),
                    grave: true,
                });
                Effect::None
            }
            // `BusEvent` is `#[non_exhaustive]`: a newer shepherd's variant
            // must not take the dashboard down.
            _ => Effect::None,
        }
    }

    /// One `Request::DogConfig` reply: the dog's section, which is the
    /// second half of an open.
    ///
    /// Guarded on [`Self::dog_target`] the way [`Self::on_sheep_config`] is
    /// guarded on `config_target`, and for the same reason: a reply for a
    /// dog the operator has already left must not re-open a pane over the
    /// screen they went back to.
    ///
    /// The schema comes off `dog_target` rather than off the wire. Nothing
    /// records a dog's schema, so the probe that ran at open is the only
    /// copy, and a refresh reuses it rather than respawning the binary.
    fn on_dog_section(&mut self, name: &str, result: Result<Response, RequestError>) -> Effect {
        let Some(probe) = self.dog_target.clone().filter(|probe| probe.name == name) else {
            return Effect::None;
        };
        match result {
            Ok(Response::DogSection { toml }) => {
                // Everything a refresh has to carry across, read before the
                // rebuild replaces the pane: see `Self::on_sheep_config`,
                // which states the argument for each. A dog pane has no env
                // sub-screen, so only two of the three apply.
                let carried = self.config_pane.as_ref().map(|pane| pane.view().clone());
                let carried_edit = self
                    .config_pane
                    .as_ref()
                    .and_then(|pane| pane.pending_edit().cloned());
                let mut pane = ConfigPane::dog(
                    probe.name,
                    probe.adopted_path,
                    probe.schema,
                    toml.as_str().to_owned(),
                );
                pane.adopt_pending_edit(carried_edit);
                if let Some(carried) = carried {
                    pane.adopt_view(carried);
                }
                self.config_pane = Some(pane);
                // The settings screen is what a dog pane opens over, and it
                // closes only now -- once there is something to look at.
                self.settings = None;
                self.release_text_mode_if_unowned();
            }
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{name}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
            }
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {}", err.message),
                    grave: true,
                });
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {other}"),
                    grave: true,
                });
            }
        }
        Effect::None
    }

    /// One `Request::SetDogConfig` reply.
    ///
    /// The pane settles either way, and a success re-reads the section --
    /// the same shape [`Self::on_field_applied`] has, for the same reason:
    /// the screen shows what the shepherd now holds rather than what the
    /// operator asked for.
    ///
    /// The sentence says the change was published and stops there. Whether
    /// the dog acted on it is the dog's own answer, which this reply does
    /// not carry and shep cannot predict -- the same fact the pane's footer
    /// states (decision 4).
    fn on_dog_section_set(
        &mut self,
        name: &str,
        ticket: u64,
        result: Result<Response, RequestError>,
    ) -> Effect {
        if let Some(pane) = self.config_pane.as_mut() {
            pane.settle(ticket);
        }
        match result {
            Ok(Response::DogConfigSet { .. }) => {
                self.notice = Some(Notice {
                    text: format!("{name}: its config is written, and {name} is told"),
                    grave: false,
                });
                Effect::Send(Sent::DogSection {
                    name: name.to_owned(),
                })
            }
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{name}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
                Effect::None
            }
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {}", err.message),
                    grave: true,
                });
                Effect::None
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {other}"),
                    grave: true,
                });
                Effect::None
            }
        }
    }

    /// One `Request::SetSheepField` reply.
    ///
    /// The pane settles either way -- a question that has been answered is
    /// not still in flight -- and a success re-reads the config so the
    /// screen shows what the shepherd now holds rather than what the
    /// operator asked for. `Effect::Send` rather than a second effect kind,
    /// so the re-read gets `Msg::Unsent` handling for free like every other
    /// request this reducer raises.
    ///
    /// `pending` is the shepherd's answer and not this pane's guess: the
    /// cost column reads `apply_group`, which is right about the field and
    /// cannot know that `autostart` is in force the moment it lands or that
    /// a config subset failed to normalize on its own.
    fn on_field_applied(
        &mut self,
        name: &str,
        ticket: u64,
        key: &str,
        result: Result<Response, RequestError>,
    ) -> Effect {
        if let Some(pane) = self.config_pane.as_mut() {
            pane.settle(ticket);
        }
        match result {
            Ok(Response::SheepFieldSet { pending, .. }) => {
                self.notice = Some(Notice {
                    text: if pending {
                        format!("{name}: {key} is set, and waits for `shep reload {name}`")
                    } else {
                        format!("{name}: {key} is set")
                    },
                    grave: false,
                });
                Effect::Send(Sent::SheepConfig {
                    name: name.to_owned(),
                })
            }
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{name}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
                Effect::None
            }
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {}", err.message),
                    grave: true,
                });
                Effect::None
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {other}"),
                    grave: true,
                });
                Effect::None
            }
        }
    }

    /// One `Request::SetSheepEnv` reply.
    ///
    /// `was_set` comes off the request rather than the reply: the answer
    /// names the key and deliberately never the value, so it cannot say
    /// which of the two things happened.
    ///
    /// Env is spawn-time in every case (`AppConfig::env` is
    /// `ApplyGroup::NeedsRespawn`), so the sentence says so unconditionally
    /// rather than reading a list that this reply does not carry.
    fn on_env_set(
        &mut self,
        name: &str,
        ticket: u64,
        key: &str,
        was_set: bool,
        result: Result<Response, RequestError>,
    ) -> Effect {
        if let Some(pane) = self.config_pane.as_mut() {
            pane.settle(ticket);
        }
        match result {
            Ok(Response::SheepEnvSet { .. }) => {
                let verb = if was_set { "set" } else { "removed" };
                self.notice = Some(Notice {
                    text: format!("{name}: env {key} {verb}, and waits for `shep reload {name}`"),
                    grave: false,
                });
                Effect::Send(Sent::SheepConfig {
                    name: name.to_owned(),
                })
            }
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{name}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
                Effect::None
            }
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {}", err.message),
                    grave: true,
                });
                Effect::None
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{name}: {other}"),
                    grave: true,
                });
                Effect::None
            }
        }
    }

    fn on_key(&mut self, key: KeyPress) -> Effect {
        // While the box is open every key is text.
        if self.mode == InputMode::Text {
            return self.on_text_key(key);
        }
        // The config pane owns the keyboard while it is open, ahead of the
        // settings screen and the armed-confirm check below. The two
        // screens cannot coexist, so this ordering is a documentation
        // choice, not a correctness one.
        if self.config_pane.is_some() {
            return self.on_pane_key(key);
        }
        // The settings screen owns its own keymap while it is open.
        if self.settings.is_some() {
            return self.on_settings_key(key);
        }
        // A cancelling keypress is consumed: a stray `j` cancels the confirm
        // and does not also move the selection, or the next reflexive Enter
        // acts on a target the operator lost track of. Cancelling is silent.
        if self
            .action
            .as_ref()
            .is_some_and(|action| action.stage == Stage::Armed)
        {
            if key == KeyPress::Confirm {
                return self.confirm();
            }
            // The one key the cancel does not consume: an operator whose
            // Ctrl-C does nothing reaches for `kill -9`, past every restore
            // path `super::term` has. Quitting discards the confirm.
            if key == KeyPress::Quit {
                return Effect::Quit;
            }
            self.action = None;
            return Effect::None;
        }
        self.notice = None;
        match key {
            KeyPress::Quit => Effect::Quit,
            // The one key whose meaning depends on state, and the bar reads
            // `esc clear` for exactly as long as clearing is what it does.
            KeyPress::Escape => {
                if self.filter.is_empty() {
                    Effect::Quit
                } else {
                    self.set_filter(String::new())
                }
            }
            // Once the link task has ended its poll receiver is gone, so an
            // `Effect::PollNow` would be silence with no reason for it.
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
            // Enter means nothing outside an armed confirm, including while one
            // is in flight: the routing rule above fires only on `Stage::Armed`.
            KeyPress::Confirm => Effect::None,
            KeyPress::FilterStart => {
                self.mode = InputMode::Text;
                Effect::None
            }
            // `map_key` produces these only in text mode, which the branch at
            // the top of this function has already taken.
            KeyPress::TextChar(_)
            | KeyPress::TextBackspace
            | KeyPress::TextApply
            | KeyPress::TextAbandon => Effect::None,
            // The read, not the open: the screen opens only once
            // `Msg::Settings` lands.
            KeyPress::Settings => Effect::LoadSettings,
            // Also the read, not the open: the pane shows the shepherd's
            // answer or nothing. `selected_row` is `None` for a group too,
            // but a group's name is what `Request::SheepConfig` wants, so
            // `e` still works on a multi-instance app's default row.
            KeyPress::Edit => self.ask_for_config(),
            // `space` acts only on the settings screen.
            KeyPress::Cycle => Effect::None,
        }
    }

    /// The settings screen's own keymap, in force while [`Self::settings`] is
    /// `Some`. Everything not named here is ignored, an action key included.
    fn on_settings_key(&mut self, key: KeyPress) -> Effect {
        self.notice = None;
        match key {
            KeyPress::Quit => return Effect::Quit,
            // Both close, but an armed confirm eats the first one, the
            // cancel-before-act rule the dashboard follows. `Escape` closing
            // rather than quitting is where this screen swaps that cascade.
            KeyPress::Settings | KeyPress::Escape => {
                let armed = self.settings.as_ref().is_some_and(Settings::is_armed);
                if armed {
                    if let Some(settings) = self.settings.as_mut() {
                        settings.pending = None;
                    }
                } else {
                    self.settings = None;
                }
            }
            // An armed candidate eats the first movement key rather than also
            // moving: the next reflexive Enter would otherwise apply an edit to
            // a row the operator lost track of. `Sent` is untouched.
            KeyPress::SelectUp
            | KeyPress::SelectDown
            | KeyPress::SelectFirst
            | KeyPress::SelectLast => {
                if let Some(settings) = self.settings.as_mut() {
                    if settings.is_armed() {
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
            // Re-reads `shep.toml`, so another process's write shows up, and
            // the cursor survives. An armed candidate eats this key too.
            KeyPress::Refresh => {
                if let Some(settings) = self.settings.as_mut()
                    && settings.is_armed()
                {
                    settings.pending = None;
                    return Effect::None;
                }
                return Effect::LoadSettings;
            }
            // The probe, not the open, same reasoning as `on_key`'s `e`:
            // the pane shows the dog's real schema and section or nothing.
            // An armed candidate eats it first, like every other key here.
            KeyPress::Edit => {
                if let Some(settings) = self.settings.as_mut()
                    && settings.is_armed()
                {
                    settings.pending = None;
                    return Effect::None;
                }
                return self.probe_dog_schema();
            }
            // Unreachable from here, named so a new variant cannot fall
            // silently into an arm that ignores it.
            KeyPress::Action(_)
            | KeyPress::FilterStart
            | KeyPress::TextChar(_)
            | KeyPress::TextBackspace
            | KeyPress::TextApply
            | KeyPress::TextAbandon => {}
        }
        Effect::None
    }

    /// `e` on the settings screen: probe the dog under the cursor for its
    /// schema.
    ///
    /// Nothing at all on a scalar row -- the six of them are the settings
    /// screen's own subject, and `space` and `Enter` already edit them.
    /// Silent rather than refused, the same rule `confirm_field` states: a
    /// refusal about a row that was never going to act trains an operator
    /// to ignore the status bar.
    ///
    /// The dogs this can reach are exactly the rows the screen is already
    /// showing -- `BUILT_IN_DOGS` plus every `adopted_dogs` key -- so no
    /// listing request is needed to tell a name the daemon knows from one
    /// it does not. An adopted-but-disabled dog is in that list on purpose:
    /// configure-then-enable is the ordinary order.
    fn probe_dog_schema(&mut self) -> Effect {
        let Some(settings) = self.settings.as_ref() else {
            return Effect::None;
        };
        let Some(SettingsRow::Dog(index)) = settings.cursor() else {
            return Effect::None;
        };
        let Some(dog) = settings.snapshot().dogs.get(index) else {
            return Effect::None;
        };
        Effect::LoadDogPane {
            name: dog.name.clone(),
            adopted_path: dog.adopted_path.clone(),
        }
    }

    /// `e`'s own handler: refuse a dog here, else ask the shepherd.
    ///
    /// The dog check is local because the answer is better local. The
    /// daemon refuses this request for a dog with a sentence that already
    /// names it, and this reducer prefixes a refusal with the name it asked
    /// about, so the round trip produced `log-rotate: log-rotate is a dog,
    /// and ...`. It also refuses for the wrong reason for this door: the
    /// daemon is talking about where a dog's config comes from, and what an
    /// operator pressing `e` needs to know is which screen edits a dog.
    ///
    /// It is a screen, not a refusal: this sentence said the config pane is
    /// for a sheep, which stopped being true when the dog pane shipped, and
    /// left an operator with no next keystroke. `s` then `e` on the dog's
    /// own row is the path, and it is the one the settings screen's own
    /// `Edit` arm takes.
    ///
    /// [`Self::selected_row`] rather than [`Self::selected_name`] for the
    /// check, and both are right: a dog runs one process, so it is never a
    /// group row, and a group row is exactly the case `selected_row`
    /// answers `None` for and `e` still has to work on.
    fn ask_for_config(&mut self) -> Effect {
        if let Some(row) = self.selected_row()
            && row.info.dog.is_some()
        {
            let name = row.info.name.clone();
            self.notice = Some(Notice {
                text: format!("{name} is a dog; press `s` for settings, then `e` on its row"),
                grave: true,
            });
            return Effect::None;
        }
        match self.selected_name() {
            Some(name) => {
                self.config_target = Some(name.clone());
                Effect::Send(Sent::SheepConfig { name })
            }
            None => Effect::None,
        }
    }

    /// Puts the keyboard back to [`InputMode::Normal`] when no pane editor
    /// owns it any more.
    ///
    /// [`InputMode::Text`] is remembered on `App` while the buffer it
    /// belongs to lives on the pane, so anything that drops or replaces the
    /// pane can leave the two disagreeing -- and a lookout in `Text` mode
    /// with nothing to type into eats every keystroke until `Esc`. A
    /// re-read rebuilds the whole `ConfigPane`, which is exactly that, and
    /// a landed write asks for one.
    ///
    /// Called only from paths where the config pane is the screen in
    /// question. The filter box owns `Text` with no marker but the mode
    /// itself, and it cannot be open while the pane is.
    fn release_text_mode_if_unowned(&mut self) {
        if self.mode != InputMode::Text {
            return;
        }
        let owned = self.config_pane.as_ref().is_some_and(|pane| {
            pane.env().map_or_else(
                || matches!(pane.pending_edit(), Some(PanePending::Typing { .. })),
                |env| env.typing().is_some(),
            )
        });
        if !owned {
            self.mode = InputMode::Normal;
        }
    }

    /// The config pane's own keymap, in force for as long as
    /// [`Self::config_pane`] is `Some`.
    ///
    /// The movement keys walk the field list, `r` re-reads the same config,
    /// `space` arms a cycle, `Enter` opens an editor or sends what is
    /// armed, and `e` or `Escape` closes. Everything else is ignored, named
    /// rather than wildcarded so a future variant does not fall silently
    /// into an arm that ignores it -- an action key in particular must
    /// never be mistaken for one this pane answers.
    ///
    /// The env sub-screen owns the keyboard ahead of all of it while it is
    /// open ([`Self::on_env_key`]), the same nesting the pane itself has
    /// over the dashboard.
    ///
    /// An armed edit eats the FIRST movement key, `Escape`, `e` and `r`
    /// rather than also doing their ordinary job -- the same
    /// cancel-before-act rule the dashboard and the settings screen already
    /// follow, and for the same reason: otherwise the operator sees the
    /// question vanish and the cursor move on one keypress, and the next
    /// reflexive `Enter` applies an edit to a row they have lost track of.
    fn on_pane_key(&mut self, key: KeyPress) -> Effect {
        self.notice = None;
        if self
            .config_pane
            .as_ref()
            .is_some_and(|pane| pane.env().is_some())
        {
            return self.on_env_key(key);
        }
        if key == KeyPress::Quit {
            return Effect::Quit;
        }
        // Checked before the dispatch below and CONSUMED, save for the two
        // keys the question is actually waiting for: `Enter` answers it,
        // and `space` walks it one step further along its own cycle -- the
        // same carve-out `on_settings_key` makes, and for the same reason.
        // Without it a bool could be flipped once and a choice could never
        // reach its third value without a cancel in between.
        if self.config_pane.as_ref().is_some_and(ConfigPane::is_armed)
            && !matches!(key, KeyPress::Confirm | KeyPress::Cycle)
        {
            if let Some(pane) = self.config_pane.as_mut() {
                pane.cancel();
            }
            return Effect::None;
        }
        match key {
            KeyPress::Quit => return Effect::Quit,
            // Both close. `Escape` closes rather than cascading to a filter
            // clear or a quit, exactly as it does on the settings screen.
            KeyPress::Edit | KeyPress::Escape => self.close_pane(),
            KeyPress::SelectUp
            | KeyPress::SelectDown
            | KeyPress::SelectFirst
            | KeyPress::SelectLast => {
                if let Some(pane) = self.config_pane.as_mut() {
                    match key {
                        KeyPress::SelectUp => pane.move_by(-1),
                        KeyPress::SelectDown => pane.move_by(1),
                        KeyPress::SelectFirst => pane.move_to_first(),
                        KeyPress::SelectLast => pane.move_to_last(),
                        _ => unreachable!(),
                    }
                }
            }
            // Re-reads the same sheep, so an override applied from another
            // window shows up. The cursor survives it: see
            // `Self::on_sheep_config`.
            KeyPress::Refresh => return self.reread_pane(),
            KeyPress::Cycle => return self.cycle_field(),
            KeyPress::Confirm => return self.confirm_field(),
            KeyPress::Action(_)
            | KeyPress::Settings
            | KeyPress::FilterStart
            | KeyPress::TextChar(_)
            | KeyPress::TextBackspace
            | KeyPress::TextApply
            | KeyPress::TextAbandon => {}
        }
        Effect::None
    }

    /// `r` from inside a pane: ask for the same target again, by whichever
    /// of the two doors it came in.
    ///
    /// A dog's SCHEMA is not re-probed. It came from the dog's binary at
    /// open and is parked on [`Self::dog_target`]; re-probing would respawn
    /// somebody else's binary on a keystroke whose job is to re-read a file.
    fn reread_pane(&mut self) -> Effect {
        let Some(pane) = self.config_pane.as_ref() else {
            return Effect::None;
        };
        let name = pane.target().name().to_owned();
        match pane.target() {
            PaneTarget::Sheep { .. } => Effect::Send(Sent::SheepConfig { name }),
            PaneTarget::Dog { .. } => Effect::Send(Sent::DogSection { name }),
        }
    }

    /// Drops the open pane and everything a reply for it would re-open.
    ///
    /// Both targets are cleared, always, and not only the one the pane
    /// happens to hold: a read for the OTHER kind can be in flight when this
    /// runs (`e` on a dog, then the settings screen closes and `e` opens a
    /// sheep), and a stale one left set is exactly the re-open behind the
    /// operator's back `config_target` exists to prevent.
    fn close_pane(&mut self) {
        self.config_pane = None;
        self.config_target = None;
        self.dog_target = None;
        self.release_text_mode_if_unowned();
    }

    /// The refusal a locked row gets, in that row's own words.
    ///
    /// Two sentences for two facts, and never one for both: shep refusing a
    /// config write is not the same as this pane having no widget for a
    /// shape a Flockfile writes perfectly well. A generic third sentence
    /// would put the pane back where it was before [`Lock`] existed.
    ///
    /// A refused row names the verb that DOES move it (decision 5), which
    /// only the two Structural fields have. The daemon's own refusal for
    /// the same write already named `shep stock`; this pane refuses
    /// locally, so its weaker sentence was the only one an operator saw.
    /// The wildcard keeps that sentence for a Structural field this binary
    /// has not been taught a remedy for, which is the honest answer rather
    /// than a guessed verb.
    fn lock_refusal(key: &str, lock: Lock) -> String {
        match lock {
            Lock::Refused => match key {
                "instances" => {
                    format!("{key} is not a config write; `shep stock` moves an instance count")
                }
                "name" => {
                    format!("{key} is not a config write; a name change is a different sheep")
                }
                _ => {
                    format!("{key} is not something a config write changes, from here or anywhere")
                }
            },
            Lock::NoWidget => {
                format!("{key} has no editor in this pane; a Flockfile still sets it")
            }
        }
    }

    /// `space` on the config pane. Arms the next value for the row under
    /// the cursor, or refuses and says why.
    ///
    /// The gate is [`Self::authorize_write`], the same one every settings
    /// write passes and for the same reason: a keystroke that changes a
    /// running flock's config needs the fat-finger catch a keystroke that
    /// stops a sheep has.
    fn cycle_field(&mut self) -> Effect {
        // The lock is checked AHEAD of the control gate, the same order
        // `confirm_field` takes. The two disagreed once, on the same row:
        // `space` on `instances` without `--allow-control` said read-only
        // while `Enter` said the field is not something a config write
        // changes. The lock wins because it is the more specific fact --
        // `--allow-control` would not help -- and a screen that answers one
        // question two ways teaches an operator to believe neither.
        if let Some((key, lock)) = self
            .config_pane
            .as_ref()
            .and_then(ConfigPane::cursor_lock)
            .map(|(key, lock)| (key.to_owned(), lock))
        {
            self.notice = Some(Notice {
                text: Self::lock_refusal(&key, lock),
                grave: true,
            });
            return Effect::None;
        }
        if self.authorize_write().is_none() {
            return Effect::None;
        }
        let now = self.now;
        if let Some(pane) = self.config_pane.as_mut() {
            pane.cycle(now);
        }
        Effect::None
    }

    /// Sends whatever the pane has armed, whichever of its two screens
    /// armed it.
    ///
    /// One function for all three, because the arm is one field
    /// ([`ConfigPane::pending_edit`]) and only its destination differs. A
    /// sheep's field edit leaves as a `Request::SetSheepField` and its env
    /// edit as a `Request::SetSheepEnv`, both recording an operator
    /// override for one key. A DOG leaves by a third door, before either:
    /// `Request::SetDogConfig` carries the whole `dogs.toml` section rather
    /// than a key, so the target is read first and the [`PaneEdit`] variant
    /// only decides the other two.
    fn send_armed(&mut self) -> Effect {
        let Some(authority) = self.authorize_write() else {
            return Effect::None;
        };
        // Read before the borrow below, and spent only once the pane really
        // had something armed: a ticket burned on a no-op would still be
        // unique, but the counter is easier to reason about when every
        // value on it names a request that went out.
        let ticket = self.next_write_ticket;
        let Some(pane) = self.config_pane.as_mut() else {
            return Effect::None;
        };
        let Some(edit) = pane.take_armed(ticket) else {
            return Effect::None;
        };
        self.next_write_ticket += 1;
        // A dog leaves by its own door and carries its whole section, not
        // one key: `Request::SetDogConfig` replaces the table, and
        // `edited_section` is what applies the edit through `toml_edit` so
        // the operator's comments survive it. `None` there is a section that
        // stopped parsing between the read and the keystroke, which is
        // reported rather than sent as an empty table.
        if let PaneTarget::Dog { name, .. } = pane.target().clone() {
            match pane.edited_section(&edit) {
                Some(toml) => {
                    return Effect::Send(Sent::SetDogSection {
                        name,
                        ticket,
                        toml: toml.into(),
                        authority,
                    });
                }
                // Settled here rather than left in flight: nothing went out,
                // so a bar saying "sent, waiting for the shepherd" would be
                // waiting on a request nobody has.
                None => {
                    pane.settle(ticket);
                    self.notice = Some(Notice {
                        text: format!("{name}: its section in dogs.toml does not parse"),
                        grave: true,
                    });
                    return Effect::None;
                }
            }
        }
        let PaneTarget::Sheep { name } = pane.target().clone() else {
            return Effect::None;
        };
        match edit {
            PaneEdit::SetEnv { key, value } => Effect::Send(Sent::SetEnv {
                name,
                ticket,
                key,
                value,
                authority,
            }),
            // No client-side validation of the value, deliberately. The
            // daemon deserializes it into the field it names and
            // re-normalizes the result -- peer input is untrusted, so it
            // must -- and a second, weaker copy of that check here would
            // refuse some shapes the daemon accepts and would drift the
            // moment `AppConfig` grows a field. An empty buffer on a field
            // that is not nullable arms `null` and comes back as a named
            // `InvalidConfig` refusal on the status bar.
            PaneEdit::Set { key, value } => Effect::Send(Sent::ApplyField {
                name,
                ticket,
                key,
                value,
                authority,
            }),
        }
    }

    /// The operator's `Enter` on the config pane. Three meanings, picked in
    /// this order, mirroring [`Self::confirm_setting`]:
    ///
    /// - Something is armed: sends it and marks it sent.
    /// - The cursor is on `env`: opens the env sub-screen.
    /// - The cursor is on a typed field: opens the editor and switches
    ///   [`InputMode::Text`] on.
    ///
    /// All three go through [`Self::authorize_write`], the editor included,
    /// for the reason [`Self::confirm_setting`]'s own doc gives: an editor
    /// a read-only operator can type a whole path into before being told no
    /// is a worse refusal than one that arrives on the keypress that asked.
    /// The env sub-screen is gated too -- it exists only to write, since
    /// the shepherd never sends a value back for it to show.
    fn confirm_field(&mut self) -> Effect {
        let Some(pane) = self.config_pane.as_ref() else {
            return Effect::None;
        };
        if pane.is_armed() {
            return self.send_armed();
        }
        let Some(kind) = pane.cursor_kind().cloned() else {
            return Effect::None;
        };
        let locked = pane.cursor_lock().map(|(key, lock)| (key.to_owned(), lock));
        let opens = matches!(kind, FieldKind::Map | FieldKind::Text | FieldKind::Integer);
        // A row `Enter` was never going to open raises nothing at all --
        // `confirm_setting`'s own third case, for its reason: a refusal
        // about a key that was never going to act trains an operator to
        // ignore the status bar. A bool and a choice are `space`'s job, and
        // `space` works.
        if !opens && locked.is_none() {
            return Effect::None;
        }
        // The lock is checked AHEAD of the control gate, and it is the more
        // specific of the two answers: `--allow-control` would not help.
        // Each lock says its own thing -- see [`Self::lock_refusal`].
        if let Some((key, lock)) = locked {
            self.notice = Some(Notice {
                text: Self::lock_refusal(&key, lock),
                grave: true,
            });
            return Effect::None;
        }
        if self.authorize_write().is_none() {
            return Effect::None;
        }
        let Some(pane) = self.config_pane.as_mut() else {
            return Effect::None;
        };
        if kind == FieldKind::Map {
            pane.open_env();
        } else {
            pane.begin_typing();
            self.mode = InputMode::Text;
        }
        Effect::None
    }

    /// The env sub-screen's own keymap, in force for as long as the pane
    /// holds one.
    ///
    /// `Escape` closes the sub-screen and NOT the pane: the cascade backs
    /// out of the innermost thing first, the same rule
    /// [`KeyPress::Escape`]'s own doc states. `e` closes the whole pane,
    /// because that is the key that opened it.
    ///
    /// `Enter` opens the editor on the row under the cursor, and sends
    /// whatever is armed -- the same two-keystroke shape every other write
    /// in lookout has.
    ///
    /// It did not, briefly, on the argument that an env change parks until
    /// the next spawn, so nothing irreversible happens on the keypress.
    /// That argument is about when the change takes EFFECT and says nothing
    /// about when the old value is LOST: `handle_set_sheep_env` writes the
    /// override store on the same call, so the previous value is gone the
    /// moment the reply lands, reload or no reload. This screen is
    /// write-only, so nothing here can read it back to put it there again,
    /// which makes an overwrite exactly as total a loss as a removal. Both
    /// arm.
    ///
    /// An armed edit eats the first stray key, the cancel-before-act rule
    /// the field list already follows, with `Enter` exempt because it is
    /// the key the question is waiting for.
    fn on_env_key(&mut self, key: KeyPress) -> Effect {
        if key == KeyPress::Quit {
            return Effect::Quit;
        }
        if self.config_pane.as_ref().is_some_and(ConfigPane::is_armed) && key != KeyPress::Confirm {
            if let Some(pane) = self.config_pane.as_mut() {
                pane.cancel();
            }
            return Effect::None;
        }
        match key {
            KeyPress::Quit => return Effect::Quit,
            KeyPress::Escape => {
                if let Some(pane) = self.config_pane.as_mut() {
                    pane.close_env();
                }
                self.release_text_mode_if_unowned();
            }
            KeyPress::Edit => self.close_pane(),
            KeyPress::SelectUp
            | KeyPress::SelectDown
            | KeyPress::SelectFirst
            | KeyPress::SelectLast => {
                if let Some(env) = self.config_pane.as_mut().and_then(ConfigPane::env_mut) {
                    match key {
                        KeyPress::SelectUp => env.move_by(-1),
                        KeyPress::SelectDown => env.move_by(1),
                        KeyPress::SelectFirst => env.move_to_first(),
                        KeyPress::SelectLast => env.move_to_last(),
                        _ => unreachable!(),
                    }
                }
            }
            KeyPress::Refresh => return self.reread_pane(),
            KeyPress::Confirm => {
                if self.config_pane.as_ref().is_some_and(ConfigPane::is_armed) {
                    return self.send_armed();
                }
                if self.authorize_write().is_none() {
                    return Effect::None;
                }
                if let Some(env) = self.config_pane.as_mut().and_then(ConfigPane::env_mut) {
                    env.begin_typing();
                    self.mode = InputMode::Text;
                }
            }
            KeyPress::Action(_)
            | KeyPress::Cycle
            | KeyPress::Settings
            | KeyPress::FilterStart
            | KeyPress::TextChar(_)
            | KeyPress::TextBackspace
            | KeyPress::TextApply
            | KeyPress::TextAbandon => {}
        }
        Effect::None
    }

    /// The config pane's own text keymap, in force for as long as one of
    /// its two editors owns [`InputMode::Text`].
    ///
    /// Does not trim the buffer, for the reason
    /// [`Self::on_settings_text_key`]'s own doc gives: this repository does
    /// not widen an accepted input grammar without a basis in the spec.
    ///
    /// Both editors ARM on `TextApply`: the operator's next `Enter`, on the
    /// now-closed editor, is what sends it. This doc said an env edit
    /// SENDS, which it did until [`Self::on_env_key`] made that screen stop
    /// being the one exception -- see its own doc for the argument.
    fn on_pane_text_key(&mut self, key: KeyPress) -> Effect {
        if key == KeyPress::Quit {
            return Effect::Quit;
        }
        if self
            .config_pane
            .as_ref()
            .is_some_and(|pane| pane.env().is_some())
        {
            return self.on_env_text_key(key);
        }
        let now = self.now;
        let Some(pane) = self.config_pane.as_mut() else {
            return Effect::None;
        };
        match key {
            KeyPress::TextChar(typed) => pane.type_char(typed),
            KeyPress::TextBackspace => pane.type_backspace(),
            KeyPress::TextApply => {
                pane.apply_typing(now);
                // `apply_typing` keeps the editor open on an integer buffer
                // that does not parse, so the mode follows what the pane
                // actually did rather than what the key asked for.
                if !matches!(pane.pending_edit(), Some(PanePending::Typing { .. })) {
                    self.mode = InputMode::Normal;
                }
            }
            KeyPress::TextAbandon => {
                pane.abandon_typing();
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
        Effect::None
    }

    /// The env sub-screen's own text keymap.
    ///
    /// `TextApply` ARMS, exactly as the field editor's does. See
    /// [`Self::on_env_key`] for why this screen stopped being the one
    /// exception to that.
    fn on_env_text_key(&mut self, key: KeyPress) -> Effect {
        let now = self.now;
        let Some(pane) = self.config_pane.as_mut() else {
            return Effect::None;
        };
        let Some(env) = pane.env_mut() else {
            return Effect::None;
        };
        match key {
            KeyPress::TextChar(typed) => env.type_char(typed),
            KeyPress::TextBackspace => env.type_backspace(),
            KeyPress::TextApply => {
                let applied = env.apply_typing();
                self.mode = InputMode::Normal;
                if let Some((key, value)) = applied {
                    pane.arm_env(key, value.map(EnvValue::from), now);
                }
            }
            KeyPress::TextAbandon => {
                env.abandon_typing();
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
        Effect::None
    }

    /// The [`WriteAuthority`] every settings write path has to hold, or
    /// [`None`] with the refusal already raised.
    ///
    /// The one place [`Control`] is read on this screen: `space` on a scalar or
    /// a dog, and `Enter` opening or applying an edit, all come through here.
    fn authorize_write(&mut self) -> Option<WriteAuthority> {
        let authority = WriteAuthority::granted(self);
        if authority.is_none() {
            self.notice = Some(Notice {
                text: READ_ONLY_REFUSAL.to_string(),
                grave: true,
            });
        }
        authority
    }

    /// `space` on the settings screen: arms a candidate for the cursor's row,
    /// or refuses through [`Self::authorize_write`].
    fn cycle_setting(&mut self) -> Effect {
        if self.authorize_write().is_none() {
            return Effect::None;
        }
        let Some(cursor) = self.settings.as_ref().and_then(Settings::cursor) else {
            return Effect::None;
        };
        match cursor {
            SettingsRow::Scalar(field) => self.cycle_scalar(field),
            SettingsRow::Dog(index) => self.cycle_dog(index),
        }
    }

    /// `space` on one of the six scalar rows. Re-arms when a candidate is
    /// already armed, so a second `space` walks one step further along the
    /// cycle. Does nothing on the two free-text fields.
    fn cycle_scalar(&mut self, field: SettingField) -> Effect {
        let Some(settings) = self.settings.as_mut() else {
            return Effect::None;
        };
        let Some(value) = settings.next_candidate(field) else {
            return Effect::None;
        };
        let source = settings.source_of(field);
        let text = confirm_text(field, &value, source);
        settings.pending = Some(Pending::Armed {
            edit: SettingEdit::Set { field, value },
            text,
            at: self.now,
        });
        Effect::None
    }

    /// `space` on a [`SettingsRow::Dog`] row: arms the opposite of the file's
    /// `enabled` bit, refusing in [`LINK_GONE`]'s words while the link is gone.
    ///
    /// The link check is this row's own, unlike [`Self::cycle_scalar`]: a
    /// confirmed toggle ends in a request to the shepherd.
    fn cycle_dog(&mut self, index: usize) -> Effect {
        if matches!(self.link, Link::Lost { .. }) {
            self.notice = Some(Notice {
                text: LINK_GONE.to_string(),
                grave: true,
            });
            return Effect::None;
        }
        let Some(settings) = self.settings.as_mut() else {
            return Effect::None;
        };
        let Some(dog) = settings.snapshot.dogs.get(index) else {
            return Effect::None;
        };
        let name = dog.name.clone();
        let enable = !dog.enabled;
        let text = if enable {
            format!("enable {name}? it starts now, no reload")
        } else {
            format!("disable {name}? it stops now and is deregistered")
        };
        settings.pending = Some(Pending::DogArmed {
            edit: DogEdit { name, enable },
            text,
            at: self.now,
        });
        Effect::None
    }

    /// The operator's `Enter` on the settings screen. On a free-text row with
    /// nothing pending it opens [`Pending::Typing`] and switches
    /// [`InputMode::Text`] on; on an armed candidate it sends and moves to
    /// [`Pending::Sent`]; anything else is untouched.
    ///
    /// Both acting cases go through [`Self::authorize_write`]. Opening the
    /// editor is gated as well as applying it, so the refusal arrives before a
    /// whole socket path is typed.
    fn confirm_setting(&mut self) -> Effect {
        let Some(settings) = self.settings.as_ref() else {
            return Effect::None;
        };
        let opens_editor = settings.pending.is_none()
            && matches!(
                settings.cursor(),
                Some(SettingsRow::Scalar(
                    SettingField::Socket | SettingField::MaxCronSleep
                ))
            );
        if !opens_editor && !settings.is_armed() {
            return Effect::None;
        }
        let Some(authority) = self.authorize_write() else {
            return Effect::None;
        };
        let Some(settings) = self.settings.as_mut() else {
            return Effect::None;
        };
        if opens_editor {
            let Some(SettingsRow::Scalar(field)) = settings.cursor() else {
                return Effect::None;
            };
            let buffer = settings.text_seed(field).to_string();
            settings.pending = Some(Pending::Typing { field, buffer });
            self.mode = InputMode::Text;
            return Effect::None;
        }
        match settings.pending.take() {
            Some(Pending::Armed { edit, text, .. }) => {
                settings.pending = Some(Pending::Sent { text });
                Effect::WriteSetting(edit, authority)
            }
            Some(Pending::DogArmed { edit, text, .. }) => {
                settings.pending = Some(Pending::Sent { text });
                Effect::WriteDog(edit, authority)
            }
            other => {
                settings.pending = other;
                Effect::None
            }
        }
    }

    /// Arms a confirm, or refuses and says why.
    ///
    /// Every refusal happens here rather than at confirm time, so an operator
    /// never answers a question that was never going to be honoured. The ladder
    /// is gate, link, nothing selected, one already in flight.
    fn arm(&mut self, verb: ActionVerb) -> Effect {
        let refusal = if self.control == Control::ReadOnly {
            Some(READ_ONLY_REFUSAL.to_string())
        } else if let Link::Retrying { attempt } = self.link {
            // Not `LINK_GONE`, which says the shepherd is gone: this is the
            // status bar's own sentence for a redial (`view/status.rs`).
            Some(format!(
                "the shepherd stopped answering — reconnecting (attempt {attempt})"
            ))
        } else if matches!(self.link, Link::Lost { .. }) {
            // The ladder is exhausted, so the shepherd really is gone.
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
    /// than leaving a question about nothing. An action already in flight
    /// keeps its line.
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
    /// On a frozen dashboard it would never expire either, since `now` stops
    /// advancing and the expiry check rides it. An action already sent keeps
    /// its line: `run_connected` answers it with an `Err` before its loop ends.
    fn disarm_on_link_change(&mut self) {
        if self
            .action
            .as_ref()
            .is_some_and(|action| action.stage == Stage::Armed)
        {
            self.action = None;
        }
    }

    /// The text keymap's router: the filter box while the settings screen is
    /// closed, [`Self::on_settings_text_key`]'s editor while it is open. The
    /// two never both own [`InputMode::Text`].
    fn on_text_key(&mut self, key: KeyPress) -> Effect {
        // Three now, and the split is still total: the config pane and the
        // settings screen cannot both be open (`e` reaches the dashboard
        // only from the dashboard, and `s` only from there too), and
        // neither can coexist with the filter box, which `Msg::Settings`'s
        // own arm closed the window on.
        if self.config_pane.is_some() {
            return self.on_pane_text_key(key);
        }
        if self.settings.is_some() {
            return self.on_settings_text_key(key);
        }
        self.on_filter_text_key(key)
    }

    /// The filter box's keymap.
    ///
    /// Ctrl-C still quits: in raw mode it is a key event, not a signal. Does
    /// not clear [`Self::notice`], unlike normal mode, since a notice can be
    /// raised with no keypress involved; the status bar hides it under the box
    /// and shows it again when the box closes.
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

    /// The settings editor's own text keymap, in force while a
    /// [`Pending::Typing`] owns [`InputMode::Text`]. The buffer is never
    /// trimmed.
    ///
    /// `TextApply` arms rather than writes: an empty buffer becomes
    /// [`SettingEdit::Unset`], anything else [`SettingEdit::Set`], and the next
    /// `Enter` sends it. `TextAbandon` leaves the screen open.
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
    /// flock, or whatever the filter leaves of it.
    ///
    /// A [`RowKey::Group`] header comes immediately before its own
    /// [`RowKey::Sheep`] entries, on [`Self::is_grouped`]'s rule. The sort key
    /// is total on purpose: the table repolls every two seconds, and a partial
    /// one would let two instances swap places under the cursor.
    ///
    /// Every cursor move reads this sequence and nothing else, so `j` and `k`
    /// cannot step onto a filtered-out row.
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

    fn visible_len(&self) -> usize {
        self.visible_rows().len()
    }

    /// Puts the selection back on a real row after the flock changed, and
    /// reports whether it moved.
    ///
    /// `previous_index` is where the selection sat before the change, read
    /// while the old map was still in place. A surviving key is left alone; a
    /// lost one falls to whatever now occupies that position, clamped to the
    /// last row rather than to row 0.
    fn reseat(&mut self, previous_index: Option<usize>) -> bool {
        // `selected_index`, not `flock.contains_key`: a selection the filter
        // hides is not seated, however present its id is. Must come before the
        // emptiness check below, which would otherwise return early for a query
        // that matches no sheep.
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
    /// Clamped rather than wrapping.
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
    /// files and asks the shepherd for lambs, and a held `k` at the top of the
    /// flock must not do that once per keypress.
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
        // A frozen dashboard re-reading live log files would put content on
        // screen newer than the banner over it. The cursor still moves, and the
        // detail pane re-renders from the frozen listing.
        if matches!(self.link, Link::Lost { .. }) {
            return Effect::None;
        }
        Effect::RefreshSelected
    }

    /// Every sheep the table's rows are drawn from, in name-then-id order: the
    /// whole flock, or whatever the filter leaves of it.
    ///
    /// A flat sheep list, not [`Self::visible_rows`]'s [`RowKey`] sequence: the
    /// title bar counts this, and a group header is not a sheep.
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

    /// Every sheep the shepherd last reported, in id order, whatever the filter
    /// hides.
    ///
    /// The host strip sums this rather than [`Self::rows`], so a name filter
    /// cannot narrow what `flock cpu`/`flock mem` add up to while the label
    /// still says `flock`.
    #[must_use]
    pub fn all_rows(&self) -> Vec<&Row> {
        self.flock.values().collect()
    }

    /// Replaces the filter and puts the selection back on a visible sheep: a
    /// keystroke that narrows the query can hide the selected one.
    fn set_filter(&mut self, query: String) -> Effect {
        if self.filter == query {
            return Effect::None;
        }
        let previous = self.selected_index();
        self.filter = query;
        if self.reseat(previous) && !matches!(self.link, Link::Lost { .. }) {
            // The cursor moved, so the feed and the lambs are about to describe
            // a different sheep.
            return Effect::RefreshSelected;
        }
        Effect::None
    }

    /// The filter as typed, empty when there is none.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Which keymap is currently in force.
    #[must_use]
    pub fn mode(&self) -> InputMode {
        self.mode
    }

    /// How many sheep the shepherd last reported, whatever the filter hides.
    #[must_use]
    pub fn flock_len(&self) -> usize {
        self.flock.len()
    }

    /// The selected row, or `None` for an empty flock.
    #[must_use]
    pub fn selected(&self) -> Option<RowKey> {
        self.selected.clone()
    }

    /// Which row of [`Self::visible_rows`] the selection sits on, derived every
    /// call rather than stored.
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let key = self.selected.clone()?;
        self.visible_rows().iter().position(|row| *row == key)
    }

    /// The selected sheep's row, which the detail pane and the feed read.
    /// `None` for a [`RowKey::Group`] selection as well as for none at all: a
    /// group has no single sheep to describe.
    #[must_use]
    pub fn selected_row(&self) -> Option<&Row> {
        match &self.selected {
            Some(RowKey::Sheep(id)) => self.flock.get(id),
            _ => None,
        }
    }

    /// The selected row's app name: a sheep's own, or a group row's.
    ///
    /// Unlike [`Self::selected_row`] this answers for a group too: a config
    /// pane is about the stored spec every instance of an app shares, which
    /// a group row names exactly, while the detail pane and feed describe
    /// one process and have nothing to show for a group.
    #[must_use]
    pub fn selected_name(&self) -> Option<String> {
        match &self.selected {
            Some(RowKey::Group(name)) => Some(name.clone()),
            Some(RowKey::Sheep(id)) => self.flock.get(id).map(|row| row.info.name.clone()),
            None => None,
        }
    }

    /// One sheep by id, whatever the filter hides: the lookup a
    /// [`RowKey::Sheep`] row's rendering needs.
    #[must_use]
    pub fn row(&self, id: u32) -> Option<&Row> {
        self.flock.get(&id)
    }

    /// Every instance of `name`, sorted by slot: the members a
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

    /// Whether `name`'s instances draw under a [`RowKey::Group`] header: more
    /// than one instance of the name, every one of them reporting a slot.
    ///
    /// Read over the whole flock rather than the filtered sequence, which a
    /// name query keeps whole either way.
    #[must_use]
    pub fn is_grouped(&self, name: &str) -> bool {
        let members = self.group_members(name);
        members.len() > 1 && members.iter().all(|row| row.info.instance.is_some())
    }

    /// `name`'s rolled-up numbers. [`GroupTotals`] gives the rule each field
    /// follows.
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

    /// `name`'s STATUS cell: the shared status word when every instance agrees,
    /// else a count per state, as `output::rows::group_status` does for
    /// `shep flock`.
    ///
    /// Reads `ProcStatus` directly, never [`Row::reported`]: a dog is never
    /// stocked to several instances, so a group has no handshake to report.
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

    /// `name`'s status when every instance agrees on one, which the STATUS
    /// colouring and the detail pane's status word key off. A mixed group's
    /// plain count text wears no colour.
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
    /// rather than because no heartbeat has fired yet. The strip says a
    /// different sentence for each: an operator shown the wrong one waits for
    /// numbers that are never coming.
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
    /// A running sheep's uptime advances between polls, from the anchor its row
    /// carries. One that is not running does not: its `uptime_ms` is a fact
    /// about how long it ran. Nothing advances once the link is [`Link::Lost`].
    #[must_use]
    pub fn uptime_ms(&self, id: u32) -> Option<u64> {
        let row = self.flock.get(&id)?;
        if !matches!(row.info.status, ProcStatus::Online | ProcStatus::Starting) {
            return Some(row.info.uptime_ms);
        }
        let elapsed = self.now.saturating_duration_since(row.anchor);
        Some(row.info.uptime_ms.saturating_add(millis(elapsed)))
    }

    /// The lamb reading for sheep `id`, with its age in milliseconds.
    ///
    /// `None` when there is no reading, or when the one there is was taken for
    /// a different sheep. The age stops when the dashboard freezes.
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

    /// Tells every scrollable screen how tall the body is. Called by the
    /// event loop before each draw, so a screen's cursor never lands on a
    /// row that was not rendered.
    pub fn note_body_rows(&mut self, rows: u16) {
        if let Some(settings) = self.settings.as_mut() {
            settings.set_rows(usize::from(rows));
        }
        if let Some(pane) = self.config_pane.as_mut() {
            // One less than the settings screen gets: the pane spends its
            // first line on a title naming the sheep, which
            // `view::pane::pane_lines` draws before any row.
            let body = usize::from(rows.saturating_sub(1));
            pane.set_rows(body);
            if let Some(env) = pane.env_mut() {
                // The sub-screen draws its own title over the pane's, so
                // it pays the same one line and no more.
                env.set_rows(body);
            }
        }
    }

    /// The open config pane, or `None` while nothing is being edited.
    #[must_use]
    pub fn config_pane(&self) -> Option<&ConfigPane> {
        self.config_pane.as_ref()
    }

    /// The resolved style level and which layer chose it, which the STYLE LEVEL
    /// row reads rather than re-resolving.
    #[must_use]
    pub fn style(&self) -> (StyleLevel, StyleSource) {
        self.style
    }

    /// Sets the resolved style level and its source. `App` reads no files, so
    /// it cannot resolve this itself.
    pub(crate) fn set_style(&mut self, style: (StyleLevel, StyleSource)) {
        self.style = style;
    }

    /// Overrides the control gate a fixture built with. Every shipped fixture
    /// hard-codes [`Control::ReadOnly`].
    #[cfg(test)]
    pub(crate) fn set_control_for_tests(&mut self, control: Control) {
        self.control = control;
    }

    /// Sets the filter directly, bypassing [`Self::set_filter`]'s reseat.
    #[cfg(test)]
    pub(crate) fn set_filter_for_tests(&mut self, query: &str) {
        self.filter = query.to_string();
    }

    /// Points the cursor at `key` without simulating keypresses.
    #[cfg(test)]
    fn select(&mut self, key: RowKey) {
        self.selected = Some(key);
    }
}

/// The prefix every action's notice shares: the verb, and the target. A single
/// sheep takes the `(id N)` form; a group names the app, having no one id.
fn target_prefix(verb: ActionVerb, target: &RowKey, name: &str) -> String {
    match target {
        RowKey::Sheep(id) => format!("{} {name} (id {id})", verb.label()),
        RowKey::Group(_) => format!("{} all instances of {name}", verb.label()),
    }
}

/// Saturating `Duration` to milliseconds. Saturates for clippy's
/// `cast_possible_truncation`, not for a lookout left open 580 million years.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What the bar says once the shepherd has answered. `Response::Reloading` is
/// an acceptance, not a result: the swaps arrive later on the bus.
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

    /// `started()`'s three sheep with the gate open and the cursor mid-list, on
    /// `web` at id 1.
    ///
    /// The table reads by name, so the ids disagree with the display order:
    /// `api` 2, `web` 1, `worker` 3. Mid-list, because a cursor clamped at
    /// either end would pass the tests that assert a stray `j` did not move it.
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

    /// `allowed()`'s shape with three instances of one app: `web` at slots 0, 1
    /// and 2, ids 1 through 3. Nothing is selected; each test selects itself.
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
    fn instanced_rows() -> Vec<ProcessInfo> {
        (0..3)
            .map(|slot| {
                ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                    .instance(Some(slot))
                    .build()
            })
            .collect()
    }

    /// The status bar's own rendered text.
    fn status_line_text(app: &App) -> String {
        super::super::view::fixtures::rendered(&super::super::view::status::status_line(app, 200))
    }

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

    #[test]
    fn a_group_confirm_states_how_many_processes_it_reaches() {
        let mut app = allowed_with_instances();
        app.select(RowKey::Group("web".to_string()));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        let prompt = status_line_text(&app);
        assert!(prompt.contains('3'), "names the blast radius: {prompt}");
    }

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

    /// `allowed()` parks the selection mid-list, so a `j` genuinely could move
    /// it.
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

    /// The snapshot renames the armed sheep out of the filter while another
    /// enters it, so id 2 stays in `self.flock` while leaving `visible_rows()`
    /// and the cursor moves to id 9. Deleting a neighbour would not separate
    /// the two: `reseat` leaves a surviving id alone.
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
        assert!(app.action().is_none());
        assert_eq!(app.update(Msg::Key(KeyPress::Confirm)), Effect::None);
    }

    /// Driven by `Msg::Tick`, so there is no sleep here.
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

    #[test]
    fn quit_still_quits_while_a_confirm_is_armed() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert_eq!(app.update(Msg::Key(KeyPress::Quit)), Effect::Quit);
    }

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

        // Sheep 1 goes away. `worker` is now row 1 rather than row 2, where
        // an index cursor would be pointing at `api`.
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

        // The last row dying clamps rather than leaving the cursor past the end.
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected(), Some(RowKey::Sheep(3)));
        app.update(Msg::Snapshot {
            rows: vec![sheep(2, "api", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.selected(), Some(RowKey::Sheep(2)));

        app.update(Msg::Snapshot {
            rows: vec![],
            at: t0,
        });
        assert_eq!(app.selected(), None);
        assert_eq!(app.selected_index(), None);
    }

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

    #[test]
    fn moving_the_selection_asks_for_lambs() {
        let (mut app, _t0) = started();
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::RefreshSelected
        );
    }

    /// `ListFlock` declines the lamb walk: a full machine enumeration every two
    /// seconds, times every open lookout.
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

    #[test]
    fn nothing_is_requested_while_the_link_is_lost() {
        let (mut app, _t0) = started();
        app.update(Msg::Frozen {
            at_local: "2026-08-16 09:00:00".to_string(),
        });
        assert_eq!(app.update(Msg::Key(KeyPress::SelectDown)), Effect::None);
    }

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

    /// The cursor still moves: re-rendering the detail pane from the frozen
    /// listing is data already on the frame. Touching the disk is not.
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

    #[test]
    fn a_running_sheeps_uptime_advances_with_the_heartbeat() {
        let (mut app, t0) = started();
        assert_eq!(app.uptime_ms(app.rows()[0].info.id), Some(60_000));
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(5),
        });
        assert_eq!(app.uptime_ms(1), Some(65_000));
    }

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

    /// A `DaemonShutdown` is a notice here, where in `bleats` it precedes a
    /// clean exit.
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

    /// `Effect::None`, not `RefreshFeed`: clearing a filter only widens the
    /// visible set, so the selection stays seated and `reseat` is a no-op.
    #[test]
    fn esc_clears_the_filter_instead_of_quitting_while_one_is_set() {
        let mut app = filtered("web");
        assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::None);
        assert_eq!(app.filter(), "");
        assert_eq!(app.rows().len(), 4);
    }

    #[test]
    fn esc_still_quits_with_no_filter_set() {
        let (mut app, _t0) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::Quit);
    }

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

    #[test]
    fn opening_the_filter_takes_a_notice_off_the_bar() {
        let (mut app, _t0) = started();
        app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
        assert!(app.notice().is_some());
        app.update(Msg::Key(KeyPress::FilterStart));
        assert!(app.notice().is_none(), "the box is what the bar shows now");
    }

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

        // A late snapshot must not unfreeze it.
        app.update(Msg::Snapshot {
            rows: vec![],
            at: t0,
        });
        assert!(matches!(app.link(), Link::Lost { .. }));
    }

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

    #[test]
    fn the_next_keypress_clears_the_notice() {
        let (mut app, _) = started();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert!(app.notice().is_some());
        app.update(Msg::Key(KeyPress::SelectDown));
        assert!(app.notice().is_none());
    }

    /// The strip reads this machine, which lookout can still see after the
    /// shepherd dies, so it is the one pane that could keep ticking under a
    /// banner saying the values are frozen.
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

    /// `run_ui`'s coalesced read is armed before the freeze, so a read can
    /// still be in flight when `Msg::Frozen` lands.
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

    /// A dashboard whose filter is set without any keymap involved.
    ///
    /// Four sheep, two of which contain `web`: `api-web` at id 1 and
    /// `web-worker` at id 4, with `cron` and `queue` between them. The table
    /// sorts by name, so the gap is what makes `j` stepping over a hidden row
    /// falsifiable.
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

    /// The fixture separates the two answers: by id it is `web` 0, `api` 1,
    /// `web` 2; by name then id it is `api` 1, `web` 0, `web` 2. The `(name,
    /// id)` tiebreak itself is not falsifiable here, since the rows arrive in
    /// id order; what this catches is the sort going missing entirely.
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

    #[test]
    fn a_filter_narrows_the_rows_and_leaves_the_real_size_readable() {
        let app = filtered("web");
        assert_eq!(app.rows().len(), 2, "api-web and web-worker");
        assert_eq!(app.flock_len(), 4, "the flock did not get smaller");
    }

    /// `ProcessSelector`'s `Name` compares with `==`, so borrowing the CLI's
    /// selector grammar would match nothing while `web-worker` is being typed.
    #[test]
    fn the_filter_matches_a_substring_and_not_a_whole_name() {
        assert_eq!(filtered("wor").rows().len(), 1, "web-worker, by its middle");
        assert_eq!(filtered("w").rows().len(), 2, "api-web, by its own middle");
    }

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

    #[test]
    fn nothing_visible_means_nothing_selected() {
        let app = filtered("zzz");
        assert_eq!(app.rows().len(), 0);
        assert_eq!(app.selected(), None);
        assert!(app.selected_row().is_none());
        assert_eq!(app.flock_len(), 4, "the flock is still four sheep");
    }

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

    #[test]
    fn an_empty_query_is_the_same_as_no_filter() {
        let mut app = filtered("zzz");
        app.set_filter(String::new());
        assert_eq!(app.rows().len(), 4);
        assert_eq!(app.selected(), Some(RowKey::Sheep(1)), "seated again");
    }

    /// `None` means the reply did not walk, `Some(vec![])` means it walked and
    /// found nothing.
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

    #[test]
    fn an_unrecognised_lamb_reply_is_a_failure_and_not_an_empty_walk() {
        let (mut app, _t0) = started();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Pong),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Failed, _))));
    }

    /// The fetch is armed before the freeze can land, so a reply can still be
    /// in flight when `Msg::Frozen` arrives.
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

    /// `Response::Reloading` is an acceptance; the swaps arrive afterwards on
    /// the bus, which the table consumes.
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

    /// `RequestError`'s full `Display` would put a Rust identifier on screen.
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

    /// The second case is the sharper one: the right shape for the wrong verb.
    /// A `Stopped` answering a `Restart` carries rows and would upsert happily.
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

    /// From the dashboard with no filter `Esc` quits; from here it must not.
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
        // and it stops rather than wrapping
        let _ = app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(
            app.settings().unwrap().cursor(),
            Some(*rows.last().unwrap())
        );
    }

    #[test]
    fn an_action_key_from_the_dashboard_is_unreachable_while_the_screen_is_up() {
        let mut app = fixtures::app_in_settings_with_control();
        // `x` is the stop key on the dashboard. In here it is not an action.
        let _ = app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert!(app.action().is_none(), "no sheep confirm can arm from here");
    }

    /// Every key sequence that ends in a write, not one key: a gate guarding
    /// `space` alone leaves the free-text editor's route reaching
    /// `WriteSetting` on a read-only lookout.
    ///
    /// The refusal is checked per keypress rather than at the end, because
    /// `on_settings_key` clears `self.notice` on every key.
    #[test]
    fn no_key_route_writes_the_config_while_the_gate_is_closed() {
        use KeyPress::{Confirm, Cycle, SelectDown, TextApply, TextChar};

        // `Settings::rows` puts the six scalars first, then the fixture's two
        // dogs: two `SelectDown`s reach `socket`, six the first dog row.
        let routes: &[(&str, &[KeyPress])] = &[
            ("space on a cycled scalar", &[Cycle, Confirm]),
            (
                "the socket editor",
                &[
                    SelectDown,
                    SelectDown,
                    Confirm,
                    TextChar('/'),
                    TextChar('x'),
                    TextApply,
                    Confirm,
                ],
            ),
            (
                "the max_cron_sleep editor",
                &[
                    SelectDown,
                    SelectDown,
                    SelectDown,
                    Confirm,
                    TextChar('9'),
                    TextChar('s'),
                    TextApply,
                    Confirm,
                ],
            ),
            (
                "the whistle gate",
                &[
                    SelectDown, SelectDown, SelectDown, SelectDown, Cycle, Confirm,
                ],
            ),
            (
                "space on a dog row",
                &[
                    SelectDown, SelectDown, SelectDown, SelectDown, SelectDown, SelectDown, Cycle,
                    Confirm,
                ],
            ),
        ];

        for (what, keys) in routes {
            let mut app = fixtures::app_in_settings(); // Control::ReadOnly
            let mut refused = false;
            for key in *keys {
                let effect = app.update(Msg::Key(*key));
                assert!(
                    !matches!(effect, Effect::WriteSetting(..) | Effect::WriteDog(..)),
                    "{what}: a read-only lookout reached {effect:?}"
                );
                refused |= app.notice().is_some_and(Notice::is_grave);
            }
            assert!(refused, "{what}: the refusal has to say why");
            assert!(
                app.settings().unwrap().pending().is_none(),
                "{what}: nothing is left armed"
            );
            assert!(
                app.settings().unwrap().typing().is_none(),
                "{what}: no editor is left open"
            );
        }
    }

    /// The half that keeps the closed-gate test honest: a gate that refused
    /// everything would pass it and be useless.
    #[test]
    fn every_one_of_those_routes_writes_once_the_gate_is_open() {
        use KeyPress::{Confirm, Cycle, SelectDown, TextApply, TextChar};

        let routes: &[(&str, &[KeyPress])] = &[
            ("space on a cycled scalar", &[Cycle, Confirm]),
            (
                "the socket editor",
                &[
                    SelectDown,
                    SelectDown,
                    Confirm,
                    TextChar('/'),
                    TextChar('x'),
                    TextApply,
                    Confirm,
                ],
            ),
            (
                "space on a dog row",
                &[
                    SelectDown, SelectDown, SelectDown, SelectDown, SelectDown, SelectDown, Cycle,
                    Confirm,
                ],
            ),
        ];

        for (what, keys) in routes {
            let mut app = fixtures::app_in_settings_with_control();
            let mut wrote = false;
            for key in *keys {
                let effect = app.update(Msg::Key(*key));
                wrote |= matches!(effect, Effect::WriteSetting(..) | Effect::WriteDog(..));
            }
            assert!(wrote, "{what}: an open gate has to reach the write");
        }
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

    /// Six log levels and one cycle key: without re-arming, the fourth needs a
    /// cancel in between.
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

    /// `style::resolve` is flag over env over config, so with `$SHEP_STYLE` or
    /// `--style` in play the write lands and nothing changes.
    #[test]
    fn a_shadowed_style_confirm_names_the_layer_that_keeps_winning() {
        for (source, layer) in [
            (StyleSource::Env, "$SHEP_STYLE"),
            (StyleSource::Flag, "--style"),
        ] {
            let mut app = fixtures::app_in_settings_with_shadowed_style(source);
            let _ = app.update(Msg::Key(KeyPress::Cycle));
            let text = app.settings().unwrap().pending().unwrap().text.to_string();
            assert!(text.contains(layer), "{source} must name itself: {text}");
            assert!(
                text.contains("keeps winning"),
                "{source} must say what it does: {text}"
            );
            assert!(
                !text.contains("the next command reads it"),
                "{source}: the next command reads {source}, not the file: {text}"
            );
        }
    }

    /// With `$SHEP_STYLE=bare` over a file saying `full`, cycling the resolved
    /// value would propose `full`: a no-op write, reported as a change.
    #[test]
    fn the_style_cycle_starts_from_the_file_and_not_the_level_in_force() {
        let mut app = fixtures::app_in_settings_with_shadowed_style(StyleSource::Env);
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(
            text.contains("set style level to plain"),
            "the file says full, so one step is plain: {text}"
        );
    }

    #[test]
    fn enter_sends_the_armed_edit() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let effect = app.update(Msg::Key(KeyPress::Confirm));
        assert!(matches!(
            effect,
            Effect::WriteSetting(
                SettingEdit::Set {
                    field: SettingField::LogLevel,
                    ..
                },
                _
            )
        ));
        assert!(app.settings().unwrap().pending().unwrap().sent);
    }

    #[test]
    fn a_written_edit_updates_the_row_and_its_source() {
        let mut app = fixtures::app_in_settings_with_control();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::WriteSetting(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
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

        // The re-read, which `run_ui` drives through `load_settings`.
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

    #[test]
    fn an_unset_write_returns_the_row_to_the_default() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for _ in 0..8 {
            let _ = app.update(Msg::Key(KeyPress::TextBackspace));
        }
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let Effect::WriteSetting(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
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

    /// Pins `Msg::Settings`'s `opening` check.
    #[test]
    fn the_cursor_survives_a_landed_writes_reload() {
        let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
        let before = app.settings().unwrap().cursor();
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let Effect::WriteSetting(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
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
        let Effect::WriteSetting(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
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
    /// clears: a settings edit is local file I/O.
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

    /// Off the raw tick rather than `self.now`, which stops advancing once the
    /// link is lost.
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

    /// `s` raises `Effect::LoadSettings` while `self.settings` is still `None`,
    /// so `x` reaches `arm()`. Once the read lands, `on_settings_key` no-ops
    /// `Confirm`, so nothing could resolve the armed action.
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

    /// `on_key` checks the text mode ahead of its settings branch, so a box
    /// left open would eat every key the settings keymap owns. The query itself
    /// is kept.
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

    /// Against a real file whose `[style] level` names a third, different
    /// level: the row reports the value threaded onto `App`, not one re-derived
    /// from the file.
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

    /// A refusal is discovered under the lock, so it lands after the confirm,
    /// and the typed text has to survive it.
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
        let Effect::WriteSetting(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
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

    #[test]
    fn refresh_cancels_an_armed_candidate_rather_than_silently_dropping_it() {
        let mut app = fixtures::app_in_settings_with_control(); // cursor on log_level
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        assert!(
            app.settings().unwrap().pending().is_some(),
            "space must arm before this test means anything"
        );
        let effect = app.update(Msg::Key(KeyPress::Refresh));
        assert_eq!(
            effect,
            Effect::None,
            "a cancel must not also raise a reload"
        );
        assert!(
            app.settings().unwrap().pending().is_none(),
            "the armed candidate must not survive `r`"
        );
    }

    #[test]
    fn arming_a_dog_names_the_live_apply_and_not_a_reload() {
        let mut app = fixtures::app_in_settings_on_dog("metrics");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(text.contains("it starts now, no reload"), "got: {text}");
    }

    #[test]
    fn disabling_says_it_deregisters() {
        let mut app = fixtures::app_in_settings_on_enabled_dog("otel");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let text = app.settings().unwrap().pending().unwrap().text.to_string();
        assert!(text.contains("deregistered"), "got: {text}");
    }

    /// One message still yields one effect.
    #[test]
    fn a_written_dog_toggle_raises_the_daemon_half() {
        let mut app = fixtures::app_in_settings_on_dog("metrics");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::WriteDog(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send the file half first");
        };
        let effect = app.update(Msg::DogWritten {
            edit,
            result: Ok(DogSource::BuiltIn),
        });
        assert!(matches!(
            effect,
            Effect::Send(Sent::Dog { enable: true, .. })
        ));
    }

    #[test]
    fn a_refused_file_half_never_reaches_the_shepherd() {
        let mut app = fixtures::app_in_settings_on_dog("metrics");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::WriteDog(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send the file half first");
        };
        let effect = app.update(Msg::DogWritten {
            edit,
            result: Err("permission denied".into()),
        });
        assert_eq!(
            effect,
            Effect::None,
            "a failed write must not ask the shepherd"
        );
        assert!(app.notice().unwrap().is_grave());
    }

    /// The scalars never leave the machine; a dog's second half does.
    #[test]
    fn a_dog_toggle_refuses_while_the_link_is_gone() {
        let mut app = fixtures::app_in_settings_on_dog("metrics");
        let _ = app.update(Msg::Frozen {
            at_local: "12:00:00".into(),
        });
        let effect = app.update(Msg::Key(KeyPress::Cycle));
        assert_eq!(effect, Effect::None);
        assert!(app.settings().unwrap().pending().is_none(), "nothing arms");
        assert!(app.notice().unwrap().is_grave());
    }

    #[test]
    fn a_scalar_still_edits_while_the_link_is_gone() {
        let mut app = fixtures::app_in_settings_with_control(); // on log_level
        let _ = app.update(Msg::Frozen {
            at_local: "12:00:00".into(),
        });
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        assert!(
            app.settings().unwrap().pending().is_some(),
            "a scalar is local file I/O and needs no shepherd"
        );
    }

    /// Drives a dog toggle to `Effect::Send`: arm, confirm the file half, then
    /// land `Msg::DogWritten` so the daemon half goes out.
    fn armed_and_sent_dog(name: &str) -> (App, Sent) {
        let mut app = fixtures::app_in_settings_on_dog(name);
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::WriteDog(edit, _) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter must send the file half first");
        };
        let Effect::Send(sent) = app.update(Msg::DogWritten {
            edit,
            result: Ok(DogSource::BuiltIn),
        }) else {
            panic!("a landed write must raise the daemon half");
        };
        (app, sent)
    }

    /// `EnableDog` answers `Response::DogStarted`. The sentence names what the
    /// shepherd did rather than a bare "done".
    #[test]
    fn a_landed_enable_names_what_the_shepherd_did() {
        let (mut app, sent) = armed_and_sent_dog("metrics");
        assert!(
            app.settings().unwrap().pending().is_some(),
            "the sent line stays up until the reply lands"
        );
        let info = ProcessInfo::builder(50, "metrics", ProcStatus::Online)
            .pid(Some(50_000))
            .dog(Some(DogSource::BuiltIn))
            .build();
        app.update(Msg::Replied {
            sent,
            result: Ok(Response::DogStarted(info)),
        });
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("enable metrics: the shepherd started it")
        );
        assert!(!app.notice().unwrap().is_grave());
        assert!(
            app.settings().unwrap().pending().is_none(),
            "the sent line clears once the reply lands"
        );
    }

    /// `DisableDog` answers `Response::Deleted`, the reply `Delete` gives. The
    /// sentence names the deregistration, since the confirm is gone by now.
    #[test]
    fn a_landed_disable_names_the_deregistration() {
        let (mut app, sent) = armed_and_sent_dog("otel");
        app.update(Msg::Replied {
            sent,
            result: Ok(Response::Deleted(vec![50])),
        });
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("disable otel: the shepherd stopped and deregistered it")
        );
        assert!(!app.notice().unwrap().is_grave());
    }

    /// Whether the settings screen's own dogs list still says `name` is
    /// enabled. Reads the snapshot, not the file: the two can disagree.
    #[track_caller]
    fn dog_enabled_in_view(app: &App, name: &str) -> bool {
        app.settings()
            .expect("the settings screen is open")
            .snapshot()
            .dogs
            .iter()
            .find(|dog| dog.name == name)
            .expect("the fixture carries this dog")
            .enabled
    }

    /// The file half lands first, so `DogView.enabled` is stale by the time
    /// this reply arrives while RUNNING keeps updating off the poll. Without
    /// the re-read a landed `enable metrics` reads `metrics | no | online`.
    #[test]
    fn a_landed_toggle_re_reads_the_file_in_both_directions() {
        for (name, enable) in [("metrics", true), ("otel", false)] {
            let (mut app, sent) = armed_and_sent_dog(name);
            assert_eq!(
                dog_enabled_in_view(&app, name),
                !enable,
                "{name}: the fixture starts on the other bit"
            );
            let reply = if enable {
                Response::DogStarted(
                    ProcessInfo::builder(50, name, ProcStatus::Online)
                        .pid(Some(50_000))
                        .dog(Some(DogSource::BuiltIn))
                        .build(),
                )
            } else {
                Response::Deleted(vec![50])
            };

            let effect = app.update(Msg::Replied {
                sent,
                result: Ok(reply),
            });

            assert_eq!(
                effect,
                Effect::LoadSettings,
                "{name}: a landed toggle has to re-read the file it changed"
            );
            assert_eq!(
                dog_enabled_in_view(&app, name),
                !enable,
                "{name}: nothing is folded into the row by hand -- the re-read is the repair"
            );

            // The re-read landing, with the bit the write put in the file.
            let mut fresh = app.settings().unwrap().snapshot().clone();
            for dog in &mut fresh.dogs {
                if dog.name == name {
                    dog.enabled = enable;
                }
            }
            app.update(Msg::Settings { result: Ok(fresh) });

            assert_eq!(
                dog_enabled_in_view(&app, name),
                enable,
                "{name}: and the row agrees with the file once it lands"
            );
        }
    }

    /// `metrics` is armed as an `enable`, so `Response::Deleted` is the right
    /// shape for the wrong verb and `Response::Pong` is a reply this binary has
    /// never heard of.
    #[test]
    fn an_unrecognised_dog_reply_says_so_rather_than_reading_as_success() {
        for reply in [Response::Pong, Response::Deleted(vec![1])] {
            let (mut app, sent) = armed_and_sent_dog("metrics");
            app.update(Msg::Replied {
                sent,
                result: Ok(reply),
            });
            assert_eq!(
                app.notice().map(ToString::to_string).as_deref(),
                Some(
                    "enable metrics: the shepherd answered something this lookout does not understand"
                )
            );
            assert!(app.notice().unwrap().is_grave());
        }
    }

    #[test]
    fn a_dog_reply_that_failed_to_send_says_so_under_the_same_prefix() {
        let (mut app, sent) = armed_and_sent_dog("metrics");
        app.update(Msg::Replied {
            sent,
            result: Err(RequestError::Closed),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert!(said.starts_with("enable metrics: "), "got {said:?}");
        assert!(said.contains(&RequestError::Closed.to_string()));
    }

    /// fails if `e` stops asking the shepherd for the selected sheep's
    /// config, or if the reply stops opening a pane over the dashboard.
    ///
    /// `e` raises the READ, not the open: the pane shows the shepherd's own
    /// answer or it shows nothing, the same rule `s` and `Msg::Settings`
    /// already follow for the settings screen.
    #[test]
    fn e_asks_for_the_selected_sheeps_config_and_the_reply_opens_the_pane() {
        let mut app =
            fixtures::with_selection(ProcessInfo::builder(9, "web", ProcStatus::Online).build());
        let effect = app.update(Msg::Key(KeyPress::Edit));
        assert_eq!(
            effect,
            Effect::Send(Sent::SheepConfig {
                name: "web".to_string()
            })
        );
        assert!(app.config_pane().is_none(), "nothing opens on the keypress");

        app.update(Msg::Replied {
            sent: Sent::SheepConfig {
                name: "web".to_string(),
            },
            result: Ok(Response::SheepConfig(Box::new(
                fixtures::sheep_config_view(),
            ))),
        });
        let pane = app.config_pane().expect("the reply opens the pane");
        assert_eq!(pane.target().name(), "web");
        assert_eq!(pane.fields().len(), 39);
    }

    /// fails if `e` on an empty flock starts sending a request for a sheep
    /// that is not there.
    #[test]
    fn e_with_nothing_selected_asks_for_nothing() {
        let mut app = fixtures::app_with(Vec::new(), fixtures::plain());
        assert_eq!(app.update(Msg::Key(KeyPress::Edit)), Effect::None);
        assert!(app.config_pane().is_none());
    }

    /// fails if `e` stops reaching a group row. A group has no single sheep,
    /// which is why `selected_row` answers `None` for one -- but every
    /// instance behind it shares one stored spec, and the group row is what
    /// a multi-instance app shows by default.
    #[test]
    fn e_on_a_group_row_asks_by_the_apps_name() {
        let mut app = fixtures::app_with(
            vec![
                ProcessInfo::builder(1, "web", ProcStatus::Online)
                    .instance(Some(0))
                    .build(),
                ProcessInfo::builder(2, "web", ProcStatus::Online)
                    .instance(Some(1))
                    .build(),
            ],
            fixtures::plain(),
        );
        app.update(Msg::Key(KeyPress::SelectFirst));
        assert!(
            app.selected_row().is_none(),
            "the selection is the group row, not one instance"
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::Edit)),
            Effect::Send(Sent::SheepConfig {
                name: "web".to_string()
            })
        );
    }

    /// fails if either key stops closing the pane, or if closing starts
    /// cascading into the dashboard's own `Escape` (which quits, or clears
    /// a filter).
    #[test]
    fn e_and_escape_both_close_the_pane_and_neither_quits() {
        for key in [KeyPress::Edit, KeyPress::Escape] {
            let mut app = fixtures::app_in_sheep_pane();
            assert_eq!(app.update(Msg::Key(key)), Effect::None);
            assert!(app.config_pane().is_none(), "{key:?}");
        }
    }

    /// fails if the pane stops owning the keyboard while it is open. `x` in
    /// particular: an action key must never arm from in here.
    #[test]
    fn the_pane_owns_the_keyboard_while_it_is_open() {
        let mut app = fixtures::app_in_sheep_pane();
        assert_eq!(
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop))),
            Effect::None
        );
        assert!(
            app.action().is_none(),
            "no action arms from inside the pane"
        );
        assert_eq!(app.update(Msg::Key(KeyPress::Settings)), Effect::None);
        assert!(
            app.settings().is_none(),
            "`s` does not open a second screen"
        );
        assert!(app.config_pane().is_some());
    }

    /// fails if the movement keys stop walking the field list.
    #[test]
    fn the_movement_keys_walk_the_fields() {
        let mut app = fixtures::app_in_sheep_pane();
        app.update(Msg::Key(KeyPress::SelectDown));
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(app.config_pane().unwrap().view().cursor(), 2);
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.config_pane().unwrap().view().cursor(), 38);
        app.update(Msg::Key(KeyPress::SelectFirst));
        assert_eq!(app.config_pane().unwrap().view().cursor(), 0);
    }

    /// fails if `r` from inside the pane stops re-reading, or if the reply
    /// throws the operator back to the first field.
    #[test]
    fn r_re_reads_the_same_sheep_and_the_cursor_survives_it() {
        let mut app = fixtures::app_in_sheep_pane();
        app.update(Msg::Key(KeyPress::SelectLast));
        let sent = Sent::SheepConfig {
            name: "web".to_string(),
        };
        assert_eq!(
            app.update(Msg::Key(KeyPress::Refresh)),
            Effect::Send(sent.clone())
        );
        app.update(Msg::Replied {
            sent,
            result: Ok(Response::SheepConfig(Box::new(
                fixtures::sheep_config_view(),
            ))),
        });
        assert_eq!(app.config_pane().unwrap().view().cursor(), 38);
    }

    /// fails if a refusal is swallowed, or if a failed refresh blanks a pane
    /// that was showing something real.
    #[test]
    fn a_refused_config_read_says_why_and_leaves_the_pane_alone() {
        let mut app = fixtures::app_in_sheep_pane();
        app.update(Msg::Replied {
            sent: Sent::SheepConfig {
                name: "web".to_string(),
            },
            result: Err(RequestError::Rpc(RpcError {
                code: RpcErrorCode::NotFound,
                message: "no sheep named web".to_string(),
                daemon_version: None,
            })),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert!(said.contains("no sheep named web"), "got {said:?}");
        assert!(app.notice().unwrap().is_grave());
        assert!(app.config_pane().is_some(), "the pane stays as it was");
    }

    /// fails if a config read the channel dropped goes unreported. Silence
    /// looks exactly like a key that is not bound.
    #[test]
    fn a_config_read_that_was_never_sent_says_so() {
        let mut app =
            fixtures::with_selection(ProcessInfo::builder(9, "web", ProcStatus::Online).build());
        app.update(Msg::Unsent {
            sent: Sent::SheepConfig {
                name: "web".to_string(),
            },
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert!(said.contains("web"), "got {said:?}");
        assert!(app.notice().unwrap().is_grave());
    }

    /// fails if the pane stops getting one row fewer than the settings
    /// screen. It spends its first line on a title naming the sheep, so a
    /// viewport told the full body height would scroll one row late.
    #[test]
    fn the_pane_gets_the_body_height_minus_its_own_title() {
        let mut app = fixtures::app_in_sheep_pane();
        app.note_body_rows(20);
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.config_pane().unwrap().view().offset(), 39 - 19);
    }

    /// Walks the pane's cursor onto `key`. The pane is a public type with
    /// no public "go to this field" key, so the cursor is driven the way
    /// an operator drives it.
    fn pane_to(app: &mut App, key: &str) {
        let index = app
            .config_pane()
            .expect("the pane is open")
            .fields()
            .fields()
            .iter()
            .position(|field| field.key == key)
            .unwrap_or_else(|| panic!("no field named {key}"));
        let _ = app.update(Msg::Key(KeyPress::SelectFirst));
        for _ in 0..index {
            let _ = app.update(Msg::Key(KeyPress::SelectDown));
        }
    }

    /// Lands a `Request::SheepConfig` reply for `web` carrying `env_keys`,
    /// the way the event loop lands one after a write or an `r`.
    fn refresh_config(app: &mut App, env_keys: &[&str]) {
        let mut config = shep_core::config::AppConfig {
            name: "web".to_string(),
            ..Default::default()
        };
        for key in env_keys {
            config.env.insert((*key).to_string(), "x".to_string());
        }
        app.update(Msg::Replied {
            sent: Sent::SheepConfig {
                name: "web".to_string(),
            },
            result: Ok(Response::SheepConfig(Box::new(
                shep_core::protocol::SheepConfigView::new(config, Vec::new(), Vec::new()),
            ))),
        });
    }

    /// The request an effect would put on the wire, or a panic naming what
    /// came back instead. The seam this test module cares about: the
    /// reducer's own `Sent` is an echo tag, and `Sent::request` is what the
    /// link task actually sends.
    fn wire(effect: Effect) -> Request {
        match effect {
            Effect::Send(sent) => sent.request(),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    /// fails if a single-field write stops going out as a
    /// `Request::SetSheepField`, or starts naming a key other than the one
    /// the operator edited.
    ///
    /// **The route is the whole point.** The same edit went out as a
    /// one-app `Request::ApplyConfig` at `ResetDepth::File` until
    /// 2026-09-05. That moved exactly this field and nothing else, and then
    /// the merge spent the operator's override for it, because a merge is a
    /// template load and a key put back to the template is not a key an
    /// operator is holding a value for. The edit therefore landed and
    /// vanished from `overridden` on the same round trip, so the `*` this
    /// pane draws never appeared and `shep flock`'s CFG column counted
    /// nothing. `rpc.rs`'s
    /// `a_field_edit_is_reported_as_an_operator_override` is the other half
    /// of this claim, asserted where the marker is actually built.
    ///
    /// One field from each of the groups the pane draws, because the
    /// classification the daemon routes on is per field.
    #[test]
    fn one_edit_reaches_the_wire_as_a_single_field_override() {
        for (key, presses) in [
            ("autorestart", 1usize), // control
            ("watch", 1),            // process
            ("merge_logs", 1),       // inputs
            ("autorestart", 2),      // twice: a second cycle re-arms
        ] {
            let mut app = fixtures::app_in_sheep_pane_with_control();
            pane_to(&mut app, key);
            for _ in 0..presses {
                let _ = app.update(Msg::Key(KeyPress::Cycle));
            }
            assert!(
                app.config_pane().unwrap().is_armed(),
                "{key}: space must arm before Enter means anything"
            );
            let request = wire(app.update(Msg::Key(KeyPress::Confirm)));
            let Request::SetSheepField {
                name,
                key: sent,
                value,
            } = request
            else {
                panic!("{key}: expected SetSheepField, got {request:?}");
            };
            assert_eq!(name, "web", "{key}");
            assert_eq!(sent, key, "{key}");
            assert!(value.is_boolean(), "{key}: {value}");
        }
    }

    /// fails if a typed field stops reaching the wire as the value that was
    /// typed, in the JSON shape that field's own kind wants. `cwd` and
    /// `max_restarts` between them cover text and integer, which arm
    /// differently, and an integer sent as a string is refused by the
    /// daemon rather than set.
    #[test]
    fn a_typed_field_reaches_the_wire_as_the_value_that_was_typed() {
        for (key, typed, want) in [
            ("cwd", "/srv/web", serde_json::json!("/srv/web")),
            ("max_restarts", "40", serde_json::json!(40)),
        ] {
            let mut app = fixtures::app_in_sheep_pane_with_control();
            pane_to(&mut app, key);
            let _ = app.update(Msg::Key(KeyPress::Confirm));
            assert_eq!(app.mode(), InputMode::Text, "{key}: the editor opens");
            for _ in 0..40 {
                let _ = app.update(Msg::Key(KeyPress::TextBackspace));
            }
            for typed in typed.chars() {
                let _ = app.update(Msg::Key(KeyPress::TextChar(typed)));
            }
            let _ = app.update(Msg::Key(KeyPress::TextApply));
            assert_eq!(app.mode(), InputMode::Normal, "{key}: the editor closes");
            let request = wire(app.update(Msg::Key(KeyPress::Confirm)));
            let Request::SetSheepField {
                key: sent, value, ..
            } = request
            else {
                panic!("{key}: expected SetSheepField, got {request:?}");
            };
            assert_eq!(sent, key, "{key}");
            assert_eq!(value, want, "{key}");
        }
    }

    /// fails if the env sub-screen stops sending one key at a time, or
    /// starts routing env through `ApplyConfig`, where no `ResetDepth`
    /// expresses one key.
    ///
    /// Also fails if either write stops ARMING first. The daemon writes the
    /// override store on the same call, so a set destroys the old value as
    /// surely as a removal does, and this screen cannot read either back.
    #[test]
    fn the_env_sub_screen_arms_then_sets_one_key_and_removes_another() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "env");
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        assert!(
            app.config_pane().unwrap().env().is_some(),
            "enter on the env row opens the sub-screen"
        );
        // The `+ new` row, under the two keys the fixture's sheep has.
        let _ = app.update(Msg::Key(KeyPress::SelectLast));
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        assert_eq!(app.mode(), InputMode::Text);
        for typed in "API_TOKEN=hunter2".chars() {
            let _ = app.update(Msg::Key(KeyPress::TextChar(typed)));
        }
        assert_eq!(
            app.update(Msg::Key(KeyPress::TextApply)),
            Effect::None,
            "applying the editor arms; it does not send"
        );
        assert_eq!(app.mode(), InputMode::Normal);
        let armed = app.config_pane().unwrap().pending_edit();
        let Some(PanePending::Armed { text, .. }) = armed else {
            panic!("{armed:?}");
        };
        assert!(text.contains("set env API_TOKEN"), "{text}");
        assert!(
            !text.contains("hunter2"),
            "the sentence must not quote the value: {text}"
        );
        let request = wire(app.update(Msg::Key(KeyPress::Confirm)));
        assert_eq!(
            request,
            Request::SetSheepEnv {
                name: "web".to_owned(),
                key: "API_TOKEN".to_owned(),
                value: Some("hunter2".to_owned().into()),
            }
        );

        // An existing key with an empty buffer removes it, and the question
        // names the key and says the value is not recoverable from here.
        let _ = app.update(Msg::Key(KeyPress::SelectFirst));
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let armed = app.config_pane().unwrap().pending_edit();
        let Some(PanePending::Armed { text, .. }) = armed else {
            panic!("{armed:?}");
        };
        assert!(text.contains("remove env DB_HOST"), "{text}");
        assert!(text.contains("cannot read the value back"), "{text}");
        let request = wire(app.update(Msg::Key(KeyPress::Confirm)));
        assert_eq!(
            request,
            Request::SetSheepEnv {
                name: "web".to_owned(),
                key: "DB_HOST".to_owned(),
                value: None,
            }
        );
    }

    /// fails if an armed env write stops being cancellable by a stray key,
    /// which is what makes it a question rather than an announcement.
    #[test]
    fn a_stray_key_cancels_an_armed_env_write() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "env");
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        assert!(
            app.config_pane().unwrap().is_armed(),
            "an empty buffer arms"
        );
        assert_eq!(app.update(Msg::Key(KeyPress::SelectDown)), Effect::None);
        assert!(!app.config_pane().unwrap().is_armed());
        assert!(
            app.config_pane().unwrap().env().is_some(),
            "the cancel must not also close the sub-screen"
        );
    }

    /// fails if the env cursor stops naming the same KEY across a refresh.
    ///
    /// A removal shortens the list, so a cursor carried by INDEX names the
    /// next key down afterwards, and a reflexive second `Enter` arms a
    /// write against a neighbour nobody chose. A key that is gone lands on
    /// `+ new`, the one row where `Enter` destroys nothing.
    #[test]
    fn the_env_cursor_is_carried_by_key_and_not_by_index_across_a_refresh() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "env");
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(
            app.config_pane().unwrap().env().unwrap().cursor_key(),
            Some("LOG_LEVEL")
        );
        // A refresh that leaves both keys in place keeps the cursor on its
        // own key rather than on row 1.
        refresh_config(&mut app, &["DB_HOST", "LOG_LEVEL"]);
        assert_eq!(
            app.config_pane().unwrap().env().unwrap().cursor_key(),
            Some("LOG_LEVEL")
        );
        // A refresh that removed the key ABOVE it keeps it on its own key,
        // which is now row 0. Carrying the index would have moved it to
        // `+ new`; carrying nothing would have moved it to `DB_HOST`.
        refresh_config(&mut app, &["LOG_LEVEL"]);
        assert_eq!(
            app.config_pane().unwrap().env().unwrap().cursor_key(),
            Some("LOG_LEVEL")
        );
        // A refresh that removed the cursor's OWN key lands on `+ new`,
        // never on whatever took its place.
        refresh_config(&mut app, &["OTHER"]);
        assert_eq!(app.config_pane().unwrap().env().unwrap().cursor_key(), None);
    }

    /// fails if a reply landing while an editor is open throws the buffer
    /// away, or strands the keyboard in `InputMode::Text`.
    ///
    /// The buffer was discarded with no notice and every later keystroke
    /// was dead until `Esc`, because `settle` cleared every variant rather
    /// than only the one that is in flight. On the env screen the
    /// discarded buffer is a secret the operator cannot read back.
    #[test]
    fn a_reply_landing_mid_edit_leaves_the_buffer_and_the_keyboard_alone() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("the confirm sends");
        };
        // The operator moves on and starts typing while it is in flight.
        pane_to(&mut app, "cwd");
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for typed in "/srv".chars() {
            let _ = app.update(Msg::Key(KeyPress::TextChar(typed)));
        }
        assert_eq!(app.mode(), InputMode::Text);
        let _ = app.update(Msg::Unsent { sent });
        assert_eq!(app.mode(), InputMode::Text, "the keyboard is not stranded");
        let pending = app.config_pane().unwrap().pending_edit();
        let Some(PanePending::Typing { key, buffer }) = pending else {
            panic!("the buffer survives the reply: {pending:?}");
        };
        assert_eq!(key, "cwd");
        assert!(buffer.ends_with("/srv"), "{buffer}");
    }

    /// fails if the pane is left in `InputMode::Text` with no editor to
    /// type into. A landed write asks for a re-read, and the re-read
    /// rebuilds the whole `ConfigPane`, editor included.
    #[test]
    fn a_refresh_that_drops_an_open_editor_puts_the_keyboard_back() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "cwd");
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        assert_eq!(app.mode(), InputMode::Text);
        refresh_config(&mut app, &["DB_HOST", "LOG_LEVEL"]);
        assert_eq!(app.mode(), InputMode::Normal);
        assert!(app.config_pane().unwrap().pending_edit().is_none());
    }

    /// fails if one write's reply clears a DIFFERENT write's in-flight
    /// line while that one is still outstanding.
    ///
    /// Only the line was ever at risk: each request names exactly its own
    /// key, so neither can carry the other's value and the config itself
    /// cannot be crossed. `PanePending::Sent` had no key to match against,
    /// so any reply settled whatever was pending.
    #[test]
    fn one_replys_arrival_does_not_clear_another_writes_in_flight_line() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::Send(first) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("the first confirm sends");
        };
        pane_to(&mut app, "watch");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::Send(second) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("the second confirm sends");
        };
        assert!(matches!(
            app.config_pane().unwrap().pending_edit(),
            Some(PanePending::Sent { key, .. }) if key == "watch"
        ));
        let _ = app.update(Msg::Replied {
            sent: first,
            result: Ok(Response::SheepFieldSet {
                name: "web".to_owned(),
                key: "autorestart".to_owned(),
                pending: false,
            }),
        });
        let pending = app.config_pane().unwrap().pending_edit();
        assert!(
            matches!(pending, Some(PanePending::Sent { key, .. }) if key == "watch"),
            "the second write is still outstanding: {pending:?}"
        );
        let _ = app.update(Msg::Replied {
            sent: second,
            result: Ok(Response::SheepFieldSet {
                name: "web".to_owned(),
                key: "watch".to_owned(),
                pending: false,
            }),
        });
        assert!(app.config_pane().unwrap().pending_edit().is_none());
    }

    /// fails if either kind of locked row starts arming an edit, or if the
    /// two stop refusing for their own reasons.
    ///
    /// `instances` is `Lock::Refused` -- shep takes no config write for it
    /// at all -- and `args` is `Lock::NoWidget`, which is only this pane
    /// having no editor for the shape. A single sentence for both is what
    /// `Lock` exists to stop.
    #[test]
    fn a_refused_field_and_one_with_no_widget_refuse_for_different_reasons() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "instances");
        assert_eq!(app.update(Msg::Key(KeyPress::Cycle)), Effect::None);
        assert!(app.config_pane().unwrap().pending_edit().is_none());
        let refused = app.notice().expect("a refusal is raised").to_string();
        assert!(refused.contains("instances"), "{refused}");
        assert!(
            !refused.contains("no editor in this pane"),
            "a field shep refuses is not a field this pane merely lacks a widget for: {refused}"
        );

        pane_to(&mut app, "args");
        assert_eq!(app.update(Msg::Key(KeyPress::Confirm)), Effect::None);
        assert!(app.config_pane().unwrap().pending_edit().is_none());
        let no_widget = app.notice().expect("a refusal is raised").to_string();
        assert!(no_widget.contains("args"), "{no_widget}");
        assert!(
            no_widget.contains("Flockfile"),
            "a shape with no widget is still one a Flockfile writes: {no_widget}"
        );
        assert_ne!(refused, no_widget, "two facts, two sentences");
    }

    /// fails if the pane refuses a Structural field without naming the
    /// verb that does move it (decision 5). The daemon's own refusal for
    /// the same field names `shep stock`; the pane refuses locally, so its
    /// weaker sentence was the only one an operator ever saw.
    #[test]
    fn a_refused_field_names_the_verb_that_owns_it() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "instances");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let said = app.notice().expect("a refusal is raised").to_string();
        assert!(said.contains("`shep stock`"), "{said}");
    }

    /// fails if `space` and `Enter` answer the same locked row two
    /// different ways.
    ///
    /// They did: `cycle_field` read the control gate first and
    /// `confirm_field` read the lock first, so on `instances` without
    /// `--allow-control` one key said read-only and the other said the
    /// field is not something a config write changes. Both are true and
    /// only one can be the answer; the lock wins, because
    /// `--allow-control` would not have helped.
    #[test]
    fn space_and_enter_refuse_a_locked_row_with_the_same_sentence() {
        for control in [Control::ReadOnly, Control::Allowed] {
            for key in ["instances", "args"] {
                let mut app = fixtures::app_in_sheep_pane();
                app.set_control_for_tests(control);
                pane_to(&mut app, key);
                let _ = app.update(Msg::Key(KeyPress::Cycle));
                let cycled = app.notice().map(ToString::to_string);
                let _ = app.update(Msg::Key(KeyPress::Confirm));
                let confirmed = app.notice().map(ToString::to_string);
                assert_eq!(cycled, confirmed, "{control:?} {key}");
                assert!(
                    cycled.as_deref().is_some_and(|text| text.contains(key)),
                    "{control:?} {key}: {cycled:?}"
                );
                assert_ne!(
                    cycled.as_deref(),
                    Some(READ_ONLY_REFUSAL),
                    "the lock is the more specific fact: {control:?} {key}"
                );
            }
        }
    }

    /// fails if a read-only lookout can arm or send a config write, or open
    /// either editor. The gate is the same fat-finger catch `x` gets: an
    /// editor a read-only operator can type a whole path into before being
    /// told no is a worse refusal than one that arrives on the keypress.
    #[test]
    fn a_read_only_pane_refuses_every_door_that_writes() {
        // One pair per door: `space` arms a cycle, `Enter` opens the text
        // editor, `Enter` on `env` opens the sub-screen.
        for (key, press) in [
            ("autorestart", KeyPress::Cycle),
            ("cwd", KeyPress::Confirm),
            ("env", KeyPress::Confirm),
        ] {
            let mut app = fixtures::app_in_sheep_pane();
            pane_to(&mut app, key);
            assert_eq!(app.update(Msg::Key(press)), Effect::None, "{key}");
            assert!(app.config_pane().unwrap().pending_edit().is_none(), "{key}");
            assert!(app.config_pane().unwrap().env().is_none(), "{key}");
            assert_eq!(app.mode(), InputMode::Normal, "{key}");
            assert_eq!(
                app.notice().map(ToString::to_string),
                Some(READ_ONLY_REFUSAL.to_string()),
                "{key}"
            );
        }
    }

    /// fails if an armed edit survives a stray key, or if that key also
    /// does its ordinary job -- the cancel-before-act rule the dashboard
    /// and the settings screen already follow.
    #[test]
    fn a_stray_key_cancels_an_armed_pane_edit_and_does_nothing_else() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        let before = app.config_pane().unwrap().view().cursor();
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        assert!(app.config_pane().unwrap().is_armed());
        assert_eq!(app.update(Msg::Key(KeyPress::SelectDown)), Effect::None);
        assert!(!app.config_pane().unwrap().is_armed());
        assert_eq!(app.config_pane().unwrap().view().cursor(), before);
        assert!(
            app.config_pane().is_some(),
            "the cancel must not also close the pane"
        );
    }

    /// fails if an armed pane edit stops expiring, which would leave a
    /// question about the shepherd on screen indefinitely.
    #[test]
    fn an_armed_pane_edit_expires_with_the_confirm_budget() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        assert!(app.config_pane().unwrap().is_armed());
        let _ = app.update(Msg::Tick {
            now: Instant::now() + CONFIRM_EXPIRY,
        });
        assert!(!app.config_pane().unwrap().is_armed());
    }

    /// fails if a landed write stops re-reading the config, if it stops
    /// saying whether the running child has the value, or if a refusal is
    /// swallowed.
    ///
    /// `pending` is the shepherd's answer and the pane reports it verbatim
    /// rather than re-deriving it from `apply_group`: the two disagree on
    /// `autostart`, and on a field whose config subset would not normalize.
    #[test]
    fn a_landed_write_re_reads_the_config_and_reports_what_the_shepherd_said() {
        for (pending, wanted) in [(false, "is set"), (true, "shep reload")] {
            let mut app = fixtures::app_in_sheep_pane_with_control();
            pane_to(&mut app, "autorestart");
            let _ = app.update(Msg::Key(KeyPress::Cycle));
            let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
                panic!("the confirm sends");
            };
            let effect = app.update(Msg::Replied {
                sent,
                result: Ok(Response::SheepFieldSet {
                    name: "web".to_owned(),
                    key: "autorestart".to_owned(),
                    pending,
                }),
            });
            assert_eq!(
                effect,
                Effect::Send(Sent::SheepConfig {
                    name: "web".to_owned()
                }),
                "a landed write re-reads what the shepherd now holds"
            );
            assert!(app.config_pane().unwrap().pending_edit().is_none());
            let notice = app.notice().expect("the outcome is reported");
            assert!(!notice.is_grave(), "{notice:?}");
            assert!(notice.to_string().contains(wanted), "{notice:?}");
        }
    }

    /// fails if the shepherd's refusal is swallowed. Every refusal this
    /// door can meet is an `Err` now rather than a field in a success
    /// reply, which is `Response::SheepFieldSet`'s own argument for
    /// carrying no `refused`: two ways to say no is one a client forgets to
    /// check.
    #[test]
    fn a_refused_write_is_reported_and_does_not_re_read() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("the confirm sends");
        };
        let effect = app.update(Msg::Replied {
            sent,
            result: Err(RequestError::Rpc(shep_core::protocol::RpcError {
                code: shep_core::protocol::RpcErrorCode::InvalidConfig,
                message: "the store is locked by another shep".to_owned(),
                daemon_version: None,
            })),
        });
        assert_eq!(effect, Effect::None, "a refusal does not re-read");
        assert!(app.config_pane().unwrap().pending_edit().is_none());
        let notice = app.notice().expect("the refusal is reported");
        assert!(notice.is_grave());
        assert!(
            notice.to_string().contains("the store is locked"),
            "{notice:?}"
        );
    }

    /// fails if the value a write carries reaches a `{:?}` (IR-41).
    ///
    /// The leak this closes was one hop outside the boundary drawn when
    /// `PaneEdit` got a hand-written redaction: the same value travelled on
    /// into `Sent::ApplyField`, which derives `Debug`, and printed
    /// `value: String("/home/ada/secret-project")` in the clear. Asserted
    /// on the whole `Effect`, which is what a diagnostic would print, and
    /// not on the value alone -- the point is that no wrapper between here
    /// and there unwraps it.
    ///
    /// `Sent::SetEnv` is here for the same reason and was never leaking:
    /// `EnvValue` has redacted itself since Task 4.
    #[test]
    fn a_write_effects_debug_names_no_value() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "cwd");
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for _ in 0..40 {
            let _ = app.update(Msg::Key(KeyPress::TextBackspace));
        }
        for typed in "/home/ada/secret-project".chars() {
            let _ = app.update(Msg::Key(KeyPress::TextChar(typed)));
        }
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let field = app.update(Msg::Key(KeyPress::Confirm));
        assert_eq!(
            format!("{field:?}"),
            "Send(ApplyField { name: \"web\", ticket: 0, key: \"cwd\", \
             value: FieldValue(<string>), authority: WriteAuthority(()) })"
        );

        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "env");
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let _ = app.update(Msg::Key(KeyPress::Confirm));
        for typed in "hunter2".chars() {
            let _ = app.update(Msg::Key(KeyPress::TextChar(typed)));
        }
        let _ = app.update(Msg::Key(KeyPress::TextApply));
        let env = app.update(Msg::Key(KeyPress::Confirm));
        assert_eq!(
            format!("{env:?}"),
            "Send(SetEnv { name: \"web\", ticket: 0, key: \"DB_HOST\", \
             value: Some(EnvValue(<7 bytes>)), authority: WriteAuthority(()) })"
        );
    }

    /// fails if two writes to the SAME field settle on the first reply.
    ///
    /// The narrower half of the two-writes hole. Matching a reply against
    /// the `Sent` variant's KEY closed it for two different fields and left
    /// it open for one field twice, which is what a ticket closes: the
    /// counter is monotonic and never reused, so no two writes can be
    /// confused whatever they name.
    #[test]
    fn two_writes_to_one_field_settle_on_their_own_replies() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::Send(first) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("the first confirm sends");
        };
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let Effect::Send(second) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("the second confirm sends");
        };
        assert_ne!(first, second, "two sends are two different tickets");

        let _ = app.update(Msg::Replied {
            sent: first,
            result: Ok(Response::SheepFieldSet {
                name: "web".to_owned(),
                key: "autorestart".to_owned(),
                pending: false,
            }),
        });
        assert!(
            matches!(
                app.config_pane().unwrap().pending_edit(),
                Some(PanePending::Sent { .. })
            ),
            "the second write to the same field is still outstanding"
        );
        let _ = app.update(Msg::Replied {
            sent: second,
            result: Ok(Response::SheepFieldSet {
                name: "web".to_owned(),
                key: "autorestart".to_owned(),
                pending: false,
            }),
        });
        assert!(app.config_pane().unwrap().pending_edit().is_none());
    }

    /// fails if a refresh silently drops a question the operator has not
    /// answered, or a write they are still waiting on.
    ///
    /// Held by reachability before this: a refresh rebuilds the whole
    /// `ConfigPane` and the arm went with it, and nothing on today's
    /// keyboard could land one while armed. That is an invariant nobody
    /// can see, and the `Sent` half IS reachable -- two writes out, the
    /// first lands, and its own re-read arrives while the second is still
    /// outstanding.
    #[test]
    fn a_refresh_keeps_an_armed_edit_and_an_outstanding_one() {
        let mut app = fixtures::app_in_sheep_pane_with_control();
        pane_to(&mut app, "autorestart");
        let _ = app.update(Msg::Key(KeyPress::Cycle));
        let armed = app.config_pane().unwrap().pending_edit().cloned();
        assert!(matches!(armed, Some(PanePending::Armed { .. })));
        refresh_config(&mut app, &[]);
        assert_eq!(
            app.config_pane().unwrap().pending_edit().cloned(),
            armed,
            "the question survives the rebuild, word for word"
        );

        let _ = app.update(Msg::Key(KeyPress::Confirm));
        let sent = app.config_pane().unwrap().pending_edit().cloned();
        assert!(matches!(sent, Some(PanePending::Sent { .. })));
        refresh_config(&mut app, &[]);
        assert_eq!(
            app.config_pane().unwrap().pending_edit().cloned(),
            sent,
            "the in-flight line survives the landed write's own re-read"
        );
    }

    /// fails if `e` on a dog row asks the shepherd for a config it will
    /// refuse, or if the refusal names the dog twice.
    ///
    /// The round trip produced `log-rotate: log-rotate is a dog, and ...`,
    /// because the shepherd's own refusal is a whole sentence that already
    /// names its subject.
    #[test]
    fn e_on_a_dog_row_refuses_locally_and_names_the_dog_once() {
        let mut app = fixtures::with_selection(
            ProcessInfo::builder(9, "log-rotate", ProcStatus::Online)
                .dog(Some(shep_core::protocol::DogSource::BuiltIn))
                .build(),
        );
        assert_eq!(
            app.update(Msg::Key(KeyPress::Edit)),
            Effect::None,
            "nothing goes to the shepherd"
        );
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert_eq!(
            said.matches("log-rotate").count(),
            1,
            "the name is said once: {said:?}"
        );
        assert!(said.contains("is a dog"), "got {said:?}");
        assert!(app.config_pane().is_none());
    }

    /// fails if `e` on a dog row withholds the path that does work. The
    /// old sentence said the config pane is for a sheep, which stopped
    /// being true when the dog pane shipped, and left the operator with a
    /// refusal and no next keystroke.
    #[test]
    fn e_on_a_dog_row_names_the_screen_that_does_edit_a_dog() {
        let mut app = fixtures::with_selection(
            ProcessInfo::builder(9, "log-rotate", ProcStatus::Online)
                .dog(Some(shep_core::protocol::DogSource::BuiltIn))
                .build(),
        );
        let _ = app.update(Msg::Key(KeyPress::Edit));
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert!(said.contains("`s`"), "{said}");
        assert!(!said.contains("the config pane is for a sheep"), "{said}");
    }

    /// fails if a refusal that names nothing reaches an operator naming
    /// nothing.
    ///
    /// `EngineStopped` is the one of this request's three refusals that
    /// carries no subject of its own: `rpc_error` renders it as `the
    /// supervisor engine has stopped`, full stop. A notice saying only that
    /// identifies neither the sheep nor the screen it came from, which is
    /// what stripping the prefix unconditionally produced.
    #[test]
    fn a_refusal_that_names_nothing_still_reaches_the_operator_named() {
        let mut app = fixtures::app_in_sheep_pane();
        app.update(Msg::Replied {
            sent: Sent::SheepConfig {
                name: "web".to_string(),
            },
            result: Err(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "the supervisor engine has stopped".to_string(),
                daemon_version: None,
            })),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert_eq!(said, "web: the supervisor engine has stopped");
        assert!(app.notice().unwrap().is_grave());
    }

    /// fails if a config reply re-opens a pane the operator has closed.
    ///
    /// Two `e` presses against a slow shepherd, then `Esc`: the second
    /// answer used to land on a dashboard the operator had gone back to and
    /// put the pane straight back up.
    #[test]
    fn a_config_reply_for_a_pane_nobody_wants_is_dropped() {
        let mut app =
            fixtures::with_selection(ProcessInfo::builder(9, "web", ProcStatus::Online).build());
        let reply = || Msg::Replied {
            sent: Sent::SheepConfig {
                name: "web".to_string(),
            },
            result: Ok(Response::SheepConfig(Box::new(
                fixtures::sheep_config_view(),
            ))),
        };
        app.update(Msg::Key(KeyPress::Edit));
        app.update(Msg::Key(KeyPress::Edit));
        app.update(reply());
        assert!(app.config_pane().is_some(), "the first answer opens it");

        app.update(Msg::Key(KeyPress::Escape));
        assert!(app.config_pane().is_none());

        app.update(reply());
        assert!(
            app.config_pane().is_none(),
            "the second answer lands on a dashboard and is dropped"
        );
        assert!(app.notice().is_none(), "silently: nothing went wrong");
    }

    /// fails if closing the pane stops making a later `e` work again. The
    /// drop above is keyed on a target, and a target left set or left stale
    /// would either re-open on a stray reply or refuse the next open.
    #[test]
    fn e_still_opens_a_pane_after_one_was_closed() {
        let mut app = fixtures::app_in_sheep_pane();
        app.update(Msg::Key(KeyPress::Escape));
        app.update(Msg::Key(KeyPress::Edit));
        app.update(Msg::Replied {
            sent: Sent::SheepConfig {
                name: "web".to_string(),
            },
            result: Ok(Response::SheepConfig(Box::new(
                fixtures::sheep_config_view(),
            ))),
        });
        assert!(app.config_pane().is_some());
    }
    /// fails if `e` on the settings screen's dog row stops probing, or
    /// starts probing something other than the row under the cursor. The
    /// path is what carries a probe to an ADOPTED dog's own binary; a
    /// built-in answers in-process and has none.
    #[test]
    fn e_on_a_settings_dog_row_probes_that_dogs_own_binary() {
        let mut app = fixtures::app_in_settings_on_dog("otel");
        assert_eq!(
            app.update(Msg::Key(KeyPress::Edit)),
            Effect::LoadDogPane {
                name: "otel".to_string(),
                adopted_path: Some(std::path::PathBuf::from("/usr/local/bin/shep-otel")),
            }
        );
        assert!(
            app.config_pane().is_none(),
            "the pane opens on the answer, never on the keypress"
        );
        assert!(
            app.settings().is_some(),
            "and the screen stays up until it does"
        );
    }

    /// fails if `e` on one of the six scalar rows starts doing something.
    /// Those rows are the settings screen's own subject, and `space` and
    /// `Enter` already edit them.
    #[test]
    fn e_on_a_settings_scalar_row_does_nothing_at_all() {
        let mut app = fixtures::app_in_settings();
        assert_eq!(app.update(Msg::Key(KeyPress::Edit)), Effect::None);
        assert!(app.config_pane().is_none());
        assert!(app.notice().is_none(), "and says nothing about it");
    }

    /// fails if a dog that publishes no schema gets a pane anyway, or if
    /// the refusal stops naming the file an operator edits instead
    /// (decision 10).
    #[test]
    fn a_dog_with_no_schema_gets_no_pane_and_is_told_where_to_edit() {
        let mut app = fixtures::app_in_settings_on_dog("otel");
        app.update(Msg::Key(KeyPress::Edit));
        assert_eq!(
            app.update(Msg::DogPane {
                name: "otel".to_string(),
                adopted_path: None,
                result: Err("otel publishes no schema; edit dogs.toml with $EDITOR".to_string()),
            }),
            Effect::None
        );
        assert!(app.config_pane().is_none());
        assert!(
            app.settings().is_some(),
            "the screen the operator pressed `e` on is still there"
        );
        let notice = app.notice().expect("a refusal is reported").to_string();
        assert!(notice.contains("dogs.toml"), "{notice}");
        assert!(notice.contains("$EDITOR"), "{notice}");
    }

    /// fails if the schema stops being the first of two halves: the pane
    /// cannot be drawn until the shepherd has answered with the section,
    /// which is the half this binary has no copy of.
    #[test]
    fn a_schema_asks_for_the_section_and_the_section_opens_the_pane() {
        let mut app = fixtures::app_in_settings_on_dog("metrics");
        app.update(Msg::Key(KeyPress::Edit));
        assert_eq!(
            app.update(Msg::DogPane {
                name: "metrics".to_string(),
                adopted_path: None,
                result: Ok(crate::dog::builtin_schema("metrics").expect("a built-in")),
            }),
            Effect::Send(Sent::DogSection {
                name: "metrics".to_string()
            })
        );
        assert!(app.config_pane().is_none(), "one half is not a pane");
        app.update(Msg::Replied {
            sent: Sent::DogSection {
                name: "metrics".to_string(),
            },
            result: Ok(Response::DogSection {
                toml: "bind = \"0.0.0.0:9615\"\n".to_string().into(),
            }),
        });
        let pane = app.config_pane().expect("both halves are a pane");
        assert_eq!(pane.target().name(), "metrics");
        assert_eq!(pane.value("bind"), "0.0.0.0:9615");
        assert!(
            app.settings().is_none(),
            "the settings screen closes once there is something to look at"
        );
    }

    /// fails if a section arriving for a dog the operator has already left
    /// re-opens a pane over the screen they went back to -- the same
    /// property `config_target` buys for a sheep.
    #[test]
    fn a_section_for_a_dog_nobody_is_waiting_for_is_dropped() {
        let mut app = fixtures::app_in_dog_pane();
        app.update(Msg::Key(KeyPress::Escape));
        assert!(app.config_pane().is_none());
        app.update(Msg::Replied {
            sent: Sent::DogSection {
                name: "bark".to_string(),
            },
            result: Ok(Response::DogSection {
                toml: fixtures::dog_section().into(),
            }),
        });
        assert!(app.config_pane().is_none(), "a late reply re-opens nothing");
    }

    /// fails if a section for the WRONG dog replaces the pane. Its twin
    /// above proves the pane is dropped once nobody is waiting; this one
    /// proves the name is checked while somebody still is, which is the
    /// half a cleared target cannot cover.
    #[test]
    fn a_section_for_a_different_dog_does_not_replace_the_pane() {
        let mut app = fixtures::app_in_dog_pane();
        app.update(Msg::Replied {
            sent: Sent::DogSection {
                name: "metrics".to_string(),
            },
            result: Ok(Response::DogSection {
                toml: "bind = \"0.0.0.0:9615\"\n".to_string().into(),
            }),
        });
        let pane = app.config_pane().expect("the bark pane is still open");
        assert_eq!(pane.target().name(), "bark");
        assert_eq!(
            pane.value("poll"),
            "60s",
            "and still holds bark's own section"
        );
    }

    /// fails if a dog's write stops carrying the WHOLE section, or if it
    /// stops going out through `SetDogConfig` -- a dog has no override
    /// store and no Flockfile, so `ApplyConfig` has nothing to mean here.
    #[test]
    fn a_dog_pane_write_carries_the_whole_edited_section() {
        let mut app = fixtures::app_in_dog_pane();
        let index = app
            .config_pane()
            .expect("the pane is open")
            .fields()
            .fields()
            .iter()
            .position(|field| field.key == "poll")
            .expect("poll is a bark field");
        app.update(Msg::Key(KeyPress::SelectFirst));
        for _ in 0..index {
            app.update(Msg::Key(KeyPress::SelectDown));
        }
        app.update(Msg::Key(KeyPress::Confirm));
        for _ in 0..3 {
            app.update(Msg::Key(KeyPress::TextBackspace));
        }
        for typed in "30s".chars() {
            app.update(Msg::Key(KeyPress::TextChar(typed)));
        }
        app.update(Msg::Key(KeyPress::TextApply));
        let Effect::Send(Sent::SetDogSection { name, toml, .. }) =
            app.update(Msg::Key(KeyPress::Confirm))
        else {
            panic!("the confirm sends the section");
        };
        assert_eq!(name, "bark");
        assert!(
            toml.as_str().contains("poll = \"30s\""),
            "{}",
            toml.as_str()
        );
        assert!(
            toml.as_str().contains("# how often"),
            "a comment shep did not write survives: {}",
            toml.as_str()
        );
    }

    /// fails if a landed write stops re-reading the section, or if the
    /// sentence starts promising the dog acted on it. Only the dog knows
    /// that, which is the same fact the pane's own footer states.
    #[test]
    fn a_landed_dog_write_re_reads_the_section_and_promises_nothing_more() {
        let mut app = fixtures::app_in_dog_pane();
        assert_eq!(
            app.update(Msg::Replied {
                sent: Sent::DogSection {
                    name: "bark".to_string(),
                },
                result: Ok(Response::DogConfigSet {
                    name: "bark".to_string()
                }),
            }),
            Effect::None,
            "a reply routed by its own request, and this one is not the write"
        );
        let Effect::Send(Sent::DogSection { name }) = app.update(Msg::Replied {
            sent: Sent::SetDogSection {
                name: "bark".to_string(),
                ticket: 0,
                toml: fixtures::dog_section().into(),
                authority: WriteAuthority::granted(&app).expect("the fixture opens the gate"),
            },
            result: Ok(Response::DogConfigSet {
                name: "bark".to_string(),
            }),
        }) else {
            panic!("a landed write re-reads");
        };
        assert_eq!(name, "bark");
        let notice = app
            .notice()
            .expect("a landed write is reported")
            .to_string();
        assert!(notice.contains("bark is told"), "{notice}");
    }

    /// fails if `r` in a dog pane starts re-probing the dog's binary. The
    /// schema was read at open and is parked on the app; a keystroke whose
    /// job is to re-read a file must not respawn somebody else's process.
    #[test]
    fn r_in_a_dog_pane_re_reads_the_section_and_never_re_probes() {
        let mut app = fixtures::app_in_dog_pane();
        assert_eq!(
            app.update(Msg::Key(KeyPress::Refresh)),
            Effect::Send(Sent::DogSection {
                name: "bark".to_string()
            })
        );
    }

    /// fails if a dog's map field starts offering the sheep env sub-screen,
    /// which writes through `Request::SetSheepEnv` and would name a sheep
    /// that does not exist. It refuses with `Lock::NoWidget`'s own sentence.
    #[test]
    fn enter_on_a_dogs_map_field_refuses_rather_than_opening_the_env_screen() {
        let mut app = fixtures::app_in_dog_pane();
        let index = app
            .config_pane()
            .expect("the pane is open")
            .fields()
            .fields()
            .iter()
            .position(|field| field.key == "sinks")
            .expect("sinks is a bark field");
        app.update(Msg::Key(KeyPress::SelectFirst));
        for _ in 0..index {
            app.update(Msg::Key(KeyPress::SelectDown));
        }
        assert_eq!(app.update(Msg::Key(KeyPress::Confirm)), Effect::None);
        assert!(
            app.config_pane().expect("still open").env().is_none(),
            "a dog has no env sub-screen"
        );
        let notice = app.notice().expect("a locked row answers").to_string();
        assert!(notice.contains("no editor in this pane"), "{notice}");
    }
}
