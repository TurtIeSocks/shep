//! The hidden `daemon` subcommand: runs the supervisor in the foreground.
//!
//! This is the re-exec target `crate::launch::launch_daemon` spawns
//! detached — `shep daemon` is never meant to be typed by a person, only
//! run as the child half of the CLI's own autostart. [`run_daemon`] loads
//! `shep.toml`, boots `shep_daemon::boot`'s supervisor, and blocks in
//! `RunningDaemon::run` until a signal or `KillDaemon` tears it down.

use std::ffi::OsStr;
use std::io::IsTerminal;

use shep_core::config::{DaemonConfig, DaemonConfigError};
use shep_core::paths::ShepPaths;
use shep_core::values::UpDuration;
use shep_daemon::boot::{BootError, BootOptions, boot};
use shep_daemon::tokio_runner::TokioRunner;
use tracing_subscriber::EnvFilter;

use crate::cli::DaemonArgs;
use crate::exit::ExitCode;

/// Everything [`run_daemon`] can fail with. Per-module, per IR-18, and the
/// same shape shep-daemon itself uses (`BootError`, `SnapshotError`,
/// `RunnerError`, `SysError`, `ConnError` — one per module, no umbrella).
///
/// It exists because [`BootError`] cannot carry a config failure: its four
/// variants are `Io { path, source }`, `AlreadyRunning { pid }`,
/// `Snapshot(SnapshotError)` and `ReadyWrite(io::Error)`, none of which
/// represents a bad `shep.toml`. Returning `Result<(), BootError>` while
/// also being required to map a [`DaemonConfigError`] to
/// [`ExitCode::InvalidConfig`] is not satisfiable, and widening
/// `BootError` would mean editing merged daemon code that is off-limits
/// for this phase.
///
/// [`Self::Boot`] and [`Self::Run`] both wrap a [`BootError`] — `boot()`
/// and `RunningDaemon::run()` share that error type — but they are kept as
/// separate variants rather than one, because they are not the same fault:
/// a `BootError` from `run()` means the supervisor came up and served
/// (possibly for a long time) and only failed during its run loop or
/// teardown, which "the daemon failed to boot" would misreport.
#[derive(Debug)]
pub enum DaemonRunError {
    /// `shep.toml` was unreadable as config.
    Config(DaemonConfigError),
    /// The supervisor failed to come up, before it ever served a request.
    Boot(BootError),
    /// The supervisor came up and served, then failed during its run loop
    /// or teardown.
    Run(BootError),
}

impl core::fmt::Display for DaemonRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Config(err) => write!(f, "invalid daemon configuration: {err}"),
            Self::Boot(err) => write!(f, "the daemon failed to boot: {err}"),
            Self::Run(err) => write!(f, "the daemon failed while running: {err}"),
        }
    }
}

impl core::error::Error for DaemonRunError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Config(err) => Some(err),
            Self::Boot(err) | Self::Run(err) => Some(err),
        }
    }
}

/// Loads `paths.daemon_config`'s raw source, treating a missing file as "no
/// file" rather than an error.
///
/// A missing `shep.toml` is not a fault — [`DaemonConfig::load`] already
/// treats `None` as "use every default" — so only
/// [`std::io::ErrorKind::NotFound`] gets swallowed here. Any other IO
/// failure (permissions, a directory where a file was expected, …) is a
/// real fault on a real path, which is exactly what [`BootError::Io`]
/// represents; it is not a config-*content* problem, so it is reported
/// through [`DaemonRunError::Boot`] rather than [`DaemonRunError::Config`].
fn read_daemon_config_source(paths: &ShepPaths) -> Result<Option<String>, DaemonRunError> {
    match std::fs::read_to_string(&paths.daemon_config) {
        Ok(src) => Ok(Some(src)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DaemonRunError::Boot(BootError::Io {
            path: paths.daemon_config.clone(),
            source,
        })),
    }
}

