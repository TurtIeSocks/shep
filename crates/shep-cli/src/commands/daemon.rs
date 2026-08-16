//! The hidden `daemon` subcommand: runs the supervisor in this process.
//!
//! [`run_daemon`] loads `shep.toml`, boots `shep_daemon::boot`'s
//! supervisor, and blocks in `RunningDaemon::run` until a signal or
//! `KillDaemon` tears it down. Two things reach it, and they are different
//! arrangements rather than different code:
//!
//! - **Autostart** — the re-exec target `crate::launch::launch_daemon`
//!   spawns detached, which is how a `shep start` typed at a terminal gets
//!   a daemon. The parent exits; this process is orphaned deliberately.
//!   `launch_daemon` passes exactly one argument, `daemon`.
//! - **`--foreground`** — an init system `exec`s this itself and stays the
//!   parent, so nothing may exit out from under it, and it wants to be told
//!   when the flock is actually back. The flag adds that report (see
//!   [`boot_options`]) and nothing else: it does not fork, re-exec, or
//!   change a single step of the boot the autostart path takes. Everything
//!   that makes this process survivable on its own — detaching from the
//!   parent's process group and terminal, redirecting stderr into
//!   `shepd.err.log` — already lives in `launch.rs`, on the *parent's* side
//!   of a re-exec that this arrangement never performs. Systemd does those
//!   jobs itself.
//!
//! Neither arrangement can be reached by accident from the other:
//! `launch_daemon` never passes `--foreground` (its own test pins the
//! argument vector), and the flag is the only thing that turns readiness
//! reporting on, so an inherited `$NOTIFY_SOCKET` cannot make an
//! autostarted daemon answer some other service's unit.

use std::ffi::OsStr;
use std::io::IsTerminal;

use shep_core::config::{DaemonConfig, DaemonConfigError, DaemonOverrides};
use shep_core::paths::ShepPaths;
use shep_core::protocol::DogSource;
use shep_core::values::UpDuration;
use shep_daemon::boot::{BootError, BootOptions, RunningDaemon, boot};
use shep_daemon::dogs::DogSpec;
use shep_daemon::notify::NOTIFY_SOCKET_ENV;
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

/// Loads config, installs the log subscriber, and boots the supervisor —
/// everything [`run_daemon`] does except serve.
///
/// Loads `shep.toml` (a missing file is not an error — see
/// [`read_daemon_config_source`]) layered under `SHEP_*` environment
/// overrides and, on top of those, the `daemon` subcommand's own flags (spec
/// §5's `file < env < flags` — see [`daemon_overrides`]), installs the log
/// subscriber those layers configure (see [`install_log_subscriber`]), folds
/// `args` in via [`boot_options`] — which is also where `[daemon]
/// enabled_dogs`/`adopted_dogs` become the dogs `boot` starts once the flock
/// is back — then boots. The re-exec'd child inherits a real environment on
/// purpose (`launch::launch_command` deliberately does not `.env_clear()`),
/// so `SHEP_LOG_JSON`, `SHEP_LOG_LEVEL`, `SHEP_SOCKET`, and
/// `SHEP_MAX_CRON_SLEEP` are read straight from `std::env::var` — the
/// environment is the *middle* layer now, not the top one; `--log-json`,
/// `--log-level`, `--socket` and `--max-cron-sleep` on the invocation itself
/// win over all of it.
/// `$NOTIFY_SOCKET` is read here too — the one read of it in the workspace
/// — and is the one variable in that list shep does not own the name of:
/// an init system sets it, and `--foreground` is what decides whether it is
/// acted on.
///
/// The subscriber goes in *here* and never in `shep_daemon::boot`: a global
/// subscriber can be installed once per process, and `boot` is called many
/// times over by one test binary. This function is called once per process —
/// by [`run_daemon`] for the hidden `daemon` subcommand, and by
/// `commands::foreground` for `runtime`/`dev` — and the e2e tier reaches it
/// as a subprocess.
///
/// Split out of what used to be `run_daemon`'s own body for
/// `commands::foreground`, which needs the booted daemon in hand rather than
/// a call that blocks until shutdown: it spawns `run()` as a task and then
/// talks to the same supervisor over its own socket, like any other client.
/// Nothing about the boot differs between the two callers, and the split is
/// what keeps that true.
///
/// `delete_flock_on_shutdown` becomes [`BootOptions::delete_flock_on_shutdown`]
/// verbatim — see that field's own doc. `run_daemon` below always passes
/// `false`; `commands::foreground::run` is the one caller that passes its
/// own `tidy_up`, so `shep dev`'s isolated session gets an empty roll on
/// the way out even when it ends by signal rather than through its own
/// `Stop`/`Delete` requests.
///
/// # Errors
/// - [`DaemonRunError::Config`] — `shep.toml` failed to parse, or a
///   `SHEP_*` override held an unparseable value.
/// - [`DaemonRunError::Boot`] — the config file itself could not be read
///   (any IO error other than "does not exist"), or the supervisor failed
///   to boot.
pub async fn boot_supervisor(
    paths: ShepPaths,
    args: &DaemonArgs,
    delete_flock_on_shutdown: bool,
) -> Result<RunningDaemon, DaemonRunError> {
    let env = |key: &str| std::env::var(key).ok();
    let file_source = read_daemon_config_source(&paths)?;
    let overrides = daemon_overrides(args);
    let config = DaemonConfig::load_layered(file_source.as_deref(), &env, &overrides)
        .map_err(DaemonRunError::Config)?;
    install_log_subscriber(&config);
    // The one read of this variable in the workspace, here beside every
    // `SHEP_*` override rather than inside shep-daemon — see
    // `shep_daemon::notify::NOTIFY_SOCKET_ENV`'s own doc.
    let notify_socket = std::env::var_os(NOTIFY_SOCKET_ENV);
    let mut options = boot_options(&config, args, notify_socket.as_deref());
    options.delete_flock_on_shutdown = delete_flock_on_shutdown;
    boot(TokioRunner::new(), paths, options)
        .await
        .map_err(DaemonRunError::Boot)
}

