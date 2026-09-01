//! The dog contract: what a dog is spawned as, and how it is served its own
//! configuration
//!
//! A dog is an ordinary supervised process that happens to speak the control
//! protocol. Nothing here teaches the engine a second kind of supervision:
//! [`dog_app`] assembles the same [`ResolvedApp`] a Flockfile entry would
//! produce, and the supervisor spawns, restarts, reloads and kills it exactly
//! as it does a sheep.
//!
//! ## The two halves of the contract
//!
//! **Where the binary comes from** is [`DogSpec::source`]. A built-in dog is
//! an argv branch of the shep binary itself; an adopted one is a binary an
//! operator installed. That is the whole difference — both are run at the
//! daemon's own trust level, and neither gets a supervision rule of its own.
//!
//! **Where the configuration comes from** is [`dog_section`], reached over
//! the socket as `Request::DogConfig`. A dog inherits `$SHEP_HOME` and
//! `$SHEP_DOG_NAME` and nothing else it did not already need in order to
//! exec: it connects to the socket the first names, handshakes, and asks for
//! the `[dog.<name>]` section the second names. The reply is opaque text the
//! dog parses, so a third-party dog is bound to the shape of its own section
//! and not to shep's config model, file discovery, or layering rules.
//!
//! ## Why the section travels over the socket
//!
//! The environment is readable from the process table on some systems,
//! inherited by every child a dog spawns, and captured into crash dumps. A
//! dog's section routinely holds sinks with credentials in them — a Discord
//! or Slack webhook URL is a bearer token in a query string — so it stays
//! off the child's environment entirely. `SECURITY.md` already discloses
//! that `flock.json` records app `env` in the clear; this declines to widen
//! that exposure to a second surface.

use core::fmt;
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use shep_core::barks::{self, Bark};
use shep_core::config::{AppConfig, DaemonConfig, ResolvedApp, normalize};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{BusEvent, DogSource, ProcessEventKind, ProcessInfo};
use shep_core::selector::ProcessSelector;
use shep_core::status::ProcStatus;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use crate::bus::SharedEvent;
use crate::supervisor::SupervisorHandle;

/// One dog the daemon knows about: its name, and where its binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogSpec {
    /// The dog's name — the `[dog.<name>]` key and the entry's name.
    pub name: String,
    /// Where its binary comes from.
    pub source: DogSource,
}

/// Error assembling a dog's app config, or reading its section
///
/// `Debug` is derived and needs no redaction: the variants carry a path, a
/// normalizer complaint about a config this module assembled itself, or a
/// TOML parser message — never a value read out of a parsed `[dog.<name>]`
/// table. The one way a section's own text can reach a message is a *syntax*
/// error, where the parser quotes the line it failed on; that is the same
/// exposure [`DaemonConfigError`](shep_core::config::DaemonConfigError)
/// already carries, and it reaches only the peer that asked, which
/// peer-cred auth has already established owns the file.
///
/// [`Self::NoBinary`] and [`Self::Io`] wrap the underlying [`std::io::Error`]
/// rather than rendering it, so a caller keeps the OS diagnostic through
/// [`core::error::Error::source`]; that costs this enum `Clone`/`PartialEq`/
/// `Eq` (IR-19's documented exception).
///
/// `#[non_exhaustive]` on this enum too, and not only on the [`DogSource`] it
/// discusses above: `shep-daemon`'s `dogs` module is `pub`, a dog gains a
/// failure shape every time it gains a source or a config surface, and an
/// out-of-tree consumer matching exhaustively today would face a breaking
/// change the day it does (IR-20).
#[non_exhaustive]
#[derive(Debug)]
pub enum DogError {
    /// A built-in dog has no program it can be spawned with. Two different
    /// causes wrap the same [`std::io::Error`]: [`std::env::current_exe`]
    /// itself failing, or — the more common case, and Linux-only — this
    /// crate's own handover-target resolution refusing every candidate it
    /// found, which includes a `current_exe` answer naming a deleted inode
    /// after this binary was replaced on disk (`dog_app`'s doc has the
    /// full argument).
    NoBinary(std::io::Error),
    /// The dog's binary comes from a source this build cannot spawn
    /// (carries the source as `Debug` renders it). [`DogSource`] is
    /// `#[non_exhaustive]`, so a name enabled by a newer shep can reach an
    /// older daemon.
    UnsupportedSource(String),
    /// The assembled config failed `normalize`, or the file read is not
    /// valid `shep.toml`, or the section it holds cannot be rendered back to
    /// TOML (carries the rejection message)
    Config(String),
    /// The file exists and could not be read
    Io(std::io::Error),
}

impl fmt::Display for DogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBinary(err) => write!(f, "this binary's own path is unresolvable: {err}"),
            Self::UnsupportedSource(source) => {
                write!(f, "no way to spawn a dog from source {source}")
            }
            Self::Config(msg) => write!(f, "dog configuration is unusable: {msg}"),
            Self::Io(err) => write!(f, "dog configuration could not be read: {err}"),
        }
    }
}

impl core::error::Error for DogError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::NoBinary(err) | Self::Io(err) => Some(err),
            Self::UnsupportedSource(_) | Self::Config(_) => None,
        }
    }
}

/// The program a built-in dog is spawned as: this binary's own resolved
/// path.
///
/// # Why `handover::exec_target` and not `std::env::current_exe()`, on unix
///
/// This used to call `current_exe` directly, and on Linux that is wrong for
/// exactly the reason `handover::exec_target`'s own doc gives for
/// a handover: `current_exe` reads `/proc/self/exe`, a symlink to the
/// *inode* this process was executed from. A package manager replaces the
/// shepherd's binary by renaming a new file over it, which leaves the old
/// inode unlinked and still open, so `current_exe` comes back
/// `"<path> (deleted)"`. No handover has to be in flight for that to bite —
/// a built-in dog crashing, autorestarting, or `shep restart metrics` after
/// such a rename hit it on the very next respawn, and the string cannot be
/// exec'd.
///
/// `exec_target`'s own doc frames its candidate order — the recorded
/// launch path preferred over `current_exe` — as chosen for an exec that
/// must reach the binary an operator just installed. A dog respawn does
/// not share that requirement in the same words; it only needs a file it
/// can exec, not specifically the newest one. The order still serves it
/// here, for a narrower reason: after a rename, the recorded launch path
/// is exactly the candidate that still resolves to a file, while
/// `current_exe` is the one that comes back `" (deleted)"`. Reusing
/// `exec_target` also keeps one place in this crate that decides "which
/// file holds my own binary" instead of two, and the one already there
/// validates every candidate before handing it to a spawn rather than
/// trusting either blindly.
///
/// One consequence is worth stating rather than discovering later: if the
/// shepherd's own binary has been replaced on disk but the running process
/// has not yet reloaded or handed over, a dog respawned this way can run
/// NEWER code than the shepherd currently has loaded in memory. That is a
/// version-skew question for the `--version` contract this phase's later
/// tasks add, not a spawn failure — this function's only job is to never
/// hand a dog a path that cannot be exec'd at all.
///
/// # Windows resolves it the plain way, and that is not a shortcut
///
/// `handover` is `#[cfg(unix)]` because `execve` and raw descriptor numbers
/// have no Windows equivalent, so `exec_target` is not reachable there and
/// this module does not compile for a Windows target if it asks for one.
/// The guard would also have nothing to catch. `" (deleted)"` is what Linux
/// puts in `/proc/self/exe` for an unlinked inode, and the rename that
/// creates one cannot happen on Windows in the first place: the filesystem
/// refuses to replace a running executable, which is why upgrading shep
/// there means stopping it. So Windows keeps `current_exe`, which answers
/// the same question correctly on a platform where the failure mode does
/// not exist.
#[cfg(unix)]
fn builtin_program() -> Result<PathBuf, DogError> {
    crate::handover::exec_target().map_err(DogError::NoBinary)
}

