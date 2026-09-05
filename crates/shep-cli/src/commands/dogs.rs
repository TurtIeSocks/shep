//! `shep enable`/`shep disable`/`shep adopt`/`shep rehome`: turning a
//! registered dog on and off, and registering or forgetting a third-party
//! one.
//!
//! None takes a connected [`Client`] and none autostarts a shepherd: all
//! four must work against a `$SHEP_HOME` with none running, so each
//! connects for itself and tolerates a failure to reach one, writing the
//! config and reporting what the next shepherd will do.
//!
//! Config first, then the daemon: [`ShepToml::edit`] runs before the socket
//! is touched, so a failed RPC still leaves a config the next boot honours.
//! `adopt` puts [`vet_binary`] ahead of both.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use shep_client::{Client, ConnectError, PROTOCOL_VERSION};
use shep_core::barks;
use shep_core::dogs::{DogVersion, SCHEMA_FLAG, VERSION_FLAG, parse_version_answer};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{DogSource, Request, Response, SelectorSpec};

use crate::cli::{AdoptArgs, BarksArgs};
use crate::commands::dog_migration::{self, DogMigrationError};
use crate::commands::shep_toml::{ShepToml, ShepTomlError};
use crate::exit::ExitCode;
use crate::output::{
    BarkRows, DogAdoptedRow, DogDisabledRow, DogEnabledRow, DogRehomedRow, Streams, emit,
    write_outcome,
};

/// [`DogEnabledRow::status`] when `enable` wrote the config and no shepherd
/// answered. A success outcome: `enable` never autostarts one.
const NO_SHEPHERD_ENABLE_STATUS: &str = "will start with the next shepherd";

/// [`DogDisabledRow::status`] when `disable` wrote the config and no
/// shepherd answered: the mirror of [`NO_SHEPHERD_ENABLE_STATUS`].
const NO_SHEPHERD_DISABLE_STATUS: &str = "not running; will not start with the next shepherd";

/// [`DogDisabledRow::status`] when a shepherd stopped the dog.
const DISABLED_STATUS: &str = "stopped";

/// Renders `err` and returns the exit code a config-write failure reports.
///
/// [`ShepTomlError::Parse`] and [`ShepTomlError::WrongShape`] are
/// config-validation failures, so [`ExitCode::InvalidConfig`];
/// [`ShepTomlError::Io`] has no more specific code than
/// [`ExitCode::Failure`].
fn fail_config(streams: &mut Streams<'_>, err: &ShepTomlError) -> ExitCode {
    let code = match err {
        ShepTomlError::Io { .. } => ExitCode::Failure,
        ShepTomlError::Parse { .. } | ShepTomlError::WrongShape { .. } => ExitCode::InvalidConfig,
    };
    streams.fail(code, &err.to_string())
}

/// [`fail_config`] for the other file: renders a `dogs.toml` failure and
/// picks its exit code.
///
/// The same split [`fail_config`] makes: a file that will not parse is
/// [`ExitCode::InvalidConfig`], everything else [`ExitCode::Failure`].
/// Wildcarded because [`DogMigrationError`] is `#[non_exhaustive]`, so a new
/// variant lands as a plain failure rather than a compile error.
fn fail_dogs_config(streams: &mut Streams<'_>, err: &DogMigrationError) -> ExitCode {
    let code = match err {
        DogMigrationError::Parse(_) => ExitCode::InvalidConfig,
        _ => ExitCode::Failure,
    };
    streams.fail(code, &err.to_string())
}

/// Where `name`'s binary comes from, according to `cfg`.
///
/// A name present in `[daemon] adopted_dogs` is an adopted dog and the path
/// recorded there is its binary; a name absent from that map is a built-in
/// dog, an argv branch of this binary. `shep.toml` is the only place either
/// verb can learn it.
fn dog_source(cfg: &ShepToml, name: &str) -> DogSource {
    cfg.adopted_dog_path(name)
        .map_or(DogSource::BuiltIn, |path| DogSource::Adopted {
            path: path.display().to_string(),
        })
}

/// Connects to `paths.socket`, distinguishing a genuine absence from a
/// shepherd that is there and refused.
///
/// `Ok(None)` is only [`ConnectError::Connect`], nothing listening: the one
/// case the four verbs tolerate silently, since none may autostart a
/// shepherd. Every other variant means a connection was established, so the
/// refusal is reported rather than folded in.
///
/// # Errors
/// The exit code and message [`Streams::fail`] already wrote, when the
/// shepherd answered and refused rather than being absent.
async fn connect_or_absent(
    paths: &ShepPaths,
    streams: &mut Streams<'_>,
) -> Result<Option<Client>, ExitCode> {
    match Client::connect(&paths.socket).await {
        Ok(client) => Ok(Some(client)),
        Err(ConnectError::Connect { .. }) => Ok(None),
        Err(err) => {
            let code = ExitCode::from(&err);
            Err(streams.fail(
                code,
                &format!("{err}; run `shep {}`", crate::VERSION_SKEW_REMEDY),
            ))
        }
    }
}

/// [`enable`]'s own failure, so its `try_edit` closure can refuse from
/// inside the lock: either the config layer's error, or a name that answers
/// to no dog at all.
///
/// `Debug` is derived, not redacted: [`Self::Config`] forwards to
/// [`ShepTomlError`]'s own redacted `Debug`, and [`Self::UnknownDog`]
/// carries dog names the refusal already prints.
#[derive(Debug)]
pub(crate) enum EnableRefusal {
    /// The read-modify-write underneath the closure failed.
    Config(ShepTomlError),
    /// The name is neither one of [`crate::dog::BUILT_IN_DOGS`] nor a key of
    /// `[daemon] adopted_dogs`. `adopted` is what that map held, read under
    /// the same lock, so a concurrent `shep adopt` cannot invalidate the
    /// alternatives the refusal names.
    UnknownDog { adopted: Vec<String> },
}

impl From<ShepTomlError> for EnableRefusal {
    fn from(err: ShepTomlError) -> Self {
        Self::Config(err)
    }
}

/// `enable`'s config half: which [`DogSource`] `name` resolves to, whether
/// it names a dog at all, and the write itself.
///
/// [`dog_source`] reads built-in-ness as an absence, so without the
/// unknown-name check a typo lands in `enabled_dogs` as a built-in and the
/// shepherd spawns `shep dog <typo>` on a ladder that cannot succeed. The
/// check sits inside the `try_edit` closure so it skips `save` and reads the
/// adopted names under the lock a concurrent `shep adopt` would race.
///
/// # Errors
/// [`EnableRefusal::Config`] if the read-modify-write failed.
/// [`EnableRefusal::UnknownDog`] if `name` names no dog.
// Windows only, for the same reason as `commands::shep_toml`'s module-wide
// allow: `EnableRefusal::Config` carries `ShepTomlError`, and `Err` crosses
// clippy's 128-byte threshold there.
#[cfg_attr(windows, allow(clippy::result_large_err))]
pub(crate) fn enable_in_config(path: &Path, name: &str) -> Result<DogSource, EnableRefusal> {
    ShepToml::try_edit(path, |cfg| {
        let source = dog_source(cfg, name);
        if matches!(source, DogSource::BuiltIn) && !crate::dog::BUILT_IN_DOGS.contains(&name) {
            return Err(EnableRefusal::UnknownDog {
                adopted: cfg.adopted_dog_names(),
            });
        }
        cfg.enable_dog(name);
        Ok(source)
    })
}

/// `shep enable <name>`: writes the config, and starts the dog if a
/// shepherd is running.
pub async fn enable(streams: &mut Streams<'_>, paths: &ShepPaths, name: &str) -> ExitCode {
    let source = match enable_in_config(&paths.daemon_config, name) {
        Ok(source) => source,
        Err(EnableRefusal::Config(err)) => return fail_config(streams, &err),
        Err(EnableRefusal::UnknownDog { adopted }) => {
            return fail_enable_unknown_dog(streams, name, &adopted);
        }
    };
    let client = match connect_or_absent(paths, streams).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    enable_after_config(streams, name, &source, client.as_ref()).await
}

/// Renders [`enable`]'s refusal of a name that names no dog.
///
/// `adopted` is every key of `[daemon] adopted_dogs`, read under the lock
/// the refusal was decided under, and empty where nothing has been adopted.
/// [`ExitCode::InvalidConfig`] rather than [`ExitCode::Usage`]: the name is
/// one the daemon config cannot resolve, not a malformed argument.
fn fail_enable_unknown_dog(streams: &mut Streams<'_>, name: &str, adopted: &[String]) -> ExitCode {
    let valid: Vec<String> = crate::dog::BUILT_IN_DOGS
        .iter()
        .map(|built_in| format!("{built_in:?}"))
        .chain(adopted.iter().map(|dog| format!("{dog:?}")))
        .collect();
    let message = format!(
        "`{name}` is not a dog; valid names are {} -- if you meant a third-party dog, \
         run `shep adopt {name}` first",
        join_with_and(&valid)
    );
    streams.fail(ExitCode::InvalidConfig, &message)
}

/// Joins `items` as an English list: `a`, `a and b`, `a, b, and c`.
///
/// The empty slice answers with the empty string, and no caller reaches it.
fn join_with_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// `enable`'s daemon half, split out so a test can drive it against a
/// `shep_client::testing` fake rather than a second, real connection.
///
/// `client: None` is [`connect_or_absent`] reporting a genuine absence. A
/// stale socket file and a daemon that was never started are not
/// distinguished, so a provisioning script can configure a host before
/// starting anything.
async fn enable_after_config(
    streams: &mut Streams<'_>,
    name: &str,
    source: &DogSource,
    client: Option<&Client>,
) -> ExitCode {
    let Some(client) = client else {
        let row = DogEnabledRow {
            name: name.to_string(),
            source: source.clone(),
            shepherd_acted: false,
            status: NO_SHEPHERD_ENABLE_STATUS.to_string(),
        };
        return write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "enable",
            row,
            streams.style,
        ));
    };
    // A name a sheep already holds comes back as
    // `RpcErrorCode::InvalidConfig`; the `Err` arm surfaces it verbatim.
    let request = Request::EnableDog {
        name: name.to_string(),
        source: source.clone(),
    };
    match client.request(request).await {
        Ok(Response::DogStarted(info)) => {
            let row = DogEnabledRow {
                name: name.to_string(),
                source: source.clone(),
                shepherd_acted: true,
                status: info.status.to_string(),
            };
            write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                "enable",
                row,
                streams.style,
            ))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// `disable`'s config half: which [`DogSource`] `name` resolved to, and the
/// removal from `[daemon] enabled_dogs`.
///
/// `disable_dog` leaves `[daemon] adopted_dogs` alone, the difference
/// between `disable` and `rehome`, so the source reads the same before or
/// after the edit. Read for the report only: `DisableDog` carries a name and
/// nothing else.
///
/// # Errors
/// [`ShepTomlError`] if the read-modify-write underneath the edit failed.
pub(crate) fn disable_in_config(path: &Path, name: &str) -> Result<DogSource, ShepTomlError> {
    ShepToml::edit(path, |cfg| {
        let source = dog_source(cfg, name);
        cfg.disable_dog(name);
        source
    })
}

/// `shep disable <name>`: removes it from the config, and stops it if a
/// shepherd is running.
pub async fn disable(streams: &mut Streams<'_>, paths: &ShepPaths, name: &str) -> ExitCode {
    let source = match disable_in_config(&paths.daemon_config, name) {
        Ok(source) => source,
        Err(err) => return fail_config(streams, &err),
    };
    let client = match connect_or_absent(paths, streams).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    disable_after_config(streams, name, &source, client.as_ref()).await
}