/// Runs the supervisor in this process until a signal or `KillDaemon`.
///
/// [`boot_supervisor`] does everything up to and including the boot; this
/// adds only `.run()` on top of it. See that function's own doc for the
/// config-loading and log-subscriber detail this used to carry directly.
///
/// # Errors
/// - [`DaemonRunError::Config`] — `shep.toml` failed to parse, or a
///   `SHEP_*` override held an unparseable value.
/// - [`DaemonRunError::Boot`] — the config file itself could not be read
///   (any IO error other than "does not exist"), or the supervisor failed
///   to boot.
/// - [`DaemonRunError::Run`] — the supervisor came up and served, then
///   failed during its run loop or teardown.
pub async fn run_daemon(paths: ShepPaths, args: &DaemonArgs) -> Result<(), DaemonRunError> {
    // A production daemon always keeps its final roll — `shep muster` after
    // a reboot is the entire reason it exists.
    boot_supervisor(paths, args, false)
        .await?
        .run()
        .await
        .map_err(DaemonRunError::Run)
}

/// Builds the CLI-flag layer of `file < env < flags` (spec §5) from the
/// `daemon` subcommand's own arguments.
///
/// Extracted out of [`run_daemon`] rather than inlined so the chain from
/// argv to a validated [`DaemonConfig`] is testable without booting anything
/// — see `every_daemon_flag_reaches_the_config` below.
#[must_use]
pub fn daemon_overrides(args: &DaemonArgs) -> DaemonOverrides {
    DaemonOverrides::new()
        .log_json(args.log_json)
        .log_level(args.log_level)
        .socket(args.socket.clone())
        .max_cron_sleep(args.max_cron_sleep)
}

