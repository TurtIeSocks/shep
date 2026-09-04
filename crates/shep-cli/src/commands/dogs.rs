//! `shep enable`/`shep disable`/`shep adopt`/`shep rehome`: the operator
//! verbs that turn a registered dog on and off, and register or forget a
//! third-party one.
//!
//! None of the four takes an already-connected [`Client`], unlike every
//! verb in `commands::lifecycle`/`commands::query`: all four must still do
//! useful work against a `$SHEP_HOME` with no shepherd running at all, so
//! `main` dispatches them straight off the resolved [`ShepPaths`] rather
//! than through `connect_client`, and each one attempts its own connection
//! here, tolerating a failure to reach one.
//!
//! **The order is config first, then the daemon — for `enable`/`disable`/
//! `rehome`.** [`ShepToml::edit`] runs before any of the three ever tries
//! the socket: if the RPC that follows fails or never gets attempted, the
//! config still says what the operator asked for, and the next boot brings
//! it up — which is the state the operator actually wanted. The reverse
//! order would leave a dog running (or stopped) that no boot restores.
//!
//! **`adopt` reverses that order for its own first step.** [`vet_binary`]
//! runs BEFORE `shep.toml` is touched at all — a refused adopt must leave
//! the config exactly as it was, because there is something here `enable`
//! structurally cannot have: a binary that might not exist, might not be a
//! file, might have no execute bit, might sit somewhere any user can
//! rewrite it, or might be something this kernel cannot run. Once vetting
//! passes, `adopt` rejoins the same config-first order as the other three.
//!
//! **None of the four autostarts a shepherd** — decision 11. Each, against
//! no running daemon, writes the config, reports what will happen with the
//! next shepherd, and exits [`ExitCode::Success`]. Autostarting a whole
//! supervisor as a side effect of a config edit would be a surprise out of
//! proportion to the ask; `shep muster` is the one verb that autostarts,
//! and it says so in its own help text.

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

/// [`DogEnabledRow::status`] when `enable` wrote the config but no shepherd
/// answered — decision 11: `enable` never autostarts one to act on its own
/// edit, so this IS the success outcome, not a partial one.
const NO_SHEPHERD_ENABLE_STATUS: &str = "will start with the next shepherd";

/// [`DogDisabledRow::status`] when `disable` wrote the config but no
/// shepherd answered — the mirror of [`NO_SHEPHERD_ENABLE_STATUS`].
const NO_SHEPHERD_DISABLE_STATUS: &str = "not running; will not start with the next shepherd";

/// [`DogDisabledRow::status`] when a shepherd stopped the dog.
const DISABLED_STATUS: &str = "stopped";

/// Renders `err` and returns the exit code a config-write failure reports.
///
/// [`ShepTomlError::Parse`] and [`ShepTomlError::WrongShape`] are both
/// config-validation failures — the same category [`ExitCode::InvalidConfig`]
/// names for a bad Flockfile (`commands::lifecycle::target_exit_code`) —
/// while [`ShepTomlError::Io`] has no more specific code than
/// [`ExitCode::Failure`]. `WrongShape` is unreachable from every call site
/// in this file today (`enable_dog`/`disable_dog`/`adopt_dog`/`rehome_dog`
/// still panic on the shape that produces it — a tracked follow-up, not
/// this match's problem); it is handled here only because `ShepTomlError`
/// is deliberately not `#[non_exhaustive]`, so this match must cover
/// every variant the type has, not just the ones this file's own callers
/// can currently produce.
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
/// The same split `fail_config` makes, and for the same reason. A file
/// that will not parse is the operator's to fix
/// ([`ExitCode::InvalidConfig`]); everything else is I/O this process
/// could not complete ([`ExitCode::Failure`]). Wildcarded rather than
/// matched arm by arm: [`DogMigrationError`] is `#[non_exhaustive]`
/// because the migration keeps meeting shapes nobody predicted, and a new
/// variant should land as a plain failure here rather than as a compile
/// error in a verb that does not know what it means.
fn fail_dogs_config(streams: &mut Streams<'_>, err: &DogMigrationError) -> ExitCode {
    let code = match err {
        DogMigrationError::Parse(_) => ExitCode::InvalidConfig,
        _ => ExitCode::Failure,
    };
    streams.fail(code, &err.to_string())
}

/// Where `name`'s binary comes from, according to `cfg`.
///
/// A name present in `[daemon] adopted_dogs` is an adopted dog, and the
/// path recorded there is its binary; a name absent from that map is a
/// built-in dog, an argv branch of this binary. That presence-or-absence is
/// the whole of the distinction — `shep-core`'s CHANGELOG records it as the
/// rule, and `shep.toml` is the only place either verb can learn it, since
/// neither `enable` nor `disable` is given a path to carry. It is the same
/// lookup [`crate::commands::daemon::boot_options`] makes over
/// `[daemon] enabled_dogs` when the shepherd starts its dogs at boot.
fn dog_source(cfg: &ShepToml, name: &str) -> DogSource {
    cfg.adopted_dog_path(name)
        .map_or(DogSource::BuiltIn, |path| DogSource::Adopted {
            path: path.display().to_string(),
        })
}

/// Connects to `paths.socket`, distinguishing a genuine absence from a
/// shepherd that IS there and refused — the `dogs.rs` spelling of the same
/// defect `crate::flock_command` fixes for `shep flock` (Task 6 / spec G4).
///
/// `Ok(None)` is the only case [`enable`]/[`disable`]/[`adopt`]/[`rehome`]
/// were ever meant to tolerate silently: [`ConnectError::Connect`],
/// `connect(2)` itself failing because nothing is listening (decision 11 —
/// none of the four may autostart a shepherd to act on its own config
/// edit). Every other [`ConnectError`] variant means a connection WAS
/// established, so a shepherd is there; folding that into `None` too is
/// what the old `Client::connect(..).ok()` did, and it reported a live
/// refusal as "will start with the next shepherd" — the same shape of bug
/// as the blanket `Err(_)` [`crate::flock_command`] used to have.
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
/// inside the lock: either the config layer's error, or a name that
/// answers to no dog at all.
///
/// [`ShepToml::try_edit`] is generic over the closure's error precisely so
/// a verb whose refusal is its own thing does not have to dress it up as a
/// [`ShepTomlError`] it is not.
enum EnableRefusal {
    /// The read-modify-write underneath the closure failed; rendered by
    /// [`fail_config`], exactly as [`ShepToml::edit`]'s `Err` always was.
    Config(ShepTomlError),
    /// The name is neither one of [`crate::dog::BUILT_IN_DOGS`] nor a key
    /// of `[daemon] adopted_dogs`. `adopted` carries what that map did
    /// hold, read under the same lock, so the refusal can name the
    /// alternatives without a second read that a concurrent `shep adopt`
    /// could invalidate.
    UnknownDog { adopted: Vec<String> },
}

impl From<ShepTomlError> for EnableRefusal {
    fn from(err: ShepTomlError) -> Self {
        Self::Config(err)
    }
}

