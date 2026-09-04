# The sheep and dog config panes: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shep lookout` gains a config pane for a sheep and one for a dog, both rendering a JSON Schema through one shared field model and one shared viewport.

**Architecture:** #124's settings screen is hardcoded to `shep.toml` across roughly thirty sites and has no scrolling. Slice 1a adds a `FieldSet` read off a JSON Schema and a `Viewport`, and moves the settings screen onto both with its seven snapshots as the proof nothing changed. Slice 1b adds three wire requests under one `PROTOCOL_VERSION` bump and the sheep pane. Slice 2 moves the config lock into shep-core so the daemon can write `dogs.toml`, and builds the dog pane.

**Tech Stack:** Rust 1.88, edition 2024. ratatui, insta snapshots, schemars 1.2.2, serde_json, toml_edit, tokio.

**Spec:** [docs/brainstorming/specs/2026-09-04-config-panes-design.md](../../brainstorming/specs/2026-09-04-config-panes-design.md). Read it before any task. "Decision N" below means that document's numbered decision.

## Global Constraints

- Clean-room rule, non-negotiable: never open, read, or port source from any pm2 checkout on this machine.
- Invoke the `shep-idiomatic-rust` skill before writing or reviewing any Rust. Cite rules as `IR-<n>`.
- **Every commit subject is a conventional commit.** `type(scope): summary`, and `!` after the scope on the commit that breaks something, in the crate that breaks. `.githooks/commit-msg` and `.github/workflows/commits.yml` enforce it. Accepted types: `feat fix perf refactor docs test ci chore style`. `revert` and `build` are refused. Enable the hook once per worktree: `git config core.hooksPath .githooks`.
- No em dashes or en dashes anywhere: prose, code comments, commit messages. Comma, colon, period or parentheses.
- Never write a real person's name, a personal email, or an absolute home-directory path into a committed file or a commit message. Repo-relative paths only.
- Every new public item needs docs and a deliberate `Debug` decision, redacted for anything carrying env or a secret, with an exact-string test (IR-41).
- Every `# Errors` section names each variant. Every `# Panics` has `#[track_caller]`.
- Prove every new test non-vacuous: mutate what it protects, watch that one test go red, grep the file to confirm the patch applied, restore.
- **One cargo shape per task.** The inner loop below, and nothing else, until the task gate at the end.
- **Snippets below that quote existing code are guesses.** They were read on 2026-09-04 and may have moved. Grep before relying on one. If the code differs, follow the code and say so in your report.

## Commands

The inner loop. The only cargo shape a task uses until its final gate:

```
cargo test -p shep --lib --all-features -- --skip ::slow::
```

Tasks 4, 7 and 8 touch shep-core or shep-daemon, so their inner loop is:

