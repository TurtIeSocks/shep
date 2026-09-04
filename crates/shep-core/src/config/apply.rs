//! How each `AppConfig` field reaches a running sheep.
//!
//! Four answers, and the difference between them is where the daemon READS
//! the field, not what the field means. A value read fresh at each decision
//! can be swapped under a running process with no disruption; one baked into
//! the child at exec cannot change until that process is replaced.
//!
//! The four entries most likely to look wrong carry their reasoning at their
//! own arm below. All four were measured against the read sites rather than
//! inferred from the field's name.

use serde::{Deserialize, Serialize};

/// Where a field's new value takes effect.
///
/// `#[non_exhaustive]`: a field could someday be applied by nudging the
/// running child through the existing `shep reopen` (SIGUSR2) signal path
/// rather than by a fresh read, a next spawn, or a full respawn -- a
/// reopen-triggered fifth group distinct from all four below. shep-core is
/// published, so an out-of-tree match on this enum needs a wildcard arm to
/// survive that addition without a major version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyGroup {
    /// Read fresh at each decision, so a write to the stored spec is enough.
    Live,
    /// Read when a process spawns, so a write reaches the next one.
    NextSpawn,
    /// Held by the running child, so that instance must be replaced.
    NeedsRespawn,
    /// Identity or flock shape, not a runtime knob.
    Structural,
}

/// Every `AppConfig` field name this table has been taught about, paired with
/// its group.
///
/// `apply_group` and `is_classified` both read this single list rather than
/// keeping two hand-maintained lists in step: a field named here only once
/// cannot drift between "classified" and "has a group".
const FIELDS: &[(&str, ApplyGroup)] = &[
    // Read by `brain::decide` when a sheep exits.
    ("autorestart", ApplyGroup::Live),
    ("max_restarts", ApplyGroup::Live),
    ("min_uptime", ApplyGroup::Live),
    ("restart_delay", ApplyGroup::Live),
    ("exp_backoff_restart_delay", ApplyGroup::Live),
    ("stop_exit_codes", ApplyGroup::Live),
    // Read by `claim_manual` when a kill ladder runs.
    ("kill_timeout", ApplyGroup::Live),
    ("graceful_timeout", ApplyGroup::Live),
    // Read fresh when extras arms a worker. These need a re-arm to take
    // effect; see `ExtrasRegistry::rearm_name`.
    ("max_memory", ApplyGroup::Live),
    ("watch", ApplyGroup::Live),
    ("ignore_watch", ApplyGroup::Live),
    ("watch_delay", ApplyGroup::Live),
    ("watch_options", ApplyGroup::Live),
    ("cron_restart", ApplyGroup::Live),
    ("cron_timezone", ApplyGroup::Live),
    ("liveness_probe", ApplyGroup::Live),
    // Read fresh per command.
    ("fold", ApplyGroup::Live),
    ("reuse_port", ApplyGroup::Live),
    // Read fresh from the stored spec each time an action is dispatched, at
    // `supervisor.rs`'s `begin_action` (`config.action_timeout.as_duration()`),
    // not baked into the long-lived per-sheep task.
    ("action_timeout", ApplyGroup::Live),
    // `kill_signal` is NOT Live, despite its two ladder-mates above. It is
    // read inside `kill_process` from the `app: &AppConfig` parameter of the
    // long-lived per-sheep task, whose `ResolvedApp` is moved in once at
    // `spawn_sheep_task` and never refreshed.
    ("kill_signal", ApplyGroup::NextSpawn),
    ("listen_timeout", ApplyGroup::NextSpawn),
    ("readiness_probe", ApplyGroup::NextSpawn),
    // Read once at muster or boot, by `restorable()`.
    ("autostart", ApplyGroup::NextSpawn),
    // Baked into the child at exec: argv, cwd, environment, credentials, the
    // fd table, the log paths it is already writing to.
    ("script", ApplyGroup::NeedsRespawn),
    ("args", ApplyGroup::NeedsRespawn),
    ("cwd", ApplyGroup::NeedsRespawn),
    ("interpreter", ApplyGroup::NeedsRespawn),
    ("env", ApplyGroup::NeedsRespawn),
    ("user", ApplyGroup::NeedsRespawn),
    ("group", ApplyGroup::NeedsRespawn),
    ("out_file", ApplyGroup::NeedsRespawn),
    ("err_file", ApplyGroup::NeedsRespawn),
    ("merge_logs", ApplyGroup::NeedsRespawn),
    ("channel", ApplyGroup::NeedsRespawn),
    ("stdin", ApplyGroup::NeedsRespawn),
    ("wait_ready", ApplyGroup::NeedsRespawn),
    // `shutdown_with_message` belongs here rather than with the kill ladder:
    // `assemble()` ORs it into whether fd 3 is opened, and that is the
    // child's own fd table.
    ("shutdown_with_message", ApplyGroup::NeedsRespawn),
    ("name", ApplyGroup::Structural),
    ("instances", ApplyGroup::Structural),
    // Read only by `normalize` to refuse it by name.
    ("increment_var", ApplyGroup::Structural),
];

/// The group `field` belongs to.
///
/// An unknown name answers [`ApplyGroup::NeedsRespawn`], the most
/// conservative of the four: a field this table has not been taught about
/// gets a restart rather than a silent claim that it applied.
/// `every_appconfig_field_has_a_group` keeps that arm unreachable for real
/// fields.
#[must_use]
pub fn apply_group(field: &str) -> ApplyGroup {
    FIELDS
        .iter()
        .find(|(name, _)| *name == field)
        .map_or(ApplyGroup::NeedsRespawn, |(_, group)| *group)
}