/// `shep enable <name>`: writes the config, and starts the dog if a
/// shepherd is running.
///
/// A name that is neither built-in nor adopted is refused before anything
/// is written. [`dog_source`] reads that distinction as an absence — a
/// name outside `[daemon] adopted_dogs` is [`DogSource::BuiltIn`] by
/// construction — so without this check a typo is written into
/// `enabled_dogs` as a built-in, and the shepherd then spawns `shep dog
/// <typo>` on a restart ladder that cannot ever succeed: `dog::run_dog`
/// refuses the name once per attempt until the budget is spent, while
/// `shep dogs` reports the dog's `SOURCE` as `built-in`, which it is not.
pub async fn enable(streams: &mut Streams<'_>, paths: &ShepPaths, name: &str) -> ExitCode {
    // `try_edit` rather than `edit`, and the check inside the closure
    // rather than before the call: the refusal must skip `save` entirely
    // (a refused enable leaves `shep.toml` untouched, down to its inode),
    // and the adopted-name lookup it turns on must happen under the lock
    // that keeps a concurrent `shep adopt` from landing between the check
    // and the write.
    // `result_large_err` on the closure, for the same reason and on the same
    // platform as the module-wide allow in `commands::shep_toml` -- see the
    // banner there. `EnableRefusal::Config` carries that module's error, so
    // this closure is measured against the same 128-byte threshold the
    // `try_edit` call in `lib.rs`'s `shep style` arm already allows for.
    #[cfg_attr(windows, allow(clippy::result_large_err))]
    let source = match ShepToml::try_edit(&paths.daemon_config, |cfg| {
        // Read from the config rather than assumed: `shep adopt` records
        // the binary and `shep enable` is what starts it afterwards, so a
        // hardcoded `BuiltIn` here sends the shepherd off to spawn `shep
        // dog <name>` and the adopted binary never runs at all.
        let source = dog_source(cfg, name);
        if matches!(source, DogSource::BuiltIn) && !crate::dog::BUILT_IN_DOGS.contains(&name) {
            return Err(EnableRefusal::UnknownDog {
                adopted: cfg.adopted_dog_names(),
            });
        }
        cfg.enable_dog(name);
        Ok(source)
    }) {
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
/// `adopted` is every key of `[daemon] adopted_dogs`, read under the same
/// lock the refusal was decided under; it is empty on a `$SHEP_HOME` where
/// nothing has ever been adopted, which is the common case for this typo.
///
/// [`ExitCode::InvalidConfig`] rather than [`ExitCode::Usage`]: the name
/// is not a malformed argument, it is one the daemon config cannot
/// resolve — the same code [`fail_adopt_name_collision`] gives the
/// mirror-image refusal on `shep adopt`.
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
/// Hand-rolled rather than pulled in: the whole of it is where the two
/// commas go, and shep has one caller ([`fail_enable_unknown_dog`]).
///
/// The empty slice answers with the empty string. No caller can reach it —
/// [`crate::dog::BUILT_IN_DOGS`] is never empty, so the one list this
/// builds always has at least two entries — but a `match` on a slice owes
/// the arm either way, and an empty string is the answer that degrades
/// most quietly if a second caller ever does.
fn join_with_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// `enable`'s daemon half, split out from [`enable`] so a test can drive it
/// against a `shep_client::testing` fake without racing a second, real
/// connection to the same socket the fake's own fixture already opened —
/// [`crate::commands::lifecycle::resolve_target`] is split out of `start`
/// for the same reason: hermetic testability of the part that has a seam.
///
/// `client: None` is [`enable`]'s own [`connect_or_absent`] reporting a
/// genuine absence — matching decision 11: this verb does not distinguish a
/// stale socket file with nothing listening from a daemon that was never
/// started, because a provisioning script configuring a host before
/// starting anything must not have to. A shepherd that IS there and
/// refused is a different case ([`connect_or_absent`]'s own doc, Task 6) —
/// that never reaches this function at all, since [`enable`] returns its
/// refusal directly.
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
    // An `EnableDog` reaching a name a sheep already holds comes back as
    // `RpcErrorCode::InvalidConfig` with the daemon's own message naming
    // the collision (`shep-daemon/src/rpc.rs`'s `EnableDog` arm) — the
    // `Err` arm below surfaces that message verbatim rather than a bare
    // code, which is already the clear report an operator needs.
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

/// `shep disable <name>`: removes it from the config, and stops it if a
/// shepherd is running.
pub async fn disable(streams: &mut Streams<'_>, paths: &ShepPaths, name: &str) -> ExitCode {
    let source = match ShepToml::edit(&paths.daemon_config, |cfg| {
        // `disable_dog` leaves `[daemon] adopted_dogs` alone — that is the
        // difference between `disable` and `rehome` — so this reads the
        // same answer before or after the edit. It is read for the report
        // only: `DisableDog` carries a name and nothing else.
        let source = dog_source(cfg, name);
        cfg.disable_dog(name);
        source
    }) {
        Ok(source) => source,
        Err(err) => return fail_config(streams, &err),
    };
    let client = match connect_or_absent(paths, streams).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    disable_after_config(streams, name, &source, client.as_ref()).await
}

/// `disable`'s daemon half — see [`enable_after_config`]'s own doc for why
/// this split exists and what `client: None` means.
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
    // `Response::Deleted`, the same reply `Delete` gives — `DisableDog`'s
    // own doc (`shep-core/src/protocol/request.rs`) says disabling
    // deregisters exactly as `Delete` does, so this reuses that reply
    // rather than inventing a shape of its own.
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
/// The modes `enable` structurally cannot have, and the reason the two
/// verbs are split rather than one verb carrying an `--exec` flag: a dog
/// that already ships inside this binary has no path to be missing, no
/// permission bit to be unset, no architecture to be wrong, and nobody
/// else who can write it.
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
    /// on this system. An adopted dog runs at the shepherd's own trust
    /// level and is exec'd again on every restart without being re-vetted,
    /// so a path any user can write is a standing way for any user to run
    /// code as the shep user. A writable directory counts for the same
    /// reason a writable file does: it lets the binary be renamed away and
    /// a replacement dropped in its place.
    WorldWritable {
        /// The offending path: the binary itself, or its directory.
        path: PathBuf,
    },
    /// It exists, is executable, and this kernel refused to exec it —
    /// the wrong architecture, or an interpreter line naming something
    /// absent.
    WillNotExec {
        /// What `exec` reported.
        reason: String,
    },
    /// It answered `--version` (see [`DogVersion`]) with a `shep-protocol`
    /// this shep does not speak. The two cannot handshake, so adopting it
    /// would register a dog that connects to nothing — G11's
    /// online-and-idle entry, refused here instead of discovered days
    /// later.
    ///
    /// Only a stated protocol reaches this. A dog that names none is
    /// [`DogVersion::protocol`]'s `None` and is adopted.
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
/// Returns the ABSOLUTE path, canonicalized. The daemon spawns from
/// `shep.toml` after a reboot, from whatever working directory the init
/// system gave it; a relative path recorded here would resolve against the
/// operator's shell and then fail to exec months later, with nothing to
/// connect the failure to the `adopt` that caused it.
///
/// The first three checks run in this order — existence, file-ness,
/// permission bit — each refusing before the next one runs, so a refusal
/// never claims the wrong cause (`NotExecutable` for a path that does not
/// exist would send an operator to `chmod` a file that is not there). The
/// fourth, [`AdoptRefusal::WorldWritable`], is [`writability`]'s, and runs
/// before the exec probe below rather than after it: the probe RUNS the
/// binary, and a binary any user can rewrite is not one to run in order to
/// find out whether it runs. The fifth,
/// [`AdoptRefusal::WillNotExec`], is answered by actually trying it —
/// spawned, and killed once it is confirmed either to have run or to be
/// still running. The question is whether this kernel can exec this file,
/// and the only authority on that is this kernel; reading a header instead
/// would mean writing a second, partial loader that disagrees with the real
/// one — on a fat Mach-O, on a shebang naming an absent interpreter, on a
/// binary needing a missing dynamic library.
///
/// The sixth, [`AdoptRefusal::ProtocolMismatch`], rides on that same
/// process: it is spawned with [`VERSION_FLAG`], so the run that proves the
/// kernel can exec the file is the same run that answers what protocol it
/// speaks. A dog that answers a protocol this shep does not speak is
/// refused here rather than adopted into an entry that connects to nothing.
/// A dog that answers nothing is adopted with [`VettedBinary::answer`]
/// `None`: answering is optional and stays optional, so every dog written
/// before the contract existed is still adoptable.
///
/// `home` and `name` are the ones this invocation resolved, not the ambient
/// ones — the probe is handed exactly the environment the adopted dog will
/// run under, so it is vetted against the daemon the operator named and
/// under the section key `adopt` is about to record.
///
/// # Errors
/// The refusal, which the caller renders — including
/// [`AdoptRefusal::ProtocolMismatch`] when the candidate names a protocol
/// this shep cannot speak. Nothing here is a shep fault, so none of these
/// is an [`ExitCode::Internal`].
/// Proves this kernel can exec `path`, and asks it what protocol it speaks.
///
/// The candidate is spawned once per probe flag, and no more: `--version`
/// first, which proves the exec works at all rather than leaving it to be
/// discovered at supervision time and reads the contract in
/// `docs/dogs.md`, then `--schema`, which asks the dog to describe its own
/// config. The second runs only once the first has decided nothing is being
/// refused. A group-writable binary is reported rather than refused, since
/// an operator naming a path has already made that call.
///
/// # Errors
/// [`AdoptRefusal`] when the path does not resolve, is not a file this
/// kernel will exec, or answers a protocol this shep cannot talk to. A
/// candidate that does not answer `--version` is NOT an error: its protocol
/// is recorded as unknown, which `docs/dogs.md` promises is never a refusal.
/// Nothing a candidate does with `--schema` is an error either, down to
/// failing to run at all; see [`DogSchema`].
pub fn vet_binary(path: &Path, home: &Path, name: &str) -> Result<VettedBinary, AdoptRefusal> {
    vet_binary_within(path, home, name, VERSION_BUDGET)
}

/// [`vet_binary`], against a caller-chosen budget for the `--version` probe.
///
/// Production has exactly one budget and [`vet_binary`] passes it. This
/// exists for tests, and for a specific failure rather than for symmetry:
/// the probe spawns a real child and bounds the wait on a wall clock, so
/// every test reaching it inherits that bound. Measured under a full
/// workspace run, a `/bin/sh` candidate takes 180 to 300ms and over a
/// second at high thread counts, against a [`VERSION_BUDGET`] of one. A
/// test asking a question that has nothing to do with timing then fails
/// because the machine was busy, which is a test reporting on the runner
/// rather than on shep.
///
/// So a test that cares about the budget passes a small one and a test that
/// does not passes a generous one, and neither is at the mercy of what else
/// is running. The alternative considered and rejected was moving them all
/// into `mod slow`, which would take roughly twenty tests out of the inner
/// loop to fix a problem none of them are about.
///
/// # Errors
/// The same [`AdoptRefusal`] set [`vet_binary`] raises, with the `--version`
/// probe bounded by `budget` rather than by [`VERSION_BUDGET`].
///
/// # What `budget` actually bounds
///
/// One wait, not the call. [`answer_text`] bounds two things separately and
/// gives each the full `budget`: the wait for the child to exit, and the
/// wait for its output to arrive. So one probe is bounded by roughly twice
/// `budget`, plus up to 50ms on macOS for `macos_deferred_exec_failure`'s
/// own poll, and a vet runs two of them, `--version` and then `--schema`.
///
/// Deliberate, and the alternative is worse. One absolute deadline shared
/// between the two waits reads tidier and would make this doc a single
/// number, but it means a child that exits at 0.9 of the budget leaves 0.1
/// for its output to be read. A probe that runs out answers unknown, and
/// unknown is deliberately silent, so tightening this would make shep go
/// quiet about exactly the dogs it is meant to notice. The two waits bound
/// different failures, a child that will not exit and a pipe a grandchild
/// is holding, and neither should be able to consume the other's room.
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
    // other (0o001). `PermissionsExt` is always in scope here — this
    // module compiles only under `#[cfg(unix)]` (`main.rs`'s own `mod
    // commands` gate), so there is no non-unix build of this function to
    // guard against.
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(AdoptRefusal::NotExecutable);
    }
    // `metadata` above already proved something exists at `path`, so this
    // canonicalize is not itself a new place to observe `Missing` — a
    // symlink loop or a race with something deleting the file between the
    // two calls is the only way it could fail, and either way there is
    // nothing more specific than `Missing` to report.
    // The vetted path is recorded in `shep.toml` under `adopted_dogs`, a file
    // an operator opens and edits, so Windows' verbatim prefix is stripped
    // before it gets there rather than after.
    let canonical = path
        .canonicalize()
        .map(|abs| shep_core::paths::strip_verbatim_prefix(&abs).into_owned())
        .map_err(|_| AdoptRefusal::Missing)?;
    let group_writable = writability(&canonical)?;
    let answer = ask_version(&canonical, home, name, budget)?;
    // Only a STATED protocol can refuse, and only this one thing can.
    // `answer.version` is deliberately not compared with anything: a
    // third-party dog's crate version has no relationship to shep's own --
    // `shep-log-rotate` 0.1.3 against shep 0.1.24 is the ordinary case, not
    // a skew -- so comparing them would refuse, or warn about, every dog
    // that exists. `docs/dogs.md`'s table is what this implements: the
    // protocol decides whether the dog can connect at all and is hard, the
    // version says which build it is and is reported.
    if let Some(dog) = answer.as_ref().and_then(|answer| answer.protocol)
        && dog != PROTOCOL_VERSION
    {
        return Err(AdoptRefusal::ProtocolMismatch {
            dog,
            shep: PROTOCOL_VERSION,
        });
    }
    // Asked AFTER the protocol refusal above, so a candidate shep is about
    // to refuse is not run a third time to answer a question the refusal
    // makes moot.
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
/// Mirrors `shep_daemon::assemble::base_env` rather than importing it,
/// because that function is private to a crate the CLI does not otherwise
/// reach into for this. The lists are duplicated and that is a real cost:
/// if the daemon's allowlist grows, this one has to follow, or a candidate
/// is vetted under conditions its supervised run will not have.
/// `a_probe_runs_with_the_daemons_environment_and_not_the_operators` is the
/// test that fails when they disagree about the two variables that matter.
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
/// One spawn shape for both probes rather than two, because the argument
/// each of them changes is the flag and everything else about running a
/// stranger's binary is the same: the cleared environment, the two
/// variables a dog is promised, the null stdin and stderr, the bounded
/// wait, and the kill afterwards. A second, looser path would be a second
/// place for one of those to be missing.
///
/// `Ok(None)` is NO ANSWER, and is never a fault: silence, a run that
/// failed, and a run still going when the budget ran out all arrive here
/// the same way. Answering either flag is optional and stays optional, so
/// every dog written before the contract existed reads as unanswered rather
/// than as broken. `Ok(Some(text))` is only ever the output of a run that
/// exited 0, which is what `docs/dogs.md` asks a dog for; nothing here
/// reads `text`, so what counts as a usable answer is each caller's own
/// question.
///
/// # Errors
/// [`AdoptRefusal::WillNotExec`], and only that: nothing here judges the
/// answer it read, so [`AdoptRefusal::ProtocolMismatch`] stays
/// [`vet_binary`]'s to raise. A caller that only wants an answer treats the
/// error as one more way of not getting one.
fn ask(
    path: &Path,
    flag: &str,
    home: &Path,
    name: &str,
    budget: Duration,
) -> Result<Option<String>, AdoptRefusal> {
    // Spawned with ONE argument, the flag being asked, and torn down
    // unconditionally: `kill` is ignored (a process that already exited is
    // not a failure to vet), but `wait` always runs, on every path out of
    // this match, so no zombie survives a refusal or a success.
    //
    // That argument is the one place shep invents an argv for an adopted
    // dog, and `dog_app`'s own doc argues the other way for the SUPERVISED
    // run: "an argv shep invented for it is one more thing it has to agree
    // with before it can start", so a dog is run with none. Both are
    // right, because they are different runs. A supervised dog must work
    // for a stranger who never read this repo, so shep asks it for nothing.
    // This process is a throwaway that exists only to be observed and
    // killed, so the cost of a candidate disagreeing about a probe flag is
    // an unknown protocol or an unread schema -- and `docs/dogs.md`
    // promises that neither is a refusal. The vet asks; the supervisor does
    // not.
    //
    // A candidate is somebody else's binary and is not assumed to be well
    // behaved about any of it: [`answer_text`] bounds the wait by
    // `budget`, so one that never exits costs that and is then killed,
    // rather than hanging the command that asked.
    //
    // `SHEP_HOME` is set to the home this invocation actually resolved, not
    // inherited: an inherited `SHEP_HOME` is usually unset, so the
    // candidate would resolve the DEFAULT home instead of `--home`'s. A dog
    // reads `SHEP_HOME` to find its socket, which is the one thing
    // `docs/dogs.md` promises it, so a rotator or anything else with a job
    // to do would connect to the LIVE daemon and do it, during the command
    // whose entire purpose is deciding whether to trust the binary at all.
    //
    // `env_clear()` and then the daemon's own allowlist, which is a
    // REVERSAL of what this comment used to say and worth recording rather
    // than quietly swapping. It argued that clearing would vet under
    // stricter conditions than the dog ever runs under, so a binary needing
    // `DYLD_LIBRARY_PATH` would be refused despite working once adopted,
    // and that vetting has to model the real thing.
    //
    // The principle was right and the conclusion inverted. The real thing
    // is not the operator's shell: an adopted dog runs with what the DAEMON
    // builds (`assemble::base_env`: `PATH` plus a short allowlist), so
    // inheriting the operator's environment vetted under LOOSER conditions
    // than the dog will ever see, and handed a stranger's binary the one
    // place credentials live. Clearing and rebuilding models the run more
    // closely, not less. See `probe_env`.
    //
    // The restart caller is the sharper case, and review is what made it
    // visible. `vet_binary` WARNS about a group-writable binary and adopts
    // it anyway, so a group member can replace an adopted dog. Every
    // `shep restart <name>` after that runs their code, and a probe that
    // ignores `--version` is a program of their choosing reading whatever
    // the operator had exported.
    //
    // An earlier revision of this comment said that filtered environment has
    // the dog's `[dog.<name>]` env merged over it, citing `AppConfig::env`'s
    // doc. That is true of a sheep and false of a dog: `dog_app` builds an
    // `AppConfig::minimal` and inserts only the two variables below, and the
    // `[dog.<name>]` section never becomes an `AppConfig` at all -- it is
    // served as opaque text over the socket, which is the whole point of
    // keeping it off the environment. `shep-daemon`'s
    // `a_dogs_child_environment_carries_shep_home_and_its_name_and_no_configuration`
    // is what pins it.
    //
    // `SHEP_DOG_NAME` rides along for the same reason, and it is `name`
    // rather than the binary's file stem because the operator's `--name` is
    // what `adopt` is about to record. One rule with no exception at this
    // seam: every way shep runs a dog -- supervised, `shep <name>`, and this
    // probe -- hands it the same two variables, so a dog is never vetted
    // under a contract it will not meet again.
    //
    // Not directly asserted by a test, unlike the other two paths, and that
    // is a property of the probe rather than an oversight: this child is
    // killed on sight (immediately, on every kernel but macOS), so anything
    // it writes to prove what it received is a race with its own teardown.
    //
    // Stdin and stderr go to null, and stdout to a pipe shep reads rather
    // than to the terminal. A candidate that writes on its way up would
    // otherwise scribble over the operator's terminal mid-vet, and a hostile
    // one could imitate shep's own output at the exact moment somebody is
    // deciding whether to trust it. The pipe is read by [`answer_text`] and
    // never rendered.
    // `env_clear` and then the allowlist, never the operator's environment.
    // Argued at the top of this function; `probe_env` builds the list.
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
            // Started before the exec probe below and before any wait: a
            // candidate that writes more than a pipe buffer holds would
            // otherwise block on its own `write` forever, and be
            // indistinguishable from one that simply never exits.
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
/// `Ok(None)` is an UNKNOWN protocol and is never a fault: everything
/// [`ask`] answers `None` for, plus output with no line 1 to read a version
/// from.
///
/// Two callers ask the same binary the same question for different reasons.
/// [`vet_binary`] asks a candidate nobody has adopted yet;
/// [`warn_of_a_dog_a_restart_would_break`] asks a dog that has been adopted
/// for months, because the answer lives in the binary and the binary
/// changes on disk with nothing watching (G12 row 5). Neither writes the
/// answer down.
///
/// # Errors
/// [`AdoptRefusal::WillNotExec`], and only that, for the reason [`ask`]
/// gives.
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
/// No `Result`, because nothing a candidate does to this probe can refuse
/// an adopt (decision 4): a dog whose schema flag is broken may still do
/// its job perfectly, and the version probe has already answered the only
/// question that can. So a failure to spawn arrives as
/// [`DogSchema::Silent`], the same as a dog that has never heard of the
/// flag.
///
/// The answer is not written anywhere, here or by any caller (decision 7).
/// `cargo install` replaces a dog's binary with nothing watching, so a
/// stored schema would be wrong at the moment it mattered, and a stale
/// schema is worse than a stale version number because it mislabels which
/// field is a credential.
fn ask_schema(path: &Path, home: &Path, name: &str, budget: Duration) -> DogSchema {
    // A run that exited 0 and printed nothing is a dog with no schema, not
    // a dog whose schema failed to parse: empty input IS invalid JSON, so
    // without this the ordinary case would earn the warning meant for a
    // broken one.
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
    /// The absolute, canonicalized path — the one `adopt` records and the
    /// daemon later exec's.
    pub path: PathBuf,
    /// The paths [`writability`] found group-writable: the binary, its
    /// directory, both, or (the ordinary case) neither. `adopt` reports one
    /// notice per entry; nothing here is a refusal.
    pub group_writable: Vec<PathBuf>,
    /// What it answered when asked for its version, and `None` when it
    /// answered nothing shep could read -- silence, a failed run, or output
    /// with no first line. `adopt` reports it and does not record it; see
    /// [`DogVersion`].
    pub answer: Option<DogVersion>,
    /// What it answered when asked for its config schema. Read by the
    /// caller and written down by nobody, for the reason [`ask_schema`]
    /// gives.
    pub schema: DogSchema,
}

/// What a candidate answered when asked for its config schema, and the
/// three answers are not one `Option`: only one of the two ways of having
/// no schema is worth telling an operator about.
///
/// Nothing here is a refusal. A dog with a broken schema flag may still do
/// its job perfectly, and refusing one would be shep judging a binary on a
/// question the binary never promised to answer.
#[derive(PartialEq, Eq)]
pub enum DogSchema {
    /// The dog printed JSON, and this is it, exactly as it wrote it. It is
    /// not validated as JSON Schema past being JSON: the dog is the
    /// authority on its own config, and a shep that disagreed about the
    /// document's shape would be wrong in the direction that hides a
    /// working dog's settings.
    Published(serde_json::Value),
    /// The dog answered nothing shep can use: it printed nothing, its run
    /// failed, it never exited, or it could not be spawned at all. Every
    /// dog written before this contract existed lands here, so it earns no
    /// warning.
    Silent,
    /// The dog printed something that is not JSON. It meant to answer and
    /// its answer cannot be read, which is a bug in that dog and the one
    /// shape `adopt` warns about.
    Unreadable,
}

/// Reports THAT there is a schema, never what is in it.
///
/// Manual, and a derive here would be a regression (IR-41). A schema
/// carries the dog's own defaults, which is the same field the secret
/// marker exists to keep off a screen, and a `{vetted:?}` in a test
/// assertion or a future log line is all it would take to put a credential
/// somewhere it is read later. `debug_reports_that_there_is_a_schema_and_never_what_is_in_it`
/// pins the exact strings.
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
/// flag is asked in its own spawn and gets the whole budget; the name
/// predates the second flag and the number is the same for both.
///
/// One second, against the milliseconds a `println!` and an exit take. The
/// generosity is for a cold, dynamically linked binary on a loaded machine,
/// and the ceiling is what an operator will sit through: `adopt` is
/// interactive and runs once. Both ways of being wrong are survivable and
/// they are not symmetric -- too short records an unknown protocol for a
/// slow dog, which costs the gate and not the adopt, while too long stalls
/// every adopt of the dogs that exist today, none of which answer at all.
///
/// `restart` asks with the same number rather than a tighter one of its
/// own, and the tighter one was tried first: a 250ms budget lost the answer
/// outright on a loaded machine, where a probe measured 180-300ms against
/// single-digit milliseconds idle. A budget that drops the warning whenever
/// the box is busy is worse than one that occasionally costs a second,
/// because a busy box is where an operator restarts things. The cost is
/// bounded and it is paid only by a restart that NAMED an adopted dog: a
/// binary that hangs is killed at the budget and the restart proceeds
/// unwarned, so the slowest thing a dog can do to `shep restart` is delay
/// it, never block it.
pub(crate) const VERSION_BUDGET: Duration = Duration::from_secs(1);

/// How often [`answer_text`] polls within [`VERSION_BUDGET`], following
/// [`macos_deferred_exec_failure`]'s own polled-with-a-budget shape rather
/// than inventing a second one.
const VERSION_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// The most a probe will read from a candidate, per spawn.
///
/// One mebibyte, against a version answer of two lines and a JSON Schema
/// that runs to single-digit kilobytes for a config with a lot of fields.
/// Three orders of magnitude of headroom, and still a bound: without one,
/// the read is limited only by the budget and by how fast the candidate can
/// write, which measured at roughly 290MB of resident memory for a second
/// of spew and is a stranger's binary deciding how much of this machine's
/// memory to take. `adopt` runs that binary twice, so it is asked twice.
///
/// Truncation is not a new outcome and never a refusal. A cut-off version
/// answer is output shep cannot read, which is the unknown protocol it
/// already was; a cut-off schema is not valid JSON, so it is
/// [`DogSchema::Unreadable`] and earns the warning decision 4 already
/// defines. Both need a dog to print a megabyte before answering, which no
/// dog following the contract does.
const PROBE_OUTPUT_LIMIT: u64 = 1024 * 1024;

/// Drains `stdout` to end on a thread, handing the text back through the
/// returned channel.
///
/// A thread rather than a read on this one, because every read here has to
/// be bounded and none of them can be. Reading before the candidate exits
/// blocks until it writes; reading after it exits blocks for as long as
/// anything the candidate spawned still holds the inherited pipe open,
/// which a candidate shep is vetting precisely because it does not trust it
/// could do forever. On the timeout path the thread is left to end on its
/// own when the pipe closes: it holds nothing but a `String` and a sender,
/// and `adopt` is a one-shot command.
fn read_in_background(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut text = String::new();
        // Bounded by [`PROBE_OUTPUT_LIMIT`], and the drop that follows is
        // half of what the bound buys: the read end closes, so a candidate
        // still writing takes an EPIPE instead of being left blocked on a
        // full pipe until the budget kills it.
        //
        // A candidate answering in bytes that are not UTF-8 is answering
        // nothing shep can read, which is the same unknown as silence. That
        // now includes a multi-byte character the cap cut in half, since
        // `read_to_string` leaves `text` empty when it fails: a candidate
        // that reached the cap mid-character was already answering
        // something no reader here was going to use.
        let _ = stdout.take(PROBE_OUTPUT_LIMIT).read_to_string(&mut text);
        let _ = tx.send(text);
    });
    rx
}

