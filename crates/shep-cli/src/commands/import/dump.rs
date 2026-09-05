//! Reading a pm2 dump into rows.
//!
//! A dump is a JSON array of instance rows, one per running process: a
//! clustered app comes back as several rows sharing a `name`. A row is flat,
//! its config fields and the whole process environment splatted together at
//! the top level. The row's `env` object holds that environment plus one
//! `env_<mode>` object per ecosystem-file block; its keys being a superset of
//! the splatted ones is what bounds env against config.
//!
//! [`parse`] reads through [`serde_json::Value`]: `#[serde(flatten)]` would
//! need a catch-all map for the `env_<name>` keys, which a row carrying the
//! environment as siblings defeats.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// One instance row out of a pm2 dump.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DumpRow {
    /// The app's name, shared by every instance of a clustered app.
    pub name: String,
    /// The script or binary pm2 was told to run.
    pub pm_exec_path: String,
    /// Arguments passed to `pm_exec_path`.
    pub args: Vec<String>,
    /// The working directory the process ran from, if the row named one.
    pub pm_cwd: Option<String>,
    /// `"node"`, `"bun"`, `"none"` (exec the binary directly), and so on.
    pub exec_interpreter: Option<String>,
    /// `"fork_mode"` or `"cluster_mode"`.
    pub exec_mode: Option<String>,
    /// Whether pm2 was told to restart the process on exit.
    pub autorestart: Option<bool>,
    /// Milliseconds pm2 waits before restarting the process.
    pub restart_delay: Option<u64>,
    /// Whether pm2 merged this process's stdout and stderr into one log.
    pub merge_logs: Option<bool>,
    /// The memory ceiling, in bytes, past which pm2 restarts the process.
    pub max_memory_restart: Option<u64>,
    /// The row's `env` map: what the process is actually running with.
    pub env: BTreeMap<String, String>,
    /// Every `env_<name>` map, keyed by the suffix. These hold only what the
    /// ecosystem file declared.
    pub declared: BTreeMap<String, BTreeMap<String, String>>,
    /// Keys dropped because their value was neither a string, a number, nor
    /// a boolean, none of which a Flockfile env can hold.
    pub unrepresentable: Vec<String>,
}

/// Why [`parse`] failed to read a dump.
#[derive(Debug)]
pub(crate) enum DumpError {
    /// The document is not valid JSON, carrying `serde_json`'s message.
    Json(String),
    /// The document parsed as JSON but is not the array of instance rows a
    /// dump is.
    NotAnArray,
    /// A row carries no `name`.
    RowMissingName {
        /// The row's position in the dump array.
        index: usize,
    },
    /// A row carries no `pm_exec_path`. Reported, never skipped: a dropped
    /// row is an app missing from the Flockfile.
    RowMissingScript {
        /// The row's position in the dump array.
        index: usize,
        /// The row's `name`.
        name: String,
        /// The keys the row did carry, sorted and truncated to 20.
        keys: Vec<String>,
    },
}

impl core::fmt::Display for DumpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Json(message) => write!(f, "not valid JSON: {message}"),
            Self::NotAnArray => f.write_str("not a pm2 dump: expected an array of instance rows"),
            Self::RowMissingName { index } => write!(f, "row {index} carries no `name`"),
            Self::RowMissingScript { index, name, keys } => write!(
                f,
                "row {index} (`{name}`) carries no `pm_exec_path`; found: {keys:?}"
            ),
        }
    }
}

impl core::error::Error for DumpError {}

/// Parses a whole dump document.
///
/// # Errors
/// - [`DumpError::Json`] if the document is not valid JSON.
/// - [`DumpError::NotAnArray`] if it is valid JSON but not an array of rows.
/// - [`DumpError::RowMissingName`] if a row carries no `name`.
/// - [`DumpError::RowMissingScript`] if a row carries no `pm_exec_path`.
pub(crate) fn parse(source: &str) -> Result<Vec<DumpRow>, DumpError> {
    let document: Value =
        serde_json::from_str(source).map_err(|err| DumpError::Json(err.to_string()))?;
    let rows = document.as_array().ok_or(DumpError::NotAnArray)?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| parse_row(index, row))
        .collect()
}

