//! An open config pane: what it is editing, its fields, and its cursor.
//!
//! The pane is a [`FieldSet`] over one target, plus the values that target
//! currently holds and a [`Viewport`] over the rows. It writes too: a
//! [`PaneEdit`] arms as a [`PanePending`], and [`ConfigPane::declared_app`]
//! turns it into the one-app [`DeclaredApp`] a `Request::ApplyConfig`
//! carries. `env` is the one field that does NOT travel that way -- see
//! [`EnvPane`].

use std::collections::BTreeSet;
use std::time::Instant;

use serde_json::{Map, Value};
use shep_core::config::{
    AppConfig, ApplyGroup, DeclaredApp, GROUP_ORDER, apply_group, flockfile_schema_json,
};
use shep_core::protocol::SheepConfigView;

use super::field::{FieldKind, FieldSet};
use super::viewport::Viewport;

/// Which thing the pane is editing.
///
/// One variant today. An enum rather than a bare name because a dog is the
/// second thing this pane will edit, and a dog decides for itself what a
/// published change reloads -- which is what [`ConfigPane::cost`]'s
/// [`Option`] is for. The variant for it is added by the task that can
/// construct one; an unconstructible variant is dead code, and this repo
/// does not carry an `#[allow(dead_code)]` to hold a place.
///
/// `Debug` is derived (IR-41): a name, not a value the pane withholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTarget {
    /// One sheep, by name.
    Sheep {
        /// The sheep.
        name: String,
    },
}

impl PaneTarget {
    /// The target's name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Sheep { name } => name,
        }
    }
}

/// Why a row cannot be edited from the pane.
///
/// Two different facts, and an operator has to be able to tell them apart:
/// one says the field is beyond editing anywhere, the other says only that
/// this screen has no widget for its shape and a Flockfile still can.
/// Collapsing them into `Field::editable` alone is what made six rows claim
/// the wrong one.
///
/// `Debug` is derived (IR-41): a bare variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    /// shep itself refuses a config write. Identity or flock shape rather
    /// than a runtime knob, so no surface changes it: `name` and
    /// `instances`, whose count moves through `shep stock` instead.
    Refused,
    /// The pane has no widget for this shape, and nothing more than that.
    /// `shep start <Flockfile>` writes these perfectly well, and
    /// [`ConfigPane::cost`] still reports what doing so would cost.
    NoWidget,
}

/// One row of the pane.
///
/// One variant today. Named rather than left as a bare index because the
/// env sub-screen is a second kind of row a later task adds, and a `usize`
/// that silently changes meaning is the failure that would cause.
///
/// `Debug` is derived (IR-41): an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRow {
    /// Index into [`ConfigPane::fields`].
    Field(usize),
}

/// One edit, ready to send.
///
/// One variant today, and named rather than left as a bare pair for
/// [`PaneRow`]'s reason: an unset is a second thing an edit will mean, and
/// a tuple that silently changes meaning is the failure that would cause.
///
/// `Debug` is derived (IR-41): a field name and a candidate value the
/// operator is looking at on screen. A secret field never reaches here --
/// [`ConfigPane::begin_typing`] seeds one empty and the Flockfile schema
/// marks none secret at all -- and `env`, which is where a sheep's secrets
/// actually live, does not travel through this type. See [`EnvPane`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEdit {
    /// Set `key` to `value`.
    Set {
        /// The field.
        key: String,
        /// The new value, already typed to the field's kind.
        value: Value,
    },
}

impl PaneEdit {
    /// Which field this edit moves.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Set { key, .. } => key,
        }
    }
}

/// The pane's one in-flight edit.
///
/// One field on [`ConfigPane`] rather than several [`Option`]s, for the
/// reason the settings screen's own `Pending` gives: typing, armed and sent
/// cannot overlap, and saying so in the type beats saying so in a guard.
///
/// `Debug` is derived (IR-41): the same subjects [`PaneEdit`]'s own doc
/// accounts for, plus a buffer the operator is watching themselves type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanePending {
    /// A text edit under construction. Owns [`super::app::InputMode::Text`]
    /// for as long as it exists.
    Typing {
        /// Which field.
        key: String,
        /// What has been typed so far.
        buffer: String,
    },
    /// Waiting for `Enter`. Nothing has gone out.
    Armed {
        /// The candidate.
        edit: PaneEdit,
        /// The question it reads as, rendered once at arm time.
        text: String,
        /// When it was armed. Only an armed edit expires.
        at: Instant,
    },
    /// Gone out, awaiting the shepherd's reply. Carries the rendered
    /// question only, so the prompt line does not change wording between
    /// the question and its own answer.
    Sent {
        /// The same rendered question.
        text: String,
    },
}