```
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

The task gate, once, each command from its own invocation with `$?` captured directly and never through a pipe:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Task 9 changes what an operator sees, so it also runs:

```
cargo build --release
./web/scripts/generate-cli-reference.sh
cd web && npx astro build
cd web && npx astro check
```

## File structure

New files, one responsibility each:

| file | owns |
| --- | --- |
| `crates/shep-cli/src/lookout/field.rs` | `Field`, `FieldKind`, `FieldSet`: a form's shape, read off a JSON Schema |
| `crates/shep-cli/src/lookout/viewport.rs` | `Viewport`: a cursor that knows what is on screen |
| `crates/shep-cli/src/lookout/pane.rs` | `ConfigPane`, `PaneTarget`, `PaneEdit`, `PanePending`: the state of an open sheep or dog pane |
| `crates/shep-cli/src/lookout/view/pane.rs` | drawing a `ConfigPane` |
| `crates/shep-core/src/config_lock.rs` | `ConfigLock` and `create_config_file`, moved from shep-cli |

Modified, and why:

| file | change |
| --- | --- |
| `crates/shep-cli/src/lookout/app.rs` | `Settings` embeds `Viewport` and a `FieldSet`; `App` gains `config_pane` and `note_body_rows`; new `Effect`/`Msg` variants |
| `crates/shep-cli/src/lookout/input.rs` | `e` maps to `KeyPress::Edit` |
| `crates/shep-cli/src/lookout/view/settings.rs` | rows, labels and sections come off the `FieldSet`; `draw_settings` honours the viewport |
| `crates/shep-cli/src/lookout/view/mod.rs` | dispatches to `draw_pane`; exposes `body_rows` |
| `crates/shep-cli/src/lookout/mod.rs` | the event loop feeds terminal size to `App::note_body_rows` and runs the new effects |
| `crates/shep-cli/src/commands/settings.rs` | `SettingField::key` and `from_key`; `settings_field_set()` |
| `crates/shep-cli/src/commands/shep_toml.rs` | re-exports `ConfigLock` from shep-core |
| `crates/shep-cli/src/commands/dogs.rs` | `ask_schema` becomes `pub(crate)` |
| `crates/shep-cli/src/dog/mod.rs` | `builtin_schema(name)` |
| `crates/shep-core/src/protocol/request.rs` | three `Request` variants, three `Response` variants, `SheepConfigView` |
| `crates/shep-core/src/protocol/mod.rs` | `PROTOCOL_VERSION` 3 to 4 |
| `crates/shep-core/src/config/scaffold.rs` | `GROUP_ORDER` becomes `pub`; a stale comment is corrected |
| `crates/shep-daemon/src/supervisor.rs` | `Command::SheepConfig`, `Command::SetSheepEnv`, their handlers |
| `crates/shep-daemon/src/rpc.rs` | three dispatch arms |
| `crates/shep-daemon/src/dogs.rs` | `set_dog_section` |

## The regression surface

Seven full-frame snapshots cover the settings screen. A layout change anywhere in `content_lines` re-diffs all of them, which is what makes them a real gate:

| snapshot | what it pins |
| --- | --- |
| `settings_fresh` | every scalar sourced from the default |
| `settings_set` | sources mixed between `shep.toml` and the default |
| `settings_confirm` | an armed confirm, at width 180 |
| `settings_typing` | the free-text editor open on `socket` |
| `settings_dogs` | three dogs, each drifting differently |
| `settings_narrow` | 45x24, the middle tier of both column tables |
| `settings_at_a_comfortable_width` | 120x30 through `view::draw` |

**Tasks 2 and 3 must leave all seven byte-identical.** `git diff --stat crates/shep-cli/src/lookout/snapshots crates/shep-cli/src/lookout/view/snapshots` printing nothing is the acceptance criterion. A diff means the generalisation changed behaviour, which is the one thing it must not do.

---

# Slice 1a: the field model and scrolling

Lands alone. No new pane, no wire change, no user-visible behaviour. The seven snapshots are the whole proof.

### Task 1: `FieldSet`, a form's shape read off a JSON Schema

**Files:**
- Create: `crates/shep-cli/src/lookout/field.rs`
- Modify: `crates/shep-cli/src/lookout/mod.rs` (add `pub mod field;`)
- Modify: `crates/shep-core/src/config/scaffold.rs:124` (`GROUP_ORDER` becomes `pub`), `:120-121` (stale comment)
- Modify: `crates/shep-core/src/config/mod.rs` (re-export `GROUP_ORDER`)

**Interfaces:**
- Consumes: nothing.
- Produces, all in `lookout::field`:
  ```rust
  pub struct Field {
      pub key: String,
      pub help: String,
      pub group: Option<String>,
      pub kind: FieldKind,
      pub default: Option<String>,
      pub secret: bool,
      pub editable: bool,
  }

  pub enum FieldKind {
      Bool,
      Integer,
      Text,
      Choice(Vec<String>),
      Map,
      Opaque,
  }

  pub struct FieldSet { /* private */ }

  impl FieldSet {
      pub fn from_properties(
          properties: &serde_json::Map<String, serde_json::Value>,
          defs: &serde_json::Map<String, serde_json::Value>,
          group_order: &[&str],
      ) -> Self;
      pub fn from_fields(fields: Vec<Field>, group_order: &[&str]) -> Self;
      pub fn fields(&self) -> &[Field];
      pub fn groups(&self) -> &[String];
      pub fn by_key(&self, key: &str) -> Option<&Field>;
      pub fn len(&self) -> usize;
      pub fn is_empty(&self) -> bool;
  }
  ```
  And in `shep_core::config`: `pub const GROUP_ORDER: &[&str]`.

`Field` derives `Debug, Clone, PartialEq, Eq`. It carries a schema, never a value, so nothing in it is secret and a derived `Debug` is correct (IR-41). Say so in its doc.

`from_properties` orders fields by group rank in `group_order`, then by the order `properties` yields them. `serde_json::Map` without the `preserve_order` feature yields alphabetical, which is what `flockfile.schema.json` already is on disk and what `scaffold.rs`'s `grouped_order` already produces. A field whose group is not in `group_order` sorts after every group that is. A field with no group sorts last. `groups()` is the ordered list of groups actually present.

- [ ] **Step 1: make `GROUP_ORDER` reachable and correct its neighbour.**

`crates/shep-core/src/config/scaffold.rs:124` reads (guess, grep it):

```rust
const GROUP_ORDER: &[&str] = &["process", "inputs", "control", "cron"];
```

Make it `pub const`, and add `pub use scaffold::GROUP_ORDER;` to `crates/shep-core/src/config/mod.rs` beside the existing `flockfile_schema_json` re-export. Then fix the comment at `scaffold.rs:120-121`, which says "half of `AppConfig` is currently ungrouped". Measured against the exported schema on 2026-09-04: 39 of 39 fields carry a group. Rewrite the two lines to say so, keeping the sentence about the fallback order.

Run: `cargo test -p shep-core --lib`
Expected: PASS, nothing changed behaviour.

Commit: `docs(core): GROUP_ORDER is public, and every field has a group`

- [ ] **Step 2: write the failing tests.**

```rust
// crates/shep-cli/src/lookout/field.rs

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
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
        assert_eq!(set.by_key("kill_timeout").unwrap().default.as_deref(), Some("1600"));
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
        assert_eq!(set.by_key("liveness_probe").unwrap().kind, FieldKind::Opaque);
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
        let set = FieldSet::from_properties(&p, &Default::default(), &["process", "inputs", "control"]);
        let keys: Vec<&str> = set.fields().iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["alpha", "beta", "zeta", "odd", "nogroup"]);
        assert_eq!(set.groups(), ["process", "control", "unknown"]);
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
        assert_eq!(set.groups(), ["process", "inputs", "control", "cron"]);
        assert!(set.fields().iter().all(|f| f.group.is_some()), "every field carries a group");
        assert_eq!(set.by_key("env").unwrap().kind, FieldKind::Map);
        assert_eq!(set.by_key("autorestart").unwrap().kind, FieldKind::Bool);
    }
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::field`
Expected: FAIL, `FieldSet` not found.

- [ ] **Step 3: the model.**

```rust
// crates/shep-cli/src/lookout/field.rs

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSet {
    fields: Vec<Field>,
    groups: Vec<String>,
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
        // Stable, so within-group order is whatever the caller gave.
        fields.sort_by_key(|f| rank(f.group.as_deref()));
        let mut groups: Vec<String> = Vec::new();
        for field in &fields {
            if let Some(g) = &field.group
                && !groups.contains(g)
            {
                groups.push(g.clone());
            }
        }
        Self { fields, groups }
    }

    /// Every field, in display order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The groups present, in display order. Empty for an ungrouped schema.
    #[must_use]
    pub fn groups(&self) -> &[String] {
        &self.groups
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
        Value::Array(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null"),
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
        Some("object") if schema.get("additionalProperties").is_some()
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
```

`shep_core::dogs::SECRET_KEY` is `"x-shep-secret"` (guess: grep `SECRET_KEY` in `crates/shep-core/src/dogs.rs`). Use the constant, not the string, for the reason the dog contract's own docs give: a transposed letter compiles.

`schema.to_value()` on a `schemars::Schema`: guess. If the method is named differently in schemars 1.2.2, `serde_json::to_value(&schema).unwrap()` is the fallback.

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::field`
Expected: PASS, 10 tests.

- [ ] **Step 4: prove two tests non-vacuous.**

Mutate `rank` so a `None` group returns `(0, 0)`; confirm with `grep -n '(0, 0)' crates/shep-cli/src/lookout/field.rs`; run; `fields_sort_by_group_rank_then_schema_order_and_groups_lists_those_present` must fail. Restore. Then mutate `kind_of`'s `"boolean"` arm to return `FieldKind::Text`; confirm; run; `a_bool_an_integer_and_a_string_get_their_kinds` and the real-schema test must both fail. Restore, and confirm with the same grep that the mutation is gone.

- [ ] **Step 5: commit.**

```bash
git add crates/shep-cli/src/lookout/field.rs crates/shep-cli/src/lookout/mod.rs
git commit -m "feat(lookout): a field model read off a JSON Schema"
```

### Task 2: `Viewport`, and the settings screen learns to scroll

**Files:**
- Create: `crates/shep-cli/src/lookout/viewport.rs`
- Modify: `crates/shep-cli/src/lookout/mod.rs` (add `pub mod viewport;`; the event loop calls `App::note_body_rows`)
- Modify: `crates/shep-cli/src/lookout/app.rs:642-653` (`Settings` embeds `Viewport`), `:911-930` (`cursor`, `move_by`, `move_to_first`, `move_to_last`)
- Modify: `crates/shep-cli/src/lookout/view/mod.rs` (`pub fn body_rows(area: Rect) -> u16`)
- Modify: `crates/shep-cli/src/lookout/view/settings.rs:548` (`content_lines`), `:660` (`draw_settings`)

**Interfaces:**
- Consumes: nothing.
- Produces, in `lookout::viewport`:
  ```rust
  pub struct Viewport { /* private */ }

  impl Viewport {
      pub fn new() -> Self;
      pub fn cursor(&self) -> usize;
      pub fn offset(&self) -> usize;
      pub fn rows(&self) -> usize;
      pub fn set_rows(&mut self, rows: usize);
      pub fn move_by(&mut self, delta: isize, len: usize);
      pub fn move_to(&mut self, index: usize, len: usize);
      pub fn clamp(&mut self, len: usize);
      pub fn hidden_above(&self) -> usize;
      pub fn hidden_below(&self, len: usize) -> usize;
  }
  ```
  On `App`: `pub fn note_body_rows(&mut self, rows: u16)`.
  In `lookout::view`: `pub fn body_rows(area: Rect) -> u16`.

Decision 8b. Today `draw_settings` takes `area.height` lines off the front of `content_lines()` with no skip, `Settings` holds `cursor: usize` and nothing else positional, and `move_by` clamps to `rows().len()` with no notion of what was drawn.

`rows == 0` means "unknown", and a `Viewport` that does not know its height never scrolls, so a `Settings` built in a test with no terminal behaves exactly as today. That is what keeps the seven snapshots identical without every fixture learning a height.

- [ ] **Step 1: write the failing tests for the viewport alone.**

```rust
// crates/shep-cli/src/lookout/viewport.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewport_that_does_not_know_its_height_never_scrolls() {
        let mut v = Viewport::new();
        v.move_by(50, 100);
        assert_eq!(v.cursor(), 50);
        assert_eq!(v.offset(), 0);
    }

    #[test]
    fn moving_past_the_bottom_pulls_the_offset_so_the_cursor_is_the_last_visible_row() {
        let mut v = Viewport::new();
        v.set_rows(10);
        v.move_by(15, 100);
        assert_eq!(v.cursor(), 15);
        assert_eq!(v.offset(), 6, "rows 6..=15 are visible");
    }

    #[test]
    fn moving_back_above_the_top_pulls_the_offset_to_the_cursor() {
        let mut v = Viewport::new();
        v.set_rows(10);
        v.move_by(30, 100);
        v.move_by(-28, 100);
        assert_eq!(v.cursor(), 2);
        assert_eq!(v.offset(), 2);
    }

    #[test]
    fn the_cursor_clamps_to_the_list_rather_than_wrapping() {
        let mut v = Viewport::new();
        v.set_rows(10);
        v.move_by(-5, 100);
        assert_eq!(v.cursor(), 0);
        v.move_by(500, 100);
        assert_eq!(v.cursor(), 99);
        assert_eq!(v.offset(), 90);
    }

    #[test]
    fn an_empty_list_leaves_the_cursor_and_offset_at_zero() {
        let mut v = Viewport::new();
        v.set_rows(10);
        v.move_by(3, 0);
        assert_eq!((v.cursor(), v.offset()), (0, 0));
    }

    #[test]
    fn hidden_counts_say_how_much_is_off_screen_either_side() {
        let mut v = Viewport::new();
        v.set_rows(10);
        v.move_to(45, 100);
        assert_eq!(v.hidden_above(), v.offset());
        assert_eq!(v.hidden_below(100), 100 - v.offset() - 10);
        assert_eq!(v.hidden_above() + 10 + v.hidden_below(100), 100);
    }

    #[test]
    fn shrinking_the_list_under_the_cursor_clamps_it_back() {
        let mut v = Viewport::new();
        v.set_rows(10);
        v.move_to(45, 100);
        v.clamp(20);
        assert_eq!(v.cursor(), 19);
        assert_eq!(v.offset(), 10);
    }

    #[test]
    fn a_shorter_terminal_brings_the_cursor_back_into_view() {
        let mut v = Viewport::new();
        v.set_rows(30);
        v.move_to(25, 100);
        assert_eq!(v.offset(), 0);
        v.set_rows(10);
        assert_eq!(v.offset(), 16, "the cursor is still the last visible row");
    }
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::viewport`
Expected: FAIL, `Viewport` not found.

- [ ] **Step 2: the viewport.**

```rust
// crates/shep-cli/src/lookout/viewport.rs

//! A cursor that knows what is on screen.
//!
//! lookout's screens used to hold a bare `cursor: usize` and draw the first
//! `height` lines, which works while every screen fits a terminal. A config
//! pane does not: a sheep has 39 fields under four headers, and a 30-line
//! terminal shows a quarter of them. This is the offset and the
//! scroll-into-view that a bare index never had.
//!
//! A viewport that does not know its height (`rows == 0`) never scrolls,
//! so a screen built in a test with no terminal behaves exactly as it did
//! before this existed. That is deliberate: it is what lets the settings
//! screen's seven snapshots stay byte-identical without every fixture
//! learning a height.

/// A cursor, an offset, and the number of rows the terminal shows.
///
/// `Debug` is derived (IR-41): three integers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewport {
    cursor: usize,
    offset: usize,
    rows: usize,
}

impl Viewport {
    /// At the top, height unknown.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The selected row's index into the list.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The first row that is drawn.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// How many rows the terminal shows, or zero if nobody has said.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Records the terminal's height, and pulls the cursor back into view
    /// if the terminal shrank under it.
    pub fn set_rows(&mut self, rows: usize) {
        self.rows = rows;
        self.ensure_visible();
    }

    /// Moves by `delta`, clamped to `0..len` rather than wrapping, the same
    /// rule the flock table follows.
    pub fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.cursor = 0;
            self.offset = 0;
            return;
        }
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, len as isize - 1) as usize;
        self.ensure_visible();
    }

    /// Jumps to `index`, clamped.
    pub fn move_to(&mut self, index: usize, len: usize) {
        if len == 0 {
            self.cursor = 0;
            self.offset = 0;
            return;
        }
        self.cursor = index.min(len - 1);
        self.ensure_visible();
    }

    /// Clamps to a list that may have shrunk since the last move.
    pub fn clamp(&mut self, len: usize) {
        self.move_by(0, len);
    }

    /// Rows above the first drawn one.
    #[must_use]
    pub fn hidden_above(&self) -> usize {
        self.offset
    }

    /// Rows below the last drawn one. Zero when the height is unknown.
    #[must_use]
    pub fn hidden_below(&self, len: usize) -> usize {
        if self.rows == 0 {
            return 0;
        }
        len.saturating_sub(self.offset + self.rows)
    }

    fn ensure_visible(&mut self) {
        if self.rows == 0 {
            return;
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.rows {
            self.offset = self.cursor + 1 - self.rows;
        }
    }
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::viewport`
Expected: PASS, 8 tests.

- [ ] **Step 3: `Settings` embeds it.**

In `app.rs`, the struct at `:642` (guess) has `cursor: usize`. Replace that field with `view: Viewport`, and update:

```rust
pub fn cursor(&self) -> Option<SettingsRow> {
    let rows = self.rows();
    rows.get(self.view.cursor().min(rows.len().saturating_sub(1))).copied()
}

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
pub fn view(&self) -> &Viewport {
    &self.view
}

/// Records the terminal's height.
pub fn set_rows(&mut self, rows: usize) {
    self.view.set_rows(rows);
}
```

`Msg::Settings`'s reload arm at `app.rs:1494-1554` (guess) preserves the cursor index across a re-read. It now preserves the whole `Viewport`: read `self.settings.as_ref().map(|s| s.view.clone())` before rebuilding, put it back after, then `clamp(rows.len())`.

- [ ] **Step 4: the terminal's height reaches the screen.**

`view/mod.rs::draw` computes a `body` `Rect` between the title and the status bar (guess: around lines 158-220; grep `let body`). Extract that arithmetic into `pub fn body_rows(area: Rect) -> u16` and use it in both places.

In `lookout/mod.rs`'s event loop, wherever the terminal size is known before a draw (guess: grep `terminal.size()` or `frame.area()`), call `app.note_body_rows(view::body_rows(area))` before `draw`.

On `App`:

```rust
/// Tells every scrollable screen how tall the body is. Called by the
/// event loop before each draw, so a screen's cursor never lands on a row
/// that was not rendered.
pub fn note_body_rows(&mut self, rows: u16) {
    if let Some(settings) = self.settings.as_mut() {
        settings.set_rows(usize::from(rows));
    }
}
```

- [ ] **Step 5: the renderer honours the viewport.**

`content_lines` at `view/settings.rs:548` (guess) walks `settings.rows()` and emits a header line when the section changes, a blank line between sections, and one line per row. It must now:

1. Skip rows with index below `settings.view().offset()`.
2. Stop after emitting `settings.view().rows()` row lines, when `rows() > 0`.
3. Always emit the section header for the first visible row, even mid-section, so a scrolled view is labelled.
4. Append `  ... N above` before the first line when `hidden_above() > 0` and `  ... N below` after the last when `hidden_below(len) > 0`, both in `palette.muted()`.

None of the seven snapshots scroll, so at `offset == 0` and `rows == 0` the output is unchanged by construction. `draw_settings` itself needs no change: the skip happens in `content_lines`.

- [ ] **Step 6: a rendered test that scrolls.**

In `view/settings.rs`'s `mod tests`, using the existing `fixtures::app_in_settings()` (guess: grep for it) which yields six scalars plus dogs:

```rust
#[test]
fn a_short_terminal_scrolls_and_says_what_it_hid() {
    let mut app = crate::lookout::view::fixtures::app_in_settings();
    app.note_body_rows(4);
    // Walk to the last row. Six scalars plus however many dogs the fixture
    // carries; SelectLast lands on the last one whatever the count.
    app.on_key(KeyPress::SelectLast);
    let settings = app.settings().unwrap();
    let lines = content_lines(&app, settings, app.palette(), 120);
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert!(text[0].contains("above"), "{text:?}");
    assert!(text.iter().any(|l| l.contains("[dogs]")), "the visible section is labelled");
    let last_row = settings.rows().len() - 1;
    assert_eq!(settings.view().cursor(), last_row);
    assert!(settings.view().offset() > 0);
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::view::settings`
Expected: PASS.

- [ ] **Step 7: the seven snapshots are byte-identical.**

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: settings`
Expected: PASS, no snapshot diffs.

Run: `git diff --stat crates/shep-cli/src/lookout/snapshots crates/shep-cli/src/lookout/view/snapshots`
Expected: no output. **This is the acceptance criterion for the task.** A `.snap.new` file anywhere means stop and find out what moved.

- [ ] **Step 8: prove non-vacuous.** Mutate `ensure_visible` to an empty body; grep to confirm; run; the scrolling render test and `moving_past_the_bottom_...` must fail. Restore.

- [ ] **Step 9: commit.**

```bash
git add crates/shep-cli/src/lookout/viewport.rs crates/shep-cli/src/lookout/mod.rs crates/shep-cli/src/lookout/app.rs crates/shep-cli/src/lookout/view/mod.rs crates/shep-cli/src/lookout/view/settings.rs
git commit -m "feat(lookout): a viewport, so a screen can hold more rows than the terminal"
```

### Task 3: the settings screen's rows come off a `FieldSet`

**Files:**
- Modify: `crates/shep-cli/src/commands/settings.rs:66-90` (`SettingField` gains `key` and `from_key`; new `settings_field_set`)
- Modify: `crates/shep-cli/src/lookout/app.rs:642` (`Settings` holds a `FieldSet`), `:889` (`rows`)
- Modify: `crates/shep-cli/src/lookout/view/settings.rs:324` (`field_label`), `:525` (`section_for`)

**Interfaces:**
- Consumes: `FieldSet`, `Field`, `FieldKind` from Task 1.
- Produces, on `SettingField`:
  ```rust
  pub fn key(self) -> &'static str;
  pub fn from_key(key: &str) -> Option<Self>;
  ```
  and in `commands::settings`:
  ```rust
  pub fn settings_field_set() -> FieldSet;
  ```
  and on `Settings`: `pub fn fields(&self) -> &FieldSet`.

This is the smallest cut that makes decision 1 true. `SettingField` stays as the WRITE key, because `Pending`, `SettingEdit`, `set_field`, `unset_field`, `next_candidate`, `current_value`, `source_of`, `text_seed` and `confirm_text` are all keyed on it and all correct. What moves onto the `FieldSet` is what a generic pane also needs: the row list, the label, and the section. `apply_cost` stays a `SettingField` match, because a sheep's cost comes from `apply_group` and a dog has none (decision 4), so cost is never the model's to know.

`Pending` is left alone, and so is the dashboard's `Action`/`Stage`. lookout has two confirm mechanisms sharing only `CONFIRM_EXPIRY`. Unifying them is real work and is out of this plan's scope. **Say in your report that you saw both and left them**, so the reviewer sees a decision rather than an oversight.

- [ ] **Step 1: write the failing tests.**

```rust
// crates/shep-cli/src/commands/settings.rs, in mod tests

#[test]
fn every_setting_field_round_trips_through_its_key() {
    for field in [
        SettingField::LogLevel,
        SettingField::LogJson,
        SettingField::Socket,
        SettingField::MaxCronSleep,
        SettingField::AllowControl,
        SettingField::StyleLevel,
    ] {
        assert_eq!(SettingField::from_key(field.key()), Some(field));
    }
    assert_eq!(SettingField::from_key("no_such_key"), None);
}

#[test]
fn the_settings_field_set_lists_the_six_scalars_in_the_screens_fixed_order() {
    let set = settings_field_set();
    let keys: Vec<&str> = set.fields().iter().map(|f| f.key.as_str()).collect();
    assert_eq!(
        keys,
        ["log_level", "log_json", "socket", "max_cron_sleep", "allow_control", "level"]
    );
    assert_eq!(set.groups(), ["[daemon]", "[whistle]", "[style]"]);
}

#[test]
fn the_cycled_scalars_are_choices_and_the_typed_ones_are_text() {
    let set = settings_field_set();
    assert!(matches!(set.by_key("log_level").unwrap().kind, FieldKind::Choice(_)));
    assert_eq!(set.by_key("log_json").unwrap().kind, FieldKind::Bool);
    assert_eq!(set.by_key("socket").unwrap().kind, FieldKind::Text);
    assert_eq!(set.by_key("max_cron_sleep").unwrap().kind, FieldKind::Text);
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: commands::settings`
Expected: FAIL, `key`, `from_key`, `settings_field_set` not found.

- [ ] **Step 2: `SettingField::key` and `from_key`.**

```rust
impl SettingField {
    /// The TOML key, which is also what a [`Field::key`] carries.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::LogLevel => "log_level",
            Self::LogJson => "log_json",
            Self::Socket => "socket",
            Self::MaxCronSleep => "max_cron_sleep",
            Self::AllowControl => "allow_control",
            Self::StyleLevel => "level",
        }
    }

    /// The inverse of [`Self::key`]. `None` for a key no scalar has.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "log_level" => Self::LogLevel,
            "log_json" => Self::LogJson,
            "socket" => Self::Socket,
            "max_cron_sleep" => Self::MaxCronSleep,
            "allow_control" => Self::AllowControl,
            "level" => Self::StyleLevel,
            _ => return None,
        })
    }
}
```

The `level` key for `StyleLevel` is a guess: check what `field_label` at `view/settings.rs:324` returns for it today and use exactly that string, because the label is what the snapshots pin.

- [ ] **Step 3: `settings_field_set`.**

```rust
/// The six scalars as a [`FieldSet`], in the order the screen has always
/// shown them, grouped by their section.
///
/// Hand-built rather than read off a schema, because `shep.toml` has none.
/// That is the point of the model: it is the common shape, not the schema.
/// The choices for `log_level` and `level` are the ladders
/// `Settings::next_candidate` already cycles (guess: `LOG_LEVEL_ORDER`
/// and `STYLE_LEVEL_ORDER` in `lookout/app.rs`; grep and import them).
#[must_use]
pub fn settings_field_set() -> FieldSet {
    use crate::lookout::field::{Field, FieldKind, FieldSet};
    let f = |field: SettingField, group: &str, kind: FieldKind| Field {
        key: field.key().to_owned(),
        help: field.key().to_owned(),
        group: Some(group.to_owned()),
        kind,
        default: None,
        secret: false,
        editable: true,
    };
    let levels = |names: &[&str]| FieldKind::Choice(names.iter().map(|s| (*s).to_owned()).collect());
    FieldSet::from_fields(
        vec![
            f(SettingField::LogLevel, "[daemon]", levels(LOG_LEVEL_ORDER)),
            f(SettingField::LogJson, "[daemon]", FieldKind::Bool),
            f(SettingField::Socket, "[daemon]", FieldKind::Text),
            f(SettingField::MaxCronSleep, "[daemon]", FieldKind::Text),
            f(SettingField::AllowControl, "[whistle]", FieldKind::Bool),
            f(SettingField::StyleLevel, "[style]", levels(STYLE_LEVEL_ORDER)),
        ],
        &["[daemon]", "[whistle]", "[style]"],
    )
}
```

- [ ] **Step 4: `Settings` holds it, and `rows` reads it.**

Add `fields: FieldSet` to `Settings`, set in `Settings::new` from `settings_field_set()`. Then `rows` at `app.rs:889` (guess):

```rust
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
pub fn fields(&self) -> &FieldSet {
    &self.fields
}
```

- [ ] **Step 5: `field_label` and `section_for` read the model.**

At `view/settings.rs:324` and `:525` (guesses), replace the two `match field` blocks:

```rust
fn field_label(settings: &Settings, field: SettingField) -> &str {
    settings
        .fields()
        .by_key(field.key())
        .map_or(field.key(), |f| f.key.as_str())
}