/// Reads one instance row out of a dump array.
fn parse_row(index: usize, row: &Value) -> Result<DumpRow, DumpError> {
    let fields = row.as_object();

    let name = string_field(fields, "name").ok_or(DumpError::RowMissingName { index })?;

    let pm_exec_path = string_field(fields, "pm_exec_path").ok_or_else(|| {
        let mut keys: Vec<String> = fields
            .map(|fields| fields.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys.truncate(20);
        DumpError::RowMissingScript {
            index,
            name: name.clone(),
            keys,
        }
    })?;

    let args = fields
        .and_then(|fields| fields.get("args"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let mut unrepresentable = Vec::new();

    let env = fields
        .and_then(|fields| fields.get("env"))
        .and_then(Value::as_object)
        .map(|env_fields| stringify_map(env_fields, &mut unrepresentable, |key| key.to_owned()))
        .unwrap_or_default();

    let declared = fields
        .map(|fields| declared_envs(fields, &mut unrepresentable))
        .unwrap_or_default();

    Ok(DumpRow {
        name,
        pm_exec_path,
        args,
        pm_cwd: string_field(fields, "pm_cwd"),
        exec_interpreter: string_field(fields, "exec_interpreter"),
        exec_mode: string_field(fields, "exec_mode"),
        autorestart: bool_field(fields, "autorestart"),
        restart_delay: u64_field(fields, "restart_delay"),
        merge_logs: bool_field(fields, "merge_logs"),
        max_memory_restart: u64_field(fields, "max_memory_restart"),
        env,
        declared,
        unrepresentable,
    })
}

/// Collects every `env_<suffix>` object on a row's top level into a map
/// keyed by the suffix. An `env_` key whose value is not a JSON object is
/// skipped.
fn declared_envs(
    fields: &Map<String, Value>,
    unrepresentable: &mut Vec<String>,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut declared = BTreeMap::new();
    for (key, value) in fields {
        let Some(suffix) = key.strip_prefix("env_").filter(|suffix| !suffix.is_empty()) else {
            continue;
        };
        let Some(env_fields) = value.as_object() else {
            continue;
        };
        let inner = stringify_map(env_fields, unrepresentable, |inner_key| {
            format!("env_{suffix}.{inner_key}")
        });
        declared.insert(suffix.to_owned(), inner);
    }
    declared
}

/// Stringifies every scalar value in a JSON object, naming the rest.
///
/// A string, number, or boolean becomes its string form: an ecosystem file's
/// `PORT: 3000` arrives as a JSON number. Anything else is dropped and its
/// key pushed onto `unrepresentable`, labelled by `label_for`.
fn stringify_map(
    fields: &Map<String, Value>,
    unrepresentable: &mut Vec<String>,
    label_for: impl Fn(&str) -> String,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in fields {
        match scalar_string(value) {
            Some(string) => {
                out.insert(key.clone(), string);
            }
            None => unrepresentable.push(label_for(key)),
        }
    }
    out
}

/// The string form of a JSON scalar, or `None` for an object, array, or
/// `null`.
fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

/// A required-or-optional string field read directly off a row's top level.
fn string_field(fields: Option<&Map<String, Value>>, key: &str) -> Option<String> {
    fields
        .and_then(|fields| fields.get(key))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// A boolean field read directly off a row's top level.
fn bool_field(fields: Option<&Map<String, Value>>, key: &str) -> Option<bool> {
    fields
        .and_then(|fields| fields.get(key))
        .and_then(Value::as_bool)
}

/// A non-negative integer field read directly off a row's top level.
fn u64_field(fields: Option<&Map<String, Value>>, key: &str) -> Option<u64> {
    fields
        .and_then(|fields| fields.get(key))
        .and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("testdata/dump.pm2.json");

    #[test]
    fn the_fixture_parses_into_four_rows_with_their_fields() {
        let rows = parse(FIXTURE).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].name, "api");
        assert_eq!(rows[0].pm_exec_path, "/srv/api/dist/server.js");
        assert_eq!(rows[0].args, ["--port", "8080"]);
        assert_eq!(rows[0].pm_cwd.as_deref(), Some("/srv/api"));
        assert_eq!(rows[0].exec_interpreter.as_deref(), Some("node"));
        assert_eq!(rows[0].exec_mode.as_deref(), Some("cluster_mode"));
        assert_eq!(rows[0].max_memory_restart, Some(536_870_912));
        assert_eq!(rows[2].restart_delay, Some(5000));
        assert_eq!(rows[2].autorestart, Some(false));
        assert_eq!(rows[2].merge_logs, Some(true));
    }

    #[test]
    fn a_row_with_no_script_is_a_named_failure() {
        let odd = r#"[{"name":"web","script":"/srv/web"}]"#;
        let err = parse(odd).unwrap_err();
        let DumpError::RowMissingScript { index, name, keys } = err else {
            panic!("expected RowMissingScript, got {err:?}")
        };
        assert_eq!(index, 0);
        assert_eq!(name, "web");
        assert!(keys.iter().any(|k| k == "script"), "{keys:?}");
    }

    /// `QUEUE_CONCURRENCY: 4` is a number in the fixture, as `PORT: 3000` is
    /// in an ecosystem file.
    #[test]
    fn declared_env_scalars_are_stringified_and_the_rest_is_named() {
        let rows = parse(FIXTURE).unwrap();
        let worker = &rows[2];
        assert_eq!(worker.declared["staging"]["QUEUE_CONCURRENCY"], "4");
        let nested = r#"[{"name":"w","pm_exec_path":"/w","env":{"OPTS":{"a":1}}}]"#;
        let rows = parse(nested).unwrap();
        assert!(rows[0].env.is_empty());
        assert_eq!(rows[0].unrepresentable, ["OPTS"]);
    }

    #[test]
    fn a_document_that_is_not_an_array_is_refused() {
        assert!(matches!(parse("{}"), Err(DumpError::NotAnArray)));
        assert!(matches!(parse("not json"), Err(DumpError::Json(_))));
    }
}