/// `disable`'s daemon half; see [`enable_after_config`] for the split and
/// for what `client: None` means.
async fn disable_after_config(
    streams: &mut Streams<'_>,
    name: &str,
    source: &DogSource,
    client: Option<&Client>,
) -> ExitCode {
    let Some(client) = client else {
        let row = DogDisabledRow {
            name: name.to_string(),
            source: source.clone(),
            shepherd_acted: false,
            status: NO_SHEPHERD_DISABLE_STATUS.to_string(),
        };
        return write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "disable",
            row,
            streams.style,
        ));
    };
    // `Response::Deleted`, the same reply `Delete` gives: disabling
    // deregisters exactly as `Delete` does.
    match client
        .request(Request::DisableDog {
            name: name.to_string(),
        })
        .await
    {
        Ok(Response::Deleted(_ids)) => {
            let row = DogDisabledRow {
                name: name.to_string(),
                source: source.clone(),
                shepherd_acted: true,
                status: DISABLED_STATUS.to_string(),
            };
            write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                "disable",
                row,
                streams.style,
            ))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// Why a binary cannot be adopted.
///
/// The modes `enable` cannot have: a dog that ships inside this binary has
/// no path to be missing, no permission bit to be unset, and nobody else
/// who can write it.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub enum AdoptRefusal {
    /// Nothing exists at that path.
    Missing,
    /// It exists and is not a file (a directory, most often a `bin/` the
    /// operator meant to point inside of).
    NotAFile,
    /// It exists and no execute bit is set for anyone.
    NotExecutable,
    /// The binary, or the directory holding it, can be written by any user
    /// on this system. An adopted dog is exec'd at the shepherd's own trust
    /// level on every restart without being re-vetted. A writable directory
    /// counts too: the binary can be renamed away and replaced.
    WorldWritable {
        /// The offending path: the binary itself, or its directory.
        path: PathBuf,
    },
    /// It exists, is executable, and this kernel will not exec it: the
    /// wrong architecture, or an interpreter line naming something absent.
    WillNotExec {
        /// What `exec` reported.
        reason: String,
    },
    /// It answered `--version` (see [`DogVersion`]) with a `shep-protocol`
    /// this shep does not speak, so adopting it would register a dog that
    /// connects to nothing. Only a stated protocol reaches this: a dog that
    /// names none is [`DogVersion::protocol`]'s `None` and is adopted.
    ProtocolMismatch {
        /// What the candidate said it speaks.
        dog: u32,
        /// [`PROTOCOL_VERSION`], what this shep speaks.
        shep: u32,
    },
}

impl std::fmt::Display for AdoptRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "no file exists at that path"),
            Self::NotAFile => write!(f, "that path is not a file"),
            Self::NotExecutable => write!(f, "no execute bit is set on that file"),
            Self::WorldWritable { path } => write!(
                f,
                "{} is writable by any user on this system, and an adopted dog runs \
                 with the shepherd's own privileges",
                path.display()
            ),
            Self::WillNotExec { reason } => {
                write!(f, "this kernel refused to run that file: {reason}")
            }
            Self::ProtocolMismatch { dog, shep } => write!(
                f,
                "this dog was built for shep protocol {dog}, and this shep speaks {shep}; \
                 reinstall the dog without --locked so it builds against the current \
                 shep-core, or run a shep that speaks {dog}"
            ),
        }
    }
}

impl core::error::Error for AdoptRefusal {}

/// Vets `path` as a dog binary, before anything is written to `shep.toml`.
///
/// Returns the absolute, canonicalized path: the daemon exec's it after a
/// reboot, from whatever directory the init system gave it. Checks in order,
/// each refusing before the next: existence, file-ness, the execute bit,
/// [`writability`], then one spawn per probe flag, since a probe runs the
/// binary under `home` and `name`, the environment the adopted dog gets.
///
/// # Errors
/// [`AdoptRefusal`] when the path does not resolve, is not a file this
/// kernel will exec, or answers a protocol this shep cannot speak. Silence
/// is not an error: unknown protocol, [`DogSchema::Silent`] schema.
pub fn vet_binary(path: &Path, home: &Path, name: &str) -> Result<VettedBinary, AdoptRefusal> {
    vet_binary_within(path, home, name, VERSION_BUDGET)
}

/// [`vet_binary`], against a caller-chosen budget for the probes.
///
/// Production has one budget and [`vet_binary`] passes it. A test whose
/// question has nothing to do with timing can pass a generous one: the probe
/// spawns a real child and bounds the wait on a wall clock.
///
/// `budget` bounds one wait, not the call. [`answer_text`] gives the full
/// budget to each of two waits, the child's exit and its output, so a probe
/// costs roughly twice `budget` and a vet runs two probes.
///
/// # Errors
/// The same [`AdoptRefusal`] set [`vet_binary`] raises.
pub fn vet_binary_within(
    path: &Path,
    home: &Path,
    name: &str,
    budget: Duration,
) -> Result<VettedBinary, AdoptRefusal> {
    let metadata = std::fs::metadata(path).map_err(|_| AdoptRefusal::Missing)?;
    if !metadata.is_file() {
        return Err(AdoptRefusal::NotAFile);
    }
    // No execute bit set for anyone: owner (0o100), group (0o010), or
    // other (0o001).
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(AdoptRefusal::NotExecutable);
    }
    // Only a symlink loop or a race with a delete can fail here, and neither
    // has anything more specific than `Missing`. The verbatim prefix is
    // stripped because the path is recorded in a file operators edit.
    let canonical = path
        .canonicalize()
        .map(|abs| shep_core::paths::strip_verbatim_prefix(&abs).into_owned())
        .map_err(|_| AdoptRefusal::Missing)?;
    let group_writable = writability(&canonical)?;
    let answer = ask_version(&canonical, home, name, budget)?;
    // `answer.version` is never compared: a third-party dog's crate version
    // has no relationship to shep's own. Only the protocol decides whether
    // the dog can connect.
    if let Some(dog) = answer.as_ref().and_then(|answer| answer.protocol)
        && dog != PROTOCOL_VERSION
    {
        return Err(AdoptRefusal::ProtocolMismatch {
            dog,
            shep: PROTOCOL_VERSION,
        });
    }
    // After the protocol refusal: a candidate shep is about to refuse is
    // not run a second time.
    let schema = ask_schema(&canonical, home, name, budget);
    Ok(VettedBinary {
        path: canonical,
        group_writable,
        answer,
        schema,
    })
}

/// The environment the probe runs a candidate with: what the daemon would
/// give the dog, and nothing else.
///
/// Mirrors `shep_daemon::assemble::base_env`, which is private to a crate
/// the CLI does not reach into. The lists are duplicated: if the daemon's
/// allowlist grows, this one has to follow, or a candidate is vetted under
/// conditions its supervised run will not have.
fn probe_env() -> Vec<(String, String)> {
    #[cfg(unix)]
    const INHERITED: &[&str] = &["HOME", "USER", "LANG", "TZ"];
    #[cfg(unix)]
    const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
    #[cfg(windows)]
    const INHERITED: &[&str] = &[
        "SystemRoot",
        "SystemDrive",
        "windir",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "COMSPEC",
        "PATHEXT",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
    ];
    #[cfg(windows)]
    const DEFAULT_PATH: &str = r"C:\Windows\system32;C:\Windows;C:\Windows\System32\Wbem";

    let path = std::env::var("PATH")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| DEFAULT_PATH.to_string());
    let mut env = vec![("PATH".to_string(), path)];
    env.extend(
        INHERITED
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|v| ((*key).to_string(), v))),
    );
    env
}

/// Runs `path` with `flag`, one of [`VERSION_FLAG`] or [`SCHEMA_FLAG`], and
/// hands back what it printed on stdout, within `budget`.
///
/// `Ok(None)` is no answer, and never a fault: silence, a run that failed,
/// and a run still going when the budget ran out all arrive that way.
/// Answering either flag is optional. `Ok(Some(text))` is only ever the
/// output of a run that exited 0; what counts as a usable answer is each
/// caller's own question.
///
/// # Errors
/// [`AdoptRefusal::WillNotExec`], and only that: nothing here judges the
/// answer it read.
fn ask(
    path: &Path,
    flag: &str,
    home: &Path,
    name: &str,
    budget: Duration,
) -> Result<Option<String>, AdoptRefusal> {
    // `env_clear` and the daemon's own allowlist, never the operator's
    // environment: this runs a stranger's binary. `SHEP_HOME` is the home
    // this invocation resolved, or the candidate finds the live daemon's
    // socket. Stdout is piped so it cannot write on the operator's terminal.
    match Command::new(path)
        .arg(flag)
        .env_clear()
        .envs(probe_env())
        .env("SHEP_HOME", home)
        .env("SHEP_DOG_NAME", name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Err(err) => Err(AdoptRefusal::WillNotExec {
            reason: err.to_string(),
        }),
        Ok(mut child) => {
            // Started before any wait: a candidate that writes more than a
            // pipe buffer holds would otherwise block on its own `write`,
            // indistinguishable from one that never exits.
            let reading = child.stdout.take().map(read_in_background);
            if let Some(reason) = macos_deferred_exec_failure(&mut child) {
                let _ = child.wait();
                return Err(AdoptRefusal::WillNotExec { reason });
            }
            let answer = answer_text(&mut child, reading, budget);
            let _ = child.kill();
            let _ = child.wait();
            Ok(answer)
        }
    }
}

/// Asks `path` for its version with [`VERSION_FLAG`] and parses the answer.
///
/// `Ok(None)` is an unknown protocol and never a fault: everything [`ask`]
/// answers `None` for, plus output with no line 1 to read a version from.
/// [`warn_of_a_dog_a_restart_would_break`] asks a dog adopted long ago,
/// since the binary changes on disk with nothing watching. Neither caller
/// writes the answer down.
///
/// # Errors
/// [`AdoptRefusal::WillNotExec`], and only that.
fn ask_version(
    path: &Path,
    home: &Path,
    name: &str,
    budget: Duration,
) -> Result<Option<DogVersion>, AdoptRefusal> {
    Ok(ask(path, VERSION_FLAG, home, name, budget)?
        .as_deref()
        .and_then(parse_version_answer))
}

/// Asks `path` for its config schema with [`SCHEMA_FLAG`], and reads the
/// answer as JSON.
///
/// `pub(crate)`: `shep lookout`'s dog config pane calls this too.
///
/// No `Result`: nothing a candidate does to this probe can refuse an adopt.
/// A failure to spawn arrives as [`DogSchema::Silent`], like a dog that has
/// never heard of the flag.
///
/// The answer is written down by nothing. `cargo install` replaces a dog's
/// binary with nothing watching, and a stale schema mislabels which field is
/// a credential.
pub(crate) fn ask_schema(path: &Path, home: &Path, name: &str, budget: Duration) -> DogSchema {
    // Empty output is a dog with no schema, not a schema that failed to
    // parse: empty input is invalid JSON, so without the guard the ordinary
    // case earns the warning meant for a broken one.
    match ask(path, SCHEMA_FLAG, home, name, budget) {
        Ok(Some(text)) if !text.trim().is_empty() => match serde_json::from_str(&text) {
            Ok(schema) => DogSchema::Published(schema),
            Err(_) => DogSchema::Unreadable,
        },
        Ok(_) | Err(_) => DogSchema::Silent,
    }
}