/// Installs the subscriber that renders the daemon's own records, for the
/// remaining life of this process.
///
/// Everything the daemon has to say about itself — a watch that could not be
/// registered, a cron pattern that would not parse, the observed RSS behind a
/// memory restart — goes through `tracing`, and a `tracing` record with no
/// subscriber is discarded where it is written. This is the one *global*
/// install in the workspace — `shep-daemon`'s `testing::capture_logs` installs
/// a scoped one per test, which is a different thing and deliberately not
/// this.
///
/// The sink is **stderr**, never a file this function opens: `launch.rs`
/// already redirects the re-exec'd daemon's stderr into
/// `$SHEP_HOME/logs/shepd.err.log`, so naming a file here would duplicate the
/// launcher's job and diverge the moment a daemon is run by hand — where the
/// parent's terminal is exactly where its records belong. Colour follows the
/// sink for the same reason ([`ansi_enabled`]): escape codes are noise in
/// `shepd.err.log` and what a terminal is for.
///
/// Records are written from tokio worker threads, so `main::run`'s `daemon`
/// arm must not be holding a `stderr().lock()` guard while this process runs.
/// Its own comment carries why, and what happens when it does.
///
/// `EnvFilter::new` neither fails nor panics on an unparseable directive: it
/// is `builder().with_default_directive(ERROR).parse_lossy(..)`, which
/// *ignores* what it cannot parse — saying `ignoring …` on stderr — and is
/// left with its `ERROR`-only default. The failure mode to guard against is
/// therefore a daemon that silently says nothing below `error` after being
/// configured for `trace`, never a crash. It is out of reach here because the
/// input is [`LogLevel::as_str`], a closed set of six literals, each a valid
/// directive on its own; widening that grammar is what would bring it back.
///
/// A failed install is reported on the same stderr rather than failing the
/// boot. It means a subscriber is already installed, which the shipped binary
/// cannot do: `main` reaches [`run_daemon`] once per process and nothing else
/// installs one. A supervisor that refused to supervise because its
/// diagnostics could not be wired would be the worse of the two outcomes.
///
/// [`LogLevel::as_str`]: shep_core::config::LogLevel::as_str
fn install_log_subscriber(config: &DaemonConfig) {
    let builder = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(config.daemon.log_level.as_str()))
        .with_writer(std::io::stderr);
    let installed = if config.daemon.log_json {
        builder.json().try_init()
    } else {
        builder
            .with_ansi(ansi_enabled(
                std::io::stderr().is_terminal(),
                std::env::var_os("NO_COLOR").as_deref(),
            ))
            .try_init()
    };
    if let Err(err) = installed {
        eprintln!("shep: the daemon's own logs are not being rendered: {err}");
    }
}

/// Whether ANSI colour belongs on the daemon's own records: only when stderr
/// is a terminal, and only when `NO_COLOR` is unset or empty.
///
/// `NO_COLOR` is honoured even though `RUST_LOG` is deliberately ignored, and
/// the two are not the same kind of variable. `RUST_LOG` would be a second way
/// to configure *shep*, competing with `[daemon] log_level` and
/// `SHEP_LOG_LEVEL` over one decision — which is what the `SHEP_`-prefix rule
/// exists to prevent, and that rule governs our own knobs. `NO_COLOR` is a
/// cross-ecosystem convention about the terminal, answered the same way by
/// every well-behaved program sharing it, so it is not ours to opt out of.
/// Reading it here rather than dropping it back is the deliberate call.
///
/// Empty means unset, per that convention: only a non-empty value suppresses
/// colour.
fn ansi_enabled(stderr_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    stderr_is_terminal && no_color.is_none_or(OsStr::is_empty)
}

/// Whether `config` sets a dog-related knob this build has no code to act
/// on: `[daemon] enabled_dogs` or any `[dog.<name>]` section.
///
/// Both parse, validate and round-trip today ([`DaemonConfig::load`],
/// [`DaemonSection`]) — spec §8's dogs infrastructure is not built yet, so
/// nothing in this binary reads either one. An operator who writes
/// `enabled_dogs = ["metrics"]` gets a daemon that boots and does nothing
/// with it.
///
/// [`DaemonSection`]: shep_core::config::daemon::DaemonSection
fn dog_config_is_inert(config: &DaemonConfig) -> bool {
    !config.daemon.enabled_dogs.is_empty() || !config.dog.is_empty()
}