/// One row of the env sub-screen.
///
/// `Debug` is derived (IR-41): an index, or a marker for the row that adds
/// a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRow {
    /// Index into [`EnvPane::keys`].
    Key(usize),
    /// The `+ new` row.
    New,
}

/// The env sub-screen: key names, and a write-only editor over them.
///
/// Write-only is decision 12 of the overrides design, not a shortcut here:
/// `Request::SheepConfig` answers with the env KEYS and no values, so this
/// screen has nothing to seed an editor with and never asks for one. A
/// value goes out through `Request::SetSheepEnv`, one key at a time.
///
/// `Debug` is derived (IR-41): key NAMES, which never left the daemon as
/// values, and a buffer the operator is watching themselves type. The
/// typed value does live in that buffer, which is why nothing in this
/// crate prints an [`EnvPane`] outside a test -- and why the value's own
/// wire type (`shep_core::protocol::EnvValue`) redacts itself the moment
/// it leaves here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPane {
    keys: Vec<String>,
    view: Viewport,
    /// `Some((None, buffer))` on the `+ new` row, where the buffer is
    /// `KEY=value`; `Some((Some(key), buffer))` on an existing key, where
    /// it is the value alone.
    typing: Option<(Option<String>, String)>,
}

impl EnvPane {
    /// A sub-screen over `keys`, cursor at the top and nothing being typed.
    #[must_use]
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys,
            view: Viewport::new(),
            typing: None,
        }
    }

    /// One row per key, then the `+ new` row.
    #[must_use]
    pub fn rows(&self) -> Vec<EnvRow> {
        let mut rows: Vec<EnvRow> = (0..self.keys.len()).map(EnvRow::Key).collect();
        rows.push(EnvRow::New);
        rows
    }

    /// The row under the cursor. Never [`None`]: [`Self::rows`] always ends
    /// with [`EnvRow::New`], so there is always at least one row.
    #[must_use]
    pub fn cursor(&self) -> Option<EnvRow> {
        let rows = self.rows();
        rows.get(self.view.cursor()).copied()
    }

    /// The key names, in display order.
    #[must_use]
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// The cursor and offset.
    #[must_use]
    pub fn view(&self) -> &Viewport {
        &self.view
    }

    /// What is being typed: which key it is for (`None` on the `+ new`
    /// row, where the buffer is the whole `KEY=value`) and the buffer.
    /// [`None`] while no editor is open.
    #[must_use]
    pub fn typing(&self) -> Option<(Option<&str>, &str)> {
        self.typing
            .as_ref()
            .map(|(key, buffer)| (key.as_deref(), buffer.as_str()))
    }

    /// Records the terminal's height, in rows of data.
    pub fn set_rows(&mut self, rows: usize) {
        let len = self.rows().len();
        self.view.set_rows(rows, len);
    }

    pub(super) fn move_by(&mut self, delta: isize) {
        let len = self.rows().len();
        self.view.move_by(delta, len);
    }

    pub(super) fn move_to_first(&mut self) {
        let len = self.rows().len();
        self.view.move_to(0, len);
    }

    pub(super) fn move_to_last(&mut self) {
        let len = self.rows().len();
        self.view.move_to(len.saturating_sub(1), len);
    }

    /// Adopts a previous sub-screen's cursor and offset, clamped. What a
    /// refresh after a set rides on, so the operator is not thrown back to
    /// the first key by their own keystroke.
    pub(super) fn adopt_view(&mut self, view: Viewport) {
        self.view = view;
        let len = self.rows().len();
        self.view.clamp(len);
    }

    /// Opens the editor on the row under the cursor.
    ///
    /// On a key: an EMPTY buffer, because the value is never read back and
    /// seeding one would mean this screen had been told a secret it is
    /// built not to hear. On `+ new`: also empty, and the operator types
    /// `KEY=value`.
    pub fn begin_typing(&mut self) {
        self.typing = match self.cursor() {
            Some(EnvRow::Key(index)) => Some((Some(self.keys[index].clone()), String::new())),
            Some(EnvRow::New) => Some((None, String::new())),
            None => None,
        };
    }

    /// Appends one typed character.
    pub fn type_char(&mut self, typed: char) {
        if let Some((_, buffer)) = self.typing.as_mut() {
            buffer.push(typed);
        }
    }

    /// Removes the last typed character.
    pub fn type_backspace(&mut self) {
        if let Some((_, buffer)) = self.typing.as_mut() {
            buffer.pop();
        }
    }

    /// Drops the editor, leaving the sub-screen open.
    pub fn abandon_typing(&mut self) {
        self.typing = None;
    }

    /// Closes the editor and reads what it holds.
    ///
    /// `(key, Some(value))` sets, `(key, None)` removes. [`None`] when
    /// nothing was being typed, and for a `+ new` buffer with no `=` or an
    /// empty key: neither names a key, and a screen that guessed one would
    /// be inventing the operator's intent.
    pub fn apply_typing(&mut self) -> Option<(String, Option<String>)> {
        let (key, buffer) = self.typing.take()?;
        match key {
            // An existing key with an empty buffer is a removal. There is
            // no separate unset key on this screen, and no widget for one
            // either: an empty value and no value are the same keystroke
            // here, and removing is the one of the two shep can express.
            Some(key) => Some((key, (!buffer.is_empty()).then_some(buffer))),
            None => {
                let (key, value) = buffer.split_once('=')?;
                if key.is_empty() {
                    return None;
                }
                Some((key.to_owned(), Some(value.to_owned())))
            }
        }
    }
}