/// Builds [`BootOptions`] from `config`, the `daemon` subcommand's own
/// flags, and whatever `$NOTIFY_SOCKET` held.
///
/// `ready_fd` stays `None` unconditionally: readiness is established by a
/// completed handshake in this phase's design, never by an inherited
/// descriptor (see this crate's `#![forbid(unsafe_code)]`).
///
/// `max_cron_sleep` stays an `Option` the whole way through: the daemon owns
/// the default and applies it once, so nothing here invents a value on the
/// way past.
///
/// `notify_socket` is taken as a parameter rather than read here, so the
/// environment read stays at the one call site in [`run_daemon`] and this
/// function stays testable — `std::env::set_var` is `unsafe` in edition
/// 2024, which this crate forbids outright. `--foreground` gates it: a shep
/// the CLI autostarted from *inside* some other notify-type service
/// inherits that service's `$NOTIFY_SOCKET`, and must not report its
/// readiness by accident.
///
/// `dogs` is where this function earns its keep over a plain field-by-field
/// copy: `[daemon] enabled_dogs` names each dog to start, in the order an
/// operator wrote it, and `[daemon] adopted_dogs` says which of those names
/// is a third-party binary rather than a built-in one — a name absent from
/// the map is [`DogSource::BuiltIn`] by construction, which is exactly what
/// [`DaemonSection::adopted_dogs`](shep_core::config::daemon::DaemonSection::adopted_dogs)'s
/// own doc promises. The map holds a `PathBuf`; [`DogSource::Adopted`]
/// holds a `String`, because the wire already refuses a non-UTF-8
/// `PathBuf` outright, so `display().to_string()` here is lossy exactly
/// where that refusal already is, and nowhere else this value travels.
#[must_use]
pub fn boot_options(
    config: &DaemonConfig,
    args: &DaemonArgs,
    notify_socket: Option<&OsStr>,
) -> BootOptions {
    BootOptions {
        socket: config.daemon.socket.clone(),
        ready_fd: None,
        restore: !args.no_restore,
        max_cron_sleep: config.daemon.max_cron_sleep.map(UpDuration::as_duration),
        notify_socket: notify_socket
            .filter(|_| args.foreground)
            .map(OsStr::to_os_string),
        dogs: config
            .daemon
            .enabled_dogs
            .iter()
            .map(|name| {
                let source = match config.daemon.adopted_dogs.get(name) {
                    Some(path) => DogSource::Adopted {
                        path: path.display().to_string(),
                    },
                    None => DogSource::BuiltIn,
                };
                DogSpec {
                    name: name.clone(),
                    source,
                }
            })
            .collect(),
        // Overwritten by `boot_supervisor`, the only caller that ever wants
        // `true` — this function has no `tidy_up`/`dev` concept of its own
        // to read it from.
        delete_flock_on_shutdown: false,
    }
}