fn section_for(settings: &Settings, field: SettingField) -> &str {
    settings
        .fields()
        .by_key(field.key())
        .and_then(|f| f.group.as_deref())
        .unwrap_or("")
}
```

Both callers gain a `settings` argument. `apply_cost` and `scalar_view` are untouched.

- [ ] **Step 6: the seven snapshots are byte-identical.**

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: settings`
Expected: PASS, no diffs.

Run: `git diff --stat crates/shep-cli/src/lookout/snapshots crates/shep-cli/src/lookout/view/snapshots`
Expected: no output. Acceptance criterion.

- [ ] **Step 7: prove non-vacuous.** In `settings_field_set`, swap the first two entries; grep; run; `the_settings_field_set_lists_...` fails AND at least one snapshot re-diffs (the order moved on screen). Restore, confirm no `.snap.new` remains.

- [ ] **Step 8: commit.**

```bash
git add crates/shep-cli/src/commands/settings.rs crates/shep-cli/src/lookout/app.rs crates/shep-cli/src/lookout/view/settings.rs
git commit -m "refactor(lookout): the settings screen reads its rows off a field model"
```

---

# Slice 1b: the wire and the sheep pane

### Task 4: three requests and the protocol bump

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs:331` (beside `ApplyConfig`), `:1538` (beside `Applied`), `:2269-2570` and `:2574-2950` (the two wire fixtures), `:3126` and `:3144` (the two protocol-pinning tests)
- Modify: `crates/shep-core/src/protocol/mod.rs:62`
- Modify: `crates/shep-daemon/src/supervisor.rs:442` (`Command`), `:976` (`SupervisorHandle`), `:5748` (beside `handle_apply_config`)
- Modify: `crates/shep-daemon/src/rpc.rs:529` (beside the `DogConfig` arm)
- Modify: `crates/shep-core/src/protocol/snapshots/shep_core__protocol__request__tests__request_wire_v3.snap` and `reply_wire_v3.snap` (renamed to `_v4` by the bump, see step 8)

**Interfaces:**
- Consumes: nothing.
- Produces, in `shep_core::protocol`:
  ```rust
  // Request
  SheepConfig { name: String },
  SetSheepEnv { name: String, key: String, value: Option<String> },
  SetDogConfig { name: String, toml: DogSectionToml },

  // Response
  SheepConfig(SheepConfigView),
  SheepEnvSet { name: String, key: String },
  DogConfigSet { name: String },

  pub struct SheepConfigView {
      pub name: String,
      pub config: AppConfig,     // env emptied
      pub env_keys: Vec<String>,
      pub overridden: Vec<String>,
      pub pending: Vec<String>,
  }
  ```
  and `PROTOCOL_VERSION == 4`. Task 8 supplies `SetDogConfig`'s handler; this task lands the variant, its fixture row, and a dispatch arm that answers `RpcErrorCode::Internal` with the message "SetDogConfig lands in task 8", so the wildcard is never what answers it.

Three, not the spec's two. `ApplyConfig` has no `ResetDepth` that overwrites one established env key: `None` appends only, `File` and `Policy` keep env, `Env` and `All` replace the whole map with the template's. A pane cannot send the whole map because decision 12 forbids it from reading the values. So env gets its own request, which is what decision 12's "lookout can set an env value" always needed and #124 left unbuilt.

**A missing handler does not fail to compile.** `Request` and `Response` are `#[non_exhaustive]` (`request.rs:212`, `:1515`) and `rpc.rs`'s dispatch ends in a wildcard arm (`:657-661`) answering "this daemon does not implement that request". The two wire snapshots are hand-written literal fixtures with no completeness check. A variant with no arm silently answers an internal error at runtime and nothing catches it, which is why step 6 tests reachability.

- [ ] **Step 1: the variants and the view type.**

In `request.rs`, after `ApplyConfig`:

```rust
/// One sheep's effective config, for a pane that is about to edit it.
///
/// `env` comes back emptied and its key names ride separately, so a value
/// never crosses the wire (decision 12 of the overrides design).
SheepConfig {
    /// The sheep's name, not a selector: a pane edits one sheep.
    name: String,
},
/// Sets, replaces, or with `None` removes one env key on one sheep,
/// recorded as an operator override. Never reads it back.
SetSheepEnv {
    /// The sheep's name.
    name: String,
    /// The env key.
    key: String,
    /// The value, or `None` to remove the key.
    value: Option<String>,
},
/// Replaces one dog's `[<name>]` section in `dogs.toml` and publishes
/// `config.dog.<name>` so a running dog re-reads it.
SetDogConfig {
    /// The dog's name, the config key.
    name: String,
    /// The whole section, as TOML text.
    toml: DogSectionToml,
},
```

After `Applied` in `Response`:

```rust
/// Answer to `SheepConfig`.
SheepConfig(SheepConfigView),
/// Answer to `SetSheepEnv`: the key that was set or removed.
SheepEnvSet {
    /// The sheep.
    name: String,
    /// The key.
    key: String,
},
/// Answer to `SetDogConfig`: the section was written and the topic
/// published.
DogConfigSet {
    /// The dog.
    name: String,
},
```

And the view type, near `SheepApplied`:

```rust
/// One sheep's effective config as a pane sees it: every field but env's
/// values, plus which fields an operator has overridden and which are
/// waiting on a respawn.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SheepConfigView {
    /// The sheep's name.
    pub name: String,
    /// The effective config with `env` cleared. Every remaining field is
    /// operator-supplied policy the pane is about to let them edit, so
    /// withholding a value would make the pane unusable while protecting
    /// nothing.
    pub config: AppConfig,
    /// The env keys, so the pane can list them. Never the values.
    pub env_keys: Vec<String>,
    /// Field names an operator has set that the Flockfile does not declare.
    pub overridden: Vec<String>,
    /// Field names parked until the next respawn.
    pub pending: Vec<String>,
}

impl SheepConfigView {
    /// Builds one, clearing `env` and recording its keys.
    #[must_use]
    pub fn new(mut config: AppConfig, overridden: Vec<String>, pending: Vec<String>) -> Self {
        let env_keys = config.env.keys().cloned().collect();
        config.env.clear();
        Self { name: config.name.clone(), config, env_keys, overridden, pending }
    }
}

/// Redacted (IR-41): `config` carries `args` and `cwd`, which routinely
/// hold a token or a home directory, and this type is what a `{:?}` on a
/// `Response` would print.
impl fmt::Debug for SheepConfigView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SheepConfigView {{ name: {:?}, env_keys: {}, overridden: {}, pending: {} }}",
            self.name,
            self.env_keys.len(),
            self.overridden.len(),
            self.pending.len()
        )
    }
}
```

- [ ] **Step 2: the failing tests in `request.rs`'s `mod tests`.**

```rust
#[test]
fn a_sheep_config_view_never_carries_an_env_value() {
    let mut config = AppConfig::default();
    config.name = "web".into();
    config.env.insert("DB_PASS".into(), "hunter2".into());
    let view = SheepConfigView::new(config, vec![], vec![]);
    assert!(view.config.env.is_empty());
    assert_eq!(view.env_keys, ["DB_PASS"]);
    let json = serde_json::to_string(&view).unwrap();
    assert!(!json.contains("hunter2"), "{json}");
}

#[test]
fn a_sheep_config_views_debug_is_the_exact_redacted_string() {
    let mut config = AppConfig::default();
    config.name = "web".into();
    config.env.insert("A".into(), "1".into());
    let view = SheepConfigView::new(config, vec!["max_restarts".into()], vec![]);
    assert_eq!(
        format!("{view:?}"),
        r#"SheepConfigView { name: "web", env_keys: 1, overridden: 1, pending: 0 }"#
    );
}
```

Add one entry per new variant to `request_wire_snapshots` (`:2269`) and `reply_wire_snapshots` (`:2574`), in the same literal style the fixtures already use. For `SheepConfig` in the reply fixture, build the view from `AppConfig::default()` with `name: "web"`.

Run: `cargo test -p shep-core --lib`
Expected: FAIL on the two new tests; the snapshot tests FAIL with a new `.snap.new`.

- [ ] **Step 3: make them pass.** The code from step 1. Accept the two `.snap.new` files after reading them: each should show exactly one added row per variant and nothing else moved.

- [ ] **Step 4: the actor.**

In `supervisor.rs`, beside `Command::ApplyConfig` (`:442`, guess):

```rust
SheepConfig {
    name: String,
    reply: oneshot::Sender<Result<Option<SheepConfigView>, SupervisorError>>,
},
SetSheepEnv {
    name: String,
    key: String,
    value: Option<String>,
    reply: oneshot::Sender<Result<bool, SupervisorError>>,
},
```

On `SupervisorHandle`, beside `apply_config` (`:976`):

```rust
pub(crate) async fn sheep_config(
    &self,
    name: String,
) -> Result<Option<SheepConfigView>, SupervisorError> {
    let (reply, rx) = oneshot::channel();
    self.tx
        .send(Msg::Command(Command::SheepConfig { name, reply }))
        .await
        .map_err(|_| SupervisorError::EngineStopped)?;
    rx.await.map_err(|_| SupervisorError::EngineStopped)?
}

/// `Ok(false)` when no sheep has that name.
pub(crate) async fn set_sheep_env(
    &self,
    name: String,
    key: String,
    value: Option<String>,
) -> Result<bool, SupervisorError> {
    let (reply, rx) = oneshot::channel();
    self.tx
        .send(Msg::Command(Command::SetSheepEnv { name, key, value, reply }))
        .await
        .map_err(|_| SupervisorError::EngineStopped)?;
    rx.await.map_err(|_| SupervisorError::EngineStopped)?
}
```

The handlers. **How `apply_one` resolves a name to a slot is a guess**: it clones `incoming.config.name` at `:5828` and then looks the slot up. Grep the next twenty lines for the lookup and use the same expression. `intended_spec(id)` at `:4751` returns `Option<&ResolvedApp>`, and `ResolvedApp::config()` yields the `AppConfig` (guess: grep `impl ResolvedApp` in `shep-core/src/config/normalize.rs`).

```rust
fn handle_sheep_config(&mut self, name: &str) -> Option<SheepConfigView> {
    let id = self.id_by_name(name)?; // guess: whatever apply_one uses
    let spec = self.intended_spec(id)?;
    let overridden = overrides::get(&self.paths.overrides, name)
        .ok()
        .flatten()
        .map(|o| o.fields.keys().cloned().collect())
        .unwrap_or_default();
    let pending = self.pending_fields(id); // guess: what fills ProcessInfo::pending
    Some(SheepConfigView::new(spec.config().clone(), overridden, pending))
}

fn handle_set_sheep_env(&mut self, name: &str, key: &str, value: Option<&str>) -> Result<bool, SupervisorError> {
    let Some(id) = self.id_by_name(name) else {
        return Ok(false);
    };
    let mut current = overrides::get(&self.paths.overrides, name)
        .map_err(|e| SupervisorError::Overrides(e.to_string()))? // guess: the variant
        .unwrap_or_default();
    let env = current
        .fields
        .entry("env")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let map = env.as_object_mut().expect("env override is always an object");
    match value {
        Some(v) => { map.insert(key.to_owned(), serde_json::Value::String(v.to_owned())); }
        None => { map.remove(key); }
    }
    let mut changes = BTreeMap::new();
    changes.insert(name.to_owned(), Some(current));
    overrides::update(&self.paths.overrides, &changes)
        .map_err(|e| SupervisorError::Overrides(e.to_string()))?;
    // The running child holds the old env, so the field parks until a
    // respawn, exactly as a NeedsRespawn edit through apply_one does.
    self.mark_pending(id, "env"); // guess: the step apply_one takes after merging
    Ok(true)
}
```

