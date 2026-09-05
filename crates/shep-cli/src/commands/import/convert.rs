//! Collapsing pm2 dump rows into shep apps.
//!
//! A dump is per-instance: a clustered app comes back as several
//! [`DumpRow`]s sharing a `name`. [`convert`] groups them into one
//! [`AppConfig`] per app, the row count becoming `instances`, the first row
//! winning every scalar including `env`.
//!
//! `exec_mode == "cluster_mode"` maps to an [`ImportNote::ClusterMode`] and
//! nothing else: shep binds nothing, so N instances on one port is
//! `EADDRINUSE` unless the app sets `SO_REUSEPORT` itself.

use std::collections::HashMap;

use shep_core::config::{AppConfig, normalize};
use shep_core::values::{MemSize, UpDuration};

use super::dump::DumpRow;
use super::env;

/// What one dump became: the apps to write, and the notes for the operator.
#[derive(Debug)]
pub(crate) struct Imported {
    /// One entry per app name, in the order the dump first mentions it.
    pub apps: Vec<AppConfig>,
    /// One per thing the operator decides, in app order.
    pub notes: Vec<ImportNote>,
}

/// Something the import cannot decide on the operator's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportNote {
    /// The app ran in pm2 cluster mode; shep binds nothing, so the app
    /// itself must set `SO_REUSEPORT`.
    ClusterMode {
        /// The app's name.
        app: String,
        /// How many instances the dump recorded as running.
        instances: u32,
    },
    /// An env key inherited from the starting shell, neither declared nor a
    /// known session-shell or pm2 key.
    InheritedEnv {
        /// The app's name.
        app: String,
        /// The inherited key.
        key: String,
    },
    /// An env value a Flockfile cannot hold.
    UnrepresentableEnv {
        /// The app's name.
        app: String,
        /// The key whose value could not be represented.
        key: String,
    },
    /// The app read its instance number from a pm2 variable, recorded as an
    /// `env` entry templated with `{{instance}}`.
    InstanceVar {
        /// The app's name.
        app: String,
        /// The pm2 variable name (`NODE_APP_INSTANCE`).
        var: String,
    },
}

/// A dump [`convert`] would not turn into apps.
#[derive(Debug)]
pub(crate) enum ConvertError {
    /// A mapped app does not normalize.
    Rejected {
        /// The app name.
        name: String,
        /// `NormalizeError`'s rendered reason.
        reason: String,
    },
}

impl core::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Rejected { name, reason } => {
                write!(f, "`{name}` does not normalize: {reason}")
            }
        }
    }
}

impl core::error::Error for ConvertError {}

/// Collapses instance rows into apps and maps every field this importer
/// knows how to map. Every returned [`AppConfig`] has already been through
/// [`shep_core::config::normalize()`].
///
/// # Errors
/// - [`ConvertError::Rejected`] if a mapped app does not normalize.
pub(crate) fn convert(rows: Vec<DumpRow>) -> Result<Imported, ConvertError> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<DumpRow>> = HashMap::new();
    for row in rows {
        groups
            .entry(row.name.clone())
            .or_insert_with(|| {
                order.push(row.name.clone());
                Vec::new()
            })
            .push(row);
    }

    let mut apps = Vec::with_capacity(order.len());
    let mut notes = Vec::new();
    for name in order {
        let group = groups
            .remove(&name)
            .expect("`order` names only keys just inserted into `groups`");
        let (app, app_notes) = convert_group(&name, &group);
        notes.extend(app_notes);
        let resolved = normalize(app).map_err(|err| ConvertError::Rejected {
            name: name.clone(),
            reason: err.to_string(),
        })?;
        apps.push(resolved.into_config());
    }

    Ok(Imported { apps, notes })
}