/// Waits, bounded by `budget`, for `child` to answer, and returns what it
/// printed.
///
/// Twice that is the worst case rather than once, because the wait for the
/// exit and the wait for the text the reader thread collected are bounded
/// separately. Only a child that has ALREADY exited successfully reaches
/// the second one, so its text is written and its own end of the pipe is
/// closed; that bound is there for a grandchild still holding the inherited
/// pipe open, not for anything the dog itself is doing.
///
/// `None` -- no answer, never a refusal -- for every way of not answering:
/// no pipe to read, a run that did not exit inside the budget, or a run
/// that exited non-zero. `Some` may still be empty: a run that exited 0 and
/// printed nothing answered, and it is each caller's own business what that
/// means for the question it asked.
///
/// The non-zero exit is not a technicality. `docs/dogs.md` says a dog
/// answers on stdout AND exits 0, so lines printed by a run that then
/// failed are not an answer -- and believing them would let a candidate
/// refuse its own adopt with a protocol number from a code path that did
/// not work.
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

/// Who besides the owner can write `canonical` and the directory holding
/// it, checked on the canonicalized path — the one actually recorded, and
/// so the one the daemon actually exec's, whatever the operator typed.
///
/// The split between refusing and warning follows the precedent OpenSSH set
/// for `authorized_keys` and sudo set for `sudoers`: refuse the unambiguous
/// case, and do not try to be clever about the ambiguous one.
///
/// World-writable is unambiguous — no legitimate deployment leaves a binary
/// the whole machine can rewrite — so it is [`AdoptRefusal::WorldWritable`].
/// Group-writable is not: a deployment directory owned by a trusted deploy
/// group is a normal, deliberate arrangement, and CI pushes into one all
/// day. Refusing it would block real setups, so it comes back as a path to
/// warn about and the adopt proceeds.
///
/// A path with no parent (`/` itself) cannot be a file and never reaches
/// here as `canonical`, so the directory half simply has nothing to check.
///
/// The sticky bit is deliberately not an exemption for the directory half.
/// It does defeat the rename-and-replace attack in a world-writable `/tmp`,
/// but a dog exec'd out of `/tmp` on every restart is not an arrangement
/// worth carving a special case for, and both precedents above refuse the
/// world-writable mode without consulting it.
///
/// # Errors
/// [`AdoptRefusal::WorldWritable`], naming whichever of the two paths it
/// found first — the binary before its directory, so the more specific
/// thing to fix is the one reported.
fn writability(canonical: &Path) -> Result<Vec<PathBuf>, AdoptRefusal> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // `mut` is read only by the `cfg(unix)` push below, so Windows sees an
    // unused `mut` and unix sees an immutable binding it needs to mutate.
    #[cfg_attr(windows, allow(unused_mut))]
    let mut group_writable = Vec::new();
    for candidate in [Some(canonical), canonical.parent()].into_iter().flatten() {
        // Unreadable metadata is not a refusal: the binary itself was
        // stat'ed successfully by the caller, and a directory whose mode
        // cannot be read is a state this check has nothing to say about.
        let Ok(metadata) = std::fs::metadata(candidate) else {
            continue;
        };
        // A world-writable ancestor is a unix hazard with a unix spelling.
        // The Windows analogue is an ACE granting write to a broad group,
        // which is not a bit to test and needs a real ACL read; `shep adopt`
        // does not perform that check there, and the operator docs say so
        // rather than implying the ancestry walk is equivalent.
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
/// 50ms, next to the ~3ms this module's own tests observe the kernel's
/// fallback taking — generous against scheduler contention under a loaded
/// `cargo test` run without meaningfully slowing `adopt` against a binary
/// that DOES run (which reaches this budget's full length every time,
/// since nothing here can prove a negative early).
#[cfg(target_os = "macos")]
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

/// How often [`macos_deferred_exec_failure`] polls within [`PROBE_BUDGET`].
#[cfg(target_os = "macos")]
const PROBE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_micros(500);

/// Catches the one way [`vet_binary`]'s `Command::spawn` can succeed for a
/// file this kernel cannot actually run.
///
/// On a kernel that reports an unexecutable file synchronously — Linux's
/// `execve`, reached through `std`'s own `posix_spawn` fast path, which
/// glibc has supported reporting `ENOEXEC`/`ENOENT` through since 2.24 —
/// `Command::spawn` above already returned `Err` and this function is
/// never reached for that case; its `#[cfg(not(target_os = "macos"))]`
/// sibling below is what runs there, and it does nothing.
///
/// macOS's own `posix_spawn` fast path (the reason `std` picks it there:
/// `fork` is comparatively expensive on this kernel) is NOT synchronous
/// for an exec-format failure: the fork has already happened by the time
/// `spawn` returns `Ok`, and a file this kernel cannot recognize is
/// instead re-executed through `/bin/sh` — the same legacy fallback a
/// shebang-less old-style script relies on — which then refuses it and
/// exits `126`, the shell convention for "found, but not executable".
/// That happens within a few milliseconds in this module's own tests,
/// well inside [`PROBE_BUDGET`]; a genuinely runnable binary is still
/// running for the budget's whole length (verified against `/bin/sleep 5`
/// while writing this function — it never reports otherwise inside it).
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
            // Exited some other way (a real, if fast, run), or the wait
            // itself failed: neither is this function's failure to report.
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => std::thread::sleep(PROBE_POLL_INTERVAL),
        }
    }
    None
}

