//! The hidden `daemon` subcommand: runs the supervisor in this process.
//!
//! [`run_daemon`] loads `shep.toml`, boots `shep_daemon::boot`'s supervisor,
//! and blocks in `RunningDaemon::run` until a signal or `KillDaemon` tears it
//! down. Autostart and `--foreground` share one code path; the flag adds
//! readiness reporting and nothing else, so an inherited `$NOTIFY_SOCKET`
//! cannot make an autostarted daemon answer another service's unit. Detaching
//! and the stderr redirect into `shepd.err.log` live in `launch.rs`.

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

/// Everything [`run_daemon`] can fail with.
///
/// [`Self::Boot`] and [`Self::Run`] both wrap a [`BootError`] and stay
/// distinct: `Run` means the supervisor came up and served.
#[derive(Debug)]
pub enum DaemonRunError {
    /// `shep.toml` was unreadable as config.
    Config(DaemonConfigError),
    /// The supervisor failed to come up, before it ever served a request.
    Boot(BootError),
    /// The supervisor came up and served, then failed during its run loop
    /// or teardown.
    Run(BootError),
    /// `[dog.<name>]` sections could not be moved into `dogs.toml`. Raised
    /// before the supervisor comes up, so no dog has read a section from
    /// either file yet.
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

// No `impl From<BootError>`: both `Boot` and `Run` wrap one, so each call
// site picks with an explicit `map_err`.

/// Loads `paths.daemon_config`'s raw source, `None` for a missing file.
///
/// Only `NotFound` is swallowed: any other IO failure is a fault on a real
/// path, and is reported through [`DaemonRunError::Boot`].
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
/// The one global install in the workspace; `shep-daemon`'s
/// `testing::capture_logs` installs a scoped one per test. The sink is
/// stderr, which `launch.rs` has already redirected into
/// `$SHEP_HOME/logs/shepd.err.log` for a re-exec'd daemon.
///
/// Records are written from tokio worker threads, so `main::run`'s `daemon`
/// arm must not hold a `stderr().lock()` guard while this process runs. A
/// failed install means a subscriber is already there, and is reported on
/// stderr rather than failing the boot.
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
/// `RUST_LOG` is ignored; `[daemon] log_level` and `SHEP_LOG_LEVEL` are the
/// only level knobs. An empty `NO_COLOR` is an unset one, per the convention.
fn ansi_enabled(stderr_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    stderr_is_terminal && no_color.is_none_or(OsStr::is_empty)
}

/// Loads config, installs the log subscriber, and boots the supervisor:
/// everything [`run_daemon`] does except serve.
///
/// Separate from [`run_daemon`] so `commands::foreground` can hold the booted
/// daemon rather than block until shutdown. The log subscriber is global, so
/// it is installed here rather than in `shep_daemon::boot`, which one test
/// binary calls many times.
///
/// # Errors
/// - [`DaemonRunError::Config`]: `shep.toml` or a `SHEP_*` override is invalid.
/// - [`DaemonRunError::Boot`]: the config file was unreadable, or the boot failed.
/// - [`DaemonRunError::DogMigration`]: `[dog]` sections could not be moved.
pub async fn boot_supervisor(
    paths: ShepPaths,
    args: &DaemonArgs,
    delete_flock_on_shutdown: bool,
) -> Result<RunningDaemon, DaemonRunError> {
    // Before the config load and before the supervisor: `dog_section` reads
    // the new file from the first request onward, and a dog can connect as
    // soon as the socket is up. Idempotent, and takes both files' locks in
    // this crate's one order, shep.toml outer, dogs.toml inner.
    let moved =
        dog_migration::migrate_dog_sections(&paths).map_err(DaemonRunError::DogMigration)?;
    if !moved.is_empty() {
        // Not `tracing`: `install_log_subscriber` runs below, so a record
        // written here would be dropped.
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
    // The one read of `$NOTIFY_SOCKET` in the workspace. Unix only: it is
    // systemd's readiness protocol, and the field stays on `BootOptions` for
    // both platforms.
    #[cfg(unix)]
    let notify_socket = std::env::var_os(NOTIFY_SOCKET_ENV);
    #[cfg(windows)]
    let notify_socket: Option<std::ffi::OsString> = None;
    let mut options = boot_options(&config, args, notify_socket.as_deref());
    options.delete_flock_on_shutdown = delete_flock_on_shutdown;
    let daemon = boot(TokioRunner::new(), paths, options)
        .await
        .map_err(DaemonRunError::Boot)?;
    // The one publisher of `config.dog.<name>`: the migration above ran
    // before `boot`, when there was no bus to publish onto.
    daemon.context().announce_dog_config(&moved);
    Ok(daemon)
}

/// Runs the supervisor in this process until a signal or `KillDaemon`.
///
/// [`boot_supervisor`] plus `.run()`.
///
/// # Errors
/// - [`DaemonRunError::Config`]: `shep.toml` or a `SHEP_*` override is invalid.
/// - [`DaemonRunError::Boot`]: the config file was unreadable, or the boot failed.
/// - [`DaemonRunError::Run`]: the supervisor served, then failed in its run
///   loop or teardown.
/// - [`DaemonRunError::DogMigration`]: `[dog]` sections could not be moved.
pub async fn run_daemon(paths: ShepPaths, args: &DaemonArgs) -> Result<(), DaemonRunError> {
    // A production daemon always keeps its final roll: `shep muster` after a
    // reboot reads it.
    boot_supervisor(paths, args, false)
        .await?
        .run()
        .await
        .map_err(DaemonRunError::Run)
}

/// Builds the CLI-flag layer of `file < env < flags` from the `daemon`
/// subcommand's own arguments.
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
/// `ready_fd` stays `None`: readiness is a completed handshake, and this crate
/// forbids unsafe code. `max_cron_sleep` stays an `Option`, so the daemon
/// applies its own default and nothing here invents a value. `notify_socket`
/// is a parameter rather than an environment read, since `std::env::set_var`
/// is `unsafe` in edition 2024; `--foreground` gates it.
///
/// `[daemon] enabled_dogs` names each dog to start, in the order an operator
/// wrote it; `[daemon] adopted_dogs` says which of those names is a
/// third-party binary, and a name absent from it is [`DogSource::BuiltIn`].
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
        // `true`.
        delete_flock_on_shutdown: false,
        // This process is the shep binary, which is what the field asserts, so
        // an init system's SIGHUP reaches the handover `shep daemon reload` does.
        handover: true,
    }
}

