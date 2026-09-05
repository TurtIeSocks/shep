//! An open config pane: what it is editing, its fields, and its cursor.
//!
//! The pane is a [`FieldSet`] over one target, plus the values that target
//! currently holds and a [`Viewport`] over the rows. It writes too: a
//! [`PaneEdit`] arms as a [`PanePending`], and leaves as a
//! `Request::SetSheepField` or, for `env`, a `Request::SetSheepEnv`. Both
//! write an operator override for one key; neither pretends to be a
//! template. See [`PaneEdit`] for why the `ApplyConfig` route this pane
//! shipped with was wrong.

use std::path::PathBuf;
use std::time::Instant;

use serde_json::{Map, Value};
use shep_core::config::{ApplyGroup, GROUP_ORDER, apply_group, flockfile_schema_json};
use shep_core::protocol::{EnvValue, SheepConfigView};

use super::field::{FieldKind, FieldSet};
use super::viewport::Viewport;

/// Which thing the pane is editing.
///
/// Two things, and they are not the same shape of edit. A sheep's config is
/// shep's own document, so shep knows what every field costs; a dog's
/// section belongs to the dog, so shep publishes the change and the dog
/// decides what to reload -- which is what [`ConfigPane::cost`]'s [`Option`]
/// is for.
///
/// `Debug` is derived (IR-41): a name and a binary's path, neither of which
/// is a value the pane withholds. A dog's SECTION can carry a credential and
/// is held on [`ConfigPane`] instead, behind that type's own redacted
/// `Debug`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTarget {
    /// One sheep, by name.
    Sheep {
        /// The sheep.
        name: String,
    },
    /// One dog, by name, with the binary its schema was probed from.
    Dog {
        /// The dog.
        name: String,
        /// The adopted binary, or [`None`] for a built-in -- whose schema
        /// comes from `crate::dog::builtin_schema`, since a built-in dog is
        /// this same binary. Kept so a re-probe asks the same path the pane
        /// opened on.
        adopted_path: Option<PathBuf>,
    },
}

impl PaneTarget {
    /// The target's name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Sheep { name } | Self::Dog { name, .. } => name,
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

/// One config field's new value, on its way out of the pane.
///
/// A newtype for the reason [`EnvValue`] is one, and it is the same
/// argument reaching one hop further than it did. `cwd` and `script`
/// routinely hold a home directory and `args` holds a token, so
/// [`ConfigPane`]'s own `Debug` already withholds the map these are lifted
/// out of. A bare [`Value`] then travelled from [`PaneEdit`] into
/// `Sent::ApplyField`, which derives `Debug`, and printed
/// `value: String("/home/ada/secret-project")` in the clear. Two
/// hand-written redactions on two types, one of which was missed, is what
/// this replaces: the protection now rides the value wherever it goes.
///
/// **The WIRE field is deliberately still a bare [`Value`]**
/// (`Request::SetSheepField`). `env` is the one field `AppConfig`'s own
/// `Debug` redacts, and `cwd` is printed in the clear by every request that
/// carries a whole config, so a newtype there would protect one copy of a
/// value the protocol prints three other ways. This one guards lookout,
/// where the rule really is that a live value is never printed.
///
/// `Debug` is manual and redacted (IR-41), exact-string-tested below. It
/// names the JSON TYPE and nothing else, which is what a diagnostic needs
/// and is not a value.
#[derive(Clone, PartialEq, Eq)]
pub struct FieldValue(Value);

impl FieldValue {
    /// The value, for the request that carries it.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

impl From<Value> for FieldValue {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

/// Prints the JSON type and never the value -- see the type doc for why.
/// Exact-string-tested below (`a_field_values_debug_names_no_value`) so a
/// future `#[derive(Debug)]` fails that test instead of silently reopening
/// the leak.
impl core::fmt::Debug for FieldValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match &self.0 {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        write!(f, "FieldValue(<{kind}>)")
    }
}

/// One edit, ready to send.
///
/// Two variants, and they leave by their own doors: a [`Self::Set`] as a
/// `Request::SetSheepField` and a [`Self::SetEnv`] as a
/// `Request::SetSheepEnv`. Both record an operator override for one key.
///
/// A [`Self::Set`] went out as a one-app `Request::ApplyConfig` at
/// `ResetDepth::File` until 2026-09-05, which moved the field correctly and
/// then SPENT the override for it: that merge is a template load, and a key
/// put back to the template is rightly not a key an operator is still
/// holding a value for. A pane's value is the operator's, so the edit
/// vanished from `overridden` the moment it landed and the `*` this pane
/// draws never appeared for it.
///
/// `Debug` is DERIVED, and safe because both value types redact themselves:
/// [`FieldValue`] names a JSON type and [`EnvValue`] names a byte count.
/// It was manual until 2026-09-05, which protected this type and nothing
/// downstream of it -- `Sent::ApplyField` held the same value behind a
/// derived `Debug` and printed it. One mechanism on the value beats two on
/// the types that carry it; see [`FieldValue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEdit {
    /// Set the config field `key` to `value`.
    Set {
        /// The field.
        key: String,
        /// The new value, already typed to the field's kind.
        value: FieldValue,
    },
    /// Set the env key `key`, or with [`None`] remove it.
    SetEnv {
        /// The env key.
        key: String,
        /// The value, or [`None`] to remove the key.
        value: Option<EnvValue>,
    },
}

impl PaneEdit {
    /// Which field or env key this edit moves.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Set { key, .. } | Self::SetEnv { key, .. } => key,
        }
    }
}

