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
//! its `[<name>]` section of `dogs.toml`, which the second names. The reply is opaque text the
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
use shep_core::config::{AppConfig, DogsConfig, ResolvedApp, normalize};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{BusEvent, DogSource, ProcessEventKind, ProcessInfo};
use shep_core::selector::ProcessSelector;
use shep_core::status::ProcStatus;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use crate::bus::{Bus, SharedEvent};
use crate::supervisor::SupervisorHandle;

/// One dog the daemon knows about: its name, and where its binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogSpec {
    /// The dog's name: the `[<name>]` key and the entry's name.
    pub name: String,
    /// Where its binary comes from.
    pub source: DogSource,
}

/// Error assembling a dog's app config, or reading its section
///
/// `Debug` is derived and needs no redaction: the variants carry a path, a
/// normalizer complaint about a config this module assembled itself, or a
/// TOML parser message, never a value read out of a parsed `[<name>]`
/// table. The one way a section's own text can reach a message is a *syntax*
/// error, where the parser quotes the line it failed on; that is the same
/// exposure [`DogsConfigError`](shep_core::config::DogsConfigError)
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
/// the `name` its `Request::DogConfig` has to carry. No `[<name>]`
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
    // The name the operator registered this dog under: the `[<name>]`
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
    // no `[<name>]` VALUE travels in the environment. That is the key,
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
    events: &Bus,
) {
    for spec in specs {
        let app = match dog_app(spec, paths) {
            Ok(app) => app,
            Err(err) => {
                tracing::warn!(dog = %spec.name, %err, "a dog did not start");
                continue;
            }
        };
        // Read before `start_dog` takes the app. This is the one place that
        // knows which file a dog's spawn actually resolved to (a built-in
        // dog's is this shep's own binary, an adopted one's is whatever the
        // operator's `[<name>]` named), and an operator reading the
        // dog's log during an upgrade is usually asking exactly that.
        let script = app.config().script.clone();
        match supervisor.start_dog(app, spec.source.clone()).await {
            Ok(info) if info.dog.is_none() => tracing::warn!(
                dog = %spec.name,
                "a sheep is already registered under this name; the dog did not start"
            ),
            Ok(info) => {
                // `start_dog` is idempotent by name, so this reply may be a
                // dog that was already running rather than one just spawned
                // — which is why the wording is about the binary this
                // shepherd resolved and not about a spawn having happened.
                // `spawn_dog_watch` narrates the spawns, off the bus, where
                // that distinction is not a guess.
                narrate(
                    events,
                    &info,
                    &format!("shep has this dog enabled, running the binary at {script}"),
                )
                .await;
            }
            Err(err) => tracing::warn!(dog = %spec.name, %err, "a dog did not start"),
        }
    }
}

/// The `[<name>]` section of `path`, a `dogs.toml`, rendered back to TOML
/// text.
///
/// Reads the file on every call rather than serving a copy cached at boot:
/// one reader can never be stale, and it is what makes
/// `shep disable X && shep enable X` re-read an edited section.
///
/// A missing file, or a file with no such section, is `Ok(String::new())` —
/// a dog with no configuration is the ordinary case, not a fault. That
/// covers every home that has never had a dog configured: `dogs.toml` is
/// written by the boot-time migration or by hand, so a home with neither
/// simply has no file.
///
/// # Errors
/// - [`DogError::Config`]: the file exists and is not valid `dogs.toml`,
///   or its section will not render back to TOML.
/// - [`DogError::Io`] — the file exists and could not be read.
pub fn dog_section(path: &Path, name: &str) -> Result<String, DogError> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(DogError::Io(err)),
    };
    // Loaded through shep-core's own type rather than parsed here, so a
    // broken `dogs.toml` is one named error and not a second parser's
    // opinion of the same file. The keys are dog names with no prefix:
    // `[metrics]` here is what `[dog.metrics]` was in `shep.toml` before
    // the boot-time migration moved it.
    let config =
        DogsConfig::load(Some(&source)).map_err(|err| DogError::Config(err.to_string()))?;
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
/// names and counts. A dog's name is the `[<name>]` key an operator
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
    ///
    /// Answers whether this CHANGED anything — whether the dog was not
    /// already marked as having got in. A dog reconnects, and often; the
    /// caller writes a line into that dog's own log the first time this
    /// shepherd hears from it, and a boolean is what keeps that from
    /// becoming one line per reconnect for the life of the daemon.
    pub fn handshook(&self, name: &str) -> bool {
        let mut seen = self.lock();
        seen.refusals.remove(name);
        seen.handshook.insert(name.to_string())
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

/// How many distinct peer pids [`PeerContacts`] remembers at once.
///
/// The question this state answers is only ever asked about a dog the
/// supervisor is running RIGHT NOW, and only ever a few seconds after that
/// dog was first seen silent ([`DOG_SILENCE_BUDGET`]). So what has to
/// survive eviction is a handful of long-lived dog processes against
/// whatever else dialled the socket recently, and a dog that reconnects
/// refreshes its own entry every time it does.
///
/// A thousand DISTINCT peer pids inside one silence budget would take about
/// two hundred `shep` invocations a second, which is not a workload — and if
/// it happened, the honest degradation is the "could not attribute this"
/// arm of `record_silent_dog`, which names both candidates instead of
/// picking one. That is why this is a bound and not a promise.
///
/// Note what does NOT churn this map: a poll loop. One `shep daemon reload`
/// was measured opening 442 connections in 9.8 seconds, and every one of
/// them came from the one CLI process, so they are one entry.
const PEER_CONTACT_CAPACITY: usize = 1024;

/// How long this map must have been watching before a pid's ABSENCE from it
/// means anything.
///
/// `Contact::None` supports exactly one claim, that a dog is not reaching this
/// shepherd's socket, and that claim is only the map's to make about a stretch
/// it was actually listening for. A successor built by [`crate::boot`] starts
/// empty at every `execve`, so without this every dog carried across a
/// `shep daemon reload` looked, for its first seconds, exactly like a dog that
/// had never called: absent from the map, therefore `Unreachable`, therefore
/// told to reinstall a binary that was fine.
///
/// One budget, and the ladder WAITS for it rather than racing it. That
/// pairing is the whole design and neither half works alone. An earlier
/// version made this three budgets and left the watch running, which put the
/// stale verdict at two budgets against a map that was still cold: every
/// unreachable dog then got the unattributable message, `silent_dogs` dropped
/// it from later looks because it was stale, and the reinstall advice this
/// ladder exists to earn became unreachable in practice. Deleting a true
/// message is as much a defect as inventing one.
///
/// So [`spawn_silent_dog_watch`] judges nothing while this is warming, and a
/// dog's silence is measured from when this shepherd could actually observe
/// it. The stale verdict lands one warm-up later than it used to and reads a
/// map that has been listening for two budgets by then.
///
/// Before it elapses `from_pid` answers [`Contact::Unknown`], which routes to
/// `Silence::Unattributed` and names both candidates instead of picking one.
/// That arm is still reachable, on the path it is actually for: a platform
/// that will not name a peer's pid.
const PEER_CONTACT_WARMUP: Duration = DOG_SILENCE_BUDGET;

/// What this daemon has observed arriving on its socket, keyed by the
/// connecting process's pid.
///
/// # What it is for
///
/// One question, asked by `record_silent_dog` and by nothing else: when a
/// dog has been running without ever handshaking, is it failing to REACH
/// this daemon, or is it reaching it and not saying who it is? Those two
/// have opposite fixes — the first is answered by reinstalling the binary,
/// and the second cannot be, because the binary is working and is merely
/// too old (or too casually written) to name itself in its `Hello`.
///
/// Before this existed the daemon could not tell them apart and asserted
/// the first. A real operator followed that advice for two days against a
/// dog that was connected the whole time and serving every request it was
/// asked, whose only defect was a `Hello` with no `dog_name` in it.
///
/// # Why a pid and not a connection count
///
/// A pid is the one identifier both sides of the question already have:
/// the supervisor knows the pid it spawned the dog as, and `SO_PEERCRED`
/// (or `getpeereid`'s macOS sibling) tells the connection layer the pid on
/// the other end of the socket. Nothing has to be added to the protocol,
/// which matters because the dogs this is diagnosing are exactly the ones
/// too old to have sent anything new.
///
/// # Platform
///
/// Unix only in practice. Windows has no post-accept peer check by design
/// (`shep_core::transport`'s module doc is the canonical writeup: the pipe's
/// ACL answers the same-user question earlier and in the kernel, and
/// establishing an admitted peer's IDENTITY would need
/// `ImpersonateNamedPipeClient` and raw FFI that `#![forbid(unsafe_code)]`
/// does not permit), so this map stays empty there and every lookup answers
/// [`Contact::Unknown`]. That is not a gap being papered over — it is the
/// reason `record_silent_dog` has a message for "could not attribute
/// this" rather than guessing.
///
/// `Debug` is derived and needs no redaction (IR-41), for the same reason
/// [`DogRefusals`]'s is: the map holds pids and two booleans' worth of
/// fact, and no configuration value can reach it.
#[derive(Debug, Clone, Default)]
pub struct PeerContacts {
    seen: Arc<Mutex<Contacts>>,
}

/// What [`PeerContacts`] holds, under its one lock.
#[derive(Debug)]
struct Contacts {
    /// When this map started watching, which is this daemon's own boot.
    ///
    /// [`tokio::time::Instant`], matching `SilentDogs` and for the same
    /// reason: a paused test moves that clock instead of sleeping out a whole
    /// budget. Under the lock rather than beside it so a test that cannot
    /// pause its clock, because it drives a real socket, can back-date it
    /// through `&self` instead.
    watching_since: Instant,
    /// One entry per remembered peer pid, at most
    /// [`PEER_CONTACT_CAPACITY`] of them.
    by_pid: BTreeMap<u32, Seen>,
    /// Ticks once per recorded connection, and is what
    /// [`Contacts::evict_oldest`] compares.
    ///
    /// A counter rather than an `Instant` because the only thing anything
    /// asks of it is which of two entries was touched later, and a counter
    /// answers that without a clock read per connection.
    clock: u64,
}

/// What has been seen from one peer pid.
#[derive(Debug)]
struct Seen {
    /// Whether any connection from this pid carried a `Hello.dog_name`.
    ///
    /// Recorded whatever the handshake's verdict was: this says what the
    /// peer SENT, and a dog refused on protocol skew still named itself.
    named_a_dog: bool,
    /// [`Contacts::clock`] as of the most recent connection from this pid.
    touched: u64,
}

/// What [`PeerContacts`] has seen from one pid.
///
/// `#[non_exhaustive]`: `shep-daemon` is a published library, and a fourth
/// answer — a peer seen but refused before it could speak, say — would
/// otherwise be a breaking change for an out-of-tree matcher (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contact {
    /// Nothing has ever connected from this pid.
    ///
    /// A dog running as this pid is not reaching the socket at all, which
    /// is the one case where reinstalling the binary is the right advice.
    None,
    /// Connections have arrived from this pid, and not one of them named a
    /// dog in its `Hello`.
    ///
    /// The dog is reaching this daemon and may be serving every request
    /// it is asked. It is built against shep-client older than 0.1.23, or
    /// it connects with `Client::connect` rather than
    /// `ReconnectingClient::connect_as_dog`.
    Anonymous,
    /// A connection from this pid named a dog in its `Hello`.
    Named,
    /// There is nothing recorded either way: no pid was available (Windows,
    /// or a `peer_cred` that reported none), or this pid's entry has been
    /// evicted.
    ///
    /// Distinct from [`Self::None`] and the distinction is the whole point:
    /// "nothing has connected" is a finding, and "I could not look" is not.
    Unknown,
}