/// A binary [`vet_binary`] accepted, and what an operator should still be
/// told about it.
#[derive(Debug, PartialEq, Eq)]
pub struct VettedBinary {
    /// The absolute, canonicalized path: the one `adopt` records and the
    /// daemon later exec's.
    pub path: PathBuf,
    /// The paths [`writability`] found group-writable: the binary, its
    /// directory, both, or neither. One notice each, never a refusal.
    pub group_writable: Vec<PathBuf>,
    /// What it answered when asked for its version, and `None` when it
    /// answered nothing shep could read. Reported, never recorded.
    pub answer: Option<DogVersion>,
    /// What it answered when asked for its config schema. Written down by
    /// nobody, for the reason [`ask_schema`] gives.
    pub schema: DogSchema,
}

/// What a candidate answered when asked for its config schema. Three
/// answers, since only one of the two ways of having no schema is worth
/// telling an operator about. Nothing here is a refusal.
#[derive(PartialEq, Eq)]
pub enum DogSchema {
    /// The dog printed JSON, exactly as it wrote it. Not validated past
    /// being JSON: the dog is the authority on its own config.
    Published(serde_json::Value),
    /// The dog answered nothing shep can use: it printed nothing, its run
    /// failed, it never exited, or it could not be spawned. Earns no
    /// warning.
    Silent,
    /// The dog printed something that is not JSON: a bug in that dog, and
    /// the one shape `adopt` warns about.
    Unreadable,
}

/// Reports that there is a schema, never what is in it.
///
/// A schema carries the dog's own defaults, the same field the secret
/// marker exists to keep off a screen, so a derive would put a credential
/// into any `{vetted:?}`.
impl core::fmt::Debug for DogSchema {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Published(_) => f.write_str("Published(..)"),
            Self::Silent => f.write_str("Silent"),
            Self::Unreadable => f.write_str("Unreadable"),
        }
    }
}

/// How long [`ask`] gives a binary to answer one probe flag, and separately
/// how long it then waits for that answer to reach the reader thread. Each
/// flag is asked in its own spawn and gets the whole budget.
///
/// One second, against the milliseconds a `println!` and an exit take. The
/// headroom is for a cold, dynamically linked binary on a loaded machine,
/// where a probe takes 180 to 300ms against single-digit milliseconds idle.
/// Too short records an unknown protocol for a slow dog; too long stalls
/// every adopt of the dogs that exist today, none of which answer at all.
///
/// `restart` asks with the same number. A binary that hangs is killed at the
/// budget and the restart proceeds unwarned.
pub(crate) const VERSION_BUDGET: Duration = Duration::from_secs(1);

/// How often [`answer_text`] polls within [`VERSION_BUDGET`].
const VERSION_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// The most a probe will read from a candidate, per spawn.
///
/// One mebibyte, against a version answer of two lines and a JSON Schema of
/// single-digit kilobytes. Without a bound the read is limited only by the
/// budget and the candidate's write speed, which reached roughly 290MB
/// resident for a second of spew.
///
/// Truncation is never a refusal: a cut-off version answer is the unknown
/// protocol it already was, and a cut-off schema is
/// [`DogSchema::Unreadable`].
const PROBE_OUTPUT_LIMIT: u64 = 1024 * 1024;

/// Drains `stdout` to end on a thread, handing the text back through the
/// returned channel.
///
/// A thread, because neither read can be bounded: before the candidate
/// exits a read blocks until it writes, and after it exits a read blocks
/// for as long as anything it spawned still holds the inherited pipe open.
/// On the timeout path the thread ends when the pipe closes, holding only a
/// `String` and a sender.
fn read_in_background(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut text = String::new();
        // The drop that follows is half of what the bound buys: the read
        // end closes, so a candidate still writing takes an EPIPE. Non-UTF-8
        // bytes read as silence, a character the cap cut in half included:
        // `read_to_string` leaves `text` empty when it fails.
        let _ = stdout.take(PROBE_OUTPUT_LIMIT).read_to_string(&mut text);
        let _ = tx.send(text);
    });
    rx
}

/// Waits, bounded by `budget`, for `child` to answer, and returns what it
/// printed.
///
/// Twice `budget` is the worst case: the wait for the exit and the wait for
/// the reader thread's text are bounded separately. Only a child that has
/// already exited successfully reaches the second, which is there for a
/// grandchild still holding the inherited pipe open.
///
/// `None` is no answer, never a refusal: no pipe to read, no exit inside the
/// budget, or a non-zero exit. `docs/dogs.md` asks a dog to answer on stdout
/// and exit 0, so lines from a run that then failed are not an answer.
/// `Some` may still be empty.
fn answer_text(
    child: &mut Child,
    reading: Option<Receiver<String>>,
    budget: Duration,
) -> Option<String> {
    let reading = reading?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if started.elapsed() >= budget => return None,
            Ok(None) => std::thread::sleep(VERSION_POLL_INTERVAL),
        }
    }
    reading.recv_timeout(budget).ok()
}

/// Who besides the owner can write `canonical` and the directory holding it.
///
/// World-writable is unambiguous, so it refuses. Group-writable is not: a
/// deployment directory owned by a trusted deploy group is a normal
/// arrangement, so it comes back as a path to warn about and the adopt
/// proceeds. The sticky bit is not an exemption. A path with no parent
/// (`/` itself) cannot be a file and never reaches here.
///
/// # Errors
/// [`AdoptRefusal::WorldWritable`], naming whichever of the two paths it
/// found first, the binary before its directory, so the more specific thing
/// to fix is the one reported.
fn writability(canonical: &Path) -> Result<Vec<PathBuf>, AdoptRefusal> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // Only the `cfg(unix)` push below writes this.
    #[cfg_attr(windows, allow(unused_mut))]
    let mut group_writable = Vec::new();
    for candidate in [Some(canonical), canonical.parent()].into_iter().flatten() {
        // Unreadable metadata is not a refusal: this check has nothing to
        // say about a directory whose mode cannot be read.
        let Ok(metadata) = std::fs::metadata(candidate) else {
            continue;
        };
        // The Windows analogue is an ACE granting write to a broad group,
        // which needs a real ACL read. `shep adopt` does not check it
        // there, and the operator docs say so.
        #[cfg(windows)]
        let _ = &metadata;
        #[cfg(unix)]
        let mode = metadata.permissions().mode();
        #[cfg(unix)]
        if mode & 0o002 != 0 {
            return Err(AdoptRefusal::WorldWritable {
                path: candidate.to_path_buf(),
            });
        }
        #[cfg(unix)]
        if mode & 0o020 != 0 {
            group_writable.push(candidate.to_path_buf());
        }
    }
    Ok(group_writable)
}

/// How long [`macos_deferred_exec_failure`] gives a spawned probe to prove
/// it cannot run, before treating it as a real, running binary.
///
/// 50ms, next to the ~3ms this module's tests observe the kernel's fallback
/// taking; generous against scheduler contention. A binary that does run
/// polls for the whole 50ms, since nothing here can prove a negative early.
#[cfg(target_os = "macos")]
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

/// How often [`macos_deferred_exec_failure`] polls within [`PROBE_BUDGET`].
#[cfg(target_os = "macos")]
const PROBE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_micros(500);

/// Catches the one way [`vet_binary`]'s `Command::spawn` can succeed for a
/// file this kernel cannot actually run.
///
/// macOS's `posix_spawn` fast path is not synchronous for an exec-format
/// failure: the fork has already happened when `spawn` returns `Ok`, and a
/// file the kernel cannot recognize is re-executed through `/bin/sh`, which
/// refuses it and exits `126`. That happens within a few milliseconds, well
/// inside [`PROBE_BUDGET`], while a runnable binary is still running.
#[cfg(target_os = "macos")]
fn macos_deferred_exec_failure(child: &mut std::process::Child) -> Option<String> {
    let start = std::time::Instant::now();
    while start.elapsed() < PROBE_BUDGET {
        match child.try_wait() {
            Ok(Some(status)) if status.code() == Some(126) => {
                return Some(
                    "this kernel could not recognize the file as an executable".to_string(),
                );
            }
            // A real, if fast, run, or a failed wait: neither is this
            // function's to report.
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => std::thread::sleep(PROBE_POLL_INTERVAL),
        }
    }
    None
}

/// Every kernel but macOS reports an exec-format failure through
/// `Command::spawn`'s `Err` arm, so there is nothing to catch.
#[cfg(not(target_os = "macos"))]
fn macos_deferred_exec_failure(_child: &mut std::process::Child) -> Option<String> {
    None
}

/// Renders `refusal` and returns the exit code an unvettable binary
/// reports: [`ExitCode::InvalidConfig`] for every mode, since what is wrong
/// is the argument `adopt` was given, not shep's own state.
fn fail_adopt(streams: &mut Streams<'_>, path: &Path, refusal: &AdoptRefusal) -> ExitCode {
    let code = ExitCode::InvalidConfig;
    let message = format!("{}: {refusal}", path.display());
    streams.fail(code, &message)
}

/// [`emit_notice`] code for the group-writable warning: caller-defined, and
/// not one of [`ExitCode::code_str`]'s, since `adopt` still succeeds here.
const GROUP_WRITABLE_NOTICE: &str = "group_writable";

/// Warns that `path` is group-writable, and lets the adopt proceed.
///
/// Goes out through [`emit_notice`] rather than [`emit_error`] so a
/// `--format json` consumer can tell a diagnostic on a successful command
/// from a failure.
fn warn_group_writable(streams: &mut Streams<'_>, path: &Path) {
    let message = format!(
        "{} is writable by its group; anyone in that group can replace the binary \
         this dog runs, and it runs with the shepherd's own privileges",
        path.display()
    );
    streams.aside(GROUP_WRITABLE_NOTICE, &message);
}

/// [`emit_notice`] code for the version report; like
/// [`GROUP_WRITABLE_NOTICE`], not a failure.
const DOG_VERSION_NOTICE: &str = "dog_version";

/// Tells the operator what the candidate answered.
///
/// The version is reported, never compared. The protocol is reported only
/// when it is missing, the operator's one chance to hear that this dog's
/// compatibility is unknown until it connects. A dog that answered nothing
/// gets no notice: that is the ordinary case.
fn report_dog_version(streams: &mut Streams<'_>, name: &str, answer: &DogVersion) {
    let message = match answer.protocol {
        Some(protocol) => format!(
            "{name} reports version {}, shep protocol {protocol}",
            answer.version
        ),
        None => format!(
            "{name} reports version {} and names no shep protocol, so whether it can \
             speak to this shep is unknown until it connects",
            answer.version
        ),
    };
    streams.aside(DOG_VERSION_NOTICE, &message);
}

/// [`emit_notice`] code for the unreadable-schema warning; not a failure.
const DOG_SCHEMA_UNREADABLE_NOTICE: &str = "dog_schema_unreadable";

/// Warns that `name` answered the schema flag with something that is not
/// JSON, and lets the adopt proceed.
///
/// The one schema answer worth a line. Silence is the ordinary case, and a
/// notice for each silent dog is how an operator learns to skip the one that
/// matters.
fn warn_unreadable_schema(streams: &mut Streams<'_>, name: &str) {
    let message = format!(
        "{name} answered `{SCHEMA_FLAG}` with something that is not JSON, so shep has \
         no description of its settings and they stay a hand-edited section. The dog \
         is adopted and runs normally; this is a bug to report to whoever wrote it"
    );
    streams.aside(DOG_SCHEMA_UNREADABLE_NOTICE, &message);
}

