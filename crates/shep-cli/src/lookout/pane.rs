//! An open config pane: what it is editing, its fields, and its cursor.
//!
//! The pane is a [`FieldSet`] over one target, plus the values that target
//! currently holds and a [`Viewport`] over the rows. It renders read-only
//! here; the write half is a later task, and every accessor below is what
//! that half will arm against.

use serde_json::{Map, Value};
use shep_core::config::{ApplyGroup, GROUP_ORDER, apply_group, flockfile_schema_json};
use shep_core::protocol::SheepConfigView;

use super::field::FieldSet;
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
    pending: Vec<String>,
    view: Viewport,
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