/// The program a built-in dog is spawned as, on a platform with no
/// handover. See the unix version above for why the two differ.
#[cfg(windows)]
fn builtin_program() -> Result<PathBuf, DogError> {
    std::env::current_exe().map_err(DogError::NoBinary)
}

/// The app config the daemon spawns `spec` from.
///
/// A built-in dog is `<this binary> dog <name>`; an adopted one is the
/// operator's binary with no arguments. Either way the child's environment
/// carries exactly two things it did not already need in order to exec:
/// `SHEP_HOME`, which is how every client locates the socket, and
/// `SHEP_DOG_NAME`, which is the name this dog was registered under and so
/// the `name` its `Request::DogConfig` has to carry. No `[dog.<name>]`
/// value is ever placed here — a dog asks for its section over the socket,
/// because the environment is readable from the process table, inherited by
/// every child, and captured into crash dumps. The section's KEY is not one
/// of its values, and a dog that cannot learn it cannot ask for the section
/// at all.
///
/// `autorestart` and the restart budget are left at their defaults: a dog
/// is supervised exactly as a sheep is.
///
/// # Errors
/// - [`DogError::NoBinary`] — a built-in dog has no program to run, either
///   because [`std::env::current_exe`] itself failed or because the
///   `builtin_program` helper above refused every candidate it found (see
///   its doc for why that includes a Linux `current_exe` answer naming a
///   deleted inode).
/// - [`DogError::UnsupportedSource`] — the source is a kind this build does
///   not know how to spawn.
/// - [`DogError::Config`] — the assembled config failed `normalize`.
pub fn dog_app(spec: &DogSpec, paths: &ShepPaths) -> Result<ResolvedApp, DogError> {
    let (script, args) = match &spec.source {
        DogSource::BuiltIn => (
            builtin_program()?.display().to_string(),
            vec!["dog".to_string(), spec.name.clone()],
        ),
        // No arguments: an adopted dog is a binary somebody else wrote, and
        // an argv shep invented for it is one more thing it has to agree
        // with before it can start.
        DogSource::Adopted { path } => (path.clone(), Vec::new()),
        source => return Err(DogError::UnsupportedSource(format!("{source:?}"))),
    };

    let mut config = AppConfig::minimal(&spec.name, &script);
    config.args = args;
    config
        .env
        .insert("SHEP_HOME".to_string(), paths.home.display().to_string());
    // The name the operator registered this dog under — the `[dog.<name>]`
    // key its own section lives beneath, and so the `name` it has to put in
    // `Request::DogConfig`. A built-in dog reads it out of its argv; an
    // adopted one has no argv at all, so it needs another way to learn it —
    // without this, a third-party dog would have to hardcode a name and
    // hope the operator typed the same one. A mismatch is silent on both
    // sides — `dog_section`
    // answers a name nobody adopted with the same empty string a registered
    // dog with no section gets — so the whole of an operator's
    // configuration could be discarded and everything still looked healthy.
    //
    // An environment entry rather than an argv, deliberately: the argv
    // decision above still holds (an argv shep invents is one more thing a
    // foreign binary has to agree with before it can start), and a dog that
    // ignores a variable it does not recognize starts exactly as it did.
    //
    // Safe to place here for the same reason `SHEP_HOME` is, and for no
    // other: a name is not a secret. The rule this does not break is that
    // no `[dog.<name>]` VALUE travels in the environment — that is the key,
    // not the section.
    config
        .env
        .insert("SHEP_DOG_NAME".to_string(), spec.name.clone());
    normalize(config).map_err(|err| DogError::Config(err.to_string()))
}

/// Starts every dog in `specs`, warning and carrying on for each one that
/// will not start.
///
/// Never fails the boot. A dog that cannot be spawned is a monitoring gap,
/// and refusing to bring the flock up over it would turn that gap into an
/// outage — the one trade this whole subsystem is built to avoid.
///
/// Two ways a dog can fail to start, both answered the same way — a
/// `warn!` naming the dog, and moving on to the next one in `specs`:
///
/// - [`dog_app`] rejects the spec before anything is registered: the
///   binary's own path is unreadable, or the source is one this build does
///   not know how to spawn.
/// - [`SupervisorHandle::start_dog`] itself fails to spawn the binary, or
///   — the guard `Request::EnableDog`'s handler already carries, and this
///   boot path has to carry too — comes back `Ok` over a sheep that
///   already holds the name. `start_dog` is idempotent by NAME, so an
///   unmarked reply means a sheep got there first: no dog was started, and
///   logging success over that reply would be the exact false positive the
///   RPC arm already refuses to give an operator who types `shep enable`.
pub async fn spawn_enabled_dogs(
    specs: &[DogSpec],
    paths: &ShepPaths,
    supervisor: &SupervisorHandle,
) {
    for spec in specs {
        let app = match dog_app(spec, paths) {
            Ok(app) => app,
            Err(err) => {
                tracing::warn!(dog = %spec.name, %err, "a dog did not start");
                continue;
            }
        };
        match supervisor.start_dog(app, spec.source.clone()).await {
            Ok(info) if info.dog.is_none() => tracing::warn!(
                dog = %spec.name,
                "a sheep is already registered under this name; the dog did not start"
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(dog = %spec.name, %err, "a dog did not start"),
        }
    }
}

/// The `[dog.<name>]` section of `path`, rendered back to TOML text.
///
/// Reads the file on every call rather than serving a copy cached at boot:
/// one reader can never be stale, and it is what makes
/// `shep disable X && shep enable X` re-read an edited section.
///
/// A missing file, or a file with no such section, is `Ok(String::new())` —
/// a dog with no configuration is the ordinary case, not a fault.
///
/// # Errors
/// - [`DogError::Config`] — the file exists and is not valid `shep.toml`,
///   or its section will not render back to TOML.
/// - [`DogError::Io`] — the file exists and could not be read.
pub fn dog_section(path: &Path, name: &str) -> Result<String, DogError> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(DogError::Io(err)),
    };
    // Loaded through the daemon's own config loader rather than parsed here,
    // so a broken `shep.toml` is one named error and not a second parser's
    // opinion of the same file. No environment closure: `SHEP_*` overrides
    // govern the daemon's own knobs and have nothing to say about a dog's
    // section.
    let config = DaemonConfig::load(Some(&source), &|_| None)
        .map_err(|err| DogError::Config(err.to_string()))?;
    match config.dog.get(name) {
        None => Ok(String::new()),
        Some(table) => toml::to_string(table).map_err(|err| DogError::Config(err.to_string())),
    }
}

/// What a refused handshake costs the dog that sent it.
///
/// Derived from how many times that dog has been refused since it last
/// handshook successfully, rather than being a tuning choice — see
/// [`DogRefusals::refused`], which is the only thing that produces one.
///
/// `#[non_exhaustive]`: `shep-daemon` is a published library, and a fourth
/// verdict (a dog refused for something other than protocol skew, say)
/// would otherwise be a breaking change for an out-of-tree matcher (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The first refusal since this dog last handshook. Restart it once,
    /// from disk: the running image is stale, and the binary on disk is
    /// very often already the right one — that is the ordinary shape of an
    /// upgrade, where the package replaced the file and the running process
    /// is simply the old one.
    Restart,
    /// The second. The restart already happened and produced a dog that
    /// speaks the same protocol the last one did, which PROVES the binary
    /// on disk cannot satisfy this daemon either. Restarting again would be
    /// a spin, not optimism. Report it stale and stop.
    Stale,
    /// Already stale, already reported. Say nothing further: a stale dog
    /// that its own `autorestart` keeps respawning would otherwise write
    /// one error line per respawn, and the operator has already been told
    /// the one thing there is to tell.
    AlreadyStale,
}