`overrides::get`'s return type, `SupervisorError`'s variant for a store error, and the two "guess" methods are exactly the kind of thing the top of this plan tells you to grep. The contract that is not a guess: after `SetSheepEnv`, `Describe` shows `"env"` in `pending` and `overrides.json` holds the key. The test in step 6 pins that.

- [ ] **Step 5: the dispatch arms.**

In `rpc.rs` beside the `DogConfig` arm at `:529`:

```rust
Request::SheepConfig { name } => match ctx.supervisor.sheep_config(name.clone()).await {
    Ok(Some(view)) => reply(Ok(Response::SheepConfig(view))),
    Ok(None) => reply(Err(RpcError {
        code: RpcErrorCode::NotFound,
        message: format!("no sheep named {name}"),
        daemon_version: None,
    })),
    Err(err) => reply(Err(RpcError {
        code: RpcErrorCode::Internal,
        message: err.to_string(),
        daemon_version: None,
    })),
},
Request::SetSheepEnv { name, key, value } => {
    match ctx.supervisor.set_sheep_env(name.clone(), key.clone(), value).await {
        Ok(true) => reply(Ok(Response::SheepEnvSet { name, key })),
        Ok(false) => reply(Err(RpcError {
            code: RpcErrorCode::NotFound,
            message: format!("no sheep named {name}"),
            daemon_version: None,
        })),
        Err(err) => reply(Err(RpcError {
            code: RpcErrorCode::Internal,
            message: err.to_string(),
            daemon_version: None,
        })),
    }
}
Request::SetDogConfig { name, .. } => reply(Err(RpcError {
    code: RpcErrorCode::Internal,
    message: format!("SetDogConfig for {name} lands in task 8"),
    daemon_version: None,
})),
```

- [ ] **Step 6: reachability tests, in `rpc.rs`'s `mod tests`.**

The existing tests build an `RpcContext` and send a `Request` (guess: `:2630` sends `Request::DogConfig`; copy its fixture). Add:

```rust
#[tokio::test]
async fn sheep_config_answers_with_env_emptied_and_its_keys_listed() {
    let ctx = /* the fixture the DogConfig test uses, with one sheep "web" registered
                 whose env holds DB_PASS=hunter2 */;
    let reply = dispatch(&ctx, Request::SheepConfig { name: "web".into() }).await;
    let Ok(Response::SheepConfig(view)) = reply else { panic!("{reply:?}") };
    assert_eq!(view.name, "web");
    assert!(view.config.env.is_empty());
    assert_eq!(view.env_keys, ["DB_PASS"]);
}

#[tokio::test]
async fn sheep_config_for_an_unknown_name_is_not_found_not_internal() {
    let ctx = /* same fixture */;
    let reply = dispatch(&ctx, Request::SheepConfig { name: "ghost".into() }).await;
    let Err(err) = reply else { panic!("{reply:?}") };
    assert_eq!(err.code, RpcErrorCode::NotFound);
}

#[tokio::test]
async fn set_sheep_env_writes_the_store_and_parks_env_until_a_respawn() {
    let ctx = /* same fixture */;
    let reply = dispatch(&ctx, Request::SetSheepEnv {
        name: "web".into(), key: "NEW".into(), value: Some("1".into()),
    }).await;
    assert!(matches!(reply, Ok(Response::SheepEnvSet { .. })), "{reply:?}");
    let stored = shep_core::overrides::get(&ctx.paths.overrides, "web").unwrap().unwrap();
    assert_eq!(stored.fields["env"]["NEW"], "1");
    let described = dispatch(&ctx, Request::Describe { /* selector for web */ }).await;
    let Ok(Response::Described(infos)) = described else { panic!() };
    assert!(infos[0].pending.as_deref().unwrap_or(&[]).contains(&"env".to_owned()));
}

#[tokio::test]
async fn every_new_variant_reaches_an_arm_and_not_the_wildcard() {
    let ctx = /* same fixture */;
    for req in [
        Request::SheepConfig { name: "ghost".into() },
        Request::SetSheepEnv { name: "ghost".into(), key: "K".into(), value: None },
        Request::SetDogConfig { name: "bark".into(), toml: String::new().into() },
    ] {
        let reply = dispatch(&ctx, req).await;
        let Err(err) = reply else { continue };
        assert_ne!(
            err.message, "this daemon does not implement that request",
            "a new variant fell through to the wildcard"
        );
    }
}
```

`dispatch` is a guess for whatever the existing tests call to run one request through `RpcContext`; grep the `DogConfig` test at `:2630` for its exact shape and the selector type `Describe` takes.

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow:: rpc`
Expected: PASS.

- [ ] **Step 7: `PROTOCOL_VERSION` to 4.**

`crates/shep-core/src/protocol/mod.rs:62`: `3` becomes `4`. Update the two tests that pin the numeral rather than reading the constant: `hello_handshake_shape` at `request.rs:3126` and `a_dogs_hello_names_the_dog_and_nothing_elses_does` at `:3144`, changing `"protocol":3` to `"protocol":4` in each literal.

**Do not touch** `crates/shep-client/src/reconnect.rs:650,767,785`, `crates/shep-cli/src/commands/daemon.rs:1781,1799`, `crates/shep-cli/src/commands/dogs.rs:2181`, or `request.rs:3180-3307`. Those hardcode "protocol 1", "protocol 2" and "protocol 3" on purpose, simulating an older daemon, and are independent of the constant.

Rename the two wire snapshot files from `_v3` to `_v4` and update the snapshot names in the two tests, since the fixture set changed with the version.

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS.

- [ ] **Step 8: prove non-vacuous.** Delete the `Request::SheepConfig` arm from `rpc.rs`; grep to confirm; run; `every_new_variant_reaches_an_arm_and_not_the_wildcard` must fail naming the wildcard message. Restore. Then in `SheepConfigView::new`, remove `config.env.clear()`; grep; run; `a_sheep_config_view_never_carries_an_env_value` must fail. Restore.

- [ ] **Step 9: commit, in two.** The variants and handlers first, the bump alone second so the breaking commit is one line of diff:

```bash
git add crates/shep-core/src/protocol/request.rs crates/shep-daemon/src/supervisor.rs crates/shep-daemon/src/rpc.rs
git commit -m "feat(core): SheepConfig, SetSheepEnv and SetDogConfig on the wire"
git add crates/shep-core/src/protocol/mod.rs crates/shep-core/src/protocol/request.rs crates/shep-core/src/protocol/snapshots
git commit -m "feat(core)!: PROTOCOL_VERSION 4, for the three config-pane requests"
```

Then run the task gate.

### Task 5: the sheep pane, read-only

**Files:**
- Create: `crates/shep-cli/src/lookout/pane.rs`
- Create: `crates/shep-cli/src/lookout/view/pane.rs`
- Modify: `crates/shep-cli/src/lookout/input.rs:66` (`e` beside `s`)
- Modify: `crates/shep-cli/src/lookout/app.rs:86` (`KeyPress::Edit`), `:320` (`Effect::LoadSheepConfig`), `:133` (`Msg::SheepConfig`), `:1878` (`on_key` routes to the pane), `:2988` (`config_pane()` accessor)
- Modify: `crates/shep-cli/src/lookout/view/mod.rs:158` (dispatch to `draw_pane` when a pane is open)
- Modify: `crates/shep-cli/src/lookout/mod.rs` (the event loop runs `Effect::LoadSheepConfig` by sending `Request::SheepConfig` and posting `Msg::SheepConfig`)

**Interfaces:**
- Consumes: `FieldSet`, `Field`, `FieldKind` (Task 1); `Viewport` (Task 2); `SheepConfigView`, `Request::SheepConfig`, `Response::SheepConfig` (Task 4); `shep_core::config::{apply_group, ApplyGroup, GROUP_ORDER, flockfile_schema_json}`.
- Produces, in `lookout::pane`:
  ```rust
  pub enum PaneTarget {
      Sheep { name: String },
      Dog { name: String, adopted_path: Option<PathBuf> },
  }

  pub struct ConfigPane { /* private */ }

  impl ConfigPane {
      pub fn sheep(view: SheepConfigView) -> Self;
      pub fn target(&self) -> &PaneTarget;
      pub fn fields(&self) -> &FieldSet;
      pub fn value(&self, key: &str) -> String;
      pub fn cost(&self, key: &str) -> Option<ApplyGroup>;
      pub fn is_overridden(&self, key: &str) -> bool;
      pub fn is_pending(&self, key: &str) -> bool;
      pub fn view(&self) -> &Viewport;
      pub fn set_rows(&mut self, rows: usize);
      pub fn rows(&self) -> Vec<PaneRow>;
      pub fn cursor(&self) -> Option<PaneRow>;
  }

  pub enum PaneRow { Field(usize) }
  ```
  On `App`: `pub fn config_pane(&self) -> Option<&ConfigPane>`.
  `KeyPress::Edit`, `Effect::LoadSheepConfig { name: String }`, `Msg::SheepConfig { result: Result<SheepConfigView, String> }`.

Decisions 2, 4, 5, 8. This task opens the pane and draws it. Task 6 makes it write.

- [ ] **Step 1: the failing tests for `ConfigPane`.**

```rust
// crates/shep-cli/src/lookout/pane.rs, mod tests

fn web() -> SheepConfigView {
    let mut config = AppConfig::default();
    config.name = "web".into();
    config.max_restarts = 32;
    config.env.insert("DB_HOST".into(), "{{shared:DB_HOST}}".into());
    SheepConfigView::new(config, vec!["max_restarts".into()], vec!["env".into()])
}

#[test]
fn a_sheep_pane_has_thirty_nine_fields_in_four_groups() {
    let pane = ConfigPane::sheep(web());
    assert_eq!(pane.fields().len(), 39);
    assert_eq!(pane.fields().groups(), ["process", "inputs", "control", "cron"]);
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
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::pane`
Expected: FAIL, `ConfigPane` not found.

- [ ] **Step 2: `ConfigPane`.**

```rust
// crates/shep-cli/src/lookout/pane.rs

//! An open config pane: what it is editing, its fields, and its cursor.

use std::path::PathBuf;

use serde_json::{Map, Value};
use shep_core::config::{apply_group, ApplyGroup, GROUP_ORDER, flockfile_schema_json};
use shep_core::protocol::SheepConfigView;

use super::field::{FieldKind, FieldSet};
use super::viewport::Viewport;

/// Which thing the pane is editing.
///
/// `Debug` is derived (IR-41): a name and a path, neither secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTarget {
    /// One sheep, by name.
    Sheep {
        /// The sheep.
        name: String,
    },
    /// One dog, by name, with its binary if adopted.
    Dog {
        /// The dog.
        name: String,
        /// `None` for a built-in.
        adopted_path: Option<PathBuf>,
    },
}

/// One row of the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRow {
    /// Index into [`ConfigPane::fields`].
    Field(usize),
}

/// The state of an open pane.
///
/// `Debug` is manual (IR-41): `values` is a sheep's config with `env` already
/// stripped by [`SheepConfigView::new`], but `args` and `cwd` are still in
/// it and routinely carry a token or a home directory.
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

impl std::fmt::Debug for ConfigPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ConfigPane {{ target: {:?}, fields: {}, cursor: {} }}",
            self.target,
            self.fields.len(),
            self.view.cursor()
        )
    }
}