/// Warns once, at boot, if [`dog_config_is_inert`] — never refuses to boot.
///
/// A hard error here would be disproportionate to what the field actually
/// does (nothing) and would break a daemon that boots cleanly today the
/// moment dogs infrastructure lands and starts reading the same field for
/// real — a config author has no way to know from the file alone which of
/// those two worlds they are in. A log line trades that all-or-nothing
/// choice for what the gap actually is: worth knowing about, not worth
/// stopping for.
fn warn_on_inert_dog_config(config: &DaemonConfig) {
    if dog_config_is_inert(config) {
        tracing::warn!(
            enabled_dogs = config.daemon.enabled_dogs.len(),
            dog_sections = config.dog.len(),
            "shep.toml configures dogs, but this build has no dogs infrastructure yet \
             — enabled_dogs and [dog.*] sections have no effect",
        );
    }
}

/// Runs the supervisor in this process until a signal or `KillDaemon`.
///
/// Loads `shep.toml` (a missing file is not an error — see
/// [`read_daemon_config_source`]) plus `SHEP_*` environment overrides, installs
/// the log subscriber those two knobs configure (see
/// [`install_log_subscriber`]), warns if the loaded config is [dog-inert]
/// (see [`warn_on_inert_dog_config`]), folds `args` in via [`boot_options`],
/// then boots and serves. The re-exec'd child inherits a real environment on
/// purpose (`launch::launch_command` deliberately does not `.env_clear()`), so
/// `SHEP_LOG_JSON`, `SHEP_LOG_LEVEL`, `SHEP_SOCKET`, and
/// `SHEP_MAX_CRON_SLEEP` are read straight from `std::env::var`.
///
/// The subscriber goes in *here* and never in `shep_daemon::boot`: a global
/// subscriber can be installed once per process, and `boot` is called many
/// times over by one test binary. This function is called once, by `main`, and
/// the e2e tier reaches it as a subprocess.
///
/// # Errors
/// - [`DaemonRunError::Config`] — `shep.toml` failed to parse, or a
///   `SHEP_*` override held an unparseable value.
/// - [`DaemonRunError::Boot`] — the config file itself could not be read
///   (any IO error other than "does not exist"), or the supervisor failed
///   to boot.
/// - [`DaemonRunError::Run`] — the supervisor came up and served, then
///   failed during its run loop or teardown.
///
/// [dog-inert]: dog_config_is_inert
pub async fn run_daemon(paths: ShepPaths, args: &DaemonArgs) -> Result<(), DaemonRunError> {
    let env = |key: &str| std::env::var(key).ok();
    let file_source = read_daemon_config_source(&paths)?;
    let config =
        DaemonConfig::load(file_source.as_deref(), &env).map_err(DaemonRunError::Config)?;
    install_log_subscriber(&config);
    warn_on_inert_dog_config(&config);
    let options = boot_options(&config, args);
    boot(TokioRunner::new(), paths, options)
        .await
        .map_err(DaemonRunError::Boot)?
        .run()
        .await
        .map_err(DaemonRunError::Run)
}

/// Builds [`BootOptions`] from `config` and the `daemon` subcommand's own
/// flags.
///
/// `ready_fd` stays `None` unconditionally: readiness is established by a
/// completed handshake in this phase's design, never by an inherited
/// descriptor (see this crate's `#![forbid(unsafe_code)]`).
///
/// `max_cron_sleep` stays an `Option` the whole way through: the daemon owns
/// the default and applies it once, so nothing here invents a value on the
/// way past.
#[must_use]
pub fn boot_options(config: &DaemonConfig, args: &DaemonArgs) -> BootOptions {
    BootOptions {
        socket: config.daemon.socket.clone(),
        ready_fd: None,
        restore: !args.no_restore,
        max_cron_sleep: config.daemon.max_cron_sleep.map(UpDuration::as_duration),
    }
}