/// The state of an open pane.
///
/// `Debug` is manual and redacted (IR-41): `values` is a sheep's config with
/// `env` already stripped by [`SheepConfigView::new`], but `args` and `cwd`
/// are still in it and routinely carry a token or a home directory.
/// `env_keys` is a key set, which is itself worth keeping out of a log, and
/// the same reasoning [`SheepConfigView`]'s own `Debug` gives applies here
/// unchanged: this type is a copy of that one's payload.
#[derive(Clone)]
pub struct ConfigPane {
    target: PaneTarget,
    fields: FieldSet,
    values: Map<String, Value>,
    env_keys: Vec<String>,
    overridden: Vec<String>,
    /// Field NAMES parked until the next respawn, as the shepherd reported
    /// them. Nothing to do with [`Self::pending_edit`], which is this
    /// pane's own one in-flight edit; the two words come from opposite
    /// ends and the collision is the shepherd's.
    pending: Vec<String>,
    view: Viewport,
    pending_edit: Option<PanePending>,
    env: Option<EnvPane>,
}

impl core::fmt::Debug for ConfigPane {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ConfigPane {{ target: {:?}, fields: {}, env_keys: {}, cursor: {} }}",
            self.target,
            self.fields.len(),
            self.env_keys.len(),
            self.view.cursor()
        )
    }
}