impl ConfigPane {
    /// A pane over one sheep's config.
    #[must_use]
    pub fn sheep(view: SheepConfigView) -> Self {
        let schema = flockfile_schema_json().to_value();
        let defs = schema["$defs"].as_object().cloned().unwrap_or_default();
        let props = defs
            .get("AppConfig")
            .and_then(|a| a.get("properties"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut set = FieldSet::from_properties(&props, &defs, GROUP_ORDER);
        // Decision 5: name cannot drift, instances only produces a note on
        // a plain apply, increment_var is not in the schema. Read-only.
        set = FieldSet::from_fields(
            set.fields()
                .iter()
                .cloned()
                .map(|mut f| {
                    if apply_group(&f.key) == ApplyGroup::Structural {
                        f.editable = false;
                    }
                    f
                })
                .collect(),
            GROUP_ORDER,
        );
        let values = serde_json::to_value(&view.config)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        Self {
            target: PaneTarget::Sheep { name: view.name },
            fields: set,
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

    /// The current value of `key`, rendered for a cell. A scalar shows
    /// bare, `null` shows `(unset)`, a map shows its key count, anything
    /// else shows compact JSON.
    #[must_use]
    pub fn value(&self, key: &str) -> String {
        if key == "env" {
            return match self.env_keys.len() {
                1 => "1 key".to_owned(),
                n => format!("{n} keys"),
            };
        }
        match self.values.get(key) {
            None | Some(Value::Null) => "(unset)".to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => n.to_string(),
            Some(other) => other.to_string(),
        }
    }

    /// What changing `key` costs. `None` for a dog, which decides for
    /// itself (decision 4).
    #[must_use]
    pub fn cost(&self, key: &str) -> Option<ApplyGroup> {
        match self.target {
            PaneTarget::Sheep { .. } => Some(apply_group(key)),
            PaneTarget::Dog { .. } => None,
        }
    }

    /// Whether an operator has overridden `key`.
    #[must_use]
    pub fn is_overridden(&self, key: &str) -> bool {
        self.overridden.iter().any(|k| k == key)
    }

    /// Whether `key` is parked until a respawn.
    #[must_use]
    pub fn is_pending(&self, key: &str) -> bool {
        self.pending.iter().any(|k| k == key)
    }

    /// The cursor and offset.
    #[must_use]
    pub fn view(&self) -> &Viewport {
        &self.view
    }

    /// Records the terminal's height.
    pub fn set_rows(&mut self, rows: usize) {
        self.view.set_rows(rows);
    }

    /// One row per field, in display order.
    #[must_use]
    pub fn rows(&self) -> Vec<PaneRow> {
        (0..self.fields.len()).map(PaneRow::Field).collect()
    }

    /// The row under the cursor.
    #[must_use]
    pub fn cursor(&self) -> Option<PaneRow> {
        let rows = self.rows();
        rows.get(self.view.cursor().min(rows.len().saturating_sub(1))).copied()
    }

    pub(crate) fn move_by(&mut self, delta: isize) {
        let len = self.rows().len();
        self.view.move_by(delta, len);
    }

    pub(crate) fn move_to_first(&mut self) {
        let len = self.rows().len();
        self.view.move_to(0, len);
    }

    pub(crate) fn move_to_last(&mut self) {
        let len = self.rows().len();
        self.view.move_to(len.saturating_sub(1), len);
    }

    /// The env keys, for the sub-screen.
    #[must_use]
    pub fn env_keys(&self) -> &[String] {
        &self.env_keys
    }
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::pane`
Expected: PASS, 5 tests.

- [ ] **Step 3: `e`, the effect, the message, the routing.**

`input.rs:66`, beside `'s'`: `KeyCode::Char('e') => Some(KeyPress::Edit),`.

`app.rs:86`, in `KeyPress`:

```rust
/// `e`: open the config pane for the selected sheep on the dashboard, or
/// for the selected dog on the settings screen. Closes the pane from
/// inside it. The reducer decides which, the same division
/// [`Self::Settings`] draws.
Edit,
```

In `Effect`: `LoadSheepConfig { name: String },`. In `Msg`: `SheepConfig { result: Result<SheepConfigView, String> },`.

On `App`: `config_pane: Option<ConfigPane>` (add to the struct and `App::new`), and:

```rust
/// The open config pane, if any.
#[must_use]
pub fn config_pane(&self) -> Option<&ConfigPane> {
    self.config_pane.as_ref()
}
```

`on_key` at `:1878` (guess) checks `self.settings.is_some()` before the dashboard keymap. Add `self.config_pane.is_some()` FIRST, routing to a new `on_pane_key`:

```rust
fn on_pane_key(&mut self, key: KeyPress) -> Effect {
    let Some(pane) = self.config_pane.as_mut() else {
        return Effect::None;
    };
    match key {
        KeyPress::Quit => Effect::Quit, // guess: whatever on_settings_key returns for Quit
        KeyPress::Edit | KeyPress::Escape => {
            self.config_pane = None;
            Effect::None
        }
        KeyPress::SelectUp => { pane.move_by(-1); Effect::None }
        KeyPress::SelectDown => { pane.move_by(1); Effect::None }
        KeyPress::SelectFirst => { pane.move_to_first(); Effect::None }
        KeyPress::SelectLast => { pane.move_to_last(); Effect::None }
        KeyPress::Refresh => match pane.target() {
            PaneTarget::Sheep { name } => Effect::LoadSheepConfig { name: name.clone() },
            PaneTarget::Dog { .. } => Effect::None,
        },
        _ => Effect::None,
    }
}
```

And the dashboard's `Edit` arm, in the dashboard keymap:

```rust
KeyPress::Edit => match self.selected_row() {
    Some(row) => Effect::LoadSheepConfig { name: row.info.name.clone() },
    None => Effect::None,
},
```

`Msg::SheepConfig` in the reducer:

```rust
Msg::SheepConfig { result } => {
    match result {
        Ok(view) => {
            let rows = self.config_pane.as_ref().map(|p| p.view().clone());
            let mut pane = ConfigPane::sheep(view);
            if let Some(v) = rows {
                pane.view = v; // guess: needs a setter or pub(super) field
                pane.view.clamp(pane.rows().len());
            }
            self.config_pane = Some(pane);
        }
        Err(message) => self.status = message, // guess: how settings reports a load failure
    }
    Effect::None
}
```

`note_body_rows` from Task 2 gains: `if let Some(pane) = self.config_pane.as_mut() { pane.set_rows(usize::from(rows)); }`.

`lookout/mod.rs`'s effect runner (guess: grep `Effect::LoadSettings` to find where effects become I/O) gains an arm that sends `Request::SheepConfig { name }` on the client and posts `Msg::SheepConfig { result }` with the reply mapped: `Response::SheepConfig(v)` to `Ok(v)`, anything else to `Err(String)`.

- [ ] **Step 4: the renderer.**

```rust
// crates/shep-cli/src/lookout/view/pane.rs

//! Drawing a [`ConfigPane`]: sections from the field set's groups, one row
//! per field, a cost column for a sheep, and the viewport's hidden counts.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use shep_core::config::ApplyGroup;

use super::super::app::App;
use super::super::pane::{ConfigPane, PaneRow, PaneTarget};
use super::settings::section_header; // make it pub(super)
use super::theme::Palette;

const GUTTER: u16 = 2;
const KEY_W: u16 = 26;
const COST_W: u16 = 10;

fn cost_label(group: ApplyGroup) -> &'static str {
    match group {
        ApplyGroup::Live => "now",
        ApplyGroup::NextSpawn => "next start",
        ApplyGroup::NeedsRespawn => "respawn",
        ApplyGroup::Structural => "read-only",
        _ => "respawn",
    }
}

/// Every line of the pane, viewport applied.
pub fn pane_lines(app: &App, pane: &ConfigPane, palette: Palette, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let rows = pane.rows();
    let view = pane.view();
    let title = match pane.target() {
        PaneTarget::Sheep { name } => format!("  {name}  (sheep config)"),
        PaneTarget::Dog { name, .. } => format!("  {name}  (dog config)"),
    };
    lines.push(Line::from(Span::styled(title, palette.title())));
    if view.hidden_above() > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ... {} above", view.hidden_above()),
            palette.muted(),
        )));
    }
    let end = if view.rows() == 0 { rows.len() } else { (view.offset() + view.rows()).min(rows.len()) };
    let mut current_group: Option<&str> = None;
    for (i, row) in rows.iter().enumerate().take(end).skip(view.offset()) {
        let PaneRow::Field(index) = row;
        let field = &pane.fields().fields()[*index];
        let group = field.group.as_deref();
        if group != current_group {
            if current_group.is_some() || i > view.offset() {
                lines.push(Line::default());
            }
            if let Some(g) = group {
                lines.push(section_header(g, palette));
            }
            current_group = group;
        }
        let selected = pane.cursor() == Some(*row);
        lines.push(field_line(pane, *index, selected, width, palette));
    }
    let hidden = view.hidden_below(rows.len());
    if hidden > 0 {
        lines.push(Line::from(Span::styled(format!("  ... {hidden} below"), palette.muted())));
    }
    if let PaneTarget::Dog { .. } = pane.target() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  a change is published to the dog, which decides what to reload",
            palette.muted(),
        )));
    }
    let _ = app;
    lines
}

fn field_line(pane: &ConfigPane, index: usize, selected: bool, width: u16, palette: Palette) -> Line<'static> {
    let field = &pane.fields().fields()[index];
    let mark = if selected { "> " } else { "  " };
    let value = if field.secret {
        if pane.value(&field.key) == "(unset)" { "(unset)".to_owned() } else { "<set>".to_owned() }
    } else {
        pane.value(&field.key)
    };
    let flags = match (pane.is_pending(&field.key), pane.is_overridden(&field.key)) {
        (true, _) => "!",
        (false, true) => "*",
        _ => " ",
    };
    let cost = pane.cost(&field.key).map_or("", cost_label);
    let value_w = width
        .saturating_sub(GUTTER + KEY_W + COST_W + 4)
        .max(8) as usize;
    let mut value = value;
    if value.chars().count() > value_w {
        value = value.chars().take(value_w.saturating_sub(1)).collect::<String>() + "…";
    }
    let text = format!(
        "{mark}{flags}{key:<kw$} {value:<vw$} {cost:>cw$}",
        key = field.key,
        kw = usize::from(KEY_W),
        vw = value_w,
        cw = usize::from(COST_W),
    );
    let style = if !field.editable {
        palette.muted()
    } else if selected {
        palette.selected()
    } else {
        palette.normal()
    };
    Line::from(Span::styled(text, style))
}

/// Draws the pane into `area`.
pub fn draw_pane(app: &App, pane: &ConfigPane, area: Rect, buffer: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for (offset, line) in pane_lines(app, pane, app.palette(), area.width)
        .into_iter()
        .take(usize::from(area.height))
        .enumerate()
    {
        let y = area.y + u16::try_from(offset).unwrap_or(0);
        buffer.set_line(area.x, y, &line, area.width);
    }
}
```

`palette.title()`, `palette.selected()`, `palette.normal()` are guesses for whatever `theme::Palette` calls them; grep `impl Palette`. The `…` character is a single ellipsis, U+2026, not three dots and not a dash.

`view/mod.rs::draw` at `:158` (guess) checks `app.settings()` and draws the settings screen instead of the dashboard. Add the same check for `app.config_pane()` before it, calling `draw_pane`.

- [ ] **Step 5: rendered frame tests.**

In `view/pane.rs`'s `mod tests`, build an `App` the way `fixtures::app_in_settings()` does (guess: copy it), then:

```rust
fn web_pane() -> ConfigPane {
    let mut config = AppConfig::default();
    config.name = "web".into();
    config.script = "./srv".into();
    config.max_restarts = 32;
    config.env.insert("DB_HOST".into(), "x".into());
    ConfigPane::sheep(SheepConfigView::new(config, vec!["max_restarts".into()], vec![]))
}

#[test]
fn a_sheep_pane_at_a_comfortable_width() {
    let app = fixtures::app(); // guess
    let pane = web_pane();
    let lines = pane_lines(&app, &pane, app.palette(), 120);
    let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
    insta::assert_snapshot!("sheep_pane_wide", text.join("\n"));
}

#[test]
fn a_sheep_pane_scrolled_to_the_cron_section_labels_it() {
    let app = fixtures::app();
    let mut pane = web_pane();
    pane.set_rows(8);
    pane.move_to_last();
    let lines = pane_lines(&app, &pane, app.palette(), 120);
    let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
    assert!(text.iter().any(|l| l.contains("above")), "{text:?}");
    assert!(text.iter().any(|l| l.trim() == "cron"), "the visible section is labelled: {text:?}");
    assert!(!text.iter().any(|l| l.contains("below")));
}

#[test]
fn every_pane_line_fits_the_width_it_was_drawn_for() {
    let app = fixtures::app();
    let pane = web_pane();
    for width in 40..=200u16 {
        for line in pane_lines(&app, &pane, app.palette(), width) {
            assert!(line.width() <= usize::from(width), "width {width}: {line:?}");
        }
    }
}

#[test]
fn a_structural_field_renders_muted_and_the_cost_column_says_why() {
    let app = fixtures::app();
    let pane = web_pane();
    let lines = pane_lines(&app, &pane, app.palette(), 120);
    let instances = lines.iter().map(ToString::to_string).find(|l| l.contains("instances")).unwrap();
    assert!(instances.contains("read-only"), "{instances}");
}
```

Review `sheep_pane_wide.snap` by eye before accepting: four section headers in order, 39 rows, `max_restarts` flagged `*`, `instances` marked read-only.

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout`
Expected: PASS. The seven settings snapshots unchanged.

- [ ] **Step 6: prove non-vacuous.** In `ConfigPane::sheep`, remove the Structural read-only loop; grep; run; `structural_fields_are_not_editable_and_the_rest_are` and `a_structural_field_renders_muted_...` both fail. Restore.

- [ ] **Step 7: commit.**

```bash
git add crates/shep-cli/src/lookout
git commit -m "feat(lookout): a config pane for a sheep, opened with e"
```

### Task 6: the sheep pane writes, and env has its sub-screen

**Files:**
- Modify: `crates/shep-cli/src/lookout/pane.rs` (`PanePending`, `PaneEdit`, `EnvPane`)
- Modify: `crates/shep-cli/src/lookout/app.rs` (`on_pane_key` gains Cycle/Confirm/text; `Effect::ApplySheepField`, `Effect::SetSheepEnv`; `Msg::SheepFieldApplied`, `Msg::SheepEnvSet`)
- Modify: `crates/shep-cli/src/lookout/view/pane.rs` (the confirm line; the env sub-screen)
- Modify: `crates/shep-cli/src/lookout/mod.rs` (run the two effects)

**Interfaces:**
- Consumes: Task 5's pane; `Request::ApplyConfig`, `DeclaredApp`, `ResetDepth::File`, `Response::Applied`, `SheepApplied` (existing); `Request::SetSheepEnv`, `Response::SheepEnvSet` (Task 4); `WriteAuthority` and `authorize_write` (existing, `app.rs:2094`).
- Produces, in `lookout::pane`:
  ```rust
  pub enum PaneEdit { Set { key: String, value: serde_json::Value } }
  pub enum PanePending {
      Typing { key: String, buffer: String },
      Armed { edit: PaneEdit, text: String, at: Instant },
      Sent { text: String },
  }
  pub struct EnvPane { /* keys, view, typing */ }
  ```
  `Effect::ApplySheepField { name: String, edit: PaneEdit, authority: WriteAuthority }`,
  `Effect::SetSheepEnv { name: String, key: String, value: Option<String>, authority: WriteAuthority }`,
  `Msg::SheepFieldApplied { result: Result<SheepApplied, String> }`,
  `Msg::SheepEnvSet { result: Result<(String, String), String> }`.

**How a single-field write reaches the store, and why `ResetDepth::File`.** The pane sends `Request::ApplyConfig` with one `DeclaredApp` whose `config` is the pane's current config with `key` changed, whose `declared` is exactly `{key}`, and `reset: ResetDepth::File`. Under `File`, "a key the template declares: reset; a key it does not declare: kept; env: kept". So one key goes to the pane's value and nothing else moves, including `instances`, which the pane never declares. Under `None` the same request would be ignored for any key already established, which is every key an operator has ever touched. This is the single most important sentence in this task.

