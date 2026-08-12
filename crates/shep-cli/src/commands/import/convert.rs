//! Collapsing pm2 dump rows into shep apps.
//!
//! A dump is per-instance: a clustered app comes back as several
//! [`DumpRow`]s sharing a `name`. [`convert`] groups those rows back into
//! one [`AppConfig`] per app and maps every field this importer knows how
//! to map, one row per the design spec's own table. The row count becomes
//! `instances` — the dump records what was *running*, not what the app was
//! configured for, which matches the muster roll's own "was up when we
//! saved" rule — and the first row in each group wins every scalar: two
//! instances of one app are the same app, and if they disagree about
//! something like `pm_exec_path`, one of them is a leftover from a deploy
//! and the first is as good a choice as any.
//!
//! Cluster mode is the cutover blocker this importer names rather than
//! silently drops on the floor: pm2's cluster master holds one listen
//! socket and hands connections off to its workers, but shep binds
//! nothing, so N independent processes on one port is `EADDRINUSE` unless
//! the app itself sets `SO_REUSEPORT`. `exec_mode == "cluster_mode"`
//! therefore maps to `reuse_port = true` on top of the row-count
//! `instances`, plus an [`ImportNote::ClusterMode`] the operator hears
//! about at import time instead of discovering it at the first restart.
//!
//! `env` is deliberately left empty on every app this module returns, and
//! `notes` carries no env entries — filtering inherited environment noise
//! out of declared config is a later importer stage's job, not this one's.

use std::collections::HashMap;

use shep_core::config::{AppConfig, normalize};
use shep_core::values::{MemSize, UpDuration};

use super::dump::DumpRow;

/// The pm2 env key that carries an instance's slot number, mapped to
/// `increment_var` rather than copied — see [`ImportNote::InstanceVar`].
const NODE_APP_INSTANCE: &str = "NODE_APP_INSTANCE";

/// What one dump became: the apps to write, and everything the operator has
/// to be told about them.
///
/// Its fields are read only by this module's own tests today: no verb reads
/// an `Imported` back out yet. `#[allow(dead_code)]` says so explicitly
/// rather than inventing a call site nothing needs yet.
#[derive(Debug)]
#[allow(dead_code)]
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
    /// An env key the app inherited from the shell that started it, which
    /// was neither declared nor a known session-shell or pm2 key.
    ///
    /// Not constructed here: collapsing instances and mapping fields is
    /// this module's whole job, and filtering inherited environment noise
    /// out of declared config belongs to a later stage of this importer.
    /// `#[allow(dead_code)]` says so explicitly rather than inventing a
    /// call site nothing here needs yet.
    #[allow(dead_code)]
    InheritedEnv {
        /// The app's name.
        app: String,
        /// The inherited key.
        key: String,
    },
    /// An env value a Flockfile cannot hold.
    ///
    /// Not constructed here, for the same reason as
    /// [`ImportNote::InheritedEnv`].
    #[allow(dead_code)]
    UnrepresentableEnv {
        /// The app's name.
        app: String,
        /// The key whose value could not be represented.
        key: String,
    },
    /// The app read its instance number from a pm2 variable, recorded as
    /// `increment_var` rather than copied as a value.
    InstanceVar {
        /// The app's name.
        app: String,
        /// The pm2 variable name (`NODE_APP_INSTANCE`).
        var: String,
    },
}

/// Why [`convert`] refused to import a dump.
#[derive(Debug)]
pub(crate) enum ConvertError {
    /// A mapped app does not normalize. Carries the app name and
    /// [`shep_core::config::NormalizeError`]'s own message.
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
/// `env` is left empty on every returned app and `notes` carries no env
/// entries: filtering inherited environment noise out of declared config
/// belongs to a later importer stage, not this one.
///
/// Not called outside this module's own tests yet: no verb wires this
/// converter into the CLI. `#[allow(dead_code)]` says so explicitly rather
/// than inventing a call site nothing needs yet.
///
/// # Errors
/// - [`ConvertError::Rejected`] — a mapped app does not normalize (carries the app name and the reason).
#[allow(dead_code)]
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
/// mapping produced. `rows` is never empty: every group [`convert`] builds
/// was created by pushing at least one row into it.
fn convert_group(name: &str, rows: &[DumpRow]) -> (AppConfig, Vec<ImportNote>) {
    let mut notes = Vec::new();
    // A dump naming anywhere near u32::MAX instances of one app does not
    // happen in practice; `AppConfig::instances` is itself a u32.
    let instances = rows.len() as u32;
    let first = &rows[0];

    let mut app = AppConfig::minimal(name, &first.pm_exec_path);
    app.args = first.args.clone();
    app.cwd = first.pm_cwd.clone();
    // `exec_interpreter: "none"` means run the script directly, which in a
    // Flockfile is the absence of `interpreter` — not the literal string
    // "none", which shep would try to exec.
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
        app.reuse_port = true;
        notes.push(ImportNote::ClusterMode {
            app: name.to_string(),
            instances,
        });
    }