/// Which dogs this daemon has refused at the handshake, and how often since
/// each last got in.
///
/// The handover design's G8, held as state: a refused dog is restarted
/// ONCE and then reported stale, and the difference between those two is
/// nothing more than whether this daemon has refused it before. Cheap to
/// clone (one `Arc`), and shared by every connection through
/// [`RpcContext`](crate::rpc::RpcContext).
///
/// **A count per dog, cleared by a successful handshake.** Clearing on the
/// handshake rather than on the restart is what bounds the whole thing: a
/// dog that keeps being refused never clears, so it never earns a second
/// restart, while a dog that gets in is back to a clean slate and would be
/// restarted once again by a LATER daemon that refuses it. One restart per
/// episode, and an episode ends when the dog is talking again.
///
/// Nothing here survives a handover, and that is correct rather than a
/// gap: a successor is a different daemon that has refused nobody yet, and
/// a dog it can talk to is not stale by any definition it could apply.
///
/// `Debug` is derived and needs no redaction (IR-41): the map holds dog
/// names and counts. A dog's name is the `[dog.<name>]` key an operator
/// typed, which `dogs.rs` already places in the child's environment for the
/// reason its module doc gives — the section's KEY is not one of its
/// VALUES, and no value ever reaches this type.
#[derive(Debug, Clone, Default)]
pub struct DogRefusals {
    /// Both halves under one lock, because every caller that changes one
    /// changes the other in the same breath and a dog seen as refused and
    /// handshook at once is a state no reader should be able to observe.
    seen: Arc<Mutex<Links>>,
}

/// What [`DogRefusals`] holds: how often each dog has been refused, and
/// which dogs have ever got in.
///
/// Private, and only ever reached under the one lock above.
#[derive(Debug, Default)]
struct Links {
    /// Refusals per dog name since that dog last handshook. A name absent
    /// from the map has not been refused since it last got in.
    refusals: BTreeMap<String, u32>,
    /// Dogs whose handshake this daemon has accepted and not refused
    /// since.
    ///
    /// Not derivable from the absence of a refusal, which is why it is
    /// kept: a dog that has never connected at all and a dog that is
    /// talking happily both have no entry in [`Self::refusals`], and
    /// telling those two apart is the whole of what G13's reporting waits
    /// on.
    handshook: BTreeSet<String>,
}

