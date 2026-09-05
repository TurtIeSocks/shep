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

use shep_client::{Client, ConnectError};
use shep_core::config::{DaemonConfig, DaemonConfigError, DaemonOverrides};
use shep_core::paths::ShepPaths;
use shep_core::protocol::DogSource;
use shep_core::values::UpDuration;
use shep_daemon::boot::{self, BootError, BootOptions, RunningDaemon, Shepherd, boot};
use shep_daemon::dogs::DogSpec;
#[cfg(unix)]
use shep_daemon::notify::NOTIFY_SOCKET_ENV;
use shep_daemon::tokio_runner::TokioRunner;
use tracing_subscriber::EnvFilter;

use crate::cli::DaemonArgs;
use crate::commands::dog_migration::{self, DogMigrationError};
use crate::commands::{admin, muster};
use crate::exit::ExitCode;
use crate::output::Streams;

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
    /// `[dog.<name>]` sections could not be moved out of `shep.toml` and
    /// into `dogs.toml`. Raised before the supervisor comes up, so no dog
    /// has read a section from either file yet.
    DogMigration(DogMigrationError),
}

impl core::fmt::Display for DaemonRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Config(err) => write!(f, "invalid daemon configuration: {err}"),
            Self::Boot(err) => write!(f, "the daemon failed to boot: {err}"),
            Self::Run(err) => write!(f, "the daemon failed while running: {err}"),
            Self::DogMigration(err) => write!(f, "invalid dog configuration: {err}"),
        }
    }
}

impl core::error::Error for DaemonRunError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Config(err) => Some(err),
            Self::Boot(err) | Self::Run(err) => Some(err),
            Self::DogMigration(err) => Some(err),
        }
    }
}

impl From<DaemonConfigError> for DaemonRunError {
    fn from(source: DaemonConfigError) -> Self {
        Self::Config(source)
    }
}

// `Self::Boot` and `Self::Run` both wrap `BootError`, so only one of them
// could ever claim `impl From<BootError> for DaemonRunError`, and this
// enum's own doc says they must stay distinct (a `BootError` from `run()`
// means the supervisor came up and served, which "failed to boot" would
// misreport). Both stay explicit `map_err` calls; see this task's own
// report.

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
/// Separate from [`run_daemon`] because `commands::foreground` needs the
/// booted daemon in hand rather than a call that blocks until shutdown: it
/// spawns `run()` as a task and then talks to the same supervisor over its
/// own socket, like any other client. Nothing about the boot differs
/// between the two callers, and the split is what keeps that true -- which
/// is why the dog-config migration is at the top of THIS function and not
/// in [`run_daemon`]. It sat there for a release, and `shep runtime` spent
/// it starting dogs against a `dogs.toml` nothing had written.
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
/// - [`DaemonRunError::DogMigration`]: `[dog.<name>]` sections could not
///   be moved out of `shep.toml` and into `dogs.toml`.
pub async fn boot_supervisor(
    paths: ShepPaths,
    args: &DaemonArgs,
    delete_flock_on_shutdown: bool,
) -> Result<RunningDaemon, DaemonRunError> {
    // Here rather than in `run_daemon`, because `run_daemon` is not the only
    // caller that boots a supervisor and serves dogs. `commands::foreground`
    // reaches this function directly for `shep runtime` and `shep dev`, and
    // `boot_options` below builds their dogs out of `enabled_dogs` exactly as
    // it does for the daemon -- so a `shep runtime` used to start a dog and
    // then hand it a `dogs.toml` that would never exist. Measured: with
    // `[dog.metrics] bind = "127.0.0.1:19616"` in shep.toml, nothing listened
    // on 19616 and the compiled default 9615 served instead, with no warning
    // and no file written. Permanent, since a container that only ever runs
    // `shep runtime` never migrates. For bark it means every sink disappears
    // and alerting stops silently.
    //
    // Before the config load and before the supervisor: `dog_section` reads
    // the new file from the first request onward and a dog can connect as
    // soon as the socket is up. A boot after the first finds nothing under
    // `[dog]` and returns before it opens either file.
    //
    // This runs in more than one place on purpose. `reload_with_wait` runs
    // the same call in the CLI process before it signals anything, so a
    // handover successor is not the process that discovers a refusal (see
    // that pre-flight's own comment). Both are safe because the migration is
    // idempotent and takes both files' locks itself, in this crate's one
    // order: shep.toml outer, dogs.toml inner. Two of them at once serialise
    // rather than race, and the loser finds nothing left to move.
    let moved =
        dog_migration::migrate_dog_sections(&paths).map_err(DaemonRunError::DogMigration)?;
    if !moved.is_empty() {
        // Named individually: an operator who did not know this was coming
        // needs to be able to find where their config went.
        //
        // `eprintln!` rather than `tracing::info!`, and only because of
        // where this sits: `install_log_subscriber` runs a few lines below,
        // so at this point in the boot there is no subscriber and a
        // `tracing` record would be dropped on the floor. Stderr is the same
        // destination that subscriber writes to (see `launch::launch_daemon`,
        // which is what redirects it into the shepherd's own log), so the
        // line lands where the rest of the boot's diagnostics do, one
        // migration only, without a format the operator has to go looking
        // for. `shep runtime` and `shep dev` are already streaming their
        // flock's bleats to this same stderr, so it reaches them too.
        eprintln!(
            "shep: moved dog config out of shep.toml and into dogs.toml: {}",
            moved.join(", ")
        );
    }
    let env = |key: &str| std::env::var(key).ok();
    let file_source = read_daemon_config_source(&paths)?;
    let overrides = daemon_overrides(args);
    let config = DaemonConfig::load_layered(file_source.as_deref(), &env, &overrides)?;
    install_log_subscriber(&config);
    // The one read of this variable in the workspace, here beside every
    // `SHEP_*` override rather than inside shep-daemon — see
    // `shep_daemon::notify::NOTIFY_SOCKET_ENV`'s own doc.
    // Unix only: `$NOTIFY_SOCKET` is systemd's readiness protocol over a
    // unix datagram socket, and Windows has nothing for it to address. The
    // field stays on `BootOptions` for both — see `shep_daemon::boot`'s own
    // note on why the config type's SHAPE should not change per platform.
    #[cfg(unix)]
    let notify_socket = std::env::var_os(NOTIFY_SOCKET_ENV);
    #[cfg(windows)]
    let notify_socket: Option<std::ffi::OsString> = None;
    let mut options = boot_options(&config, args, notify_socket.as_deref());
    options.delete_flock_on_shutdown = delete_flock_on_shutdown;
    let daemon = boot(TokioRunner::new(), paths, options)
        .await
        .map_err(DaemonRunError::Boot)?;
    // The one publisher of `config.dog.<name>` in the workspace, and it is
    // here because this is where the bus is: the migration above ran before
    // `boot`, in a process that had none yet, so the announcement waits for
    // the daemon it will travel through. A dog that attaches after this
    // frame has gone out has not missed anything -- it asks for its section
    // at startup, and the section it gets is the moved one.
    //
    // The other two writers of `dogs.toml` deliberately say nothing, each
    // for its own reason, and both carry that reason at their own call site:
    // `reload_with_wait`'s pre-flight below, and `forget_dog_section` in
    // `commands::dogs`. Neither is fixable by routing a CLI write through a
    // request variant, and neither wants to be.
    daemon.context().announce_dog_config(&moved);
    Ok(daemon)
}

