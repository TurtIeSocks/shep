//! A form's shape, read off a JSON Schema.
//!
//! Every config pane in lookout renders one of these. A JSON Schema is
//! already a field list with types, defaults and descriptions, which is
//! exactly what a form needs, so this is the common shape rather than an
//! abstraction invented to share code. The Flockfile schema, a dog's own
//! `--schema` answer, and a hand-built list for `shep.toml` all become a
//! [`FieldSet`], and one renderer draws all three.

use serde_json::{Map, Value};

/// What the widget for one field is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// `type: boolean`. Cycles.
    Bool,
    /// `type: integer`. Typed.
    Integer,
    /// `type: string`, or a `$ref` that resolves to one. Typed.
    Text,
    /// A closed set: `enum`, or `oneOf` of `const`s. Cycles.
    Choice(Vec<String>),
    /// `type: object` with `additionalProperties`. Opens a sub-screen.
    Map,
    /// Anything else, including a nested object. Read-only, shown as JSON.
    Opaque,
}

/// One field of a form.
///
/// `Debug` is derived rather than redacted (IR-41): this is a schema, and a
/// schema describes a value without carrying one. A secret's SHAPE is not a
/// secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The property name, which is also the key a write carries.
    pub key: String,
    /// What the operator reads beside it: `init.blurb`, else `description`,
    /// else the key.
    pub help: String,
    /// `init.group`, where the schema assigns one.
    pub group: Option<String>,
    /// The widget.
    pub kind: FieldKind,
    /// The schema's own `default`, rendered as the pane will show it. `None`
    /// for an absent or `null` default.
    pub default: Option<String>,
    /// `x-shep-secret`. The pane shows `<set>` and never reads the value.
    pub secret: bool,
    /// Whether the pane may edit it. `false` for [`FieldKind::Opaque`], and
    /// for anything a caller marks read-only after the fact.
    pub editable: bool,
}

/// An ordered set of fields, grouped.
///
/// The groups themselves are not stored. A renderer reads each field's own
/// [`Field::group`] as it walks the list, which is what a scrolled window
/// needs anyway: a pane whose top row is the middle of `control` has to
/// draw that header from the row, not from a list of every group the set
/// has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSet {
    fields: Vec<Field>,
}

impl FieldSet {
    /// Reads a schema's `properties`, resolving one level of `$ref` into
    /// `defs`, and orders the result by `group_order`.
    ///
    /// Within a group, fields keep the order `properties` yields them.
    /// `serde_json::Map` without `preserve_order` yields alphabetical, which
    /// is what the Flockfile schema already is on disk. A field whose group
    /// is not in `group_order` sorts after every group that is; a field with
    /// no group sorts last.
    #[must_use]
    pub fn from_properties(
        properties: &Map<String, Value>,
        defs: &Map<String, Value>,
        group_order: &[&str],
    ) -> Self {
        let fields = properties
            .iter()
            .map(|(key, schema)| field_from(key, schema, defs))
            .collect();
        Self::from_fields(fields, group_order)
    }

    /// Orders an already-built list by `group_order`, for a caller that has
    /// no schema (the settings screen builds its six by hand).
    #[must_use]
    pub fn from_fields(mut fields: Vec<Field>, group_order: &[&str]) -> Self {
        let rank = |group: Option<&str>| -> (usize, usize) {
            match group {
                None => (2, 0),
                Some(g) => match group_order.iter().position(|known| *known == g) {
                    Some(i) => (0, i),
                    None => (1, 0),
                },
            }
        };
        // Stable, so within-group order is whatever the caller gave. The
        // sort is also what makes a group CONTIGUOUS, which every renderer
        // relies on: a header is pushed when the group changes, so a group
        // split in two would have its name drawn twice.
        fields.sort_by_key(|f| rank(f.group.as_deref()));
        Self { fields }
    }

    /// Every field, in display order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The field named `key`.
    #[must_use]
    pub fn by_key(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// How many fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Follows one `$ref` of the form `#/$defs/Name` into `defs`.
fn resolve<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .and_then(|name| defs.get(name))
        .unwrap_or(schema)
}

/// `anyOf: [T, {type: null}]` is `T` with the field optional. Anything
/// else is left as it was.
fn strip_nullable<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> &'a Value {
    let Some(arms) = schema.get("anyOf").and_then(Value::as_array) else {
        return schema;
    };
    let non_null: Vec<&Value> = arms
        .iter()
        .filter(|arm| arm.get("type").and_then(Value::as_str) != Some("null"))
        .collect();
    match non_null.as_slice() {
        [one] => resolve(one, defs),
        _ => schema,
    }
}