/// [`emit_notice`](crate::output::emit_notice) code for the warning
/// `restart` prints before it restarts a dog whose binary on disk cannot
/// speak to this shepherd. Not a failure: the restart still happens.
const DOG_BINARY_SKEW_NOTICE: &str = "dog_binary_skew";

/// Warns, before `restart` sends anything, about a dog whose binary on disk
/// speaks a protocol this shepherd does not.
///
/// The running dog works, the binary it would come back from does not, and
/// the two meet at the next restart. A warning, never a refusal: the binary
/// may be exactly what the operator just installed. A dog that does not
/// answer [`VERSION_FLAG`] is unknown rather than stale and gets nothing.
///
/// Only a `Name` selector is probed: a built-in dog has no `[daemon]
/// adopted_dogs` entry, an `all` or `/regex/` sweep names no dog, and an
/// `Id` would cost a round trip. `budget` is a parameter for the reason
/// [`vet_binary_within`] is.
pub fn warn_of_a_dog_a_restart_would_break(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    selectors: &[SelectorSpec],
    budget: Duration,
) {
    for selector in selectors {
        let SelectorSpec::Name(name) = selector else {
            continue;
        };
        // `None` for every name `[daemon] adopted_dogs` has never heard
        // of, which is every built-in dog and every sheep.
        let Ok(Some(binary)) = ShepToml::adopted_dog_path_readonly(&paths.daemon_config, name)
        else {
            continue;
        };
        // Asked, never remembered: a protocol recorded at adopt time would
        // copy a number that changes on disk with nothing watching. A dog
        // that ignores `--version` runs its ordinary job instead, for up to
        // `budget`; `docs/dogs.md` names that cost.
        let Ok(Some(answer)) = ask_version(&binary, &paths.home, name, budget) else {
            continue;
        };
        let Some(disk) = answer.protocol else {
            continue;
        };
        if disk == PROTOCOL_VERSION {
            continue;
        }
        let message = format!(
            "`{name}`'s binary at {} was built for shep protocol {disk}, and this shep \
             speaks {PROTOCOL_VERSION}; restarting it brings it back on that binary, \
             unable to connect. Run a shep that speaks {disk}, or reinstall the dog \
             against protocol {PROTOCOL_VERSION}, and restart it again",
            binary.display()
        );
        streams.aside(DOG_BINARY_SKEW_NOTICE, &message);
    }
}

/// `shep adopt <path> [--name <name>]`: vets a binary shep has never seen,
/// records it, and starts it if a shepherd is running.
///
/// `args.path` is resolved by [`resolve_adopt_path`], and `args.name`
/// defaults to the resolved binary's file stem ([`default_dog_name`]); a
/// defaulted name goes through the same [`collides_with_a_verb`] refusal an
/// explicit `--name` would.
pub async fn adopt(streams: &mut Streams<'_>, paths: &ShepPaths, args: &AdoptArgs) -> ExitCode {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path_var = std::env::var_os("PATH");
    let candidate = resolve_adopt_path(&args.path, home.as_deref(), path_var.as_deref());
    // Before vetting: `vet_binary` spawns the candidate, and a refusal
    // that runs after that spawn has already run what it refuses.
    let name = match &args.name {
        Some(name) => name.clone(),
        None => default_dog_name(&candidate),
    };
    if collides_with_a_verb(&name) {
        return fail_adopt_name_collision(streams, &name);
    }
    let vetted = match vet_binary(&candidate, &paths.home, &name) {
        Ok(vetted) => vetted,
        Err(refusal) => return fail_adopt(streams, &candidate, &refusal),
    };
    let path = vetted.path;
    for writable in &vetted.group_writable {
        warn_group_writable(streams, writable);
    }
    if let Some(answer) = &vetted.answer {
        report_dog_version(streams, &name, answer);
    }
    // The schema is stored by nothing and asked fresh where it is needed,
    // so the only thing to report is the one answer that is a bug.
    if vetted.schema == DogSchema::Unreadable {
        warn_unreadable_schema(streams, &name);
    }
    if let Err(err) = ShepToml::edit(&paths.daemon_config, |cfg| {
        cfg.adopt_dog(&name, &path);
    }) {
        return fail_config(streams, &err);
    }
    let client = match connect_or_absent(paths, streams).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    adopt_after_config(streams, &name, &path, client.as_ref()).await
}

/// Resolves `raw`, `shep adopt`'s own path argument, before it reaches
/// [`vet_binary`]: as given, with a leading `~/` expanded against `home`,
/// then looked up on `path_var`. First hit wins; if none finds anything,
/// `raw` comes back unchanged so `vet_binary` reports the same
/// [`AdoptRefusal::Missing`] it always has.
///
/// `home` and `path_var` are parameters, read once by [`adopt`], so this
/// stays a pure function of its inputs: this crate forbids `unsafe`, and a
/// test cannot reach `std::env::set_var`. All three routes funnel into the
/// one [`vet_binary`] call, so this changes what `adopt` can find, never
/// what it vets.
fn resolve_adopt_path(raw: &Path, home: Option<&Path>, path_var: Option<&OsStr>) -> PathBuf {
    if raw.exists() {
        return raw.to_path_buf();
    }
    if let Some(expanded) = raw
        .to_str()
        .and_then(|value| expand_tilde_candidate(value, home))
        && expanded.exists()
    {
        return expanded;
    }
    if let Some(found) = lookup_on_path(raw, path_var) {
        return found;
    }
    raw.to_path_buf()
}

/// `~/`-expands `value` against `home`, for [`resolve_adopt_path`]'s second
/// step. `None` for anything [`shep_core::config::expand_home_tilde`]
/// refuses, or that does not start with `~` at all: [`resolve_adopt_path`]
/// moves on rather than surfacing a tilde-specific error for a path that may
/// never have been a tilde path.
fn expand_tilde_candidate(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if !value.starts_with('~') {
        return None;
    }
    shep_core::config::expand_home_tilde(value, home)
        .ok()
        .map(PathBuf::from)
}

/// Looks `name` up on `path_var` the way a shell would, and only that way:
/// a bare name with no directory component of its own (`shep-log-rotate`,
/// not `./shep-log-rotate`), and only a hit with an execute bit set for
/// someone, so a same-named non-executable file earlier on `$PATH` does not
/// block the real binary further down it.
fn lookup_on_path(name: &Path, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let is_bare = name
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty());
    if !is_bare {
        return None;
    }
    let dirs = path_var?;
    std::env::split_paths(dirs)
        .flat_map(|dir| {
            candidate_file_names(name)
                .into_iter()
                .map(move |file| dir.join(file))
        })
        .find(|candidate| {
            #[cfg(unix)]
            use std::os::unix::fs::PermissionsExt as _;
            // Windows has no execute bit and `CreateProcess` is the only
            // authority on `%PATHEXT%`, so being a file is the test there;
            // the spawn refuses a non-executable one with the OS's message.
            std::fs::metadata(candidate).is_ok_and(|meta| {
                #[cfg(unix)]
                {
                    meta.is_file() && meta.permissions().mode() & 0o111 != 0
                }
                #[cfg(windows)]
                {
                    meta.is_file()
                }
            })
        })
}

/// The file names a bare command could resolve to in one `$PATH` directory.
///
/// On unix that is the name itself and nothing else: a file is runnable if
/// its execute bit is set, whatever it is called.
///
/// Windows resolves a bare command through `%PATHEXT%`, and `cargo install`
/// writes `foo.exe`, never `foo`. The bare name is tried first, so an
/// extensionless file is still found; the extensions follow in `%PATHEXT%`
/// order. The fallback list is the documented default for a system where
/// the variable is unset.
fn candidate_file_names(name: &Path) -> Vec<std::ffi::OsString> {
    #[cfg(unix)]
    {
        vec![name.as_os_str().to_os_string()]
    }
    #[cfg(windows)]
    {
        let mut names = vec![name.as_os_str().to_os_string()];
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        for ext in pathext.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            let mut with_ext = name.as_os_str().to_os_string();
            with_ext.push(ext);
            names.push(with_ext);
        }
        names
    }
}

/// The dog name `shep adopt` defaults to when `--name` is omitted: `path`'s
/// file stem with one leading `shep-` stripped, the way `cargo` strips
/// `cargo-`. A stem that would strip to an empty name is kept whole.
///
/// Derived from `path` as resolved, before canonicalization: a symlink's own
/// name is what an operator typed.
fn default_dog_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| path.to_str().unwrap_or("dog"));
    stem.strip_prefix("shep-")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(stem)
        .to_string()
}

/// Whether `name` already names a built-in verb or one of its visible
/// aliases. A dog adopted under such a name could never be reached: `shep
/// <name>` dispatches to the built-in verb first.
///
/// Read off the real `clap::Command` tree rather than a hand-copied list, so
/// a verb added later is refused automatically.
fn collides_with_a_verb(name: &str) -> bool {
    use clap::CommandFactory as _;
    crate::cli::Cli::command()
        .get_subcommands()
        .any(|sub| sub.get_name() == name || sub.get_all_aliases().any(|alias| alias == name))
}

/// Renders the refusal for a name `shep adopt` will not accept because it
/// already names a built-in verb or alias.
fn fail_adopt_name_collision(streams: &mut Streams<'_>, name: &str) -> ExitCode {
    let code = ExitCode::InvalidConfig;
    let message = format!(
        "`{name}` is already a shep verb or alias, so an adopted dog by that name could never \
         be reached -- pick another name with --name"
    );
    streams.fail(code, &message)
}

/// `adopt`'s daemon half; see [`enable_after_config`] for the split and for
/// what `client: None` means.
async fn adopt_after_config(
    streams: &mut Streams<'_>,
    name: &str,
    path: &Path,
    client: Option<&Client>,
) -> ExitCode {
    let source = DogSource::Adopted {
        path: path.display().to_string(),
    };
    let Some(client) = client else {
        let row = DogAdoptedRow {
            name: name.to_string(),
            source,
            shepherd_acted: false,
            status: NO_SHEPHERD_ENABLE_STATUS.to_string(),
        };
        return write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "adopt",
            row,
            streams.style,
        ));
    };
    let request = Request::EnableDog {
        name: name.to_string(),
        source: source.clone(),
    };
    match client.request(request).await {
        Ok(Response::DogStarted(info)) => {
            let row = DogAdoptedRow {
                name: name.to_string(),
                source,
                shepherd_acted: true,
                status: info.status.to_string(),
            };
            write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                "adopt",
                row,
                streams.style,
            ))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// `shep rehome <name>`: stops an adopted dog and forgets it entirely.
///
/// Two files, in this order: [`ShepToml::rehome_dog`] strikes the
/// registration from `shep.toml`, then
/// [`dog_migration::forget_dog_section`] strikes the configuration from
/// `dogs.toml`. The second is here because [`ShepToml`] owns one file, and
/// the other needs the staged-temp, `fsync` and `rename` path that keeps
/// webhook credentials at `0600`.
pub async fn rehome(streams: &mut Streams<'_>, paths: &ShepPaths, name: &str) -> ExitCode {
    let source = match ShepToml::edit(&paths.daemon_config, |cfg| {
        // Read before `rehome_dog` erases it. `None` is legitimate: a name
        // never adopted, or a built-in dog's own.
        let source = cfg.adopted_dog_path(name).map(|path| DogSource::Adopted {
            path: path.display().to_string(),
        });
        cfg.rehome_dog(name);
        source
    }) {
        Ok(source) => source,
        Err(err) => return fail_config(streams, &err),
    };
    // Non-zero rather than pressed on with: the dog is out of `shep.toml`
    // by now, so a success would claim the webhook URLs were gone with them
    // still on disk. No `config.dog.<name>` frame, unlike the other writers
    // of `dogs.toml`: the daemon half below stops the dog.
    if let Err(err) = dog_migration::forget_dog_section(&paths.dogs_config, name) {
        return fail_dogs_config(streams, &err);
    }
    let client = match connect_or_absent(paths, streams).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    rehome_after_config(streams, name, source, client.as_ref()).await
}