/// Maps a boot or run failure to the process exit status the parent will read.
///
/// [`BootError`] is `#[non_exhaustive]`, so the [`DaemonRunError::Boot`] arm
/// carries a wildcard and only [`BootError::AlreadyRunning`] gets its own
/// code. [`DaemonRunError::Run`] is always [`ExitCode::Failure`], since
/// `RunningDaemon::run()` names only `BootError::Io`.
#[must_use]
pub fn daemon_exit_code(err: &DaemonRunError) -> ExitCode {
    match err {
        DaemonRunError::Config(_) => ExitCode::InvalidConfig,
        DaemonRunError::Boot(boot_err) => match boot_err {
            BootError::AlreadyRunning { .. } => ExitCode::DaemonAlreadyRunning,
            // `Io`/`Snapshot`/`ReadyWrite` today, plus any future variant
            // `#[non_exhaustive]` makes room for.
            _ => ExitCode::Failure,
        },
        DaemonRunError::Run(_) => ExitCode::Failure,
        // Every refusal this variant carries, I/O faults included, is a
        // shep.toml or dogs.toml an operator has to edit.
        DaemonRunError::DogMigration(_) => ExitCode::InvalidConfig,
    }
}

/// Which mechanism a reload uses to give the flock a shepherd running this
/// binary's code.
///
/// [`Self::StopAndStart`] is permanent: it serves the three cases the handover
/// cannot, Windows, a shepherd predating the handover, and a handover that
/// fails to rehydrate.
#[derive(Debug, PartialEq, Eq)]
enum Arm {
    /// The `execve` handover: the shepherd replaces its own image and the
    /// flock never stops.
    ///
    /// Unix only: Windows has no `execve`.
    #[cfg(unix)]
    Handover,
    /// Stop the shepherd the way `kill` does, wait out its teardown, start a
    /// successor, and muster the roll back.
    StopAndStart,
}

