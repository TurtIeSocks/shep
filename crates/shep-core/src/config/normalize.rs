//! Validation and normalization: `AppConfig` -> `ResolvedApp`
//!
//! `ResolvedApp` is a proof token: constructing one is only possible through
//! [`normalize`], so daemon code can require it and skip re-validation.

use core::fmt;

use std::path::Path;

use std::collections::BTreeSet;

use globset::Glob;

use crate::config::{
    AppConfig, CronParseError, CronSchedule, KillSignal, ProbeConfig, ProbeTarget,
};
use crate::values::UpDuration;

/// Shortest `interval` a `liveness_probe` may name.
///
/// The daemon's liveness loop floors whatever it is handed at this same value
/// (its own `MIN_PROBE_INTERVAL`), so a smaller number would be *honoured* as
/// this one with nothing to say so — in a detached daemon, not even a log
/// line. That is the reasoning `max_cron_sleep` was settled on (`MIN_CRON_SLEEP`
/// rejects rather than clamps), and it applies here for the same reason: the
/// user's file is the only place the discrepancy could ever be noticed.
///
/// One second is a floor no legitimate configuration wants to be under. A
/// liveness check asked for more often than that is polling, and for
/// [`ProbeKind::Exec`](crate::config::ProbeKind::Exec) it is that many process
/// spawns per second, per sheep, for as long as the sheep runs.
const MIN_LIVENESS_INTERVAL: UpDuration = UpDuration::from_millis(1_000);

/// Shortest `interval` a `readiness_probe` may name.
///
/// A whole second lower than [`MIN_LIVENESS_INTERVAL`], and deliberately so:
/// the readiness wait honours its `interval` exactly as written and is bounded
/// by the app's `listen_timeout`, so there is no clamp for a rejection to keep
/// honest here — only the zero, which would spin that wait for the whole
/// `listen_timeout`. A fast app that polls every 20ms to leave `starting`
/// sooner is asking for something the daemon really does, so this floor must
/// not take it away.
const MIN_READINESS_INTERVAL: UpDuration = UpDuration::from_millis(1);

/// Longest `action_timeout` an app may name.
///
/// Not a floor this time but a ceiling, and for a different reason than
/// [`MIN_LIVENESS_INTERVAL`]'s: there, a smaller number was silently
/// honoured as the floor; here, a larger one could never be honoured by
/// ANY caller at all. The daemon clamps every RPC deadline a client can
/// possibly ask for — its own `MAX_DEADLINE_MS`, 60s, in `shep-daemon`'s
/// `rpc` module — so an `action_timeout` at or above that line describes a
/// wait the daemon could never finish inside any request budget, no matter
/// how generous a caller's own `Client::request_with_deadline` call is.
/// That is not "the caller forgot to widen its deadline" (this crate has no
/// way to see a caller's choice, and does not try to); it is "no choice
/// exists", which is what makes it a config error rather than a caller's to
/// fix. Set 2s under the hard clamp — the same margin `action_timeout`'s own
/// default keeps under the *default* RPC budget — so the daemon still has
/// room to build the `TimedOut` row and get it back down the wire after the
/// wait itself gives up.
const MAX_ACTION_TIMEOUT: UpDuration = UpDuration::from_millis(58_000);

/// A validated app config — only obtainable via [`normalize`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedApp {
    config: AppConfig,
}

impl ResolvedApp {
    /// Borrow the validated configuration
    #[must_use]
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Unwrap the validated configuration (consumes the proof token)
    #[must_use]
    pub fn into_config(self) -> AppConfig {
        self.config
    }
}

/// Expands a leading `~/` against `home`, and refuses `~user/`.
///
/// `~` is a shell feature. A shell expands it before a program ever sees the
/// argument, so a value read out of a Flockfile has nothing between it and
/// the parser and arrives literally: shep would look for a directory named
/// `~`. A process manager standing in for the shell that would otherwise
/// have started the process inherits this narrow piece of its job.
///
/// Deliberately narrow. `~/` only:
///
/// - `~user/...` is refused. Resolving it means a passwd lookup, and under a
///   systemd unit the answer depends on who the daemon runs as rather than
///   on who wrote the file.
/// - `$VAR` is NOT expanded, here or anywhere. Once a config file expands
///   variables it has to answer WHICH environment it means -- the operator's,
///   the daemon's, or the app's own `env` table -- and there is no good
///   answer.
///
/// A path that does not start with `~` is returned untouched, so this is a
/// no-op for every absolute and relative path anyone already has.
///
/// Thin wrapper over [`expand_home_tilde`], the one piece of tilde-expansion
/// logic in the workspace: this attaches the sheep name and field
/// [`NormalizeError`] carries, so a Flockfile refusal names the line to
/// edit. A caller with no such context — `shep-cli`'s own `shep adopt`,
/// resolving a bare CLI argument — calls [`expand_home_tilde`] directly.
///
/// # Errors
/// - [`NormalizeError::TildeUser`] if the path names another user's home.
/// - [`NormalizeError::NoHomeForTilde`] if `~/` is used and `home` is `None`.
fn expand_tilde(
    value: &str,
    home: Option<&Path>,
    name: &str,
    field: &'static str,
) -> Result<String, NormalizeError> {
    expand_home_tilde(value, home).map_err(|err| match err {
        TildeError::OtherUser => NormalizeError::TildeUser {
            name: name.to_string(),
            field,
            value: value.to_string(),
        },
        TildeError::NoHome => NormalizeError::NoHomeForTilde {
            name: name.to_string(),
            field,
        },
    })
}

/// Why [`expand_home_tilde`] refused a value — the two failure modes with
/// no per-field context attached, for a caller that has none to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TildeError {
    /// The value names another user's home (`~user/...`). Refused rather
    /// than resolved: answering it means a passwd lookup, and under a
    /// systemd unit the answer depends on who the process runs as rather
    /// than on who wrote the value.
    OtherUser,
    /// The value begins `~/` and no home directory could be determined.
    NoHome,
}

impl fmt::Display for TildeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OtherUser => write!(
                f,
                "shep expands only `~/` (your own home); another user's home needs a \
                 passwd lookup whose answer depends on who the process runs as"
            ),
            Self::NoHome => write!(f, "begins with `~/` but no home directory could be found"),
        }
    }
}

impl core::error::Error for TildeError {}