impl PeerContacts {
    /// Builds an empty record — a daemon nothing has connected to yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one connection arriving from `pid`.
    ///
    /// Called before a byte is read, so a peer that connects and then says
    /// nothing at all still counts as having reached this daemon — which is
    /// exactly the distinction the silence ladder needs.
    pub fn connected(&self, pid: u32) {
        let mut seen = self.lock();
        seen.clock = seen.clock.saturating_add(1);
        let clock = seen.clock;
        match seen.by_pid.get_mut(&pid) {
            Some(entry) => entry.touched = clock,
            None => {
                seen.by_pid.insert(
                    pid,
                    Seen {
                        named_a_dog: false,
                        touched: clock,
                    },
                );
                seen.evict_oldest();
            }
        }
    }

    /// Records that a connection from `pid` named a dog in its `Hello`.
    ///
    /// Sticky: once a pid has named a dog, a later anonymous connection from
    /// the same pid does not unsay it. The question being answered is "has
    /// this process EVER named itself", and a dog that names itself on one
    /// connection is not the dog this diagnosis is about.
    pub fn named_a_dog(&self, pid: u32) {
        let mut seen = self.lock();
        seen.clock = seen.clock.saturating_add(1);
        let clock = seen.clock;
        let entry = seen.by_pid.entry(pid).or_insert(Seen {
            named_a_dog: false,
            touched: clock,
        });
        entry.named_a_dog = true;
        entry.touched = clock;
        seen.evict_oldest();
    }

    /// Whether this map is still too new for an absence to mean anything.
    ///
    /// Read by [`spawn_silent_dog_watch`], which judges no dog while it is
    /// true. See `PEER_CONTACT_WARMUP` for why the ladder waits rather than
    /// races.
    #[must_use]
    pub fn is_warming(&self) -> bool {
        self.lock().watching_since.elapsed() < PEER_CONTACT_WARMUP
    }

    /// Back-dates the watching clock so this map reads as warm.
    ///
    /// For the cases that drive a REAL socket and so cannot pause their
    /// clock. `a_dog_that_never_calls_still_earns_its_rebuild_after_the_warm_up`
    /// is the one that walks the boundary, and it does not use this.
    #[cfg(test)]
    pub(crate) fn force_warm(&self) {
        let mut seen = self.lock();
        seen.watching_since = Instant::now() - PEER_CONTACT_WARMUP * 2;
    }

    /// What has been seen from `pid`, or [`Contact::Unknown`] when there is
    /// no pid to ask about.
    #[must_use]
    pub fn from_pid(&self, pid: Option<u32>) -> Contact {
        let Some(pid) = pid else {
            return Contact::Unknown;
        };
        let seen = self.lock();
        match seen.by_pid.get(&pid) {
            // Absence is a finding only once this map has been watching long
            // enough for it to be one. See `PEER_CONTACT_WARMUP`.
            None if seen.watching_since.elapsed() < PEER_CONTACT_WARMUP => Contact::Unknown,
            None => Contact::None,
            Some(seen) if seen.named_a_dog => Contact::Named,
            Some(_) => Contact::Anonymous,
        }
    }

    /// Takes the lock, recovering from a poisoned one rather than
    /// propagating the panic — the same call [`DogRefusals::lock`] makes,
    /// for the same reason: every critical section here is a lookup or an
    /// increment on a plain `BTreeMap`, so a panic elsewhere cannot leave a
    /// torn value, and taking down a daemon whose whole job is staying up
    /// would be the worse failure.
    fn lock(&self) -> std::sync::MutexGuard<'_, Contacts> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for Contacts {
    fn default() -> Self {
        Self {
            watching_since: Instant::now(),
            by_pid: BTreeMap::new(),
            clock: 0,
        }
    }
}

