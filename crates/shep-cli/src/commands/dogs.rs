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
//! `rehome`.** [`ShepToml::save`] runs before any of the three ever tries
//! the socket: if the RPC that follows fails or never gets attempted, the
//! config still says what the operator asked for, and the next boot brings
//! it up — which is the state the operator actually wanted. The reverse
//! order would leave a dog running (or stopped) that no boot restores.
//!
//! **`adopt` reverses that order for its own first step.** [`vet_binary`]
//! runs BEFORE `shep.toml` is touched at all — a refused adopt must leave
//! the config exactly as it was, because there is something here `enable`
//! structurally cannot have: a binary that might not exist, might not be a
//! file, might have no execute bit, or might be something this kernel
//! cannot run. Once vetting passes, `adopt` rejoins the same config-first
//! order as the other three.
//!
//! **None of the four autostarts a shepherd** — decision 11. Each, against
//! no running daemon, writes the config, reports what will happen with the
//! next shepherd, and exits [`ExitCode::Success`]. Autostarting a whole
//! supervisor as a side effect of a config edit would be a surprise out of
//! proportion to the ask; `shep muster` is the one verb that autostarts,
//! and it says so in its own help text.

use std::path::{Path, PathBuf};
use std::process::Command;

use shep_client::Client;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{DogSource, Request, Response};