    if first.env.contains_key(NODE_APP_INSTANCE) {
        app.increment_var = Some(NODE_APP_INSTANCE.to_string());
        notes.push(ImportNote::InstanceVar {
            app: name.to_string(),
            var: NODE_APP_INSTANCE.to_string(),
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

    /// fails if instance rows stop collapsing — three apps out of four rows
    /// is the whole of what "the dump is per-instance" means, and an
    /// importer that skipped it would register `api` twice under one name,
    /// which `shep start` then refuses.
    #[test]
    fn four_instance_rows_collapse_into_three_apps() {
        let imported = imported();
        let names: Vec<&str> = imported.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["api", "worker", "migrate"]);
        assert_eq!(imported.apps[0].instances, 2, "api ran two instances");
        assert_eq!(imported.apps[1].instances, 1);
    }

    /// fails if grouping only merges rows that happen to sit next to each
    /// other — chunking consecutive elements looks like a reasonable
    /// shortcut for "rows sharing a name" and would pass every other test
    /// here, since the fixture's two `api` rows are already adjacent. This
    /// interleaves them so a position-based grouping pass sees three
    /// singleton-ish runs instead of the two groups a name-keyed pass does.
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

    /// fails if any single mapping is dropped. One case per field rather
    /// than one per test: the mapping is a table, and a table is worth
    /// asserting as a table — a reader comparing this against the design
    /// spec's own table sees the same rows in the same order.
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

        // `exec_interpreter: "none"` means run the script directly, which in
        // a Flockfile is the ABSENCE of `interpreter` — not the literal
        // string "none", which shep would try to exec.
        let migrate = &imported.apps[2];
        assert_eq!(migrate.interpreter, None);
        assert_eq!(migrate.script, "/srv/migrate/bin/migrate");
    }

    /// fails if a cluster-mode app comes across without `reuse_port`, or
    /// without the note. Both halves are the same cutover blocker: shep
    /// binds nothing, so four instances on one port is EADDRINUSE, and the
    /// operator has to hear it at import time rather than at first start.
    #[test]
    fn cluster_mode_sets_reuse_port_and_says_so() {
        let imported = imported();
        assert!(imported.apps[0].reuse_port, "api ran in cluster mode");
        assert!(
            !imported.apps[1].reuse_port,
            "fork mode must not assert an option the app never set"
        );
        assert!(imported.notes.contains(&ImportNote::ClusterMode {
            app: "api".to_string(),
            instances: 2,
        }));
    }

    /// fails if `NODE_APP_INSTANCE` is copied into the app env as a value.
    /// Copying it pins instance 0's number into every instance, which is
    /// worse than dropping it: every worker would believe it is worker 0.
    #[test]
    fn the_pm2_instance_variable_becomes_increment_var_and_never_a_value() {
        let imported = imported();
        let api = &imported.apps[0];
        assert_eq!(api.increment_var.as_deref(), Some("NODE_APP_INSTANCE"));
        assert!(!api.env.contains_key("NODE_APP_INSTANCE"));
        assert!(imported.notes.contains(&ImportNote::InstanceVar {
            app: "api".to_string(),
            var: "NODE_APP_INSTANCE".to_string(),
        }));
    }

    /// fails if a mapped app is returned without going through `normalize`.
    /// Every app this fixture produces must be one the daemon would accept;
    /// a Flockfile that `shep start` refuses is not an import, it is a
    /// deferred failure.
    #[test]
    fn every_mapped_app_normalizes() {
        for app in imported().apps {
            let name = app.name.clone();
            shep_core::config::normalize(app).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }
}