/// Runs the supervisor in this process until a signal or `KillDaemon`.
///
/// [`boot_supervisor`] does everything up to and including the boot; this
/// adds only `.run()` on top of it. See that function's own doc for the
/// config-loading, dog-migration and log-subscriber detail this used to
/// carry directly.
///
/// # Errors
/// - [`DaemonRunError::Config`] — `shep.toml` failed to parse, or a
///   `SHEP_*` override held an unparseable value.
/// - [`DaemonRunError::Boot`] — the config file itself could not be read
///   (any IO error other than "does not exist"), or the supervisor failed
///   to boot.
/// - [`DaemonRunError::Run`] — the supervisor came up and served, then
///   failed during its run loop or teardown.
/// - [`DaemonRunError::DogMigration`]: `[dog.<name>]` sections could not be
///   moved out of `shep.toml` and into `dogs.toml`.
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
        // Every dog that EXISTS, which is not the list above: that one is
        // the spawn order and holds only what an operator switched on.
        // `Request::SetDogConfig` is guarded on this one, because the dog
        // most in need of configuring is the one that is disabled or has
        // never started. The same two sources `fail_enable_unknown_dog`
        // calls valid names, plus `enabled_dogs` itself, so a name a
        // hand-edited `shep.toml` enables without adopting is still a dog
        // this shepherd tries to spawn and still one it may hold a section
        // for.
        known_dogs: crate::dog::BUILT_IN_DOGS
            .iter()
            .map(|built_in| (*built_in).to_string())
            .chain(config.daemon.adopted_dogs.keys().cloned())
            .chain(config.daemon.enabled_dogs.iter().cloned())
            .collect(),
        // Overwritten by `boot_supervisor`, the only caller that ever wants
        // `true` — this function has no `tidy_up`/`dev` concept of its own
        // to read it from.
        delete_flock_on_shutdown: false,
        // This process IS the shep binary, which is the assertion that
        // field is about (see its own doc). Both callers of this function
        // are that binary, so the opt-in belongs here rather than in each
        // of them: `run_daemon` is the autostarted shepherd and
        // `commands::foreground::run` is `shep runtime`/`shep dev`, and an
        // init system's SIGHUP should reach the same handover an operator's
        // `shep daemon reload` does.
        handover: true,
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
        // With `Config`, not with `Boot`. Every refusal this variant
        // carries is a shep.toml or dogs.toml an operator has to edit
        // before the daemon will come up (a name in both files, an entry
        // under `[dog]` that is not a section, a dogs.toml that will not
        // parse), which is the same fault `InvalidConfig` already names.
        // Its two I/O arms are the imprecise half of that: a `dogs.toml`
        // that cannot be written is a disk fault rather than a bad value,
        // and it exits `InvalidConfig` all the same. One code per variant
        // is what keeps the mapping readable, and the message on stderr is
        // what tells the two apart.
        DaemonRunError::DogMigration(_) => ExitCode::InvalidConfig,
    }
}

/// Which mechanism a reload uses to give the flock a shepherd running this
/// binary's code.
///
/// [`Self::StopAndStart`] is not scaffolding to be deleted when the
/// handover lands. It is the permanent answer to three cases the handover
/// cannot serve (spec H5): Windows, which has no `exec`; any shepherd
/// predating the handover, which cannot be taught it after the fact; and a
/// handover that fails to rehydrate, where the operator needs a reload that
/// works rather than a wedged one.
#[derive(Debug, PartialEq, Eq)]
enum Arm {
    /// Phase 2's `execve` handover: the shepherd replaces its own image and
    /// the flock never stops.
    ///
    /// Unix only, and the whole handover is: Windows has no `execve`, so
    /// there is no image for a successor to become and
    /// [`Self::for_daemon`] never returns this there.
    #[cfg(unix)]
    Handover,
    /// Stop the shepherd the way `kill` does, wait out its teardown, start a
    /// successor, and muster the roll back.
    StopAndStart,
}

/// The first shep release whose shepherd can hand its flock to a successor.
///
/// Read only by the unix arm of [`Arm::for_daemon`] and by this module's own
/// tests, so a non-test Windows build has no caller at all;
/// `#[cfg_attr(windows, allow(dead_code))]` says so explicitly rather than
/// inventing a call site.
///
/// The handover has to exist in the OLD shepherd (spec H6): a shepherd
/// already running cannot be taught it after the fact, so this is a floor on
/// the version the CLI finds running, never on its own. `0.1.17` is the last
/// release that shipped without it, which makes the next possible release
/// the floor. Early rather than late: a version below this genuinely cannot
/// carry a flock, while a version above it that somehow cannot simply
/// refuses the fitness query and falls back.
///
/// This is the whole of what keeps `PROTOCOL_VERSION` where it is.
/// `Request::HandoverFitness` is a variant a shepherd below the floor has
/// never seen, and one asked it would end the connection on a decode error
/// rather than answering. The floor is why none is ever asked.
#[cfg_attr(windows, allow(dead_code))]
const HANDOVER_SINCE: &str = "0.1.18";

/// `major.minor.patch` as three numbers, or `None` for anything this cannot
/// read.
///
/// Hand-rolled rather than a semver dependency, because the whole question
/// is "is this three-number version at least that one" and the answer is a
/// tuple comparison. Any pre-release or build suffix is dropped with the
/// patch component it hangs off, so `0.1.18-rc.1` reads as `0.1.18`: a
/// pre-release of the version that carries the handover carries it too.
///
/// `None` for a version with fewer than three components, a non-numeric one,
/// or anything else unrecognised, and every caller treats `None` as the safe
/// arm.
#[cfg_attr(windows, allow(dead_code))]
fn version_parts(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?;
    let patch = patch
        .split_once(['-', '+'])
        .map_or(patch, |(number, _suffix)| number);
    Some((major, minor, patch.parse().ok()?))
}

impl Arm {
    /// The arm that reloads a shepherd reporting `daemon_version`, or `None`
    /// where the CLI could not learn it.
    ///
    /// [`Self::Handover`] needs the shepherd being replaced to carry the
    /// mechanism, so this compares its version against [`HANDOVER_SINCE`]
    /// (spec H6). It says nothing about whether that shepherd's particular
    /// FLOCK can be carried, which is a question only the shepherd can
    /// answer and `reload` asks it over the socket.
    ///
    /// A shepherd reporting this binary's OWN version is treated as
    /// carrying the handover too, whatever the floor says, and that matters
    /// for exactly one window: the branch that builds the handover is
    /// itself below the floor until the release carrying it exists, and
    /// without this its own end-to-end tests could never reach the arm they
    /// are there to prove. From the first release at or past
    /// [`HANDOVER_SINCE`] the two conditions coincide and this one decides
    /// nothing. The exposure while it does not is a development build of
    /// `0.1.17` meeting an INSTALLED `0.1.17`, which is asked a query it
    /// cannot parse, ends the connection, and is stopped and started, which is
    /// the same outcome the floor alone would have reached by a longer road.
    ///
    /// **Unknown always means the safe arm, never the fast one.** `None`
    /// arrives from a shepherd whose refusal named no version — one built
    /// before the refusal carried the field, which no upgrade can reach
    /// backwards to fix — and from a home where nothing answered at all.
    /// Guessing at either would hand a flock to a mechanism the shepherd
    /// holding it does not have. A version string this CLI cannot parse is
    /// unknown on the same terms.
    fn for_daemon(daemon_version: Option<&str>) -> Self {
        #[cfg(unix)]
        {
            let Some(running) = daemon_version.and_then(version_parts) else {
                return Self::StopAndStart;
            };
            let floor = version_parts(HANDOVER_SINCE)
                .expect("HANDOVER_SINCE is a literal three-number version");
            let own = version_parts(env!("CARGO_PKG_VERSION"))
                .expect("this crate's own version is a three-number version");
            if running >= floor || running == own {
                Self::Handover
            } else {
                Self::StopAndStart
            }
        }
        // No `execve`, so no handover, whatever the shepherd's version says.
        #[cfg(windows)]
        {
            let _ = daemon_version;
            Self::StopAndStart
        }
    }
}