/// `rehome`'s daemon half; see [`enable_after_config`] for the split and
/// for what `client: None` means. Sends the same `DisableDog` request
/// `disable` does: [`rehome`] has already erased the registration `disable`
/// leaves alone.
async fn rehome_after_config(
    streams: &mut Streams<'_>,
    name: &str,
    source: Option<DogSource>,
    client: Option<&Client>,
) -> ExitCode {
    let Some(client) = client else {
        let row = DogRehomedRow {
            name: name.to_string(),
            source,
            shepherd_acted: false,
            status: NO_SHEPHERD_DISABLE_STATUS.to_string(),
        };
        return write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "rehome",
            row,
            streams.style,
        ));
    };
    match client
        .request(Request::DisableDog {
            name: name.to_string(),
        })
        .await
    {
        Ok(Response::Deleted(_ids)) => {
            let row = DogRehomedRow {
                name: name.to_string(),
                source,
                shepherd_acted: true,
                status: DISABLED_STATUS.to_string(),
            };
            write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                "rehome",
                row,
                streams.style,
            ))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// `shep barks`: the alert history, newest last.
///
/// Reads `barks.jsonl` straight off disk and never connects to the
/// shepherd. [`barks::read`] is the forgiving half of that file's contract:
/// a line a writer died mid-append leaves unparseable costs that one record,
/// not the whole read.
///
/// `--tail N` takes the last N records, since [`barks::read`] answers oldest
/// first.
pub fn barks(streams: &mut Streams<'_>, paths: &ShepPaths, args: &BarksArgs) -> ExitCode {
    let mut history = match barks::read(&paths.barks) {
        Ok(history) => history,
        Err(err) => {
            return streams.fail(ExitCode::Failure, &err.to_string());
        }
    };
    if let Some(tail) = args.tail {
        let keep_from = history.len().saturating_sub(tail);
        history.drain(..keep_from);
    }
    write_outcome(emit(
        &mut *streams.out,
        streams.fmt,
        "barks",
        BarkRows(history),
        streams.style,
    ))
}

// `unix` because the adopt-vetting cases read execute bits and the
// world-writable bit. What Windows claims instead is covered by
// `tests/cli_e2e.rs`.
#[cfg(all(test, unix))]
mod tests {
    use shep_client::testing::{
        fake_client_capturing_envelopes, fake_client_replying_err, sample_ack, sample_info,
        serve_one_request,
    };
    use shep_core::protocol::RpcErrorCode;

    use super::*;
    use crate::cli::Format;

    /// Exact strings, not a "contains no secret" probe: the output is half
    /// the derive and half [`ShepTomlError`]'s manual impl, and either alone
    /// could start printing the document.
    #[test]
    fn enable_refusal_debug_never_prints_the_document() {
        let path = std::path::PathBuf::from("/home/ada/.shep/shep.toml");
        let secret = "https://hooks.example.com/services/T00/B00/super-secret-token";
        let broken = format!("[dog.bark]\nwebhook = \"{secret}\"\n[daemon\n");
        let source = broken.parse::<toml_edit::DocumentMut>().unwrap_err();

        let wrong_shape = EnableRefusal::Config(ShepTomlError::WrongShape {
            path: path.clone(),
            key: "style",
            found: "string",
        });
        assert_eq!(
            format!("{wrong_shape:?}"),
            "Config(WrongShape { path: \"/home/ada/.shep/shep.toml\", key: \"style\", \
             found: \"string\" })"
        );

        let parse = EnableRefusal::Config(ShepTomlError::Parse { path, source });
        let debug = format!("{parse:?}");
        assert!(
            !debug.contains(secret),
            "the document must never reach Debug: {debug}"
        );
        assert!(!debug.contains("webhook"), "{debug}");
        assert_eq!(
            debug,
            "Config(Parse { path: \"/home/ada/.shep/shep.toml\", message: \"invalid table \
             header\\nexpected `.`, `]`\" })"
        );

        let unknown = EnableRefusal::UnknownDog {
            adopted: vec!["otel".to_string()],
        };
        assert_eq!(format!("{unknown:?}"), "UnknownDog { adopted: [\"otel\"] }");
    }