/// See [`macos_deferred_exec_failure`]'s own doc: every other kernel this
/// crate targets reports an exec-format failure synchronously, through
/// `Command::spawn`'s `Err` arm, so there is nothing for this half to
/// catch.
#[cfg(not(target_os = "macos"))]
fn macos_deferred_exec_failure(_child: &mut std::process::Child) -> Option<String> {
    None
}

/// Renders `refusal` and returns the exit code an unvettable binary
/// reports. [`ExitCode::InvalidConfig`] for every mode — what's wrong is
/// the argument `adopt` was given, not shep's own state, the same category
/// a bad Flockfile value gets.
fn fail_adopt(streams: &mut Streams<'_>, path: &Path, refusal: &AdoptRefusal) -> ExitCode {
    let code = ExitCode::InvalidConfig;
    let message = format!("{}: {refusal}", path.display());
    streams.fail(code, &message)
}

/// [`emit_notice`] code for the group-writable warning — caller-defined,
/// like every notice code, and deliberately not one of
/// [`ExitCode::code_str`]'s: `adopt` still succeeds here.
const GROUP_WRITABLE_NOTICE: &str = "group_writable";

/// Warns that `path` is group-writable, and lets the adopt proceed.
///
/// Not an error and not a refusal — see [`writability`] for why this mode
/// is warned about rather than refused. It goes out through
/// [`emit_notice`] rather than [`emit_error`] for exactly the reason that
/// function exists: a `--format json` consumer must be able to tell a
/// diagnostic on a successful command from a failure.
fn warn_group_writable(streams: &mut Streams<'_>, path: &Path) {
    let message = format!(
        "{} is writable by its group; anyone in that group can replace the binary \
         this dog runs, and it runs with the shepherd's own privileges",
        path.display()
    );
    streams.aside(GROUP_WRITABLE_NOTICE, &message);
}

/// [`emit_notice`] code for the version report -- caller-defined, like
/// [`GROUP_WRITABLE_NOTICE`], and like it not a failure: `adopt` succeeded.
const DOG_VERSION_NOTICE: &str = "dog_version";

/// Tells the operator what the candidate answered.
///
/// Only the version is reported, never compared -- see [`vet_binary`] for
/// why comparing a third-party dog's crate version with shep's own would
/// report every dog that exists. The protocol is not reported when it
/// matches, because by then it has already decided the only thing it can
/// decide; it is reported when it is missing, because that is the operator's
/// one chance to hear that this dog's compatibility is unknown and will be
/// found out at a handshake instead.
///
/// A dog that answered nothing at all gets no notice. Every dog that
/// existed when this contract was written is in that group, and a line on
/// stderr for each of them would be noise about the ordinary case.
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

/// [`emit_notice`] code for the unreadable-schema warning -- caller-defined
/// like the two above, and like them not a failure: `adopt` succeeded.
const DOG_SCHEMA_UNREADABLE_NOTICE: &str = "dog_schema_unreadable";