/// The running shepherd's own crate version, as far as a failed connect can
/// report it.
///
/// Only a protocol refusal names one: every other connect failure happened
/// before the shepherd said who it is. `None` therefore means unknown, which
/// is not the same as absent and is treated identically by
/// [`Arm::for_daemon`].
fn version_from_refusal(err: &ConnectError) -> Option<&str> {
    match err {
        ConnectError::ProtocolMismatch { daemon_version, .. } => daemon_version.as_deref(),
        _ => None,
    }
}

/// Replaces the running shepherd with one running this binary's code, and
/// brings the flock back.
///
/// The verb a version-skew refusal names, and one of the three recovery
/// verbs the guard exempts — so it must work against a shepherd that
/// refuses the handshake, which is why it never needs the socket to succeed.
///
/// `guard` is threaded from `crate::run` rather than named here, so the
/// exemption stays decided in one place.
pub async fn reload(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: crate::VersionGuard,
) -> ExitCode {
    reload_with_wait(streams, paths, guard, admin::KILL_TEARDOWN_WAIT).await
}

/// As [`reload`], but with a caller-chosen teardown wait — the same
/// injectable-timing shape `commands::admin`'s `kill_with_wait` carries, and
/// for the same reason.
async fn reload_with_wait(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: crate::VersionGuard,
    wait: std::time::Duration,
) -> ExitCode {
    // Before the connection, and before either arm below. The handover arm
    // execs a successor that re-reads this same file through
    // `boot_supervisor`; a value that fails to load there exits the
    // successor with the predecessor already gone, leaving the flock
    // running with nothing supervising it. `toml_edit` already keeps
    // `ShepToml::edit` from writing a file that will not PARSE, so the gap
    // this closes is a valid TOML, invalid VALUE file. `whistle` runs the
    // same check for the same reason (`whistle::mod::whistle`).
    //
    // FILE ONLY, deliberately, with `&|_| None` in place of an env layer:
    // the daemon being reloaded is already running, so its own env and its
    // own boot-time flags already loaded successfully once and have not
    // changed since. Only the file changed, so the file is the only input
    // worth checking here. Layering THIS process's env would be worse than
    // skipping the check: the handover successor inherits the OLD daemon's
    // argv and env through `execve`, not this CLI invocation's, so a value
    // this process's shell happens to rescue (an exported `SHEP_LOG_LEVEL`
    // the daemon never saw) would pass here and still exit the successor
    // with the predecessor gone. A file-only check cannot pass where the
    // successor fails, because a valid file plus an already-valid env and
    // flags stays valid; it can refuse a file the successor would have
    // survived on a boot-time flag, which is accepted on purpose, since a
    // config value that only works because of a flag someone passed at
    // boot is a trap worth naming.
    if let Err(err) = read_daemon_config_source(paths).and_then(|source| {
        DaemonConfig::load(source.as_deref(), &|_| None).map_err(DaemonRunError::from)
    }) {
        return streams.fail(daemon_exit_code(&err), &err.to_string());
    }

    // The same argument as the config check above, aimed at the other file
    // the successor reads. The migration runs at the top of every boot
    // (`boot_supervisor`), so on the handover arm it runs in a successor
    // whose predecessor is already gone: a refusal there exits the
    // successor and leaves the flock running with nothing supervising it.
    // Reproduced before this line existed, with a dog named in both files:
    // `shep daemon reload` failed, the sheep survived reparented to init,
    // `shep flock` reported it stopped, and a recovering `shep muster`
    // started a second copy alongside the orphan. `shep daemon reload` is
    // the documented upgrade command, so for most operators the migration's
    // first run happens inside a handover.
    //
    // RUN, not dry-run, and that is what makes this a pre-flight rather
    // than a second opinion. The migration is idempotent -- a boot after
    // the first finds nothing under `[dog]` and returns before it opens
    // either file -- so doing the work here leaves the successor's own call
    // with nothing to do, and there is no window in which a file changes
    // between a check and the act it was checking. It takes both files'
    // locks itself, in this crate's one order (`shep.toml` outer,
    // `dogs.toml` inner), and this process holds neither, so the ordering
    // is unchanged.
    //
    // This is the CLI process, and nothing below has signalled the
    // predecessor yet, so a refusal here ends the verb with the running
    // shepherd untouched -- which is the whole point.
    //
    // NO `config.dog.<name>` frame goes out from here, unlike the same
    // call in `boot_supervisor` above, and that is the deliberate half.
    // This is an operator's own short-lived process: it has no bus, and
    // the daemon's bus is not something a client may publish onto. Adding
    // a request variant so a CLI write could reach it would put an
    // announcement anything holding the socket can forge next to one the
    // daemon makes about work it did itself. Nothing is lost by the
    // silence: this pre-flight is followed immediately by the handover it
    // is clearing the way for, and the migration relocates values without
    // changing any of them. A dog that re-reads its section against the
    // successor finds the same settings in the new file, and a dog that
    // never re-reads is already holding them. Both exist: bark's event
    // source ends on a handover, so it exits and comes back up reading
    // `dogs.toml`, while metrics redials underneath a `ReconnectingClient`,
    // keeps its pid and its `restarts 0`, and reads no config again.
    match dog_migration::migrate_dog_sections(paths) {
        Ok(moved) if moved.is_empty() => {}
        Ok(moved) => {
            // In front of the person who typed the verb, not in the
            // daemon's log. Before this ran here the successor printed it
            // to the shepherd's own stderr, where an operator who did not
            // know the move was coming had no reason to look.
            streams.aside(
                "reload",
                &format!(
                    "moved dog config out of shep.toml and into dogs.toml: {}",
                    moved.join(", ")
                ),
            );
        }
        Err(err) => {
            let err = DaemonRunError::DogMigration(err);
            return streams.fail(daemon_exit_code(&err), &err.to_string());
        }
    }

    // Connected to ask who is there, and, on the handover arm, whether
    // this flock can be carried. Dropped before anything is signalled: this
    // connection is to the process about to be replaced.
    let connected = match Client::connect(&paths.socket).await {
        Ok(client) => Ok(client),
        Err(err) => Err(version_from_refusal(&err).map(str::to_owned)),
    };
    // `cfg(unix)`, like its only reader below. Windows has no `execve`, so
    // the arm is never in question there and the binding would be an
    // `unused_variables` warning on live code rather than a value nothing
    // happens to read yet.
    #[cfg(unix)]
    let running_version = match &connected {
        Ok(client) => Some(client.daemon().daemon_version.clone()),
        Err(from_refusal) => from_refusal.clone(),
    };
    #[cfg(unix)]
    if Arm::for_daemon(running_version.as_deref()) == Arm::Handover
        // A version learned from a protocol refusal cannot reach the
        // handover arm, and spec H3a is explicit about why: the decision
        // needs a fitness answer, a refused handshake has no connection to
        // ask over, and a stop-and-start is the right arm for a shepherd the
        // socket will not talk to anyway.
        && let Ok(client) = &connected
    {
        match ask_fitness(client).await {
            Fitness::Carryable => {
                // Before the signal, never after: the shepherd is about to
                // replace its own image, and a connection held across that
                // is a connection to a process that no longer exists.
                drop(connected);
                return hand_over(streams, paths, guard, wait).await;
            }
            // Not a failure. The flock carries something this shepherd
            // cannot move in place, so the reload happens the other way and
            // the operator is told which sheep and why (spec H3a), in front
            // of the person who typed the verb, not in the daemon's log.
            Fitness::Refused(reason) => {
                streams.aside("reload", &reason);
            }
        }
    }
    // Before the stop arm too, and for a sharper reason than the handover's.
    // `stop_and_start` signals the shepherd and then waits for its control
    // address to stop answering. On Windows that address is a named pipe and
    // the wait probes it, treating `ERROR_PIPE_BUSY` as a daemon that has not
    // finished; a client handle held open here keeps the pipe instance alive
    // past the daemon's exit, so the wait can run out its whole budget and
    // the reload fails `DeadlineExceeded` without ever starting a successor.
    // `admin::kill_with_wait` drops its own client before the same wait for
    // the same reason.
    drop(connected);
    stop_and_start(streams, paths, guard, wait).await
}

