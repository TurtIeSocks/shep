//! Rendering apps as Flockfile TOML.
//!
//! [`flockfile`] serializes a projection of [`AppConfig`] ([`Rendered`]),
//! not the type itself: `AppConfig` is `#[serde(default)]` across roughly
//! forty fields, and serializing it directly would write every one of them
//! at its spec default.

use std::collections::BTreeMap;

use serde::Serialize;
use shep_core::config::AppConfig;
use shep_core::values::{MemSize, UpDuration};

/// The subset of an `AppConfig` a pm2 import can produce, rendered as one
/// `[[app]]` table. Every field is skipped when it matches the spec default.
#[derive(Debug, Serialize)]
struct Rendered {
    name: String,
    script: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interpreter: Option<String>,
    #[serde(skip_serializing_if = "is_one")]
    instances: u32,
    #[serde(skip_serializing_if = "is_true")]
    autorestart: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    restart_delay: Option<UpDuration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_memory: Option<MemSize>,
    #[serde(skip_serializing_if = "is_false")]
    merge_logs: bool,
    #[serde(skip_serializing_if = "is_false")]
    reuse_port: bool,
    // Last for readability, not correctness: `toml`'s serializer buffers
    // table-typed fields and emits them after the scalars whatever the
    // declaration order.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

/// `true` when `instances` is the spec default, 1.
fn is_one(instances: &u32) -> bool {
    *instances == 1
}

/// `true` when `value` is `true`, which is `autorestart`'s spec default.
fn is_true(value: &bool) -> bool {
    *value
}

/// `true` when `value` is `false`, the spec default for `merge_logs` and
/// `reuse_port`.
fn is_false(value: &bool) -> bool {
    !*value
}

impl From<&AppConfig> for Rendered {
    fn from(app: &AppConfig) -> Self {
        Self {
            name: app.name.clone(),
            script: app.script.clone(),
            args: app.args.clone(),
            cwd: app.cwd.clone(),
            interpreter: app.interpreter.clone(),
            instances: app.instances,
            autorestart: app.autorestart,
            restart_delay: app.restart_delay,
            max_memory: app.max_memory,
            merge_logs: app.merge_logs,
            reuse_port: app.reuse_port,
            env: app.env.clone(),
        }
    }
}

/// The whole document: one `[[app]]` table per app.
///
/// The document key `Flockfile::parse` requires is `app`, though
/// `Flockfile`'s own field is `apps`. `RawFlockfile` is
/// `#[serde(deny_unknown_fields)]`, so a document keyed `apps` does not parse.
#[derive(Debug, Serialize)]
struct Doc {
    #[serde(rename = "app")]
    apps: Vec<Rendered>,
}

/// Renders apps as Flockfile TOML: one `[[app]]` table each, carrying only
/// the fields that differ from a spec default.
///
/// # Errors
/// [`toml::ser::Error`] if the `toml` backend refuses the document.
pub(crate) fn flockfile(apps: &[AppConfig]) -> Result<String, toml::ser::Error> {
    let doc = Doc {
        apps: apps.iter().map(Rendered::from).collect(),
    };
    toml::to_string(&doc)
}

#[cfg(test)]
mod tests {
    use shep_core::config::{FlockFormat, Flockfile};

    use super::*;
    use crate::commands::import::convert::convert;
    use crate::commands::import::dump;

    #[test]
    fn flockfile_round_trips_through_the_real_parser() {
        let apps = convert(dump::parse(include_str!("testdata/dump.pm2.json")).unwrap())
            .unwrap()
            .apps;
        let rendered = flockfile(&apps).unwrap();
        let parsed = Flockfile::parse(&rendered, FlockFormat::Toml).unwrap();
        assert_eq!(parsed.apps, apps);
    }

    #[test]
    fn defaults_are_left_out() {
        let rendered = flockfile(&[AppConfig::minimal("web", "./srv")]).unwrap();
        assert_eq!(
            rendered.trim(),
            "[[app]]\nname = \"web\"\nscript = \"./srv\""
        );
    }

    #[test]
    fn newtype_values_render_in_their_string_form() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.max_memory = Some(MemSize::from_bytes(536_870_912));
        app.restart_delay = Some(UpDuration::from_millis(5000));
        let rendered = flockfile(&[app]).unwrap();
        assert!(rendered.contains("max_memory = \"512M\""), "{rendered}");
        assert!(rendered.contains("restart_delay = \"5s\""), "{rendered}");
    }
}