/// Builds one [`AppConfig`] from a name's instance rows, plus the notes the
/// mapping produced. `rows` is never empty.
fn convert_group(name: &str, rows: &[DumpRow]) -> (AppConfig, Vec<ImportNote>) {
    let mut notes = Vec::new();
    // `AppConfig::instances` is itself a u32.
    let instances = rows.len() as u32;
    let first = &rows[0];

    let mut app = AppConfig::minimal(name, &first.pm_exec_path);
    app.args = first.args.clone();
    app.cwd = first.pm_cwd.clone();
    // `exec_interpreter: "none"` means run the script directly, which in a
    // Flockfile is the absence of `interpreter`.
    app.interpreter = first
        .exec_interpreter
        .clone()
        .filter(|interpreter| interpreter != "none");
    if let Some(autorestart) = first.autorestart {
        app.autorestart = autorestart;
    }
    if let Some(restart_delay) = first.restart_delay {
        app.restart_delay = Some(UpDuration::from_millis(restart_delay));
    }
    if let Some(merge_logs) = first.merge_logs {
        app.merge_logs = merge_logs;
    }
    if let Some(max_memory_restart) = first.max_memory_restart {
        app.max_memory = Some(MemSize::from_bytes(max_memory_restart));
    }
    app.instances = instances;

    if first.exec_mode.as_deref() == Some("cluster_mode") {
        // No `reuse_port = true`: the field picks reload's mode, and a dump
        // cannot say whether the app sets `SO_REUSEPORT`.
        notes.push(ImportNote::ClusterMode {
            app: name.to_string(),
            instances,
        });
    }

    let app_env = env::split(first);
    app.env = app_env.env;
    if let Some(var) = app_env.instance_var {
        // The template, not the value: pm2 reported instance 0's number.
        app.env.insert(var.clone(), "{{instance}}".to_string());
        notes.push(ImportNote::InstanceVar {
            app: name.to_string(),
            var,
        });
    }
    for key in app_env.inherited {
        notes.push(ImportNote::InheritedEnv {
            app: name.to_string(),
            key,
        });
    }
    for key in &first.unrepresentable {
        notes.push(ImportNote::UnrepresentableEnv {
            app: name.to_string(),
            key: key.clone(),
        });
    }

    (app, notes)
}

#[cfg(test)]
mod tests {
    use shep_core::values::{MemSize, UpDuration};

    use super::*;
    use crate::commands::import::dump;

    fn imported() -> Imported {
        convert(dump::parse(include_str!("testdata/dump.pm2.json")).unwrap()).unwrap()
    }

    #[test]
    fn four_instance_rows_collapse_into_three_apps() {
        let imported = imported();
        let names: Vec<&str> = imported.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["api", "worker", "migrate"]);
        assert_eq!(imported.apps[0].instances, 2, "api ran two instances");
        assert_eq!(imported.apps[1].instances, 1);
    }

    /// The fixture's two `api` rows are adjacent, so this interleaves them.
    #[test]
    fn same_named_rows_collapse_even_when_not_adjacent() {
        let mut rows = dump::parse(include_str!("testdata/dump.pm2.json")).unwrap();
        let second_api = rows.remove(1);
        rows.push(second_api);
        let imported = convert(rows).unwrap();
        let names: Vec<&str> = imported.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["api", "worker", "migrate"]);
        assert_eq!(
            imported.apps[0].instances, 2,
            "the two `api` rows must still collapse once they are no longer adjacent"
        );
    }

    #[test]
    fn every_mapped_field_lands_where_the_table_says() {
        let imported = imported();
        let api = &imported.apps[0];
        assert_eq!(api.script, "/srv/api/dist/server.js");
        assert_eq!(api.args, ["--port", "8080"]);
        assert_eq!(api.cwd.as_deref(), Some("/srv/api"));
        assert_eq!(api.interpreter.as_deref(), Some("node"));
        assert_eq!(api.max_memory, Some(MemSize::from_bytes(536_870_912)));
        assert!(api.autorestart);

        let worker = &imported.apps[1];
        assert_eq!(worker.interpreter.as_deref(), Some("bun"));
        assert!(!worker.autorestart);
        assert_eq!(worker.restart_delay, Some(UpDuration::from_millis(5000)));
        assert!(worker.merge_logs);

        // `exec_interpreter: "none"` maps to the absence of `interpreter`.
        let migrate = &imported.apps[2];
        assert_eq!(migrate.interpreter, None);
        assert_eq!(migrate.script, "/srv/migrate/bin/migrate");
    }

    #[test]
    fn cluster_mode_says_so_without_setting_a_field_nothing_reads() {
        let imported = imported();
        assert!(imported.notes.contains(&ImportNote::ClusterMode {
            app: "api".to_string(),
            instances: 2,
        }));
        for app in &imported.apps {
            assert!(
                !app.reuse_port,
                "`{}`: the importer must not emit a field normalize refuses",
                app.name
            );
        }
    }

    #[test]
    fn the_pm2_instance_variable_becomes_an_env_template_and_never_a_value() {
        let imported = imported();
        let api = &imported.apps[0];
        assert_eq!(
            api.env.get("NODE_APP_INSTANCE").map(String::as_str),
            Some("{{instance}}")
        );
        assert!(imported.notes.contains(&ImportNote::InstanceVar {
            app: "api".to_string(),
            var: "NODE_APP_INSTANCE".to_string(),
        }));
    }

    #[test]
    fn every_mapped_app_normalizes() {
        for app in imported().apps {
            let name = app.name.clone();
            shep_core::config::normalize(app).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }
}