/// What a shepherd says about carrying its own flock.
///
/// The daemon owns both the gate and the wording; this is only the two
/// answers the CLI acts on. The refusal reaches the operator verbatim.
#[cfg(unix)]
#[derive(Debug)]
enum Fitness {
    /// Every sheep can be carried across the exec.
    Carryable,
    /// At least one cannot, and this sentence says which and why.
    Refused(String),
}

/// Asks `client`'s shepherd whether its flock can be handed over in place.
///
/// **Every failure is a refusal**, and none of them is reported as an error.
/// A shepherd that answers the wrong reply, refuses the request, or drops
/// the connection has told this CLI nothing it can act on, and the stop arm
/// works against all three. The alternative, signalling on a maybe, is the
/// one outcome spec H3a exists to prevent, since a signal carries no reply
/// and a shepherd that refuses after being signalled leaves the flock down.
#[cfg(unix)]
async fn ask_fitness(client: &Client) -> Fitness {
    match client
        .request(shep_core::protocol::Request::HandoverFitness)
        .await
    {
        Ok(shep_core::protocol::Response::HandoverFitness { refusal: None }) => Fitness::Carryable,
        Ok(shep_core::protocol::Response::HandoverFitness {
            refusal: Some(reason),
        }) => Fitness::Refused(reason),
        Ok(other) => Fitness::Refused(format!(
            "this shepherd answered a handover question with {other:?}, so its flock is being \
             stopped and started instead"
        )),
        Err(err) => Fitness::Refused(format!(
            "this shepherd could not say whether its flock can be handed over ({err}), so it is \
             being stopped and started instead"
        )),
    }
}

/// Signals the shepherd to replace its own image in place, then waits for
/// the successor to serve and reports the flock.
///
/// The flock never stops here. A carried sheep keeps its pid, its log file
/// handles and its place in the shepherd's registry, so the report at the
/// end names the same processes that were running before the verb was typed.
///
/// The signal is SIGHUP, and the shepherd's own handler is what decides what
/// to do with it. That is the trigger for the reason spec H3 gives: the case
/// that most needs a reload is the one where the shepherd refuses this
/// client at the handshake, and a remedy delivered over the channel it is
/// meant to repair is not a remedy. The DECISION already happened over the
/// socket, before this was called.
///
/// **The wait is what makes this recoverable.** A handover can still fail
/// after the signal (a binary that will not exec, a successor that cannot
/// rehydrate) and a shepherd that meets one falls back to its own graceful
/// stop. So this waits for a shepherd of THIS binary's version to answer,
/// and takes the stop arm's own tail when none does: connect or spawn, then
/// muster. An operator gets a working reload either way.
#[cfg(unix)]
async fn hand_over(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: crate::VersionGuard,
    wait: std::time::Duration,
) -> ExitCode {
    let pid = match proven_shepherd(streams, paths) {
        Ok(pid) => pid,
        Err(code) => return code,
    };
    // Opened BEFORE the signal, and held across it. The predecessor's
    // accepted connections are not carried across its `execve`, so this one
    // closing is the proof that the old image is gone. Without it there is
    // no way to tell a successor from a predecessor that has been signalled
    // and has not execed yet: both answer, on the same pid, at the same
    // version.
    let Ok(witness) = Client::connect(&paths.socket).await else {
        // No witness, no handover. Signalling without one would leave
        // `await_successor` with nothing to outlive, so it would accept the
        // first answer it got, which is the predecessor's, which is the
        // whole hole the witness exists to close.
        //
        // Reaching here at all is anomalous rather than informative: the
        // fitness query a moment ago needed a connection of its own, so a
        // daemon WAS answering. A transient failure here says something is
        // wrong, not that nothing is running, and the stop arm is correct
        // under either reading.
        let message = "could not hold a connection across the handover signal; \
                       stopping and starting instead";
        streams.aside("reload", message);
        return stop_and_start(streams, paths, guard, wait).await;
    };
    if let Err((code, message)) = signal_handover(pid) {
        return streams.fail(code, &message);
    }
    match await_successor(paths, &witness, wait).await {
        // The successor carried the flock; nothing to restore.
        Some(client) => report_reload(&client, streams, false).await,
        None => {
            // Said out loud rather than silently repaired: the flock is
            // about to be started rather than carried, so an operator who
            // was promised unchanged pids needs to know they did not get
            // them.
            let message = "the shepherd did not come back on this version after the handover \
                           signal; starting one instead";
            streams.aside("reload", message);
            let client = match crate::connect_or_spawn_client(streams, paths, guard).await {
                Ok(client) => client,
                Err(code) => return code,
            };
            report_reload(&client, streams, true).await
        }
    }
}

/// Asks the shepherd at `pid` to hand its flock to a successor.
///
/// SIGHUP, because SIGUSR2 is already the log-reopen signal and SIGHUP was
/// otherwise unhandled when the handover was designed. A shepherd too old to
/// hand over installs it as a graceful stop instead, which is why this is
/// never sent to one: the arm selection decides that before this is reached.
///
/// # Errors
/// The exit code and the sentence to report, when the pid is not one this
/// platform can name or the signal itself failed. The caller prints it; this
/// function writes nothing.
#[cfg(unix)]
fn signal_handover(pid: u32) -> Result<(), (ExitCode, String)> {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    let Ok(target) = i32::try_from(pid) else {
        let message = format!("the recorded pid {pid} is not one this platform can signal");
        return Err((ExitCode::Internal, message));
    };
    signal::kill(Pid::from_raw(target), Signal::SIGHUP).map_err(|errno| {
        let message = format!("could not signal the shepherd at pid {pid}: {errno}");
        (ExitCode::Failure, message)
    })
}