impl Contacts {
    /// Drops the least recently touched entry, if the map has outgrown
    /// [`PEER_CONTACT_CAPACITY`].
    ///
    /// A scan rather than a second index: it runs only on the insert that
    /// overflows a full map, over at most a thousand entries of two words
    /// each, and the alternative is a heap to keep in step with every
    /// touch — which is more code to be wrong in for a cost nothing here
    /// can measure.
    fn evict_oldest(&mut self) {
        if self.by_pid.len() <= PEER_CONTACT_CAPACITY {
            return;
        }
        let oldest = self
            .by_pid
            .iter()
            .min_by_key(|(_, seen)| seen.touched)
            .map(|(pid, _)| *pid);
        if let Some(pid) = oldest {
            self.by_pid.remove(&pid);
        }
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
/// name is a `[<name>]` KEY rather than one of its values.
#[derive(Debug, Default)]
pub(crate) struct SilentDogs {
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
pub(crate) async fn check_silent_dogs(
    supervisor: &SupervisorHandle,
    refusals: &DogRefusals,
    contacts: &PeerContacts,
    events: &Bus,
    seen: &mut SilentDogs,
    now: Instant,
) -> Vec<(String, Refusal)> {
    // A stopped engine has no dogs left to wait on. `seen` is left untouched
    // rather than cleared: a look that could not see the flock has learned
    // nothing about it, and must not hand every dog a fresh budget.
    // Nothing is judged while attribution is still maturing. A verdict
    // written now would read a cold map, and the stale rung is spent once:
    // `silent_dogs` drops a stale dog from every later look, so a wrong
    // answer here is the LAST answer. `seen` is left untouched for the same
    // reason the stopped-engine arm above leaves it: a look that could not
    // judge has learned nothing, and each dog's budget starts when this
    // shepherd could actually observe it. See `PEER_CONTACT_WARMUP`.
    if contacts.is_warming() {
        return Vec::new();
    }
    let Ok(infos) = supervisor.list_checked().await else {
        return Vec::new();
    };
    let silent = silent_dogs(&infos, refusals);
    let mut acted = Vec::new();
    for name in seen.due(&silent, now) {
        // Read off the SAME listing the silence was judged from, so the pid
        // a message names is the process that was silent rather than
        // whatever holds the name by the time the message is written.
        let info = infos.iter().find(|info| info.name == name);
        let evidence = Silence::of(info.and_then(|info| info.pid), contacts);
        let verdict = record_silent_dog(&name, info, evidence, refusals, events, supervisor).await;
        acted.push((name, verdict));
    }
    acted
}

/// What this shepherd actually observed about a silent dog's connections —
/// the difference between two silences that look identical in a listing and
/// have opposite fixes.
///
/// Built from two facts and no inference: the pid the supervisor spawned the
/// dog as, and what [`PeerContacts`] has seen arrive from that pid. Every
/// arm below is something the daemon watched happen; there is deliberately
/// no arm for a cause it merely suspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Silence {
    /// Nothing has ever connected from the dog's pid. The dog is not
    /// reaching this shepherd's socket at all.
    Unreachable {
        /// The pid nothing has arrived from.
        pid: u32,
    },
    /// Connections HAVE arrived from the dog's pid, and not one of them
    /// named a dog. The dog reaches this shepherd and may be serving every
    /// request it is asked; what it does not do is say who it is.
    Anonymous {
        /// The pid those connections came from.
        pid: u32,
    },
    /// There is no pid to attribute by, so neither of the above can be
    /// ruled in or out. Windows, a unix whose OS declines to name a peer's
    /// pid, a dog whose process had already gone by the time the listing was
    /// read, or an entry aged out of a full [`PeerContacts`].
    Unattributed,
}

impl Silence {
    /// What `pid`'s connection history says, if anything.
    ///
    /// [`Contact::Named`] lands in [`Self::Unattributed`] rather than in an
    /// arm of its own, and that is the honest answer rather than a lazy one.
    /// A pid that named a dog and is nonetheless being judged silent is a
    /// contradiction: naming a dog is what sets `handshook`, and
    /// [`silent_dogs`] filters a handshook dog out before it can be seen
    /// quiet. The ways to reach it are pid reuse and a race with an entry
    /// being evicted — both of which mean the attribution is not to be
    /// trusted for THIS dog, which is precisely what `Unattributed` says.
    fn of(pid: Option<u32>, contacts: &PeerContacts) -> Self {
        match (pid, contacts.from_pid(pid)) {
            (Some(pid), Contact::None) => Self::Unreachable { pid },
            (Some(pid), Contact::Anonymous) => Self::Anonymous { pid },
            _ => Self::Unattributed,
        }
    }
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
/// needs no cooperation from the client.
///
/// **Peer credentials are read, but only to EXPLAIN the silence — never to
/// detect it.** Which dogs are silent is still the set difference above, and
/// still works identically on Windows, where there is no post-accept peer
/// check at all. What `SO_PEERCRED` (and `getpeereid`'s macOS sibling) buys
/// is the `evidence` parameter: whether anything has ever connected from
/// this dog's pid, and whether any of it named a dog. That turns one verdict
/// asserting a cause into three saying what was seen — see [`stale_verdict`]
/// for the incident that makes the difference worth a syscall. Where the
/// platform will not name a pid, [`Silence::Unattributed`] says so instead
/// of guessing.
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
    info: Option<&ProcessInfo>,
    evidence: Silence,
    refusals: &DogRefusals,
    events: &Bus,
    supervisor: &SupervisorHandle,
) -> Refusal {
    let verdict = refusals.refused(name);
    match verdict {
        Refusal::Restart => {
            let seen = first_rung_evidence(evidence);
            tracing::warn!(
                dog = %name,
                silent_for_secs = DOG_SILENCE_BUDGET.as_secs(),
                evidence = %seen,
                "a dog has been running without ever answering this shepherd; restarting it once from the binary on disk"
            );
            if let Some(info) = info {
                narrate(
                    events,
                    info,
                    &format!(
                        "this dog has been running for {}s without ever answering this shepherd: {seen}. Restarting it once from the binary on disk",
                        DOG_SILENCE_BUDGET.as_secs()
                    ),
                )
                .await;
            }
            // Awaited rather than spawned, which is the one difference from
            // `record_refused_dog`: that caller is a connection handler
            // holding a socket, and this one is a background loop with
            // nothing to delay. Awaiting it keeps the next look from running
            // while a kill ladder is still in flight, so a dog is never
            // judged mid-restart.
            restart_refused_dog(supervisor, name).await;
        }
        Refusal::Stale => {
            let verdict = stale_verdict(name, evidence);
            tracing::error!(dog = %name, "{verdict}");
            // Into the dog's OWN log as well, because that is the file the
            // verdict tells the operator to read. Before this it could not
            // hold a word of the reason it was being read for.
            if let Some(info) = info {
                narrate(events, info, &verdict).await;
            }
        }
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

/// The one clause the first rung adds about what this shepherd has seen.
///
/// Short, because the restart it accompanies happens either way and the
/// operator has nothing to decide yet. It is here so that the `journalctl`
/// record of the first rung already carries the fact the second rung's
/// verdict will turn on — an operator reading backwards after the fact
/// should not have to take the verdict's word for it.
fn first_rung_evidence(evidence: Silence) -> String {
    match evidence {
        Silence::Unreachable { pid } => {
            format!("nothing has connected to this shepherd from pid {pid}")
        }
        Silence::Anonymous { pid } => format!(
            "pid {pid} has connected to this shepherd without naming a dog, so the restart is unlikely to help"
        ),
        Silence::Unattributed => {
            "this shepherd cannot tell which process opened a connection".to_string()
        }
    }
}

/// The stale verdict, written from what this shepherd OBSERVED.
///
/// # The bug this replaces
///
/// One message served all three cases and asserted the harshest of them:
/// *the binary on disk cannot talk to this shep either, so this dog is
/// stale — rebuild or reinstall it*. That is earned only when the dog never
/// reached the socket. A real operator was given it about a dog that was
/// connected the whole time and correctly serving `DogConfig` and
/// `ListFlock`, whose only defect was a `Hello` with no `dog_name` in it,
/// and spent two days reinstalling a binary that reinstalling could never
/// have fixed.
///
/// So each arm below says what was seen, and then the step that actually
/// follows from it. The sentence *the binary on disk cannot talk to this
/// shep either* appears on exactly one path, and it is the path where this
/// shepherd watched nothing arrive.
///
/// # Why every arm ends in a command
///
/// The reader is an operator mid-incident with no agent helping them and no
/// reason to know what a handshake is. A verdict that names a cause and
/// stops leaves them to invent the next step, which is how the original
/// message turned into two days of reinstalling.
///
/// [`Refusal::Stale`]'s sibling ladder in [`record_refused_dog`] is
/// deliberately NOT softened to match. There the daemon watched a handshake
/// be refused on protocol skew and then watched the restarted binary be
/// refused the same way, so *rebuild or reinstall it against this shep* is
/// a conclusion from evidence rather than a guess.
fn stale_verdict(name: &str, evidence: Silence) -> String {
    let seen = "a dog restarted for never answering this shepherd has still not answered it";
    match evidence {
        Silence::Unreachable { pid } => format!(
            "{seen}, and nothing has ever connected to this shepherd's socket from its process (pid {pid}): \
             the binary on disk cannot reach this shep either, so this dog is stale and will not be \
             restarted again. Read its own log with `shep bleats {name}` for what it says about \
             connecting, then rebuild or reinstall it and run `shep restart {name}`. A dog \
             installed with cargo wants `cargo install <crate> --force`: its own version does \
             not change when the shep it was built against does, so a plain `cargo install` \
             reports the package already installed, builds nothing, and exits 0"
        ),
        Silence::Anonymous { pid } => format!(
            "{seen}, but its process (pid {pid}) HAS connected to this shepherd — every time without \
             naming a dog in its handshake, which is the only thing this shepherd waits for. The dog \
             is reaching shep and may be serving every request it is asked; reinstalling the same \
             build will NOT change that. It is built against shep-client older than 0.1.23, or it \
             connects with `Client::connect` instead of `ReconnectingClient::connect_as_dog`. Rebuild \
             it against shep-client 0.1.23 or newer, then run `shep restart {name}`. With cargo \
             that means `cargo install <crate> --force`: the dog's own version does not change \
             when its shep-client does, so a plain `cargo install` builds nothing and exits 0. \
             It will not be restarted again in the meantime, and it goes on running"
        ),
        Silence::Unattributed => format!(
            "{seen}, and this shepherd could not tell which process opened its connections, so it \
             cannot say which of two things is wrong. Either the dog is not reaching the socket at \
             all — rebuild or reinstall it — or it is reaching it and never names itself in the \
             handshake, which means a build against shep-client older than 0.1.23 and which \
             reinstalling the same build will not fix. Run `shep bleats {name}` to tell them apart: \
             a dog that cannot reach the socket says so in its own log, and one that is connected \
             and merely anonymous does not. It will not be restarted again"
        ),
    }
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
    contacts: PeerContacts,
    events: Bus,
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
            check_silent_dogs(&supervisor, &refusals, &contacts, &events, &mut seen, now).await;
        }
    })
}

/// The marker every line shep writes into a dog's own log begins with, once
/// the timestamp is past.
///
/// A dog's log is the dog's voice, and an operator reading it is entitled to
/// assume every line came from the process they started. Shep now writes
/// into that file too, so the file has to say which lines are whose.
///
/// Short and bracketed because it sits at the head of a line already
/// carrying a 30-character timestamp, and an operator scanning a long file
/// has to be able to pick these out without reading them. A dog that emits
/// `[shep]` at the start of a line of its own is impersonating the shepherd
/// in its own log, which is its problem rather than something this can
/// defend against.
const SHEP_VOICE: &str = "[shep]";

/// Writes one line of shep's own narration into `info`'s log, and publishes
/// it to whoever is following that log live.
///
/// # Why this exists
///
/// Every message about the incident this subsystem was rebuilt for went to
/// `shepd.err.log`. The dog's own log — the file shep's own error message
/// told the operator to read — could not contain a word of it. So `shep
/// bleats log-rotate` showed a dog repeating itself into an empty room, and
/// nothing about why shep had given up on it.
///
/// # Why it writes the file directly rather than through the pump
///
/// The pump owns a buffered handle and forwards what it writes to the bus,
/// so routing narration through it would put both halves in one place. It
/// is not used, for one reason that decides it: the pump ends when its
/// sheep's streams reach EOF, so the last thing there is to say about a dog
/// — how it exited — arrives after the only thing that could write it is
/// gone. A narration that silently vanished for exactly the events worth
/// narrating would be worse than none.
///
/// Writing directly is safe because of the property [`open_append`]
/// documents at length: `O_APPEND` makes every write seek to end atomically,
/// which is what already lets several instances share one log file. The
/// whole line is assembled and written in one call for that reason. It also
/// inherits `O_NOFOLLOW` and the ancestry check from that same function, so
/// narration cannot be redirected through a planted symlink any more than a
/// sheep's own output can.
///
/// The cost is ordering, and it is named rather than hidden: a narration
/// line can land ahead of dog output that was appended earlier and is still
/// in the pump's buffer, which is bounded by `IDLE_FLUSH` at 50 ms. The
/// timestamps say which came first even when the file order does not.
///
/// # The live half
///
/// [`Bus::publish_log`] is the same call a sheep's own line takes to reach
/// `shep bleats --follow`, so a follower sees the narration interleaved
/// with the dog's output in arrival order, marked exactly as the file marks
/// it. It carries the marker but not the timestamp: the stamp is a property
/// of the file (see `LOG_TIMESTAMP_FORMAT`), and a follower is watching the
/// line arrive.
///
/// A dog with no `err_file` — a peer daemon predating the field, in a
/// listing this daemon did not resolve — still gets the live half. Nothing
/// is logged about the missing path: it is not a failure, it is a listing
/// that did not carry one.
pub(crate) async fn narrate(events: &Bus, info: &ProcessInfo, message: &str) {
    let line = format!("{SHEP_VOICE} {message}");
    if let Some(path) = &info.err_file {
        let mut written = String::with_capacity(line.len() + 32);
        shep_core::logstamp::stamp_into(&mut written);
        written.push_str(&line);
        written.push('\n');
        // A failed open is already logged by `open_append`, with the path
        // and the OS error; there is nothing this could add. A failed write
        // is not, so it is reported here — and neither is propagated,
        // because a log shep cannot write to must not change what shep does
        // about the dog.
        if let Ok(mut file) = crate::tokio_runner::open_append(Path::new(path)).await {
            use tokio::io::AsyncWriteExt as _;
            // Taken AFTER the open, deliberately. The pump waits on this lock
            // for every line it writes, so holding it across a filesystem
            // open would stall a sheep's output for as long as the open took.
            // Held across the write and the flush together, which is the
            // whole record: see `crate::tokio_runner::record_lock` for what
            // interleaves without it.
            let _record = crate::tokio_runner::record_lock(Path::new(path))
                .lock_owned()
                .await;
            // The flush is not optional and not belt-and-braces.
            // `tokio::fs::File` copies into its own buffer and hands the real
            // `write(2)` to the blocking pool, so `write_all` returning means
            // the bytes were ACCEPTED, not that they reached the file — and
            // the type does not flush on drop, which is stated in tokio's own
            // docs. Without this, the line is written on a best-effort basis
            // and is lost outright often enough for a test reading the file
            // straight afterwards to catch it. That would be the original
            // defect all over again: shep with something to say and an empty
            // log where it should have said it.
            let written = async {
                file.write_all(written.as_bytes()).await?;
                file.flush().await
            }
            .await;
            if let Err(error) = written {
                tracing::warn!(
                    dog = %info.name,
                    %error,
                    "shep's own narration did not reach this dog's log"
                );
            }
        }
    }
    events.publish_log(BusEvent::LogErr { id: info.id, line });
}

/// `narrate`, for a caller that knows a dog's NAME and not its listing.
///
/// Spawned rather than awaited, and that is the reason this is a second
/// function rather than a parameter. Both callers are connection handlers
/// mid-handshake: one is about to send an ack the dog is waiting on, and
/// the other has a refusal already queued behind it. Neither may be held up
/// by a listing round trip and a file open, and neither has anything to do
/// with the answer.
///
/// A name that does not resolve to a dog is silently nothing. By the time
/// this runs the dog may have been stopped, deleted, or replaced by a sheep
/// of the same name; narrating into whatever holds the name now would be a
/// worse answer than saying nothing.
pub(crate) fn narrate_by_name(
    supervisor: &SupervisorHandle,
    events: &Bus,
    name: &str,
    message: String,
) {
    let supervisor = supervisor.clone();
    let events = events.clone();
    let name = name.to_string();
    tokio::spawn(async move {
        let Ok(infos) = supervisor.list_checked().await else {
            return;
        };
        if let Some(info) = infos
            .iter()
            .find(|info| info.name == name && info.dog.is_some())
        {
            narrate(&events, info, &message).await;
        }
    });
}

/// How a dog's process stopped existing, in the plainest words there are.
///
/// A signal number rather than a name, which is the rule
/// [`ExitInfo::signal`](shep_core::protocol::ExitInfo::signal) states for
/// itself: the number is what the OS reported, and shep-core deliberately
/// carries no table to turn it into `SIGKILL`. The daemon is an OS-aware
/// layer and could, but a dog's log is read next to `journalctl`, and one
/// spelling in both places is worth more than a nicer one in this.
fn exit_words(info: &ProcessInfo) -> String {
    match info.last_exit {
        Some(exit) => match (exit.code, exit.signal) {
            (Some(code), _) => format!("this dog's process exited with code {code}"),
            (None, Some(signal)) => {
                format!("this dog's process was killed by signal {signal}")
            }
            (None, None) => {
                "this dog's process stopped, and the OS reported neither an exit code nor a signal"
                    .to_string()
            }
        },
        // Reachable rather than defensive: `last_exit` is `None` when the
        // peer that built this listing predates the field, and a successor
        // reading a carried entry can see one.
        None => "this dog's process stopped, and this shepherd has no record of how".to_string(),
    }
}

/// Watches the bus and records, locally, every enabled dog that exhausts
/// its restart budget.
///
/// The shepherd cannot DELIVER an alert about a dead bark dog: it has no
/// sinks and no webhook code, by design. What it can guarantee is a local
/// trail, so an operator reading `shep barks` after an outage finds the
/// moment alerting stopped rather than a gap they have to infer.
///
/// It also writes a dog's own spawn and exit into that dog's log, in shep's
/// voice (see `narrate`). Read from the bus rather than from the two
/// places that cause them, and for the same reason the bark record above is:
/// a `Start` on the bus is a spawn that really happened, while a call site
/// answering `Ok` covers `start_dog`'s idempotent no-op as well, and the
/// pump that could have carried an exit is already ending when the exit
/// fires.
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
    publish: Bus,
    barks: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                // Only a DOG's Errored earns a bark record. A sheep's
                // Errored is bark's job — bark writes those records over its
                // own client connection — and duplicating that write here
                // would leave one event with two authors in one file. Exit
                // is excluded from the BARKS file too: it fires on every
                // restart a dog survives, and a `barks.jsonl` full of those
                // is one an operator stops reading. It is not excluded from
                // the dog's own log, which is where an operator goes looking
                // for exactly that.
                Ok(event) => {
                    let BusEvent::Process {
                        event: kind, info, ..
                    } = &*event
                    else {
                        continue;
                    };
                    if info.dog.is_none() {
                        continue;
                    }
                    match kind {
                        ProcessEventKind::Errored => {
                            record_dog_errored(&barks, &info.name, info.restarts);
                        }
                        ProcessEventKind::Start => {
                            let pid = info
                                .pid
                                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string());
                            narrate(
                                &publish,
                                info,
                                &format!("shep started this dog; its process is pid {pid}"),
                            )
                            .await;
                        }
                        ProcessEventKind::Exit => {
                            narrate(&publish, info, &exit_words(info)).await;
                        }
                        _ => {}
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

    /// fails if a `[<name>]` value is folded into the child's
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
            &paths.dogs_config,
            "[bark]\nwebhook = \"https://example.invalid/hook\"\n",
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
        let path = dir.path().join("dogs.toml");
        std::fs::write(
            &path,
            "[bark]\ndebounce = \"30s\"\n\n[metrics]\nport = 9615\n",
        )
        .unwrap();

        let bark = dog_section(&path, "bark").unwrap();
        assert!(bark.contains("debounce"));
        assert!(
            !bark.contains("9615"),
            "one dog never sees another's config"
        );
        // Round-trips as TOML, since that is the contract the dog parses under.
        let parsed: toml::Table = toml::from_str(&bark).unwrap();
        assert_eq!(parsed["debounce"].as_str(), Some("30s"));

        assert_eq!(dog_section(&path, "absent").unwrap(), "");
        assert_eq!(
            dog_section(&dir.path().join("gone.toml"), "bark").unwrap(),
            ""
        );
    }

