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
use std::process::{Command, Stdio};

use shep_client::Client;
use shep_core::barks;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{DogSource, Request, Response};

use crate::cli::{AdoptArgs, BarksArgs};
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

/// `shep enable <name>`: writes the config, and starts the dog if a
/// shepherd is running.
pub async fn enable(streams: &mut Streams<'_>, paths: &ShepPaths, name: &str) -> ExitCode {
    // The read and the edit are one `edit` call because they are one
    // transaction: the source below is read from the same document this
    // then writes back, under the lock that keeps a concurrent `shep
    // adopt` from landing between the two.
    let source = match ShepToml::edit(&paths.daemon_config, |cfg| {
        // Read from the config rather than assumed: `shep adopt` records
        // the binary and `shep enable` is what starts it afterwards, so a
        // hardcoded `BuiltIn` here sends the shepherd off to spawn `shep
        // dog <name>` and the adopted binary never runs at all.
        let source = dog_source(cfg, name);
        cfg.enable_dog(name);
        source
    }) {
        Ok(source) => source,
        Err(err) => return fail_config(streams, &err),
    };
    let client = Client::connect(&paths.socket).await.ok();
    enable_after_config(streams, name, &source, client.as_ref()).await
}

/// `enable`'s daemon half, split out from [`enable`] so a test can drive it
/// against a `shep_client::testing` fake without racing a second, real
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
    let client = Client::connect(&paths.socket).await.ok();
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
pub fn vet_binary(path: &Path, home: &Path) -> Result<VettedBinary, AdoptRefusal> {
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
    let canonical = path.canonicalize().map_err(|_| AdoptRefusal::Missing)?;
    let group_writable = writability(&canonical)?;
    // Spawned with no arguments — an adopted dog is run exactly this way
    // (`dog_app`'s own doc) — and torn down unconditionally: `kill` is
    // ignored (a process that already exited is not a failure to vet), but
    // `wait` always runs, on every path out of this match, so no zombie
    // survives a refusal or a success.
    //
    // `SHEP_HOME` is set to the home this invocation actually resolved, and
    // that is a fix rather than a detail. It used to be inherited, so
    // `shep adopt --home /tmp/scratch ./my-dog` vetted `my-dog` against
    // whatever `SHEP_HOME` the shell had, which is usually nothing, so the
    // candidate resolved the DEFAULT home instead. A dog reads `SHEP_HOME`
    // to find its socket, which is the one thing `docs/dogs.md` promises it,
    // so a rotator or anything else with a job to do connected to the live
    // daemon and did it, during the command whose entire purpose was
    // deciding whether to trust the binary at all. Found 2026-08-20 while
    // building `shep-log-rotate`; nothing was lost only because that dog's
    // default size threshold happened to be larger than the logs it found.
    //
    // NOT `env_clear()`, though an earlier note of mine suggested it. A real
    // adopted dog runs with the daemon's own filtered environment merged
    // under its `[dog.<name>]` env (`AppConfig::env`'s doc), so clearing
    // here would vet under stricter conditions than the dog will ever run
    // under, and a binary needing `DYLD_LIBRARY_PATH` or its like would be
    // refused despite working perfectly once adopted. Vetting has to model
    // the real thing, not an idealised one.
    //
    // Stdio goes to null. A candidate that writes on its way up would
    // otherwise scribble over the operator's terminal mid-vet, and a hostile
    // one could imitate shep's own output at the exact moment somebody is
    // deciding whether to trust it.
    match Command::new(&canonical)
        .env("SHEP_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
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
            Ok(VettedBinary {
                path: canonical,
                group_writable,
            })
        }
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

    let group_writable = Vec::new();
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
    let vetted = match vet_binary(&candidate, &paths.home) {
        Ok(vetted) => vetted,
        Err(refusal) => return fail_adopt(streams, &candidate, &refusal),
    };
    let path = vetted.path;
    for writable in &vetted.group_writable {
        warn_group_writable(streams, writable);
    }
    if let Err(err) = ShepToml::edit(&paths.daemon_config, |cfg| {
        cfg.adopt_dog(&name, &path);
    }) {
        return fail_config(streams, &err);
    }
    let client = Client::connect(&paths.socket).await.ok();
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
    let client = Client::connect(&paths.socket).await.ok();
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
            vet_binary(&dir.path().join("nope"), dir.path()),
            Err(AdoptRefusal::Missing)
        );
        assert_eq!(
            vet_binary(dir.path(), dir.path()),
            Err(AdoptRefusal::NotAFile)
        );

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            vet_binary(&plain, dir.path()),
            Err(AdoptRefusal::NotExecutable)
        );

        // The same file, now executable: the ONLY thing that changed is the
        // mode bit, so a `vet_binary` that refused for some other reason
        // fails here rather than passing for the wrong one.
        chmod(&plain, 0o755);
        let vetted = vet_binary(&plain, dir.path()).unwrap();
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
            vet_binary(&bogus, dir.path()),
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
            vet_binary(&bin, dir.path()),
            Err(AdoptRefusal::WorldWritable {
                path: bin.canonicalize().unwrap(),
            }),
            "a world-writable binary must be refused"
        );

        // The file is now sound; the directory holding it is not.
        chmod(&bin, 0o755);
        chmod(dir.path(), 0o777);
        assert_eq!(
            vet_binary(&bin, dir.path()),
            Err(AdoptRefusal::WorldWritable {
                path: bin.canonicalize().unwrap().parent().unwrap().to_path_buf(),
            }),
            "a world-writable directory must be refused too"
        );
        // Restored so the tempdir cleans up from a known state.
        chmod(dir.path(), 0o700);
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

        let vetted = vet_binary(&bin, dir.path()).unwrap();
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
    /// between the two verbs is that this one forgets the registration,
    /// including the `[dog.<name>]` table and the `adopted_dogs` entry
    /// `disable` deliberately keeps (`disable_with_no_shepherd_writes_the_config_and_exits_zero`
    /// pins the keeping half of that contrast).
    #[tokio::test]
    async fn rehome_forgets_everything_disable_deliberately_keeps() {
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