impl ConfigPane {
    /// A pane over one sheep's config, read off the Flockfile schema.
    ///
    /// The schema is the field list: it already carries every property's
    /// type, default and group, so the pane reads the same document `shep
    /// init` scaffolds from rather than keeping a second list of 39 names
    /// in step with it.
    #[must_use]
    pub fn sheep(view: SheepConfigView) -> Self {
        let schema = flockfile_schema_json().to_value();
        let defs = schema
            .get("$defs")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let properties = defs
            .get("AppConfig")
            .and_then(|app| app.get("properties"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let set = FieldSet::from_properties(&properties, &defs, GROUP_ORDER);
        // A Structural field is identity or flock shape, not a runtime knob:
        // `name` cannot drift without becoming a different sheep, and
        // `instances` is routed through `handle_scale` rather than through a
        // config write at all. Read-only here, so the pane never offers an
        // edit the daemon would refuse.
        let fields = FieldSet::from_fields(
            set.fields()
                .iter()
                .cloned()
                .map(|mut field| {
                    if apply_group(&field.key) == ApplyGroup::Structural {
                        field.editable = false;
                    }
                    field
                })
                .collect(),
            GROUP_ORDER,
        );
        let values = serde_json::to_value(&view.config)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        Self {
            target: PaneTarget::Sheep { name: view.name },
            fields,
            values,
            env_keys: view.env_keys,
            overridden: view.overridden,
            pending: view.pending,
            view: Viewport::new(),
            pending_edit: None,
            env: None,
        }
    }

    /// The one in-flight edit, or [`None`].
    #[must_use]
    pub fn pending_edit(&self) -> Option<&PanePending> {
        self.pending_edit.as_ref()
    }

    /// The open env sub-screen, or [`None`] when the field list is what is
    /// on screen.
    #[must_use]
    pub fn env(&self) -> Option<&EnvPane> {
        self.env.as_ref()
    }

    pub(super) fn env_mut(&mut self) -> Option<&mut EnvPane> {
        self.env.as_mut()
    }

    /// Opens the env sub-screen over this sheep's key names.
    pub(super) fn open_env(&mut self) {
        self.env = Some(EnvPane::new(self.env_keys.clone()));
    }

    /// Closes it, leaving the field list up.
    pub(super) fn close_env(&mut self) {
        self.env = None;
    }

    /// The key under the cursor, and why the pane will not edit it, when it
    /// will not. [`None`] both for a row that edits and for no row at all.
    ///
    /// The one place a caller asks "may I edit what is selected", so a
    /// refusal is raised for the right one of [`Lock`]'s two reasons rather
    /// than for a generic third.
    #[must_use]
    pub fn cursor_lock(&self) -> Option<(&str, Lock)> {
        let PaneRow::Field(index) = self.cursor()?;
        let field = self.fields.fields().get(index)?;
        self.lock(&field.key).map(|lock| (field.key.as_str(), lock))
    }

    /// The kind of widget the row under the cursor wants, or [`None`] for
    /// no row at all.
    #[must_use]
    pub fn cursor_kind(&self) -> Option<&FieldKind> {
        let PaneRow::Field(index) = self.cursor()?;
        self.fields.fields().get(index).map(|field| &field.kind)
    }

    /// Arms the opposite of what a bool holds, or the next name in a
    /// choice. Does nothing for a locked field, or for one no keystroke
    /// cycles ([`FieldKind::Text`], [`FieldKind::Integer`],
    /// [`FieldKind::Map`], [`FieldKind::Opaque`]).
    pub fn cycle(&mut self, now: Instant) {
        let Some(PaneRow::Field(index)) = self.cursor() else {
            return;
        };
        let Some(field) = self.fields.fields().get(index) else {
            return;
        };
        if self.lock(&field.key).is_some() {
            return;
        }
        // The base is whatever is already armed FOR THIS FIELD, so a second
        // `space` walks one step further along the cycle rather than
        // re-deriving the same value the stored config would give -- the
        // same rule `Settings::next_candidate` states at length, and
        // without it a choice of six could never reach its fourth value.
        // An arm for a DIFFERENT field (the cursor moved after arming) is
        // not a base; that starts fresh from the stored value.
        let armed_here = match &self.pending_edit {
            Some(PanePending::Armed {
                edit: PaneEdit::Set { key, value },
                ..
            }) if *key == field.key => Some(value),
            _ => None,
        };
        let current = armed_here.or_else(|| self.values.get(&field.key));
        let next = match &field.kind {
            FieldKind::Bool => Value::Bool(!current.and_then(Value::as_bool).unwrap_or(false)),
            FieldKind::Choice(names) if !names.is_empty() => {
                let current = current.and_then(Value::as_str);
                let next = current
                    .and_then(|value| names.iter().position(|name| name == value))
                    .map_or(0, |i| (i + 1) % names.len());
                Value::String(names[next].clone())
            }
            FieldKind::Choice(_)
            | FieldKind::Text
            | FieldKind::Integer
            | FieldKind::Map
            | FieldKind::Opaque => return,
        };
        let edit = PaneEdit::Set {
            key: field.key.clone(),
            value: next,
        };
        let text = self.confirm_text(&edit);
        self.pending_edit = Some(PanePending::Armed {
            edit,
            text,
            at: now,
        });
    }

    /// Opens the text editor on the row under the cursor. Does nothing for
    /// a locked field, or for one that is not typed.
    ///
    /// Seeded with what is on screen, except for a secret, which is seeded
    /// empty: the pane renders `<set>` for one and never holds the value,
    /// so a seed would have to invent it.
    pub fn begin_typing(&mut self) {
        let Some(PaneRow::Field(index)) = self.cursor() else {
            return;
        };
        let Some(field) = self.fields.fields().get(index) else {
            return;
        };
        if self.lock(&field.key).is_some()
            || !matches!(field.kind, FieldKind::Text | FieldKind::Integer)
        {
            return;
        }
        let seed = if field.secret {
            String::new()
        } else {
            match self.value(&field.key) {
                unset if unset == "(unset)" => String::new(),
                value => value,
            }
        };
        self.pending_edit = Some(PanePending::Typing {
            key: field.key.clone(),
            buffer: seed,
        });
    }

    /// Appends one typed character.
    pub fn type_char(&mut self, typed: char) {
        if let Some(PanePending::Typing { buffer, .. }) = self.pending_edit.as_mut() {
            buffer.push(typed);
        }
    }

    /// Removes the last typed character.
    pub fn type_backspace(&mut self) {
        if let Some(PanePending::Typing { buffer, .. }) = self.pending_edit.as_mut() {
            buffer.pop();
        }
    }

    /// Turns the buffer into an armed edit, typed to the field's kind.
    ///
    /// An empty buffer is `null`, which is how a nullable field is unset.
    /// An integer field whose buffer does not parse keeps the editor open
    /// rather than arming a string the daemon would refuse: the operator is
    /// mid-word, not wrong.
    pub fn apply_typing(&mut self, now: Instant) {
        let Some(PanePending::Typing { key, buffer }) = self.pending_edit.take() else {
            return;
        };
        let kind = self.fields.by_key(&key).map(|field| field.kind.clone());
        let value = match (kind, buffer.as_str()) {
            (_, "") => Value::Null,
            (Some(FieldKind::Integer), text) => match text.parse::<i64>() {
                Ok(number) => Value::from(number),
                Err(_) => {
                    self.pending_edit = Some(PanePending::Typing { key, buffer });
                    return;
                }
            },
            (_, text) => Value::String(text.to_owned()),
        };
        let edit = PaneEdit::Set { key, value };
        let text = self.confirm_text(&edit);
        self.pending_edit = Some(PanePending::Armed {
            edit,
            text,
            at: now,
        });
    }

    /// Drops an editor under construction, leaving the pane open.
    pub fn abandon_typing(&mut self) {
        if matches!(self.pending_edit, Some(PanePending::Typing { .. })) {
            self.pending_edit = None;
        }
    }

    /// Drops an armed edit. A request already sent is not cancellable by a
    /// keypress, the same rule every other confirm in lookout follows.
    pub fn cancel(&mut self) {
        if matches!(self.pending_edit, Some(PanePending::Armed { .. })) {
            self.pending_edit = None;
        }
    }

    /// Whether an edit is armed -- the one state a stray key has to eat
    /// rather than also doing its ordinary job.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        matches!(self.pending_edit, Some(PanePending::Armed { .. }))
    }

    /// When the armed edit was armed, for the expiry the tick runs.
    #[must_use]
    pub fn armed_at(&self) -> Option<Instant> {
        match self.pending_edit {
            Some(PanePending::Armed { at, .. }) => Some(at),
            _ => None,
        }
    }

    /// Takes the armed edit out and marks it sent. [`None`] when nothing is
    /// armed, and the pane is left exactly as it was.
    pub fn take_armed(&mut self) -> Option<PaneEdit> {
        match self.pending_edit.take() {
            Some(PanePending::Armed { edit, text, .. }) => {
                self.pending_edit = Some(PanePending::Sent { text });
                Some(edit)
            }
            other => {
                self.pending_edit = other;
                None
            }
        }
    }

    /// Clears whatever is in flight, once the shepherd has answered.
    pub fn settle(&mut self) {
        self.pending_edit = None;
    }

    /// The question an armed edit reads as, in what it costs.
    fn confirm_text(&self, edit: &PaneEdit) -> String {
        let PaneEdit::Set { key, value } = edit;
        let shown = match value {
            Value::Null => "(unset)".to_owned(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let name = self.target.name();
        // `ApplyGroup` is `#[non_exhaustive]`, so the wildcard is required
        // rather than chosen, and it answers `respawn` for the reason
        // `view::pane::cost_label` gives: a group this binary has not been
        // taught about promises a restart rather than a silent claim that
        // the change applied. `Structural` cannot reach here -- `cycle` and
        // `begin_typing` both refuse a locked field -- and lands in the
        // same conservative arm if it ever does.
        match self.cost(key) {
            Some(ApplyGroup::Live) => format!("set {key} = {shown}? {name} takes it now"),
            Some(ApplyGroup::NextSpawn) => {
                format!("set {key} = {shown}? {name} picks it up at its next start")
            }
            Some(_) => format!("set {key} = {shown}? {name} is respawned to pick it up"),
            None => format!("set {key} = {shown}? {name} is told, and decides what to reload"),
        }
    }

    /// The one-app [`DeclaredApp`] a sheep edit sends.
    ///
    /// `declared` is EXACTLY the edited key, and that is the whole write
    /// path: `Request::ApplyConfig` is sent at `ResetDepth::File`, under
    /// which a key the template declares is reset and a key it does not
    /// declare is kept. One name in `declared` is therefore one field
    /// moved, and a second name would be a second field moved in silence.
    /// At the default `ResetDepth::None` the same request would be ignored
    /// for every key an operator has ever touched.
    ///
    /// [`None`] when the edited value does not deserialize into an
    /// [`AppConfig`] -- an integer field set to `null`, say -- rather than
    /// sending a config the daemon would refuse.
    #[must_use]
    pub fn declared_app(&self, edit: &PaneEdit) -> Option<DeclaredApp> {
        let PaneEdit::Set { key, value } = edit;
        let mut values = self.values.clone();
        values.insert(key.clone(), value.clone());
        // `env` is absent from `values` in all but name: the shepherd
        // strips it on the way out (decision 12), so what round-trips here
        // is the empty map, and the pane could not put a value back even if
        // it wanted to. `declared_env` is empty for the same reason.
        let config: AppConfig = serde_json::from_value(Value::Object(values)).ok()?;
        Some(DeclaredApp {
            config,
            declared: core::iter::once(key.clone()).collect(),
            declared_env: BTreeSet::new(),
        })
    }

    /// What is being edited.
    #[must_use]
    pub fn target(&self) -> &PaneTarget {
        &self.target
    }

    /// The form.
    #[must_use]
    pub fn fields(&self) -> &FieldSet {
        &self.fields
    }

    /// The current value of `key`, rendered for a cell.
    ///
    /// A scalar shows bare, an absent or `null` value shows `(unset)`, and
    /// anything else shows compact JSON. `env` is the one field whose value
    /// this pane never holds -- the shepherd strips it on the way out -- so
    /// it shows its key count instead, and the sub-screen shows the names.
    #[must_use]
    pub fn value(&self, key: &str) -> String {
        if key == "env" {
            return match self.env_keys.len() {
                1 => "1 key".to_owned(),
                count => format!("{count} keys"),
            };
        }
        match self.values.get(key) {
            None | Some(Value::Null) => "(unset)".to_owned(),
            Some(Value::String(text)) => text.clone(),
            Some(Value::Bool(flag)) => flag.to_string(),
            Some(Value::Number(number)) => number.to_string(),
            Some(other) => other.to_string(),
        }
    }

    /// What changing `key` costs.
    ///
    /// [`None`] is not reachable for a sheep and is not dead weight either:
    /// a dog decides for itself what a published change reloads, so the
    /// answer for one is "the pane does not know", and every caller already
    /// renders that as an empty cost cell rather than as a guess.
    #[must_use]
    pub fn cost(&self, key: &str) -> Option<ApplyGroup> {
        match self.target {
            PaneTarget::Sheep { .. } => Some(apply_group(key)),
        }
    }

    /// Why the pane will not edit `key`, or [`None`] when it will.
    ///
    /// [`Lock::Refused`] outranks [`Lock::NoWidget`]: a Structural field
    /// that also happened to have no widget is still refused by shep, which
    /// is the fact that survives the pane gaining every widget it lacks.
    #[must_use]
    pub fn lock(&self, key: &str) -> Option<Lock> {
        if self.cost(key) == Some(ApplyGroup::Structural) {
            return Some(Lock::Refused);
        }
        match self.fields.by_key(key) {
            Some(field) if !field.editable => Some(Lock::NoWidget),
            _ => None,
        }
    }

    /// Whether an operator has overridden `key`.
    #[must_use]
    pub fn is_overridden(&self, key: &str) -> bool {
        self.overridden.iter().any(|name| name == key)
    }

    /// Whether `key` is parked until the next respawn.
    #[must_use]
    pub fn is_pending(&self, key: &str) -> bool {
        self.pending.iter().any(|name| name == key)
    }

    /// The cursor and offset.
    #[must_use]
    pub fn view(&self) -> &Viewport {
        &self.view
    }

    /// Records the terminal's height, in rows of data.
    pub fn set_rows(&mut self, rows: usize) {
        let len = self.rows().len();
        self.view.set_rows(rows, len);
    }

    /// One row per field, in display order.
    #[must_use]
    pub fn rows(&self) -> Vec<PaneRow> {
        (0..self.fields.len()).map(PaneRow::Field).collect()
    }

    /// The row under the cursor, or `None` for an empty form.
    #[must_use]
    pub fn cursor(&self) -> Option<PaneRow> {
        let rows = self.rows();
        rows.get(self.view.cursor()).copied()
    }

    pub(super) fn move_by(&mut self, delta: isize) {
        let len = self.rows().len();
        self.view.move_by(delta, len);
    }

    pub(super) fn move_to_first(&mut self) {
        let len = self.rows().len();
        self.view.move_to(0, len);
    }

    pub(super) fn move_to_last(&mut self) {
        let len = self.rows().len();
        self.view.move_to(len.saturating_sub(1), len);
    }

    /// Adopts a previous pane's cursor and offset, clamped to this one's
    /// own row count. What a refresh of an already-open pane rides on, so
    /// `r` does not throw the operator back to the first field.
    pub(super) fn adopt_view(&mut self, view: Viewport) {
        self.view = view;
        let len = self.rows().len();
        self.view.clamp(len);
    }

    /// Re-opens the env sub-screen on the refreshed key list, at the cursor
    /// and offset it had. Setting a key re-reads the whole config, and
    /// without this the sub-screen would slam shut on the operator's own
    /// keystroke -- and on the one screen where the keystroke ADDS a row.
    pub(super) fn adopt_env_view(&mut self, view: Viewport) {
        let mut env = EnvPane::new(self.env_keys.clone());
        env.adopt_view(view);
        self.env = Some(env);
    }

    #[cfg(test)]
    pub(crate) fn move_to_key(&mut self, key: &str) {
        if let Some(index) = self
            .fields
            .fields()
            .iter()
            .position(|field| field.key == key)
        {
            let len = self.rows().len();
            self.view.move_to(index, len);
        }
    }
}

#[cfg(test)]
mod tests {
    use shep_core::config::AppConfig;

    use super::*;

    fn web() -> SheepConfigView {
        let mut config = AppConfig {
            name: "web".into(),
            max_restarts: 32,
            ..AppConfig::default()
        };
        config
            .env
            .insert("DB_HOST".into(), "{{shared:DB_HOST}}".into());
        SheepConfigView::new(config, vec!["max_restarts".into()], vec!["env".into()])
    }

    #[test]
    fn a_sheep_pane_has_thirty_nine_fields_in_four_groups() {
        let pane = ConfigPane::sheep(web());
        assert_eq!(pane.fields().len(), 39);
        assert!(!pane.fields().is_empty());
        let mut groups: Vec<&str> = Vec::new();
        for field in pane.fields().fields() {
            let group = field.group.as_deref().expect("every field carries a group");
            if groups.last() != Some(&group) {
                groups.push(group);
            }
        }
        assert_eq!(groups, ["process", "inputs", "control", "cron"]);
    }

    #[test]
    fn a_value_renders_bare_for_a_scalar_and_as_a_count_for_env() {
        let pane = ConfigPane::sheep(web());
        assert_eq!(pane.value("max_restarts"), "32");
        assert_eq!(pane.value("autorestart"), "true");
        assert_eq!(pane.value("cwd"), "(unset)");
        assert_eq!(pane.value("env"), "1 key");
    }

    #[test]
    fn cost_comes_from_apply_group_for_a_sheep() {
        let pane = ConfigPane::sheep(web());
        assert_eq!(pane.cost("max_restarts"), Some(ApplyGroup::Live));
        assert_eq!(pane.cost("kill_signal"), Some(ApplyGroup::NextSpawn));
        assert_eq!(pane.cost("script"), Some(ApplyGroup::NeedsRespawn));
        assert_eq!(pane.cost("instances"), Some(ApplyGroup::Structural));
    }

    #[test]
    fn structural_fields_are_not_editable_and_the_rest_are() {
        let pane = ConfigPane::sheep(web());
        for key in ["name", "instances"] {
            assert!(!pane.fields().by_key(key).unwrap().editable, "{key}");
        }
        assert!(pane.fields().by_key("max_restarts").unwrap().editable);
    }

    #[test]
    fn overridden_and_pending_are_read_off_the_view() {
        let pane = ConfigPane::sheep(web());
        assert!(pane.is_overridden("max_restarts"));
        assert!(!pane.is_overridden("autorestart"));
        assert!(pane.is_pending("env"));
        assert!(!pane.is_pending("max_restarts"));
    }

    /// fails if the cursor stops tracking the viewport, or if the last row
    /// stops being reachable.
    #[test]
    fn the_cursor_walks_the_rows_and_clamps_at_both_ends() {
        let mut pane = ConfigPane::sheep(web());
        assert_eq!(pane.cursor(), Some(PaneRow::Field(0)));
        pane.move_by(1);
        assert_eq!(pane.cursor(), Some(PaneRow::Field(1)));
        pane.move_by(-5);
        assert_eq!(pane.cursor(), Some(PaneRow::Field(0)));
        pane.move_to_last();
        assert_eq!(pane.cursor(), Some(PaneRow::Field(38)));
        assert_eq!(
            pane.fields().fields()[38].key,
            "cron_timezone",
            "the last row is the last field"
        );
        pane.move_to_first();
        assert_eq!(pane.cursor(), Some(PaneRow::Field(0)));
    }

    /// fails if a refresh throws the operator back to the first field.
    #[test]
    fn a_refreshed_pane_keeps_the_cursor_it_had() {
        let mut pane = ConfigPane::sheep(web());
        pane.set_rows(10);
        pane.move_to_last();
        let carried = pane.view().clone();
        let mut fresh = ConfigPane::sheep(web());
        fresh.adopt_view(carried);
        assert_eq!(fresh.cursor(), Some(PaneRow::Field(38)));
    }

    /// fails if `space` on a bool stops proposing the opposite of what is
    /// on screen, or stops naming the field in the question it asks.
    #[test]
    fn cycling_a_bool_arms_a_set_with_the_flipped_value() {
        let mut pane = ConfigPane::sheep(web());
        pane.move_to_key("autorestart");
        pane.cycle(Instant::now());
        let Some(PanePending::Armed {
            edit: PaneEdit::Set { key, value },
            text,
            ..
        }) = pane.pending_edit()
        else {
            panic!("{:?}", pane.pending_edit());
        };
        assert_eq!(key, "autorestart");
        assert_eq!(*value, serde_json::json!(false));
        assert!(text.contains("autorestart"), "{text}");
    }

    /// fails if a field that costs a respawn stops saying so, or stops
    /// naming the sheep that pays for it.
    #[test]
    fn a_respawn_field_arms_a_confirm_that_names_the_death() {
        let mut pane = ConfigPane::sheep(web());
        pane.move_to_key("merge_logs");
        pane.cycle(Instant::now());
        let Some(PanePending::Armed { text, .. }) = pane.pending_edit() else {
            panic!("{:?}", pane.pending_edit());
        };
        assert!(text.contains("respawn"), "{text}");
        assert!(text.contains("web"), "{text}");
    }

    /// fails if a field shep refuses a config write for starts arming one.
    #[test]
    fn a_read_only_field_does_not_arm() {
        let mut pane = ConfigPane::sheep(web());
        pane.move_to_key("instances");
        pane.cycle(Instant::now());
        assert!(pane.pending_edit().is_none());
        pane.begin_typing();
        assert!(pane.pending_edit().is_none());
    }

    /// fails if a typed integer arms as a JSON string, which
    /// `AppConfig`'s own deserializer would then refuse, and if the editor
    /// stops opening on the value that is already on screen.
    #[test]
    fn typing_into_an_integer_and_applying_arms_a_number_not_a_string() {
        let mut pane = ConfigPane::sheep(web());
        pane.move_to_key("max_restarts");
        pane.begin_typing();
        let Some(PanePending::Typing { buffer, .. }) = pane.pending_edit() else {
            panic!("{:?}", pane.pending_edit());
        };
        assert_eq!(buffer, "32", "the editor opens on what is on screen");
        pane.type_backspace();
        pane.type_backspace();
        for c in "40".chars() {
            pane.type_char(c);
        }
        pane.apply_typing(Instant::now());
        let Some(PanePending::Armed {
            edit: PaneEdit::Set { value, .. },
            ..
        }) = pane.pending_edit()
        else {
            panic!("{:?}", pane.pending_edit());
        };
        assert_eq!(*value, serde_json::json!(40));
    }

    /// fails if one edit ever declares more than the key it edited. The
    /// whole write path rides on this: under `ResetDepth::File` a declared
    /// key is reset and an undeclared one is kept, so a second name in
    /// `declared` is a second field silently moved.
    #[test]
    fn a_declared_app_for_one_edit_declares_only_that_key() {
        let pane = ConfigPane::sheep(web());
        let edit = PaneEdit::Set {
            key: "max_restarts".into(),
            value: serde_json::json!(40),
        };
        let app = pane.declared_app(&edit).unwrap();
        assert_eq!(app.config.max_restarts, 40);
        assert_eq!(app.config.name, "web");
        assert_eq!(
            app.declared,
            ["max_restarts".to_owned()].into_iter().collect()
        );
        assert!(app.declared_env.is_empty());
        assert!(
            app.config.env.is_empty(),
            "env is never round-tripped through a pane"
        );
    }

    /// fails if the env sub-screen stops offering a row to add a key on, or
    /// stops reading an empty buffer on an existing key as a removal.
    #[test]
    fn the_env_pane_lists_keys_and_a_new_row_and_an_empty_apply_means_unset() {
        let mut env = EnvPane::new(vec!["A".into(), "B".into()]);
        assert_eq!(env.rows().len(), 3, "two keys and a + new row");
        env.move_to_last();
        env.begin_typing();
        for c in "C=3".chars() {
            env.type_char(c);
        }
        assert_eq!(
            env.apply_typing(),
            Some(("C".to_owned(), Some("3".to_owned())))
        );
        env.move_to_first();
        env.begin_typing();
        assert_eq!(env.apply_typing(), Some(("A".to_owned(), None)));
    }

    /// fails if the pane's own `Debug` starts printing the config it holds.
    /// `args` and `cwd` live in `values` and routinely carry a token or a
    /// home directory (IR-41).
    #[test]
    fn the_panes_debug_names_no_value_it_holds() {
        let mut config = AppConfig {
            name: "web".into(),
            cwd: Some("/home/ada/secret-project".into()),
            args: vec!["--token".into(), "hunter2".into()],
            ..AppConfig::default()
        };
        config.env.insert("DB_PASSWORD".into(), "hunter2".into());
        let pane = ConfigPane::sheep(SheepConfigView::new(config, Vec::new(), Vec::new()));
        assert_eq!(
            format!("{pane:?}"),
            r#"ConfigPane { target: Sheep { name: "web" }, fields: 39, env_keys: 1, cursor: 0 }"#
        );
    }
}
