//! Splitting a dump row's environment into what a Flockfile should carry and
//! what the operator has to decide about.
//!
//! pm2 flattens the shell that ran `pm2 start` into every row's `env` map
//! alongside whatever the ecosystem file actually declared. The `env_<name>`
//! maps are the one place that flattening never reaches: by construction
//! they hold exactly what an ecosystem file wrote, never a login shell's
//! contribution. [`split`] treats their union as the declared env, writes
//! it, and checks everything else in `env` against two short, closed lists
//! before deciding whether it is quiet noise or something the operator has
//! to see. **Do not grow either list by guessing** — a session-junk list
//! that is too long is the one direction that *does* lose data silently; an
//! incomplete list merely costs one extra line of output.
//!
//! `PATH` sits in [`SESSION_SHELL`] deliberately. The design spec captures
//! `PATH` into the *unit*, not an app's env — that is the mechanism making
//! an interpreter under `~/.bun` findable after a reboot. An app's
//! Flockfile carrying a login shell's `PATH` would defeat it.

use std::collections::BTreeMap;

use super::dump::DumpRow;

/// Variables a login shell puts in every process it starts. Dropped without
/// comment: they describe the session that ran `pm2 start`, not the app, and
/// an init-started daemon has none of them.
const SESSION_SHELL: &[&str] = &[
    "COLORTERM",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "EDITOR",
    "HISTFILE",
    "HISTSIZE",
    "HOME",
    "HOSTNAME",
    "LANG",
    "LOGNAME",
    "LS_COLORS",
    "MAIL",
    "MOTD_SHOWN",
    "OLDPWD",
    "PAGER",
    "PATH",
    "PWD",
    "SHELL",
    "SHLVL",
    "TERM",
    "TMPDIR",
    "USER",
    "VISUAL",
    "_",
];

/// Prefixes with the same standing as [`SESSION_SHELL`].
const SESSION_SHELL_PREFIXES: &[&str] = &["LC_", "SSH_", "SUDO_", "XDG_"];

/// Variables pm2 puts into a process it supervises. Short on purpose: a key
/// this list misses is NAMED rather than written, so the cost of an
/// incomplete list is a line of output, not a wrong Flockfile.
const PM2_INJECTED: &[&str] = &["NODE_APP_INSTANCE", "pm_id", "unique_id"];

/// Prefix with the same standing as [`PM2_INJECTED`].
const PM2_INJECTED_PREFIXES: &[&str] = &["PM2_"];

/// The pm2 variable an app reads its instance number from. Recorded as
/// [`AppEnv::instance_var`] rather than copied as a value: the dump holds
/// instance 0's number, and writing it would tell every instance it is
/// instance 0.
const PM2_INSTANCE_VAR: &str = "NODE_APP_INSTANCE";

/// What an app's env became, and what the operator has to decide.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AppEnv {
    /// The env to write into the Flockfile.
    pub env: BTreeMap<String, String>,
    /// Keys present on the running process that were neither declared nor
    /// recognised — named in the output, never written.
    pub inherited: Vec<String>,
    /// The pm2 variable the app read its instance number from, if any.
    pub instance_var: Option<String>,
}

/// Splits one row's environment into what a Flockfile should carry and what
/// the operator has to decide about.
///
/// A key in the declared union (the row's `env_<name>` maps, combined) is
/// written, taking its value from `env` when the running process has one —
/// the dump records what is actually running — or from the declared map
/// otherwise. A key in `env` that is not declared is checked against
/// [`SESSION_SHELL`] and [`PM2_INJECTED`] (dropped silently, except
/// [`PM2_INSTANCE_VAR`], which becomes [`AppEnv::instance_var`] instead of a
/// value); everything else is named in [`AppEnv::inherited`] and not
/// written, so the operator decides whether it belongs in the Flockfile,
/// the unit, or nowhere.
pub(crate) fn split(row: &DumpRow) -> AppEnv {
    let mut declared_union: BTreeMap<&str, &str> = BTreeMap::new();
    for declared in row.declared.values() {
        for (key, value) in declared {
            declared_union.insert(key.as_str(), value.as_str());
        }
    }

    let env = declared_union
        .iter()
        .map(|(&key, &declared_value)| {
            let value = row.env.get(key).map_or(declared_value, String::as_str);
            (key.to_string(), value.to_string())
        })
        .collect();

    let mut inherited = Vec::new();
    let mut instance_var = None;
    for key in row.env.keys() {
        if declared_union.contains_key(key.as_str()) {
            continue;
        }
        if key.as_str() == PM2_INSTANCE_VAR {
            instance_var = Some(key.clone());
        } else if !is_session_shell(key) && !is_pm2_injected(key) {
            inherited.push(key.clone());
        }
    }

    AppEnv {
        env,
        inherited,
        instance_var,
    }
}