use crate::cli::{AdoptArgs, Format};
use crate::commands::shep_toml::{ShepToml, ShepTomlError};
use crate::exit::ExitCode;
use crate::output::{
    DogAdoptedRow, DogDisabledRow, DogEnabledRow, DogRehomedRow, Streams, emit, emit_error,
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
/// [`ShepTomlError::Parse`] is a config-validation failure — the same
/// category [`ExitCode::InvalidConfig`] names for a bad Flockfile
/// (`commands::lifecycle::target_exit_code`) — while
/// [`ShepTomlError::Io`] has no more specific code than
/// [`ExitCode::Failure`].
fn fail_config(streams: &mut Streams<'_>, fmt: Format, err: &ShepTomlError) -> ExitCode {
    let code = match err {
        ShepTomlError::Io { .. } => ExitCode::Failure,
        ShepTomlError::Parse { .. } => ExitCode::InvalidConfig,
    };
    let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
    code
}

/// `shep enable <name>`: writes the config, and starts the dog if a
/// shepherd is running.
pub async fn enable(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode {
    let mut cfg = match ShepToml::open(&paths.daemon_config) {
        Ok(cfg) => cfg,
        Err(err) => return fail_config(streams, fmt, &err),
    };
    cfg.enable_dog(name);
    if let Err(err) = cfg.save() {
        return fail_config(streams, fmt, &err);
    }
    let client = Client::connect(&paths.socket).await.ok();
    enable_after_config(streams, fmt, name, client.as_ref()).await
}

/// `enable`'s daemon half, split out from [`enable`] so a test can drive it
/// against a [`shep_client::testing`] fake without racing a second, real
/// connection to the same socket the fake's own fixture already opened —
/// [`crate::commands::lifecycle::resolve_target`] is split out of `start`
/// for the same reason: hermetic testability of the part that has a seam.
///
/// `client: None` is [`enable`]'s own `Client::connect(..).ok()` — every way
/// a connection can fail is folded into "no shepherd running" here, matching
/// decision 11: this verb does not distinguish a stale socket file from a
/// genuinely absent daemon, because a provisioning script configuring a host
/// before starting anything must not have to.
async fn enable_after_config(
    streams: &mut Streams<'_>,
    fmt: Format,
    name: &str,
    client: Option<&Client>,
) -> ExitCode {
    let Some(client) = client else {
        let row = DogEnabledRow {
            name: name.to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: NO_SHEPHERD_ENABLE_STATUS.to_string(),
        };
        return write_outcome(emit(&mut *streams.out, fmt, "enable", row));
    };
    // Always `BuiltIn`: `shep adopt` (a later verb) is the one that carries
    // a path. An `EnableDog` reaching a name a sheep already holds comes
    // back as `RpcErrorCode::InvalidConfig` with the daemon's own message
    // naming the collision (`shep-daemon/src/rpc.rs`'s `EnableDog` arm) —
    // the `Err` arm below surfaces that message verbatim rather than a bare
    // code, which is already the clear report an operator needs.
    let request = Request::EnableDog {
        name: name.to_string(),
        source: DogSource::BuiltIn,
    };
    match client.request(request).await {
        Ok(Response::DogStarted(info)) => {
            let row = DogEnabledRow {
                name: name.to_string(),
                source: DogSource::BuiltIn,
                shepherd_acted: true,
                status: info.status.to_string(),
            };
            write_outcome(emit(&mut *streams.out, fmt, "enable", row))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// `shep disable <name>`: removes it from the config, and stops it if a
/// shepherd is running.
pub async fn disable(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode {
    let mut cfg = match ShepToml::open(&paths.daemon_config) {
        Ok(cfg) => cfg,
        Err(err) => return fail_config(streams, fmt, &err),
    };
    cfg.disable_dog(name);
    if let Err(err) = cfg.save() {
        return fail_config(streams, fmt, &err);
    }
    let client = Client::connect(&paths.socket).await.ok();
    disable_after_config(streams, fmt, name, client.as_ref()).await
}

/// `disable`'s daemon half — see [`enable_after_config`]'s own doc for why
/// this split exists and what `client: None` means.
async fn disable_after_config(
    streams: &mut Streams<'_>,
    fmt: Format,
    name: &str,
    client: Option<&Client>,
) -> ExitCode {
    let Some(client) = client else {
        let row = DogDisabledRow {
            name: name.to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: NO_SHEPHERD_DISABLE_STATUS.to_string(),
        };
        return write_outcome(emit(&mut *streams.out, fmt, "disable", row));
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
                source: DogSource::BuiltIn,
                shepherd_acted: true,
                status: DISABLED_STATUS.to_string(),
            };
            write_outcome(emit(&mut *streams.out, fmt, "disable", row))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// Why a binary cannot be adopted.
///
/// The three modes `enable` structurally cannot have, and the reason the
/// two verbs are split rather than one verb carrying an `--exec` flag: a
/// dog that already ships inside this binary has no path to be missing, no
/// permission bit to be unset, and no architecture to be wrong.
#[derive(Debug, PartialEq, Eq)]
pub enum AdoptRefusal {
    /// Nothing exists at that path.
    Missing,
    /// It exists and is not a file (a directory, most often a `bin/` the
    /// operator meant to point inside of).
    NotAFile,
    /// It exists and no execute bit is set for anyone.
    NotExecutable,
    /// It exists, is executable, and this kernel refused to exec it —
    /// the wrong architecture, or an interpreter line naming something
    /// absent.
    WillNotExec {
        /// What `exec` reported.
        reason: String,
    },
}

impl std::fmt::Display for AdoptRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "no file exists at that path"),
            Self::NotAFile => write!(f, "that path is not a file"),
            Self::NotExecutable => write!(f, "no execute bit is set on that file"),
            Self::WillNotExec { reason } => {
                write!(f, "this kernel refused to run that file: {reason}")
            }
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
/// The three checks run in this order — existence, file-ness, permission
/// bit — each refusing before the next one runs, so a refusal never claims
/// the wrong cause (`NotExecutable` for a path that does not exist would
/// send an operator to `chmod` a file that is not there). The fourth check,
/// [`AdoptRefusal::WillNotExec`], is answered by actually trying it —
/// spawned with the same (empty) argument list the daemon uses for an
/// adopted dog (`shep-daemon/src/dogs.rs`'s `dog_app`), and killed once it
/// is confirmed either to have run or to be still running. The question is
/// whether this kernel can exec this file, and the only authority on that
/// is this kernel; reading a header instead would mean writing a second,
/// partial loader that disagrees with the real one — on a fat Mach-O, on a
/// shebang naming an absent interpreter, on a binary needing a missing
/// dynamic library.
///
/// # Errors
/// The refusal, which the caller renders. Nothing here is a shep fault, so
/// none of these is an [`ExitCode::Internal`].
pub fn vet_binary(path: &Path) -> Result<PathBuf, AdoptRefusal> {
    let metadata = std::fs::metadata(path).map_err(|_| AdoptRefusal::Missing)?;
    if !metadata.is_file() {
        return Err(AdoptRefusal::NotAFile);
    }
    // No execute bit set for anyone: owner (0o100), group (0o010), or
    // other (0o001). `PermissionsExt` is always in scope here — this
    // module compiles only under `#[cfg(unix)]` (`main.rs`'s own `mod
    // commands` gate), so there is no non-unix build of this function to
    // guard against.
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(AdoptRefusal::NotExecutable);
    }
    // `metadata` above already proved something exists at `path`, so this
    // canonicalize is not itself a new place to observe `Missing` — a
    // symlink loop or a race with something deleting the file between the
    // two calls is the only way it could fail, and either way there is
    // nothing more specific than `Missing` to report.
    let canonical = path.canonicalize().map_err(|_| AdoptRefusal::Missing)?;
    // Spawned with no arguments — an adopted dog is run exactly this way
    // (`dog_app`'s own doc) — and torn down unconditionally: `kill` is
    // ignored (a process that already exited is not a failure to vet), but
    // `wait` always runs, on every path out of this match, so no zombie
    // survives a refusal or a success.
    match Command::new(&canonical).spawn() {
        Err(err) => Err(AdoptRefusal::WillNotExec {
            reason: err.to_string(),
        }),
        Ok(mut child) => {
            if let Some(reason) = macos_deferred_exec_failure(&mut child) {
                let _ = child.wait();
                return Err(AdoptRefusal::WillNotExec { reason });
            }
            let _ = child.kill();
            let _ = child.wait();
            Ok(canonical)
        }
    }
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
/// reports. [`ExitCode::InvalidConfig`] for all three modes — what's wrong
/// is the argument `adopt` was given, not shep's own state, the same
/// category a bad Flockfile value gets.
fn fail_adopt(
    streams: &mut Streams<'_>,
    fmt: Format,
    path: &Path,
    refusal: &AdoptRefusal,
) -> ExitCode {
    let code = ExitCode::InvalidConfig;
    let message = format!("{}: {refusal}", path.display());
    let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &message);
    code
}

/// `shep adopt <name> <path>`: vets a binary shep has never seen, records
/// it, and starts it if a shepherd is running.
pub async fn adopt(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &AdoptArgs,
) -> ExitCode {
    let path = match vet_binary(&args.path) {
        Ok(path) => path,
        Err(refusal) => return fail_adopt(streams, fmt, &args.path, &refusal),
    };
    let mut cfg = match ShepToml::open(&paths.daemon_config) {
        Ok(cfg) => cfg,
        Err(err) => return fail_config(streams, fmt, &err),
    };
    cfg.adopt_dog(&args.name, &path);
    if let Err(err) = cfg.save() {
        return fail_config(streams, fmt, &err);
    }
    let client = Client::connect(&paths.socket).await.ok();
    adopt_after_config(streams, fmt, &args.name, &path, client.as_ref()).await
}

/// `adopt`'s daemon half — see [`enable_after_config`]'s own doc for why
/// this split exists and what `client: None` means.
async fn adopt_after_config(
    streams: &mut Streams<'_>,
    fmt: Format,
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
        return write_outcome(emit(&mut *streams.out, fmt, "adopt", row));
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
            write_outcome(emit(&mut *streams.out, fmt, "adopt", row))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// `shep rehome <name>`: stops an adopted dog and forgets it entirely.
pub async fn rehome(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode {
    let mut cfg = match ShepToml::open(&paths.daemon_config) {
        Ok(cfg) => cfg,
        Err(err) => return fail_config(streams, fmt, &err),
    };
    // Read before `rehome_dog` erases it — the row below reports what this
    // verb forgot, and `None` (a name never adopted, or a built-in dog's
    // own name) is a legitimate answer, not a fault.
    let source = cfg.adopted_dog_path(name).map(|path| DogSource::Adopted {
        path: path.display().to_string(),
    });
    cfg.rehome_dog(name);
    if let Err(err) = cfg.save() {
        return fail_config(streams, fmt, &err);
    }
    let client = Client::connect(&paths.socket).await.ok();
    rehome_after_config(streams, fmt, name, source, client.as_ref()).await
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
    fmt: Format,
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
        return write_outcome(emit(&mut *streams.out, fmt, "rehome", row));
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
            write_outcome(emit(&mut *streams.out, fmt, "rehome", row))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    use super::*;

    fn streams<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Streams<'a> {
        Streams { out, err }
    }

    /// fails if `enable` sends anything but `EnableDog` with the name it was
    /// given and a `BuiltIn` source — the class of bug that left `restart`
    /// and `delete` sending `Request::Stop` with every test green.
    #[tokio::test]
    async fn enable_asks_the_shepherd_to_start_that_dog_as_a_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = enable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "metrics",
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
        let code = enable(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            "metrics",
        )
        .await;

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

    /// The name-collision guard `shep-daemon/src/rpc.rs`'s `EnableDog` arm
    /// carries (Task 6): `start_dog` is idempotent by name, so an unmarked
    /// entry coming back means a sheep already holds `name`, and the daemon
    /// refuses with `InvalidConfig` naming the collision. This pins that the
    /// operator sees that message verbatim on stderr, not a bare code —
    /// this verb sits directly on top of that guard and must not swallow it.
    #[tokio::test]
    async fn enable_reports_a_name_collision_with_the_daemons_own_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let message =
            "a sheep is already registered as `bark`; rename it or give the dog another name";
        let (client, _daemon) =
            fake_client_replying_err(&path, RpcErrorCode::InvalidConfig, message).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "bark",
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
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = disable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "bark",
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
        let mut seed = ShepToml::open(&paths.daemon_config).unwrap();
        seed.enable_dog("bark");
        seed.save().unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = disable(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            "bark",
        )
        .await;

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
        let path = dir.path().join("s.sock");
        let (client, _daemon) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = disable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "ghost",
            Some(&client),
        )
        .await;
        assert_eq!(code, ExitCode::NotFound);
    }

    /// The three modes `enable` cannot have, and the reason the two verbs
    /// are split. fails if any of them is reported as one of the others —
    /// "not executable" for a path that does not exist sends an operator to
    /// `chmod` a file that is not there.
    #[test]
    fn a_binary_shep_has_never_seen_is_vetted_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            vet_binary(&dir.path().join("nope")),
            Err(AdoptRefusal::Missing)
        );
        assert_eq!(vet_binary(dir.path()), Err(AdoptRefusal::NotAFile));

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(vet_binary(&plain), Err(AdoptRefusal::NotExecutable));

        // The same file, now executable: the ONLY thing that changed is the
        // mode bit, so a `vet_binary` that refused for some other reason
        // fails here rather than passing for the wrong one.
        let mut mode = std::fs::metadata(&plain).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&plain, mode).unwrap();
        assert_eq!(vet_binary(&plain).unwrap(), plain.canonicalize().unwrap());

        // Executable, and not something this kernel can run.
        let bogus = dir.path().join("bogus");
        std::fs::write(&bogus, b"\x7fELF\x00\x00\x00 not really").unwrap();
        let mut mode = std::fs::metadata(&bogus).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&bogus, mode).unwrap();
        assert!(matches!(
            vet_binary(&bogus),
            Err(AdoptRefusal::WillNotExec { .. })
        ));
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
            name: "otel".to_string(),
            path: dir.path().join("nope"),
        };
        let code = adopt(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            &args,
        )
        .await;

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
            name: "otel".to_string(),
            path: dir.path().join("nope"),
        };
        let code = adopt(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            &args,
        )
        .await;

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
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let binary = PathBuf::from("/usr/local/bin/shep-otel");
        let _ = adopt_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
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
            name: "otel".to_string(),
            path: binary,
        };
        let code = adopt(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            &args,
        )
        .await;

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

    /// fails if `rehome` behaves as `disable` does — the whole difference
    /// between the two verbs is that this one forgets the registration,
    /// including the `[dog.<name>]` table and the `adopted_dogs` entry
    /// `disable` deliberately keeps (`disable_with_no_shepherd_writes_the_config_and_exits_zero`
    /// pins the keeping half of that contrast).
    #[tokio::test]
    async fn rehome_forgets_everything_disable_deliberately_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let mut seed = ShepToml::open(&paths.daemon_config).unwrap();
        seed.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        seed.save().unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            "otel",
        )
        .await;

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
    }

    /// fails if `rehome` sends anything but `DisableDog` with the name it
    /// was given — the `rehome` sibling of `disable_asks_the_shepherd_to_stop_that_dog`.
    #[tokio::test]
    async fn rehome_asks_the_shepherd_to_stop_that_dog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = rehome_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
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
        let mut seed = ShepToml::open(&paths.daemon_config).unwrap();
        seed.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        seed.save().unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            "otel",
        )
        .await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        let cfg = shep_core::config::DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(cfg.daemon.enabled_dogs.is_empty());
        assert!(!cfg.daemon.adopted_dogs.contains_key("otel"));
    }
}