/// Expands a leading `~/` in `value` against `home` — the one piece of
/// tilde-expansion logic in the workspace, shared by `expand_tilde` above
/// (Flockfile path fields, private to this module) and `shep-cli`'s own
/// `shep adopt` (its `<path>` argument).
///
/// Deliberately narrow, matching `expand_tilde`'s own doc: `~/` only.
/// `~user/...` is refused (a passwd lookup, whose answer depends on who
/// the process runs as) and `$VAR` is never expanded, here or anywhere.
///
/// A value that does not start with `~` is returned unchanged, so this is
/// a no-op for every absolute and relative path anyone already has.
///
/// # Errors
/// - [`TildeError::OtherUser`] if the value names another user's home.
/// - [`TildeError::NoHome`] if `~/` is used and `home` is `None`.
pub fn expand_home_tilde(value: &str, home: Option<&Path>) -> Result<String, TildeError> {
    let Some(rest) = value.strip_prefix('~') else {
        return Ok(value.to_string());
    };
    // `~` alone, or `~/...`. Anything else after the tilde names a user.
    if !(rest.is_empty() || rest.starts_with('/')) {
        return Err(TildeError::OtherUser);
    }
    let Some(home) = home else {
        return Err(TildeError::NoHome);
    };
    // `join` would discard `home` for a rest that still looks absolute, so
    // the separator is trimmed and the two halves are concatenated instead.
    let joined = home.join(rest.trim_start_matches('/'));
    Ok(joined.to_string_lossy().into_owned())
}

/// Every field of an [`AppConfig`] that carries a filesystem path.
///
/// Named once, and walked by [`expand_paths`] and by its own test, so a
/// fifth path field added later fails that test until it is handled.
/// Expanding `~/` in some path fields and not others would be worse than
/// expanding in none: it teaches that tildes work and then fails somewhere
/// the operator has no reason to suspect.
#[cfg(test)]
const PATH_FIELDS: &[&str] = &["script", "cwd", "out_file", "err_file"];

/// Expands `~/` in every path field of `app`, in place.
///
/// # Errors
/// Whatever [`expand_tilde`] refuses, named with the field that carried it.
fn expand_paths(app: &mut AppConfig, home: Option<&Path>) -> Result<(), NormalizeError> {
    let name = app.name.clone();
    app.script = expand_tilde(&app.script, home, &name, "script")?;
    for (field, slot) in [
        ("cwd", &mut app.cwd),
        ("out_file", &mut app.out_file),
        ("err_file", &mut app.err_file),
    ] {
        if let Some(value) = slot {
            *slot = Some(expand_tilde(value, home, &name, field)?);
        }
    }
    Ok(())
}

/// Validates one app config
///
/// # Errors
///
/// - [`NormalizeError::MissingName`]: `name` is empty.
/// - [`NormalizeError::InvalidName`]: `name` contains a path separator or a
///   colon, or is `.`/`..`.
/// - [`NormalizeError::ReservedEnvVar`]: `env` sets `SHEP_INSTANCE` or
///   `SHEP_NAME`, which shep injects itself (carries the app name and the
///   variable).
/// - [`NormalizeError::MissingScript`]: `script` is empty.
/// - [`NormalizeError::ZeroInstances`]: `instances == 0`.
/// - [`NormalizeError::InvalidCron`]: `cron_restart` is not valid in
///   croner's dialect (carries the pattern and the rejection reason).
/// - [`NormalizeError::InvalidTimezone`]: `cron_timezone` is not a name in
///   the IANA time-zone database.
/// - [`NormalizeError::InvalidProbe`]: `readiness_probe` or `liveness_probe`
///   has a target [`ProbeTarget::parse`] rejects (carries which probe and
///   the rendered reason).
/// - [`NormalizeError::ZeroFailureThreshold`]: a probe's `failure_threshold`
///   is explicitly `0`.
/// - [`NormalizeError::IntervalBelowMinimum`]: a probe's `interval` is under
///   the floor its own loop honours: a full second for `liveness_probe`, and
///   only "greater than zero" for `readiness_probe` (carries which probe, the
///   value and the floor).
/// - [`NormalizeError::ZeroMaxMemory`]: `max_memory` is `0`.
/// - [`NormalizeError::ActionTimeoutTooLong`]: `action_timeout` is at or
///   above the ceiling no RPC caller could ever be given room to wait past
///   (carries the app name, the value and the ceiling).
/// - [`NormalizeError::InvalidKillSignal`]: `kill_signal` names a signal the
///   daemon's stop ladder cannot send (carries the app name and the value).
/// - [`NormalizeError::WatchWithoutCwd`]: `watch` is `true` with no `cwd`
///   set.
/// - [`NormalizeError::ZeroWatchDelay`]: `watch_delay` is `0`.
/// - [`NormalizeError::InvalidWatchGlob`]: a `watch_options` or
///   `ignore_watch` pattern globset will not compile (carries the app name,
///   which of the two lists, the pattern and the reason).
pub fn normalize(app: AppConfig) -> Result<ResolvedApp, NormalizeError> {
    normalize_with_home(app, std::env::home_dir().as_deref())
}