/// Maps a boot or run failure to the process exit status the parent will
/// read.
///
/// [`BootError`] is not `#[non_exhaustive]` and has exactly four variants,
/// so the [`DaemonRunError::Boot`] arm matches it exhaustively rather than
/// carrying a `_` arm that would silently absorb a fifth variant added
/// later. [`DaemonRunError::Run`] maps unconditionally to
/// [`ExitCode::Failure`] rather than re-inspecting the inner `BootError`:
/// `RunningDaemon::run()`'s own `# Errors` section names only
/// `BootError::Io`, so `AlreadyRunning` — the one variant with its own
/// dedicated code — can only ever come from the [`DaemonRunError::Boot`]
/// arm, where a daemon has not yet claimed the flock; a daemon that already
/// served has no "already running" outcome left to report.
#[must_use]
pub fn daemon_exit_code(err: &DaemonRunError) -> ExitCode {
    match err {
        DaemonRunError::Config(_) => ExitCode::InvalidConfig,
        DaemonRunError::Boot(boot_err) => match boot_err {
            BootError::AlreadyRunning { .. } => ExitCode::DaemonAlreadyRunning,
            BootError::Io { .. } | BootError::Snapshot(_) | BootError::ReadyWrite(_) => {
                ExitCode::Failure
            }
        },
        DaemonRunError::Run(_) => ExitCode::Failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colour is a terminal's business and `NO_COLOR`'s, in that order.
    ///
    /// fails if the `NO_COLOR` read is dropped (the third case turns true
    /// again), and fails if it is read as a plain presence check rather than
    /// per the convention (the fourth case turns false, and a shell that
    /// exports an empty `NO_COLOR=` silently loses its colour).
    #[test]
    fn colour_needs_a_terminal_and_no_no_color() {
        assert!(ansi_enabled(true, None));
        assert!(!ansi_enabled(false, None), "a file never gets escape codes");
        assert!(!ansi_enabled(true, Some(OsStr::new("1"))));
        assert!(
            ansi_enabled(true, Some(OsStr::new(""))),
            "an empty NO_COLOR is an unset NO_COLOR"
        );
        assert!(
            !ansi_enabled(false, Some(OsStr::new("1"))),
            "the two reasons to suppress colour must not cancel out"
        );
    }

    /// `dog_config_is_inert` is what [`warn_on_inert_dog_config`] gates on,
    /// so this pins the predicate directly rather than capturing a global
    /// tracing subscriber to observe the log line it drives.
    ///
    /// Three cases: a config with neither knob set is not inert (nothing to
    /// warn about); `enabled_dogs` alone trips it; a `[dog.<name>]` section
    /// with `enabled_dogs` left empty trips it too — an operator writing
    /// per-dog config ahead of `enable`-ing it is exactly as inert as one
    /// who only enabled a dog, and a predicate that checked just one field
    /// would miss half of what this function exists to catch.
    #[test]
    fn dog_config_is_inert_catches_either_knob_alone() {
        let neither = DaemonConfig::load(None, &|_| None).unwrap();
        assert!(!dog_config_is_inert(&neither));

        let enabled_only =
            DaemonConfig::load(Some("[daemon]\nenabled_dogs = [\"metrics\"]\n"), &|_| None)
                .unwrap();
        assert!(dog_config_is_inert(&enabled_only));

        let section_only =
            DaemonConfig::load(Some("[dog.metrics]\nport = 9615\n"), &|_| None).unwrap();
        assert!(dog_config_is_inert(&section_only));
    }

    #[test]
    fn boot_options_pass_ready_fd_none_and_the_configured_socket() {
        let config =
            DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/custom.sock\"\n"), &|_| None)
                .unwrap();
        let opts = boot_options(&config, &DaemonArgs { no_restore: false });
        assert!(
            opts.ready_fd.is_none(),
            "readiness is a handshake in this phase"
        );
        assert_eq!(
            opts.socket.as_deref(),
            Some(std::path::Path::new("/tmp/custom.sock"))
        );
        assert!(opts.restore, "the default is to restore the muster roll");
    }

    /// The knob has to survive the trip from `shep.toml` into `BootOptions`.
    /// Fails if the field is dropped on the floor between the two structs —
    /// the entire failure mode of a knob nobody plumbed — and fails if a
    /// default is invented here instead of being left to the daemon, which
    /// owns the one place `DEFAULT_MAX_CRON_SLEEP` is applied.
    #[test]
    fn boot_options_carry_the_configured_max_cron_sleep_and_invent_none() {
        let configured =
            DaemonConfig::load(Some("[daemon]\nmax_cron_sleep = \"5m\"\n"), &|_| None).unwrap();
        assert_eq!(
            boot_options(&configured, &DaemonArgs { no_restore: false }).max_cron_sleep,
            Some(core::time::Duration::from_secs(300))
        );

        let unset = DaemonConfig::load(None, &|_| None).unwrap();
        assert_eq!(
            boot_options(&unset, &DaemonArgs { no_restore: false }).max_cron_sleep,
            None,
            "an unset knob must stay None: the daemon owns the default"
        );
    }

    /// The negated flag has to actually reach `BootOptions`. With the old
    /// `#[arg(long, default_value_t = true)] restore: bool` there was no
    /// argv that produced `false`, so this case could not be written at
    /// all.
    #[test]
    fn no_restore_boots_without_the_muster_roll() {
        let config = DaemonConfig::load(None, &|_| None).unwrap();
        let opts = boot_options(&config, &DaemonArgs { no_restore: true });
        assert!(!opts.restore);
    }

    #[test]
    fn already_running_gets_its_own_exit_code_and_everything_else_is_failure() {
        use DaemonRunError::{Boot, Config, Run};
        assert_eq!(
            daemon_exit_code(&Boot(BootError::AlreadyRunning { pid: Some(7) })),
            ExitCode::DaemonAlreadyRunning
        );
        assert_eq!(
            daemon_exit_code(&Boot(BootError::AlreadyRunning { pid: None })),
            ExitCode::DaemonAlreadyRunning
        );
        assert_eq!(
            daemon_exit_code(&Boot(BootError::Io {
                path: "/x".into(),
                source: std::io::Error::other("x"),
            })),
            ExitCode::Failure
        );
        // A teardown failure reported through `Run` stays `Failure` even
        // when it wraps the same `BootError::Io` variant that, through
        // `Boot`, can share the value with other `Failure`-mapped
        // variants — `Run` never earns `DaemonAlreadyRunning`, because a
        // daemon that already served has no such outcome left to report.
        assert_eq!(
            daemon_exit_code(&Run(BootError::Io {
                path: "/x".into(),
                source: std::io::Error::other("x"),
            })),
            ExitCode::Failure
        );
        // The mapping that was unreachable through the old
        // `Result<(), BootError>`.
        assert_eq!(
            daemon_exit_code(&Config(DaemonConfigError::Toml("expected `=`".into()))),
            ExitCode::InvalidConfig
        );
        assert_eq!(
            daemon_exit_code(&Config(DaemonConfigError::BadEnvValue(
                "SHEP_LOG_JSON",
                "maybe".into()
            ))),
            ExitCode::InvalidConfig
        );
    }

    /// `Boot` and `Run` must not say the same thing about *when* the
    /// failure happened — that distinction is the whole reason `Run`
    /// exists (see the enum's own doc): a daemon that served for a week and
    /// then failed during teardown must not be reported as having "failed
    /// to boot".
    #[test]
    fn boot_and_run_report_different_phases_for_the_same_underlying_error() {
        use DaemonRunError::{Boot, Run};
        let io_err = || BootError::Io {
            path: "/x".into(),
            source: std::io::Error::other("x"),
        };
        let boot_msg = Boot(io_err()).to_string();
        let run_msg = Run(io_err()).to_string();
        // Both wrap the same `BootError`, whose own `Display` still says
        // "boot step failed" regardless of phase — that wording lives in
        // shep-daemon, off-limits for this fix (see this enum's own doc).
        // What must differ is `DaemonRunError`'s own outer wording: only
        // `Boot` may claim the daemon "failed to boot".
        assert_ne!(boot_msg, run_msg);
        assert!(boot_msg.starts_with("the daemon failed to boot"));
        assert!(
            !run_msg.starts_with("the daemon failed to boot"),
            "a run-phase failure must not still claim to be a boot failure: {run_msg:?}"
        );
        assert!(run_msg.starts_with("the daemon failed while running"));
    }
}