/// The pane's one in-flight edit.
///
/// One field on [`ConfigPane`] rather than several [`Option`]s, for the
/// reason the settings screen's own `Pending` gives: typing, armed and sent
/// cannot overlap, and saying so in the type beats saying so in a guard.
///
/// Shared by the field list and the env sub-screen. Both arm, and neither
/// needed a mechanism of its own: `view::status` renders whatever is here,
/// `Msg::Tick` expires whatever is armed, and the only thing that differs
/// is the [`PaneEdit`] variant inside.
///
/// `Debug` is MANUAL and redacted (IR-41), exact-string-tested below. Two
/// of the three variants would otherwise print a value: `Typing`'s buffer
/// is what the operator is halfway through typing, which on the env screen
/// is a secret, and `Armed`/`Sent`'s `text` is a rendered sentence that
/// quotes a config value verbatim. The env sentence deliberately does not
/// quote its own value ([`ConfigPane::confirm_text`]), but the field
/// sentence does, so the whole field is withheld rather than one arm of it.
#[derive(Clone, PartialEq, Eq)]
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
    /// Gone out, awaiting the shepherd's reply.
    ///
    /// `ticket` is what a landing reply is matched against
    /// ([`ConfigPane::settle`]). Without it any reply settled whatever was
    /// pending, so a second write's "sent, waiting" line vanished the
    /// moment the FIRST write's reply landed, while the second was still
    /// outstanding. It is a ticket rather than the KEY, which was the
    /// first fix and left the same hole one step narrower: two writes to
    /// the same field, in flight at once, still settled on the first reply.
    Sent {
        /// The write this is waiting on. Minted by `App` per send, so no
        /// two are ever equal.
        ticket: u64,
        /// Which field or env key is in flight. Not what a reply is
        /// matched against -- see `ticket` -- but what a `{:?}` names.
        key: String,
        /// The rendered question, so the prompt line does not change
        /// wording between the question and its own answer.
        text: String,
    },
}

/// Prints the key and never a buffer or a rendered sentence -- see the type
/// doc for why. Exact-string-tested below
/// (`a_pane_pendings_debug_names_no_value`).
impl core::fmt::Debug for PanePending {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Typing { key, buffer } => write!(
                f,
                "PanePending::Typing {{ key: {key:?}, buffer: <{} chars> }}",
                buffer.chars().count()
            ),
            Self::Armed { edit, .. } => write!(f, "PanePending::Armed {{ edit: {edit:?} }}"),
            Self::Sent { ticket, key, .. } => {
                write!(f, "PanePending::Sent {{ ticket: {ticket}, key: {key:?} }}")
            }
        }
    }
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
/// `Debug` is MANUAL and redacted (IR-41), exact-string-tested below. The
/// key names are withheld for the reason [`ConfigPane`]'s own `Debug`
/// withholds `env_keys`, and the buffer because on this screen it IS the
/// secret: an operator typing `DB_PASSWORD=hunter2` has the whole of it in
/// there. A convention that nothing prints an `EnvPane` would not survive
/// the first `{:?}` somebody adds to a diagnostic, which is why this is a
/// test rather than a comment.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvPane {
    keys: Vec<String>,
    view: Viewport,
    /// `Some((None, buffer))` on the `+ new` row, where the buffer is
    /// `KEY=value`; `Some((Some(key), buffer))` on an existing key, where
    /// it is the value alone.
    typing: Option<(Option<String>, String)>,
}