/// [`normalize`], with the home directory supplied rather than read.
///
/// A parameter so the `~/` expansion above is testable without mutating the
/// process environment, which is racy under a parallel `cargo test`. This is
/// also the seam that matters for correctness rather than only for tests:
/// the daemon may run as a different user than the CLI, so `~` has to be
/// resolved where the config is normalised, not where it is executed.
///
/// # Errors
/// The same set [`normalize`] documents.
pub fn normalize_with_home(
    mut app: AppConfig,
    home: Option<&Path>,
) -> Result<ResolvedApp, NormalizeError> {
    if app.name.is_empty() {
        return Err(NormalizeError::MissingName);
    }
    if app.name.contains(['/', '\\', ':']) || app.name == "." || app.name == ".." {
        return Err(NormalizeError::InvalidName(app.name));
    }
    for var in ["SHEP_INSTANCE", "SHEP_NAME"] {
        if app.env.contains_key(var) {
            return Err(NormalizeError::ReservedEnvVar {
                name: app.name.clone(),
                var,
            });
        }
    }
    if app.script.is_empty() {
        return Err(NormalizeError::MissingScript);
    }
    // After the emptiness checks, so a missing script is reported as missing
    // rather than as a path problem, and before every check below that reads
    // a path.
    expand_paths(&mut app, home)?;
    if app.instances == 0 {
        return Err(NormalizeError::ZeroInstances);
    }
    if let Some(pattern) = &app.cron_restart {
        CronSchedule::parse(pattern, app.cron_timezone.as_deref()).map_err(|e| match e {
            CronParseError::Pattern { pattern, reason } => {
                NormalizeError::InvalidCron { pattern, reason }
            }
            CronParseError::Timezone { name } => NormalizeError::InvalidTimezone { name },
        })?;
    } else if let Some(tz_name) = &app.cron_timezone {
        // A Flockfile can carry `cron_timezone` with no `cron_restart` to
        // pair it with — still a typo the user wants to hear about (spec §5).
        crate::config::cron::parse_timezone_name(tz_name).ok_or_else(|| {
            NormalizeError::InvalidTimezone {
                name: tz_name.clone(),
            }
        })?;
    }
    validate_probe(
        app.readiness_probe.as_ref(),
        "readiness_probe",
        MIN_READINESS_INTERVAL,
    )?;
    validate_probe(
        app.liveness_probe.as_ref(),
        "liveness_probe",
        MIN_LIVENESS_INTERVAL,
    )?;
    if app.max_memory.is_some_and(|limit| limit.bytes() == 0) {
        // A ceiling every live process is over, armed against every poll: the
        // enforcer would report a breach on its first reading and on every
        // reading after it, and the restart that follows is automatic, which
        // RESETS the restart budget rather than spending it. `max_restarts`
        // cannot end that loop, so it has to be refused here.
        return Err(NormalizeError::ZeroMaxMemory { name: app.name });
    }
    if let Some(name) = &app.kill_signal
        && KillSignal::parse(name).is_none()
    {
        // Rejected rather than clamped, and this one is the sharpest case of
        // that trade in the file: a typo silently falling back to SIGTERM
        // would cost the operator every stop and every reload for the life
        // of the process, with the only evidence in a detached daemon's log
        // at the moment of a stop. `max_cron_sleep` and `MIN_LIVENESS_INTERVAL`
        // reject for the same reason at lower stakes: the user's file is the
        // only place a silently-substituted value could ever be noticed.
        return Err(NormalizeError::InvalidKillSignal {
            name: app.name,
            value: name.clone(),
        });
    }
    if app.action_timeout > MAX_ACTION_TIMEOUT {
        // Rejected rather than clamped, the same trade `MIN_LIVENESS_INTERVAL`
        // and `max_cron_sleep` already made: a daemon running detached has no
        // reader for a log line saying the value was silently lowered, so the
        // Flockfile would be the only place the discrepancy ever showed up —
        // and here there is no honest lowered value to fall back to anyway,
        // since every value above the ceiling is equally unreachable by any
        // caller.
        return Err(NormalizeError::ActionTimeoutTooLong {
            name: app.name,
            value: app.action_timeout,
            max: MAX_ACTION_TIMEOUT,
        });
    }
    if app.watch && app.cwd.is_none() {
        // `watch` asked for a feature the daemon has no directory to arm:
        // there is no cwd in the Flockfile, and defaulting to the daemon's
        // own cwd risks watching the whole filesystem under a systemd unit
        // with no `WorkingDirectory=`.
        return Err(NormalizeError::WatchWithoutCwd { name: app.name });
    }
    if app.watch_delay == Some(UpDuration::from_millis(0)) {
        // notify's debouncer derives its own poll tick as `watch_delay / 4`
        // and runs it on a dedicated OS thread, so a zero turns that thread
        // into `loop { sleep(0); lock(); }`, a CPU-spinning busy loop.
        // shep-daemon's watch arming floors this independently too (its
        // `MIN_WATCH_DELAY`), the same belt-and-suspenders shape
        // `validate_probe`'s interval check has opposite the liveness loop's
        // own floor.
        return Err(NormalizeError::ZeroWatchDelay { name: app.name });
    }
    // Both lists are checked whether or not `watch` is on. A pattern globset
    // will not compile is a typo, and the user wants it named now rather than
    // the day they flip `watch = true` and wonder why saving a file changes
    // nothing — the same reasoning that makes `watch` without `cwd` a config
    // error above.
    validate_watch_globs(&app.name, "watch_options", &app.watch_options)?;
    validate_watch_globs(&app.name, "ignore_watch", &app.ignore_watch)?;
    Ok(ResolvedApp { config: app })
}

/// Validates one of an app's two watch glob lists, rejecting any pattern
/// globset will not compile. `field` is the Flockfile field name
/// (`"watch_options"` or `"ignore_watch"`), carried into any error so the
/// user knows which list to edit. The compiled globs are discarded — this
/// function's job is rejection; the daemon builds its own watch filter when
/// it arms the watch.
fn validate_watch_globs(
    name: &str,
    field: &'static str,
    patterns: &[String],
) -> Result<(), NormalizeError> {
    for pattern in patterns {
        Glob::new(pattern).map_err(|err| NormalizeError::InvalidWatchGlob {
            name: name.to_string(),
            field,
            pattern: pattern.clone(),
            reason: err.to_string(),
        })?;
    }
    Ok(())
}

/// Validates one probe's target, `failure_threshold` and `interval`, if the
/// probe is configured. `probe` is the Flockfile field name
/// (`"readiness_probe"` or `"liveness_probe"`), carried into any error so the
/// user knows which field to edit; `min_interval` is the floor that probe's
/// own loop in the daemon honours, which is why the two call sites pass
/// different ones. Its own parsed [`ProbeTarget`] is discarded — this
/// function's job is rejection; the daemon re-parses when it arms the probe.
fn validate_probe(
    probe: Option<&ProbeConfig>,
    name: &'static str,
    min_interval: UpDuration,
) -> Result<(), NormalizeError> {
    let Some(probe) = probe else {
        return Ok(());
    };
    ProbeTarget::parse(probe).map_err(|reason| NormalizeError::InvalidProbe {
        probe: name,
        reason: reason.to_string(),
    })?;
    if probe.failure_threshold == 0 {
        // Unhealthy before the first probe ever runs — not a configuration
        // anybody wants, and it would make the liveness loop restart the
        // sheep immediately and forever.
        return Err(NormalizeError::ZeroFailureThreshold { probe: name });
    }
    if probe.interval < min_interval {
        // Not a configuration anybody wants either. Both probe loops sleep
        // `interval` between attempts, so a zero turns either into a hot
        // spin — for `ProbeKind::Exec`, hundreds of process spawns per
        // second, per sheep. A liveness interval that is merely *small*
        // is refused for a second reason: `spawn_liveness_task` rounds it UP
        // to its own `MIN_PROBE_INTERVAL`, which would leave the user's file
        // the only place the discrepancy exists and nothing anywhere to
        // report it. Rejecting rather than clamping is what `max_cron_sleep`
        // settled on for that same trade; the daemon-side floor stays too,
        // because this crate does not own the boot wiring that guarantees
        // every `ProbeConfig` reaching the loop came through here.
        return Err(NormalizeError::IntervalBelowMinimum {
            probe: name,
            value: probe.interval,
            min: min_interval,
        });
    }
    Ok(())
}

/// Validates a whole flock, rejecting duplicate sheep names
///
/// # Errors
///
/// Everything [`normalize`] returns, plus
/// [`NormalizeError::DuplicateName`] — two apps share a `name`.
pub fn normalize_all(apps: Vec<AppConfig>) -> Result<Vec<ResolvedApp>, NormalizeError> {
    let mut seen = BTreeSet::new();
    apps.into_iter()
        .map(|app| {
            if !seen.insert(app.name.clone()) {
                return Err(NormalizeError::DuplicateName(app.name));
            }
            normalize(app)
        })
        .collect()
}