/// Warns that `name` answered the schema flag with something that is not
/// JSON, and lets the adopt proceed.
///
/// The one schema answer worth a line, and the reason is the difference
/// between a dog that said nothing and a dog that tried. Silence is the
/// ordinary case, since every dog written before this contract is silent,
/// and a notice for each of them is how an operator learns to skip the one
/// that matters. Unreadable output is a bug in that dog which its author
/// can fix, and shep is the only thing positioned to notice it.
///
/// What it costs the operator is named rather than left to be discovered:
/// the dog works, and the settings it would have described are edited by
/// hand instead.
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
/// speak to this shepherd -- caller-defined, like the two above, and like
/// them not a failure: the restart still happens.
const DOG_BINARY_SKEW_NOTICE: &str = "dog_binary_skew";

/// Warns, before `restart` sends anything, about a dog whose binary ON DISK
/// speaks a protocol this shepherd does not -- G12's row 5.
///
/// Row 5 is the one state in that matrix where nothing is wrong yet. The
/// running dog is connected and working, the binary it would come back from
/// is not, and the two only meet at the next restart, which may be days
/// away and for an unrelated reason. This is the moment that restart
/// happens, so this is where the operator can still be told.
///
/// **A warning, never a refusal.** They asked for the restart, the binary on
/// disk may be exactly what they just installed, and refusing an explicit
/// command on a prediction is a worse failure than letting them watch it
/// happen with the warning in hand. G12 row 5's fix names two ways out and
/// picks neither, so the message does the same.
///
/// **Silence is the answer for everything else.** A dog that does not answer
/// [`VERSION_FLAG`] is unknown, not stale -- that is the state `adopt`
/// records under G11's "recorded as unknown", and this is its reader. Every
/// dog that existed when the contract was written is in that group, so a
/// warning there would be a line on stderr for the ordinary case, which is
/// how an operator learns to skip the one that matters.
///
/// # A built-in dog cannot reach this, by construction
///
/// The only way in is a path out of `[daemon] adopted_dogs`. A built-in dog
/// has no entry there ([`dog_source`]), and could not use one: the shepherd
/// runs `metrics` and `bark` as `<its own binary> dog <name>`, so a built-in
/// dog's binary on disk IS the shepherd's, and there is no second thing to
/// drift. There is no branch here skipping them, and none to forget to
/// write.
///
/// # Why only a `Name` selector
///
/// The daemon includes a dog only for a selector that NAMED it
/// (`ProcessSelector::is_exact`), so `restart all` and a `/regex/` sweep
/// pass every dog by and there is nothing for this to warn about. That
/// leaves `Id`, which is exact and could name a dog: it is not probed,
/// because the CLI would have to ask the daemon what that id is called
/// before it could look up a path, and a round trip is a lot to spend on
/// the rarer way of typing the same thing. An operator who restarts a dog
/// by id gets the restart and no warning.
/// Takes the probe budget rather than reading [`VERSION_BUDGET`], for the
/// same reason [`vet_binary_within`] exists. Both callers have a budget in
/// hand, so there is no wrapper to add: `restart` passes the production
/// one, and a test passes one that contention cannot exhaust.
///
/// A test here asks whether the warning fires, in what order, and what it
/// says. At the production budget it was also asking how busy the machine
/// was, because a probe that times out answers unknown, and unknown is
/// deliberately silent, so the test failed reporting that no warning fired.
/// True about the runner, empty about shep.
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
        // The one way in, and the reason a built-in dog is not a case here:
        // this answers `None` for every name `[daemon] adopted_dogs` has
        // never heard of, which is every built-in dog and every sheep.
        let Ok(Some(binary)) = ShepToml::adopted_dog_path_readonly(&paths.daemon_config, name)
        else {
            continue;
        };
        // Asked, never remembered. A protocol recorded at adopt time would
        // be a copy of a number that changes on disk with nothing watching,
        // and row 5 IS the binary changing after the adopt -- so the stored
        // copy would be wrong at precisely the moment it was needed.
        //
        // Reusing `ask_version` here costs something `adopt` does not pay,
        // and the trade is different enough to argue separately rather than
        // inherit. That function's own comment is explicit that a candidate
        // ignoring `--version` runs its ordinary job instead, with
        // `SHEP_HOME` pointing at the live daemon. For `adopt` that is a
        // one-off against a binary nobody trusts yet. Here the dog is
        // already adopted and already running, the probe repeats on every
        // named restart, and the population that ignores `--version` is
        // every dog written before this contract existed, which today is
        // all of them. So `shep restart log-rotate` can rotate once before
        // the restart, and a bark dog can open a second subscription, both
        // for up to `VERSION_BUDGET`.
        //
        // Accepted rather than overlooked, on three grounds. The command
        // is about to restart that dog anyway, so the dog runs either way
        // and the question is only whether it briefly overlaps itself. The
        // window is bounded and the process is killed. And the alternative
        // is not asking, which is the state that let a stale dog sit
        // `online` for two days. It is a real cost though, so it is named
        // in `docs/dogs.md` where an operator can read it rather than only
        // here.
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
/// `args.path` is resolved before anything else ([`resolve_adopt_path`]),
/// and `args.name` defaults to the resolved binary's file stem
/// ([`default_dog_name`]) when omitted -- a defaulted name goes through the
/// same [`collides_with_a_verb`] refusal an explicit `--name` would.
///
/// **The collision check runs before [`vet_binary`], not after.**
/// `vet_binary` spawns the candidate to prove this kernel can exec it, so
/// checking the name first means a refused name never gets that binary run
/// at all -- a refusal that already ran the thing it refuses is not a
/// refusal. `default_dog_name` needs only the resolved `candidate`, never
/// the vetted/canonicalized path, so nothing forces the other order.
pub async fn adopt(streams: &mut Streams<'_>, paths: &ShepPaths, args: &AdoptArgs) -> ExitCode {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path_var = std::env::var_os("PATH");
    let candidate = resolve_adopt_path(&args.path, home.as_deref(), path_var.as_deref());
    // Named and checked for a verb collision BEFORE vetting, deliberately:
    // `vet_binary` below spawns the candidate to prove this kernel can exec
    // it, and a refusal that runs after that spawn has already run the
    // thing it refuses. `default_dog_name` only needs the resolved
    // `candidate`, never the vetted/canonicalized path, so this reorder
    // costs nothing -- see `a_name_collision_is_refused_before_the_candidate_is_ever_spawned`.
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
    // The schema itself is reported by nothing and stored by nothing: it is
    // asked fresh wherever it is needed (decision 7), so the only thing
    // `adopt` has to say about it is the one way of answering that is a bug.
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

/// Resolves `raw` -- `shep adopt`'s own path argument -- before it reaches
/// [`vet_binary`]: (a) as given, (b) with a leading `~/` expanded against
/// `home`, (c) looked up on `path_var`. First hit wins; if none of the
/// three finds anything, `raw` comes back unchanged so `vet_binary` reports
/// the same [`AdoptRefusal::Missing`] it always has.
///
/// `home` and `path_var` are taken as parameters, read once by [`adopt`]
/// from `$HOME`/`$PATH`, rather than read here -- the same reason
/// [`resolve_paths`](crate::resolve_paths) takes an `env` closure instead
/// of calling `std::env::var` inline: it keeps this function a pure
/// function of its inputs, testable with a fabricated home or `$PATH`
/// without touching the real environment (this crate forbids `unsafe`
/// code outright, so a test cannot use `std::env::set_var` to do that
/// either -- it is `unsafe` as of edition 2024).
///
/// All three routes funnel into the one [`vet_binary`] call in [`adopt`]
/// once this returns -- existence, file-ness, the execute bit and the exec
/// probe are exactly as strict for a `$PATH` hit or a `~/`-expanded one as
/// for a literal path, so this changes what `adopt` can FIND, never what
/// it VETS. `cargo install shep-log-rotate` puts the binary on `$PATH`
/// under its own name; this is what lets `shep adopt shep-log-rotate` find
/// it there instead of demanding the full install path.
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

/// `~/`-expands `value` against `home`, for [`resolve_adopt_path`]'s
/// second step. `None` for anything
/// [`shep_core::config::expand_home_tilde`] refuses (another user's home,
/// or no home to expand against) or that does not start with `~` at all --
/// either way [`resolve_adopt_path`] moves on to its next step rather than
/// surfacing a tilde-specific error, since the path might never have been
/// a tilde path to begin with.
///
/// The one piece of tilde-expansion logic in the workspace lives in
/// shep-core, shared with Flockfile path fields (`normalize::expand_tilde`)
/// -- this is not a second implementation of it.
fn expand_tilde_candidate(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if !value.starts_with('~') {
        return None;
    }
    shep_core::config::expand_home_tilde(value, home)
        .ok()
        .map(PathBuf::from)
}

/// Looks `name` up on `path_var` (`$PATH`'s own syntax: `:`-separated
/// directories), the way a shell would -- and only the way a shell would: a
/// bare name with no directory component of its own (`shep-log-rotate`,
/// not `./shep-log-rotate` or `/opt/bin/shep-log-rotate`), and only a hit
/// with an execute bit set for someone, so a same-named non-executable
/// file earlier on `$PATH` does not block the real binary further down it.
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
            // On unix a file is runnable if any execute bit is set. Windows
            // has no execute bit: what makes a file runnable there is its
            // extension being in `%PATHEXT%`, and `CreateProcess` is the
            // only real authority on it. Being a file is the honest test
            // here — the spawn itself is what refuses a non-executable one,
            // with a message from the OS rather than a guess from us.
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
/// **Windows resolves a bare command through `%PATHEXT%`**, and without that
/// this function could not find anything `cargo install` produces — the help
/// text for `shep adopt` promises exactly that case ("a bare name already on
/// `$PATH` (`cargo install` puts one there)"), and `cargo install` writes
/// `foo.exe`, never `foo`. Measured: `shep adopt shep-log-rotate` refused
/// with "no file exists at that path" on a machine where
/// `shep-log-rotate.exe` was sitting on `$PATH`.
///
/// The bare name is tried first anyway, so a genuinely extensionless file is
/// still found; the extensions are appended in `%PATHEXT%` order, which is
/// the order the shell itself would try them. The fallback list is the
/// documented default for a system where the variable is somehow unset.
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
/// `cargo-` from its own external subcommands. `shep-log-rotate` defaults
/// to `log-rotate`; a binary with no `shep-` prefix keeps its whole stem,
/// and a binary literally named `shep-` (stem would strip to empty) keeps
/// its whole stem too, rather than defaulting to an unreachable empty name.
///
/// Derived from `path` as resolved (pre-canonicalize), not from
/// `vet_binary`'s canonicalized return value: a symlink's own name is what
/// an operator typed and expects to see, not whatever file it happens to
/// point at.
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
/// aliases -- `shep adopt`'s own refusal, since a dog adopted under such a
/// name could never be reached: `shep <name>` always dispatches to the
/// built-in verb first (see `lib.rs`'s `dispatch_adopted_dog`).
///
/// Reads the name and every alias straight off the real `clap::Command`
/// tree (`Cli::command()`) rather than a hand-copied list, so a verb added
/// later is refused automatically instead of silently becoming
/// unreachable once adopted under it.
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

/// `adopt`'s daemon half — see [`enable_after_config`]'s own doc for why
/// this split exists and what `client: None` means.
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
/// Two files, in this order. [`ShepToml::rehome_dog`] strikes the
/// registration from `shep.toml`, then
/// [`dog_migration::forget_dog_section`] strikes the configuration from
/// `dogs.toml`. The cross-file half is here rather than inside
/// `rehome_dog` because [`ShepToml`] owns one file and writing the other
/// needs the staged-temp, `fsync` and `rename` path that keeps webhook
/// credentials at `0600`.
pub async fn rehome(streams: &mut Streams<'_>, paths: &ShepPaths, name: &str) -> ExitCode {
    let source = match ShepToml::edit(&paths.daemon_config, |cfg| {
        // Read before `rehome_dog` erases it — the row below reports what
        // this verb forgot, and `None` (a name never adopted, or a
        // built-in dog's own name) is a legitimate answer, not a fault.
        let source = cfg.adopted_dog_path(name).map(|path| DogSource::Adopted {
            path: path.display().to_string(),
        });
        cfg.rehome_dog(name);
        source
    }) {
        Ok(source) => source,
        Err(err) => return fail_config(streams, &err),
    };
    // Reported and non-zero rather than pressed on with: the dog is out of
    // `shep.toml` by now, so the daemon half below would stop it and the
    // operator would be told it was forgotten while its configuration, and
    // the webhook URLs in it, were still on disk.
    if let Err(err) = dog_migration::forget_dog_section(&paths.dogs_config, name) {
        return fail_dogs_config(streams, &err);
    }
    let client = match connect_or_absent(paths, streams).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    rehome_after_config(streams, name, source, client.as_ref()).await
}

/// `rehome`'s daemon half — see [`enable_after_config`]'s own doc for why
/// this split exists and what `client: None` means. Sends the same
/// `DisableDog` request `disable` does: stopping an adopted dog to forget
/// it is the same stop `disable` already performs, and `rehome_dog`
/// (called by [`rehome`] above, before this half ever runs) is what makes
/// the difference — it also erases the registration `disable` leaves
/// alone.
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
/// shepherd — this module's own doc gives the reasoning shared with `shep
/// flush --daemon`, the other verb that answers from a file rather than the
/// socket. [`barks::read`] is already the forgiving half of that file's own
/// contract: a line a writer died mid-append leaves unparseable costs that
/// one record, not the whole read, so nothing here has to re-implement
/// that tolerance.
///
/// `--tail N` takes the LAST N records — [`BarksArgs::tail`]'s own doc, and
/// [`barks::read`]'s: oldest first, so the tail of that `Vec` is the most
/// recent N, in the same newest-last order the untailed read already has.
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

// `unix` because the adopt-vetting cases read a candidate's execute bits and its ancestors' world-writable bit — guarantees the Windows tier
// deliberately makes differently, each argued at its own call site
// above. What Windows claims instead is covered by `tests/cli_e2e.rs`
// and by the real-flock verification in the Windows port's own notes;
// this module's unix coverage is unchanged.
#[cfg(all(test, unix))]
mod tests {
    use shep_client::testing::{
        fake_client_capturing_envelopes, fake_client_replying_err, sample_ack, sample_info,
        serve_one_request,
    };
    use shep_core::protocol::RpcErrorCode;

    use super::*;
    use crate::cli::Format;

    /// Every test in this module drives one of the dog verbs under
    /// `--format table` -- none of them exercises the JSON envelope -- so
    /// `fmt` is fixed here rather than threaded through every call site.
    fn streams<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Streams<'a> {
        Streams {
            out,
            err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        }
    }

    /// fails if `enable` sends anything but `EnableDog` with the name and
    /// the source it was given — the class of bug that left `restart` and
    /// `delete` sending `Request::Stop` with every test green.
    /// `enable_of_an_adopted_dog_sends_the_path_the_config_recorded` is the
    /// half that pins where the source comes from in the first place.
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

    /// The defect this test exists for: `shep adopt <path> --name otel` recorded
    /// the binary, and the `shep enable otel` after it sent `BuiltIn`
    /// regardless — so the shepherd spawned `shep dog otel`, an argv branch
    /// of this binary, the operator's own binary never ran, and nothing
    /// reported an error anywhere.
    ///
    /// Driven through `enable` end to end rather than through
    /// [`enable_after_config`], because the lookup that was missing lives
    /// in the config half; a fixture that only binds the socket
    /// ([`serve_one_request`], not `fake_client_*`) is what lets `enable`
    /// perform its own `Client::connect` the way it does in production.
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

    /// The bug this test exists for: `shep enable` scaffolded `[dog.<name>]`
    /// into `shep.toml` while a dog's config had already moved to
    /// `dogs.toml`, so an operator who enabled a dog and then configured it
    /// where the docs say to had a name in both files. The migration refuses
    /// that, correctly, and the daemon exits 4 with the flock unsupervised.
    /// Enabling must leave nothing behind that a later boot can collide with.
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

        // The operator then configures the dog where `docs/dogs.md` now
        // tells them to, and the next `shep muster` runs the migration.
        std::fs::write(&paths.dogs_config, "[metrics]\nbind = \"127.0.0.1:9615\"\n").unwrap();
        crate::commands::dog_migration::migrate_dog_sections(&paths)
            .expect("a boot after an enable must not refuse over a section enable wrote");
    }

    /// Task 6 / spec G4, the `dogs.rs` spelling of the same bug `shep flock`
    /// had: `Client::connect(..).ok()` folded a handshake REFUSAL into
    /// `None`, exactly as it folds a genuine absence into `None` — so a live
    /// shepherd that refused was reported as "will start with the next
    /// shepherd" instead of the refusal.
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

    /// fails if a `shep enable` with no shepherd running is reported as a
    /// failure. The config edit is the part the operator asked for, and it
    /// landed; the dog comes up with the next boot. A non-zero exit here
    /// would make `shep enable` unusable in a provisioning script that
    /// configures a host before starting anything.
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

    /// fails if `shep enable` accepts a name that answers to no dog.
    ///
    /// `dog_source` reads built-in-ness as an ABSENCE from `[daemon]
    /// adopted_dogs`, so before this guard a typo was written into
    /// `enabled_dogs` as a built-in and exited zero. The shepherd then
    /// spawned `shep dog <typo>` on the restart ladder, `dog::run_dog`
    /// refused the name once per attempt until the budget was spent, and
    /// `shep dogs` reported its `SOURCE` as `built-in` throughout -- a name
    /// that is not one.
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

    /// fails if the refusal names only the built-ins. An operator with dogs
    /// adopted is the one likeliest to have mistyped one of THEIR names, so
    /// the adopted set is the half of the list they need.
    ///
    /// Read inside `enable`'s own `try_edit` closure, under the lock, rather
    /// than by a second read afterwards: a concurrent `shep adopt` landing
    /// between the two would make the message name a set that was never the
    /// one the refusal was decided against.
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

    /// fails if the guard swallows the case it exists to allow through: a
    /// name `shep adopt` recorded is a dog, and `enable` must still take it.
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

    /// fails if `disable` grows the guard `enable` just did.
    ///
    /// It must not: a `shep.toml` written before that guard existed -- or by
    /// hand -- can already carry a name that answers to no dog, and `shep
    /// disable <name>` is the only way to get it back out of `enabled_dogs`.
    /// A symmetrical check here would strand exactly the operators the
    /// guard is meant to rescue, with a restart-looping dog and no verb that
    /// will remove it.
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

    /// fails if the refusal's list grammar loses a comma or an `and`. The
    /// two-name case is what every `$SHEP_HOME` with nothing adopted gets,
    /// and the three-name case is the first one with an adopted dog in it.
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

    /// The name-collision guard `shep-daemon/src/rpc.rs`'s `EnableDog` arm
    /// carries (Task 6): `start_dog` is idempotent by name, so an unmarked
    /// entry coming back means a sheep already holds `name`, and the daemon
    /// refuses with `InvalidConfig` naming the collision. This pins that the
    /// operator sees that message verbatim on stderr, not a bare code —
    /// this verb sits directly on top of that guard and must not swallow it.
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

    /// The `disable` sibling of
    /// `enable_asks_the_shepherd_to_start_that_dog_as_a_built_in`: fails if
    /// `disable` sends anything but `DisableDog` with the name it was given.
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

    /// The `disable` sibling of
    /// `enable_with_no_shepherd_writes_the_config_and_exits_zero`.
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

    /// `disable` reused `Delete`'s own selector path (Task 6's `rpc.rs`
    /// doc), so a dog not currently registered answers `NotFound` exactly as
    /// `shep stop` would for a selector matching nothing — the config edit
    /// still lands (`disable_with_no_shepherd_writes_the_config_and_exits_zero`
    /// pins that half); this pins that the daemon's own report still reaches
    /// the operator rather than being swallowed as a false success.
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

    /// Sets `mode` on `path`, for the permission-bit cases below.
    fn chmod(path: &Path, mode: u32) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, mode);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// The modes `enable` cannot have, and the reason the two verbs
    /// are split. fails if any of them is reported as one of the others —
    /// "not executable" for a path that does not exist sends an operator to
    /// `chmod` a file that is not there.
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

        // The same file, now executable: the ONLY thing that changed is the
        // mode bit, so a `vet_binary` that refused for some other reason
        // fails here rather than passing for the wrong one.
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

    /// fails if a binary any local user can rewrite is adopted. The path is
    /// vetted once, here, and then exec'd by the daemon at the shepherd's
    /// own trust level on every restart with no re-vetting — so a
    /// world-writable path is a standing way for any user on the box to run
    /// code as the shep user, indefinitely.
    ///
    /// Both halves matter: a writable DIRECTORY is as good as a writable
    /// file, because it lets the binary be renamed away and a replacement
    /// dropped in its place.
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
    /// on `$1` the way a real dog does: `version_body` for the version
    /// flag, `schema_body` for the schema flag, and nothing at all for any
    /// other argument. A dog's answer is text on stdout and an exit status,
    /// and a shell script is the shortest thing that can produce any pair of
    /// those on demand.
    ///
    /// The dispatch is not decoration. A fixture that answers every flag the
    /// same way answers the schema flag with its version text, which is
    /// unreadable JSON and earns a warning, so nine tests about something
    /// else would each carry a notice nobody asserts on and a real
    /// regression in that warning could hide among them.
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

    /// A dog that answers the version flag with `body`, and the schema flag
    /// the way every dog written before this contract does: with nothing.
    /// A binary built on clap exits non-zero on a flag it has never heard
    /// of, and one that ignores its arguments prints its version; both are
    /// [`DogSchema::Silent`], and silence is the shorter of the two to
    /// write.
    fn dog_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        probe_script(dir, name, body, "exit 0")
    }

    /// fails if a dog built against a protocol this shep cannot speak is
    /// adopted anyway — G11's whole point: it would become an
    /// online-and-idle entry whose failure surfaces days later, at a
    /// handshake nobody is watching.
    ///
    /// Pins both numbers and both fixes in the message, per the spec's
    /// "a message naming the fix is the feature".
    ///
    /// Mutation check: dropping the `protocol != PROTOCOL_VERSION` guard
    /// reddens this — the vet returns `Ok` and the adopt succeeds.
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

    /// fails if a version difference is treated as a protocol difference.
    /// The two numbers answer different questions (`docs/dogs.md`): a
    /// third-party dog's crate version has no relationship to shep's own,
    /// so it is reported and never compared, while the protocol is the
    /// only one that can refuse.
    ///
    /// Mutation check: refusing on anything but the protocol reddens this.
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

    /// fails if a dog that says nothing at all is refused. Every dog that
    /// exists predates this contract, including both published ones, so
    /// refusing silence would break the entire population —
    /// `docs/dogs.md` promises in published prose that it never happens.
    /// The protocol is unknown and prediction degrades to G8's
    /// post-connection detection for that dog alone.
    ///
    /// Mutation check: making the parser answer `Some(PROTOCOL_VERSION)`
    /// for empty output reddens the `answer` assertion here rather than
    /// leaving it passing for the wrong reason.
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

    /// fails if the version line alone is treated as a protocol claim. A
    /// clap-built dog prints `<name> <version>` for free without ever
    /// having heard of `shep-protocol`, so this is the commonest partial
    /// answer there is, and it is an unknown protocol rather than a
    /// mismatched one.
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
    /// inherits a wall-clock bound. At the production one second, a test
    /// asking whether a version string parses fails when the machine is
    /// busy, which is the runner reporting on itself. Thirty seconds is not
    /// a claim that a probe should ever take that long; it is far enough
    /// above any contention this suite produces that timing stops being a
    /// variable in tests that never meant to measure it.
    ///
    /// `a_candidate_that_never_exits_does_not_hang_the_vet` deliberately
    /// does NOT use this: it is the one test that is about the bound, so it
    /// passes the real budget.
    const TEST_BUDGET: Duration = Duration::from_secs(30);

    /// fails if the probe hands a candidate the operator's environment.
    ///
    /// The candidate is somebody else's binary, and `vet_binary` warns
    /// about a group-writable one rather than refusing it, so a group
    /// member can replace an adopted dog and have their code run on the
    /// next `shep restart <name>`. If the probe inherited the operator's
    /// shell, that code would be reading whatever they had exported.
    ///
    /// `CARGO_PKG_NAME` is the sentinel because cargo sets it for this very
    /// process and it is not on the daemon's allowlist, so no environment
    /// has to be mutated to run this: a leak would show up as the child
    /// seeing a variable this test never gave it.
    #[cfg(unix)]
    #[test]
    fn a_probe_runs_with_the_daemons_environment_and_not_the_operators() {
        assert!(
            std::env::var("CARGO_PKG_NAME").is_ok(),
            "the sentinel has to be in this process for its absence downstream to mean anything"
        );
        let dir = tempfile::tempdir().unwrap();
        // The sentinel has to be the LAST field of line 1, because that is
        // the only part `parse_version_answer` keeps. An earlier version of
        // this test put it first and asserted on `version`, so the string it
        // checked could never have contained the sentinel: it passed with
        // `env_clear` removed, which is the definition of proving nothing.
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

    /// fails if a candidate that never exits hangs `adopt`. The vet spawns
    /// somebody else's binary and cannot assume it is well behaved, and a
    /// hung `adopt` is worse than an unknown protocol: the operator has no
    /// output, no refusal, and nothing to interrupt but the command.
    ///
    /// Mutation check: replacing the bounded wait with a blocking
    /// `child.wait()` hangs this test rather than reddening it, which is
    /// the point — the assertion on elapsed time is what turns that into a
    /// failure a CI run can report.
    #[test]
    fn a_candidate_that_never_exits_does_not_hang_the_vet() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dog_script(dir.path(), "shep-otel", "sleep 30");

        let started = std::time::Instant::now();
        // The real budget, not `TEST_BUDGET`: this test IS the bound.
        let vetted = vet_binary_within(&bin, dir.path(), "otel", VERSION_BUDGET).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(
            vetted.answer, None,
            "a candidate that never answered is unknown"
        );
        // Absolute rather than a multiple of `VERSION_BUDGET`: the bound
        // has to stay wrong when the budget is what got mutated away. Ten
        // seconds against a one-second budget and a candidate that sleeps
        // thirty -- wide enough for a loaded CI runner, narrow enough that
        // waiting on the candidate reddens this rather than passing slowly.
        assert!(
            elapsed < Duration::from_secs(10),
            "the vet is bounded by its own budget, not by the candidate: {elapsed:?}"
        );
    }

    /// fails if a candidate decides how much of this machine's memory a
    /// probe takes. The read is on a thread with no bound but the budget
    /// and the writer's speed, so a binary that spews reached roughly 290MB
    /// resident for a second of it, and `adopt` spawns that binary twice.
    ///
    /// Twice the cap is written, so a read that stopped anywhere else shows
    /// up in the length rather than only in the memory it took. `trap ''
    /// PIPE` is what lets the script reach its own `exit 0` after shep
    /// closes the pipe underneath it, since a candidate killed by SIGPIPE
    /// would answer `None` for its exit status and say nothing about where
    /// the read stopped.
    ///
    /// Mutation check: dropping the `take` reddens this on the length.
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

    /// fails if an answer from a candidate that exited non-zero is
    /// believed. `docs/dogs.md` says a dog answers on stdout **and exits
    /// 0**; a non-zero exit means the run that printed those lines failed,
    /// so what it printed is not an answer — least of all one that can
    /// refuse an adopt.
    ///
    /// Mutation check: dropping the `status.success()` test reddens this —
    /// the mismatched protocol on stdout becomes a refusal.
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

    /// fails if unparseable output is a refusal. A binary that answers
    /// `--version` with a usage message, a banner, or bytes is answering
    /// nothing shep asked for, and shep has no standing to refuse a dog
    /// over the shape of text it never promised to print.
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

    /// fails if the parser stops tolerating what `docs/dogs.md` says it
    /// tolerates: a name on line 1 that is ignored, blank lines, key order,
    /// unknown keys, and above all a reserved `shep-` key that does not
    /// exist yet. That last one is the compatibility promise the published
    /// contract makes on behalf of a parser nobody has written — a third
    /// number gets its own line later, and a shep that predates it must
    /// ignore the line rather than refuse the dog.
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

    /// A dog whose schema half is what the test is about. The version half
    /// always names this shep's own protocol, so a schema test never fails
    /// for a reason that has nothing to do with the schema.
    fn two_flag_dog(dir: &Path, schema_body: &str) -> PathBuf {
        probe_script(
            dir,
            "shep-otel",
            &format!("echo 'shep-otel 0.1.3'\necho 'shep-protocol: {PROTOCOL_VERSION}'"),
            schema_body,
        )
    }

    /// fails if a dog that answers the schema flag with real JSON Schema is
    /// not asked, or is asked and not read. This is the whole point of the
    /// second probe: everything below is a way of getting nothing, and this
    /// is the way of getting something.
    ///
    /// Mutation check: never spawning the schema flag, or dropping the
    /// parse, leaves `schema` anything but `Published` and reddens this.
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

    /// fails if a run that printed a schema and then failed is believed, or
    /// is treated as a fault. `docs/dogs.md` asks for stdout AND an exit 0,
    /// so a non-zero exit means the run that printed those bytes did not
    /// work -- and a dog with a broken schema flag may still scrape
    /// perfectly, so it is adopted with no schema and no warning.
    ///
    /// Mutation check: dropping the exit-status test records that `{}` as a
    /// published schema and reddens the first assertion.
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

    /// fails if silence on the schema flag is anything but a dog with no
    /// schema. Every dog written before this contract is in that group: it
    /// exits without printing, or prints its usage on stderr, and a warning
    /// for each of them is a line about the ordinary case.
    ///
    /// Mutation check: treating empty output as unparseable JSON reddens
    /// both assertions here at once.
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

    /// fails if a dog that answers the schema flag with something that is
    /// not JSON is refused, or is passed over in silence. It is the one
    /// shape that earns a warning: the dog meant to answer and its answer
    /// cannot be read, which is a bug in that dog its author can fix.
    /// Exactly one warning, because the count is what an operator reads.
    ///
    /// Mutation check: dropping the `Unreadable` arm silences the warning
    /// and reddens the count.
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

    /// Every file under `dir`, recursively, as text. A file shep wrote that
    /// is not UTF-8 is skipped rather than failing the walk; none of them
    /// are today, and a binary one could not carry the marker anyway.
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

    /// fails if a schema reaches anything shep writes. Decision 7: asked
    /// fresh, stored nowhere, because `cargo install` replaces a dog's
    /// binary with nothing watching and a stale schema is worse than a
    /// stale version number, since it mislabels which field is a
    /// credential.
    ///
    /// Nothing stores it today, and it is guaranteed structurally: the
    /// function that records an adopted dog has no schema to take. A
    /// structural guarantee is exactly the kind a later signature change
    /// removes with nothing going red, so this asserts on the files rather
    /// than on the shape of a call. The whole home is walked, not just
    /// `shep.toml`, so a schema parked in `dogs.toml` or a cache beside it
    /// would be caught too. The dog's binary lives in its own directory
    /// because the fixture script contains the schema text it prints.
    ///
    /// Mutation check: appending the schema to `shep.toml` after the edit
    /// reddens this.
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

    /// fails if a schema's own text can reach a log through `Debug`. A dog
    /// author's `Default` is what a `default` in the schema carries, and it
    /// is the same field the secret marker exists to keep off a screen, so
    /// the shape goes out and the content does not (IR-41). Exact string,
    /// because the failure mode is somebody replacing this with a derive.
    #[test]
    fn debug_reports_that_there_is_a_schema_and_never_what_is_in_it() {
        let schema = DogSchema::Published(serde_json::json!({"token": "hunter2"}));
        assert_eq!(format!("{schema:?}"), "Published(..)");
        assert_eq!(format!("{:?}", DogSchema::Silent), "Silent");
        assert_eq!(format!("{:?}", DogSchema::Unreadable), "Unreadable");
    }

    /// fails if a group-writable deployment directory is refused rather
    /// than warned about — that is a legitimate, common arrangement with a
    /// trusted deploy group, and refusing it would block real setups — and
    /// fails if the warning is silent, since the operator's only chance to
    /// hear about it is this one command.
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

    /// fails if a refused adopt still edits `shep.toml`. The vetting is
    /// worth nothing if the config records the binary anyway and the next
    /// boot tries to run it.
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

    /// The refusal itself is not thrown away — `adopt`'s own report is the
    /// only place an operator learns which of the three modes it was.
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

    /// fails if `adopt` sends anything but `EnableDog` with the name it was
    /// given and an `Adopted` source carrying the vetted path — the `adopt`
    /// sibling of `enable_asks_the_shepherd_to_start_that_dog_as_a_built_in`.
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

    /// The `adopt` sibling of `enable_with_no_shepherd_writes_the_config_and_exits_zero`.
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

    /// fails if `adopt` stops defaulting the name when `--name` is omitted,
    /// or defaults it from the whole file name instead of the stripped
    /// stem: `shep-otel` must default to `otel`, the way `cargo` strips
    /// `cargo-` from its own external subcommands.
    ///
    /// Mutation check: reverting `default_dog_name` to return `stem`
    /// unstripped reddens this (`shep-otel` recorded verbatim) rather than
    /// `otel`.
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

    /// fails if `adopt` accepts a name that already belongs to a built-in
    /// verb or alias -- such a dog could never be reached, since
    /// `dispatch_adopted_dog` (`lib.rs`) only ever runs once clap has
    /// already failed to match the name against a real subcommand.
    ///
    /// Mirrors [`a_refused_adopt_leaves_the_config_untouched`]'s own shape:
    /// the refusal must happen before `shep.toml` is touched at all, same
    /// as every other `AdoptRefusal`.
    ///
    /// Mutation check: reverting `collides_with_a_verb` to always return
    /// `false` reddens this (`Success` and a written config instead of
    /// `InvalidConfig` and an absent one).
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
        // "stop" is a real verb; "ls" is `flock`'s own visible alias. Both
        // must be refused the same way.
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

    /// fails if `adopt` refuses a name collision only after already
    /// vetting the candidate -- `vet_binary` spawns the binary as part of
    /// vetting (to prove this kernel can exec it), so a refusal that runs
    /// after `vet_binary` has already run the thing it refuses. The
    /// outcome alone (`InvalidConfig`, no `shep.toml` write) is identical
    /// whichever order runs first -- `adopt_refuses_a_name_that_collides_with_a_built_in_verb`
    /// above cannot tell them apart -- so this test distinguishes the two
    /// orders by which REFUSAL REASON reaches the operator instead of by a
    /// spawn side effect: `args.path` here names nothing on disk, so
    /// `vet_binary` fails at its very first check (`std::fs::metadata`,
    /// `AdoptRefusal::Missing`) without ever attempting to spawn anything.
    /// If the collision check runs first, the operator sees the collision
    /// message; if `vet_binary` runs first, they see "no file exists at
    /// that path" instead, for a name that was always going to be refused
    /// either way. A process-spawn side effect (a marker file a script
    /// writes) was tried first and dropped: `vet_binary`'s own exec probe
    /// polls for up to 50ms before falling back to a hard kill, and that
    /// race was not reliably observable from this test's own harness --
    /// this path-shaped signal has no timing to race at all.
    ///
    /// Mutation check: moving the `collides_with_a_verb` check back to
    /// after `vet_binary` reddens this -- the reported refusal becomes
    /// `AdoptRefusal::Missing`'s message instead of the collision one.
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

    /// fails if `resolve_adopt_path` stops trying a literal path first, or
    /// tries the other two steps when the literal one already exists --
    /// the base case every other `resolve_adopt_path` test builds on.
    #[test]
    fn resolve_adopt_path_prefers_a_literal_path_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("thing");
        std::fs::write(&binary, "").unwrap();

        let resolved = resolve_adopt_path(&binary, None, None);
        assert_eq!(resolved, binary);
    }

    /// fails if `shep adopt '~/.cargo/bin/shep-log-rotate'` stops working
    /// -- issue 1's second repro. `resolve_adopt_path` must expand `~/`
    /// against the `home` it is given when the literal path (a literal
    /// `~` directory, which does not exist) is not there.
    ///
    /// Mutation check: reverting the tilde-expansion step (returning
    /// `raw` unchanged whenever the literal path is missing, skipping
    /// straight to the `$PATH` lookup) reddens this -- `resolve_adopt_path`
    /// would return the untouched `~/...` path, which `vet_binary` then
    /// reports `Missing` for.
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

    /// fails if `shep adopt shep-log-rotate` stops working when the binary
    /// is only on `$PATH` -- issue 1's first repro (`cargo install
    /// shep-log-rotate` puts it there under its own name).
    ///
    /// Mutation check: reverting the `$PATH` step to return `None`
    /// unconditionally reddens this the same way the tilde mutation above
    /// reddens its own test.
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

    /// fails if a `$PATH` lookup fires for a name that already names a
    /// directory of its own (`./thing`, `bin/thing`, `/opt/thing`) -- a
    /// shell never searches `$PATH` for one of those, and neither should
    /// this: a same-named file elsewhere on `$PATH` would silently adopt
    /// the wrong binary.
    #[test]
    fn resolve_adopt_path_does_not_path_search_a_name_with_a_directory_component() {
        let path_dir = tempfile::tempdir().unwrap();
        // A file that WOULD match if `$PATH` were searched -- proving the
        // guard, not just an absent or non-executable one.
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

    /// fails if none of the three resolution steps finding nothing stops
    /// returning `raw` unchanged -- the funnel that keeps a plain missing
    /// path reporting the same [`AdoptRefusal::Missing`] it always has,
    /// rather than a resolution-specific error.
    #[test]
    fn resolve_adopt_path_returns_raw_unchanged_when_nothing_resolves() {
        let raw = Path::new("/nonexistent/shep-nothing");
        assert_eq!(resolve_adopt_path(raw, None, None), raw);
    }

    /// fails if `default_dog_name` stops stripping exactly one leading
    /// `shep-`, keeps stripping a binary that never had the prefix, or
    /// leaves a pathological `shep-` binary defaulting to an empty
    /// (unreachable) name.
    #[test]
    fn default_dog_name_strips_one_leading_shep_prefix_and_no_further() {
        assert_eq!(
            default_dog_name(Path::new("/opt/bin/shep-log-rotate")),
            "log-rotate"
        );
        assert_eq!(default_dog_name(Path::new("/opt/bin/otel")), "otel");
        // Stripping the prefix here would leave "", an unreachable name --
        // the whole stem is kept instead.
        assert_eq!(default_dog_name(Path::new("/opt/bin/shep-")), "shep-");
    }

    /// fails if `collides_with_a_verb` misses a real subcommand name, an
    /// alias of one, or refuses a name that names neither.
    ///
    /// Mutation check: reverting `collides_with_a_verb` to always return
    /// `false` reddens the first two assertions; always `true` reddens the
    /// third.
    #[test]
    fn collides_with_a_verb_covers_names_and_visible_aliases() {
        assert!(collides_with_a_verb("stop"), "a real verb must collide");
        assert!(collides_with_a_verb("ls"), "flock's own alias must collide");
        assert!(
            !collides_with_a_verb("watchdog"),
            "an arbitrary name must not collide"
        );
    }

    /// fails if `rehome` behaves as `disable` does — the whole difference
    /// between the two verbs is that this one forgets the registration and
    /// the configuration, including the `adopted_dogs` entry `disable`
    /// deliberately keeps (`disable_with_no_shepherd_writes_the_config_and_exits_zero`
    /// pins the keeping half of that contrast).
    ///
    /// The configuration half is two files: a `[dog.otel]` an un-migrated
    /// `shep.toml` still carries, and the `[otel]` in `dogs.toml` that is
    /// where one lives now. `metrics` is beside it to catch a rewrite that
    /// forgets more than it was asked to.
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

    /// fails if the `dogs.toml` rewrite goes back to `std::fs::write`.
    /// That file is where an operator pastes a Discord or Slack webhook
    /// URL, which is a bearer token in a path, so it is `0600` and the
    /// rewrite installs a staged inode carrying that mode rather than
    /// trusting the mode it found. A file an older shep (or an operator's
    /// own `touch`) left wide comes back narrow, the same property
    /// `shep_toml`'s `editing_a_world_readable_config_leaves_it_owner_only`
    /// pins for the other file.
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

    /// Rehoming a dog nobody ever configured is an ordinary thing to do:
    /// no `dogs.toml` exists, and this verb must not invent an empty one
    /// or fail over its absence.
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

    /// fails if `rehome` sends anything but `DisableDog` with the name it
    /// was given — the `rehome` sibling of `disable_asks_the_shepherd_to_stop_that_dog`.
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

    /// The `rehome` sibling of `disable_with_no_shepherd_writes_the_config_and_exits_zero`.
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
    /// `ops`. `barks` tests below only ever care about ordering and
    /// filtering, so every field but the two callers vary is fixed.
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

    /// fails if `barks` connects to a shepherd, ever renders the ring in
    /// any order but the one it was written in, or drops the `WHEN` column
    /// — this is the read-the-file contract `commands/dogs.rs`'s own
    /// module doc states for this verb, and no client fixture exists
    /// anywhere in this function's path to accidentally satisfy. `#[test]`,
    /// not `#[tokio::test]`: `barks` is a plain synchronous fn, exactly
    /// like `logs::flush_daemon` — the other verb that answers from a file
    /// with no socket in reach at all.
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

    /// fails if `--tail N` shows anything but the LAST N records — the
    /// distinction `BarksArgs::tail`'s own doc draws against "the first
    /// N", which would show an operator the oldest history instead of the
    /// most recent.
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

    /// `--tail` past the length of the whole ring is the whole ring, not a
    /// usage error or a panic — `history.len().saturating_sub(tail)`'s own
    /// reason for being `saturating`.
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

    /// fails if a ring nobody has ever written to is treated as a failure —
    /// no barks yet is the state every fresh `$SHEP_HOME` starts in, and
    /// `barks::read`'s own doc already makes that `Ok(vec![])` rather than
    /// an I/O error; this pins that `dogs::barks` still exits `Success` and
    /// prints headers rather than nothing (`render_table`'s own rule for an
    /// empty payload).
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

    /// fails if one line a writer died mid-append leaves refuses the whole
    /// read — the CONTEXT this task was handed is explicit that a corrupt
    /// trailing line must cost the reader that one record, not the file,
    /// and `barks::read` (shep-core) is where that tolerance actually
    /// lives; this test is `dogs::barks`' own proof that nothing between
    /// here and there swallows it.
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