    /// fails if the read that moved to `dogs.toml` changed what a dog is
    /// served.
    #[test]
    fn a_section_reaches_the_wire_exactly_as_it_did_from_shep_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dogs.toml");
        std::fs::write(&path, "[bark]\ndebounce = \"30s\"\n").expect("write");

        // Byte-for-byte what the old `[dog.bark]` read produced. The
        // dog-facing contract not moving is the whole of decision 3, so it
        // is pinned as a string rather than as a parse.
        assert_eq!(
            dog_section(&path, "bark").expect("section"),
            "debounce = \"30s\"\n"
        );
    }

    #[test]
    fn a_dog_with_no_section_still_gets_an_empty_string() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dogs.toml");
        std::fs::write(&path, "[bark]\ndebounce = \"30s\"\n").expect("write");

        assert_eq!(dog_section(&path, "metrics").expect("section"), "");
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
        let watch = spawn_dog_watch(rx, events.clone(), barks_path.clone());

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
        let watch = spawn_dog_watch(rx, events.clone(), barks_path.clone());

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

    /// How often [`settle_until`] looks while the watch works.
    ///
    /// Finer than [`DOG_SILENCE_POLL`] so a rung is seen inside the poll
    /// period it lands in, which is the resolution the timing assertions
    /// below are read at.
    const SETTLE_STEP: Duration = Duration::from_millis(250);

    /// How long [`settle_until`] gives a rung before it gives up.
    ///
    /// A whole warm-up plus both rungs is fifteen seconds of virtual time,
    /// so this is generous by a wide margin on purpose: it is a hang guard
    /// and not a timing assertion, and the assertions that DO measure make
    /// their own claims. Generosity is free here, because the clock it
    /// bounds is virtual -- a wait that never settles fails in about no real
    /// time at all rather than sitting on a wall clock.
    const LADDER_BUDGET: Duration = Duration::from_secs(45);

    /// Waits for `settled` to answer true, and answers how much virtual time
    /// that took.
    ///
    /// The forcing mechanism (IR-46) the two watch tests below run on, and
    /// the reason neither counts yields any more. Under `start_paused` the
    /// runtime advances the clock itself, but only once every task is idle,
    /// and work on the blocking pool holds it there: measured on this
    /// workspace, a 60s virtual sleep did not resolve until a 300ms
    /// `spawn_blocking` had returned. That is what makes sleeping here a
    /// barrier rather than a slower spin, and it matters
    /// because each of `spawn_silent_dog_watch`'s looks goes through a
    /// supervisor round trip and a write into the dog's own log, which
    /// `narrate` puts on the blocking pool.
    ///
    /// A `yield_now` loop cannot do this, and that is the bug being removed
    /// rather than a style preference. Yielding keeps a task runnable, so
    /// the runtime never idles, so the clock never advances of its own
    /// accord and the blocking pool is never waited for. What is left is a
    /// wall-clock race between a few hundred cheap scheduler passes and a
    /// real file write, which a loaded machine wins: these tests passed on
    /// every quiet run and failed on CI, and raising the yield count only
    /// moved the load at which they failed.
    ///
    /// Panics, naming `what`, if `settled` has not answered true within
    /// `within` of virtual time. Bounded rather than open so a watch that
    /// stops looking says so here, instead of hanging until the harness
    /// times out the whole binary and names nothing -- the second failure
    /// shape IR-46 exists to catch.
    async fn settle_until(
        what: &str,
        within: Duration,
        mut settled: impl FnMut() -> bool,
    ) -> Duration {
        let began = Instant::now();
        tokio::time::timeout(within, async {
            while !settled() {
                tokio::time::sleep(SETTLE_STEP).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} did not happen within {within:?} of virtual time"));
        began.elapsed()
    }

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
        // Past the warm-up: the ladder judges nothing while attribution is
        // still maturing, and this case is about the rungs rather than the
        // gate. `a_dog_that_never_calls_still_earns_its_rebuild_after_the_warm_up`
        // walks the boundary itself.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;
        let refusals = &h.ctx.dog_refusals;
        let contacts = &h.ctx.peer_contacts;
        let events = &h.ctx.events;
        let mut seen = SilentDogs::default();
        let t0 = Instant::now();

        assert!(
            check_silent_dogs(&h.ctx.supervisor, refusals, contacts, events, &mut seen, t0)
                .await
                .is_empty(),
            "a dog seen quiet for the first time has not yet been quiet for any length of time"
        );

        assert_eq!(
            check_silent_dogs(
                &h.ctx.supervisor,
                refusals,
                contacts,
                events,
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
                contacts,
                events,
                &mut seen,
                t0 + 2 * DOG_SILENCE_BUDGET
            )
            .await,
            vec![("metrics".to_string(), Refusal::Stale)],
            "the restart ran and the dog still has not spoken, so the ladder ends here"
        );
        assert_eq!(refusals.stale(), vec!["metrics".to_string()]);

        assert!(
            check_silent_dogs(
                &h.ctx.supervisor,
                refusals,
                contacts,
                events,
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
        let contacts = &h.ctx.peer_contacts;
        let events = &h.ctx.events;
        refusals.handshook("metrics");
        let mut seen = SilentDogs::default();
        let t0 = Instant::now();

        for elapsed in [0, 1, 2, 10] {
            assert!(
                check_silent_dogs(
                    &h.ctx.supervisor,
                    refusals,
                    contacts,
                    events,
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
        let contacts = &h.ctx.peer_contacts;
        let events = &h.ctx.events;
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
                    contacts,
                    events,
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
        // Past the warm-up: the ladder judges nothing while attribution is
        // still maturing, and this case is about the rungs rather than the
        // gate. `a_dog_that_never_calls_still_earns_its_rebuild_after_the_warm_up`
        // walks the boundary itself.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;
        let refusals = &h.ctx.dog_refusals;
        let contacts = &h.ctx.peer_contacts;
        let events = &h.ctx.events;
        let mut seen = SilentDogs::default();
        let t0 = Instant::now();

        for look in 0..20 {
            assert!(
                check_silent_dogs(
                    &h.ctx.supervisor,
                    refusals,
                    contacts,
                    events,
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
                contacts,
                events,
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
    /// IR-46: [`settle_until`] is the forcing mechanism -- virtual time the
    /// runtime walks by itself, under a timeout this test sets, rather than
    /// a real sleep. So this stays in the fast tier rather than `mod slow`.
    ///
    /// Fails if the warm-up swallows the one verdict it exists to protect.
    ///
    /// The regression this exists for: `PEER_CONTACT_WARMUP` was three
    /// budgets while the ladder kept running, so the stale rung was spent at
    /// two budgets against a map that was still cold. `Silence::of` answered
    /// `Unattributed`, `silent_dogs` then dropped the dog because it was
    /// stale, and no later look ever reclassified it. A dog that genuinely
    /// never reached the socket could no longer be told to rebuild, which is
    /// the exact advice this ladder exists to earn.
    ///
    /// A unit test over `from_pid` and `stale_verdict` passed throughout,
    /// because both were right in isolation. That is the same shape as the
    /// bug this whole branch is about, so this case walks the real watch
    /// across the real boundary instead.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_never_calls_still_earns_its_rebuild_after_the_warm_up() {
        // A lower bound on when `PeerContacts` started warming, taken
        // before the harness that builds it. The timing assertion below
        // reads against this rather than against the watch's own spawn,
        // because the map's clock starts inside `harness` and nothing out
        // here can ask it when.
        let map_started_no_earlier_than = Instant::now();
        let h = crate::testing::harness(vec![
            ProcScript::never_exits(),
            ProcScript::never_exits(),
            ProcScript::never_exits(),
        ]);
        start_test_dog(&h.ctx, "metrics").await;
        let refusals = h.ctx.dog_refusals.clone();
        let contacts = h.ctx.peer_contacts.clone();
        assert!(contacts.is_warming(), "a fresh map starts cold");

        let watch = spawn_silent_dog_watch(
            h.ctx.supervisor.clone(),
            refusals.clone(),
            contacts.clone(),
            h.ctx.events.clone(),
        );

        // Waiting for the first rung, rather than walking a fixed number of
        // ticks and then asserting nothing happened, is what turns the
        // warm-up gate from assumed into proved. An "assert nothing yet"
        // over a fixed walk passes just as happily when the watch's loop
        // never ran at all, which is the vacuous shape IR-46 names.
        let restart_rung = settle_until("the silent dog's restart rung", LADDER_BUDGET, || {
            !refusals.restarting().is_empty()
        })
        .await;
        assert_eq!(
            refusals.restarting(),
            vec!["metrics".to_string()],
            "the dog nothing ever connected from is the one that earns the rung"
        );

        // WHEN the rung landed is the whole regression. A ladder running on
        // a cold map reaches this one budget after the watch spawned; one
        // that waits reaches it a budget after the warm-up ends, and the
        // warm-up cannot have ended before `map_started_no_earlier_than`.
        // The `is_warming` assertion above is what makes this discriminate:
        // it pins the map as still cold at spawn, so the two cases cannot
        // land on the same instant.
        let first_rung_at = map_started_no_earlier_than.elapsed();
        assert!(
            first_rung_at >= PEER_CONTACT_WARMUP + DOG_SILENCE_BUDGET,
            "a cold map must judge nothing: the first rung landed {first_rung_at:?} in, \
             which is inside the {PEER_CONTACT_WARMUP:?} warm-up plus one \
             {DOG_SILENCE_BUDGET:?} budget of silence it has to wait out"
        );
        assert!(
            restart_rung >= DOG_SILENCE_BUDGET,
            "no rung can be earned in less than a whole budget of silence: {restart_rung:?}"
        );

        // The second rung, read off a map that has now been listening for
        // longer than any dog has been quiet.
        settle_until("the silent dog's stale rung", LADDER_BUDGET, || {
            refusals.stale().contains(&"metrics".to_string())
        })
        .await;
        let info = h
            .ctx
            .supervisor
            .list()
            .await
            .into_iter()
            .find(|info| info.name == "metrics")
            .expect("the dog fixture is listed");
        let verdict = stale_verdict("metrics", Silence::of(info.pid, &contacts));
        assert!(
            verdict.contains("cannot reach this shep"),
            "the earned rebuild advice must survive the warm-up: {verdict}"
        );
        watch.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_watcher_restarts_a_silent_dog_after_one_budget_of_paused_time() {
        let h = crate::testing::harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        start_test_dog(&h.ctx, "metrics").await;
        // Past the warm-up: the ladder judges nothing while attribution is
        // still maturing, and this case is about the rungs rather than the
        // gate. `a_dog_that_never_calls_still_earns_its_rebuild_after_the_warm_up`
        // walks the boundary itself.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;
        let refusals = h.ctx.dog_refusals.clone();

        let watch = spawn_silent_dog_watch(
            h.ctx.supervisor.clone(),
            refusals.clone(),
            h.ctx.peer_contacts.clone(),
            h.ctx.events.clone(),
        );

        // The watcher's own interval fires an immediate first tick, which
        // records the dog as seen-silent-since-now the same way every
        // direct-call test above seeds `seen`. Nothing here has to arrange
        // for that tick to run: the wait below idles the runtime, and an
        // idle runtime is exactly when the paused clock moves and a due
        // timer fires.
        let waited = settle_until("the silent dog's restart", LADDER_BUDGET, || {
            !refusals.restarting().is_empty()
        })
        .await;

        assert_eq!(
            refusals.restarting(),
            vec!["metrics".to_string()],
            "one budget of silence, driven through the watcher's own tick, must earn exactly one restart"
        );
        // The budget is the claim this test's name makes, so it is asserted
        // rather than assumed. A watch that judged a dog early would earn
        // the same restart and pass on the line above alone.
        assert!(
            waited >= DOG_SILENCE_BUDGET,
            "a restart is earned by a whole budget of silence, not by less: {waited:?}"
        );
        assert!(
            refusals.stale().is_empty(),
            "one silence is a dog to restart, not a dog to give up on"
        );

        watch.abort();
    }

    /// Fails if a pid nothing has ever connected from cannot be told apart
    /// from one that has connected and said nothing useful.
    ///
    /// The whole diagnosis rests on that difference: one means the dog is
    /// not reaching the socket, the other means it is reaching it and not
    /// naming itself, and they have opposite fixes.
    #[tokio::test(start_paused = true)]
    async fn a_pid_that_never_called_is_told_apart_from_one_that_called_anonymously() {
        let contacts = PeerContacts::new();

        // Past the warm-up first: on a map this new, absence is not yet a
        // finding, and `a_cold_map_does_not_claim_a_pid_never_called` is the
        // case that pins that. Here the subject is the None/Anonymous/Named
        // distinction, so the clock is moved out of the way.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;

        assert_eq!(
            contacts.from_pid(Some(4242)),
            Contact::None,
            "nothing has connected from this pid, and that is a finding"
        );
        assert_eq!(
            contacts.from_pid(None),
            Contact::Unknown,
            "no pid to ask about is not the same as a pid nothing came from"
        );

        contacts.connected(4242);
        assert_eq!(
            contacts.from_pid(Some(4242)),
            Contact::Anonymous,
            "a connection that named no dog is exactly the case the operator lost two days to"
        );

        contacts.named_a_dog(4242);
        assert_eq!(contacts.from_pid(Some(4242)), Contact::Named);
    }

    /// A successor's map starts empty at every `execve`, so for its first
    /// few seconds every dog carried across the handover is absent from it.
    /// Reading that absence as "this dog never called" put the reinstall
    /// verdict on a dog that was fine, which is the message this ladder
    /// exists to stop shep guessing. `crate::boot`'s own comment already
    /// claimed the property this pins.
    #[tokio::test(start_paused = true)]
    async fn a_cold_map_does_not_claim_a_pid_never_called() {
        let contacts = PeerContacts::new();

        assert_eq!(
            contacts.from_pid(Some(4242)),
            Contact::Unknown,
            "a map this new was not listening long enough for an absence to mean anything"
        );
        assert_eq!(
            stale_verdict("metrics", Silence::of(Some(4242), &contacts)),
            stale_verdict("metrics", Silence::Unattributed),
            "an unwarmed map must reach the arm that names both candidates"
        );

        // One tick short of the warm-up is still too new.
        tokio::time::advance(PEER_CONTACT_WARMUP - Duration::from_millis(1)).await;
        assert_eq!(contacts.from_pid(Some(4242)), Contact::Unknown);

        // And past it the absence is earned, so the reinstall advice comes
        // back. Deleting a true message would be its own defect.
        tokio::time::advance(Duration::from_millis(2)).await;
        assert_eq!(
            contacts.from_pid(Some(4242)),
            Contact::None,
            "shep was listening for a whole budget past the dog's silence"
        );
        assert!(
            stale_verdict("metrics", Silence::of(Some(4242), &contacts))
                .contains("cannot reach this shep"),
            "the earned reinstall advice must survive"
        );
    }

    /// Fails if a later anonymous connection unsays an earlier named one.
    ///
    /// The question is whether this process has EVER named itself. A dog
    /// that does so once is not the dog this diagnosis is about, and a
    /// reconnect that happened to be read before its `Hello` must not move
    /// it back into the pile.
    #[test]
    fn a_pid_that_has_named_a_dog_goes_on_having_named_one() {
        let contacts = PeerContacts::new();
        contacts.named_a_dog(7);
        contacts.connected(7);
        assert_eq!(contacts.from_pid(Some(7)), Contact::Named);
    }

    /// Fails if a full map forgets a pid that is still calling rather than
    /// the one that stopped.
    ///
    /// The bound exists so this state cannot grow without limit, and the
    /// eviction rule exists so the bound cannot cost the answer: a dog
    /// reconnects, so it is touched, so it survives any amount of churn
    /// from short-lived `shep` invocations.
    #[tokio::test(start_paused = true)]
    async fn a_full_map_forgets_the_pid_that_stopped_calling() {
        let contacts = PeerContacts::new();
        // An evicted entry reads as `None` only once the map is old enough
        // for an absence to be a finding at all. The subject here is
        // eviction, so the warm-up is moved out of the way.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;
        let dog = 1;
        contacts.named_a_dog(dog);

        // Every stranger arrives after the dog's first call, and the dog
        // calls again partway through — which is what a live dog does.
        for pid in 2..=u32::try_from(PEER_CONTACT_CAPACITY).unwrap() {
            contacts.connected(pid);
            if pid % 8 == 0 {
                contacts.connected(dog);
            }
        }
        let stranger = 2;
        for pid in 1_000_000..1_000_100 {
            contacts.connected(pid);
        }

        assert_eq!(
            contacts.from_pid(Some(dog)),
            Contact::Named,
            "a peer that keeps calling must outlive a hundred that called once"
        );
        assert_eq!(
            contacts.from_pid(Some(stranger)),
            Contact::None,
            "the oldest untouched entry is the one the bound spends"
        );
        assert!(
            contacts.lock().by_pid.len() <= PEER_CONTACT_CAPACITY,
            "the map must not grow past its bound"
        );
    }

    /// Fails if the stale verdict asserts a cause this shepherd did not
    /// watch happen.
    ///
    /// The regression this exists for, in full: one message served all three
    /// cases and claimed *the binary on disk cannot talk to this shep
    /// either, so rebuild or reinstall it*. A dog that was connected the
    /// whole time and serving every request was reported that way, and its
    /// operator spent two days reinstalling a binary that reinstalling could
    /// never have fixed.
    ///
    /// So the assertion is not that the wording is nice. It is that the
    /// stale-binary claim appears on the ONE path where nothing was ever
    /// seen to arrive, and that the path it cost two days on says the
    /// opposite out loud.
    #[test]
    fn the_stale_verdict_claims_only_what_this_shepherd_watched() {
        let unreachable = stale_verdict("metrics", Silence::Unreachable { pid: 900 });
        assert!(
            unreachable.contains("nothing has ever connected"),
            "the reinstall advice has to be earned by an observation: {unreachable}"
        );
        assert!(unreachable.contains("pid 900"), "{unreachable}");
        assert!(
            unreachable.contains("rebuild or reinstall it"),
            "a dog that never reached the socket is the case reinstalling does fix: {unreachable}"
        );

        let anonymous = stale_verdict("log-rotate", Silence::Anonymous { pid: 901 });
        assert!(
            !anonymous.contains("cannot reach this shep"),
            "this dog reached shep; claiming otherwise is the whole defect: {anonymous}"
        );
        assert!(
            anonymous.contains("reinstalling the same build will NOT"),
            "the two days were spent on advice this line has to refuse: {anonymous}"
        );
        assert!(
            anonymous.contains("0.1.23"),
            "the fix is a newer shep-client, and the message has to name it: {anonymous}"
        );
        assert!(
            anonymous.contains("`shep restart log-rotate`"),
            "every verdict ends in something the reader can run: {anonymous}"
        );

        let unattributed = stale_verdict("metrics", Silence::Unattributed);
        // The WHOLE command, not the flag on its own. `contains("--force")`
        // would pass on any sentence that happened to mention it, and the
        // point is that an operator can copy what they are shown: a plain
        // `cargo install <crate>` on a dog whose version has not moved prints
        // "already installed", builds nothing, and exits 0, so advice missing
        // this is advice that silently does nothing.
        for verdict in [&unreachable, &anonymous] {
            assert!(
                verdict.contains("`cargo install <crate> --force`"),
                "an actionable verdict must carry the whole forced reinstall command: {verdict}"
            );
        }
        assert!(
            unattributed.contains("could not tell which process"),
            "not knowing has to be said rather than papered over: {unattributed}"
        );
        assert!(
            unattributed.contains("`shep bleats metrics`"),
            "the one command that separates the two candidates: {unattributed}"
        );

        for verdict in [&unreachable, &anonymous, &unattributed] {
            assert!(
                !verdict.contains("the binary on disk cannot talk to this shep either"),
                "the sentence that was asserted on every path is gone: {verdict}"
            );
        }
    }

    /// Fails if evidence is read as anything but what was observed.
    ///
    /// [`Contact::Named`] is the interesting row: a pid that named a dog and
    /// is nonetheless being judged silent is a contradiction (naming one is
    /// what sets `handshook`, and `silent_dogs` filters a handshook dog out),
    /// so the only honest reading is that the attribution cannot be trusted
    /// for this dog.
    #[tokio::test(start_paused = true)]
    async fn evidence_is_read_off_the_record_and_never_guessed() {
        let contacts = PeerContacts::new();
        // `Unreachable` is only ever read off a map that has been watching
        // long enough to claim it; see `a_cold_map_does_not_claim_a_pid_never_called`.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;
        contacts.connected(11);
        contacts.named_a_dog(12);

        assert_eq!(
            Silence::of(Some(10), &contacts),
            Silence::Unreachable { pid: 10 }
        );
        assert_eq!(
            Silence::of(Some(11), &contacts),
            Silence::Anonymous { pid: 11 }
        );
        assert_eq!(
            Silence::of(Some(12), &contacts),
            Silence::Unattributed,
            "a pid that named a dog and is silent anyway is a contradiction, not a diagnosis"
        );
        assert_eq!(
            Silence::of(None, &contacts),
            Silence::Unattributed,
            "no pid is no attribution, which is a different answer from no contact"
        );
    }

    /// Fails if shep's own account of a dog stays in `shepd.err.log`, where
    /// the dog's operator was never told to look.
    ///
    /// Every message in the incident behind this module went to the daemon's
    /// log. The dog's own log — the file shep's error message told the
    /// operator to read — could not hold a word of it, so `shep bleats
    /// log-rotate` showed a dog repeating itself into an empty room and
    /// nothing about why shep had given up on it.
    ///
    /// Both halves are asserted, because they have different failure modes:
    /// the file is what survives to be read afterwards, and the bus is what
    /// a `shep bleats --follow` sees live.
    #[tokio::test]
    async fn shep_s_own_account_of_a_dog_reaches_that_dog_s_log() {
        let h = crate::testing::harness(vec![ProcScript::never_exits()]);
        start_test_dog(&h.ctx, "log-rotate").await;
        let info = h
            .ctx
            .supervisor
            .list()
            .await
            .into_iter()
            .find(|info| info.name == "log-rotate")
            .expect("the dog fixture must be listed");
        let err_log = info
            .err_file
            .clone()
            .expect("a dog's log paths are resolved");

        // A real `log.*` forwarder, the same one a `shep bleats --follow`
        // gets: `Bus::publish_log` skips the whole publish while nothing has
        // registered an interest in log topics, so a plain `subscribe()`
        // would assert that the gate is shut rather than that the narration
        // travels. Registered before the narration, because a broadcast
        // receiver starts at the channel's current tail.
        let (out_tx, mut following) = tokio::sync::mpsc::channel(16);
        let forwarder = crate::bus::spawn_forwarder(
            &h.ctx.events,
            crate::bus::TopicFilter::new(&["log.*".to_string()]).unwrap(),
            out_tx,
        );

        narrate(&h.ctx.events, &info, "shep did a thing worth saying").await;

        let written = std::fs::read_to_string(&err_log).expect("the narration must reach the log");
        let line = written
            .strip_suffix('\n')
            .expect("one whole line, newline included");
        assert!(
            line.ends_with("[shep] shep did a thing worth saying"),
            "the line must be marked as shep's voice, not the dog's: {line:?}"
        );
        let (stamp, rest) = line.split_at(shep_core::logstamp::LOG_STAMP_BYTES);
        assert_eq!(
            rest, "[shep] shep did a thing worth saying",
            "the stamp is the same fixed-width prefix every other line carries: {line:?}"
        );
        chrono::DateTime::parse_from_rfc3339(stamp.trim_end())
            .unwrap_or_else(|err| panic!("{stamp:?} must parse as RFC 3339: {err}"));

        let frame = tokio::time::timeout(Duration::from_secs(5), following.recv())
            .await
            .expect("a follower must be told inside the budget")
            .expect("the forwarder must deliver rather than end");
        match shep_core::protocol::decode_frame::<BusEvent>(&frame).unwrap() {
            BusEvent::LogErr { id, line } => {
                assert_eq!(id, info.id, "the line belongs to the dog it is about");
                assert_eq!(
                    line, "[shep] shep did a thing worth saying",
                    "a follower sees the marker and not the file's stamp"
                );
            }
            other => panic!("narration must reach a follower as a log line, got {other:?}"),
        }
        forwarder.abort();
    }
}