    /// Every test here drives a dog verb under `--format table`.
    fn streams<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Streams<'a> {
        Streams {
            out,
            err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        }
    }

    #[test]
    fn enable_in_config_writes_the_name_and_reports_a_built_in_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");

        let source = enable_in_config(&path, "metrics").unwrap();

        assert!(matches!(source, DogSource::BuiltIn));
        assert!(
            ShepToml::read_only(&path)
                .unwrap()
                .enabled_dog_names()
                .contains(&"metrics".to_string())
        );
    }

    #[test]
    fn enable_in_config_refuses_a_name_that_is_no_dog_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "").unwrap();

        let refusal = enable_in_config(&path, "nonsense");

        assert!(matches!(refusal, Err(EnableRefusal::UnknownDog { .. })));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "a refused enable leaves shep.toml untouched"
        );
    }

    #[test]
    fn disable_in_config_removes_the_name_and_keeps_the_adoption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nenabled_dogs = [\"otel\"]\n\n[daemon.adopted_dogs]\notel = \"/usr/local/bin/shep-otel\"\n",
        )
        .unwrap();

        let source = disable_in_config(&path, "otel").unwrap();

        assert!(matches!(source, DogSource::Adopted { .. }));
        let cfg = ShepToml::read_only(&path).unwrap();
        assert!(cfg.enabled_dog_names().is_empty());
        assert_eq!(
            cfg.adopted_dog_names(),
            vec!["otel".to_string()],
            "disable is not rehome, so the adoption survives"
        );
    }

    #[tokio::test]
    async fn enable_asks_the_shepherd_to_start_that_dog_as_a_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = enable_after_config(
            &mut streams(&mut out, &mut err),
            "metrics",
            &DogSource::BuiltIn,
            Some(&client),
        )
        .await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.body,
            Request::EnableDog {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
            }
        );
    }

    /// End to end through `enable`, since the lookup lives in the config
    /// half: [`serve_one_request`] only binds the socket, so `enable` does
    /// its own `Client::connect`.
    #[tokio::test]
    async fn enable_of_an_adopted_dog_sends_the_path_the_config_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        ShepToml::edit(&paths.daemon_config, |seed| {
            seed.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();
        let handle = serve_one_request(
            &paths.socket,
            sample_ack(),
            Response::DogStarted(sample_info()),
        )
        .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(&mut streams(&mut out, &mut err), &paths, "otel").await;

        assert_eq!(code, ExitCode::Success);
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("enable must reach the wire; it hung instead of connecting")
            .unwrap();
        assert_eq!(
            envelope.body,
            Request::EnableDog {
                name: "otel".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            }
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("adopted"),
            "the row must render an adopted dog as adopted: {text}"
        );
    }

    /// A name holding values in both `shep.toml` and `dogs.toml` makes the
    /// migration refuse and the daemon exit 4 with the flock unsupervised.
    #[tokio::test]
    async fn enabling_a_dog_does_not_leave_a_section_that_refuses_the_next_boot() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(&mut streams(&mut out, &mut err), &paths, "metrics").await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        assert!(
            !written.contains("[dog"),
            "enable must write no dog section into shep.toml: {written}"
        );

        // The operator configures the dog where `docs/dogs.md` says to,
        // and the next `shep muster` runs the migration.
        std::fs::write(&paths.dogs_config, "[metrics]\nbind = \"127.0.0.1:9615\"\n").unwrap();
        crate::commands::dog_migration::migrate_dog_sections(&paths)
            .expect("a boot after an enable must not refuse over a section enable wrote");
    }

    #[tokio::test]
    async fn enable_reports_a_refusal_as_a_refusal_not_as_no_shepherd() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        let refusal = shep_core::protocol::RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "this daemon speaks protocol 1, this client speaks 2".to_string(),
            daemon_version: Some("0.1.8".to_string()),
        };
        let _daemon = shep_client::testing::fake_daemon(&paths.socket, Err(refusal)).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(&mut streams(&mut out, &mut err), &paths, "metrics").await;

        assert_ne!(code, ExitCode::Success);
        let text = String::from_utf8(err).unwrap();
        assert!(
            !text.contains(NO_SHEPHERD_ENABLE_STATUS),
            "a refusal is not an absence: {text}"
        );
        assert!(text.contains("shep daemon reload"), "{text}");
    }

    /// A non-zero exit would make `shep enable` unusable in a provisioning
    /// script that configures a host before starting anything.
    #[tokio::test]
    async fn enable_with_no_shepherd_writes_the_config_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(&mut streams(&mut out, &mut err), &paths, "metrics").await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        assert!(
            written.contains("metrics"),
            "the config edit must still land: {written}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("next shepherd"),
            "the operator needs to know the dog is not running yet: {text}"
        );
    }

    #[tokio::test]
    async fn enable_refuses_a_name_that_is_neither_built_in_nor_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(&mut streams(&mut out, &mut err), &paths, "pydog").await;

        assert_eq!(code, ExitCode::InvalidConfig);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("pydog"),
            "the refusal must name the name: {text}"
        );
        assert!(
            text.contains("shep adopt pydog"),
            "the refusal must name the way out, the way `shep adopt`'s own \
             name-collision refusal does: {text}"
        );
        assert!(
            !paths.daemon_config.exists(),
            "a refused enable must leave the config untouched -- `try_edit` \
             skips `save`, so a `$SHEP_HOME` that had no `shep.toml` still \
             has none"
        );
    }

    /// Read under `enable`'s own lock, so a concurrent `shep adopt` cannot
    /// make the message name a set the refusal was never decided against.
    #[tokio::test]
    async fn enable_refusal_names_the_adopted_dogs_alongside_the_built_ins() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        ShepToml::edit(&paths.daemon_config, |cfg| {
            cfg.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(&mut streams(&mut out, &mut err), &paths, "pydog").await;

        assert_eq!(code, ExitCode::InvalidConfig);
        let text = String::from_utf8(err).unwrap();
        for expected in ["\"metrics\"", "\"bark\"", "\"otel\""] {
            assert!(
                text.contains(expected),
                "the refusal must name {expected} among the valid names: {text}"
            );
        }
    }

    #[tokio::test]
    async fn enable_still_accepts_a_name_adopt_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        ShepToml::edit(&paths.daemon_config, |cfg| {
            cfg.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(&mut streams(&mut out, &mut err), &paths, "otel").await;

        assert_eq!(code, ExitCode::Success, "{}", String::from_utf8_lossy(&err));
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        assert!(written.contains("otel"), "{written}");
    }

    /// A hand-written `shep.toml` can carry a name that answers to no dog,
    /// and `shep disable <name>` is the only way back out of `enabled_dogs`.
    #[tokio::test]
    async fn disable_still_removes_a_name_enable_would_now_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(paths.daemon_config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.daemon_config,
            "[daemon]\nenabled_dogs = [\"pydog\", \"metrics\"]\n",
        )
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = disable(&mut streams(&mut out, &mut err), &paths, "pydog").await;

        assert_eq!(code, ExitCode::Success, "{}", String::from_utf8_lossy(&err));
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        assert!(
            !written.contains("pydog"),
            "disable is the escape hatch out of a config enable would now \
             refuse to write: {written}"
        );
        assert!(
            written.contains("metrics"),
            "and it touches nothing else: {written}"
        );
    }

    #[test]
    fn join_with_and_reads_as_an_english_list() {
        let one = ["a".to_string()];
        let two = ["a".to_string(), "b".to_string()];
        let three = ["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(join_with_and(&[]), "");
        assert_eq!(join_with_and(&one), "a");
        assert_eq!(join_with_and(&two), "a and b");
        assert_eq!(join_with_and(&three), "a, b, and c");
    }

    /// `start_dog` is idempotent by name, so an unmarked entry coming back
    /// means a sheep already holds `name`. The operator must see the
    /// daemon's message verbatim, not a bare code.
    #[tokio::test]
    async fn enable_reports_a_name_collision_with_the_daemons_own_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let message =
            "a sheep is already registered as `bark`; rename it or give the dog another name";
        let (client, _daemon) =
            fake_client_replying_err(&path, RpcErrorCode::InvalidConfig, message).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable_after_config(
            &mut streams(&mut out, &mut err),
            "bark",
            &DogSource::BuiltIn,
            Some(&client),
        )
        .await;

        assert_eq!(code, ExitCode::InvalidConfig);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains(message),
            "the daemon's own message must reach the operator: {text}"
        );
    }

    #[tokio::test]
    async fn disable_asks_the_shepherd_to_stop_that_dog() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = disable_after_config(
            &mut streams(&mut out, &mut err),
            "bark",
            &DogSource::BuiltIn,
            Some(&client),
        )
        .await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.body,
            Request::DisableDog {
                name: "bark".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn disable_with_no_shepherd_writes_the_config_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        ShepToml::edit(&paths.daemon_config, |seed| seed.enable_dog("bark")).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = disable(&mut streams(&mut out, &mut err), &paths, "bark").await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        let cfg = shep_core::config::DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(
            cfg.daemon.enabled_dogs.is_empty(),
            "disable must remove the name from enabled_dogs: {written}"
        );
    }

    /// `disable` reuses `Delete`'s own selector path, so a dog not
    /// registered answers `NotFound` as `shep stop` would.
    #[tokio::test]
    async fn disable_of_a_dog_the_shepherd_does_not_have_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _daemon) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = disable_after_config(
            &mut streams(&mut out, &mut err),
            "ghost",
            &DogSource::BuiltIn,
            Some(&client),
        )
        .await;
        assert_eq!(code, ExitCode::NotFound);
    }

    fn chmod(path: &Path, mode: u32) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, mode);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// Each refusal must name its own cause: "not executable" for a missing
    /// path sends an operator to `chmod` a file that is not there.
    #[test]
    fn a_binary_shep_has_never_seen_is_vetted_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            vet_binary_within(&dir.path().join("nope"), dir.path(), "probe", TEST_BUDGET),
            Err(AdoptRefusal::Missing)
        );
        assert_eq!(
            vet_binary_within(dir.path(), dir.path(), "probe", TEST_BUDGET),
            Err(AdoptRefusal::NotAFile)
        );

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            vet_binary_within(&plain, dir.path(), "probe", TEST_BUDGET),
            Err(AdoptRefusal::NotExecutable)
        );

        // The same file, now executable: only the mode bit changed, so a
        // refusal for another reason fails here.
        chmod(&plain, 0o755);
        let vetted = vet_binary_within(&plain, dir.path(), "probe", TEST_BUDGET).unwrap();
        assert_eq!(vetted.path, plain.canonicalize().unwrap());
        assert!(
            vetted.group_writable.is_empty(),
            "an 0o755 binary in an 0o700 directory has nothing to warn about: {vetted:?}"
        );

        // Executable, and not something this kernel can run.
        let bogus = dir.path().join("bogus");
        std::fs::write(&bogus, b"\x7fELF\x00\x00\x00 not really").unwrap();
        chmod(&bogus, 0o755);
        assert!(matches!(
            vet_binary_within(&bogus, dir.path(), "probe", TEST_BUDGET),
            Err(AdoptRefusal::WillNotExec { .. })
        ));
    }

    #[test]
    fn a_binary_any_user_can_rewrite_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("dog");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        chmod(&bin, 0o757);
        assert_eq!(
            vet_binary_within(&bin, dir.path(), "probe", TEST_BUDGET),
            Err(AdoptRefusal::WorldWritable {
                path: bin.canonicalize().unwrap(),
            }),
            "a world-writable binary must be refused"
        );

        // The file is now sound; the directory holding it is not.
        chmod(&bin, 0o755);
        chmod(dir.path(), 0o777);
        assert_eq!(
            vet_binary_within(&bin, dir.path(), "probe", TEST_BUDGET),
            Err(AdoptRefusal::WorldWritable {
                path: bin.canonicalize().unwrap().parent().unwrap().to_path_buf(),
            }),
            "a world-writable directory must be refused too"
        );
        // Restored so the tempdir cleans up from a known state.
        chmod(dir.path(), 0o700);
    }

    /// Writes an executable `/bin/sh` script at `dir/name` that dispatches
    /// on `$1`: `version_body` for the version flag, `schema_body` for the
    /// schema flag, nothing for any other argument.
    ///
    /// A fixture answering every flag the same way answers the schema flag
    /// with unreadable JSON, so every other test would carry that warning.
    fn probe_script(dir: &Path, name: &str, version_body: &str, schema_body: &str) -> PathBuf {
        let path = dir.join(name);
        let body = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version)\n{version_body}\n;;\n\
             --schema)\n{schema_body}\n;;\nesac\n"
        );
        std::fs::write(&path, body).unwrap();
        chmod(&path, 0o755);
        path
    }

    /// A dog that answers the version flag with `body` and the schema flag
    /// with nothing.
    fn dog_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        probe_script(dir, name, body, "exit 0")
    }

    /// Adopting one would make an online-and-idle entry whose failure
    /// surfaces days later, at a handshake nobody is watching.
    #[tokio::test]
    async fn a_dog_that_speaks_another_protocol_is_refused_at_adopt() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let stale = PROTOCOL_VERSION + 1;
        let bin = dog_script(
            dir.path(),
            "shep-otel",
            &format!("echo 'shep-otel 0.1.3'\necho 'shep-protocol: {stale}'"),
        );

        assert_eq!(
            vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET),
            Err(AdoptRefusal::ProtocolMismatch {
                dog: stale,
                shep: PROTOCOL_VERSION,
            })
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::InvalidConfig);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains(&stale.to_string()) && text.contains(&PROTOCOL_VERSION.to_string()),
            "the refusal names both numbers: {text}"
        );
        assert!(
            text.contains("--locked") && text.contains("run a shep that speaks"),
            "the refusal names both fixes, and picks neither: {text}"
        );
        assert!(
            !paths.daemon_config.exists(),
            "a refused adopt must not write shep.toml"
        );
    }

    /// A third-party dog's crate version has no relationship to shep's own,
    /// so it is reported and never compared.
    #[tokio::test]
    async fn a_dog_whose_version_is_nothing_like_sheps_is_still_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let bin = dog_script(
            dir.path(),
            "shep-otel",
            &format!("echo 'shep-otel 9.9.9-rc1'\necho 'shep-protocol: {PROTOCOL_VERSION}'"),
        );

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(
            vetted.answer,
            Some(DogVersion {
                version: "9.9.9-rc1".to_string(),
                protocol: Some(PROTOCOL_VERSION),
            })
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(
            code,
            ExitCode::Success,
            "a version difference is not a refusal"
        );
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("9.9.9-rc1"),
            "the version it answered is reported: {text}"
        );
    }

    /// Refusing silence would break every dog that predates the contract,
    /// so detection falls back to the handshake for that dog alone.
    #[tokio::test]
    async fn a_dog_that_does_not_answer_is_adopted_with_an_unknown_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let bin = dog_script(dir.path(), "shep-otel", "exit 0");

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(vetted.answer, None, "silence is an unknown, not an answer");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(
            code,
            ExitCode::Success,
            "a dog predating the contract is adoptable"
        );
        let cfg = std::fs::read_to_string(&paths.daemon_config).unwrap();
        assert!(cfg.contains("otel"), "and it is recorded: {cfg}");
    }

    /// A clap-built dog prints `<name> <version>` and never mentions
    /// `shep-protocol`: unknown, not mismatched.
    #[tokio::test]
    async fn a_dog_that_names_no_protocol_is_adopted_with_the_version_it_gave() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let bin = dog_script(dir.path(), "shep-otel", "echo 'shep-otel 0.1.3'");

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(
            vetted.answer,
            Some(DogVersion {
                version: "0.1.3".to_string(),
                protocol: None,
            })
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::Success);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("0.1.3") && text.contains("unknown"),
            "the operator hears the version and that the protocol is unknown: {text}"
        );
    }

    /// A budget for tests that are not about the budget.
    ///
    /// The probe spawns a real child, so every test reaching `vet_binary`
    /// inherits a wall-clock bound, and at the production one second a test
    /// asking whether a version string parses fails on a busy machine.
    const TEST_BUDGET: Duration = Duration::from_secs(30);

    /// `CARGO_PKG_NAME` is the sentinel: cargo sets it for this process and
    /// it is not on the daemon's allowlist, so a leak shows up as the child
    /// seeing a variable this test never gave it, with no environment
    /// mutated.
    #[cfg(unix)]
    #[test]
    fn a_probe_runs_with_the_daemons_environment_and_not_the_operators() {
        assert!(
            std::env::var("CARGO_PKG_NAME").is_ok(),
            "the sentinel has to be in this process for its absence downstream to mean anything"
        );
        let dir = tempfile::tempdir().unwrap();
        // The sentinel must be the last field of line 1, the only part
        // `parse_version_answer` keeps; anywhere else and the test passes
        // with `env_clear` removed.
        let bin = dog_script(
            dir.path(),
            "shep-otel",
            "echo \"shep-otel ${CARGO_PKG_NAME:-clean}\"",
        );

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        let version = vetted.answer.expect("the candidate answered").version;

        assert_eq!(
            version, "clean",
            "the operator's environment reached a candidate: it saw CARGO_PKG_NAME"
        );
    }

    /// The assertion on elapsed time is what turns a blocking `child.wait()`
    /// into a reported failure rather than a hang.
    #[test]
    fn a_candidate_that_never_exits_does_not_hang_the_vet() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dog_script(dir.path(), "shep-otel", "sleep 30");

        let started = std::time::Instant::now();
        // The real budget, not `TEST_BUDGET`: this test is the bound.
        let vetted = vet_binary_within(&bin, dir.path(), "otel", VERSION_BUDGET).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(
            vetted.answer, None,
            "a candidate that never answered is unknown"
        );
        // Absolute, not a multiple of `VERSION_BUDGET`, so the bound stays
        // wrong when the budget is what got mutated. Ten seconds against a
        // candidate that sleeps thirty.
        assert!(
            elapsed < Duration::from_secs(10),
            "the vet is bounded by its own budget, not by the candidate: {elapsed:?}"
        );
    }

    /// Twice the cap is written, so a read that stopped anywhere else shows
    /// up in the length. `trap '' PIPE` lets the script reach its own `exit
    /// 0` after shep closes the pipe: a candidate killed by SIGPIPE answers
    /// `None` for its exit status and says nothing about where the read
    /// stopped.
    #[test]
    fn a_candidate_that_will_not_stop_talking_is_read_no_further_than_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = "a".repeat(1024);
        let chunks = PROBE_OUTPUT_LIMIT / 1024 * 2;
        let bin = probe_script(
            dir.path(),
            "shep-otel",
            &format!(
                "trap '' PIPE\ni=0\nwhile [ $i -lt {chunks} ]; do \
                 printf '%s' '{chunk}'; i=$((i+1)); done\nexit 0"
            ),
            "exit 0",
        );

        let answer = ask(&bin, VERSION_FLAG, dir.path(), "otel", TEST_BUDGET)
            .expect("what a candidate prints is never a refusal")
            .expect("it exited 0, so it answered");

        assert_eq!(
            answer.len() as u64,
            PROBE_OUTPUT_LIMIT,
            "the read stops at the cap, whatever the candidate does after it"
        );
    }

    /// `docs/dogs.md` asks a dog to answer on stdout and exit 0, so lines
    /// from a run that then failed cannot refuse an adopt.
    #[test]
    fn an_answer_from_a_failed_run_is_not_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let stale = PROTOCOL_VERSION + 1;
        let bin = dog_script(
            dir.path(),
            "shep-otel",
            &format!("echo 'shep-otel 0.1.3'\necho 'shep-protocol: {stale}'\nexit 3"),
        );

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(vetted.answer, None);
    }

    /// shep has no standing to refuse a dog over the shape of text it never
    /// promised to print.
    #[test]
    fn output_that_answers_nothing_shep_asked_is_not_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dog_script(
            dir.path(),
            "shep-otel",
            "echo 'error: unrecognized option --version'",
        );

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(
            vetted.answer,
            Some(DogVersion {
                version: "--version".to_string(),
                protocol: None,
            }),
            "the last field of line 1 is taken as the version, whatever it is"
        );
    }

    /// What `docs/dogs.md` says the parser tolerates: an ignored name on
    /// line 1, blank lines, key order, unknown keys, and a reserved `shep-`
    /// key that does not exist yet. A shep that predates a third number must
    /// ignore its line rather than refuse the dog.
    #[test]
    fn a_future_shep_key_is_ignored_rather_than_breaking_the_parser() {
        let answer = parse_version_answer(
            "some-other-crate-name 0.4.0\n\nshep-channel: 7\nother: whatever\n\
             shep-protocol: 2\nshep-lambs: 9\n",
        );
        assert_eq!(
            answer,
            Some(DogVersion {
                version: "0.4.0".to_string(),
                protocol: Some(2),
            })
        );

        assert_eq!(parse_version_answer(""), None, "no output is no answer");
        assert_eq!(
            parse_version_answer("shep-otel 0.1.3\nshep-protocol: two\n"),
            Some(DogVersion {
                version: "0.1.3".to_string(),
                protocol: None,
            }),
            "a protocol that is not a decimal is unknown, not a refusal"
        );
    }

    /// A dog whose schema half is what the test is about; the version half
    /// always names this shep's own protocol.
    fn two_flag_dog(dir: &Path, schema_body: &str) -> PathBuf {
        probe_script(
            dir,
            "shep-otel",
            &format!("echo 'shep-otel 0.1.3'\necho 'shep-protocol: {PROTOCOL_VERSION}'"),
            schema_body,
        )
    }

    #[test]
    fn a_dog_that_answers_a_schema_has_it_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = two_flag_dog(
            dir.path(),
            "echo '{\"title\":\"otel\",\"properties\":{\"endpoint\":{\"type\":\"string\"}}}'",
        );

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();

        let DogSchema::Published(schema) = &vetted.schema else {
            panic!("a dog that printed valid JSON Schema has one: {vetted:?}");
        };
        assert_eq!(
            schema["properties"]["endpoint"]["type"], "string",
            "the schema is kept as the dog wrote it, not summarised"
        );
    }

    /// A dog with a broken schema flag may still do its job.
    #[tokio::test]
    async fn a_dog_whose_schema_run_exits_non_zero_has_no_schema_and_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let bin = two_flag_dog(dir.path(), "echo '{}'; exit 3");

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(
            vetted.schema,
            DogSchema::Silent,
            "a failed run printed no answer, whatever reached stdout"
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::Success, "no schema is not a refusal");
        let text = String::from_utf8(err).unwrap();
        assert!(
            !text.contains(DOG_SCHEMA_UNREADABLE_NOTICE),
            "only unreadable output earns the warning, not a failed run: {text}"
        );
    }

    /// Every dog written before this contract is silent, and a warning for
    /// each is a line about the ordinary case.
    #[tokio::test]
    async fn a_dog_that_prints_no_schema_has_no_schema_and_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let bin = two_flag_dog(dir.path(), "exit 0");

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(vetted.schema, DogSchema::Silent, "silence is no schema");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::Success, "silence is not a refusal");
        let text = String::from_utf8(err).unwrap();
        assert!(
            !text.contains(DOG_SCHEMA_UNREADABLE_NOTICE),
            "a dog that answered nothing is the ordinary case, not a warning: {text}"
        );
    }

    /// The dog meant to answer and its answer cannot be read: a bug its
    /// author can fix. Exactly one warning, because the count is what an
    /// operator reads.
    #[tokio::test]
    async fn a_dog_that_prints_invalid_json_is_adopted_with_one_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let bin = two_flag_dog(dir.path(), "echo 'error: unrecognized option --schema'");

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(
            vetted.schema,
            DogSchema::Unreadable,
            "output that is not JSON is unreadable, not absent"
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::Success, "a broken schema is not a refusal");
        let text = String::from_utf8(err).unwrap();
        assert_eq!(
            text.matches(&format!("notice[{DOG_SCHEMA_UNREADABLE_NOTICE}]"))
                .count(),
            1,
            "one warning, and only one: {text}"
        );
        assert!(
            paths.daemon_config.exists(),
            "the dog is adopted despite the warning"
        );
    }

    /// Every file under `dir`, recursively, as text. A non-UTF-8 file is
    /// skipped rather than failing the walk.
    fn every_file_under(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(every_file_under(&path));
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                found.push((path, text));
            }
        }
        found
    }

    /// `cargo install` replaces a dog's binary with nothing watching, and a
    /// stale schema mislabels which field is a credential. Asserts on the
    /// files, walking the whole home; the dog's binary lives in its own
    /// directory because the fixture script contains the schema it prints.
    #[tokio::test]
    async fn a_published_schema_reaches_no_file_shep_writes() {
        let home = tempfile::tempdir().unwrap();
        let binaries = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, home.path());
        let marker = "only-ever-in-the-schema";
        let bin = two_flag_dog(
            binaries.path(),
            &format!("echo '{{\"title\":\"{marker}\",\"properties\":{{}}}}'"),
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;
        assert_eq!(code, ExitCode::Success);

        let written = every_file_under(home.path());
        assert!(
            written.iter().any(|(_, text)| text.contains("otel")),
            "the adopt has to have written the dog somewhere for this to mean anything: {written:?}"
        );
        for (path, text) in &written {
            assert!(
                !text.contains(marker),
                "the schema was stored in {}: {text}",
                path.display()
            );
        }
    }

    /// A `default` in the schema carries the dog author's own `Default`.
    /// Exact string: the failure mode is a derive replacing the impl.
    #[test]
    fn debug_reports_that_there_is_a_schema_and_never_what_is_in_it() {
        let schema = DogSchema::Published(serde_json::json!({"token": "hunter2"}));
        assert_eq!(format!("{schema:?}"), "Published(..)");
        assert_eq!(format!("{:?}", DogSchema::Silent), "Silent");
        assert_eq!(format!("{:?}", DogSchema::Unreadable), "Unreadable");
    }

    /// A deployment directory owned by a trusted deploy group is legitimate,
    /// and this command is the operator's only chance to hear about it.
    #[tokio::test]
    async fn a_group_writable_binary_is_adopted_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let deploy = dir.path().join("deploy");
        std::fs::create_dir(&deploy).unwrap();
        let bin = deploy.join("shep-otel");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        chmod(&bin, 0o775);
        chmod(&deploy, 0o775);

        let vetted = vet_binary_within(&bin, dir.path(), "otel", TEST_BUDGET).unwrap();
        assert_eq!(
            vetted.group_writable,
            vec![bin.canonicalize().unwrap(), deploy.canonicalize().unwrap()],
            "both the binary and its directory are group-writable"
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: bin.clone(),
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::Success, "group-writable is a warning");
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains(&bin.canonicalize().unwrap().display().to_string()),
            "the warning names the path: {text}"
        );
        assert!(
            text.contains("group"),
            "the warning says what the risk is: {text}"
        );
    }

    /// The vetting is worth nothing if the config records the binary anyway.
    #[tokio::test]
    async fn a_refused_adopt_leaves_the_config_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: dir.path().join("nope"),
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::InvalidConfig);
        assert!(
            !paths.daemon_config.exists(),
            "a refused adopt must never touch shep.toml: {}",
            paths.daemon_config.display()
        );
    }

    /// `adopt`'s own report is the only place an operator learns which
    /// refusal mode it was.
    #[tokio::test]
    async fn adopt_of_a_missing_binary_reports_the_refusal_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: dir.path().join("nope"),
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::InvalidConfig);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("no file exists at that path"),
            "the refusal must reach the operator: {text}"
        );
    }

    #[tokio::test]
    async fn adopt_asks_the_shepherd_to_start_that_dog_with_its_adopted_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let binary = PathBuf::from("/usr/local/bin/shep-otel");
        let _ = adopt_after_config(
            &mut streams(&mut out, &mut err),
            "otel",
            &binary,
            Some(&client),
        )
        .await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.body,
            Request::EnableDog {
                name: "otel".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            }
        );
    }

    #[tokio::test]
    async fn adopt_with_no_shepherd_writes_the_config_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let binary = dir.path().join("shep-otel");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        let mut mode = std::fs::metadata(&binary).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&binary, mode).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            name: Some("otel".to_string()),
            path: binary,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        assert!(
            written.contains("otel"),
            "the config edit must still land: {written}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("next shepherd"),
            "the operator needs to know the dog is not running yet: {text}"
        );
    }

    #[tokio::test]
    async fn adopt_with_no_name_flag_defaults_from_the_stripped_stem() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let binary = dir.path().join("shep-otel");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        let mut mode = std::fs::metadata(&binary).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&binary, mode).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            path: binary,
            name: None,
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        let cfg = shep_core::config::DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(
            cfg.daemon.adopted_dogs.contains_key("otel"),
            "the defaulted name must be `otel`, not `shep-otel`: {written}"
        );
    }

    /// `dispatch_adopted_dog` (`lib.rs`) runs only once clap has failed to
    /// match the name against a real subcommand, so such a dog is
    /// unreachable. The refusal must precede any `shep.toml` write.
    #[tokio::test]
    async fn adopt_refuses_a_name_that_collides_with_a_built_in_verb() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let binary = dir.path().join("watchdog");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        let mut mode = std::fs::metadata(&binary).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&binary, mode).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        // "stop" is a real verb; "ls" is `flock`'s own visible alias.
        for reserved in ["stop", "ls"] {
            let args = AdoptArgs {
                path: binary.clone(),
                name: Some(reserved.to_string()),
            };
            let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;
            assert_eq!(
                code,
                ExitCode::InvalidConfig,
                "`{reserved}` must be refused"
            );
        }
        assert!(
            !paths.daemon_config.exists(),
            "a name collision must never touch shep.toml: {}",
            paths.daemon_config.display()
        );
    }

    /// The outcome is the same whichever order runs, so the two are told
    /// apart by the refusal: `args.path` names nothing on disk, so vet-first
    /// would give "no file exists at that path".
    #[tokio::test]
    async fn a_name_collision_is_refused_before_vet_binary_ever_runs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = AdoptArgs {
            path: dir.path().join("nope"),
            name: Some("stop".to_string()),
        };
        let code = adopt(&mut streams(&mut out, &mut err), &paths, &args).await;

        assert_eq!(code, ExitCode::InvalidConfig);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains("already a shep verb or alias"),
            "the collision must be the reported reason, not vet_binary's own refusal: {text}"
        );
        assert!(
            !text.contains("no file exists at that path"),
            "vet_binary must never run on a name that was always going to be refused: {text}"
        );
    }

    #[test]
    fn resolve_adopt_path_prefers_a_literal_path_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("thing");
        std::fs::write(&binary, "").unwrap();

        let resolved = resolve_adopt_path(&binary, None, None);
        assert_eq!(resolved, binary);
    }

    #[test]
    fn resolve_adopt_path_expands_a_leading_tilde_against_the_given_home() {
        let home = tempfile::tempdir().unwrap();
        let binary_dir = home.path().join(".cargo/bin");
        std::fs::create_dir_all(&binary_dir).unwrap();
        let binary = binary_dir.join("shep-log-rotate");
        std::fs::write(&binary, "").unwrap();

        let raw = Path::new("~/.cargo/bin/shep-log-rotate");
        let resolved = resolve_adopt_path(raw, Some(home.path()), None);
        assert_eq!(resolved, binary);
    }

    /// `cargo install shep-log-rotate` puts the binary on `$PATH` under its
    /// own name, and nowhere else.
    #[test]
    fn resolve_adopt_path_falls_back_to_a_path_lookup_for_a_bare_name() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("shep-log-rotate");
        std::fs::write(&binary, "").unwrap();
        let mut mode = std::fs::metadata(&binary).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&binary, mode).unwrap();
        let path_var = std::ffi::OsString::from(dir.path());

        let raw = Path::new("shep-log-rotate");
        let resolved = resolve_adopt_path(raw, None, Some(&path_var));
        assert_eq!(resolved, binary);
    }

    /// A same-named file elsewhere on `$PATH` would silently adopt the
    /// wrong binary.
    #[test]
    fn resolve_adopt_path_does_not_path_search_a_name_with_a_directory_component() {
        let path_dir = tempfile::tempdir().unwrap();
        // A file that would match if `$PATH` were searched, so the guard is
        // what this proves rather than an absent file.
        let decoy = path_dir.path().join("thing");
        std::fs::write(&decoy, "").unwrap();
        let mut mode = std::fs::metadata(&decoy).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&decoy, mode).unwrap();
        let path_var = std::ffi::OsString::from(path_dir.path());

        let raw = Path::new("./thing");
        let resolved = resolve_adopt_path(raw, None, Some(&path_var));
        assert_eq!(
            resolved, raw,
            "a name with its own directory must never be searched on $PATH"
        );
    }

    /// Keeps a plain missing path reporting [`AdoptRefusal::Missing`]
    /// rather than a resolution-specific error.
    #[test]
    fn resolve_adopt_path_returns_raw_unchanged_when_nothing_resolves() {
        let raw = Path::new("/nonexistent/shep-nothing");
        assert_eq!(resolve_adopt_path(raw, None, None), raw);
    }

    #[test]
    fn default_dog_name_strips_one_leading_shep_prefix_and_no_further() {
        assert_eq!(
            default_dog_name(Path::new("/opt/bin/shep-log-rotate")),
            "log-rotate"
        );
        assert_eq!(default_dog_name(Path::new("/opt/bin/otel")), "otel");
        // Stripping the prefix here would leave "", an unreachable name,
        // so the whole stem is kept.
        assert_eq!(default_dog_name(Path::new("/opt/bin/shep-")), "shep-");
    }

    #[test]
    fn collides_with_a_verb_covers_names_and_visible_aliases() {
        assert!(collides_with_a_verb("stop"), "a real verb must collide");
        assert!(collides_with_a_verb("ls"), "flock's own alias must collide");
        assert!(
            !collides_with_a_verb("watchdog"),
            "an arbitrary name must not collide"
        );
    }

    /// The configuration half is two files: a `[dog.otel]` an un-migrated
    /// `shep.toml` still carries, and the `[otel]` in `dogs.toml` where one
    /// lives now. `metrics` is beside it to catch a rewrite that forgets
    /// more than it was asked to.
    #[tokio::test]
    async fn rehome_forgets_everything_disable_deliberately_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        ShepToml::edit(&paths.daemon_config, |seed| {
            seed.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();
        std::fs::write(
            &paths.dogs_config,
            "[otel]\nendpoint = \"127.0.0.1:4317\"\n\n[metrics]\nbind = \"127.0.0.1:9615\"\n",
        )
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(&mut streams(&mut out, &mut err), &paths, "otel").await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        let cfg = shep_core::config::DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(
            cfg.daemon.enabled_dogs.is_empty(),
            "rehome must remove the name from enabled_dogs: {written}"
        );
        assert!(
            !cfg.daemon.adopted_dogs.contains_key("otel"),
            "rehome must forget the adopted_dogs entry disable deliberately keeps: {written}"
        );
        assert!(
            !cfg.dog.contains_key("otel"),
            "rehome must remove [dog.otel] too, unlike disable: {written}"
        );
        let dogs = std::fs::read_to_string(&paths.dogs_config).unwrap();
        let dogs = shep_core::config::DogsConfig::load(Some(&dogs)).unwrap();
        assert!(
            !dogs.dog.contains_key("otel"),
            "rehome must strike the section from dogs.toml, where a dog's config lives now"
        );
        assert!(
            dogs.dog.contains_key("metrics"),
            "and must leave every other dog's section exactly where it was"
        );
    }

    /// `dogs.toml` holds webhook URLs, so it is `0600` and the rewrite
    /// installs a staged inode carrying that mode rather than trusting the
    /// mode it found.
    #[tokio::test]
    async fn rehoming_narrows_a_world_readable_dogs_toml() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        ShepToml::edit(&paths.daemon_config, |seed| {
            seed.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();
        std::fs::write(
            &paths.dogs_config,
            "[otel]\nendpoint = \"127.0.0.1:4317\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&paths.dogs_config, std::fs::Permissions::from_mode(0o644))
            .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(&mut streams(&mut out, &mut err), &paths, "otel").await;

        assert_eq!(code, ExitCode::Success);
        let mode = std::fs::metadata(&paths.dogs_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:o}");
    }

    /// Rehoming a dog nobody ever configured must not invent an empty
    /// `dogs.toml` or fail over its absence.
    #[tokio::test]
    async fn rehoming_with_no_dogs_toml_at_all_writes_none() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        ShepToml::edit(&paths.daemon_config, |seed| {
            seed.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(&mut streams(&mut out, &mut err), &paths, "otel").await;

        assert_eq!(code, ExitCode::Success);
        assert!(
            !paths.dogs_config.exists(),
            "nothing to strike, nothing written"
        );
    }

    #[tokio::test]
    async fn rehome_asks_the_shepherd_to_stop_that_dog() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = rehome_after_config(
            &mut streams(&mut out, &mut err),
            "otel",
            Some(DogSource::Adopted {
                path: "/usr/local/bin/shep-otel".to_string(),
            }),
            Some(&client),
        )
        .await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.body,
            Request::DisableDog {
                name: "otel".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn rehome_with_no_shepherd_writes_the_config_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        ShepToml::edit(&paths.daemon_config, |seed| {
            seed.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(&mut streams(&mut out, &mut err), &paths, "otel").await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        let cfg = shep_core::config::DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(cfg.daemon.enabled_dogs.is_empty());
        assert!(!cfg.daemon.adopted_dogs.contains_key("otel"));
    }

    use shep_core::barks::{self, Bark, SinkOutcome};

    /// A bark named `subject`, at `at_ms`, delivered to one sink named
    /// `ops`; every other field fixed.
    fn bark_for(subject: &str, at_ms: u64) -> Bark {
        Bark {
            at_ms,
            rule: "watchdog".to_string(),
            subject: subject.to_string(),
            message: "restart budget exhausted".to_string(),
            sinks: vec![SinkOutcome {
                sink: "ops".to_string(),
                error: None,
            }],
        }
    }

    /// `#[test]`, not `#[tokio::test]`: `barks` answers from a file, with
    /// no socket in reach.
    #[test]
    fn barks_renders_the_ring_newest_last_with_no_client_anywhere_in_reach() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        barks::append(&paths.barks, &bark_for("web", 1), barks::DEFAULT_MAX_BYTES).unwrap();
        barks::append(
            &paths.barks,
            &bark_for("worker", 2),
            barks::DEFAULT_MAX_BYTES,
        )
        .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = barks(
            &mut streams(&mut out, &mut err),
            &paths,
            &BarksArgs { tail: None },
        );

        assert_eq!(code, ExitCode::Success);
        let text = String::from_utf8(out).unwrap();
        let web_at = text.find("web").expect("the older bark must be rendered");
        let worker_at = text
            .find("worker")
            .expect("the newer bark must be rendered");
        assert!(
            web_at < worker_at,
            "newest last: web (older) must render before worker (newer): {text}"
        );
    }

    #[test]
    fn tail_shows_only_the_most_recent_n_barks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        for (subject, at_ms) in [("first", 1), ("second", 2), ("third", 3)] {
            barks::append(
                &paths.barks,
                &bark_for(subject, at_ms),
                barks::DEFAULT_MAX_BYTES,
            )
            .unwrap();
        }

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = barks(
            &mut streams(&mut out, &mut err),
            &paths,
            &BarksArgs { tail: Some(2) },
        );

        assert_eq!(code, ExitCode::Success);
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("first"),
            "--tail 2 must drop the oldest of three: {text}"
        );
        assert!(text.contains("second"), "{text}");
        assert!(text.contains("third"), "{text}");
    }

    /// `history.len().saturating_sub(tail)`'s reason for being `saturating`.
    #[test]
    fn tail_larger_than_the_ring_shows_everything() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        barks::append(&paths.barks, &bark_for("web", 1), barks::DEFAULT_MAX_BYTES).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = barks(
            &mut streams(&mut out, &mut err),
            &paths,
            &BarksArgs { tail: Some(50) },
        );

        assert_eq!(code, ExitCode::Success);
        assert!(String::from_utf8(out).unwrap().contains("web"));
    }

    /// `barks::read` answers `Ok(vec![])` rather than an I/O error, and
    /// `dogs::barks` must still exit `Success` and print headers.
    #[test]
    fn no_ring_file_yet_is_an_empty_history_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = barks(
            &mut streams(&mut out, &mut err),
            &paths,
            &BarksArgs { tail: None },
        );

        assert_eq!(code, ExitCode::Success);
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("WHEN"),
            "an empty history still prints its header row: {text}"
        );
    }

    /// The tolerance lives in `barks::read`; this is `dogs::barks`' own
    /// proof that nothing between here and there swallows it.
    #[test]
    fn a_corrupt_trailing_line_costs_one_record_not_the_whole_read() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.home).unwrap();
        barks::append(&paths.barks, &bark_for("web", 1), barks::DEFAULT_MAX_BYTES).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&paths.barks)
            .unwrap()
            .write_all(b"{\"at_ms\": 2, \"rul\n")
            .unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = barks(
            &mut streams(&mut out, &mut err),
            &paths,
            &BarksArgs { tail: None },
        );

        assert_eq!(code, ExitCode::Success);
        assert!(String::from_utf8(out).unwrap().contains("web"));
    }
}