impl DogRefusals {
    /// Builds an empty record — a daemon that has refused nobody.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one refused handshake from the dog named `name`, and says
    /// what the daemon should do about it.
    ///
    /// The rule is G8's, and it falls out of the count rather than being
    /// configured: the first refusal earns [`Refusal::Restart`], the second
    /// [`Refusal::Stale`], and every one after that
    /// [`Refusal::AlreadyStale`]. There is no fourth case and no dial.
    pub fn refused(&self, name: &str) -> Refusal {
        let mut seen = self.lock();
        // A dog this daemon was talking to a moment ago is no longer one it
        // can vouch for. The connection that earned the mark is gone, and
        // the process behind the name may not be the one that made it.
        seen.handshook.remove(name);
        let count = seen.refusals.entry(name.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        match *count {
            1 => Refusal::Restart,
            2 => Refusal::Stale,
            _ => Refusal::AlreadyStale,
        }
    }

    /// Records that `name` handshook successfully, clearing whatever this
    /// daemon held against it.
    ///
    /// A dog that is talking to this daemon is not stale by any definition
    /// this daemon can apply, whatever happened before.
    pub fn handshook(&self, name: &str) {
        let mut seen = self.lock();
        seen.refusals.remove(name);
        seen.handshook.insert(name.to_string());
    }

    /// Whether `name` has handshook with this daemon and not been refused
    /// since.
    ///
    /// The question G13's reporting asks of every dog before it reports
    /// anything: a dog that has answered is one whose state is a fact, and
    /// a dog that has not is one the answer would be a guess about.
    #[must_use]
    pub fn has_handshook(&self, name: &str) -> bool {
        self.lock().handshook.contains(name)
    }

    /// Every dog whose one restart from disk is in flight, sorted.
    ///
    /// Exactly the dogs refused ONCE. The restart G8 owes them has been
    /// asked for and its outcome has not arrived, so neither answer is
    /// available yet: they are not stale, and they are not talking.
    #[must_use]
    pub fn restarting(&self) -> Vec<String> {
        self.lock()
            .refusals
            .iter()
            .filter(|(_, count)| **count == 1)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Every dog this daemon has given up on, sorted.
    ///
    /// A dog is stale once it has been refused twice: the first refusal
    /// bought it a restart from disk, and the second proved the disk binary
    /// is no better. This is the daemon's own answer to "which dogs cannot
    /// talk to me", and the reading G13's `daemon reload` reporting is
    /// meant to take.
    #[must_use]
    pub fn stale(&self) -> Vec<String> {
        self.lock()
            .refusals
            .iter()
            .filter(|(_, count)| **count >= 2)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The record, treating a poisoned lock as ordinary data.
    ///
    /// Every critical section here is a lookup or an increment on a plain
    /// `BTreeMap` and a `BTreeSet`, so a panic elsewhere cannot leave a torn
    /// value, and taking down a daemon whose whole job is staying up would
    /// be the worse failure — the same argument
    /// [`FlockRegistry`](crate::snapshot) already makes for its own map.
    fn lock(&self) -> std::sync::MutexGuard<'_, Links> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Records a dog's refused handshake and acts on it — the whole of the
/// handover design's G8, from the one place that knows a refusal happened.
///
/// Three steps and one prohibition:
///
/// 1. **Record it.** Every refusal is logged, at a level that says what the
///    daemon is doing about it. Before this existed the refusal was logged
///    at no level at all, which is half of what G8 calls a defect: a dog
///    could go mute and neither side would write a line.
/// 2. **Restart it once, from disk.** A restart re-spawns the binary the
///    dog's stored config names, which is the file on disk now and not the
///    image the running process was started from. That is the entire
///    automatic fix, and it is enough for the ordinary case: the package
///    already replaced the binary and the running process is merely old.
/// 3. **Then stop.** A second refusal proves the disk binary cannot satisfy
///    this daemon either, so a third restart would be a spin rather than
///    optimism. The dog is reported stale and left alone.
///
/// **Never loops**, and that is a property of the state rather than a
/// budget: [`DogRefusals`] only clears a dog's count when that dog
/// handshakes successfully, so a dog that cannot get in cannot earn a
/// second restart however many times it is refused.
///
/// The restart runs on its own task rather than inline. A restart is a full
/// kill ladder, which can take as long as the dog's `kill_timeout`, and the
/// caller is a connection handler holding a socket this daemon has already
/// refused — there is nothing left on that connection worth delaying for.
///
/// **What this does NOT do is stop a stale dog's own `autorestart`.** A dog
/// whose process EXITS on a refused handshake — which is what a dog does
/// when its very first connection is refused, rather than one it lost to a
/// handover — is respawned by the supervisor exactly as any sheep would be,
/// and goes on being refused until its restart budget runs out and
/// [`spawn_dog_watch`] records the exhaustion. That loop is bounded, it is
/// the supervisor's existing mechanism, and G8 is about not ADDING daemon
/// restarts on top of it.
pub fn record_refused_dog(
    name: &str,
    client_version: &str,
    refusals: &DogRefusals,
    supervisor: &SupervisorHandle,
) -> Refusal {
    let verdict = refusals.refused(name);
    match verdict {
        Refusal::Restart => {
            tracing::warn!(
                dog = %name,
                dog_version = %client_version,
                "refused a dog on protocol skew; restarting it once from the binary on disk"
            );
            let supervisor = supervisor.clone();
            let name = name.to_string();
            tokio::spawn(async move { restart_refused_dog(&supervisor, &name).await });
        }
        Refusal::Stale => tracing::error!(
            dog = %name,
            dog_version = %client_version,
            "refused a dog on protocol skew again after restarting it: the binary on disk speaks the same protocol the running one did, so this dog is stale and will not be restarted again. Rebuild or reinstall it against this shep"
        ),
        Refusal::AlreadyStale => tracing::debug!(
            dog = %name,
            dog_version = %client_version,
            "refused a dog already reported stale"
        ),
    }
    verdict
}

/// Restarts the dog named `name`, logging either outcome.
///
/// [`SupervisorHandle::restart_automatic`] rather than the operator door:
/// nobody typed this, so an operator's own `stop` or `delete` landing
/// mid-ladder must take the dog off it rather than being silently converted
/// into the restart it raced.
///
/// An exact-name selector, which is also the only kind that reaches a dog
/// at all — the supervisor deliberately keeps dogs out of `all` and out of
/// pattern matches, so an operator's `shep restart all` never touches one.
async fn restart_refused_dog(supervisor: &SupervisorHandle, name: &str) {
    match supervisor
        .restart_automatic(ProcessSelector::Name(name.to_string()))
        .await
    {
        Ok(_) => tracing::info!(dog = %name, "restarted a refused dog from the binary on disk"),
        // Not an error the daemon can act on: the dog may have been
        // disabled between the refusal and this restart, or the engine may
        // be shutting down. Either way the dog is not coming back on this
        // daemon's initiative, and saying so once is the whole of what is
        // left to do.
        Err(err) => tracing::warn!(dog = %name, %err, "a refused dog could not be restarted"),
    }
}

/// How long a registered, running dog may stay silent before this shepherd
/// concludes it is never going to talk to it (IR-26).
///
/// A dog's handshake is one connect and one round trip on a local socket,
/// measured in milliseconds, so this is not a tuned number — it is three
/// orders of magnitude of slack. What it is actually sized against is the
/// slowest LEGITIMATE silence: a dog carried across a handover has to notice
/// its connection died and dial back, and a third-party dog is free to sleep
/// a second or so before it does. Five seconds outlasts that and still fits
/// inside the time an operator spends watching a reload.
///
/// **Deliberately not `shep daemon reload`'s own settle wait**, which is
/// three seconds and lives in `shep-cli`. That budget answers how long a
/// human's command should hold its output open before reporting; this one
/// answers how long the shepherd should go on believing a silence. They
/// happen to be the same order of magnitude today, and tying them together
/// would make either one's tuning a silent change to the other's meaning.
pub const DOG_SILENCE_BUDGET: Duration = Duration::from_secs(5);

/// Gap between two of [`spawn_silent_dog_watch`]'s looks.
///
/// Finer than [`DOG_SILENCE_BUDGET`] so that a dog's restart is asked for
/// near the moment its budget runs out rather than up to a whole budget
/// after it. One look is one message to the supervisor actor and no syscall
/// per dog, so the cost of looking often is nearly nothing.
const DOG_SILENCE_POLL: Duration = Duration::from_secs(1);

/// Every dog the supervisor is running that has never once handshaken with
/// this daemon, sorted — the set that both [`spawn_silent_dog_watch`] and
/// `rpc::dog_staleness` are built on.
///
/// The two callers read it for different purposes and must not disagree
/// about the population: one REPORTS these dogs as unsettled and the other
/// eventually gives up on them, and a dog that could be in one set but not
/// the other would be reported forever or condemned unreported.
///
/// Only a dog with a PROCESS counts, and a stale one is already answered
/// for — [`crate::rpc`]'s own doc has the long form of both exclusions.
pub(crate) fn silent_dogs(infos: &[ProcessInfo], refusals: &DogRefusals) -> Vec<String> {
    let stale = refusals.stale();
    let mut names: Vec<String> = infos
        .iter()
        .filter(|info| {
            info.dog.is_some()
                && matches!(info.status, ProcStatus::Starting | ProcStatus::Online)
                && !refusals.has_handshook(&info.name)
                && !stale.contains(&info.name)
        })
        .map(|info| info.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// When each currently-silent dog was first SEEN silent.
///
/// The elapsed-time half of [`spawn_silent_dog_watch`], and the reason that
/// watch is a task on a clock rather than a branch inside
/// `rpc::dog_staleness`. Staleness is a query, and `shep daemon reload`
/// polls it in a loop: a ladder driven from there would walk a merely slow
/// dog from restart to stale in the time it takes to ask three times. A dog
/// here is measured against how long it has been quiet and never against
/// how often anybody looked.
///
/// `Debug` is derived and needs no redaction (IR-41), for the same reason
/// [`DogRefusals`]'s is: the map holds dog names and instants, and a dog's
/// name is a `[dog.<name>]` KEY rather than one of its values.
#[derive(Debug, Default)]
struct SilentDogs {
    /// One instant per dog currently silent. A name absent from the map is a
    /// dog that was talking, stopped, or deleted at the last look.
    first_seen: BTreeMap<String, Instant>,
}

impl SilentDogs {
    /// The dogs that have now been silent for a whole [`DOG_SILENCE_BUDGET`],
    /// given the set observed silent at `now`.
    ///
    /// `now` is a parameter rather than read in here so that a test can move
    /// the clock instead of waiting on one, and so that every dog in one look
    /// is judged against the same instant.
    fn due(&mut self, silent: &[String], now: Instant) -> Vec<String> {
        // A dog that answered, stopped, or was deleted is not silent any
        // more, and starts a fresh budget if it ever falls quiet again.
        self.first_seen.retain(|name, _| silent.contains(name));
        let mut due = Vec::new();
        for name in silent {
            let since = self.first_seen.entry(name.clone()).or_insert(now);
            if now.saturating_duration_since(*since) >= DOG_SILENCE_BUDGET {
                // Rearmed rather than forgotten: the next rung of the ladder
                // costs another whole budget, so a dog whose restart was just
                // asked for gets exactly the chance to speak that it had the
                // first time.
                *since = now;
                due.push(name.clone());
            }
        }
        due
    }
}

/// One look: which of this daemon's dogs have now been quiet too long, and
/// what each of them earned.
///
/// Returns what it acted on, which is what its tests assert against; the
/// loop that calls it discards the answer.
async fn check_silent_dogs(
    supervisor: &SupervisorHandle,
    refusals: &DogRefusals,
    seen: &mut SilentDogs,
    now: Instant,
) -> Vec<(String, Refusal)> {
    // A stopped engine has no dogs left to wait on. `seen` is left untouched
    // rather than cleared: a look that could not see the flock has learned
    // nothing about it, and must not hand every dog a fresh budget.
    let Ok(infos) = supervisor.list_checked().await else {
        return Vec::new();
    };
    let silent = silent_dogs(&infos, refusals);
    let mut acted = Vec::new();
    for name in seen.due(&silent, now) {
        let verdict = record_silent_dog(&name, refusals, supervisor).await;
        acted.push((name, verdict));
    }
    acted
}

/// Enters a dog that has gone quiet into G8's ladder — the same ladder a
/// named refusal enters, reached by inference rather than by the dog saying
/// who it is.
///
/// **Why the inference is needed at all.** `record_refused_dog` is keyed on
/// `Hello::dog_name`, and that field was added in phase 3. A client speaking
/// an OLDER protocol cannot send one, so the connection it refuses is
/// anonymous and the ladder is never entered. G8's one restart therefore
/// reached only dogs new enough to name themselves, which is the exact
/// complement of the dogs that need it. The set difference this rides on
/// needs no cooperation from the client — and no peer credentials, which
/// would have meant `SO_PEERCRED` on unix against
/// `GetNamedPipeClientProcessId` on Windows, re-forking a transport phase 15
/// deliberately unified into `shep_core::transport`.
///
/// **The tradeoff, named where the decision is.** A dog that is merely SLOW
/// to connect is restarted once, for nothing. That is bounded by the ladder
/// it enters — one restart, then a report, then silence — and it is cheap
/// for a process that is by definition not yet doing its job. It also heals
/// itself: the moment such a dog does handshake, [`DogRefusals::handshook`]
/// clears everything held against it, including a stale mark it should never
/// have earned. Against that, the cost of NOT inferring was measured in
/// production: a dog `online` with zero restarts and a refusal repeating in
/// its own log without end.
async fn record_silent_dog(
    name: &str,
    refusals: &DogRefusals,
    supervisor: &SupervisorHandle,
) -> Refusal {
    let verdict = refusals.refused(name);
    match verdict {
        Refusal::Restart => {
            tracing::warn!(
                dog = %name,
                silent_for_secs = DOG_SILENCE_BUDGET.as_secs(),
                "a dog has been running without ever answering this shepherd; restarting it once from the binary on disk"
            );
            // Awaited rather than spawned, which is the one difference from
            // `record_refused_dog`: that caller is a connection handler
            // holding a socket, and this one is a background loop with
            // nothing to delay. Awaiting it keeps the next look from running
            // while a kill ladder is still in flight, so a dog is never
            // judged mid-restart.
            restart_refused_dog(supervisor, name).await;
        }
        Refusal::Stale => tracing::error!(
            dog = %name,
            "a dog restarted for never answering this shepherd has still not answered it: the binary on disk cannot talk to this shep either, so this dog is stale and will not be restarted again. Read its own log with `shep bleats`, then rebuild or reinstall it and restart it"
        ),
        // Unreachable through this path, because `silent_dogs` filters a
        // stale dog out before it can be seen quiet again. Kept as a real
        // arm anyway: it is the honest thing to do with a rung the ladder
        // defines, and a future caller that stops filtering would otherwise
        // find a `todo!` here.
        Refusal::AlreadyStale => tracing::debug!(
            dog = %name,
            "a silent dog that was already reported stale"
        ),
    }
    verdict
}

/// Watches for dogs that are running and have never once spoken to this
/// shepherd, and enters each into G8's ladder after
/// [`DOG_SILENCE_BUDGET`] of silence: restarted once from the binary on
/// disk, then reported stale, then left alone.
///
/// **Anchored to the daemon's BOOT, and that is load-bearing.** A handover
/// is an `execve`: same pid, same children, new image. Every task spawned
/// before it belonged to the predecessor's runtime and simply stops existing
/// at the exec, while the successor installs the state that task was meant
/// to resolve. An earlier phase found and closed six instances of exactly
/// that bug. `boot` runs again in the successor, so the successor spawns its
/// own watch here — whereas a per-dog one-shot timer armed at spawn time
/// would die at the exec, which is the precise moment a carried dog's
/// silence matters most.
///
/// Its `JoinHandle` is held by the caller and aborted at teardown, for the
/// same reason [`spawn_dog_watch`]'s is: the loop has no end of its own, and
/// nothing may restart a dog while the daemon is shutting down.
pub fn spawn_silent_dog_watch(
    supervisor: SupervisorHandle,
    refusals: DogRefusals,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(DOG_SILENCE_POLL);
        // A look missed under load is not a look owed: the budget is measured
        // off the clock and not off a tick count, so catching up would buy
        // nothing and cost a burst of supervisor traffic.
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seen = SilentDogs::default();
        loop {
            let now = ticks.tick().await;
            check_silent_dogs(&supervisor, &refusals, &mut seen, now).await;
        }
    })
}

/// Watches the bus and records, locally, every enabled dog that exhausts
/// its restart budget.
///
/// The shepherd cannot DELIVER an alert about a dead bark dog: it has no
/// sinks and no webhook code, by design. What it can guarantee is a local
/// trail, so an operator reading `shep barks` after an outage finds the
/// moment alerting stopped rather than a gap they have to infer.
///
/// A bus WATCHER rather than a branch inside the supervisor, and the
/// distinction is the phase's own tripwire: this answers *who should see
/// this*, from outside, and the supervisor stays a machine that knows only
/// how to supervise. A `dog` arm inside `handle_exited` would be the same
/// behaviour reaching into the wrong place.
///
/// Its `JoinHandle` is held by the caller and aborted at teardown: the task
/// parks on a broadcast receiver, which ends on its own when the sender
/// drops, and holding the handle is what makes the end deterministic rather
/// than dependent on sender count.
pub fn spawn_dog_watch(
    mut events: broadcast::Receiver<SharedEvent>,
    barks: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                // Only a DOG's Errored is this watcher's business. A sheep's
                // Errored is bark's job — bark writes those records over its
                // own client connection — and duplicating that write here
                // would leave one event with two authors in one file. Exit
                // is excluded too: it fires on every restart a dog survives,
                // and a `barks.jsonl` full of those is one an operator stops
                // reading.
                Ok(event) => {
                    if let BusEvent::Process {
                        event: ProcessEventKind::Errored,
                        info,
                        ..
                    } = &*event
                        && info.dog.is_some()
                    {
                        record_dog_errored(&barks, &info.name, info.restarts);
                    }
                }
                // The bus DROPS events for a lagging subscriber rather than
                // queuing them (`tokio::sync::broadcast`'s own contract), so
                // a dog's death notice may be among what this receiver just
                // lost. There is no poll to recover it — building one here
                // would be building a second bark dog inside the shepherd,
                // exactly the subsystem this module exists to avoid.
                // Metrics' `shep_dog_up` is the intended answer to this gap.
                Err(RecvError::Lagged(count)) => {
                    tracing::warn!(
                        count,
                        "the shepherd's dog watch dropped bus events; a dog's exhausted restart budget may have gone unrecorded"
                    );
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// Records `name`'s exhausted restart budget as a [`Bark`] the shepherd
/// wrote itself, and logs the same facts at `tracing::error!`.
///
/// The two records serve different audiences: `message` is plain English
/// for an operator reading `shep barks` mid-incident, and the `tracing`
/// event carries the same facts structured for `journalctl`. `sinks` is
/// left empty, which is how a [`Bark`] says the shepherd has no webhook
/// code of its own (see [`Bark::sinks`]'s own doc).
///
/// A dog is supervised with `AppConfig`'s own defaults — [`dog_app`] never
/// overrides `max_restarts` — so `AppConfig::default().max_restarts` is the
/// exhausted budget for every dog, not a guess.
fn record_dog_errored(barks_path: &Path, name: &str, restarts: u32) {
    let budget = AppConfig::default().max_restarts;
    tracing::error!(dog = %name, restarts, budget, "a dog exhausted its restart budget");
    let bark = Bark {
        at_ms: crate::now_ms(),
        rule: "daemon".to_string(),
        subject: name.to_string(),
        message: format!(
            "dog {name} exhausted its restart budget: {restarts} restarts against a budget of {budget}"
        ),
        sinks: Vec::new(),
    };
    if let Err(err) = barks::append(barks_path, &bark, barks::DEFAULT_MAX_BYTES) {
        tracing::warn!(%err, dog = %name, "failed to record a dog's exhausted restart budget");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::ProcScript;
    use crate::testing::test_paths;
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    /// fails if a `current_exe` answer carrying Linux's `" (deleted)"`
    /// suffix ever becomes a built-in dog's script. Before this fix
    /// `dog_app` called `std::env::current_exe()` unguarded and used
    /// whatever it returned as the script directly; on Linux, a package
    /// manager renaming a new file over the running shepherd's inode makes
    /// that string literal, and exec'ing it fails.
    ///
    /// `current_exe` cannot be made to return that string on this platform
    /// (or on any platform, safely), so this exercises the exact function
    /// `builtin_program` now delegates to —
    /// `crate::handover::resolve_target`, proven to refuse a deleted-inode
    /// candidate by
    /// `handover::tests::resolve_target_refuses_a_synthetic_deleted_inode_candidate`
    /// — and pins what a dog's own error reads once that refusal reaches
    /// `DogError`, since a message naming the fix is the feature this
    /// phase asks for.
    ///
    /// Unix only, because the guard it pins is: `handover` is `#[cfg(unix)]`,
    /// and the `" (deleted)"` string it refuses is Linux's answer for an
    /// unlinked `/proc/self/exe`. Windows resolves a built-in dog's program
    /// with `current_exe` and has no such state to refuse.
    #[cfg(unix)]
    #[test]
    fn a_deleted_inode_answer_from_current_exe_never_becomes_a_dogs_script() {
        let refusal = crate::handover::resolve_target(
            [None, Some(PathBuf::from("/opt/shep/shep (deleted)"))],
            None,
        )
        .unwrap_err();
        let err = DogError::NoBinary(refusal);
        assert_eq!(
            err.to_string(),
            "this binary's own path is unresolvable: no binary to exec: \
             /opt/shep/shep (deleted) (names a deleted inode, not a file)"
        );
    }

    /// fails if a `[dog.<name>]` value is folded into the child's
    /// environment. That is the design's whole reason for putting config on
    /// the socket: a webhook URL in the environment is readable from the
    /// process table on some systems, inherited by every child the dog
    /// spawns, and captured into crash dumps. The assertion is over the
    /// ASSEMBLED spec, not the config, because `assemble` is where an env
    /// map would actually be merged.
    ///
    /// Also fails if the section's KEY stops travelling there, which is the
    /// opposite rule and not a contradiction of it: `SHEP_DOG_NAME` is what
    /// a dog puts in `Request::DogConfig` to ask for the section in the
    /// first place, so withholding it withholds the configuration rather
    /// than protecting it.
    #[test]
    fn a_dogs_child_environment_carries_shep_home_and_its_name_and_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::write(
            &paths.daemon_config,
            "[dog.bark]\nwebhook = \"https://example.invalid/hook\"\n",
        )
        .unwrap();
        let spec = DogSpec {
            name: "bark".to_string(),
            source: DogSource::BuiltIn,
        };
        let app = dog_app(&spec, &paths).unwrap();
        let assembled = crate::assemble::assemble(&app, 0, &paths, None);
        assert_eq!(
            assembled.env.get("SHEP_HOME"),
            Some(&paths.home.display().to_string())
        );
        assert_eq!(
            assembled.env.get("SHEP_DOG_NAME"),
            Some(&"bark".to_string()),
            "a dog is told the name its own section lives under"
        );
        assert!(
            !assembled
                .env
                .values()
                .any(|v| v.contains("example.invalid")),
            "a dog's configuration never travels in its environment: {:?}",
            assembled.env
        );
    }

    /// fails if a built-in dog is spawned as anything but this binary's own
    /// hidden `dog <name>` branch, and fails if an adopted one is given
    /// arguments it never asked for — which would make every third-party
    /// dog see an argv shep invented for it.
    #[test]
    fn a_built_in_dog_runs_this_binary_and_an_adopted_one_runs_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        let built_in = dog_app(
            &DogSpec {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
            },
            &paths,
        )
        .unwrap();
        assert_eq!(
            built_in.config().script,
            std::env::current_exe().unwrap().display().to_string()
        );
        assert_eq!(built_in.config().args, vec!["dog", "metrics"]);

        let adopted = dog_app(
            &DogSpec {
                name: "otel".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            },
            &paths,
        )
        .unwrap();
        assert_eq!(adopted.config().script, "/usr/local/bin/shep-otel");
        assert!(adopted.config().args.is_empty());
        assert_eq!(
            adopted.config().name,
            "otel",
            "the NAME is the config key, never the filename"
        );
    }

    /// fails if an ADOPTED dog is left to guess the name it was registered
    /// under. A built-in dog can read its own argv (`dog <name>`, asserted
    /// above); an adopted one is given no argv at all, on purpose, so the
    /// environment is the only channel it has. Without this it has to
    /// hardcode a name and hope the operator typed the same one — and a
    /// mismatch is answered with the same empty section a dog with no
    /// configuration gets, so it looks exactly like working.
    ///
    /// Asserted on the name the operator chose, not on the binary's file
    /// stem: `shep adopt ./shep-otel --name telemetry` registers
    /// `telemetry`, and the filename is not the key.
    #[test]
    fn an_adopted_dog_is_told_the_name_it_was_registered_under() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        let adopted = dog_app(
            &DogSpec {
                name: "telemetry".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            },
            &paths,
        )
        .unwrap();

        assert!(
            adopted.config().args.is_empty(),
            "the name arrives without shep inventing an argv for a foreign binary"
        );
        assert_eq!(
            adopted.config().env.get("SHEP_DOG_NAME"),
            Some(&"telemetry".to_string())
        );
    }

    /// fails if `dog_section` returns the whole file, or a typed structure,
    /// or fails on a file with no such section. The blob is what a
    /// third-party dog parses; handing it a table it did not ask for is the
    /// same bug as handing it nothing.
    #[test]
    fn a_dogs_section_comes_back_as_its_own_table_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nlog_json = true\n\n[dog.bark]\ndebounce = \"30s\"\n\n[dog.metrics]\nport = 9615\n",
        )
        .unwrap();

        let bark = dog_section(&path, "bark").unwrap();
        assert!(bark.contains("debounce"));
        assert!(
            !bark.contains("9615"),
            "one dog never sees another's config"
        );
        assert!(!bark.contains("log_json"), "nor the daemon's own");
        // Round-trips as TOML, since that is the contract the dog parses under.
        let parsed: toml::Table = toml::from_str(&bark).unwrap();
        assert_eq!(parsed["debounce"].as_str(), Some("30s"));

        assert_eq!(dog_section(&path, "absent").unwrap(), "");
        assert_eq!(
            dog_section(&dir.path().join("gone.toml"), "bark").unwrap(),
            ""
        );
    }

    /// A minimal `Process` bus event, `name` carrying either a sheep's or a
    /// dog's entry depending on `dog`.
    fn process_event(name: &str, kind: ProcessEventKind, dog: Option<DogSource>) -> SharedEvent {
        SharedEvent::new(BusEvent::Process {
            event: kind,
            info: ProcessInfo::builder(1, name, ProcStatus::Errored)
                .restarts(16)
                .dog(dog)
                .build(),
            manually: false,
            at_ms: 1_700_000_000_000,
        })
    }

    fn errored_event(name: &str, dog: Option<DogSource>) -> SharedEvent {
        process_event(name, ProcessEventKind::Errored, dog)
    }

    /// Polls `path` under a real timeout until it holds at least `n` barks.
    /// The watcher writing to it runs as a separate task, so a bare read
    /// races it; a bare `recv().await` on nothing is the hang this project
    /// has already paid for twice.
    async fn await_barks(path: &std::path::Path, n: usize) -> Vec<Bark> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let found = barks::read(path).unwrap_or_default();
                if found.len() >= n {
                    return found;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("barks.jsonl never reached the expected record count")
    }

    /// fails if the shepherd records a sheep's death as well as a dog's.
    /// Bark writes the sheep records; two writers for one event, into one
    /// file, is how a history stops being trustworthy. Both halves are
    /// needed: without the negative assertion, a watcher that recorded
    /// EVERY `Errored` passes.
    #[tokio::test]
    async fn the_shepherd_records_a_dog_that_gave_up_and_leaves_the_sheep_to_bark() {
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");
        let (events, rx) = crate::bus::test_bus(16);
        let watch = spawn_dog_watch(rx, barks_path.clone());

        events.send(errored_event("web", None)).unwrap();
        events
            .send(errored_event("bark", Some(DogSource::BuiltIn)))
            .unwrap();

        let recorded = await_barks(&barks_path, 1).await;
        assert_eq!(recorded.len(), 1, "one record, and it is the dog's");
        assert_eq!(recorded[0].subject, "bark");
        assert_eq!(recorded[0].rule, "daemon");
        assert!(
            recorded[0].sinks.is_empty(),
            "the shepherd has no sinks and says so by carrying none"
        );

        watch.abort();
    }

    /// fails if a restart a dog survives is recorded as a death. A dog that
    /// crashes and comes back is not an outage, and a `barks.jsonl` full of
    /// them is one an operator stops reading.
    #[tokio::test]
    async fn a_dog_that_merely_exited_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");
        let (events, rx) = crate::bus::test_bus(16);
        let watch = spawn_dog_watch(rx, barks_path.clone());

        events
            .send(process_event(
                "bark",
                ProcessEventKind::Exit,
                Some(DogSource::BuiltIn),
            ))
            .unwrap();
        // A real `Errored` after it is what proves the watcher was ever
        // listening: without this, a watcher that recorded nothing at all
        // (dead code, or the wrong topic) would also pass.
        events
            .send(errored_event("bark", Some(DogSource::BuiltIn)))
            .unwrap();

        let recorded = await_barks(&barks_path, 1).await;
        assert_eq!(
            recorded.len(),
            1,
            "the Exit left no record; only the Errored that followed it did"
        );

        watch.abort();
    }

    /// fails if a refused dog is restarted more than once. The one-restart
    /// rule is the whole difference between an automatic fix and a crash
    /// loop, and it is derived from the count rather than configured: a
    /// second refusal PROVES the binary on disk cannot satisfy this daemon,
    /// because the restart already ran it.
    #[test]
    fn a_refused_dog_earns_one_restart_and_is_then_stale_forever() {
        let refusals = DogRefusals::new();
        assert!(refusals.stale().is_empty());

        assert_eq!(refusals.refused("metrics"), Refusal::Restart);
        assert!(
            refusals.stale().is_empty(),
            "one refusal is a dog to restart, not a dog to give up on"
        );

        assert_eq!(refusals.refused("metrics"), Refusal::Stale);
        assert_eq!(refusals.stale(), vec!["metrics".to_string()]);

        // The refusals a stale dog's own autorestart goes on producing must
        // not each buy another restart, and must not each be reported.
        for _ in 0..5 {
            assert_eq!(refusals.refused("metrics"), Refusal::AlreadyStale);
        }
        assert_eq!(refusals.stale(), vec!["metrics".to_string()]);
    }

    /// fails if a dog that got back in stays marked. The count is cleared
    /// by a successful handshake and by nothing else, which is what makes
    /// "one restart" mean one restart per episode rather than one per
    /// daemon: a dog talking to this daemon is not stale by any definition
    /// it could apply, and a LATER refusal is a new episode with its own
    /// restart.
    #[test]
    fn a_dog_that_gets_in_is_owed_a_fresh_restart_if_it_is_ever_refused_again() {
        let refusals = DogRefusals::new();
        assert_eq!(refusals.refused("metrics"), Refusal::Restart);
        refusals.handshook("metrics");
        assert!(refusals.stale().is_empty());
        assert_eq!(
            refusals.refused("metrics"),
            Refusal::Restart,
            "the restart that fixed it must not be charged against the next episode"
        );
    }

    /// fails if the record is per-daemon rather than per-dog. A stale
    /// `bark` must not spend `metrics`'s one restart, and a healthy
    /// `metrics` handshake must not clear `bark`'s stale mark.
    #[test]
    fn each_dog_carries_its_own_count() {
        let refusals = DogRefusals::new();
        assert_eq!(refusals.refused("bark"), Refusal::Restart);
        assert_eq!(refusals.refused("bark"), Refusal::Stale);

        assert_eq!(
            refusals.refused("metrics"),
            Refusal::Restart,
            "bark's two refusals are bark's"
        );
        refusals.handshook("metrics");
        assert_eq!(
            refusals.stale(),
            vec!["bark".to_string()],
            "one dog getting in says nothing about another"
        );
    }

    /// fails if a dog mid-restart reads as an answer. G13's report is taken
    /// once every dog has settled, and a dog refused ONCE has not: the
    /// restart G8 owes it has been asked for and its verdict has not come
    /// back. Reading that as "not stale" is exactly the early report the
    /// whole rule exists to prevent — it is the state a stale dog passes
    /// through on its way to being stale.
    #[test]
    fn a_dog_mid_restart_is_neither_stale_nor_settled() {
        let refusals = DogRefusals::new();
        assert!(refusals.restarting().is_empty());

        refusals.refused("metrics");
        assert_eq!(refusals.restarting(), vec!["metrics".to_string()]);
        assert!(refusals.stale().is_empty());

        refusals.refused("metrics");
        assert!(
            refusals.restarting().is_empty(),
            "a dog that has been given up on is settled, not still being restarted"
        );
        assert_eq!(refusals.stale(), vec!["metrics".to_string()]);
    }

    /// fails if "this daemon has heard from that dog" is inferred from the
    /// absence of a refusal. A dog that has never connected and a dog
    /// talking happily both have no refusal recorded, and telling those two
    /// apart is the whole of what G13's report waits on: the first is a
    /// carried dog that has not dialled back yet.
    ///
    /// The refusal half is the other direction. A dog this daemon accepted
    /// and has since refused is not one it can vouch for any more — the
    /// connection that earned the mark is gone.
    #[test]
    fn only_an_accepted_handshake_says_a_dog_has_answered() {
        let refusals = DogRefusals::new();
        assert!(
            !refusals.has_handshook("metrics"),
            "a dog nobody has heard from has not answered"
        );

        refusals.handshook("metrics");
        assert!(refusals.has_handshook("metrics"));
        assert!(!refusals.has_handshook("bark"), "one dog answers for one");

        refusals.refused("metrics");
        assert!(
            !refusals.has_handshook("metrics"),
            "the handshake that earned the mark is the one that just died"
        );
    }

    /// Registers one built-in dog on `ctx`'s supervisor, exactly as
    /// `spawn_enabled_dogs` does at boot, and hands back the harness's own
    /// refusal record.
    /// How long [`start_test_dog`] waits on the supervisor before calling it
    /// a hang rather than a slow start.
    ///
    /// Generous on purpose: this bounds a fixture, so it is a deadlock
    /// guard and not a timing assertion. A value tight enough to measure
    /// anything would be a test about how fast the supervisor accepts a
    /// request, which is not what any caller here is asking.
    const DOG_FIXTURE_START_BUDGET: Duration = Duration::from_secs(10);

    async fn start_test_dog(ctx: &crate::rpc::RpcContext, name: &str) {
        let spec = DogSpec {
            name: name.to_string(),
            source: DogSource::BuiltIn,
        };
        let app = dog_app(&spec, &ctx.paths).expect("the dog fixture must assemble");
        // Bounded, because the callers below run under a paused clock and
        // this await is the one thing in them that is not already forced
        // (IR-46). A supervisor that stopped consuming its request would
        // otherwise hang the suite here, before any of the forcing
        // machinery those tests set up has run. Under `start_paused` tokio
        // auto-advances to the next deadline once every task is idle, so
        // this timeout still fires rather than waiting on a wall clock.
        tokio::time::timeout(
            DOG_FIXTURE_START_BUDGET,
            ctx.supervisor.start_dog(app, DogSource::BuiltIn),
        )
        .await
        .expect("the dog fixture must start inside its budget")
        .expect("the dog fixture must start");
    }

    /// fails if a dog that never speaks to this shepherd is left alone.
    ///
    /// This is the production case, and the whole of why the inference
    /// exists: a dog on an older protocol cannot send `Hello::dog_name`, so
    /// the refusal it earns is anonymous and `record_refused_dog` never runs
    /// for it. The ladder is the same one a named refusal walks -- one
    /// restart from the binary on disk, then a report, then silence -- and
    /// the assertion is that being unable to name itself does not exempt a
    /// dog from it.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_never_answers_is_restarted_once_and_then_marked_stale() {
        let h = crate::testing::harness(vec![
            ProcScript::never_exits(),
            ProcScript::never_exits(),
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]);
        start_test_dog(&h.ctx, "metrics").await;
        let refusals = &h.ctx.dog_refusals;
        let mut seen = SilentDogs::default();
        let t0 = Instant::now();

        assert!(
            check_silent_dogs(&h.ctx.supervisor, refusals, &mut seen, t0)
                .await
                .is_empty(),
            "a dog seen quiet for the first time has not yet been quiet for any length of time"
        );

        assert_eq!(
            check_silent_dogs(
                &h.ctx.supervisor,
                refusals,
                &mut seen,
                t0 + DOG_SILENCE_BUDGET
            )
            .await,
            vec![("metrics".to_string(), Refusal::Restart)],
            "a whole budget of silence buys the one restart from disk"
        );
        assert_eq!(refusals.restarting(), vec!["metrics".to_string()]);
        assert!(
            refusals.stale().is_empty(),
            "one silence is a dog to restart, not a dog to give up on"
        );

        assert_eq!(
            check_silent_dogs(
                &h.ctx.supervisor,
                refusals,
                &mut seen,
                t0 + 2 * DOG_SILENCE_BUDGET
            )
            .await,
            vec![("metrics".to_string(), Refusal::Stale)],
            "the restart ran and the dog still has not spoken: the binary on disk cannot talk to this shep either"
        );
        assert_eq!(refusals.stale(), vec!["metrics".to_string()]);

        assert!(
            check_silent_dogs(
                &h.ctx.supervisor,
                refusals,
                &mut seen,
                t0 + 3 * DOG_SILENCE_BUDGET
            )
            .await
            .is_empty(),
            "a dog already given up on is not laddered again, however long it stays quiet"
        );
    }