/// Whether `field` is named explicitly in the table above, as opposed to
/// reaching the conservative fallback. Test-facing.
#[must_use]
pub fn is_classified(field: &str) -> bool {
    FIELDS.iter().any(|(name, _)| *name == field)
}

/// How much of a Flockfile load overwrites what the operator has set since.
///
/// A mode touches what its name says, and the design spec
/// (`2026-09-02-config-overrides-design.md` §3) states each variant against
/// three columns rather than two: whether `env` is reset, whether a key the
/// template declares is reset, and whether a key it does not declare is.
///
/// **Not a two-by-two grid**, and an earlier version of this comment said it
/// was. There are two independent choices, but the settings one has three
/// settings rather than two -- untouched, declared only, or everything --
/// which makes six combinations. These four are the ones worth having; the
/// two dropped both reset undeclared keys while sparing declared ones, which
/// is an operation nobody wants.
///
/// `#[non_exhaustive]`: no fifth depth is anticipated, but this type travels
/// inside `Request::ApplyConfig` on the wire, so the attribute stays as
/// insurance against a peer on an older build that cannot decode a variant
/// it predates; without it, a non-exhaustive match at a call site in another
/// crate would silently compile against a peer that can never receive it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResetDepth {
    /// Append keys nobody established. Overwrite nothing. The default,
    /// because a Flockfile arrives from the app's own repository.
    ///
    /// `env`: kept. A key the template declares: kept, unless nobody has
    /// established it yet, which is the append. A key it does not declare:
    /// kept, since there is nothing to append it against.
    #[default]
    None,
    /// Put back what the template declares, and nothing else.
    ///
    /// `env`: kept. A key the template declares: reset. A key it does not
    /// declare: kept -- an app stocked to four instances against a file with
    /// no `instances` line keeps its count, because the file never entered
    /// that argument. This is the mode that fixes the footgun `Policy` below
    /// reintroduces.
    File,
    /// Put non-`env` settings back to the template, `env` kept. Every
    /// setting goes back, declared or not: a key the template is silent
    /// about goes to the value a fresh start off that template would give
    /// it. `env` is operator-supplied data while the rest is operator-tuned
    /// policy: resetting policy is recoverable, resetting data takes the
    /// app's database away.
    ///
    /// `env`: kept. A key the template declares: reset. A key it does not
    /// declare: reset too, to the template's own default.
    Policy,
    /// Reset `env` back to the template and leave everything else alone.
    ///
    /// This mode touches data, not policy, so it has no opinion about any
    /// setting: a restart budget the template happens to mention is not the
    /// operator's to lose to a flag that says `env`. On the settings axis it
    /// is therefore `None`, append included, because the flag widens a load
    /// rather than narrowing one.
    ///
    /// `env`: reset. A key the template declares: kept, save for the same
    /// append `None` does. A key it does not declare: kept.
    Env,
    /// Put everything back to the template, `env` included, and drop the
    /// override record.
    ///
    /// `env`: reset. A key the template declares: reset. A key it does not
    /// declare: reset too, to the template's own default.
    All,
}

#[cfg(test)]
mod tests {
    use super::{ApplyGroup, apply_group, is_classified};
    use crate::config::AppConfig;

    /// fails if any AppConfig field is missing from the table. A field added
    /// to the struct without a group would route as its default and either
    /// apply live when it cannot, or need a restart when it does not.
    #[test]
    fn every_appconfig_field_has_a_group() {
        let serde_json::Value::Object(fields) = serde_json::to_value(AppConfig::default()).unwrap()
        else {
            panic!("AppConfig must serialize as an object");
        };
        let missing: Vec<&String> = fields.keys().filter(|k| !is_classified(k)).collect();
        assert!(
            missing.is_empty(),
            "unclassified AppConfig fields: {missing:?}"
        );
    }

    /// fails if kill_signal is classified Live. It is read from the
    /// per-sheep task's frozen ResolvedApp, moved in once at
    /// spawn_sheep_task and never refreshed, so an edit reaches the next
    /// spawn and not the next kill. Its ladder-mates kill_timeout and
    /// graceful_timeout ARE read fresh, in claim_manual, which is why this
    /// one looks like it belongs with them.
    #[test]
    fn kill_signal_reaches_the_next_spawn_not_the_next_kill() {
        assert_eq!(apply_group("kill_signal"), ApplyGroup::NextSpawn);
        assert_eq!(apply_group("kill_timeout"), ApplyGroup::Live);
        assert_eq!(apply_group("graceful_timeout"), ApplyGroup::Live);
    }

    /// fails if shutdown_with_message is classified anything but
    /// NeedsRespawn. assemble() ORs it into whether fd 3 is opened for the
    /// child, which is the child's own fd table and cannot change under a
    /// running process.
    #[test]
    fn shutdown_with_message_is_baked_into_the_child() {
        assert_eq!(
            apply_group("shutdown_with_message"),
            ApplyGroup::NeedsRespawn
        );
    }

    /// fails if the split drifts from what the spec recorded.
    #[test]
    fn the_split_is_nineteen_four_fourteen_three() {
        let serde_json::Value::Object(fields) = serde_json::to_value(AppConfig::default()).unwrap()
        else {
            panic!("AppConfig must serialize as an object");
        };
        let count = |want: ApplyGroup| fields.keys().filter(|k| apply_group(k) == want).count();
        assert_eq!(count(ApplyGroup::Live), 19, "Live");
        assert_eq!(count(ApplyGroup::NextSpawn), 4, "NextSpawn");
        assert_eq!(count(ApplyGroup::NeedsRespawn), 14, "NeedsRespawn");
        assert_eq!(count(ApplyGroup::Structural), 3, "Structural");
    }
}