/// The first shep release whose shepherd can hand its flock to a successor.
///
/// A floor on the version the CLI finds running, never on its own: the
/// handover has to exist in the shepherd being replaced. `0.1.17` is the last
/// release that shipped without it. It also keeps `PROTOCOL_VERSION` where it
/// is, since `Request::HandoverFitness` is a variant a shepherd below the
/// floor would end the connection on.
///
/// Only the unix arm of [`Arm::for_daemon`] reads it, hence the `allow` on
/// Windows.
#[cfg_attr(windows, allow(dead_code))]
const HANDOVER_SINCE: &str = "0.1.18";

/// `major.minor.patch` as three numbers, or `None` for anything this cannot
/// read.
///
/// Any pre-release or build suffix is dropped with the patch component it
/// hangs off, so `0.1.18-rc.1` reads as `0.1.18`. Every caller treats `None`
/// as the safe arm.
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
    /// The arm that reloads a shepherd reporting `daemon_version`.
    ///
    /// [`Self::Handover`] needs the shepherd being replaced to carry the
    /// mechanism, so this compares its version against [`HANDOVER_SINCE`]; a
    /// shepherd reporting this binary's own version always counts as
    /// [`Self::Handover`], whatever the floor says. Whether that shepherd's
    /// flock can be carried is a separate question, asked over the socket.
    /// Unknown takes the safe arm: a `None` version, and one this CLI cannot
    /// parse.
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
/// before the shepherd said who it is.
fn version_from_refusal(err: &ConnectError) -> Option<&str> {
    match err {
        ConnectError::ProtocolMismatch { daemon_version, .. } => daemon_version.as_deref(),
        _ => None,
    }
}

/// Replaces the running shepherd with one running this binary's code, and
/// brings the flock back.
///
/// One of the three recovery verbs the version guard exempts, so it must work
/// against a shepherd that refuses the handshake and never needs the socket to
/// succeed. `guard` is threaded from `crate::run`.
pub async fn reload(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: crate::VersionGuard,
) -> ExitCode {
    reload_with_wait(streams, paths, guard, admin::KILL_TEARDOWN_WAIT).await
}