/// Polls the control socket until a shepherd running THIS binary's version
/// answers, or `wait` expires.
///
/// A handshake is a stronger confirmation than a reply to the signal would
/// have been: it proves the successor is serving, not merely that the
/// predecessor received something. The version is what tells the two images
/// apart: the predecessor is still answering on the same socket right up
/// until the moment it execs, and the whole point of the verb is that the
/// version moved.
///
/// A connect failure inside the window is expected, not a fault: the
/// successor is mid-boot and has not accepted yet.
#[cfg(unix)]
async fn await_successor(
    paths: &ShepPaths,
    witness: &Client,
    wait: std::time::Duration,
) -> Option<Client> {
    let deadline = tokio::time::Instant::now() + wait;

    // Stage one: wait out the predecessor. A request answered on `witness`
    // says the OLD image is still serving, because that connection cannot
    // survive its exec. Only once it stops answering is a fresh connection
    // worth trusting.
    //
    // Never skipped. A caller without a witness must not reach here at all,
    // because a handover with nothing to outlive accepts the predecessor's
    // own answer as the successor's.
    while witness
        .request(shep_core::protocol::Request::ListFlock)
        .await
        .is_ok()
    {
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(SUCCESSOR_POLL_INTERVAL).await;
    }

    // Stage two: the old image is gone. Now an answered request can only
    // have come from the successor.
    loop {
        // A handshake proves a daemon answered. It does NOT prove the
        // answer came from the successor, and the two are genuinely
        // indistinguishable here: `execve` keeps the pid, and the arm that
        // reaches this code can be selected against a shepherd of this very
        // version, so neither the pid nor the version in the ack separates
        // them. Connecting to the PREDECESSOR is therefore normal, not a
        // race to be narrowed, and its connection dies at the exec.
        //
        // So the readiness test is a REQUEST, not a handshake: an answered
        // request can only have come from a daemon that is serving, which
        // the outgoing image stops doing the moment it execs. Four Linux CI
        // jobs found this by way of `daemon reload` reporting a successful
        // handover and then exiting 5 on the very next call.
        if let Ok(client) = Client::connect(&paths.socket).await
            && client.daemon().daemon_version == env!("CARGO_PKG_VERSION")
            && client
                .request(shep_core::protocol::Request::ListFlock)
                .await
                .is_ok()
        {
            return Some(client);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(SUCCESSOR_POLL_INTERVAL).await;
    }
}

/// Gap between [`await_successor`]'s probes. Short, because an `execve` plus
/// a rehydrate is milliseconds of work and the operator is waiting.
#[cfg(unix)]
const SUCCESSOR_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// The pid of the shepherd owning this home, or the code and sentence
/// saying why there is none to act on.
///
/// The pidfile alone is not proof, since a stale one from a crash still
/// exists and the pid may have been reused, so this reads the lock the daemon
/// holds for its whole life instead. Shared by both arms: they act on the
/// same shepherd, with the same three ways of not finding one.
fn proven_shepherd(streams: &mut Streams<'_>, paths: &ShepPaths) -> Result<u32, ExitCode> {
    match boot::daemon_liveness(paths) {
        Ok(Shepherd::Running(pid)) => Ok(pid),
        // Alive and owns the home, but has not recorded a pid yet. Not an
        // absence, and not a pid to guess at.
        Ok(Shepherd::Booting) => {
            let message = "a shepherd is starting up and has not recorded its pid yet; try again";
            Err(streams.fail(ExitCode::DaemonUnreachable, message))
        }
        // Nothing to replace. Says what starts one instead, rather than
        // starting it unasked: `reload` is how an operator makes a RUNNING
        // system match the binary, and a home with no shepherd has no
        // running system to make match.
        Ok(Shepherd::Absent) => {
            let message = format!(
                "no shepherd is running, so there is nothing to reload (nothing holds the lock \
                 on `{}`). `shep muster` brings the flock up from the roll",
                boot::pidfile(paths).display()
            );
            Err(streams.fail(ExitCode::DaemonUnreachable, &message))
        }
        Err(err) => Err(streams.fail(ExitCode::Failure, &err.to_string())),
    }
}

/// Stops the shepherd, waits it out, starts a successor, and musters.
///
/// Four steps, no new mechanism in any of them: the pidfile lock proves the
/// pid ([`boot::daemon_liveness`]), `commands::admin` owns the
/// signal and the teardown wait, and `crate::connect_or_spawn_client` is the
/// same autostart `shep start` and `shep muster` already use.
async fn stop_and_start(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: crate::VersionGuard,
    wait: std::time::Duration,
) -> ExitCode {
    let pid = match proven_shepherd(streams, paths) {
        Ok(pid) => pid,
        Err(code) => return code,
    };
    if let Err((code, message)) = admin::signal_graceful_stop(pid) {
        return streams.fail(code, &message);
    }
    if !admin::wait_for_socket_to_disappear(&paths.socket, wait).await {
        let message = "the shepherd was signalled, but teardown is still in progress; \
                       nothing has been started in its place";
        return streams.fail(ExitCode::DeadlineExceeded, message);
    }
    let client = match crate::connect_or_spawn_client(streams, paths, guard).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    report_reload(&client, streams, true).await
}

/// Reports the shepherd now serving, then what happened to each sheep.
///
/// The per-sheep table is the whole report on purpose (spec H4): the
/// handover arm does not stop the flock, so nothing here may announce that
/// it did, and nothing may assume a reload gives a sheep a new pid. The one
/// line above the table names the SHEPHERD's version and pid, which is true
/// under both arms and is the fact the operator ran this verb for.
///
/// `restored` says which arm ran, and it decides how the flock is asked for.
///
/// Under the STOP arm the successor has already restored the roll by the
/// time this runs, so `Request::Muster` spawns nothing new and reports the
/// flock that restore produced. See `commands::muster::muster`'s own doc on
/// why that is idempotent.
///
/// Under the HANDOVER arm it must NOT muster, for two reasons that arrived
/// together. Semantically there is nothing to restore: the sheep never
/// stopped, so a muster is being asked to bring back processes that are
/// already running, off a roll the predecessor never had a chance to
/// rewrite. And in practice it fails, which is how this was found. Four
/// Linux CI jobs caught `shep daemon reload` reporting a successful handover
/// and then exiting 5:
///
/// ```text
/// notice[reload]: the shepherd is now 0.1.17 (pid 10579)
/// error[daemon_unreachable]: the connection closed before a reply arrived
/// ```
///
/// A successor answers its socket as soon as the listener is carried, which
/// is before its rehydrate has finished, so a muster arriving in that window
/// meets a daemon not yet ready to serve one. A plain `ListFlock` is what
/// the handover arm actually wants: it reports, and asks for nothing.
async fn report_reload(client: &Client, streams: &mut Streams<'_>, restored: bool) -> ExitCode {
    report_reload_waiting(client, streams, restored, DOG_SETTLE_WAIT).await
}

/// As [`report_reload`], but with a caller-chosen dog wait — the same
/// injectable-timing shape [`reload_with_wait`] carries, and for the same
/// reason.
async fn report_reload_waiting(
    client: &Client,
    streams: &mut Streams<'_>,
    restored: bool,
    dog_wait: std::time::Duration,
) -> ExitCode {
    let shepherd = client.daemon();
    let message = format!(
        "the shepherd is now {} (pid {})",
        shepherd.daemon_version, shepherd.pid
    );
    streams.aside("reload", &message);
    report_dog_staleness(client, streams, &shepherd.daemon_version, dog_wait).await;
    if restored {
        return muster::muster(client, streams).await;
    }
    crate::commands::query::flock(client, streams).await
}

/// How long a reload waits for the flock's dogs to finish reconnecting
/// before it reports what came back.
///
/// Sized against the round trip it has to outlast, which is not the
/// reconnect: a dog refused on the handshake is restarted once from the
/// binary on disk (G8), and only the SECOND refusal — after a full kill
/// ladder and a fresh spawn — is what makes it stale. A budget shorter
/// than that would report every stale dog as healthy, which is the
/// failure this whole reading exists to avoid.
///
/// Only ever paid when a dog has not answered. An ordinary reload finds
/// nothing pending on its first ask and spends none of this.
const DOG_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Gap between [`report_dog_staleness`]'s asks. Coarser than
/// [`SUCCESSOR_POLL_INTERVAL`], because what this waits on is a process
/// being killed and respawned rather than an `execve`.
const DOG_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Reports the dogs that could not come back, once the shepherd has heard
/// from all of them.
///
/// **The waiting is the point** (spec G13). A dog's recorded crate version
/// describes the process that was running when it connected, so before a
/// reload it is evidence about something about to stop existing, and a
/// report taken then is a claim rather than a finding. Afterwards the same
/// question has a real answer — this shepherd either accepted the dog's
/// handshake or refused it — and asking early would get the answer for the
/// wrong daemon. So this asks, waits while anything is unsettled, and asks
/// again.
///
/// **Silent unless something is wrong.** A flock whose dogs all came back
/// prints nothing at all: the sentence an operator wants after a reload is
/// the shepherd's version and their own flock, and a line-per-reload
/// saying the dogs are fine would bury both.
///
/// **What it does not say.** That a dog is stale is a fact about two
/// refused handshakes, not about the binary on disk — shep never reads
/// that file's version, and cannot, until a dog answers `--version` (G11,
/// a later phase). So the sentence reports what happened and suggests a
/// remedy; it never promises the remedy will work.
///
/// A shepherd that will not answer is left alone rather than reported. The
/// reload itself has already succeeded by the time this runs, and a dog
/// reading that could not be taken is not a reason to say anything about
/// the dogs.
async fn report_dog_staleness(
    client: &Client,
    streams: &mut Streams<'_>,
    daemon_version: &str,
    wait: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let Ok(shep_core::protocol::Response::DogStaleness { stale, pending }) = client
            .request(shep_core::protocol::Request::DogStaleness)
            .await
        else {
            return;
        };
        let out_of_time = tokio::time::Instant::now() >= deadline;
        if pending.is_empty() || out_of_time {
            if !stale.is_empty() {
                streams.aside("reload", &stale_dog_report(&stale, daemon_version));
            }
            if out_of_time && !pending.is_empty() {
                streams.aside("reload", &unsettled_dog_report(&pending, wait));
            }
            return;
        }
        tokio::time::sleep(DOG_POLL_INTERVAL).await;
    }
}