    /// fails if a dog that answered is restarted anyway.
    ///
    /// The case that matters most, and the one that passes for the wrong
    /// reason if the inference never fires at all -- so it is written
    /// against a clock ten budgets past the point where a silent dog would
    /// have been condemned twice over.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_answers_inside_the_budget_is_never_touched() {
        let h = crate::testing::harness(vec![ProcScript::never_exits()]);
        start_test_dog(&h.ctx, "metrics").await;
        let refusals = &h.ctx.dog_refusals;
        refusals.handshook("metrics");
        let mut seen = SilentDogs::default();
        let t0 = Instant::now();

        for elapsed in [0, 1, 2, 10] {
            assert!(
                check_silent_dogs(
                    &h.ctx.supervisor,
                    refusals,
                    &mut seen,
                    t0 + elapsed * DOG_SILENCE_BUDGET
                )
                .await
                .is_empty(),
                "a dog this shepherd has heard from is not silent at any point on the clock"
            );
        }
        assert!(refusals.restarting().is_empty());
        assert!(refusals.stale().is_empty());
    }

    /// fails if a dog already reported stale is put back on the ladder.
    ///
    /// A stale dog goes on being quiet forever, so nothing about its silence
    /// is news. Re-laddering it would spend a restart the record already
    /// says was spent, and would write the same report once per budget for
    /// as long as the daemon runs.
    #[tokio::test(start_paused = true)]
    async fn a_dog_already_stale_is_not_laddered_again() {
        let h = crate::testing::harness(vec![ProcScript::never_exits()]);
        start_test_dog(&h.ctx, "metrics").await;
        let refusals = &h.ctx.dog_refusals;
        refusals.refused("metrics");
        refusals.refused("metrics");
        assert_eq!(refusals.stale(), vec!["metrics".to_string()]);

        let mut seen = SilentDogs::default();
        let t0 = Instant::now();
        for elapsed in [0, 1, 2, 5] {
            assert!(
                check_silent_dogs(
                    &h.ctx.supervisor,
                    refusals,
                    &mut seen,
                    t0 + elapsed * DOG_SILENCE_BUDGET
                )
                .await
                .is_empty(),
                "the ladder ends at stale; there is no rung after it to reach"
            );
        }
    }

    /// fails if the ladder is driven by how often somebody looks rather than
    /// by how long a dog has been quiet.
    ///
    /// The regression test for the shape this fix deliberately avoids.
    /// `Request::DogStaleness` derives the same set, and `shep daemon
    /// reload` polls it every 50ms while it waits -- so a ladder driven from
    /// there would restart a merely slow dog and report it stale inside a
    /// second, before it had any chance to speak. Twenty looks inside one
    /// budget must cost a dog nothing, and the twenty-first, one tick past
    /// the budget, must cost it exactly one restart.
    #[tokio::test(start_paused = true)]
    async fn asking_repeatedly_does_not_advance_the_ladder() {
        let h = crate::testing::harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        start_test_dog(&h.ctx, "metrics").await;
        let refusals = &h.ctx.dog_refusals;
        let mut seen = SilentDogs::default();
        let t0 = Instant::now();

        for look in 0..20 {
            assert!(
                check_silent_dogs(
                    &h.ctx.supervisor,
                    refusals,
                    &mut seen,
                    t0 + (DOG_SILENCE_BUDGET / 20) * look
                )
                .await
                .is_empty(),
                "look {look} fell inside the budget and must not have moved the dog along"
            );
        }
        assert!(refusals.restarting().is_empty());

        assert_eq!(
            check_silent_dogs(
                &h.ctx.supervisor,
                refusals,
                &mut seen,
                t0 + DOG_SILENCE_BUDGET
            )
            .await,
            vec![("metrics".to_string(), Refusal::Restart)],
            "the clock is what moves the dog along, and it has now moved"
        );
    }

    /// fails if `spawn_silent_dog_watch`'s own loop stops calling
    /// `check_silent_dogs` at all -- the gap none of the tests above can
    /// see, since every one of them calls `check_silent_dogs` directly and
    /// would keep passing even if the watcher's `ticks.tick().await` path
    /// were deleted entirely.
    ///
    /// IR-46: paused virtual time, advanced past one whole
    /// [`DOG_SILENCE_BUDGET`], is the forcing mechanism -- not a real sleep,
    /// so this stays in the fast tier rather than `mod slow`. The loop of
    /// `yield_now` calls below is what lets the watcher's own spawned task,
    /// and the engine task it talks to over a channel, actually run:
    /// `tokio::time::advance` wakes a sleeper whose deadline has elapsed,
    /// it does not itself drive the scheduler through everything that
    /// sleeper then does.
    #[tokio::test(start_paused = true)]
    async fn the_watcher_restarts_a_silent_dog_after_one_budget_of_paused_time() {
        let h = crate::testing::harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        start_test_dog(&h.ctx, "metrics").await;
        let refusals = h.ctx.dog_refusals.clone();

        let watch = spawn_silent_dog_watch(h.ctx.supervisor.clone(), refusals.clone());

        // The watcher's own interval fires an immediate first tick, which
        // records the dog as seen-silent-since-now the same way every
        // direct-call test above seeds `seen`. That first tick has to
        // actually run, against the PRE-advance clock, before time moves --
        // otherwise the "first look" lands after the jump and the dog
        // never accrues a whole budget of silence.
        tokio::task::yield_now().await;

        // `Interval::tick()` reports the DEADLINE it was scheduled for, not
        // the clock's current instant -- so one large `advance` collapses
        // every missed tick into a single wake whose reported time has only
        // moved forward by one `DOG_SILENCE_POLL`, not by however far the
        // clock actually jumped. Advancing one poll period at a time, and
        // letting the watcher's own task run after each step, is what
        // actually walks its reported `now` past a whole budget the way a
        // real, un-paused clock would.
        let ticks_in_a_budget = (DOG_SILENCE_BUDGET.as_nanos() / DOG_SILENCE_POLL.as_nanos())
            .try_into()
            .expect("a silence budget of a few seconds fits in a u32 tick count");
        for _ in 0..ticks_in_a_budget {
            tokio::time::advance(DOG_SILENCE_POLL).await;
            tokio::task::yield_now().await;
        }
        for _ in 0..64 {
            if !refusals.restarting().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(
            refusals.restarting(),
            vec!["metrics".to_string()],
            "one budget of silence, driven through the watcher's own tick, must earn exactly one restart"
        );
        assert!(
            refusals.stale().is_empty(),
            "one silence is a dog to restart, not a dog to give up on"
        );

        watch.abort();
    }
}