/// Maps a boot or run failure to the process exit status the parent will
/// read.
///
/// [`BootError`] is `#[non_exhaustive]` (IR-20), so the
/// [`DaemonRunError::Boot`] arm carries a wildcard rather than naming all
/// four of today's variants: a boot failure this crate does not yet know
/// about should still exit non-zero, same as [`BootError::Io`],
/// [`BootError::Snapshot`], and [`BootError::ReadyWrite`] already do — only
/// [`BootError::AlreadyRunning`] gets its own code. [`DaemonRunError::Run`]
/// maps unconditionally to [`ExitCode::Failure`] rather than re-inspecting
/// the inner `BootError`: `RunningDaemon::run()`'s own `# Errors` section
/// names only `BootError::Io`, so `AlreadyRunning` — the one variant with
/// its own dedicated code — can only ever come from the
/// [`DaemonRunError::Boot`] arm, where a daemon has not yet claimed the
/// flock; a daemon that already served has no "already running" outcome
/// left to report.
#[must_use]
pub fn daemon_exit_code(err: &DaemonRunError) -> ExitCode {
    match err {
        DaemonRunError::Config(_) => ExitCode::InvalidConfig,
        DaemonRunError::Boot(boot_err) => match boot_err {
            BootError::AlreadyRunning { .. } => ExitCode::DaemonAlreadyRunning,
            // BootError::Io/Snapshot/ReadyWrite today, plus any future
            // variant IR-20's `#[non_exhaustive]` makes room for.
            _ => ExitCode::Failure,
        },
        DaemonRunError::Run(_) => ExitCode::Failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::config::LogLevel;

    /// fails if `run_daemon` builds its overrides from the wrong fields, or
    /// drops one. Drives the config assembly, not the boot — booting a real
    /// shepherd is `daemon_e2e`'s job. Asserts all four fields rather than
    /// sampling one, which is what makes the mutation below land on exactly
    /// one assertion.
    #[test]
    fn every_daemon_flag_reaches_the_config() {
        let args = DaemonArgs {
            no_restore: false,
            foreground: false,
            log_json: Some(true),
            log_level: Some(LogLevel::Trace),
            socket: Some(std::path::PathBuf::from("/tmp/flag.sock")),
            max_cron_sleep: Some(UpDuration::from_millis(120_000)),
        };
        let cfg = DaemonConfig::load_layered(
            Some(
                "[daemon]\nlog_json = false\nlog_level = \"error\"\nsocket = \"/tmp/file.sock\"\n",
            ),
            &|_| None,
            &daemon_overrides(&args),
        )
        .unwrap();
        assert!(cfg.daemon.log_json);
        assert_eq!(cfg.daemon.log_level, LogLevel::Trace);
        assert_eq!(
            cfg.daemon.socket,
            Some(std::path::PathBuf::from("/tmp/flag.sock"))
        );
        assert_eq!(
            cfg.daemon.max_cron_sleep,
            Some(UpDuration::from_millis(120_000))
        );
    }

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

    #[test]
    fn boot_options_pass_ready_fd_none_and_the_configured_socket() {
        let config =
            DaemonConfig::load(Some("[daemon]\nsocket = \"/tmp/custom.sock\"\n"), &|_| None)
                .unwrap();
        let opts = boot_options(
            &config,
            &DaemonArgs {
                no_restore: false,
                foreground: false,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            None,
        );
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

    /// fails if `enabled_dogs` or `adopted_dogs` is dropped between
    /// `shep.toml` and `BootOptions` — the entire failure mode of a knob
    /// nobody plumbed, and the one this file has been warning about since
    /// the field was added with no reader. Both halves: a bare name is a
    /// built-in, and a name with a path is adopted.
    #[test]
    fn boot_options_carry_every_enabled_dog_with_the_source_the_file_names() {
        let src = r#"
[daemon]
enabled_dogs = ["metrics", "otel"]

[daemon.adopted_dogs]
otel = "/usr/local/bin/shep-otel"
"#;
        let config = DaemonConfig::load(Some(src), &|_| None).unwrap();
        let opts = boot_options(
            &config,
            &DaemonArgs {
                no_restore: false,
                foreground: false,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            None,
        );
        assert_eq!(
            opts.dogs,
            vec![
                DogSpec {
                    name: "metrics".into(),
                    source: DogSource::BuiltIn
                },
                DogSpec {
                    name: "otel".into(),
                    source: DogSource::Adopted {
                        path: "/usr/local/bin/shep-otel".into()
                    }
                },
            ]
        );
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
            boot_options(
                &configured,
                &DaemonArgs {
                    no_restore: false,
                    foreground: false,
                    log_json: None,
                    log_level: None,
                    socket: None,
                    max_cron_sleep: None,
                },
                None
            )
            .max_cron_sleep,
            Some(core::time::Duration::from_secs(300))
        );

        let unset = DaemonConfig::load(None, &|_| None).unwrap();
        assert_eq!(
            boot_options(
                &unset,
                &DaemonArgs {
                    no_restore: false,
                    foreground: false,
                    log_json: None,
                    log_level: None,
                    socket: None,
                    max_cron_sleep: None,
                },
                None
            )
            .max_cron_sleep,
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
        let opts = boot_options(
            &config,
            &DaemonArgs {
                no_restore: true,
                foreground: false,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            None,
        );
        assert!(!opts.restore);
    }

    /// fails if `--foreground` stops reaching the boot option. The flag is
    /// the only thing that turns readiness reporting on, and a unit whose
    /// ExecStart lost it hangs until TimeoutStartSec with nothing to say why.
    #[test]
    fn the_foreground_flag_reaches_the_boot_options() {
        let config = DaemonConfig::load(None, &|_| None).unwrap();
        let bare = boot_options(
            &config,
            &DaemonArgs {
                no_restore: false,
                foreground: false,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            None,
        );
        assert!(
            bare.notify_socket.is_none(),
            "an autostarted daemon reports to nobody"
        );

        let supervised = boot_options(
            &config,
            &DaemonArgs {
                no_restore: false,
                foreground: true,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            Some(OsStr::new("/run/systemd/notify")),
        );
        assert_eq!(
            supervised.notify_socket.as_deref(),
            Some(OsStr::new("/run/systemd/notify"))
        );

        // Without the flag the address is ignored, so a shep the CLI
        // autostarted from inside some other notify-type service cannot
        // report ITS readiness by accident.
        let unflagged = boot_options(
            &config,
            &DaemonArgs {
                no_restore: false,
                foreground: true,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            None,
        );
        assert!(unflagged.notify_socket.is_none());

        let inherited = boot_options(
            &config,
            &DaemonArgs {
                no_restore: false,
                foreground: false,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            Some(OsStr::new("/run/systemd/notify")),
        );
        assert!(inherited.notify_socket.is_none());
    }

    /// fails if `--foreground` ever starts implying `--no-restore`, or the
    /// other way round. The two are independent, and a foreground daemon
    /// that skipped the restore would report a unit ready while supervising
    /// an empty flock — the exact outcome the readiness ordering exists to
    /// rule out.
    #[test]
    fn foreground_and_no_restore_are_independent() {
        let config = DaemonConfig::load(None, &|_| None).unwrap();
        let opts = boot_options(
            &config,
            &DaemonArgs {
                no_restore: false,
                foreground: true,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            Some(OsStr::new("/run/systemd/notify")),
        );
        assert!(opts.restore, "a supervised daemon still musters its roll");
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