/// The sentence naming the dogs this shepherd has given up on.
///
/// Two whole sentences rather than one with the number interpolated
/// through it: the singular and the plural differ in four places, and a
/// reader checking the wording should not have to run the ternaries in
/// their head to see what either one says.
///
/// **It no longer prescribes the reinstall, and that is the fix rather than
/// a softening.** This sentence used to end *rebuild or reinstall it against
/// shep X, then restart it*, on the reasoning that naming only the reinstall
/// left an operator one step short — `cargo install` alone leaves the old
/// code running, because a rename leaves the running inode mapped (G10). The
/// two-command remedy is still right where a rebuild is the answer. The
/// error was asserting that it always is.
///
/// [`Request::DogStaleness`](shep_core::protocol::Request::DogStaleness)
/// answers with NAMES. The population it names is
/// `shep_daemon::dogs::DogRefusals::stale`, which holds two kinds of dog: one
/// whose handshake this shepherd watched be refused twice, where a rebuild
/// genuinely is the remedy, and one that simply never spoke, where it may not
/// be. A dog reaching this shepherd on every connection and never naming
/// itself in the `Hello` is in the second set, is doing its job perfectly,
/// and is not fixed by reinstalling the same build — an operator given this
/// sentence about exactly that dog spent two days reinstalling it.
///
/// So this reports what happened and sends the reader to the one place that
/// knows which case they have. `shep_daemon::dogs::stale_verdict` writes its
/// finding into the dog's own log at the moment of the give-up, from peer
/// credentials only the shepherd could read and only then; nothing reaches
/// this function but a name. Restating a fork this side of the wire cannot
/// resolve would be a second, worse account of a state the log already
/// describes exactly.
///
/// The daemon's version is still named, as a fact rather than an
/// instruction: it is what a rebuild would have to target, and the dog's log
/// is where the reader learns whether a rebuild is what they need.
fn stale_dog_report(stale: &[String], daemon_version: &str) -> String {
    match stale {
        [only] => format!(
            "the `{only}` dog cannot talk to this shepherd; restarting it from the binary on \
             disk did not help, so shep has given up and will not restart it again. \
             `shep bleats {only}` holds what shep saw when it gave up, and the fix follows from \
             that -- reinstalling the same build is not always it. This shepherd is shep \
             {daemon_version}, if a rebuild is what that log calls for"
        ),
        many => format!(
            "these dogs cannot talk to this shepherd: {}; restarting them from the binaries on \
             disk did not help, so shep has given up and will not restart them again. \
             `shep bleats <dog>` holds what shep saw when it gave up on each, and the fix \
             follows from that -- reinstalling the same build is not always it. This shepherd \
             is shep {daemon_version}, if a rebuild is what those logs call for",
            quoted_names(many)
        ),
    }
}

/// The sentence for dogs that had not answered within the reload's settle
/// wait.
///
/// Said out loud rather than folded into silence, because silence here
/// would mean the same thing as a clean reload and this is not one: the
/// reading was taken before these dogs had answered, so it speaks for the
/// rest of the flock and not for them.
///
/// **This population is not who it was expected to be.** G8's ladder
/// (`shep_daemon::dogs::DogRefusals`) restarts a silent dog at
/// [`shep_daemon::dogs::DOG_SILENCE_BUDGET`] and marks it stale five seconds
/// after that, both later than this reload's own wait — so a dog stuck on a
/// protocol it cannot speak lands HERE, not in [`stale_dog_report`], every
/// time. Written for that population rather than for the fast, transient
/// remnant a shorter ladder would have left behind.
///
/// **Names what the shepherd actually knows, not a verdict it does not
/// have.** It cannot yet say whether this dog will come back — that
/// question belongs to [`stale_dog_report`], later, if the restart does not
/// help — but it does know a restart is coming and when, and it knows where
/// the dog's own account of the silence lives. `shep bleats <dog>` is where
/// the answer actually was in production, so this sentence points there
/// instead of shrugging.
fn unsettled_dog_report(pending: &[String], wait: std::time::Duration) -> String {
    let budget = shep_daemon::dogs::DOG_SILENCE_BUDGET;
    match pending {
        [only] => format!(
            "the `{only}` dog has not answered this shepherd after {wait:?}; a dog silent \
             past {budget:?} is restarted once from the binary on disk and then reported \
             stale, and `shep bleats {only}` shows why"
        ),
        many => format!(
            "these dogs have not answered this shepherd after {wait:?}: {}; a dog silent past \
             {budget:?} is restarted once from the binary on disk and then reported stale, and \
             `shep bleats <dog>` shows why for each",
            quoted_names(many)
        ),
    }
}