/// Prints counts and never a key or a buffer -- see the type doc for why.
/// Exact-string-tested below (`an_env_panes_debug_names_no_key_and_no_value`).
impl core::fmt::Debug for EnvPane {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "EnvPane {{ keys: {}, cursor: {}, typing: {} }}",
            self.keys.len(),
            self.view.cursor(),
            self.typing.is_some()
        )
    }
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

    /// The key the cursor is on, or [`None`] on the `+ new` row.
    ///
    /// What a refresh carries across instead of the cursor's INDEX. A
    /// removal shortens the list, so an index that survived would name a
    /// DIFFERENT key afterwards and a reflexive second `Enter` would arm a
    /// write against the neighbour.
    #[must_use]
    pub fn cursor_key(&self) -> Option<&str> {
        match self.cursor()? {
            EnvRow::Key(index) => self.keys.get(index).map(String::as_str),
            EnvRow::New => None,
        }
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

    /// Adopts a previous sub-screen's offset, and puts the cursor back on
    /// `cursor_key` BY NAME.
    ///
    /// A set re-reads the whole config, so without this the operator would
    /// be thrown back to the first key by their own keystroke. Carrying the
    /// cursor's INDEX instead is worse than either: a removal shortens the
    /// list under it, so the same index names the NEXT key afterwards and a
    /// reflexive second `Enter` arms a write against a neighbour nobody
    /// chose.
    ///
    /// A key that is gone -- which is exactly the removal case -- puts the
    /// cursor on the `+ new` row rather than on whatever took its place.
    /// That is the one row where `Enter` cannot destroy anything, which is
    /// the property being bought; landing on the neighbour would be the
    /// silent retarget this exists to prevent, one row further down.
    pub(super) fn adopt_view(&mut self, view: Viewport, cursor_key: Option<&str>) {
        self.view = view;
        let len = self.rows().len();
        let index = match cursor_key {
            Some(key) => self
                .keys
                .iter()
                .position(|name| name == key)
                .unwrap_or(len - 1),
            None => len - 1,
        };
        self.view.move_to(index, len);
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
    /// The dog's `[<name>]` table as TOML TEXT, and [`None`] for a sheep.
    ///
    /// Kept beside the parsed `values` rather than instead of them, because
    /// the two answer different questions: `values` is what the rows render,
    /// and this is what a write edits. `Request::SetDogConfig` replaces the
    /// whole section, so an edit that re-rendered it from `values` would
    /// throw away every comment the operator wrote -- see
    /// [`Self::edited_section`].
    section: Option<String>,
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
            section: None,
        }
    }

    /// A pane over one dog's `[<name>]` section.
    ///
    /// `schema` is the dog's own answer to the schema flag -- probed at open
    /// rather than read from anywhere, because nothing records one: `shep
    /// adopt` uses it for the vet and writes down only the path. `section`
    /// is the table as `Request::DogConfig` rendered it, empty when
    /// `dogs.toml` has no such table.
    ///
    /// FLAT, in schema order, with no group headers (decision 3): a dog's
    /// schema carries no `init.group`, and inventing sections for one would
    /// be shep deciding how somebody else's config reads.
    ///
    /// A [`FieldKind::Map`] row is marked not editable, so it draws
    /// [`Lock::NoWidget`] and refuses with that lock's own sentence. The
    /// env sub-screen is the only map editor this pane has and it is a
    /// SHEEP's env: it writes through `Request::SetSheepEnv` and renders
    /// [`Self::env_keys`], neither of which means anything for a dog.
    #[must_use]
    pub fn dog(
        name: String,
        adopted_path: Option<PathBuf>,
        schema: Value,
        section: String,
    ) -> Self {
        let defs = schema
            .get("$defs")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let set = FieldSet::from_properties(&properties, &defs, &[]);
        let fields = FieldSet::from_fields(
            set.fields()
                .iter()
                .cloned()
                .map(|mut field| {
                    if field.kind == FieldKind::Map {
                        field.editable = false;
                    }
                    field
                })
                .collect(),
            &[],
        );
        let values: Map<String, Value> = section
            .parse::<toml::Table>()
            .ok()
            .and_then(|table| serde_json::to_value(table).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        Self {
            target: PaneTarget::Dog { name, adopted_path },
            fields,
            values,
            env_keys: Vec::new(),
            overridden: Vec::new(),
            pending: Vec::new(),
            view: Viewport::new(),
            pending_edit: None,
            env: None,
            section: Some(section),
        }
    }

    /// The section with `edit` applied, comments and key order intact, ready
    /// for `Request::SetDogConfig`. [`None`] for a sheep pane, for an env
    /// edit, and for a section that does not parse.
    ///
    /// `toml_edit` rather than a re-render of [`Self::values`], and that is
    /// the whole reason this method exists: the request replaces the section
    /// wholesale, so a re-render would delete every comment in it on the
    /// operator's own keystroke.
    ///
    /// A `null` value REMOVES the key, which is how the pane's empty buffer
    /// unsets one, and is what puts the dog back on its own default.
    #[must_use]
    pub fn edited_section(&self, edit: &PaneEdit) -> Option<String> {
        let section = self.section.as_deref()?;
        let PaneEdit::Set { key, value } = edit else {
            return None;
        };
        let mut doc: toml_edit::DocumentMut = section.parse().ok()?;
        match value.as_value() {
            Value::Null => {
                doc.remove(key);
            }
            Value::Bool(flag) => doc[key] = toml_edit::value(*flag),
            // A number that is neither an i64 nor an f64 is not something
            // TOML can hold, so the edit is refused rather than rounded.
            Value::Number(number) => match (number.as_i64(), number.as_f64()) {
                (Some(int), _) => doc[key] = toml_edit::value(int),
                (None, Some(float)) => doc[key] = toml_edit::value(float),
                (None, None) => return None,
            },
            Value::String(text) => doc[key] = toml_edit::value(text.as_str()),
            other => doc[key] = toml_edit::value(other.to_string()),
        }
        Some(doc.to_string())
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
            }) if *key == field.key => Some(value.as_value()),
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
            value: next.into(),
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
        let edit = PaneEdit::Set {
            key,
            value: value.into(),
        };
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

    /// Takes the armed edit out and marks it sent under `ticket`. [`None`]
    /// when nothing is armed, and the pane is left exactly as it was.
    ///
    /// The ticket is the caller's, because the caller is what mints one per
    /// send and puts the same value on the request. See
    /// [`PanePending::Sent`].
    pub fn take_armed(&mut self, ticket: u64) -> Option<PaneEdit> {
        match self.pending_edit.take() {
            Some(PanePending::Armed { edit, text, .. }) => {
                self.pending_edit = Some(PanePending::Sent {
                    ticket,
                    key: edit.key().to_owned(),
                    text,
                });
                Some(edit)
            }
            other => {
                self.pending_edit = other;
                None
            }
        }
    }

    /// Clears the in-flight line for `ticket`, once the shepherd has
    /// answered that write and not another.
    ///
    /// Guarded twice, and both guards were bugs before they were guards.
    ///
    /// It only clears [`PanePending::Sent`], the way [`Self::cancel`] only
    /// clears `Armed`. Unguarded it cleared every variant, so a reply
    /// landing while the operator had an editor open threw the buffer away
    /// with no notice -- and, since nothing reset the input mode, left
    /// every subsequent keystroke dead until `Esc`. On the env screen that
    /// discarded buffer is a secret the operator cannot read back.
    ///
    /// It only clears a `Sent` whose own ticket matches. Two writes can be
    /// in flight at once (send one, arm another, send that), and without
    /// the match the FIRST reply cleared the SECOND's "sent, waiting" line
    /// while the second was still outstanding. The requests themselves
    /// never confused each other -- each names exactly its own key, so
    /// neither can carry the other's value -- but the line on screen did.
    /// Matching on the KEY was the first fix and left the same hole one
    /// step narrower, for two writes to the SAME field.
    pub fn settle(&mut self, ticket: u64) {
        if matches!(&self.pending_edit, Some(PanePending::Sent { ticket: sent, .. }) if *sent == ticket)
        {
            self.pending_edit = None;
        }
    }

    /// Arms an env write from the sub-screen's own editor.
    ///
    /// Env arms like everything else, and the brief's argument for sending
    /// straight from the editor did not survive review: it said an env
    /// change parks until the next spawn, which is about when the change
    /// takes EFFECT. The old value is LOST on the reply, because the
    /// daemon writes the override store on the same call. Under a
    /// write-only screen an overwritten value is exactly as unrecoverable
    /// as a deleted one, so a set arms too, not only a removal.
    pub(super) fn arm_env(&mut self, key: String, value: Option<EnvValue>, now: Instant) {
        let edit = PaneEdit::SetEnv { key, value };
        let text = self.confirm_text(&edit);
        self.pending_edit = Some(PanePending::Armed {
            edit,
            text,
            at: now,
        });
    }

    /// The question an armed edit reads as.
    ///
    /// **It names the field's CLASS, not what this write will do**, and
    /// the two are not the same thing. `apply_group` is a fact about the
    /// field; whether a given write reaches the running child is a fact
    /// about the flock, and only the shepherd has it. A `Live` field parks
    /// when the config subset it would reach cannot normalize on its own,
    /// and `autostart` is `NextSpawn` and yet in force the moment it lands.
    /// This sentence said `web takes it now` about the first of those and
    /// was then contradicted, one keystroke later, by a status line saying
    /// it waits for a reload.
    ///
    /// Three things speak, and only this one is a prediction. The status
    /// bar after the reply reports what the shepherd actually did
    /// ([`super::app::App`]'s own reply handler), and the row's `!` flag is
    /// the durable answer, read off the shepherd's parked-field list on
    /// every refresh.
    ///
    /// `NeedsRespawn` used to be exempted, on the argument that for that
    /// group the prediction cannot be wrong, since nothing carries a
    /// respawn-only field to a child that is already running. That argument
    /// is about the CHILD and the sentence was about the FLOCK, and the two
    /// are not the same claim: `web is respawned to pick it up` was read as
    /// a promise that shep restarts something, and shep restarts nothing.
    /// The daemon parks the field and waits. So this arm names the reload
    /// it waits for, in the words the reply's own status line uses, and no
    /// arm predicts an outcome now.
    ///
    /// An env sentence names its key and NEVER its value, unlike a field
    /// sentence, which quotes what it is setting. The value has just been
    /// typed, so echoing it tells the operator nothing they cannot see, and
    /// it would put a secret into a string that outlives the editor, sits
    /// on the status bar, and rides inside [`PanePending`].
    ///
    /// A field the schema marks `x-shep-secret` gets that same protection,
    /// rendered as the `<set>` its own cell reads rather than as a quoted
    /// credential. Nothing above this reaches it: `begin_typing` seeds the
    /// editor empty for a secret and the cell never holds the value, so
    /// this sentence was the one place a credential got into a string that
    /// outlives the keystroke.
    fn confirm_text(&self, edit: &PaneEdit) -> String {
        let name = self.target.name();
        let (key, value) = match edit {
            PaneEdit::SetEnv {
                key,
                value: Some(_),
            } => {
                return format!("set env {key}? {key} waits for `shep reload {name}`");
            }
            PaneEdit::SetEnv { key, value: None } => {
                return format!(
                    "remove env {key}? it waits for `shep reload {name}`, and lookout cannot \
                     read the value back to put it there again"
                );
            }
            PaneEdit::Set { key, value } => (key, value),
        };
        let secret = self.fields.by_key(key).is_some_and(|field| field.secret);
        let shown = match value.as_value() {
            Value::Null => "(unset)".to_owned(),
            _ if secret => "<set>".to_owned(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        // `ApplyGroup` is `#[non_exhaustive]`, so the wildcard is required
        // rather than chosen, and it answers `respawn` for the reason
        // `view::pane::cost_label` gives: a group this binary has not been
        // taught about promises a restart rather than a silent claim that
        // the change applied. `Structural` cannot reach here -- `cycle` and
        // `begin_typing` both refuse a locked field -- and lands in the
        // same conservative arm if it ever does.
        match self.cost(key) {
            Some(ApplyGroup::Live) => format!("set {key} = {shown}? {key} is a live setting"),
            Some(ApplyGroup::NextSpawn) => {
                format!("set {key} = {shown}? {key} is read when {name} spawns")
            }
            Some(_) => format!("set {key} = {shown}? {key} waits for `shep reload {name}`"),
            None => format!("set {key} = {shown}? {name} is told, and decides what to reload"),
        }
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
            // The dog decides, not shep. Said once at the foot of the pane
            // rather than guessed per row (decision 4).
            PaneTarget::Dog { .. } => None,
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

    /// Adopts a previous pane's in-flight edit, so a refresh does not
    /// silently drop a question the operator has not answered or a write
    /// they are still waiting on.
    ///
    /// [`PanePending::Typing`] is deliberately NOT carried: its buffer was
    /// seeded from a value this refresh may have just changed, so keeping
    /// it would put the operator halfway through editing something that is
    /// no longer there. `App::release_text_mode_if_unowned` is what puts
    /// the keyboard back when that happens.
    pub(super) fn adopt_pending_edit(&mut self, previous: Option<PanePending>) {
        self.pending_edit = match previous {
            Some(carried @ (PanePending::Armed { .. } | PanePending::Sent { .. })) => Some(carried),
            Some(PanePending::Typing { .. }) | None => None,
        };
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
    pub(super) fn adopt_env_view(&mut self, view: Viewport, cursor_key: Option<&str>) {
        let mut env = EnvPane::new(self.env_keys.clone());
        env.adopt_view(view, cursor_key);
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
        assert_eq!(value.as_value(), &serde_json::json!(false));
        assert!(text.contains("autorestart"), "{text}");
    }

    /// fails if the confirm goes back to promising an OUTCOME for a class
    /// of field that does not always have that outcome.
    ///
    /// `apply_group` is a fact about the field and this write's fate is a
    /// fact about the flock, and only the shepherd has the second. The
    /// sentence said `web takes it now` for a `Live` field, and `watch` is
    /// a `Live` field that parks whenever the config subset it would reach
    /// cannot normalize alone -- so the pane promised, and the status line
    /// one keystroke later said the opposite. It names the class now.
    ///
    /// `NeedsRespawn` is the one arm that may still promise, which the test
    /// below asserts: nothing carries a respawn-only field to a child that
    /// is already running, so that prediction cannot be wrong.
    #[test]
    fn a_live_confirm_names_the_class_and_promises_no_outcome() {
        for key in ["autorestart", "watch"] {
            let mut pane = ConfigPane::sheep(web());
            pane.move_to_key(key);
            pane.cycle(Instant::now());
            let Some(PanePending::Armed { text, .. }) = pane.pending_edit() else {
                panic!("{:?}", pane.pending_edit());
            };
            assert_eq!(
                pane.cost(key),
                Some(ApplyGroup::Live),
                "the fixture must keep {key} Live or the test means nothing"
            );
            assert!(text.contains("is a live setting"), "{text}");
            assert!(!text.contains("takes it now"), "{text}");
            assert!(!text.contains("respawn"), "{text}");
            assert!(!text.contains("next start"), "{text}");
        }
    }

    /// fails if a `NextSpawn` confirm goes back to promising that a restart
    /// is what it takes. `autostart` is in that group and is in force the
    /// moment it lands, because the daemon reads it at muster.
    #[test]
    fn a_next_spawn_confirm_names_when_the_field_is_read() {
        let mut pane = ConfigPane::sheep(web());
        pane.move_to_key("autostart");
        pane.cycle(Instant::now());
        let Some(PanePending::Armed { text, .. }) = pane.pending_edit() else {
            panic!("{:?}", pane.pending_edit());
        };
        assert_eq!(pane.cost("autostart"), Some(ApplyGroup::NextSpawn));
        assert!(text.contains("is read when web spawns"), "{text}");
        assert!(!text.contains("respawn"), "{text}");
    }

    /// fails if a respawn-class confirm goes back to promising a respawn
    /// nothing performs. The daemon PARKS such a field and waits, which is
    /// what the status line one keystroke later already said, so the
    /// confirm said the flock would restart and the reply said it had not.
    #[test]
    fn a_respawn_field_arms_a_confirm_that_names_the_reload_it_waits_for() {
        for key in ["merge_logs", "shutdown_with_message"] {
            let mut pane = ConfigPane::sheep(web());
            pane.move_to_key(key);
            pane.cycle(Instant::now());
            let Some(PanePending::Armed { text, .. }) = pane.pending_edit() else {
                panic!("{:?}", pane.pending_edit());
            };
            assert_eq!(
                pane.cost(key),
                Some(ApplyGroup::NeedsRespawn),
                "the fixture must keep {key} NeedsRespawn or the test means nothing"
            );
            assert!(text.contains("waits for `shep reload web`"), "{text}");
            assert!(!text.contains("is respawned"), "{text}");
        }
    }

    /// fails if either env sentence promises a respawn. Env is
    /// `NeedsRespawn` in every case, so both arms carried the same wrong
    /// promise the field arm did, and against the same status line.
    #[test]
    fn an_env_confirm_names_the_reload_it_waits_for_and_never_the_value() {
        for value in [Some("hunter2".to_owned().into()), None] {
            let pane = ConfigPane::sheep(web());
            let text = pane.confirm_text(&PaneEdit::SetEnv {
                key: "DB_PASSWORD".into(),
                value,
            });
            assert!(text.contains("waits for `shep reload web`"), "{text}");
            assert!(!text.contains("respawn"), "{text}");
            assert!(!text.contains("hunter2"), "{text}");
        }
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
        assert_eq!(value.as_value(), &serde_json::json!(40));
    }

    /// fails if a typed edit stops carrying the value the field's kind
    /// wants. The request names one key and one JSON value, and the daemon
    /// deserializes that value into the field it names, so an integer field
    /// handed `"40"` is refused rather than set.
    #[test]
    fn an_edit_carries_the_key_and_the_typed_value_and_nothing_else() {
        let mut pane = ConfigPane::sheep(web());
        pane.move_to_key("max_restarts");
        pane.begin_typing();
        pane.type_backspace();
        pane.type_backspace();
        for c in "40".chars() {
            pane.type_char(c);
        }
        pane.apply_typing(Instant::now());
        let Some(PaneEdit::Set { key, value }) = pane.take_armed(7) else {
            panic!("{:?}", pane.pending_edit());
        };
        assert_eq!(key, "max_restarts");
        assert_eq!(value.as_value(), &serde_json::json!(40));
        assert!(
            !matches!(value.as_value(), serde_json::Value::String(_)),
            "an integer field must not travel as a string"
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

    /// The bark section every dog test below reads: a comment, a scalar,
    /// and a sink carrying a credential.
    fn bark_section() -> String {
        "# how often\npoll = \"60s\"\nhistory_bytes = 4096\n\n[sinks.ops]\nkind = \"slack\"\nurl = \"https://hooks.example/x\"\n".to_owned()
    }

    fn bark_pane() -> ConfigPane {
        let schema = crate::dog::builtin_schema("bark").expect("bark is a built-in");
        ConfigPane::dog("bark".into(), None, schema, bark_section())
    }

    /// fails if a dog pane starts inventing sections for a schema that
    /// declares none (decision 3), if the secret marker stops reaching the
    /// field set, or if shep starts claiming to know what a dog's field
    /// costs (decision 4).
    #[test]
    fn a_dog_pane_is_flat_in_schema_order_and_marks_the_secret() {
        let pane = bark_pane();
        assert!(
            pane.fields()
                .fields()
                .iter()
                .all(|field| field.group.is_none()),
            "a dog's schema carries no group, so the pane draws no headers"
        );
        let keys: Vec<&str> = pane
            .fields()
            .fields()
            .iter()
            .map(|f| f.key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["history_bytes", "poll", "rules", "sink_timeout", "sinks"],
            "schema order, which for a serde_json map is alphabetical"
        );
        assert!(
            pane.fields()
                .by_key("sinks")
                .expect("sinks is a field")
                .secret
        );
        assert_eq!(pane.value("poll"), "60s");
        assert_eq!(pane.cost("poll"), None);
    }

    /// fails if the pane starts offering an editor for a shape it has none
    /// for, which for a dog is the map `sinks` is. It draws
    /// [`Lock::NoWidget`], never [`Lock::Refused`]: shep writes that section
    /// happily, this screen simply has no widget for a table of tables.
    #[test]
    fn a_dogs_map_field_is_locked_for_want_of_a_widget_and_not_refused() {
        let pane = bark_pane();
        assert_eq!(pane.lock("sinks"), Some(Lock::NoWidget));
        assert_eq!(pane.lock("rules"), Some(Lock::NoWidget));
        assert_eq!(pane.lock("poll"), None);
    }

    /// A dog whose schema declares one secret STRING. No built-in has one:
    /// bark's only secret is a map, and a map has no editor to type a
    /// secret into, so the leak the confirm sentence could carry was not
    /// reachable from any fixture the pane already had.
    fn secret_dog_pane() -> ConfigPane {
        let schema = serde_json::json!({
            "properties": {
                "webhook": { "type": "string", "x-shep-secret": true },
            }
        });
        ConfigPane::dog(
            "pydog".into(),
            None,
            schema,
            "webhook = \"https://hook/OLD\"\n".to_owned(),
        )
    }

    /// fails if the confirm sentence quotes a credential. The pane renders
    /// `<set>` for a secret and seeds its editor empty, and the sentence
    /// outlives both: it sits in `PanePending`, prints on the status bar,
    /// and survives `take_armed` for the whole round trip.
    #[test]
    fn a_secret_fields_confirm_never_quotes_what_was_typed() {
        let mut pane = secret_dog_pane();
        pane.move_to_key("webhook");
        pane.begin_typing();
        for typed in "https://hook/T0PS3CRET".chars() {
            pane.type_char(typed);
        }
        pane.apply_typing(Instant::now());
        let Some(PanePending::Armed { text, .. }) = pane.pending_edit() else {
            panic!("{:?}", pane.pending_edit());
        };
        assert!(!text.contains("T0PS3CRET"), "{text}");
        assert!(text.contains("set webhook = <set>?"), "{text}");
    }

    /// fails if a write starts re-rendering the section from the parsed
    /// values, which would delete every comment in a file shep does not
    /// author on the operator's own keystroke.
    #[test]
    fn an_edited_section_keeps_its_comments_and_changes_one_key() {
        let pane = bark_pane();
        let out = pane
            .edited_section(&PaneEdit::Set {
                key: "poll".into(),
                value: serde_json::json!("30s").into(),
            })
            .expect("the fixture section parses");
        assert!(out.contains("# how often"), "{out}");
        assert!(out.contains("poll = \"30s\""), "{out}");
        assert!(out.contains("history_bytes = 4096"), "{out}");
        assert!(out.contains("url = \"https://hooks.example/x\""), "{out}");
    }

    /// fails if an empty buffer stops unsetting a dog's key, which is how
    /// the pane puts one back on the dog's own default.
    #[test]
    fn a_null_edit_removes_the_key_from_the_section() {
        let pane = bark_pane();
        let out = pane
            .edited_section(&PaneEdit::Set {
                key: "history_bytes".into(),
                value: serde_json::Value::Null.into(),
            })
            .expect("the fixture section parses");
        assert!(!out.contains("history_bytes"), "{out}");
        assert!(out.contains("# how often"), "{out}");
    }

    /// fails if a sheep pane grows a section, which would send a sheep's
    /// config out through a dog's door.
    #[test]
    fn a_sheep_pane_has_no_section_to_edit() {
        let pane = ConfigPane::sheep(web());
        assert_eq!(
            pane.edited_section(&PaneEdit::Set {
                key: "cwd".into(),
                value: serde_json::json!("/srv").into(),
            }),
            None
        );
    }

    /// fails if a live config value ever reaches a `{:?}` (IR-41).
    /// fails if a live config value ever reaches a `{:?}` (IR-41).
    ///
    /// `cwd` and `script` routinely hold a home directory and `args` holds
    /// a token, which is why [`ConfigPane`]'s own `Debug` withholds the map
    /// these come out of. This is the same value one hop further on, and it
    /// is asserted on [`FieldValue`] itself rather than on the types that
    /// carry it: `PaneEdit` had a hand-written redaction and
    /// `Sent::ApplyField`, which holds the same value, did not.
    #[test]
    fn a_field_values_debug_names_no_value() {
        for (value, want) in [
            (serde_json::json!("/home/ada/secret-project"), "<string>"),
            (serde_json::json!(40), "<number>"),
            (serde_json::json!(false), "<bool>"),
            (serde_json::json!(null), "<null>"),
            (serde_json::json!(["--token", "hunter2"]), "<array>"),
            (serde_json::json!({ "a": 1 }), "<object>"),
        ] {
            let wrapped: FieldValue = value.into();
            assert_eq!(format!("{wrapped:?}"), format!("FieldValue({want})"));
        }
    }

    /// fails if [`PaneEdit`] starts printing the value it is about to set.
    /// Derived now, and safe only because both value types redact
    /// themselves; this pins that it stays that way.
    #[test]
    fn a_pane_edits_debug_names_no_value() {
        let set = PaneEdit::Set {
            key: "cwd".into(),
            value: serde_json::json!("/home/ada/secret-project").into(),
        };
        assert_eq!(
            format!("{set:?}"),
            r#"Set { key: "cwd", value: FieldValue(<string>) }"#
        );
        let env = PaneEdit::SetEnv {
            key: "DB_PASSWORD".into(),
            value: Some("hunter2".to_owned().into()),
        };
        assert_eq!(
            format!("{env:?}"),
            r#"SetEnv { key: "DB_PASSWORD", value: Some(EnvValue(<7 bytes>)) }"#
        );
    }

    /// fails if [`PanePending`] starts printing a buffer or a rendered
    /// question (IR-41). The buffer is what the operator is halfway
    /// through typing, and on the env screen that is the secret itself;
    /// the question quotes the value a field edit is setting.
    #[test]
    fn a_pane_pendings_debug_names_no_value() {
        let typing = PanePending::Typing {
            key: "cwd".into(),
            buffer: "/home/ada/secret-project".into(),
        };
        assert_eq!(
            format!("{typing:?}"),
            r#"PanePending::Typing { key: "cwd", buffer: <24 chars> }"#
        );
        let armed = PanePending::Armed {
            edit: PaneEdit::Set {
                key: "cwd".into(),
                value: serde_json::json!("/home/ada/secret-project").into(),
            },
            text: "set cwd = /home/ada/secret-project? web takes it now".into(),
            at: Instant::now(),
        };
        assert_eq!(
            format!("{armed:?}"),
            r#"PanePending::Armed { edit: Set { key: "cwd", value: FieldValue(<string>) } }"#
        );
        let sent = PanePending::Sent {
            ticket: 7,
            key: "cwd".into(),
            text: "set cwd = /home/ada/secret-project? web takes it now".into(),
        };
        assert_eq!(
            format!("{sent:?}"),
            r#"PanePending::Sent { ticket: 7, key: "cwd" }"#
        );
    }

    /// fails if [`EnvPane`] starts printing a key name or the buffer
    /// (IR-41). The buffer on the `+ new` row is the whole
    /// `KEY=value`, secret included.
    #[test]
    fn an_env_panes_debug_names_no_key_and_no_value() {
        let mut env = EnvPane::new(vec!["DB_PASSWORD".into(), "API_TOKEN".into()]);
        env.move_to_last();
        env.begin_typing();
        for typed in "STRIPE_KEY=sk_live_1".chars() {
            env.type_char(typed);
        }
        assert_eq!(
            format!("{env:?}"),
            "EnvPane { keys: 2, cursor: 2, typing: true }"
        );
    }

    /// fails if a dog pane's `Debug` starts printing the section it holds
    /// (IR-41). A dog's section is the single most credential-dense thing
    /// this screen ever loads -- bark's own `sinks` table is a webhook URL
    /// with a bearer token in it -- and the redaction is on the pane rather
    /// than on each type that carries the text, which is the rule this
    /// branch settled on after two hand-written redactions missed one.
    #[test]
    fn a_dog_panes_debug_names_no_section() {
        let pane = bark_pane();
        assert_eq!(
            format!("{pane:?}"),
            r#"ConfigPane { target: Dog { name: "bark", adopted_path: None }, fields: 5, env_keys: 0, cursor: 0 }"#
        );
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