/// The `type` keyword, which may be a string or a `[T, "null"]` list.
fn type_of(schema: &Value) -> Option<&str> {
    match schema.get("type")? {
        Value::String(s) => Some(s.as_str()),
        Value::Array(arr) => arr.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    }
}

fn kind_of(schema: &Value, defs: &Map<String, Value>) -> FieldKind {
    let schema = strip_nullable(resolve(schema, defs), defs);
    if let Some(consts) = schema.get("oneOf").and_then(Value::as_array) {
        let names: Vec<String> = consts
            .iter()
            .filter_map(|arm| arm.get("const").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        if !names.is_empty() && names.len() == consts.len() {
            return FieldKind::Choice(names);
        }
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let names: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if names.len() == values.len() {
            return FieldKind::Choice(names);
        }
    }
    match type_of(schema) {
        Some("boolean") => FieldKind::Bool,
        Some("integer") => FieldKind::Integer,
        Some("string") => FieldKind::Text,
        Some("object")
            if schema.get("additionalProperties").is_some()
                && schema.get("properties").is_none() =>
        {
            FieldKind::Map
        }
        _ => FieldKind::Opaque,
    }
}

/// Renders a default the way the pane will show the value: bare for a
/// scalar, compact JSON for anything else, `None` for `null`.
fn render_default(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

fn field_from(key: &str, schema: &Value, defs: &Map<String, Value>) -> Field {
    let init = schema.get("init");
    let help = init
        .and_then(|i| i.get("blurb"))
        .or_else(|| schema.get("description"))
        .and_then(Value::as_str)
        .map_or_else(|| key.to_owned(), str::to_owned);
    let group = init
        .and_then(|i| i.get("group"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let kind = kind_of(schema, defs);
    let editable = kind != FieldKind::Opaque;
    Field {
        key: key.to_owned(),
        help,
        group,
        kind,
        default: render_default(schema.get("default")),
        secret: schema
            .get(shep_core::dogs::SECRET_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false),
        editable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    /// The groups the set's fields carry, in the order they first appear.
    /// A group that appeared twice would show up twice, which is the
    /// contiguity a renderer's one-header-per-group rule depends on.
    fn groups_of(set: &FieldSet) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for field in set.fields() {
            if let Some(group) = &field.group
                && seen.last() != Some(group)
            {
                seen.push(group.clone());
            }
        }
        seen
    }

    #[test]
    fn a_bool_an_integer_and_a_string_get_their_kinds() {
        let p = props(json!({
            "watch": { "type": "boolean", "default": false },
            "max_restarts": { "type": "integer", "format": "uint32", "default": 16 },
            "cwd": { "type": ["string", "null"], "default": null },
        }));
        let set = FieldSet::from_properties(&p, &Default::default(), &[]);
        assert_eq!(set.by_key("watch").unwrap().kind, FieldKind::Bool);
        assert_eq!(set.by_key("max_restarts").unwrap().kind, FieldKind::Integer);
        assert_eq!(set.by_key("cwd").unwrap().kind, FieldKind::Text);
    }

    #[test]
    fn a_ref_into_defs_takes_the_named_types_kind() {
        let p = props(json!({
            "kill_timeout": { "$ref": "#/$defs/UpDuration", "default": "1600" },
        }));
        let d = props(json!({
            "UpDuration": { "type": "string", "pattern": "^\\d+(ms|h|m|s)?$" },
        }));
        let set = FieldSet::from_properties(&p, &d, &[]);
        assert_eq!(set.by_key("kill_timeout").unwrap().kind, FieldKind::Text);
        assert_eq!(
            set.by_key("kill_timeout").unwrap().default.as_deref(),
            Some("1600")
        );
    }

    #[test]
    fn any_of_with_null_is_the_other_arm() {
        let p = props(json!({
            "max_memory": {
                "anyOf": [{ "$ref": "#/$defs/MemSize" }, { "type": "null" }],
                "default": null,
            },
        }));
        let d = props(json!({ "MemSize": { "type": "string" } }));
        let set = FieldSet::from_properties(&p, &d, &[]);
        assert_eq!(set.by_key("max_memory").unwrap().kind, FieldKind::Text);
        assert_eq!(set.by_key("max_memory").unwrap().default, None);
    }

    #[test]
    fn one_of_consts_is_a_choice_in_schema_order() {
        let p = props(json!({
            "kind": {
                "oneOf": [
                    { "type": "string", "const": "http" },
                    { "type": "string", "const": "tcp" },
                    { "type": "string", "const": "exec" },
                ],
            },
        }));
        let set = FieldSet::from_properties(&p, &Default::default(), &[]);
        assert_eq!(
            set.by_key("kind").unwrap().kind,
            FieldKind::Choice(vec!["http".into(), "tcp".into(), "exec".into()])
        );
    }

    #[test]
    fn a_string_map_is_a_map_and_a_nested_object_is_opaque() {
        let p = props(json!({
            "env": { "type": "object", "additionalProperties": { "type": "string" } },
            "liveness_probe": {
                "anyOf": [{ "$ref": "#/$defs/ProbeConfig" }, { "type": "null" }],
            },
        }));
        let d = props(json!({
            "ProbeConfig": { "type": "object", "properties": { "kind": {} } },
        }));
        let set = FieldSet::from_properties(&p, &d, &[]);
        assert_eq!(set.by_key("env").unwrap().kind, FieldKind::Map);
        assert_eq!(
            set.by_key("liveness_probe").unwrap().kind,
            FieldKind::Opaque
        );
        assert!(!set.by_key("liveness_probe").unwrap().editable);
    }

    #[test]
    fn help_prefers_the_blurb_then_the_description_then_the_key() {
        let p = props(json!({
            "a": { "type": "boolean", "description": "desc", "init": { "blurb": "blurb" } },
            "b": { "type": "boolean", "description": "desc" },
            "c": { "type": "boolean" },
        }));
        let set = FieldSet::from_properties(&p, &Default::default(), &[]);
        assert_eq!(set.by_key("a").unwrap().help, "blurb");
        assert_eq!(set.by_key("b").unwrap().help, "desc");
        assert_eq!(set.by_key("c").unwrap().help, "c");
    }

    #[test]
    fn fields_sort_by_group_rank_then_schema_order_and_groups_lists_those_present() {
        let p = props(json!({
            "zeta": { "type": "boolean", "init": { "group": "control" } },
            "alpha": { "type": "boolean", "init": { "group": "process" } },
            "beta": { "type": "boolean", "init": { "group": "control" } },
            "nogroup": { "type": "boolean" },
            "odd": { "type": "boolean", "init": { "group": "unknown" } },
        }));
        let set =
            FieldSet::from_properties(&p, &Default::default(), &["process", "inputs", "control"]);
        let keys: Vec<&str> = set.fields().iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["alpha", "beta", "zeta", "odd", "nogroup"]);
        assert_eq!(
            groups_of(&set),
            ["process", "control", "unknown"],
            "and each group is contiguous, so a renderer draws its header once"
        );
    }

    #[test]
    fn the_secret_marker_is_read_off_the_extension_key() {
        let p = props(json!({
            "url": { "type": "string", "x-shep-secret": true },
            "path": { "type": "string" },
        }));
        let set = FieldSet::from_properties(&p, &Default::default(), &[]);
        assert!(set.by_key("url").unwrap().secret);
        assert!(!set.by_key("path").unwrap().secret);
    }

    #[test]
    fn a_default_is_rendered_the_way_the_pane_will_show_it() {
        let p = props(json!({
            "b": { "type": "boolean", "default": true },
            "n": { "type": "integer", "default": 16 },
            "s": { "type": "string", "default": "1s" },
            "l": { "type": "array", "default": [], "items": { "type": "string" } },
        }));
        let set = FieldSet::from_properties(&p, &Default::default(), &[]);
        assert_eq!(set.by_key("b").unwrap().default.as_deref(), Some("true"));
        assert_eq!(set.by_key("n").unwrap().default.as_deref(), Some("16"));
        assert_eq!(set.by_key("s").unwrap().default.as_deref(), Some("1s"));
        assert_eq!(set.by_key("l").unwrap().default.as_deref(), Some("[]"));
    }

    #[test]
    fn the_real_flockfile_schema_yields_thirty_nine_fields_in_four_groups() {
        let schema = shep_core::config::flockfile_schema_json().to_value();
        let defs = schema["$defs"].as_object().unwrap();
        let props = defs["AppConfig"]["properties"].as_object().unwrap();
        let set = FieldSet::from_properties(props, defs, shep_core::config::GROUP_ORDER);
        assert_eq!(set.len(), 39);
        assert_eq!(groups_of(&set), ["process", "inputs", "control", "cron"]);
        assert!(
            set.fields().iter().all(|f| f.group.is_some()),
            "every field carries a group"
        );
        assert_eq!(set.by_key("env").unwrap().kind, FieldKind::Map);
        assert_eq!(set.by_key("autorestart").unwrap().kind, FieldKind::Bool);
    }
}