/// `` `a`, `b`, `c` `` — the list shape both sentences above end on.
fn quoted_names(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VersionGuard;
    use crate::cli::Format;
    use shep_core::config::LogLevel;
    use shep_core::protocol::{Request, Response};

    /// fails if `run_daemon` builds its overrides from the wrong fields, or
    /// drops one. Drives the config assembly, not the boot — booting a real
    /// shepherd is `daemon_e2e`'s job. Asserts all four fields rather than
    /// sampling one, which is what makes the mutation below land on exactly
    /// one assertion.
    #[test]
    fn every_daemon_flag_reaches_the_config() {
        let args = DaemonArgs {
            cmd: None,
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
                cmd: None,
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
                cmd: None,
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

    /// fails if `known_dogs` is the enabled list under another name. The
    /// dog an operator most wants to configure is the one that is adopted
    /// and still switched off, and the daemon guards `SetDogConfig` on
    /// this field, so an assembly that only carried `enabled_dogs` would
    /// refuse a section for exactly that dog. The built-ins are in it for
    /// the same reason: `metrics` is a dog whether or not anyone has
    /// enabled it yet.
    #[test]
    fn boot_options_know_every_dog_that_exists_and_not_only_the_enabled_ones() {
        let src = r#"
[daemon]
enabled_dogs = ["metrics"]

[daemon.adopted_dogs]
otel = "/usr/local/bin/shep-otel"
"#;
        let config = DaemonConfig::load(Some(src), &|_| None).unwrap();
        let opts = boot_options(
            &config,
            &DaemonArgs {
                cmd: None,
                no_restore: false,
                foreground: false,
                log_json: None,
                log_level: None,
                socket: None,
                max_cron_sleep: None,
            },
            None,
        );

        let known: std::collections::BTreeSet<&str> =
            opts.known_dogs.iter().map(String::as_str).collect();
        assert_eq!(
            known,
            ["bark", "metrics", "otel"].into_iter().collect(),
            "adopted-and-disabled is the case this field exists for"
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
                    cmd: None,
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
                    cmd: None,
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
                cmd: None,
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
                cmd: None,
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
                cmd: None,
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
                cmd: None,
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
                cmd: None,
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
                cmd: None,
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
        // A refused dog migration is a file the operator has to edit, so it
        // shares `Config`'s code rather than falling through to `Failure`.
        assert_eq!(
            daemon_exit_code(&DaemonRunError::DogMigration(
                DogMigrationError::WouldOverwrite {
                    name: "metrics".to_string(),
                }
            )),
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

    /// A shepherd that answers cleanly and names an older version still
    /// takes the stop arm in phase 1: the handover has to exist in the
    /// shepherd being replaced (spec H6), and no released shep carries it.
    #[test]
    fn reload_picks_the_stop_arm_against_a_daemon_too_old_to_hand_over() {
        assert_eq!(Arm::for_daemon(Some("0.1.8")), Arm::StopAndStart);
    }

    /// The floor itself, from the other side: the last release that cannot
    /// hand over still takes the stop arm, and the version arithmetic is
    /// component-wise rather than lexical, which `0.1.9` against `0.1.18`
    /// is the case that tells apart.
    #[test]
    fn reload_picks_the_stop_arm_at_every_version_below_the_floor() {
        assert_eq!(Arm::for_daemon(Some("0.1.9")), Arm::StopAndStart);
        assert_eq!(Arm::for_daemon(Some("0.1.16")), Arm::StopAndStart);
        assert_eq!(
            Arm::for_daemon(Some("not a version")),
            Arm::StopAndStart,
            "a version this CLI cannot read is unknown, and unknown is the safe arm"
        );
    }

    /// The one allowance the floor makes, and the reason this branch can
    /// test its own handover at all: a shepherd reporting this binary's own
    /// version is this binary. See [`Arm::for_daemon`]'s own doc for the
    /// window it is open in and what it costs while it is.
    #[cfg(unix)]
    #[test]
    fn a_shepherd_of_this_binarys_own_version_answers_for_itself() {
        assert_eq!(
            Arm::for_daemon(Some(env!("CARGO_PKG_VERSION"))),
            Arm::Handover
        );
    }

    /// A shepherd predating Task 4's field sends no `daemon_version` at
    /// all, and no upgrade can reach backwards to change that. Unknown must
    /// mean the safe arm, never the fast one.
    #[test]
    fn reload_picks_the_stop_arm_when_the_handshake_is_refused_without_a_version() {
        let refusal = ConnectError::ProtocolMismatch {
            client: shep_core::protocol::PROTOCOL_VERSION,
            daemon_version: None,
            message: "this daemon speaks protocol 1".to_string(),
        };
        assert_eq!(version_from_refusal(&refusal), None);
        assert_eq!(
            Arm::for_daemon(version_from_refusal(&refusal)),
            Arm::StopAndStart
        );
    }

    /// The other half: a refusal that DOES name a version must hand it on.
    /// This is the seam Task 4 added the field for, and dropping it here
    /// would leave phase 2 unable to ever pick the handover after a
    /// protocol bump -- the one case it matters most.
    #[test]
    fn a_refusal_that_names_a_version_yields_it_for_the_arm_choice() {
        let refusal = ConnectError::ProtocolMismatch {
            client: shep_core::protocol::PROTOCOL_VERSION,
            daemon_version: Some("0.1.8".to_string()),
            message: "this daemon speaks protocol 1".to_string(),
        };
        assert_eq!(version_from_refusal(&refusal), Some("0.1.8"));
    }

    /// Nothing else in a connect failure names a version: the handshake
    /// never got far enough for the shepherd to say who it is.
    #[test]
    fn a_connect_failure_that_is_not_a_refusal_names_no_version() {
        let err = ConnectError::HandshakeClosed;
        assert_eq!(version_from_refusal(&err), None);
    }

    /// Spec H4: the report is per-sheep, because phase 2's handover does
    /// not stop the flock and the same output shape has to be true under
    /// both arms. Sheep DO stop under phase 1's stop arm -- this pins the
    /// SHAPE, not the mechanism.
    #[tokio::test]
    async fn reload_reports_each_sheep_rather_than_announcing_the_flock_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _envelopes) = shep_client::testing::fake_client_answering(&addr, |_req| {
            Response::Mustered(vec![shep_client::testing::sample_info()])
        })
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            report_reload(&client, &mut streams, true).await
        };

        assert_eq!(code, ExitCode::Success);
        let text = format!(
            "{}{}",
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap()
        );
        assert!(text.contains("web"), "{text}");
        assert!(!text.to_lowercase().contains("flock stopped"), "{text}");
    }

    /// A home no shepherd owns has nothing to reload, and the pidfile lock
    /// is what proves that -- not the socket, which is exactly what a
    /// skewed shepherd refuses over. Drives `reload` end to end, so the
    /// connect, the arm choice and the liveness proof are all in the path.
    #[tokio::test]
    async fn reload_refuses_a_home_no_shepherd_owns() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        std::fs::create_dir_all(&paths.pids).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            reload(&mut streams, &paths, VersionGuard::Exempt).await
        };

        assert_eq!(code, ExitCode::DaemonUnreachable);
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("no shepherd"), "{text}");
    }

    /// Fails if `reload` signals anything on a `shep.toml` that will not
    /// load. The successor execs into a fresh `boot_supervisor`, so a bad
    /// value there exits the daemon AFTER the predecessor is gone, leaving a
    /// running flock with no shepherd. The value below is valid TOML and an
    /// invalid level, exactly the gap `toml_edit`'s own parse check cannot
    /// close.
    ///
    /// No daemon runs in this test, so without the pre-flight `reload`
    /// reaches `Client::connect`, fails, and returns `DaemonUnreachable`
    /// instead of `InvalidConfig`: a connection attempt against a home no
    /// daemon owns, rather than a refusal that never dials at all.
    #[tokio::test]
    async fn reload_refuses_a_shep_toml_that_will_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(paths.daemon_config.parent().unwrap()).unwrap();
        std::fs::write(&paths.daemon_config, "[daemon]\nlog_level = \"verbose\"\n").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            reload(&mut streams, &paths, VersionGuard::Exempt).await
        };

        assert_eq!(code, ExitCode::InvalidConfig);
        let rendered = String::from_utf8(err).unwrap();
        assert!(
            rendered.contains("verbose"),
            "the refusal must name the bad value: {rendered}"
        );
    }

    /// A handshake that names a version at or past [`HANDOVER_SINCE`] is the
    /// only thing that selects the handover.
    #[cfg(unix)]
    #[test]
    fn reload_picks_the_handover_against_a_daemon_new_enough_to_carry_its_flock() {
        assert_eq!(Arm::for_daemon(Some("9.9.9")), Arm::Handover);
        assert_eq!(Arm::for_daemon(Some(HANDOVER_SINCE)), Arm::Handover);
    }

    /// What keeps `PROTOCOL_VERSION` where it is.
    ///
    /// `Request::HandoverFitness` is a variant an older daemon has never
    /// seen, so asking one would be a parse error rather than a refusal it
    /// could answer. The version gate is what makes that unreachable: the
    /// arm is chosen from the handshake, and a daemon predating the handover
    /// takes the stop arm with the query never sent. This drives the whole
    /// verb and reads the wire, because the claim is about what is on it.
    #[tokio::test]
    async fn a_reload_against_an_older_daemon_never_sends_the_query() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        std::fs::create_dir_all(&paths.pids).unwrap();
        let mut sent = shep_client::testing::fake_daemon_answering_with_ack(
            &paths.socket,
            ack_naming("0.1.8"),
            |_| Response::Pong,
        )
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            reload(&mut streams, &paths, VersionGuard::Exempt).await;
        }

        let asked: Vec<Request> = std::iter::from_fn(|| sent.try_recv().ok())
            .map(|envelope| envelope.body)
            .collect();
        assert!(
            !asked
                .iter()
                .any(|req| matches!(req, Request::HandoverFitness)),
            "a daemon that cannot parse the query must never be asked it: {asked:?}"
        );
    }

    /// The other half, and what makes the test above non-vacuous: a daemon
    /// new enough IS asked, exactly once, before anything is signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_reload_against_a_newer_daemon_asks_before_it_signals() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        std::fs::create_dir_all(&paths.pids).unwrap();
        let mut sent = shep_client::testing::fake_daemon_answering_with_ack(
            &paths.socket,
            ack_naming("9.9.9"),
            |_| Response::HandoverFitness { refusal: None },
        )
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            reload(&mut streams, &paths, VersionGuard::Exempt).await;
        }

        let asked: Vec<Request> = std::iter::from_fn(|| sent.try_recv().ok())
            .map(|envelope| envelope.body)
            .collect();
        assert_eq!(
            asked
                .iter()
                .filter(|req| matches!(req, Request::HandoverFitness))
                .count(),
            1,
            "exactly one fitness query, and it is the first thing asked: {asked:?}"
        );
    }

    /// A refused flock is told to the operator who typed the verb, in words
    /// naming the sheep and the feature, not left in the daemon's log
    /// (spec H3a).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_flock_prints_the_reason_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        std::fs::create_dir_all(&paths.pids).unwrap();
        let _sent = shep_client::testing::fake_daemon_answering_with_ack(
            &paths.socket,
            ack_naming("9.9.9"),
            |_| Response::HandoverFitness {
                refusal: Some("sheep 'clustered' has more than one instance".to_string()),
            },
        )
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            reload(&mut streams, &paths, VersionGuard::Exempt).await
        };

        let text = format!(
            "{}{}",
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap()
        );
        assert!(text.contains("more than one instance"), "{text}");
        // No shepherd owns this home, so the stop arm it fell back to has
        // nothing to stop -- which is what proves it took that arm at all.
        assert_eq!(code, ExitCode::DaemonUnreachable, "{text}");
    }

    /// fails if the dog reading is taken before the dogs have answered —
    /// which is the whole of G13. The shepherd here reports `metrics` as
    /// unsettled twice and stale on the third ask, so the two answers
    /// genuinely DIFFER: a report taken on the first ask says nothing at
    /// all, and only one taken after the dog has finished settling names
    /// it. Both halves are asserted, because a reload that asked once and
    /// happened to catch the third answer would pass the output check on
    /// its own.
    #[tokio::test]
    async fn a_reload_waits_for_a_pending_dog_before_it_reports_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let asks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&asks);
        let (client, _envelopes) = shep_client::testing::fake_client_answering(&addr, move |req| {
            if !matches!(req, Request::DogStaleness) {
                return Response::Mustered(vec![]);
            }
            let seen = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if seen < 2 {
                Response::DogStaleness {
                    stale: vec![],
                    pending: vec!["metrics".to_string()],
                }
            } else {
                Response::DogStaleness {
                    stale: vec!["metrics".to_string()],
                    pending: vec![],
                }
            }
        })
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            report_reload_waiting(
                &client,
                &mut streams,
                true,
                std::time::Duration::from_secs(3),
            )
            .await;
        }

        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("metrics") && text.contains("shep has given up"),
            "the dog that could not come back must be named: {text}"
        );
        assert!(
            asks.load(std::sync::atomic::Ordering::SeqCst) >= 3,
            "an answer taken on the first ask is a claim about a dog that had not spoken"
        );
    }

    /// fails if an ordinary reload starts talking about dogs. Every reload
    /// on a healthy flock takes this path, so a line here is a line the
    /// operator reads every time — and the two facts they ran the verb for,
    /// the shepherd's version and their own flock, are the ones it would
    /// bury.
    #[tokio::test]
    async fn a_reload_whose_dogs_all_answered_says_nothing_about_them() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _envelopes) = shep_client::testing::fake_client_answering(&addr, |req| {
            if matches!(req, Request::DogStaleness) {
                Response::DogStaleness {
                    stale: vec![],
                    pending: vec![],
                }
            } else {
                Response::Mustered(vec![shep_client::testing::sample_info()])
            }
        })
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            report_reload_waiting(
                &client,
                &mut streams,
                true,
                std::time::Duration::from_secs(3),
            )
            .await;
        }

        let text = format!(
            "{}{}",
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap()
        );
        assert!(
            !text.contains("dog"),
            "a flock whose dogs all came back has nothing to say about them: {text}"
        );
    }

    /// fails if a dog that never answers hangs the verb, or is silently
    /// counted as healthy. Both outcomes are wrong in opposite directions:
    /// the reload has already succeeded, so it must finish, and the reading
    /// it took cannot speak for a dog that never spoke.
    #[tokio::test]
    async fn a_reload_stops_waiting_for_a_dog_that_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _envelopes) = shep_client::testing::fake_client_answering(&addr, |req| {
            if matches!(req, Request::DogStaleness) {
                Response::DogStaleness {
                    stale: vec![],
                    pending: vec!["metrics".to_string()],
                }
            } else {
                Response::Mustered(vec![])
            }
        })
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            // The forcing mechanism (IR-46), and the assertion at the same
            // time: a budget that is never consulted loops forever, and a
            // test that waited for it would hang the suite rather than
            // report which line is wrong.
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                report_reload_waiting(
                    &client,
                    &mut streams,
                    true,
                    std::time::Duration::from_millis(150),
                ),
            )
            .await
            .expect("a dog that never answers must not hold the verb open");
        }

        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("metrics") && text.contains("shep bleats metrics"),
            "an unanswered dog is reported as unanswered, not as healthy: {text}"
        );
    }

    /// fails if the report claims something shep did not do. It knows two
    /// handshakes were refused and that a restart ran the binary on disk in
    /// between; it does NOT know what version that file holds, and cannot
    /// until a dog answers `--version` (G11, a later phase). Nor does it
    /// know WHY any named dog stayed silent -- it is handed names, and the
    /// evidence exists only in each dog's own log -- so a sentence
    /// prescribing a reinstall would be a verdict on a question this side of
    /// the wire never asked. Pinned as an exact string in both shapes,
    /// because the singular and the plural are written out separately and a
    /// copy-paste between them is invisible.
    #[test]
    fn the_stale_report_says_what_happened_and_never_reads_the_disk() {
        let one = stale_dog_report(&["metrics".to_string()], "0.1.22");
        assert_eq!(
            one,
            "the `metrics` dog cannot talk to this shepherd; restarting it from the binary on \
             disk did not help, so shep has given up and will not restart it again. \
             `shep bleats metrics` holds what shep saw when it gave up, and the fix follows \
             from that -- reinstalling the same build is not always it. This shepherd is shep \
             0.1.22, if a rebuild is what that log calls for"
        );

        let two = stale_dog_report(&["bark".to_string(), "metrics".to_string()], "0.1.22");
        assert_eq!(
            two,
            "these dogs cannot talk to this shepherd: `bark`, `metrics`; restarting them from \
             the binaries on disk did not help, so shep has given up and will not restart them \
             again. `shep bleats <dog>` holds what shep saw when it gave up on each, and the \
             fix follows from that -- reinstalling the same build is not always it. This \
             shepherd is shep 0.1.22, if a rebuild is what those logs call for"
        );
    }

    /// Pinned as an exact string in both shapes for the same reason as the
    /// stale report above: a message naming the fix is the feature. Fails
    /// if the sentence claims a verdict the shepherd does not have yet, or
    /// if it stops pointing at `shep bleats` — that command is where the
    /// answer actually was in the production incident this phase traces to.
    #[test]
    fn the_unsettled_report_says_what_to_check_and_never_claims_a_verdict() {
        let one = unsettled_dog_report(&["metrics".to_string()], std::time::Duration::from_secs(3));
        assert_eq!(
            one,
            "the `metrics` dog has not answered this shepherd after 3s; a dog silent past 5s \
             is restarted once from the binary on disk and then reported stale, and `shep \
             bleats metrics` shows why"
        );

        let two = unsettled_dog_report(
            &["bark".to_string(), "metrics".to_string()],
            std::time::Duration::from_secs(3),
        );
        assert_eq!(
            two,
            "these dogs have not answered this shepherd after 3s: `bark`, `metrics`; a dog \
             silent past 5s is restarted once from the binary on disk and then reported \
             stale, and `shep bleats <dog>` shows why for each"
        );
    }

    /// A [`HelloAck`] naming `version`, for the arm-selection tests.
    fn ack_naming(version: &str) -> shep_core::protocol::HelloAck {
        shep_core::protocol::HelloAck {
            daemon_version: version.to_string(),
            protocol: shep_core::protocol::PROTOCOL_VERSION,
            pid: 4242,
        }
    }
}