- [ ] **Step 1: the failing tests.**

```rust
// pane.rs, mod tests

#[test]
fn cycling_a_bool_arms_a_set_with_the_flipped_value() {
    let mut pane = ConfigPane::sheep(web());
    pane.move_to_key("autorestart");
    pane.cycle(Instant::now());
    let Some(PanePending::Armed { edit: PaneEdit::Set { key, value }, text, .. }) = pane.pending() else {
        panic!("{:?}", pane.pending());
    };
    assert_eq!(key, "autorestart");
    assert_eq!(*value, serde_json::json!(false));
    assert!(text.contains("autorestart"));
}

#[test]
fn a_respawn_field_arms_a_confirm_that_names_the_death() {
    let mut pane = ConfigPane::sheep(web());
    pane.move_to_key("merge_logs");
    pane.cycle(Instant::now());
    let Some(PanePending::Armed { text, .. }) = pane.pending() else { panic!() };
    assert!(text.contains("respawn"), "{text}");
    assert!(text.contains("web"), "{text}");
}

#[test]
fn a_read_only_field_does_not_arm() {
    let mut pane = ConfigPane::sheep(web());
    pane.move_to_key("instances");
    pane.cycle(Instant::now());
    assert!(pane.pending().is_none());
}

#[test]
fn typing_into_an_integer_and_applying_arms_a_number_not_a_string() {
    let mut pane = ConfigPane::sheep(web());
    pane.move_to_key("max_restarts");
    pane.begin_typing();
    for c in "40".chars() { pane.type_char(c); }
    pane.apply_typing(Instant::now());
    let Some(PanePending::Armed { edit: PaneEdit::Set { value, .. }, .. }) = pane.pending() else { panic!() };
    assert_eq!(*value, serde_json::json!(40));
}

#[test]
fn a_declared_app_for_one_edit_declares_only_that_key() {
    let pane = ConfigPane::sheep(web());
    let edit = PaneEdit::Set { key: "max_restarts".into(), value: serde_json::json!(40) };
    let app = pane.declared_app(&edit).unwrap();
    assert_eq!(app.config.max_restarts, 40);
    assert_eq!(app.config.name, "web");
    assert_eq!(app.declared, ["max_restarts".to_owned()].into_iter().collect());
    assert!(app.declared_env.is_empty());
    assert!(app.config.env.is_empty(), "env is never round-tripped through a pane");
}

#[test]
fn the_env_pane_lists_keys_and_a_new_row_and_an_empty_apply_means_unset() {
    let mut env = EnvPane::new(vec!["A".into(), "B".into()]);
    assert_eq!(env.rows().len(), 3, "two keys and a + new row");
    env.move_to_last();
    env.begin_typing();
    for c in "C=3".chars() { env.type_char(c); }
    assert_eq!(env.apply_typing(), Some(("C".to_owned(), Some("3".to_owned()))));
    env.move_to_first();
    env.begin_typing();
    assert_eq!(env.apply_typing(), Some(("A".to_owned(), None)));
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout::pane`
Expected: FAIL, the methods do not exist.

- [ ] **Step 2: the edit model on `ConfigPane`.**

```rust
/// One edit, ready to send.
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

/// The pane's one in-flight edit. One field, not several `Option`s, for the
/// reason `Settings::pending` gives: typing, armed and sent cannot overlap.
#[derive(Debug, Clone)]
pub enum PanePending {
    /// A text edit under construction.
    Typing { key: String, buffer: String },
    /// Waiting for `Enter`. Nothing has gone out.
    Armed { edit: PaneEdit, text: String, at: Instant },
    /// Gone out, awaiting the reply.
    Sent { text: String },
}

impl ConfigPane {
    /// The in-flight edit.
    #[must_use]
    pub fn pending(&self) -> Option<&PanePending> {
        self.pending.as_ref()
    }

    /// Arms an edit for the field under the cursor, or does nothing for a
    /// field that cycles nothing.
    pub fn cycle(&mut self, now: Instant) {
        let Some(PaneRow::Field(index)) = self.cursor() else { return };
        let field = &self.fields.fields()[index];
        if !field.editable {
            return;
        }
        let next = match &field.kind {
            FieldKind::Bool => {
                let current = self.values.get(&field.key).and_then(Value::as_bool).unwrap_or(false);
                Value::Bool(!current)
            }
            FieldKind::Choice(names) => {
                let current = self.values.get(&field.key).and_then(Value::as_str);
                let i = current.and_then(|c| names.iter().position(|n| n == c)).map_or(0, |i| (i + 1) % names.len());
                Value::String(names[i].clone())
            }
            _ => return,
        };
        let edit = PaneEdit::Set { key: field.key.clone(), value: next };
        let text = self.confirm_text(&edit);
        self.pending = Some(PanePending::Armed { edit, text, at: now });
    }

    /// Opens the text editor on the field under the cursor.
    pub fn begin_typing(&mut self) {
        let Some(PaneRow::Field(index)) = self.cursor() else { return };
        let field = &self.fields.fields()[index];
        if !field.editable || !matches!(field.kind, FieldKind::Text | FieldKind::Integer) {
            return;
        }
        let seed = if field.secret { String::new() } else {
            match self.value(&field.key).as_str() { "(unset)" => String::new(), v => v.to_owned() }
        };
        self.pending = Some(PanePending::Typing { key: field.key.clone(), buffer: seed });
    }

    pub fn type_char(&mut self, c: char) {
        if let Some(PanePending::Typing { buffer, .. }) = self.pending.as_mut() {
            buffer.push(c);
        }
    }

    pub fn type_backspace(&mut self) {
        if let Some(PanePending::Typing { buffer, .. }) = self.pending.as_mut() {
            buffer.pop();
        }
    }

    /// Turns the buffer into an armed edit, typed to the field's kind. An
    /// empty buffer means `null`, which is how a nullable field is unset.
    pub fn apply_typing(&mut self, now: Instant) {
        let Some(PanePending::Typing { key, buffer }) = self.pending.take() else { return };
        let kind = self.fields.by_key(&key).map(|f| f.kind.clone());
        let value = match (kind, buffer.as_str()) {
            (_, "") => Value::Null,
            (Some(FieldKind::Integer), s) => match s.parse::<i64>() {
                Ok(n) => Value::from(n),
                Err(_) => {
                    self.pending = Some(PanePending::Typing { key, buffer });
                    return;
                }
            },
            (_, s) => Value::String(s.to_owned()),
        };
        let edit = PaneEdit::Set { key, value };
        let text = self.confirm_text(&edit);
        self.pending = Some(PanePending::Armed { edit, text, at: now });
    }

    pub fn abandon_typing(&mut self) {
        if matches!(self.pending, Some(PanePending::Typing { .. })) {
            self.pending = None;
        }
    }

    pub fn cancel(&mut self) {
        if matches!(self.pending, Some(PanePending::Armed { .. })) {
            self.pending = None;
        }
    }

    /// Takes the armed edit out, marking it sent.
    pub fn take_armed(&mut self) -> Option<PaneEdit> {
        match self.pending.take() {
            Some(PanePending::Armed { edit, text, .. }) => {
                self.pending = Some(PanePending::Sent { text });
                Some(edit)
            }
            other => {
                self.pending = other;
                None
            }
        }
    }

    pub fn settle(&mut self) {
        self.pending = None;
    }

    fn confirm_text(&self, edit: &PaneEdit) -> String {
        let PaneEdit::Set { key, value } = edit;
        let shown = match value { Value::Null => "(unset)".to_owned(), Value::String(s) => s.clone(), v => v.to_string() };
        let name = match &self.target { PaneTarget::Sheep { name } | PaneTarget::Dog { name, .. } => name };
        match self.cost(key) {
            Some(ApplyGroup::NeedsRespawn) => format!("set {key} = {shown}? {name} is respawned to pick it up"),
            Some(ApplyGroup::NextSpawn) => format!("set {key} = {shown}? takes effect at the next start"),
            Some(_) => format!("set {key} = {shown}? takes effect now"),
            None => format!("set {key} = {shown}? {name} is told, and decides what to reload"),
        }
    }

    /// The one-app `DeclaredApp` a sheep edit sends. `declared` is exactly
    /// the edited key, so under `ResetDepth::File` nothing else moves.
    pub fn declared_app(&self, edit: &PaneEdit) -> Option<DeclaredApp> {
        let PaneTarget::Sheep { .. } = &self.target else { return None };
        let PaneEdit::Set { key, value } = edit;
        let mut values = self.values.clone();
        values.insert(key.clone(), value.clone());
        let config: AppConfig = serde_json::from_value(Value::Object(values)).ok()?;
        Some(DeclaredApp {
            config,
            declared: std::iter::once(key.clone()).collect(),
            declared_env: Default::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn move_to_key(&mut self, key: &str) {
        if let Some(i) = self.fields.fields().iter().position(|f| f.key == key) {
            let len = self.rows().len();
            self.view.move_to(i, len);
        }
    }
}
```

`ConfigPane` gains `pending: Option<PanePending>`, initialised `None`. `DeclaredApp`'s field names are from `crates/shep-core/src/config/flockfile.rs:45`; `AppConfig` must round-trip through JSON for `declared_app` to work, which `drifted_fields` already relies on.

- [ ] **Step 3: `EnvPane`.**

```rust
/// The env sub-screen: key names, write-only (decision 12).
///
/// `Debug` is derived (IR-41): key names and a buffer the operator is
/// typing, which is a value they can see because they are typing it.
#[derive(Debug, Clone)]
pub struct EnvPane {
    keys: Vec<String>,
    view: Viewport,
    typing: Option<(Option<String>, String)>,
}

/// One row of the env sub-screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRow {
    /// Index into the keys.
    Key(usize),
    /// The `+ new` row.
    New,
}

impl EnvPane {
    #[must_use]
    pub fn new(keys: Vec<String>) -> Self {
        Self { keys, view: Viewport::new(), typing: None }
    }

    #[must_use]
    pub fn rows(&self) -> Vec<EnvRow> {
        let mut rows: Vec<EnvRow> = (0..self.keys.len()).map(EnvRow::Key).collect();
        rows.push(EnvRow::New);
        rows
    }

    #[must_use]
    pub fn cursor(&self) -> Option<EnvRow> {
        let rows = self.rows();
        rows.get(self.view.cursor().min(rows.len().saturating_sub(1))).copied()
    }

    #[must_use]
    pub fn keys(&self) -> &[String] { &self.keys }
    #[must_use]
    pub fn view(&self) -> &Viewport { &self.view }
    #[must_use]
    pub fn typing(&self) -> Option<&str> { self.typing.as_ref().map(|(_, b)| b.as_str()) }

    pub fn set_rows(&mut self, rows: usize) { self.view.set_rows(rows); }
    pub fn move_by(&mut self, delta: isize) { let len = self.rows().len(); self.view.move_by(delta, len); }
    pub fn move_to_first(&mut self) { let len = self.rows().len(); self.view.move_to(0, len); }
    pub fn move_to_last(&mut self) { let len = self.rows().len(); self.view.move_to(len.saturating_sub(1), len); }

    /// On a key: type a new value, seeded empty because the value is never
    /// read back. On `+ new`: type `KEY=value`.
    pub fn begin_typing(&mut self) {
        self.typing = match self.cursor() {
            Some(EnvRow::Key(i)) => Some((Some(self.keys[i].clone()), String::new())),
            Some(EnvRow::New) => Some((None, String::new())),
            None => None,
        };
    }

    pub fn type_char(&mut self, c: char) { if let Some((_, b)) = self.typing.as_mut() { b.push(c); } }
    pub fn type_backspace(&mut self) { if let Some((_, b)) = self.typing.as_mut() { b.pop(); } }
    pub fn abandon_typing(&mut self) { self.typing = None; }

    /// `(key, Some(value))` to set, `(key, None)` to unset. `None` for a
    /// `+ new` buffer with no `=`.
    pub fn apply_typing(&mut self) -> Option<(String, Option<String>)> {
        let (key, buffer) = self.typing.take()?;
        match key {
            Some(k) => Some((k, if buffer.is_empty() { None } else { Some(buffer) })),
            None => {
                let (k, v) = buffer.split_once('=')?;
                if k.is_empty() { return None; }
                Some((k.to_owned(), Some(v.to_owned())))
            }
        }
    }
}
```

`ConfigPane` gains `env: Option<EnvPane>` and `pub fn env(&self) -> Option<&EnvPane>`. `Enter` on the `env` row (kind `Map`) sets `self.env = Some(EnvPane::new(self.env_keys.clone()))`; `Escape` inside it clears it.

- [ ] **Step 4: keys, effects, messages.**

In `on_pane_key`, when `pane.env().is_some()` route to the env sub-screen first (movement, `Confirm` begins typing, text keys, `TextApply` yields `(key, value)` and raises `Effect::SetSheepEnv`, `Escape` closes). Otherwise:

```rust
KeyPress::Cycle => {
    let Some(authority) = self.authorize_write() else { return Effect::None };
    let _ = authority; // the arm is gated; the send below re-mints
    if let Some(pane) = self.config_pane.as_mut() { pane.cycle(self.now); }
    Effect::None
}
KeyPress::Confirm => {
    let Some(pane) = self.config_pane.as_mut() else { return Effect::None };
    match pane.pending() {
        Some(PanePending::Armed { .. }) => {
            let Some(authority) = self.authorize_write() else { return Effect::None };
            let pane = self.config_pane.as_mut().unwrap();
            let Some(edit) = pane.take_armed() else { return Effect::None };
            match pane.target().clone() {
                PaneTarget::Sheep { name } => Effect::ApplySheepField { name, edit, authority },
                PaneTarget::Dog { .. } => Effect::None, // task 9
            }
        }
        None => {
            match pane.cursor().and_then(|PaneRow::Field(i)| pane.fields().fields().get(i)) {
                Some(f) if f.kind == FieldKind::Map => { pane.open_env(); }
                Some(f) if matches!(f.kind, FieldKind::Text | FieldKind::Integer) => {
                    pane.begin_typing();
                    self.mode = InputMode::Text;
                }
                _ => {}
            }
            Effect::None
        }
        _ => Effect::None,
    }
}
```