/// Error type returned from [`normalize`] and [`normalize_all`]
///
/// Growth is expected: every config surface this crate learns to validate
/// brings its own rejection reasons with it (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    /// `name` is empty
    MissingName,
    /// `name` contains `/`, `\` or `:`, or is `.`/`..`. A path separator
    /// would escape the shep home, since the name becomes a filesystem path
    /// stem; a colon is the `name:slot` separator, and is also illegal in a
    /// Windows filename, which a sheep name becomes part of. Carries the
    /// name.
    InvalidName(String),
    /// An app's `env` sets a variable shep injects itself. Carries the sheep
    /// name and the variable, so the error names the entry to edit.
    ReservedEnvVar {
        /// The sheep name
        name: String,
        /// The variable the app tried to set
        var: &'static str,
    },
    /// `script` is empty
    MissingScript,
    /// `instances` is zero
    ZeroInstances,
    /// `cron_restart` is not valid in croner's dialect. Carries the pattern
    /// and the rejection reason — croner's own sentence where croner did the
    /// rejecting, ours where shep's pre-parse pass did.
    InvalidCron {
        /// The pattern as the user wrote it
        pattern: String,
        /// Why it was rejected
        reason: String,
    },
    /// `cron_timezone` is not a name in the IANA time-zone database
    InvalidTimezone {
        /// The value as the user wrote it
        name: String,
    },
    /// Two apps in one flock share this name
    DuplicateName(String),
    /// A `readiness_probe` or `liveness_probe` target is malformed. Carries
    /// which probe and the rendered reason.
    InvalidProbe {
        /// `"readiness_probe"` or `"liveness_probe"` — the Flockfile field
        /// name, so the error names the line the user has to edit.
        probe: &'static str,
        /// [`ProbeTarget::parse`]'s rendered rejection reason.
        reason: String,
    },
    /// A `readiness_probe` or `liveness_probe` has `failure_threshold == 0`.
    ZeroFailureThreshold {
        /// `"readiness_probe"` or `"liveness_probe"` — the Flockfile field
        /// name, so the error names the line the user has to edit.
        probe: &'static str,
    },
    /// A `readiness_probe` or `liveness_probe` has an `interval` under the
    /// floor its own loop in the daemon honours. At `0` that would spin the
    /// loop as fast as the runtime allows; a `liveness_probe` under a full
    /// second would instead be silently polled at that second.
    IntervalBelowMinimum {
        /// `"readiness_probe"` or `"liveness_probe"` — the Flockfile field
        /// name, so the error names the line the user has to edit.
        probe: &'static str,
        /// The value as the user wrote it.
        value: UpDuration,
        /// The floor it failed.
        min: UpDuration,
    },
    /// `max_memory` is `0` — a ceiling every live process is already over, so
    /// the enforcer would restart the sheep on every poll forever. Carries
    /// the app name.
    ZeroMaxMemory {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
    },
    /// `action_timeout` is at or above `normalize`'s own ceiling — a wait no
    /// RPC caller could ever be given enough deadline to outlast, since the
    /// daemon clamps every deadline a caller can ask for. Carries the app
    /// name, the value as written, and the ceiling it failed.
    ActionTimeoutTooLong {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// The value as the user wrote it.
        value: UpDuration,
        /// The ceiling it failed.
        max: UpDuration,
    },
    /// `kill_signal` names a signal the daemon's stop ladder cannot send.
    /// Carries the app name and the value as written.
    InvalidKillSignal {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// The value as the user wrote it.
        value: String,
    },
    /// `watch` is enabled but the app sets no `cwd`, so there is no
    /// directory to watch. Carries the app name.
    WatchWithoutCwd {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
    },
    /// A path begins `~user/`, naming another user's home.
    ///
    /// Refused rather than resolved: answering it means a passwd lookup, and
    /// under a systemd unit the answer is not obviously the one anyone meant.
    /// `~/` is supported; this is not.
    TildeUser {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// Which field carried it.
        field: &'static str,
        /// The path as written.
        value: String,
    },
    /// A path begins `~/` and no home directory could be determined.
    NoHomeForTilde {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// Which field carried it.
        field: &'static str,
    },
    /// `watch_delay` is `0`, which would spin the debouncer's own OS thread.
    /// Carries the app name.
    ZeroWatchDelay {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
    },
    /// A `watch_options` or `ignore_watch` pattern is one globset will not
    /// compile, so the watch it describes could never be armed.
    InvalidWatchGlob {
        /// The sheep name, so the error names which Flockfile entry to edit.
        name: String,
        /// `"watch_options"` or `"ignore_watch"` — the Flockfile field name,
        /// so the error names which of the two lists to edit.
        field: &'static str,
        /// The pattern as the user wrote it.
        pattern: String,
        /// globset's own rendered reason.
        reason: String,
    },
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => f.write_str("app config is missing a name"),
            Self::InvalidName(n) => {
                write!(
                    f,
                    "sheep name `{n}` may not contain a path separator or a colon, or be `.` or `..`"
                )
            }
            Self::ReservedEnvVar { name, var } => write!(
                f,
                "sheep `{name}` sets `{var}` in env, but shep injects it: use a different name, or `{{{{instance}}}}` in your own variable"
            ),
            Self::MissingScript => f.write_str("app config is missing a script"),
            Self::ZeroInstances => f.write_str("instances must be at least 1"),
            Self::InvalidCron { pattern, reason } => {
                write!(f, "invalid cron_restart pattern `{pattern}`: {reason}")
            }
            Self::InvalidTimezone { name } => {
                write!(f, "`{name}` is not a recognized IANA timezone")
            }
            Self::DuplicateName(n) => write!(f, "duplicate sheep name `{n}`"),
            Self::InvalidProbe { probe, reason } => write!(f, "{probe}: {reason}"),
            Self::ZeroFailureThreshold { probe } => {
                write!(f, "{probe}.failure_threshold must be at least 1")
            }
            Self::IntervalBelowMinimum { probe, value, min } => {
                write!(f, "{probe}.interval is `{value}`: must be at least {min}")
            }
            Self::ZeroMaxMemory { name } => {
                write!(
                    f,
                    "sheep `{name}` has max_memory = 0, a limit nothing can stay under"
                )
            }
            Self::ActionTimeoutTooLong { name, value, max } => {
                write!(
                    f,
                    "sheep `{name}` has action_timeout = {value}: must be at most {max}, \
                     the longest wait any caller's deadline could ever cover"
                )
            }
            Self::InvalidKillSignal { name, value } => {
                write!(
                    f,
                    "`{name}`: kill_signal `{value}` is not one shep can send (accepted: {})",
                    KillSignal::ACCEPTED.join(", ")
                )
            }
            Self::TildeUser { name, field, value } => write!(
                f,
                "`{name}`: {field} is `{value}`, and shep expands only `~/` (your own home). \
                 Another user's home needs a passwd lookup whose answer depends on who the \
                 daemon runs as, so write the path out in full instead."
            ),
            Self::NoHomeForTilde { name, field } => write!(
                f,
                "`{name}`: {field} begins with `~/` but no home directory could be found. \
                 Set $HOME, or write the path out in full."
            ),
            Self::WatchWithoutCwd { name } => {
                write!(f, "sheep `{name}` has watch = true but no cwd to watch")
            }
            Self::ZeroWatchDelay { name } => {
                write!(
                    f,
                    "sheep `{name}` has watch_delay = 0: must be greater than 0"
                )
            }
            Self::InvalidWatchGlob {
                name,
                field,
                pattern,
                reason,
            } => write!(
                f,
                "sheep `{name}` has an invalid {field} pattern `{pattern}`: {reason}"
            ),
        }
    }
}