/// As [`reload`], but with a caller-chosen teardown wait.
async fn reload_with_wait(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: crate::VersionGuard,
    wait: std::time::Duration,
) -> ExitCode {
    // Before the connection and before either arm: the handover arm execs a
    // successor that re-reads this file, and a value that fails to load there
    // exits it with the predecessor already gone. File only, since the
    // successor inherits the daemon's own env and flags through `execve`.
    if let Err(err) = read_daemon_config_source(paths).and_then(|source| {
        DaemonConfig::load(source.as_deref(), &|_| None).map_err(DaemonRunError::from)
    }) {
        return streams.fail(daemon_exit_code(&err), &err.to_string());
    }

    // The same argument, aimed at the other file the successor reads: nothing
    // below has signalled the predecessor yet, so a refusal here ends the verb
    // with the running shepherd untouched. Run, not dry-run: the migration is
    // idempotent, and leaves no window between a check and the act it checked.
    match dog_migration::migrate_dog_sections(paths) {
        Ok(moved) if moved.is_empty() => {}
        Ok(moved) => {
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

    // Connected to ask who is there, and, on the handover arm, whether this
    // flock can be carried. Dropped before anything is signalled.
    let connected = match Client::connect(&paths.socket).await {
        Ok(client) => Ok(client),
        Err(err) => Err(version_from_refusal(&err).map(str::to_owned)),
    };
    // `cfg(unix)`, like its only reader: on Windows the arm is never in
    // question and the binding would warn as unused.
    #[cfg(unix)]
    let running_version = match &connected {
        Ok(client) => Some(client.daemon().daemon_version.clone()),
        Err(from_refusal) => from_refusal.clone(),
    };
    #[cfg(unix)]
    if Arm::for_daemon(running_version.as_deref()) == Arm::Handover
        // A version learned from a protocol refusal cannot reach this arm: the
        // decision needs a fitness answer, and a refused handshake has no
        // connection to ask over.
        && let Ok(client) = &connected
    {
        match ask_fitness(client).await {
            Fitness::Carryable => {
                // Before the signal: the shepherd is about to replace its own
                // image, and this connection would not survive it.
                drop(connected);
                return hand_over(streams, paths, guard, wait).await;
            }
            // Not a failure: the flock carries something that cannot move in
            // place, so the reload happens the other way and says why.
            Fitness::Refused(reason) => {
                streams.aside("reload", &reason);
            }
        }
    }
    // Before the stop arm too: on Windows the control address is a named pipe,
    // and a handle held open here keeps the pipe instance alive past the
    // daemon's exit, so `stop_and_start`'s wait would run out.
    drop(connected);
    stop_and_start(streams, paths, guard, wait).await
}

/// What a shepherd says about carrying its own flock.
///
/// The daemon owns both the gate and the wording; the refusal reaches the
/// operator verbatim.
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
/// Every failure is a refusal rather than an error: a shepherd that answers
/// the wrong reply, refuses the request, or drops the connection has said
/// nothing this CLI can act on, and the stop arm works against all three. A
/// signal carries no reply, so the question is settled before one is sent.
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
/// The flock never stops: a carried sheep keeps its pid, its log file handles
/// and its place in the shepherd's registry. The signal is the trigger, not
/// the decision, which already happened over the socket. A handover can still
/// fail after the signal, so this waits for a shepherd of this binary's
/// version and takes the stop arm's own tail when none answers.
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
    // Held across the signal: accepted connections are not carried across the
    // `execve`, so this one closing is the proof the old image is gone.
    // Predecessor and successor otherwise answer on the same pid and version.
    let Ok(witness) = Client::connect(&paths.socket).await else {
        // No witness, no handover: `await_successor` with nothing to outlive
        // would take the predecessor's own answer as the successor's.
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
            // The flock is about to be started rather than carried, so the
            // pids change and the operator is told.
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
/// SIGHUP, since SIGUSR2 is already the log-reopen signal. A shepherd too old
/// to hand over installs SIGHUP as a graceful stop, so the arm selection keeps
/// this away from one.
///
/// # Errors
/// The exit code and the sentence to report, when the pid is not one this
/// platform can name or the signal itself failed. The caller prints it.
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

/// Polls the control socket until a shepherd running this binary's version
/// answers, or `wait` expires.
///
/// The version is what tells the two images apart: the predecessor answers on
/// the same socket right up until it execs. A connect failure inside the
/// window is expected, not a fault.
#[cfg(unix)]
async fn await_successor(
    paths: &ShepPaths,
    witness: &Client,
    wait: std::time::Duration,
) -> Option<Client> {
    let deadline = tokio::time::Instant::now() + wait;

    // Stage one: wait out the predecessor. A request answered on `witness`
    // says the old image is still serving, since that connection cannot
    // survive its exec. Never skipped: a caller without a witness must not
    // reach here.
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

    // Stage two: the old image is gone, so an answered request can only have
    // come from the successor.
    loop {
        // A handshake proves a daemon answered, not that the successor did:
        // `execve` keeps the pid, and this arm can be selected against a
        // shepherd of this very version. Only a served request separates them,
        // since the outgoing image stops serving the moment it execs.
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

/// Gap between [`await_successor`]'s probes. Short: an `execve` plus a
/// rehydrate is milliseconds of work.
#[cfg(unix)]
const SUCCESSOR_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// The pid of the shepherd owning this home, or the code and sentence
/// saying why there is none to act on.
///
/// Reads the lock the daemon holds for its whole life, not the pidfile: a
/// stale pidfile from a crash still exists and its pid may have been reused.
fn proven_shepherd(streams: &mut Streams<'_>, paths: &ShepPaths) -> Result<u32, ExitCode> {
    match boot::daemon_liveness(paths) {
        Ok(Shepherd::Running(pid)) => Ok(pid),
        // Alive and owns the home, but has not recorded a pid yet.
        Ok(Shepherd::Booting) => {
            let message = "a shepherd is starting up and has not recorded its pid yet; try again";
            Err(streams.fail(ExitCode::DaemonUnreachable, message))
        }
        // Nothing to replace: `reload` matches a running system to the binary,
        // so it never starts one unasked.
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
/// [`boot::daemon_liveness`] proves the pid, `commands::admin` owns the signal
/// and the teardown wait, and `crate::connect_or_spawn_client` is the
/// autostart `shep start` already uses.
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
/// The handover arm does not stop the flock, so nothing here may announce that
/// it did or assume a sheep has a new pid.
///
/// `restored` says which arm ran, and decides how the flock is asked for.
/// Under the stop arm the successor has already restored the roll, so
/// `Request::Muster` spawns nothing new. Under the handover arm it must not
/// muster: a successor answers its socket as soon as the listener is carried,
/// before its rehydrate has finished, so `ListFlock` asks for nothing.
async fn report_reload(client: &Client, streams: &mut Streams<'_>, restored: bool) -> ExitCode {
    report_reload_waiting(client, streams, restored, DOG_SETTLE_WAIT).await
}

/// As [`report_reload`], but with a caller-chosen dog wait.
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

/// How long a reload waits for the flock's dogs to finish reconnecting.
///
/// Three seconds, sized against the round trip it has to outlast: a dog
/// refused on the handshake is restarted once from the binary on disk, and
/// only the second refusal, after a full kill ladder and a fresh spawn, makes
/// it stale. Paid only when a dog has not answered.
const DOG_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Gap between [`report_dog_staleness`]'s asks. Coarser than
/// [`SUCCESSOR_POLL_INTERVAL`]: this waits on a process being killed and
/// respawned rather than an `execve`.
const DOG_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Reports the dogs that could not come back, once the shepherd has heard
/// from all of them.
///
/// The waiting is the point: a dog's recorded crate version describes the
/// process that was running when it connected, so a reading taken before the
/// reload answers for the wrong daemon.
///
/// Silent unless something is wrong. A shepherd that will not answer is left
/// alone: the reload has already succeeded by the time this runs.
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
/// Two whole sentences rather than one with the number interpolated: the
/// singular and the plural differ in four places. It prescribes no remedy,
/// because `shep_daemon::dogs::DogRefusals::stale` holds both a dog refused
/// twice, where a rebuild is the fix, and one that never spoke, where it may
/// not be. Only the dog's own log tells them apart.
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
/// Silence would read like a clean reload: the reading was taken before these
/// dogs answered, so it speaks for the rest of the flock and not for them.
///
/// The ladder restarts a silent dog at
/// [`shep_daemon::dogs::DOG_SILENCE_BUDGET`] and marks it stale five seconds
/// later, both after this reload's own wait, so a dog stuck on a protocol it
/// cannot speak lands here rather than in [`stale_dog_report`].
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

/// `` `a`, `b`, `c` ``: the list shape both reports end on.
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

        // Without the flag the address is ignored, so a shep autostarted
        // inside another notify-type service cannot report readiness by accident.
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
        assert_eq!(
            daemon_exit_code(&Run(BootError::Io {
                path: "/x".into(),
                source: std::io::Error::other("x"),
            })),
            ExitCode::Failure
        );
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
        assert_eq!(
            daemon_exit_code(&DaemonRunError::DogMigration(
                DogMigrationError::WouldOverwrite {
                    name: "metrics".to_string(),
                }
            )),
            ExitCode::InvalidConfig
        );
    }

    #[test]
    fn boot_and_run_report_different_phases_for_the_same_underlying_error() {
        use DaemonRunError::{Boot, Run};
        let io_err = || BootError::Io {
            path: "/x".into(),
            source: std::io::Error::other("x"),
        };
        let boot_msg = Boot(io_err()).to_string();
        let run_msg = Run(io_err()).to_string();
        assert_ne!(boot_msg, run_msg);
        assert!(boot_msg.starts_with("the daemon failed to boot"));
        assert!(
            !run_msg.starts_with("the daemon failed to boot"),
            "a run-phase failure must not still claim to be a boot failure: {run_msg:?}"
        );
        assert!(run_msg.starts_with("the daemon failed while running"));
    }

    #[test]
    fn reload_picks_the_stop_arm_against_a_daemon_too_old_to_hand_over() {
        assert_eq!(Arm::for_daemon(Some("0.1.8")), Arm::StopAndStart);
    }

    /// Component-wise rather than lexical: `0.1.9` against `0.1.18` is the
    /// case that tells the two apart.
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

    #[cfg(unix)]
    #[test]
    fn a_shepherd_of_this_binarys_own_version_answers_for_itself() {
        assert_eq!(
            Arm::for_daemon(Some(env!("CARGO_PKG_VERSION"))),
            Arm::Handover
        );
    }

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

    #[test]
    fn a_refusal_that_names_a_version_yields_it_for_the_arm_choice() {
        let refusal = ConnectError::ProtocolMismatch {
            client: shep_core::protocol::PROTOCOL_VERSION,
            daemon_version: Some("0.1.8".to_string()),
            message: "this daemon speaks protocol 1".to_string(),
        };
        assert_eq!(version_from_refusal(&refusal), Some("0.1.8"));
    }

    #[test]
    fn a_connect_failure_that_is_not_a_refusal_names_no_version() {
        let err = ConnectError::HandshakeClosed;
        assert_eq!(version_from_refusal(&err), None);
    }

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

    /// Drives `reload` end to end, so the connect, the arm choice and the
    /// liveness proof are all in the path.
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

    /// Valid TOML holding an invalid level, the gap `toml_edit`'s own parse
    /// check cannot close. No daemon runs here, so without the pre-flight
    /// `reload` would return `DaemonUnreachable`.
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

    #[cfg(unix)]
    #[test]
    fn reload_picks_the_handover_against_a_daemon_new_enough_to_carry_its_flock() {
        assert_eq!(Arm::for_daemon(Some("9.9.9")), Arm::Handover);
        assert_eq!(Arm::for_daemon(Some(HANDOVER_SINCE)), Arm::Handover);
    }

    /// `Request::HandoverFitness` is a variant an older daemon cannot parse,
    /// so the version gate has to keep it unsent.
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
        // No shepherd owns this home, so the stop arm has nothing to stop,
        // which is what proves it took that arm.
        assert_eq!(code, ExitCode::DaemonUnreachable, "{text}");
    }

    /// The shepherd here reports `metrics` as unsettled twice and stale on the
    /// third ask, so a reload that asked once would pass the output check.
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
            // The forcing mechanism as well as the assertion: a budget that
            // is never consulted would hang the suite instead of failing.
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

    /// Pinned as an exact string in both shapes, since the singular and the
    /// plural are written out separately.
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

    /// Pinned as an exact string in both shapes.
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

    /// A `HelloAck` naming `version`, for the arm-selection tests.
    fn ack_naming(version: &str) -> shep_core::protocol::HelloAck {
        shep_core::protocol::HelloAck {
            daemon_version: version.to_string(),
            protocol: shep_core::protocol::PROTOCOL_VERSION,
            pid: 4242,
        }
    }
}