The text-mode router `on_text_key` at `:2406` (guess) gains a `self.config_pane.is_some()` branch forwarding `TextChar`/`TextBackspace`/`TextApply`/`TextAbandon` to the pane (or its env sub-screen), returning to `InputMode::Normal` on apply or abandon, exactly as `on_settings_text_key` does.

`Msg::Tick` expires an `Armed` pane edit after `CONFIRM_EXPIRY`, beside where it expires `Pending::Armed`.

`lookout/mod.rs` runs the effects: `ApplySheepField` builds `pane.declared_app(&edit)` (the `App` must expose it, or the effect carries the `DeclaredApp` built at confirm time, which is simpler: build it in the `Confirm` arm and put it on the effect), sends `Request::ApplyConfig { apps: vec![app], reset: ResetDepth::File }`, and posts `Msg::SheepFieldApplied { result }` with `Response::Applied(v)` mapped to `Ok(v.into_iter().next().unwrap())`. `SetSheepEnv` sends the request and posts `Msg::SheepEnvSet`.

Both `Msg` arms: on `Ok`, `pane.settle()` and raise `Effect::LoadSheepConfig` to re-read, and if `SheepApplied::refused` is `Some`, put it on the status line. On `Err`, `settle()` and show the error. The reload keeps the viewport, as Task 5's `Msg::SheepConfig` arm already does.

- [ ] **Step 5: the renderer shows the confirm and the sub-screen.**

In `pane_lines`, after the title: if `pane.pending()` is `Armed { text, .. }` or `Sent { text }`, push `  {text}` in `palette.confirm()` (guess the style name; the settings screen renders `SettingsPrompt` somewhere in `content_lines`, copy its style). If `Typing { key, buffer }`, the field's row shows `buffer` with a trailing `_` instead of its value.

If `pane.env()` is `Some(env)`, render that instead: title `  {name}  env (write-only)`, one row per key showing `KEY  <set>` (or `KEY  {{shared:...}}` in full if the pane learns the value is a reference; it cannot today, so `<set>` for all), and a `+ new` row; the typing buffer on the selected row.

- [ ] **Step 6: an end-to-end test against a fake daemon.**

