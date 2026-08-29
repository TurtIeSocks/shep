//! Rendering apps as Flockfile TOML.
//!
//! [`flockfile`] serializes a purpose-built projection of [`AppConfig`]
//! ([`Rendered`]), not the type itself: `AppConfig` is `#[serde(default)]`
//! across roughly forty fields, and an importer that serialized it directly
//! would write every one of them at its own spec default, burying the
//! handful an operator actually needs to read.

use std::collections::BTreeMap;

use serde::Serialize;
use shep_core::config::AppConfig;
use shep_core::values::{MemSize, UpDuration};

/// The subset of an `AppConfig` a pm2 import can produce, rendered as one
/// `[[app]]` table. Every field is skipped when it matches the spec
/// default, so an imported Flockfile reads as what the operator has to
/// know about rather than as a dump of every knob shep has.
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
    // LAST for readability, not correctness: a hand-written `[[app]]` table
    // cannot have a scalar key after a `[app.env]` sub-table header, but
    // `toml` 0.8's `Serializer` does not share that constraint — verified
    // empirically (moved this field to the front of the struct and it
    // still round-tripped) — it buffers every table-typed field and always
    // emits it after the scalars, regardless of Rust declaration order.
    // Kept last anyway so the rendered file reads scalars-then-map, the way
    // a person would write it by hand.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

/// `true` when `instances` is the spec default (1) — the value every
/// non-cluster app carries, and cluster apps carry the row count instead.
fn is_one(instances: &u32) -> bool {
    *instances == 1
}

/// `true` when `value` is `true` — `autorestart`'s spec default, the one
/// field here whose default is `true` rather than `false`.
fn is_true(value: &bool) -> bool {
    *value
}

/// `true` when `value` is `false` — the spec default for `merge_logs` and
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
/// `apps` is `#[serde(rename = "app")]` on purpose: `Flockfile`'s own
/// public field is named `apps`, which is what makes the wrong key easy to
/// write here, but the document key `Flockfile::parse` requires is `app` —
/// `RawFlockfile` (`shep-core/src/config/flockfile.rs`) is
/// `#[serde(deny_unknown_fields)]`, so a document keyed `apps` is one shep
/// refuses to read back.
#[derive(Debug, Serialize)]
struct Doc {
    #[serde(rename = "app")]
    apps: Vec<Rendered>,
}

/// Renders apps as Flockfile TOML: one `[[app]]` table each, carrying only
/// the fields that differ from a spec default.
///
/// # Errors
/// [`toml::ser::Error`] — the `toml` backend refused to serialize the
/// document. Not expected in practice: every field [`Rendered`] carries is
/// either a plain scalar or a newtype whose `Serialize` collapses to a
/// string, and `toml`'s own serializer emits table-typed fields (`env`)
/// after every scalar regardless of struct declaration order (see
/// [`Rendered`]'s own doc on that field).
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

    /// fails if the renderer emits a Flockfile shep cannot read back — a
    /// wrong document key (`apps` for `app`), a field name that drifted
    /// from `AppConfig`, or a value shep cannot parse in its own grammar
    /// (a raw integer where `MemSize`/`UpDuration` need a string, say). It
    /// parses with the REAL parser and compares against the apps that went
    /// in, so the projection cannot drift from `AppConfig` without this
    /// reddening.
    #[test]
    fn flockfile_round_trips_through_the_real_parser() {
        let apps = convert(dump::parse(include_str!("testdata/dump.pm2.json")).unwrap())
            .unwrap()
            .apps;
        let rendered = flockfile(&apps).unwrap();
        let parsed = Flockfile::parse(&rendered, FlockFormat::Toml).unwrap();
        assert_eq!(parsed.apps, apps);
    }

    /// fails if a spec default starts being written. An imported Flockfile
    /// listing all forty of shep's knobs is one nobody reads, and the two
    /// lines that matter — `reuse_port` and `max_memory` — are what gets
    /// lost in it.
    #[test]
    fn defaults_are_left_out() {
        let rendered = flockfile(&[AppConfig::minimal("web", "./srv")]).unwrap();
        assert_eq!(
            rendered.trim(),
            "[[app]]\nname = \"web\"\nscript = \"./srv\""
        );
    }

    /// fails if a value shep writes stops being one shep can parse back:
    /// 536870912 bytes must render as `512M` and 5000 ms as `5s`, because
    /// `MemSize` and `UpDuration` serialize as their string forms and a
    /// renderer emitting raw integers would produce a Flockfile whose
    /// `max_memory = 536870912` is a TOML integer where a string is
    /// expected.
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