impl core::error::Error for NormalizeError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// All four path fields expand `~/`, and expanding some but not others
    /// would be worse than expanding none: it teaches that tildes work and
    /// then fails where the operator has no reason to suspect it.
    #[test]
    fn every_path_field_expands_a_leading_tilde() {
        let home = Path::new("/home/ada");
        let mut app = AppConfig::minimal("web", "~/app/server.js");
        app.cwd = Some("~/app".to_string());
        app.out_file = Some("~/logs/out.log".to_string());
        app.err_file = Some("~/logs/err.log".to_string());

        let resolved = normalize_with_home(app, Some(home)).expect("all four expand");
        let c = resolved.config();
        // Expectations are built with `join` rather than written as literals:
        // the separator is `/` here and `\` on Windows, and hardcoding one
        // turned CI's three Windows legs red when this test first landed.
        let expect = |rest: &str| home.join(rest).to_string_lossy().into_owned();
        assert_eq!(c.script, expect("app/server.js"));
        assert_eq!(c.cwd.as_deref(), Some(expect("app").as_str()));
        assert_eq!(c.out_file.as_deref(), Some(expect("logs/out.log").as_str()));
        assert_eq!(c.err_file.as_deref(), Some(expect("logs/err.log").as_str()));
    }

    /// The anti-drift half. A fifth path field added to `AppConfig` fails
    /// here until `expand_paths` handles it, which is the only thing keeping
    /// the "all four or none" rule true over time.
    #[test]
    fn the_path_field_list_matches_what_expand_paths_walks() {
        let home = Path::new("/home/ada");
        let mut app = AppConfig::minimal("web", "~/s");
        app.cwd = Some("~/c".to_string());
        app.out_file = Some("~/o".to_string());
        app.err_file = Some("~/e".to_string());

        let resolved = normalize_with_home(app, Some(home)).expect("expands");
        let c = resolved.config();
        let expanded = [
            ("script", Some(c.script.as_str())),
            ("cwd", c.cwd.as_deref()),
            ("out_file", c.out_file.as_deref()),
            ("err_file", c.err_file.as_deref()),
        ];
        assert_eq!(
            expanded.len(),
            PATH_FIELDS.len(),
            "PATH_FIELDS and this test must name the same set"
        );
        for (field, value) in expanded {
            assert!(
                PATH_FIELDS.contains(&field),
                "`{field}` is not in PATH_FIELDS"
            );
            assert!(
                value.is_some_and(|v| v.starts_with("/home/ada")),
                "`{field}` was not expanded: {value:?}"
            );
        }
    }

    /// A path with no tilde is untouched, so this is a no-op for every
    /// absolute and relative path anyone already has.
    #[test]
    fn a_path_without_a_tilde_is_left_exactly_as_written() {
        let app = AppConfig::minimal("web", "./server.js");
        let resolved =
            normalize_with_home(app, Some(Path::new("/home/ada"))).expect("no tilde, no change");
        assert_eq!(resolved.config().script, "./server.js");
    }

    /// `~user/` needs a passwd lookup whose answer depends on who the daemon
    /// runs as, so it is refused rather than guessed at.
    #[test]
    fn another_users_home_is_refused_rather_than_resolved() {
        let app = AppConfig::minimal("web", "~deploy/app/server.js");
        let err = normalize_with_home(app, Some(Path::new("/home/ada")))
            .expect_err("~user/ must not resolve");
        assert!(
            matches!(err, NormalizeError::TildeUser { field, .. } if field == "script"),
            "the refusal names the field: {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("~/"),
            "and says what IS supported: {rendered}"
        );
        assert!(
            !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
            "no em or en dash in copy a user reads: {rendered}"
        );
    }

    /// `$VAR` is not expanded, here or anywhere. A config file that expands
    /// variables has to answer whose environment it means.
    #[test]
    fn a_dollar_variable_is_not_expanded() {
        let app = AppConfig::minimal("web", "$HOME/server.js");
        let resolved = normalize_with_home(app, Some(Path::new("/home/ada"))).expect("left alone");
        assert_eq!(resolved.config().script, "$HOME/server.js");
    }

    /// `~/` with no home to expand against is an error naming the field
    /// rather than a path containing a literal tilde.
    #[test]
    fn a_tilde_with_no_home_is_an_error_not_a_literal_path() {
        let app = AppConfig::minimal("web", "~/server.js");
        let err = normalize_with_home(app, None).expect_err("nothing to expand against");
        assert!(
            matches!(err, NormalizeError::NoHomeForTilde { .. }),
            "{err:?}"
        );
    }

    /// Pins [`expand_home_tilde`]'s own public contract directly, apart
    /// from [`expand_tilde`]'s wrapping of it into a [`NormalizeError`]:
    /// `shep-cli`'s own `shep adopt` calls this function directly (it has
    /// no app name or field to attach), so its behavior needs its own
    /// coverage independent of whatever `AppConfig`-shaped test drives it
    /// above.
    #[test]
    fn expand_home_tilde_covers_its_four_documented_cases() {
        let home = Path::new("/home/ada");
        assert_eq!(
            expand_home_tilde("~/bin/dog", Some(home)).unwrap(),
            home.join("bin/dog").to_string_lossy()
        );
        assert_eq!(
            expand_home_tilde("/opt/bin/dog", Some(home)).unwrap(),
            "/opt/bin/dog",
            "a value with no leading ~ is returned unchanged"
        );
        assert_eq!(
            expand_home_tilde("~/bin/dog", None).unwrap_err(),
            TildeError::NoHome
        );
        assert_eq!(
            expand_home_tilde("~deploy/bin/dog", Some(home)).unwrap_err(),
            TildeError::OtherUser
        );
    }

    /// `reuse_port` loads because reload's overlap mode is chosen from it —
    /// refusing it would deny an operator the only way to ask for an
    /// overlapping reload.
    #[test]
    fn reuse_port_loads_now_that_reload_reads_it() {
        let mut app = AppConfig::minimal("web", "./server");
        app.reuse_port = true;

        let resolved = normalize(app).expect("reuse_port is implemented");
        assert!(resolved.config().reuse_port);
    }

    /// The default is off, so every Flockfile that does not mention it keeps
    /// loading.
    #[test]
    fn an_app_that_never_mentions_reuse_port_still_normalizes() {
        let resolved = normalize(AppConfig::minimal("web", "./server"))
            .expect("the common case must be untouched");
        assert!(!resolved.config().reuse_port);
    }
    use crate::config::AppConfig;

    #[test]
    fn a_colon_in_a_name_is_refused_because_it_is_the_instance_separator() {
        let err = normalize(AppConfig::minimal("web:2", "./srv")).unwrap_err();
        assert_eq!(err, NormalizeError::InvalidName("web:2".to_string()));

        let rendered = err.to_string();
        assert!(rendered.contains(':'), "says which character: {rendered}");
        assert!(
            !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
            "no em or en dash in copy a user reads: {rendered}"
        );

        assert!(normalize(AppConfig::minimal("web-2", "./srv")).is_ok());
    }

    #[test]
    fn the_reserved_env_vars_are_refused_rather_than_overwritten() {
        for var in ["SHEP_INSTANCE", "SHEP_NAME"] {
            let mut app = AppConfig::minimal("web", "./srv");
            app.env.insert(var.to_string(), "mine".to_string());
            let err = normalize(app).unwrap_err();
            let rendered = err.to_string();
            assert!(rendered.contains(var), "names the variable: {rendered}");
            assert!(
                !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
                "no em or en dash in copy a user reads: {rendered}"
            );
        }
    }

    #[test]
    fn valid_minimal_config_normalizes() {
        let resolved = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        assert_eq!(resolved.config().name, "web");
    }

    #[test]
    fn names_that_reach_the_filesystem_are_rejected() {
        // A name becomes a log/pid file stem via Path::join; a slash-prefixed
        // or dotdot name would escape $SHEP_HOME. Reject at the config boundary.
        for bad in ["/etc/passwd", "..", ".", "a/b", "a\\b"] {
            assert_eq!(
                normalize(AppConfig::minimal(bad, "./srv")).unwrap_err(),
                NormalizeError::InvalidName(bad.to_string())
            );
        }
        assert!(normalize(AppConfig::minimal("web-1", "./srv")).is_ok());
    }

    #[test]
    fn missing_name_and_script_are_distinct_errors() {
        assert_eq!(
            normalize(AppConfig::minimal("", "./srv")).unwrap_err(),
            NormalizeError::MissingName
        );
        assert_eq!(
            normalize(AppConfig::minimal("web", "")).unwrap_err(),
            NormalizeError::MissingScript
        );
    }

    #[test]
    fn zero_instances_rejected() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 0;
        assert_eq!(normalize(app).unwrap_err(), NormalizeError::ZeroInstances);
    }

    #[test]
    fn bad_cron_pattern_rejected_with_pattern_and_reason_carried_through() {
        // fails if the reason is not carried through from croner.
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("not a cron".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidCron { pattern, reason } => {
                assert_eq!(pattern, "not a cron");
                assert!(!reason.is_empty());
            }
            other => panic!("expected InvalidCron, got {other:?}"),
        }
    }

    #[test]
    fn five_tokens_of_garbage_cron_pattern_rejected() {
        // fails if the validator only counts whitespace-separated tokens
        // instead of checking each field's range: five numeric-looking
        // tokens, all out of range.
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("99 99 99 99 99".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidCron { pattern, .. } => {
                assert_eq!(pattern, "99 99 99 99 99");
            }
            other => panic!("expected InvalidCron, got {other:?}"),
        }
    }

    #[test]
    fn bad_cron_timezone_rejected_alongside_a_valid_cron_restart() {
        // fails if the `cron_restart` branch maps CronParseError::Timezone to
        // anything but NormalizeError::InvalidTimezone. CronSchedule::parse
        // resolves the zone before it looks at the pattern, so a valid pattern
        // paired with a bad zone is the only input that reaches that arm — the
        // zone-with-no-pattern test below takes the separate `else if` branch.
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_restart = Some("0 3 * * *".to_string());
        app.cron_timezone = Some("Mars/Olympus".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidTimezone { name } => assert_eq!(name, "Mars/Olympus"),
            other => panic!("expected InvalidTimezone, got {other:?}"),
        }
    }

    #[test]
    fn cron_timezone_validated_even_without_cron_restart() {
        // fails if timezone validation is skipped when there's no pattern to
        // pair it with — a Flockfile with only a bad `cron_timezone` is a
        // typo the user wants to hear about (spec §5).
        let mut app = AppConfig::minimal("web", "./srv");
        app.cron_timezone = Some("Mars/Olympus".to_string());
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidTimezone { name } => assert_eq!(name, "Mars/Olympus"),
            other => panic!("expected InvalidTimezone, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_names_rejected_across_a_flock() {
        let apps = vec![
            AppConfig::minimal("web", "./a"),
            AppConfig::minimal("web", "./b"),
        ];
        assert_eq!(
            normalize_all(apps).unwrap_err(),
            NormalizeError::DuplicateName("web".to_string())
        );
    }

    fn probe_config(target: &str) -> crate::config::ProbeConfig {
        crate::config::ProbeConfig {
            kind: crate::config::ProbeKind::Http,
            target: target.to_string(),
            interval: crate::values::UpDuration::from_millis(10_000),
            timeout: crate::values::UpDuration::from_millis(5_000),
            failure_threshold: 3,
        }
    }

    #[test]
    fn malformed_readiness_probe_target_rejected_naming_the_field() {
        // fails if validate_probe is never called for readiness_probe, or if
        // it drops which of the two probe fields the rejection came from
        let mut app = AppConfig::minimal("web", "./srv");
        app.readiness_probe = Some(probe_config("not-a-url"));
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidProbe { probe, reason } => {
                assert_eq!(probe, "readiness_probe");
                assert!(!reason.is_empty());
            }
            other => panic!("expected InvalidProbe, got {other:?}"),
        }
    }

    #[test]
    fn malformed_liveness_probe_target_rejected_naming_the_field() {
        // fails if only readiness_probe is ever validated, leaving a bad
        // liveness_probe target to surface later at the daemon's first poll
        let mut app = AppConfig::minimal("web", "./srv");
        app.liveness_probe = Some(probe_config("not-a-url"));
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidProbe { probe, .. } => assert_eq!(probe, "liveness_probe"),
            other => panic!("expected InvalidProbe, got {other:?}"),
        }
    }

    #[test]
    fn valid_probe_targets_accepted() {
        // fails if validate_probe rejects a well-formed target outright
        let mut app = AppConfig::minimal("web", "./srv");
        app.readiness_probe = Some(probe_config("http://127.0.0.1:8080/healthz"));
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn zero_failure_threshold_rejected() {
        // fails if failure_threshold is never inspected — a threshold of 0
        // means "unhealthy before the first probe ever runs"
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.failure_threshold = 0;
        app.readiness_probe = Some(probe);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ZeroFailureThreshold {
                probe: "readiness_probe"
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("at least 1"), "{err}");
    }

    #[test]
    fn zero_interval_rejected() {
        // fails if interval is never inspected — a zero interval would spin
        // the readiness wait as fast as the runtime allows for the whole
        // `listen_timeout` (`await_ready` deliberately does not floor it)
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = UpDuration::from_millis(0);
        app.readiness_probe = Some(probe);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::IntervalBelowMinimum {
                probe: "readiness_probe",
                value: UpDuration::from_millis(0),
                min: MIN_READINESS_INTERVAL,
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("must be at least"), "{err}");
    }

    #[test]
    fn a_liveness_interval_under_the_floor_is_rejected_rather_than_clamped() {
        // fails if the liveness check is `interval == 0` rather than a
        // floor. A 500ms interval survives an equality check and is then
        // rounded UP to a full second by `spawn_liveness_task`'s own
        // `MIN_PROBE_INTERVAL` — an app polled at half the rate its
        // Flockfile asks for, with nothing anywhere to say so: that clamp
        // writes no record at all, so not even the daemon's own log names
        // it. Also fails if the rejection drops the value
        // the user wrote, which is the one number that tells them what to
        // edit.
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = UpDuration::from_millis(500);
        app.liveness_probe = Some(probe);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::IntervalBelowMinimum {
                probe: "liveness_probe",
                value: UpDuration::from_millis(500),
                min: MIN_LIVENESS_INTERVAL,
            }
        );
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn a_liveness_interval_exactly_at_the_floor_is_accepted() {
        // fails if the comparison is `<=` rather than `<` — the floor is a
        // value the liveness loop honours exactly, so naming it must not be
        // an error (IR-40: sweep the boundary, not just past it).
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = MIN_LIVENESS_INTERVAL;
        app.liveness_probe = Some(probe);
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn a_sub_second_readiness_interval_is_accepted() {
        // fails if both probes are validated against the liveness floor. A
        // readiness wait is bounded by `listen_timeout` and honours its
        // `interval` exactly as written (`await_ready` argues the case
        // itself), so a fast app polling every 50ms to leave `starting`
        // sooner is asking for something the daemon really does — refusing
        // it would take a working feature away to fix a clamp that only the
        // liveness loop has.
        let mut app = AppConfig::minimal("web", "./srv");
        let mut probe = probe_config("http://127.0.0.1:8080/healthz");
        probe.interval = UpDuration::from_millis(50);
        app.readiness_probe = Some(probe);
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn zero_max_memory_rejected() {
        // fails if `max_memory` is never inspected. Zero is a ceiling every
        // live process is already over, so the enforcer breaches on its
        // first reading and every reading after it — and the restart that
        // follows is automatic, which RESETS the restart budget, so
        // `max_restarts` never ends the loop.
        let mut app = AppConfig::minimal("web", "./srv");
        app.max_memory = Some(crate::values::MemSize::from_bytes(0));
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ZeroMaxMemory {
                name: "web".to_string()
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("max_memory"), "{err}");
    }

    #[test]
    fn a_nonzero_max_memory_is_accepted() {
        // fails if the check fires on `max_memory` being set at all rather
        // than on its being zero — that would refuse every app that
        // configures a limit, which is the whole feature
        let mut app = AppConfig::minimal("web", "./srv");
        app.max_memory = Some("512M".parse().unwrap());
        assert!(normalize(app).is_ok());
    }

    /// fails if a `kill_signal` shep cannot send is accepted here. Accepting it
    /// is what put SIGTERM on the wire for the life of the process with nothing
    /// but one daemon log line to say so — the clamp this rejection replaces.
    #[test]
    fn a_kill_signal_shep_cannot_send_is_refused_by_name() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.kill_signal = Some("SIGUSR1".to_string());

        let err = normalize(app).unwrap_err();

        assert_eq!(
            err,
            NormalizeError::InvalidKillSignal {
                name: "web".to_string(),
                value: "SIGUSR1".to_string(),
            }
        );
        // The message has to name the accepted set, because the operator's next
        // move is picking a different word and there is nowhere else to look.
        let rendered = err.to_string();
        assert!(rendered.contains("SIGUSR1"), "{rendered}");
        assert!(rendered.contains("SIGTERM"), "{rendered}");
        assert!(rendered.contains("SIGUSR2"), "{rendered}");
    }

    /// fails if the four supported names, their bare forms, or a lowercase
    /// spelling stop being accepted. This is the compatibility half: every
    /// spelling `stop_signal` accepted before this task must still normalize.
    #[test]
    fn every_spelling_the_daemon_already_accepted_still_normalizes() {
        for name in [
            "SIGTERM", "TERM", "sigterm", "term", "SIGINT", "INT", "SIGQUIT", "QUIT", "SIGUSR2",
            "USR2", "sigusr2",
        ] {
            let mut app = AppConfig::minimal("web", "./srv");
            app.kill_signal = Some(name.to_string());
            assert!(
                normalize(app).is_ok(),
                "`{name}` was accepted before this task and must still be"
            );
        }
    }

    /// fails if an unset `kill_signal` is refused — the overwhelmingly common
    /// case, and the one a validation pass is most likely to break by treating
    /// `None` as an empty string.
    #[test]
    fn an_unset_kill_signal_is_not_a_config_error() {
        let app = AppConfig::minimal("web", "./srv");
        assert!(app.kill_signal.is_none());
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn action_timeout_past_the_ceiling_is_rejected() {
        // fails if `action_timeout` is never inspected. One millisecond over
        // the ceiling is deliberate: a test at a round number like 60s could
        // pass by coincidence if the check used the wrong constant entirely
        // (`MAX_DEADLINE_MS` itself, say, instead of the margin under it).
        let mut app = AppConfig::minimal("web", "./srv");
        app.action_timeout = UpDuration::from_millis(MAX_ACTION_TIMEOUT.as_millis() + 1);
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ActionTimeoutTooLong {
                name: "web".to_string(),
                value: UpDuration::from_millis(MAX_ACTION_TIMEOUT.as_millis() + 1),
                max: MAX_ACTION_TIMEOUT,
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("action_timeout"), "{err}");
    }

    #[test]
    fn action_timeout_at_the_ceiling_is_accepted() {
        // fails if the comparison is `>=` rather than `>` — the ceiling
        // itself still leaves the daemon its full margin under the hard
        // clamp, so it is not one of the values nothing could ever satisfy.
        let mut app = AppConfig::minimal("web", "./srv");
        app.action_timeout = MAX_ACTION_TIMEOUT;
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn the_default_action_timeout_is_accepted() {
        // fails if `AppConfig::default()`'s own value ever drifts past the
        // ceiling normalize enforces — the one combination that must never
        // reject the config nobody customized.
        assert!(normalize(AppConfig::minimal("web", "./srv")).is_ok());
    }

    #[test]
    fn zero_watch_delay_rejected() {
        // fails if `watch_delay` is never inspected. notify's debouncer
        // derives its poll tick as `watch_delay / 4` and sleeps it on its own
        // OS thread, so zero is `loop { sleep(0); lock(); }`, a CPU-spinning
        // busy loop.
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_delay = Some(UpDuration::from_millis(0));
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::ZeroWatchDelay {
                name: "web".to_string()
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("watch_delay"), "{err}");
    }

    #[test]
    fn a_zero_watch_delay_is_rejected_with_watch_off() {
        // fails if the check is nested inside the `watch` block: an app
        // carrying `watch_delay = "0"` with `watch = false` would normalize
        // clean, and the spin would arrive the day someone flips `watch =
        // true` — the same reasoning that puts the glob checks outside it
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_delay = Some(UpDuration::from_millis(0));
        assert!(matches!(
            normalize(app).unwrap_err(),
            NormalizeError::ZeroWatchDelay { .. }
        ));
    }

    #[test]
    fn a_nonzero_watch_delay_is_accepted() {
        // fails if the check fires on `watch_delay` being set at all rather
        // than on its being zero — that would refuse every app that tunes
        // its own debounce
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_delay = Some(UpDuration::from_millis(1));
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn default_failure_threshold_from_toml_accepted() {
        // fails if the check fires on the ordinary default instead of only
        // an explicit 0. Deserializes a Flockfile snippet that omits
        // `failure_threshold` entirely, so this exercises the real
        // `#[serde(default = "default_failure_threshold")]` path
        // (config/app.rs) rather than duplicating `probe_config`'s
        // hardcoded `3` — a literal that wouldn't notice if the wired
        // default ever changed.
        let src = r#"
name = "web"
script = "./srv"

[readiness_probe]
kind = "http"
target = "http://127.0.0.1:8080/healthz"
"#;
        let app: AppConfig = toml::from_str(src).unwrap();
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn watch_true_without_cwd_rejected_naming_the_app() {
        // fails if a validator never looks at `watch`, or looks at it but
        // carries no app name, leaving the user unable to tell which
        // Flockfile entry to edit
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::WatchWithoutCwd {
                name: "web".to_string()
            }
        );
        // fails if the message regresses to a bare variant name with no
        // explanation — following the sibling precedent at app.rs:261.
        assert!(err.to_string().contains("no cwd to watch"), "{err}");
    }

    #[test]
    fn watch_true_with_cwd_accepted() {
        // fails if the check fires on `watch` alone, ignoring that a cwd was
        // actually provided
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn a_watch_options_glob_that_will_not_compile_is_rejected() {
        // fails if `watch_options` patterns are never compiled at config
        // time — the sheep would then report `online` with no watch armed
        // and nothing but a log line to say so. Also fails if the rejection
        // blames the whole list instead of the one bad pattern: the valid
        // `src/**` comes first, so an error carrying it, or carrying the
        // patterns joined together, is not the pattern the user must fix.
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_options = vec!["src/**".to_string(), "[".to_string()];
        let err = normalize(app).unwrap_err();
        assert_eq!(
            err,
            NormalizeError::InvalidWatchGlob {
                name: "web".to_string(),
                field: "watch_options",
                pattern: "[".to_string(),
                reason: Glob::new("[").unwrap_err().to_string(),
            }
        );
        // fails if the message drops the app name, the list or the pattern —
        // the three things that name the Flockfile line to edit.
        let rendered = err.to_string();
        for expected in ["web", "watch_options", "`[`"] {
            assert!(
                rendered.contains(expected),
                "{expected} missing: {rendered}"
            );
        }
    }

    #[test]
    fn an_ignore_watch_glob_that_will_not_compile_is_rejected() {
        // fails if only `watch_options` is ever compiled, leaving a mistyped
        // `ignore_watch` to cost the app its watch at arm time instead
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.ignore_watch = vec!["[".to_string()];
        match normalize(app).unwrap_err() {
            NormalizeError::InvalidWatchGlob { field, pattern, .. } => {
                assert_eq!(field, "ignore_watch");
                assert_eq!(pattern, "[");
            }
            other => panic!("expected InvalidWatchGlob, got {other:?}"),
        }
    }

    #[test]
    fn a_glob_that_will_not_compile_is_rejected_with_watch_off() {
        // fails if glob validation is nested inside the `watch` check: an app
        // carrying a mistyped glob with `watch = false` would then normalize
        // clean, and the typo would surface only the day someone flips
        // `watch = true`
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_options = vec!["[".to_string()];
        assert!(matches!(
            normalize(app).unwrap_err(),
            NormalizeError::InvalidWatchGlob { .. }
        ));
    }

    #[test]
    fn well_formed_watch_globs_are_accepted() {
        // fails if the new check rejects patterns globset compiles happily —
        // recursive `**`, a character class, a negated character class and a
        // brace alternation are all ordinary globset syntax a Flockfile is
        // entitled to use. Also fails if the check is wired to a parser that
        // is not globset's: every one of these is valid to globset and a
        // syntax error to a regex engine.
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch = true;
        app.cwd = Some("/srv/web".to_string());
        app.watch_options = vec!["src/**/*.rs".to_string(), "*.[ch]".to_string()];
        app.ignore_watch = vec!["target/**".to_string(), "**/[!.]*.{tmp,swp}".to_string()];
        assert!(normalize(app).is_ok());
    }

    #[test]
    fn watch_options_without_watch_or_cwd_accepted() {
        // fails if the check is keyed on `watch_options` being non-empty
        // rather than on `watch` being true — that would reject a Flockfile
        // that never asked to be watched
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_options = vec!["src/**".to_string()];
        assert!(normalize(app).is_ok());
    }
}