/// Whether `key` is a login shell's own variable rather than an app's.
fn is_session_shell(key: &str) -> bool {
    SESSION_SHELL.contains(&key)
        || SESSION_SHELL_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// Whether `key` is one pm2 injects into a process it supervises.
fn is_pm2_injected(key: &str) -> bool {
    PM2_INJECTED.contains(&key)
        || PM2_INJECTED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::import::dump;

    fn rows() -> Vec<DumpRow> {
        dump::parse(include_str!("testdata/dump.pm2.json")).unwrap()
    }

    /// fails if the declared env stops being the union of the `env_<name>`
    /// maps — an importer reading `env` instead would write all eleven of
    /// api's keys, twenty-four of which across the design's real dump were a
    /// dead login session.
    #[test]
    fn only_declared_keys_are_written() {
        let api = split(&rows()[0]);
        assert_eq!(
            api.env.keys().collect::<Vec<_>>(),
            ["NODE_ENV"],
            "one declared key, and the ten inherited ones are not it"
        );
        assert_eq!(api.env["NODE_ENV"], "production");
    }

    /// fails if a declared key absent from the running process's env is
    /// dropped. `env` holds what is running; the declared map is what the
    /// ecosystem file asked for, and a key the process never received is
    /// still one the operator declared.
    #[test]
    fn a_declared_key_missing_from_the_running_env_still_comes_across() {
        let worker = split(&rows()[2]);
        assert_eq!(worker.env["QUEUE_CONCURRENCY"], "4");
        assert_eq!(worker.env["QUEUE_URL"], "redis://127.0.0.1:6379/2");
    }

    /// fails if an ambiguous key is dropped silently. This is the whole
    /// decision the design refuses to make on the operator's behalf: a
    /// heuristic that guessed which inherited vars matter will eventually be
    /// wrong, and being wrong silently is what makes it expensive.
    #[test]
    fn an_unrecognised_inherited_key_is_named_and_not_written() {
        let api = split(&rows()[0]);
        assert_eq!(api.inherited, ["BUN_INSTALL"]);
        assert!(!api.env.contains_key("BUN_INSTALL"));

        let worker = split(&rows()[2]);
        assert_eq!(worker.inherited, ["JAVA_HOME"]);

        // An app started by hand has no declared env at all, so every key it
        // is running with is the operator's to decide on.
        let migrate = split(&rows()[3]);
        assert_eq!(migrate.inherited, ["DATABASE_URL"]);
        assert!(migrate.env.is_empty());
    }

    /// fails if a session or pm2 key starts being named. Naming them is not
    /// harmless: twenty-four lines of `LS_COLORS` and `XDG_SESSION_ID` per
    /// app is how an operator learns to skim past the two lines that matter.
    #[test]
    fn session_and_pm2_keys_are_dropped_without_comment() {
        let api = split(&rows()[0]);
        for quiet in [
            "SSH_TTY",
            "XDG_SESSION_ID",
            "MOTD_SHOWN",
            "LS_COLORS",
            "LANG",
            "SHLVL",
            "PATH",
            "PM2_HOME",
            "NODE_APP_INSTANCE",
        ] {
            assert!(
                !api.inherited.iter().any(|k| k == quiet),
                "{quiet} was named"
            );
            assert!(!api.env.contains_key(quiet), "{quiet} was written");
        }
        assert_eq!(api.instance_var.as_deref(), Some("NODE_APP_INSTANCE"));
    }
}