`crates/shep-client/src/testing.rs` has `fake_daemon_across_handovers` and `Handovers::envelopes()` (guess; the dog contract's tests use them). Drive `run_ui` or the reducer with a scripted `Response::Applied`, and assert the `Request::ApplyConfig` envelope the fake daemon received has `reset == ResetDepth::File` and `apps[0].declared == {key}`. If driving the whole UI is impractical, a reducer-level test that captures the `Effect::ApplySheepField` and checks `declared_app` is the fallback; say which you did.

The spec's Testing section asks for a field in each of the four groups, so loop the assertion over one field from each:

```rust
for (key, value) in [
    ("autorestart", json!(false)),        // control
    ("cwd", json!("/srv/web")),           // process
    ("args", json!(["--port", "8080"])),  // inputs
    ("cron_restart", json!("0 4 * * *")), // cron
] {
    let edit = PaneEdit::Set { key: key.into(), value };
    let app = pane.declared_app(&edit).unwrap();
    assert_eq!(app.declared, [key.to_owned()].into_iter().collect(), "{key}");
    // ... send, then assert the envelope's reset and declared as above
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout`
Expected: PASS.

- [ ] **Step 7: prove non-vacuous.** In `declared_app`, change `declared` to `Default::default()`; grep; run; `a_declared_app_for_one_edit_declares_only_that_key` fails. Restore. Then change `ResetDepth::File` to `ResetDepth::None` in the effect runner; grep; run; the end-to-end test fails. Restore.

- [ ] **Step 8: commit.**

```bash
git add crates/shep-cli/src/lookout
git commit -m "feat(lookout): a sheep pane edit reaches the overrides store, and env is write-only"
```

Then run the task gate.

---

# Slice 2: the dog pane

### Task 7: the config lock moves where the daemon can hold it

**Files:**
- Create: `crates/shep-core/src/config_lock.rs`
- Modify: `crates/shep-core/src/lib.rs` (`pub mod config_lock;`)
- Modify: `crates/shep-cli/src/commands/shep_toml.rs:877-951` (delete the two definitions; `pub(super) use shep_core::config_lock::{ConfigLock, create_config_file};`)
- Modify: `crates/shep-cli/src/commands/dog_migration.rs:494` and every other caller `git grep -n 'ConfigLock::acquire\|create_config_file' crates/shep-cli` finds

**Interfaces:**
- Consumes: nothing.
- Produces, in `shep_core::config_lock`:
  ```rust
  pub struct ConfigLock { /* private */ }
  impl ConfigLock {
      pub fn acquire(path: &Path) -> std::io::Result<Self>;
  }
  pub fn create_config_file(parent: &Path) -> std::io::Result<tempfile::NamedTempFile>;
  ```

Decision 6, corrected. The daemon holds no lock on `dogs.toml`: `ConfigLock` and `create_config_file` are `pub(super)` in `shep-cli`, all three of today's writers live there, and the daemon only reads. `overrides.rs` in shep-core has its own `OverridesLock`, a sibling-lockfile `flock(2)` on unix and `share_mode(0)` on Windows. **Move `ConfigLock` rather than adopt `OverridesLock`**, because the CLI's lock ordering (`shep.toml` outer, `dogs.toml` inner, per `dog_migration.rs`'s header) is written against `ConfigLock`'s semantics and three call sites already hold it. Consolidating the two lock types is a later pass; say so in your report.

- [ ] **Step 1: the failing test.**

```rust
// crates/shep-core/src/config_lock.rs, mod tests

#[test]
fn a_second_acquire_on_the_same_path_blocks_until_the_first_drops() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dogs.toml");
    let first = ConfigLock::acquire(&path).unwrap();
    let path2 = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let _second = ConfigLock::acquire(&path2).unwrap();
        tx.send(()).unwrap();
    });
    assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(), "must block");
    drop(first);
    rx.recv_timeout(std::time::Duration::from_secs(5)).expect("must proceed once released");
    t.join().unwrap();
}

#[test]
fn a_staged_config_file_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = create_config_file(dir.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = tmp.as_file().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
```

Run: `cargo test -p shep-core --lib config_lock`
Expected: FAIL, module not found.

- [ ] **Step 2: move the code verbatim.** Copy `ConfigLock`, both `acquire` impls, and `create_config_file` from `shep_toml.rs:877-951` into the new file with `pub` visibility, keeping every comment. Add `pub mod config_lock;` to `shep-core/src/lib.rs`. Replace the originals in `shep_toml.rs` with `pub(super) use shep_core::config_lock::{ConfigLock, create_config_file};`. If `create_config_file` reads `OWNER_ONLY_FILE_MODE` from `atomic_file.rs`, import it from there rather than duplicating the constant.

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS, every existing test unchanged.

- [ ] **Step 3: prove non-vacuous.** In `create_config_file`, drop the permission-setting call; grep; run; `a_staged_config_file_is_owner_only` fails. Restore.

- [ ] **Step 4: commit.**

```bash
git add crates/shep-core/src/config_lock.rs crates/shep-core/src/lib.rs crates/shep-cli/src/commands/shep_toml.rs
git commit -m "refactor(core): the config lock moves where both the daemon and the CLI can hold it"
```

### Task 8: the daemon writes a dog's section and publishes

**Files:**
- Modify: `crates/shep-daemon/src/dogs.rs:341` (beside `dog_section`)
- Modify: `crates/shep-daemon/src/rpc.rs` (replace Task 4's placeholder arm)

**Interfaces:**
- Consumes: `ConfigLock`, `create_config_file` (Task 7); `Request::SetDogConfig`, `Response::DogConfigSet` (Task 4); `publish_dog_config_changed` (`bus.rs:160`); `DogsConfig::load` (`shep-core/src/config/dogs.rs:42`).
- Produces, in `shep_daemon::dogs`:
  ```rust
  pub fn set_dog_section(path: &Path, name: &str, section: &str) -> Result<(), DogError>;
  ```

Decision 6. In `rpc.rs`, not the actor: `RpcContext` carries `events: Bus` and `dogs_config: PathBuf`, both in scope where dispatch runs, and `dogs.toml` is not supervisor state.

The section arrives as TOML text (the whole `[<name>]` table's body). The daemon parses the existing file with `toml_edit::DocumentMut`, parses the incoming text as its own document and takes its root table, replaces `doc[name]`, and validates the rendered result with `DogsConfig::load` before writing, so the daemon never writes a file it cannot read back. Comments outside the replaced table survive because `toml_edit` preserves them; comments inside it are the pane's to preserve, which Task 9 does by editing the text it was given rather than regenerating it.

- [ ] **Step 1: the failing tests.**

```rust
// crates/shep-daemon/src/dogs.rs, mod tests

#[test]
fn set_dog_section_replaces_one_table_and_leaves_the_rest_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dogs.toml");
    std::fs::write(&path, "# top comment\n[metrics]\nbind = \"127.0.0.1:9100\"\n\n[bark]\npoll = \"60s\"\n").unwrap();
    set_dog_section(&path, "bark", "poll = \"30s\"\nhistory_bytes = 4096\n").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# top comment"), "{text}");
    assert!(text.contains("bind = \"127.0.0.1:9100\""), "{text}");
    assert!(text.contains("poll = \"30s\""), "{text}");
    assert!(!text.contains("poll = \"60s\""), "{text}");
    let parsed = shep_core::config::DogsConfig::load(Some(&text)).unwrap();
    assert_eq!(parsed.dog["bark"]["history_bytes"].as_integer(), Some(4096));
}

#[test]
fn set_dog_section_creates_the_file_when_there_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dogs.toml");
    set_dog_section(&path, "bark", "poll = \"30s\"\n").unwrap();
    let parsed = shep_core::config::DogsConfig::load(Some(&std::fs::read_to_string(&path).unwrap())).unwrap();
    assert!(parsed.dog.contains_key("bark"));
}

#[test]
fn set_dog_section_refuses_text_that_is_not_a_table_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dogs.toml");
    std::fs::write(&path, "[bark]\npoll = \"60s\"\n").unwrap();
    let err = set_dog_section(&path, "bark", "this is = = not toml").unwrap_err();
    assert!(err.to_string().contains("bark"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "[bark]\npoll = \"60s\"\n");
}

#[cfg(unix)]
#[test]
fn set_dog_section_writes_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dogs.toml");
    set_dog_section(&path, "bark", "poll = \"30s\"\n").unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}
```

And in `rpc.rs`'s `mod tests`, replacing Task 4's placeholder check:

```rust
#[tokio::test]
async fn set_dog_config_writes_the_file_and_a_subscriber_hears_about_it() {
    let ctx = /* the DogConfig fixture, with ctx.dogs_config pointing at a tempdir file */;
    let mut sub = ctx.events.subscribe(); // guess: Bus derefs to broadcast::Sender
    let reply = dispatch(&ctx, Request::SetDogConfig {
        name: "bark".into(),
        toml: "poll = \"30s\"\n".to_owned().into(),
    }).await;
    assert!(matches!(reply, Ok(Response::DogConfigSet { .. })), "{reply:?}");
    let text = std::fs::read_to_string(&ctx.dogs_config).unwrap();
    assert!(text.contains("poll = \"30s\""));
    let event = tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .expect("the topic must be published")
        .unwrap();
    assert_eq!(event.topic(), "config.dog.bark"); // guess: SharedEvent exposes the BusEvent
}
```

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow:: dogs`
Expected: FAIL, `set_dog_section` not found.

- [ ] **Step 2: `set_dog_section`.**

```rust
/// Replaces `name`'s table in `dogs.toml` with `section` and writes the
/// file back, owner-only, under the same lock the CLI's writers hold.
///
/// The file is hand-editable on purpose, so this reads, modifies and writes
/// under the lock rather than overwriting from memory, and a table other
/// than `name`'s is untouched byte for byte. The rendered result is parsed
/// by `DogsConfig::load` before the write, so a section this daemon cannot
/// read back never reaches disk.
///
/// # Errors
/// - [`DogError::Io`]: the lock, the read, the staging file, or the rename.
/// - [`DogError::Config`]: `section` is not a TOML table, or the result does
///   not parse as a `DogsConfig`. Nothing has been written.
pub fn set_dog_section(path: &Path, name: &str, section: &str) -> Result<(), DogError> {
    use std::io::Write as _;
    use toml_edit::{DocumentMut, Item};

    let _lock = shep_core::config_lock::ConfigLock::acquire(path).map_err(DogError::Io)?;
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(DogError::Io(err)),
    };
    let mut doc: DocumentMut = existing
        .parse()
        .map_err(|e: toml_edit::TomlError| DogError::Config(format!("dogs.toml does not parse: {e}")))?;
    let incoming: DocumentMut = section
        .parse()
        .map_err(|e: toml_edit::TomlError| DogError::Config(format!("[{name}] does not parse: {e}")))?;
    doc[name] = Item::Table(incoming.as_table().clone());
    let rendered = doc.to_string();
    shep_core::config::DogsConfig::load(Some(&rendered))
        .map_err(|e| DogError::Config(format!("[{name}] would not load: {e}")))?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = shep_core::config_lock::create_config_file(parent).map_err(DogError::Io)?;
    tmp.write_all(rendered.as_bytes()).map_err(DogError::Io)?;
    tmp.as_file().sync_all().map_err(DogError::Io)?;
    tmp.persist(path).map_err(|e| DogError::Io(e.error))?;
    shep_core::atomic_file::sync_dir(parent).map_err(DogError::Io)?;
    Ok(())
}
```

`DogError` (guess: `dogs.rs:93-98` shows `NoBinary` and `UnsupportedSource`; there is an `Io` variant used at `:345`) gains a `Config(String)` variant if it has none, with a `Display` that prints the string. `Item::Table(incoming.as_table().clone())`: `DocumentMut::as_table()` is a guess for the root-table accessor in the pinned toml_edit; grep the version in `Cargo.lock` and its docs.

- [ ] **Step 3: the dispatch arm.** Replace Task 4's placeholder:

```rust
Request::SetDogConfig { name, toml } => {
    match crate::dogs::set_dog_section(&ctx.dogs_config, &name, toml.as_str()) {
        Ok(()) => {
            crate::bus::publish_dog_config_changed(&ctx.events, std::slice::from_ref(&name));
            reply(Ok(Response::DogConfigSet { name }))
        }
        Err(err) => reply(Err(RpcError {
            code: RpcErrorCode::InvalidConfig,
            message: err.to_string(),
            daemon_version: None,
        })),
    }
}
```

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS.

- [ ] **Step 4: prove non-vacuous.** Delete the `publish_dog_config_changed` call; grep; run; the subscriber test fails at its timeout. Restore. Then delete the `DogsConfig::load` validation; grep; run; `set_dog_section_refuses_text_that_is_not_a_table_and_writes_nothing` fails (the garbage is a parseable but wrong document, or the write lands). Restore.

- [ ] **Step 5: commit.**

```bash
git add crates/shep-daemon/src/dogs.rs crates/shep-daemon/src/rpc.rs
git commit -m "feat(daemon): a dog's section can be written over the wire, and the dog is told"
```

Then run the task gate.

### Task 9: the dog pane, and the docs

**Files:**
- Modify: `crates/shep-cli/src/dog/mod.rs` (`builtin_schema`)
- Modify: `crates/shep-cli/src/commands/dogs.rs:946` (`ask_schema` becomes `pub(crate)`)
- Modify: `crates/shep-cli/src/lookout/pane.rs` (`ConfigPane::dog`, `PaneEdit` applies to a TOML section)
- Modify: `crates/shep-cli/src/lookout/app.rs` (`Edit` on a settings dog row; `Effect::LoadDogPane`, `Effect::SetDogConfig`; `Msg::DogPane`, `Msg::DogConfigSet`)
- Modify: `crates/shep-cli/src/lookout/mod.rs` (run the effects: probe the schema, fetch the section, send the write)
- Modify: `web/src/pages/docs/lookout.astro`, `web/src/pages/docs/overrides.astro`, `web/src/pages/docs/dogs.astro`, `web/src/pages/docs/getting-started.astro`, `docs/dogs.md`

**Interfaces:**
- Consumes: everything above. `ask_schema(path, home, name, budget) -> DogSchema` (`commands/dogs.rs:946`); `VERSION_BUDGET` (`:1017`); `DogSchema::{Published, Silent, Unreadable}` (`:996`); `shep_client::dogs::config_schema::<T>()`; `Request::DogConfig`/`Response::DogSection` (existing); `Request::SetDogConfig`/`Response::DogConfigSet` (Task 4, live since Task 8).
- Produces, in `crate::dog`: `pub(crate) fn builtin_schema(name: &str) -> Option<serde_json::Value>`. On `ConfigPane`: `pub fn dog(name: String, adopted_path: Option<PathBuf>, schema: serde_json::Value, section: String) -> Self` and `pub fn edited_section(&self, edit: &PaneEdit) -> Option<String>`.

Decisions 3, 6, 10. A dog's schema is not persisted: `adopt` uses `DogSchema::Published(Value)` for the vet and records only the path. So the pane probes at open. An adopted dog is asked `--schema` through `ask_schema`; a built-in is asked in-process through `config_schema::<T>()`, since it is this binary.

- [ ] **Step 1: `builtin_schema`.**

```rust
// crates/shep-cli/src/dog/mod.rs

/// The schema a built-in dog would print for `--schema`, without spawning:
/// it is this binary, so the answer is one call away.
///
/// `None` for a name that is not a built-in, which is how a caller tells
/// an adopted dog (probe its path) from a built-in (this).
pub(crate) fn builtin_schema(name: &str) -> Option<serde_json::Value> {
    use shep_client::dogs::config_schema;
    let schema = match name {
        "metrics" => config_schema::<metrics::MetricsConfig>().ok()?,
        "bark" => config_schema::<bark::BarkConfig>().ok()?,
        _ => return None,
    };
    serde_json::to_value(schema).ok()
}

#[cfg(test)]
mod builtin_schema_tests {
    use super::*;

    #[test]
    fn both_built_ins_answer_and_a_stranger_does_not() {
        assert!(builtin_schema("metrics").is_some());
        let bark = builtin_schema("bark").unwrap();
        assert_eq!(bark["properties"]["sinks"][shep_core::dogs::SECRET_KEY], true);
        assert!(builtin_schema("otel").is_none());
    }
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: builtin_schema`
Expected: PASS.

- [ ] **Step 2: `ConfigPane::dog` and `edited_section`.**

```rust
impl ConfigPane {
    /// A pane over one dog's section. `schema` is the dog's own `--schema`
    /// answer, `section` its current `[<name>]` table as TOML text.
    ///
    /// Flat, in schema order (decision 3): no `group_order`.
    #[must_use]
    pub fn dog(name: String, adopted_path: Option<PathBuf>, schema: Value, section: String) -> Self {
        let defs = schema.get("$defs").and_then(Value::as_object).cloned().unwrap_or_default();
        let props = schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let fields = FieldSet::from_properties(&props, &defs, &[]);
        let values: Map<String, Value> = section
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| v.as_object().cloned())
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

    /// The section with `edit` applied, comments kept, for `SetDogConfig`.
    /// `None` for a sheep pane.
    #[must_use]
    pub fn edited_section(&self, edit: &PaneEdit) -> Option<String> {
        let section = self.section.as_deref()?;
        let PaneEdit::Set { key, value } = edit;
        let mut doc: toml_edit::DocumentMut = section.parse().ok()?;
        match value {
            Value::Null => { doc.remove(key); }
            Value::Bool(b) => doc[key] = toml_edit::value(*b),
            Value::Number(n) => doc[key] = toml_edit::value(n.as_i64()?),
            Value::String(s) => doc[key] = toml_edit::value(s.as_str()),
            other => doc[key] = toml_edit::value(other.to_string()),
        }
        Some(doc.to_string())
    }
}
```

`ConfigPane` gains `section: Option<String>`. Tests:

```rust
#[test]
fn a_dog_pane_is_flat_in_schema_order_and_marks_the_secret() {
    let schema = crate::dog::builtin_schema("bark").unwrap();
    let pane = ConfigPane::dog("bark".into(), None, schema, "poll = \"60s\"\n".into());
    assert!(pane.fields().groups().is_empty());
    assert!(pane.fields().by_key("sinks").unwrap().secret);
    assert_eq!(pane.value("poll"), "60s");
    assert_eq!(pane.cost("poll"), None);
}

#[test]
fn an_edited_section_keeps_its_comments_and_changes_one_key() {
    let schema = crate::dog::builtin_schema("bark").unwrap();
    let section = "# how often\npoll = \"60s\"\nhistory_bytes = 4096\n";
    let pane = ConfigPane::dog("bark".into(), None, schema, section.into());
    let out = pane.edited_section(&PaneEdit::Set { key: "poll".into(), value: json!("30s") }).unwrap();
    assert!(out.contains("# how often"), "{out}");
    assert!(out.contains("poll = \"30s\""), "{out}");
    assert!(out.contains("history_bytes = 4096"), "{out}");
}
```

- [ ] **Step 3: keys, effects, messages.**

On the settings screen, `KeyPress::Edit` with the cursor on `SettingsRow::Dog(i)` raises `Effect::LoadDogPane { name, adopted_path }` from `snapshot.dogs[i]`. The effect runner: `builtin_schema(&name)` first; if `None` and `adopted_path` is `Some(p)`, `ask_schema(&p, home, &name, VERSION_BUDGET)` and take `DogSchema::Published(v)`; anything else is "no schema" (decision 10). With a schema, send `Request::DogConfig { name }` and post `Msg::DogPane { result: Ok((schema, section)) }`; without, post `Err("<name> publishes no schema; edit dogs.toml with $EDITOR")`.

`Msg::DogPane`: `Ok((schema, section))` builds `ConfigPane::dog(...)` and closes the settings screen; `Err(m)` shows `m` on the status line and stays.

The `Confirm` arm from Task 6's `PaneTarget::Dog { .. } => Effect::None` becomes: `pane.edited_section(&edit)` on the effect as `Effect::SetDogConfig { name, toml, authority }`, which sends `Request::SetDogConfig` and posts `Msg::DogConfigSet { result }`. On `Ok`, `settle()` and reload the section via a fresh `Effect::LoadDogPane`.

`cycle` and `begin_typing` already refuse a non-editable field; a `secret` field's `begin_typing` seeds empty (Task 6 wrote it that way) so a webhook is replaced and never shown.

- [ ] **Step 4: a rendered frame.**

```rust
#[test]
fn a_dog_pane_at_a_comfortable_width() {
    let app = fixtures::app();
    let schema = crate::dog::builtin_schema("bark").unwrap();
    let pane = ConfigPane::dog("bark".into(), None, schema,
        "poll = \"60s\"\n[sinks.ops]\nkind = \"slack\"\nurl = \"https://hooks.example/x\"\n".into());
    let lines = pane_lines(&app, &pane, app.palette(), 120);
    let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
    assert!(!text.iter().any(|l| l.contains("hooks.example")), "a secret is never rendered: {text:?}");
    assert!(text.iter().any(|l| l.contains("<set>")));
    assert!(text.iter().any(|l| l.contains("decides what to reload")));
    insta::assert_snapshot!("dog_pane_wide", text.join("\n"));
}
```

Run: `cargo test -p shep --lib --all-features -- --skip ::slow:: lookout`
Expected: PASS.

- [ ] **Step 5: prove non-vacuous.** In `field_line`, render the value for a secret field instead of `<set>`; grep; run; `a_dog_pane_at_a_comfortable_width` fails on the `hooks.example` assertion. Restore.

- [ ] **Step 6: the docs.** Five pages, and the rule for each is to change what became false and add what became true, in that page's existing voice. Grep each for the thing you are changing before assuming it is fine.

- `web/src/pages/docs/lookout.astro`: the `e` key, both panes, the cost column, the env sub-screen, and that a dog with no schema gets a message rather than a pane.
- `web/src/pages/docs/overrides.astro`: the sheep pane as a way to set an override, and that a pane edit uses `--reset=file` semantics for one key.
- `docs/dogs.md` and `web/src/pages/docs/dogs.astro`: what publishing a schema now buys, which is a pane. The "Answering is optional" paragraph's "what it gives up is a settings pane later" becomes present tense.
- `web/src/pages/docs/getting-started.astro`: the protocol is 4 and `shep daemon reload` after upgrading.

Then the web half of the gate: `cargo build --release`, `./web/scripts/generate-cli-reference.sh` (expect no diff: no verb's help text moved), `npx astro build`, `npx astro check`.

- [ ] **Step 7: commit, in two.**

```bash
git add crates/shep-cli/src
git commit -m "feat(lookout): a config pane for a dog, probing its schema at open"
git add docs/dogs.md web/src/pages/docs
git commit -m "docs(web): both config panes, the e key, and protocol 4"
```

Then run the full task gate including the web half.

---

## Out of scope, deliberately

- **Unifying `Pending`, `PanePending` and `Action`/`Stage`.** lookout now has three confirm mechanisms sharing only `CONFIRM_EXPIRY`. Task 3 and Task 6 both say so in their reports. Worth its own pass, and this plan's author flagged it rather than folding it in.
- **Consolidating `ConfigLock` and `OverridesLock`.** Two sibling-lockfile schemes in shep-core after Task 7. Task 7's report names it.
- **Recursing into nested objects.** `FieldKind::Opaque` covers them read-only.
- **A live-versus-needs-restart axis for dog fields.** The dog spec owns that and left it out.
- **Showing an env reference in full.** Decision 12 allows a `{{shared:X}}` reference to be shown, but no request returns enough to tell a reference from a literal, so every env value renders `<set>` for now.

## Self-review

**Spec coverage.** Every decision, and the task that carries it:

| decision | task |
| --- | --- |
| 1, one renderer three sources | 1, 3, 5, 9 |
| 2, sheep sections by `init.group` | 5 |
| 3, dog flat | 9 |
| 4, cost badge, confirm on respawn, none for a dog | 5, 6, 9 |
| 5, Structural read-only | 5 |
| 6, daemon writes `dogs.toml`, needs a lock | 7, 8 |
| 6b, the sheep read request | 4 |
| 7, `PROTOCOL_VERSION` 4 | 4 |
| 8, `e` on the selection | 5, 9 |
| 8b, scrolling | 2 |
| 9, env write-only sub-screen | 4 (`SetSheepEnv`), 6 |
| 10, no schema no pane | 9 |
| Wire: missing handler is silent | 4 step 6 |
| Testing: subscriber receives the event | 8 |
| Testing: older daemon refuses at the handshake | existing test moves in 4 step 7 |
| Docs | 9 |

Gap found and closed during the review: the spec's Testing section asks that "a sheep field in each of the four groups lands through `ApplyConfig`". Task 6 step 6 originally tested one; it now loops over `autorestart` (control), `cwd` (process), `args` (inputs) and `cron_restart` (cron).

**Placeholder scan.** No TBD, TODO, "implement later". Every "guess" is labelled and paired with a grep. Two places say "copy the fixture": Task 4 step 6 and Task 5 step 5. Both name the exact line to copy from, which is what the rule permits.

**Type consistency.** `FieldSet::from_properties(&Map, &Map, &[&str])` in Task 1 is called that way in 5 and 9. `Viewport::{move_by(isize, usize), move_to(usize, usize), set_rows(usize)}` in Task 2 is called that way in 3, 5, 6. `SheepConfigView::new(AppConfig, Vec<String>, Vec<String>)` in Task 4 is called that way in 5's tests. `PaneEdit::Set { key, value }` in Task 6 is what Task 9's `edited_section` matches. `set_dog_section(&Path, &str, &str)` in Task 8 matches its dispatch arm. `builtin_schema(&str) -> Option<Value>` in Task 9 matches both its callers.
