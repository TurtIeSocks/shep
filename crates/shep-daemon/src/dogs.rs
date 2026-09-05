//! The dog contract: what a dog is spawned as, and how it is served its own
//! configuration
//!
//! A dog is an ordinary supervised process that speaks the control protocol.
//! [`dog_app`] assembles the same [`ResolvedApp`] a Flockfile entry would, and
//! the supervisor supervises it as a sheep. Both [`DogSpec::source`] kinds run
//! at the daemon's own trust level.
//!
//! Configuration travels over the socket through [`dog_section`]: a dog
//! inherits `$SHEP_HOME` and `$SHEP_DOG_NAME`, then asks for its `[<name>]`
//! section of `dogs.toml`. No section value reaches the child's environment,
//! which is readable from the process table and inherited by every child.

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
/// `Debug` needs no redaction: a path, a normalizer complaint, or a TOML
/// parser message, never a value read out of a parsed `[<name>]` table. A
/// syntax error can quote a line of the section's own source, but only to
/// the peer that asked, which peer-cred auth already established owns the
/// file.
///
/// [`Self::NoBinary`] and [`Self::Io`] wrap their [`std::io::Error`] rather
/// than rendering it, which costs this enum `Clone`, `PartialEq` and `Eq`.
#[non_exhaustive]
#[derive(Debug)]
pub enum DogError {
    /// A built-in dog has no program it can be spawned with: either
    /// [`std::env::current_exe`] failed, or handover-target resolution refused
    /// every candidate.
    NoBinary(std::io::Error),
    /// The dog's binary comes from a source this build cannot spawn (carries
    /// the source as `Debug` renders it). [`DogSource`] is `#[non_exhaustive]`,
    /// so a name enabled by a newer shep can reach an older daemon.
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

/// The program a built-in dog is spawned as: this binary's own resolved path.
///
/// Through `handover::exec_target` rather than [`std::env::current_exe`],
/// which on Linux answers `"<path> (deleted)"` once a package manager has
/// replaced the running binary, and that cannot be exec'd. A respawned dog
/// can therefore run newer code than the shepherd currently has loaded, if
/// the binary was replaced before this shepherd reloaded or handed over.
#[cfg(unix)]
fn builtin_program() -> Result<PathBuf, DogError> {
    crate::handover::exec_target().map_err(DogError::NoBinary)
}

/// The program a built-in dog is spawned as, on a platform with no handover.
///
/// Windows refuses to replace a running executable, so no unlinked inode can
/// exist for `current_exe` to name.
#[cfg(windows)]
fn builtin_program() -> Result<PathBuf, DogError> {
    std::env::current_exe().map_err(DogError::NoBinary)
}

/// The app config the daemon spawns `spec` from.
///
/// A built-in dog is `<this binary> dog <name>`; an adopted one is the
/// operator's binary with no arguments. The environment carries exactly
/// `SHEP_HOME` and `SHEP_DOG_NAME`, never a `[<name>]` value: a dog asks for
/// its section over the socket.
///
/// # Errors
/// - [`DogError::NoBinary`] if a built-in dog has no program to run.
/// - [`DogError::UnsupportedSource`] if the source is a kind this build does
///   not know how to spawn.
/// - [`DogError::Config`] if the assembled config failed `normalize`.
pub fn dog_app(spec: &DogSpec, paths: &ShepPaths) -> Result<ResolvedApp, DogError> {
    let (script, args) = match &spec.source {
        DogSource::BuiltIn => (
            builtin_program()?.display().to_string(),
            vec!["dog".to_string(), spec.name.clone()],
        ),
        // No arguments: an adopted dog is somebody else's binary, and an argv
        // shep invented for it is one more thing it has to agree with.
        DogSource::Adopted { path } => (path.clone(), Vec::new()),
        source => return Err(DogError::UnsupportedSource(format!("{source:?}"))),
    };

    let mut config = AppConfig::minimal(&spec.name, &script);
    config.args = args;
    config
        .env
        .insert("SHEP_HOME".to_string(), paths.home.display().to_string());
    // The `[<name>]` key this dog's section lives beneath, and so the `name`
    // it puts in `Request::DogConfig`. An adopted dog has no argv to read it
    // from, so this is its only channel.
    config
        .env
        .insert("SHEP_DOG_NAME".to_string(), spec.name.clone());
    normalize(config).map_err(|err| DogError::Config(err.to_string()))
}

/// Starts every dog in `specs`, warning and carrying on for each one that
/// will not start.
///
/// Never fails the boot: a dog that cannot be spawned is a monitoring gap, and
/// refusing to bring the flock up over it turns that gap into an outage.
/// [`SupervisorHandle::start_dog`] is idempotent by name, so an `Ok` reply
/// carrying no `dog` means a sheep already held the name and nothing started.
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
        // Read before `start_dog` takes the app: this is the one place that
        // knows which file the spawn resolved to.
        let script = app.config().script.clone();
        match supervisor.start_dog(app, spec.source.clone()).await {
            Ok(info) if info.dog.is_none() => tracing::warn!(
                dog = %spec.name,
                "a sheep is already registered under this name; the dog did not start"
            ),
            Ok(info) => {
                // `start_dog` is idempotent by name, so this reply may be a
                // dog that was already running: the wording is about the
                // binary this shepherd resolved, not about a spawn.
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

/// The `[<name>]` section of `path`, a `dogs.toml`, as the operator wrote
/// it: the table's own text, without its header.
///
/// Reads the file on every call, so one reader can never be stale and
/// `shep disable X && shep enable X` re-reads an edited section. A missing
/// file, or one with no such section, is `Ok(String::new())`.
///
/// The span, not a re-render: rendering a parsed table drops the comments
/// inside a section and sorts its keys. [`set_dog_section`] writes back the
/// text its caller hands it.
///
/// # Errors
/// - [`DogError::Config`] if the file exists and is not valid `dogs.toml`, or
///   its section will not render back to TOML.
/// - [`DogError::Io`] if the file exists and could not be read.
pub fn dog_section(path: &Path, name: &str) -> Result<String, DogError> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(DogError::Io(err)),
    };
    // Through shep-core's own type, so a broken `dogs.toml` is one named error
    // and not a second parser's opinion of the same file.
    let config =
        DogsConfig::load(Some(&source)).map_err(|err| DogError::Config(err.to_string()))?;
    let Some(table) = config.dog.get(name) else {
        return Ok(String::new());
    };
    // Cannot fail: the stricter parse above already read the same bytes.
    let doc: toml_edit::DocumentMut = source
        .parse()
        .map_err(|err: toml_edit::TomlError| DogError::Config(err.to_string()))?;
    match doc.get(name) {
        Some(toml_edit::Item::Table(spanned)) => Ok(spanned.to_string()),
        // An inline table, `bark = { poll = "60s" }`, is a valid entry
        // whose span is `{ ... }`, which is not a section body and is not
        // something the pane could write back. Rendered, as every section
        // was before: there is no comment to lose inside one line.
        _ => toml::to_string(table).map_err(|err| DogError::Config(err.to_string())),
    }
}

/// Replaces `name`'s table in `path`, a `dogs.toml`, with `section` and
/// writes the file back owner-only, under the same lock the CLI's two
/// writers of that file hold.
///
/// `dogs.toml` is hand-editable on purpose ([`DogsConfig`]'s own doc calls
/// it deliberately not a locked shep-owned store), so this reads, modifies
/// and writes the document it read rather than rendering a parsed map back
/// out: every table other than `name`'s comes through byte for byte, and so
/// does a comment outside it. A comment INSIDE the replaced table is the
/// caller's to carry, because the caller is what decided the section's new
/// text, and [`dog_section`] hands it the span rather than a re-render
/// precisely so that it can. The header's own decor, a comment line above
/// `[name]` and anything trailing the header, is carried across here
/// instead: it sits neither inside the section nor outside the table, so
/// neither half would otherwise keep it.
///
/// The rendered result is handed to [`DogsConfig::load`] before anything
/// reaches disk, so a section this daemon could not serve back never lands.
/// That gate is the same one `shep rehome`'s writer takes, and it is the
/// stricter of the two parses: a stray top-level scalar is a valid document
/// and not a valid `DogsConfig`.
///
/// The write itself is the three steps `shep-cli`'s `write_dogs_config`
/// takes, for its reasons: staged in a sibling file created at
/// [`shep_core::atomic_file::OWNER_ONLY_FILE_MODE`] (this is where an
/// operator is told to paste a webhook URL, which is a bearer token in a
/// path), `fsync`ed, then `rename`d over `path` so a crash leaves the whole
/// file or none of it.
///
/// Whether `name` is a dog at all is the CALLER's question, not this
/// function's: the answer lives in the supervisor, and `rpc::dispatch` asks
/// it before calling here.
///
/// # Errors
/// - [`DogError::Io`]: the lock, the read, the staging file, the `fsync` or
///   the rename.
/// - [`DogError::Config`]: `section` is not valid TOML, `path` is not valid
///   `dogs.toml`, or the spliced result would not load. Nothing has been
///   written in any of the three.
pub fn set_dog_section(path: &Path, name: &str, section: &str) -> Result<(), DogError> {
    use std::io::Write as _;

    use toml_edit::{DocumentMut, Item};

    // Held across the read, the splice and the rename, and dropped on the
    // way out. Two writers that read before either wrote would lose one of
    // the two sections whichever way the renames raced; `forget_dog_section`
    // takes the same lock on the same path for the same reason. This
    // function takes no other lock, so it can never be the half of a
    // deadlock that holds `dogs.toml` and waits on `shep.toml`.
    let _lock = shep_core::config_lock::ConfigLock::acquire(path).map_err(DogError::Io)?;

    // A missing file is the ordinary first write: a home that has never had
    // a dog configured has no `dogs.toml` at all.
    let existing = match std::fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(DogError::Io(err)),
    };
    let mut doc: DocumentMut = existing.parse().map_err(|err: toml_edit::TomlError| {
        DogError::Config(format!("dogs.toml does not parse: {err}"))
    })?;
    let incoming: DocumentMut = section.parse().map_err(|err: toml_edit::TomlError| {
        DogError::Config(format!("[{name}] does not parse: {err}"))
    })?;

    // `set_implicit(false)`, or a section emptied down to nothing takes the
    // dog's table with it. A document's root table is implicit, and an
    // implicit table with no keys of its own renders no header at all. A
    // section that still holds a key renders `[name]` either way, so this
    // line shows up only in the emptied case -- which is where an operator
    // arrives by deleting the last key in the pane.
    let mut table = incoming.as_table().clone();
    table.set_implicit(false);
    // A comment above `[name]`, and anything trailing the header itself,
    // are decor on the TABLE rather than text inside it -- `dog_section`
    // hands the caller the body and cannot carry them, and replacing the
    // item wholesale would drop both. Copied across so an operator's note
    // about what a dog is for survives a pane write, as the notes between
    // its keys already do.
    if let Some(Item::Table(existing)) = doc.get(name) {
        *table.decor_mut() = existing.decor().clone();
    }
    doc[name] = Item::Table(table);

    let rendered = doc.to_string();
    DogsConfig::load(Some(&rendered))
        .map_err(|err| DogError::Config(format!("[{name}] would not load: {err}")))?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = shep_core::config_lock::create_config_file(parent).map_err(DogError::Io)?;
    tmp.write_all(rendered.as_bytes()).map_err(DogError::Io)?;
    tmp.as_file().sync_all().map_err(DogError::Io)?;
    // `persist` is `rename(2)`. On failure the staging file comes back
    // inside the error and its `Drop` removes it, so a failed replace
    // leaves nothing behind in `$SHEP_HOME`.
    tmp.persist(path).map_err(|err| DogError::Io(err.error))?;
    // `sync_all` made the CONTENTS durable; this makes the rename that
    // published them durable. A no-op on Windows.
    shep_core::atomic_file::sync_dir(parent).map_err(DogError::Io)?;
    Ok(())
}

/// What a refused handshake costs the dog that sent it.
///
/// Derived from how many times that dog has been refused since it last
/// handshook, by [`DogRefusals::refused`] and nothing else.
///
/// `#[non_exhaustive]`: a fourth verdict would otherwise be a breaking change
/// for an out-of-tree matcher.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The first refusal since this dog last handshook. Restart it once from
    /// disk: an upgrade that replaced the file usually leaves the disk binary
    /// already correct.
    Restart,
    /// The second: the restarted dog speaks the same protocol, which proves
    /// the binary on disk cannot satisfy this daemon either. Report it stale
    /// and stop.
    Stale,
    /// Already stale, already reported. Say nothing further: a stale dog its
    /// own `autorestart` keeps respawning would write one line per respawn.
    AlreadyStale,
}

/// Which dogs this daemon has refused at the handshake, and how often since
/// each last got in.
///
/// Cheap to clone (one `Arc`), and shared by every connection through
/// [`RpcContext`](crate::rpc::RpcContext).
///
/// A count is cleared by a successful handshake and by nothing else, which
/// bounds the ladder: a dog that keeps being refused never clears, so it never
/// earns a second restart. Nothing survives a handover, and a dog a successor
/// can talk to is not stale.
#[derive(Debug, Clone, Default)]
pub struct DogRefusals {
    /// Both halves under one lock: a dog seen as refused and handshook at once
    /// is a state no reader should be able to observe.
    seen: Arc<Mutex<Links>>,
}

/// What [`DogRefusals`] holds: how often each dog has been refused, and
/// which dogs have ever got in.
///
/// Only ever reached under [`DogRefusals`]'s one lock.
#[derive(Debug, Default)]
struct Links {
    /// Refusals per dog name since that dog last handshook. A name absent
    /// from the map has not been refused since it last got in.
    refusals: BTreeMap<String, u32>,
    /// Dogs whose handshake this daemon has accepted and not refused since.
    ///
    /// Not derivable from the absence of a refusal: a dog that has never
    /// connected and one that is talking happily both have no entry in
    /// [`Self::refusals`].
    handshook: BTreeSet<String>,
}

impl DogRefusals {
    /// Builds an empty record: a daemon that has refused nobody.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one refused handshake from the dog named `name`, and says
    /// what the daemon should do about it.
    ///
    /// The first refusal earns [`Refusal::Restart`], the second
    /// [`Refusal::Stale`], every one after that [`Refusal::AlreadyStale`].
    pub fn refused(&self, name: &str) -> Refusal {
        let mut seen = self.lock();
        // The connection that earned the mark is gone, and the process behind
        // the name may not be the one that made it.
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
    /// Answers whether that changed anything, so the caller can write into the
    /// dog's own log the first time this shepherd hears from it and not once
    /// per reconnect.
    pub fn handshook(&self, name: &str) -> bool {
        let mut seen = self.lock();
        seen.refusals.remove(name);
        seen.handshook.insert(name.to_string())
    }

    /// Whether `name` has handshook with this daemon and not been refused
    /// since.
    #[must_use]
    pub fn has_handshook(&self, name: &str) -> bool {
        self.lock().handshook.contains(name)
    }

    /// Every dog whose one restart from disk is in flight, sorted.
    ///
    /// Exactly the dogs refused once: the restart they are owed has been asked
    /// for and its outcome has not arrived.
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
    /// A dog is stale once it has been refused twice, which means its one
    /// restart from disk did not help.
    #[must_use]
    pub fn stale(&self) -> Vec<String> {
        self.lock()
            .refusals
            .iter()
            .filter(|(_, count)| **count >= 2)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The record, treating a poisoned lock as ordinary data: every critical
    /// section here is a lookup or an increment, so a panic elsewhere cannot
    /// leave a torn value.
    fn lock(&self) -> std::sync::MutexGuard<'_, Links> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How many distinct peer pids [`PeerContacts`] remembers at once.
///
/// What has to survive eviction is a handful of long-lived dog processes,
/// which reconnect and so refresh their own entries, against whatever else
/// dialled the socket recently. A thousand distinct pids inside one
/// [`DOG_SILENCE_BUDGET`] would take about two hundred `shep` invocations a
/// second, and the degradation is `record_silent_dog`'s unattributed arm.
const PEER_CONTACT_CAPACITY: usize = 1024;

/// How long this map must have been watching before a pid's absence from it
/// means anything.
///
/// A successor built by [`crate::boot`] starts empty at every `execve`, so
/// without this every dog carried across a `shep daemon reload` would look,
/// for its first seconds, like a dog that never called. The stale rung is
/// spent once, so a verdict against a cold map would be the last one.
///
/// While the map warms, `from_pid` answers [`Contact::Unknown`], which routes
/// to `Silence::Unattributed`.
const PEER_CONTACT_WARMUP: Duration = DOG_SILENCE_BUDGET;

/// What this daemon has observed arriving on its socket, keyed by the
/// connecting process's pid.
///
/// One question, asked by `record_silent_dog`: when a dog has been running
/// without ever handshaking, is it failing to reach this daemon, or reaching
/// it and not saying who it is? Those have opposite fixes. A pid is the
/// identifier both sides already have, so nothing is added to a protocol the
/// dogs being diagnosed are too old to speak.
///
/// Unix only in practice: Windows has no post-accept peer check, so this map
/// stays empty there and every lookup answers [`Contact::Unknown`].
#[derive(Debug, Clone, Default)]
pub struct PeerContacts {
    seen: Arc<Mutex<Contacts>>,
}

/// What [`PeerContacts`] holds, under its one lock.
#[derive(Debug)]
struct Contacts {
    /// When this map started watching, which is this daemon's own boot.
    ///
    /// [`tokio::time::Instant`], so a paused test moves the clock instead of
    /// sleeping out a budget. Under the lock so a test that drives a real
    /// socket can back-date it through `&self`.
    watching_since: Instant,
    /// One entry per remembered peer pid, at most
    /// [`PEER_CONTACT_CAPACITY`] of them.
    by_pid: BTreeMap<u32, Seen>,
    /// Ticks once per recorded connection, and is what
    /// [`Contacts::evict_oldest`] compares.
    ///
    /// A counter rather than an `Instant`: the only question asked of it is
    /// which of two entries was touched later.
    clock: u64,
}

/// What has been seen from one peer pid.
#[derive(Debug)]
struct Seen {
    /// Whether any connection from this pid carried a `Hello.dog_name`.
    ///
    /// Recorded whatever the handshake's verdict was: a dog refused on
    /// protocol skew still named itself.
    named_a_dog: bool,
    /// [`Contacts::clock`] as of the most recent connection from this pid.
    touched: u64,
}

/// What [`PeerContacts`] has seen from one pid.
///
/// `#[non_exhaustive]`: a fourth answer would otherwise be a breaking change
/// for an out-of-tree matcher.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contact {
    /// Nothing has ever connected from this pid.
    ///
    /// A dog running as this pid is not reaching the socket at all, the one
    /// case where reinstalling the binary is the right advice.
    None,
    /// Connections have arrived from this pid, and not one of them named a
    /// dog in its `Hello`.
    ///
    /// The dog is reaching this daemon and may be serving every request it is
    /// asked. It is built against shep-client older than 0.1.23, or it connects
    /// with `Client::connect` rather than
    /// `ReconnectingClient::connect_as_dog`.
    Anonymous,
    /// A connection from this pid named a dog in its `Hello`.
    Named,
    /// There is nothing recorded either way: no pid was available, or this
    /// pid's entry has been evicted.
    ///
    /// Distinct from [`Self::None`]: "nothing has connected" is a finding, and
    /// "I could not look" is not.
    Unknown,
}

impl PeerContacts {
    /// Builds an empty record: a daemon nothing has connected to yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one connection arriving from `pid`.
    ///
    /// Called before a byte is read, so a peer that connects and says nothing
    /// still counts as having reached this daemon.
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
    /// Sticky: the question is whether this process has ever named itself, so
    /// a later anonymous connection from the same pid does not unsay it.
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
    /// true.
    #[must_use]
    pub fn is_warming(&self) -> bool {
        self.lock().watching_since.elapsed() < PEER_CONTACT_WARMUP
    }

    /// Back-dates the watching clock so this map reads as warm.
    ///
    /// For the cases that drive a real socket and so cannot pause their
    /// clock.
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
            // enough for it to be one.
            None if seen.watching_since.elapsed() < PEER_CONTACT_WARMUP => Contact::Unknown,
            None => Contact::None,
            Some(seen) if seen.named_a_dog => Contact::Named,
            Some(_) => Contact::Anonymous,
        }
    }

    /// Takes the lock, treating a poisoned one as ordinary data: every
    /// critical section here is a lookup or an increment on a plain
    /// `BTreeMap`, so a panic elsewhere cannot leave a torn value.
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
    /// overflows a full map.
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

/// Records a dog's refused handshake and acts on it.
///
/// The first refusal earns one restart from the binary on disk, enough where a
/// package replaced the file and the running process is merely old. A second
/// proves the disk binary cannot satisfy this daemon either, so the dog is
/// reported stale and left alone. A dog that cannot get in never clears its
/// count, so it cannot earn a second restart.
///
/// The restart runs on its own task: it is a full kill ladder, and the caller
/// is a connection handler holding a socket this daemon has already refused.
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
/// mid-ladder takes the dog off the ladder. An exact-name selector is the only
/// kind that reaches a dog: the supervisor keeps dogs out of `all` and out of
/// pattern matches.
async fn restart_refused_dog(supervisor: &SupervisorHandle, name: &str) {
    match supervisor
        .restart_automatic(ProcessSelector::Name(name.to_string()))
        .await
    {
        Ok(_) => tracing::info!(dog = %name, "restarted a refused dog from the binary on disk"),
        // Not an error the daemon can act on: the dog may have been disabled
        // between the refusal and this restart, or the engine may be shutting
        // down.
        Err(err) => tracing::warn!(dog = %name, %err, "a refused dog could not be restarted"),
    }
}

/// How long a registered, running dog may stay silent before this shepherd
/// concludes it is never going to talk to it.
///
/// A handshake is one connect and one round trip on a local socket. Five
/// seconds is sized against the slowest legitimate silence: a dog carried
/// across a handover has to notice its connection died and dial back, and a
/// third-party dog is free to sleep a second first.
///
/// Not `shep daemon reload`'s three-second settle wait, which lives in
/// `shep-cli` and answers how long a command holds its output open.
pub const DOG_SILENCE_BUDGET: Duration = Duration::from_secs(5);

/// Gap between two of [`spawn_silent_dog_watch`]'s looks.
///
/// Finer than [`DOG_SILENCE_BUDGET`] so a dog's restart is asked for near the
/// moment its budget runs out. One look is one message to the supervisor actor
/// and no syscall per dog.
const DOG_SILENCE_POLL: Duration = Duration::from_secs(1);

/// Every dog the supervisor is running that has never once handshaken with
/// this daemon, sorted.
///
/// [`spawn_silent_dog_watch`] and `rpc::dog_staleness` both read it and must
/// not disagree about the population: a dog in one set but not the other would
/// be reported forever or condemned unreported. Only a dog with a process
/// counts, and a stale one is already answered for.
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

/// When each currently-silent dog was first seen silent.
///
/// Why that watch is a task on a clock rather than a branch inside
/// `rpc::dog_staleness`: staleness is a query, and `shep daemon reload` polls
/// it in a loop, so a ladder driven from there would walk a merely slow dog
/// from restart to stale in the time it takes to ask three times.
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
    /// `now` is a parameter so every dog in one look is judged against the
    /// same instant, and so a test can move the clock.
    fn due(&mut self, silent: &[String], now: Instant) -> Vec<String> {
        // A dog that answered, stopped, or was deleted is not silent any more,
        // and starts a fresh budget if it falls quiet again.
        self.first_seen.retain(|name, _| silent.contains(name));
        let mut due = Vec::new();
        for name in silent {
            let since = self.first_seen.entry(name.clone()).or_insert(now);
            if now.saturating_duration_since(*since) >= DOG_SILENCE_BUDGET {
                // Rearmed rather than forgotten: the next rung costs another
                // whole budget.
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
/// Returns what it acted on; the loop that calls it discards the answer.
pub(crate) async fn check_silent_dogs(
    supervisor: &SupervisorHandle,
    refusals: &DogRefusals,
    contacts: &PeerContacts,
    events: &Bus,
    seen: &mut SilentDogs,
    now: Instant,
) -> Vec<(String, Refusal)> {
    // Nothing is judged while attribution is still maturing: the stale rung is
    // spent once, so a wrong answer here is the last answer.
    if contacts.is_warming() {
        return Vec::new();
    }
    // `seen` is left untouched rather than cleared: a look that could not
    // judge has learned nothing, and must not hand every dog a fresh budget.
    let Ok(infos) = supervisor.list_checked().await else {
        return Vec::new();
    };
    let silent = silent_dogs(&infos, refusals);
    let mut acted = Vec::new();
    for name in seen.due(&silent, now) {
        // Off the same listing the silence was judged from, so the pid a
        // message names is the process that was silent.
        let info = infos.iter().find(|info| info.name == name);
        let evidence = Silence::of(info.and_then(|info| info.pid), contacts);
        let verdict = record_silent_dog(&name, info, evidence, refusals, events, supervisor).await;
        acted.push((name, verdict));
    }
    acted
}

/// What this shepherd observed about a silent dog's connections: the
/// difference between two silences that look identical in a listing and have
/// opposite fixes.
///
/// Built from two facts and no inference: the pid the supervisor spawned the
/// dog as, and what [`PeerContacts`] has seen arrive from that pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Silence {
    /// Nothing has ever connected from the dog's pid. The dog is not
    /// reaching this shepherd's socket at all.
    Unreachable {
        /// The pid nothing has arrived from.
        pid: u32,
    },
    /// Connections have arrived from the dog's pid, and not one of them named
    /// a dog. The dog reaches this shepherd and may be serving every request it
    /// is asked; what it does not do is say who it is.
    Anonymous {
        /// The pid those connections came from.
        pid: u32,
    },
    /// There is no pid to attribute by, so neither of the above can be ruled
    /// in or out: Windows, an OS that declines to name a peer's pid, a process
    /// already gone, or an entry aged out of a full [`PeerContacts`].
    Unattributed,
}

impl Silence {
    /// What `pid`'s connection history says, if anything.
    ///
    /// [`Contact::Named`] lands in [`Self::Unattributed`]: naming a dog sets
    /// `handshook`, and [`silent_dogs`] filters a handshook dog out before it
    /// can be seen quiet, so the only ways to reach it are pid reuse and a race
    /// with an eviction. Neither is attribution to trust.
    fn of(pid: Option<u32>, contacts: &PeerContacts) -> Self {
        match (pid, contacts.from_pid(pid)) {
            (Some(pid), Contact::None) => Self::Unreachable { pid },
            (Some(pid), Contact::Anonymous) => Self::Anonymous { pid },
            _ => Self::Unattributed,
        }
    }
}

/// Enters a dog that has gone quiet into the same ladder a named refusal
/// enters, reached by inference rather than by the dog saying who it is.
///
/// `record_refused_dog` is keyed on `Hello::dog_name`, which a client speaking
/// an older protocol cannot send, so its ladder reaches only dogs new enough to
/// name themselves. The set difference this rides on needs no cooperation from
/// the client; peer credentials are read only to fill in `evidence`.
///
/// A dog that is merely slow to connect is restarted once for nothing, and
/// heals itself: [`DogRefusals::handshook`] clears everything held against a
/// dog the moment it handshakes.
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
            // Awaited rather than spawned: this keeps the next look from
            // running while a kill ladder is in flight, so a dog is never
            // judged mid-restart.
            restart_refused_dog(supervisor, name).await;
        }
        Refusal::Stale => {
            let verdict = stale_verdict(name, evidence);
            tracing::error!(dog = %name, "{verdict}");
            // Into the dog's own log as well, because that is the file the
            // verdict tells the operator to read.
            if let Some(info) = info {
                narrate(events, info, &verdict).await;
            }
        }
        // Unreachable here: `silent_dogs` filters a stale dog out before it
        // can be seen quiet again. A real arm, so a caller that stops filtering
        // does not find a `todo!`.
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
/// operator has nothing to decide yet.
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

/// The stale verdict, written from what this shepherd observed.
///
/// The claim that the binary on disk cannot talk to this shep either belongs
/// on exactly one path, the one where this shepherd watched nothing arrive:
/// asserting it about a connected but anonymous dog sends an operator to
/// reinstall a binary that reinstalling cannot fix. Every arm ends in a
/// command, since the reader is an operator mid-incident.
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
/// shepherd, and enters each into the ladder after [`DOG_SILENCE_BUDGET`] of
/// silence: restarted once from the binary on disk, then reported stale,
/// then left alone.
///
/// Anchored to the daemon's boot rather than to a dog's spawn: a handover is
/// an `execve`, so a per-dog timer would die at the exec, and `boot` runs again
/// in the successor.
///
/// Its `JoinHandle` is held by the caller and aborted at teardown: the loop has
/// no end of its own, and nothing may restart a dog during shutdown.
pub fn spawn_silent_dog_watch(
    supervisor: SupervisorHandle,
    refusals: DogRefusals,
    contacts: PeerContacts,
    events: Bus,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(DOG_SILENCE_POLL);
        // A look missed under load is not a look owed: the budget runs off the
        // clock, not off a tick count.
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
/// A dog's log is the dog's voice and shep writes into that file too, so the
/// file has to say which lines are whose. Short and bracketed because it sits
/// behind a 30-character timestamp.
const SHEP_VOICE: &str = "[shep]";

/// Writes one line of shep's own narration into `info`'s log, and publishes
/// it to whoever is following that log live.
///
/// The file is written directly rather than through the pump, which ends when
/// its sheep's streams reach EOF, before there is anything to say about how the
/// dog exited. Safe because [`open_append`] opens with `O_APPEND`: every write
/// seeks to end atomically, so the whole line is assembled and written in one
/// call. The cost is ordering, since a narration line can land ahead of dog
/// output still in the pump's buffer, bounded by `IDLE_FLUSH`.
///
/// A dog with no `err_file` still reaches a live follower.
pub(crate) async fn narrate(events: &Bus, info: &ProcessInfo, message: &str) {
    let line = format!("{SHEP_VOICE} {message}");
    if let Some(path) = &info.err_file {
        let mut written = String::with_capacity(line.len() + 32);
        shep_core::logstamp::stamp_into(&mut written);
        written.push_str(&line);
        written.push('\n');
        // A failed open is already logged by `open_append`; a failed write is
        // not. Neither is propagated: a log shep cannot write to must not
        // change what shep does about the dog.
        if let Ok(mut file) = crate::tokio_runner::open_append(Path::new(path)).await {
            use tokio::io::AsyncWriteExt as _;
            // Taken after the open: the pump waits on this lock for every line
            // it writes, so holding it across a filesystem open would stall a
            // sheep's output. Held across the write and the flush together.
            let _record = crate::tokio_runner::record_lock(Path::new(path))
                .lock_owned()
                .await;
            // `tokio::fs::File` hands the real `write(2)` to the blocking pool
            // and does not flush on drop, so `write_all` returning means the
            // bytes were accepted rather than written.
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

/// `narrate`, for a caller that knows a dog's name and not its listing.
///
/// Spawned rather than awaited: both callers are connection handlers
/// mid-handshake, and neither may be held up by a listing round trip and a
/// file open. A name that does not resolve to a dog is silently nothing.
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
/// A signal number rather than a name, the rule
/// [`ExitInfo::signal`](shep_core::protocol::ExitInfo::signal) states for
/// itself: a dog's log is read next to `journalctl`.
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
        // Reachable rather than defensive: `last_exit` is `None` when the peer
        // that built this listing predates the field.
        None => "this dog's process stopped, and this shepherd has no record of how".to_string(),
    }
}

/// Watches the bus and records, locally, every enabled dog that exhausts its
/// restart budget, and writes each dog's spawn and exit into its own log.
///
/// The shepherd cannot deliver an alert about a dead bark dog: it has no sinks
/// and no webhook code, so what it guarantees is a local trail in `shep barks`.
/// Read from the bus rather than from the call sites: a `Start` on the bus is a
/// spawn that really happened, while `start_dog` answering `Ok` covers its
/// idempotent no-op too.
///
/// Its `JoinHandle` is held by the caller and aborted at teardown: the task
/// parks on a broadcast receiver.
pub fn spawn_dog_watch(
    mut events: broadcast::Receiver<SharedEvent>,
    publish: Bus,
    barks: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                // Only a dog's `Errored` earns a bark record: bark writes the
                // sheep ones itself, and one event with two authors in one file
                // is a history nobody can trust. `Exit` fires on every restart
                // a dog survives, so it stays out of the barks file.
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
                // The bus drops events for a lagging subscriber, so a dog's
                // death notice may be among what this receiver just lost.
                // Metrics' `shep_dog_up` is the intended answer.
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
/// `sinks` is left empty, which is how a [`Bark`] says the shepherd has no
/// webhook code of its own. [`dog_app`] never overrides `max_restarts`, so
/// `AppConfig::default().max_restarts` is the exhausted budget for every dog.
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

    /// `current_exe` cannot safely be made to return `" (deleted)"`, so this
    /// drives `crate::handover::resolve_target`, which `builtin_program`
    /// delegates to. Unix only, because `handover` is.
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

    /// Asserted over the assembled spec rather than the config, because
    /// `assemble` is where an env map would be merged. `SHEP_DOG_NAME` is no
    /// exception to the rule: it is the key a dog needs to ask for its section
    /// at all.
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

    /// An adopted dog is given no argv, so the environment is its only
    /// channel, and a mismatch looks exactly like a dog with no configuration.
    /// The name is the one the operator chose, not the binary's file stem.
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
        // Round-trips as TOML, the contract the dog parses under.
        let parsed: toml::Table = toml::from_str(&bark).unwrap();
        assert_eq!(parsed["debounce"].as_str(), Some("30s"));

        assert_eq!(dog_section(&path, "absent").unwrap(), "");
        assert_eq!(
            dog_section(&dir.path().join("gone.toml"), "bark").unwrap(),
            ""
        );
    }

    #[test]
    fn a_section_reaches_the_wire_exactly_as_it_did_from_shep_toml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dogs.toml");
        std::fs::write(&path, "[bark]\ndebounce = \"30s\"\n").expect("write");

        // Pinned as a string: the dog-facing contract is the exact text.
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

    /// fails if the write regenerates the file from a parsed map instead of
    /// editing it in place: a comment above another dog's table, or that
    /// dog's own keys, would not survive. `dogs.toml` is hand-editable on
    /// purpose, so an operator's file coming back rewritten is the bug.
    #[test]
    fn set_dog_section_replaces_one_table_and_leaves_the_rest_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");
        std::fs::write(
            &path,
            "# top comment\n[metrics]\nbind = \"127.0.0.1:9100\"\n\n[bark]\npoll = \"60s\"\n",
        )
        .unwrap();

        set_dog_section(&path, "bark", "poll = \"30s\"\nhistory_bytes = 4096\n").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# top comment"), "{text}");
        assert!(text.contains("bind = \"127.0.0.1:9100\""), "{text}");
        assert!(text.contains("poll = \"30s\""), "{text}");
        assert!(!text.contains("poll = \"60s\""), "{text}");
        let parsed = DogsConfig::load(Some(&text)).unwrap();
        assert_eq!(parsed.dog["bark"]["history_bytes"].as_integer(), Some(4096));
    }

    /// fails if the READ renders a parsed map back out instead of handing
    /// back the text an operator wrote. The sibling test above pins a
    /// comment OUTSIDE the edited table, which the write side preserves on
    /// its own; a comment INSIDE it, and the order of the keys around it,
    /// survive only if the section the pane was handed was the raw span.
    /// `toml::map::Map` is a `BTreeMap` without `preserve_order`, so a
    /// re-render alphabetises as well as stripping. The comment ABOVE the
    /// header is the third case, decor on the table itself, which only the
    /// write side can carry.
    #[test]
    fn a_pane_round_trip_keeps_the_comments_and_the_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");
        std::fs::write(
            &path,
            "# what bark is for\n[bark]\n# why bark polls slowly\npoll = \"60s\"\nzz_last = 1\naa_first = 2\n",
        )
        .unwrap();

        let section = dog_section(&path, "bark").unwrap();
        assert!(section.contains("# why bark polls slowly"), "{section}");
        assert!(
            section.find("zz_last") < section.find("aa_first"),
            "the keys come back in the operator\'s order: {section}"
        );

        // What the pane does: write back what it was handed, one value
        // changed.
        set_dog_section(&path, "bark", &section.replace("60s", "30s")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# why bark polls slowly"), "{text}");
        assert!(
            text.contains("# what bark is for"),
            "the header's own comment lives on the table, not inside it: {text}"
        );
        assert!(text.contains("poll = \"30s\""), "{text}");
        assert!(
            text.find("zz_last") < text.find("aa_first"),
            "the write keeps the order the read handed it: {text}"
        );
    }

    /// fails if the writer needs a file to already be there. A home that has
    /// never had a dog configured has no `dogs.toml` at all, which is the
    /// ordinary case for the first section anyone writes.
    #[test]
    fn set_dog_section_creates_the_file_when_there_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");

        set_dog_section(&path, "bark", "poll = \"30s\"\n").unwrap();

        let parsed = DogsConfig::load(Some(&std::fs::read_to_string(&path).unwrap())).unwrap();
        assert!(parsed.dog.contains_key("bark"));
    }

    /// fails if the validation runs after the write, or not at all. A
    /// section this daemon cannot read back must never reach disk: the file
    /// it would land in is the one every dog is served from, so one bad
    /// section would take the rest of the kennel down with it.
    #[test]
    fn set_dog_section_refuses_text_that_is_not_a_table_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");
        std::fs::write(&path, "[bark]\npoll = \"60s\"\n").unwrap();

        let err = set_dog_section(&path, "bark", "this is = = not toml").unwrap_err();

        assert!(err.to_string().contains("bark"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[bark]\npoll = \"60s\"\n"
        );
    }

    /// fails if the only gate is the parse of the incoming text. This
    /// section parses fine on its own and the spliced document is valid
    /// TOML; what it is not is a valid `DogsConfig`, because the file it
    /// lands in has a top-level scalar in it. The daemon serves every dog
    /// from one `DogsConfig::load` of this file, so a write that leaves it
    /// unloadable takes the whole kennel down and not just this dog.
    #[test]
    fn set_dog_section_refuses_a_result_the_daemon_could_not_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");
        std::fs::write(&path, "port = 9100\n[bark]\npoll = \"60s\"\n").unwrap();

        let err = set_dog_section(&path, "bark", "poll = \"30s\"\n").unwrap_err();

        assert!(err.to_string().contains("bark"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "port = 9100\n[bark]\npoll = \"60s\"\n"
        );
    }

    /// fails if the dog's table vanishes when its last key is deleted. A
    /// root table is implicit, and an implicit table with no keys of its
    /// own renders no header at all, so an operator who cleared a section
    /// in the pane would find `[bark]` gone from a file they are invited to
    /// hand-edit -- and `shep describe` with it.
    #[test]
    fn set_dog_section_leaves_the_table_behind_when_the_section_is_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");
        std::fs::write(&path, "[bark]\npoll = \"60s\"\n").unwrap();

        set_dog_section(&path, "bark", "").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[bark]"), "{text}");
        let parsed = DogsConfig::load(Some(&text)).unwrap();
        assert!(parsed.dog["bark"].is_empty(), "{text}");
    }

    /// fails if the file is created at the ambient umask. This is where
    /// `docs/dogs.md` tells an operator to paste a webhook URL, which is a
    /// bearer token in a path; the CLI's own writer creates it `0600` and a
    /// second writer at `0644` would be the downgrade.
    #[cfg(unix)]
    #[test]
    fn set_dog_section_writes_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");

        set_dog_section(&path, "bark", "poll = \"30s\"\n").unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
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

    /// Polls `path` under a real timeout until it holds at least `n` barks:
    /// the watcher writing to it runs as a separate task, so a bare read races
    /// it.
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

    /// Both halves are needed: without the negative assertion, a watcher that
    /// recorded every `Errored` passes.
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
        // A real `Errored` after it proves the watcher was listening at all:
        // without it, a watcher that recorded nothing would pass.
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
        // not each buy another restart.
        for _ in 0..5 {
            assert_eq!(refusals.refused("metrics"), Refusal::AlreadyStale);
        }
        assert_eq!(refusals.stale(), vec!["metrics".to_string()]);
    }

    /// The count is cleared by a successful handshake and by nothing else, so
    /// "one restart" means one per episode rather than one per daemon.
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

    /// The unsettled-dog report is taken once every dog has settled, and a dog
    /// refused once has not: the restart it is owed has been asked for and its
    /// verdict has not come back.
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

    /// A dog that has never connected and one talking happily both have no
    /// refusal recorded, and telling them apart is what the unsettled-dog
    /// report waits on.
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

    /// How long [`start_test_dog`] waits on the supervisor before calling it a
    /// hang rather than a slow start.
    ///
    /// Generous on purpose: a deadlock guard, not a timing assertion.
    const DOG_FIXTURE_START_BUDGET: Duration = Duration::from_secs(10);

    /// How often [`settle_until`] looks while the watch works.
    ///
    /// Finer than [`DOG_SILENCE_POLL`] so a rung is seen inside the poll
    /// period it lands in.
    const SETTLE_STEP: Duration = Duration::from_millis(250);

    /// How long [`settle_until`] gives a rung before it gives up.
    ///
    /// A hang guard, not a timing assertion: a whole warm-up plus both rungs
    /// is fifteen seconds of virtual time. Generosity is free, because the
    /// clock it bounds is virtual.
    const LADDER_BUDGET: Duration = Duration::from_secs(45);

    /// Waits for `settled` to answer true, and answers how much virtual time
    /// that took.
    ///
    /// Sleeping here is a barrier rather than a slower spin: under
    /// `start_paused` the runtime advances the clock only once every task is
    /// idle, and work on the blocking pool holds it there. Each of
    /// `spawn_silent_dog_watch`'s looks writes into the dog's own log, which
    /// `narrate` puts on that pool. A `yield_now` loop keeps a task runnable,
    /// so the runtime never idles and the clock never advances.
    ///
    /// Panics, naming `what`, if `settled` has not answered true within
    /// `within` of virtual time.
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
        // Bounded, because the callers below run under a paused clock and this
        // await is the one thing in them not already forced. `start_paused`
        // auto-advances to the next deadline once every task is idle, so the
        // timeout fires rather than waiting on a wall clock.
        tokio::time::timeout(
            DOG_FIXTURE_START_BUDGET,
            ctx.supervisor.start_dog(app, DogSource::BuiltIn),
        )
        .await
        .expect("the dog fixture must start inside its budget")
        .expect("the dog fixture must start");
    }

    /// The production case for the inference: a dog on an older protocol
    /// cannot send `Hello::dog_name`, so the refusal it earns is anonymous and
    /// `record_refused_dog` never runs for it.
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
        // still maturing, and this case is about the rungs, not the gate.
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

    /// Written against a clock ten budgets past the point where a silent dog
    /// would have been condemned twice over: this case passes for the wrong
    /// reason if the inference never fires at all.
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

    /// Re-laddering a stale dog would spend a restart the record already says
    /// was spent, and write the same report once per budget for as long as the
    /// daemon runs.
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

    /// `Request::DogStaleness` derives the same set and `shep daemon reload`
    /// polls it every 50ms, so a ladder driven from there would restart a
    /// merely slow dog and report it stale inside a second.
    #[tokio::test(start_paused = true)]
    async fn asking_repeatedly_does_not_advance_the_ladder() {
        let h = crate::testing::harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        start_test_dog(&h.ctx, "metrics").await;
        // Past the warm-up: the ladder judges nothing while attribution is
        // still maturing, and this case is about the rungs, not the gate.
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

    /// Fails if the warm-up swallows the one verdict it exists to protect, or
    /// if `spawn_silent_dog_watch`'s own loop stops calling `check_silent_dogs`
    /// at all: every test above calls it directly, and would keep passing with
    /// the watcher's tick path deleted.
    ///
    /// A warm-up wider than the ladder spends the stale rung against a map that
    /// is still cold, and `silent_dogs` then drops the dog, so no later look
    /// reclassifies it. [`settle_until`] drives virtual time here, so this
    /// stays in the fast tier rather than `mod slow`.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_never_calls_still_earns_its_rebuild_after_the_warm_up() {
        // A lower bound on when `PeerContacts` started warming, taken before
        // the harness that builds it: the map's clock starts inside `harness`
        // and nothing out here can ask it when.
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
        // ticks and asserting nothing happened, is what turns the warm-up gate
        // from assumed into proved: an "assert nothing yet" passes just as
        // happily when the watch's loop never ran at all.
        let restart_rung = settle_until("the silent dog's restart rung", LADDER_BUDGET, || {
            !refusals.restarting().is_empty()
        })
        .await;
        assert_eq!(
            refusals.restarting(),
            vec!["metrics".to_string()],
            "the dog nothing ever connected from is the one that earns the rung"
        );

        // When the rung landed is the whole point: a ladder on a cold map
        // reaches it one budget after the watch spawned, one that waits a
        // budget after the warm-up ends. The `is_warming` assertion above pins
        // the map as cold at spawn, so the two cannot coincide.
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
        // still maturing, and this case is about the rungs, not the gate.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;
        let refusals = h.ctx.dog_refusals.clone();

        let watch = spawn_silent_dog_watch(
            h.ctx.supervisor.clone(),
            refusals.clone(),
            h.ctx.peer_contacts.clone(),
            h.ctx.events.clone(),
        );

        // The watcher's own interval fires an immediate first tick, which
        // records the dog as seen-silent-since-now. The wait below idles the
        // runtime, and an idle runtime is when the paused clock moves.
        let waited = settle_until("the silent dog's restart", LADDER_BUDGET, || {
            !refusals.restarting().is_empty()
        })
        .await;

        assert_eq!(
            refusals.restarting(),
            vec!["metrics".to_string()],
            "one budget of silence, driven through the watcher's own tick, must earn exactly one restart"
        );
        // The budget is asserted rather than assumed: a watch that judged a
        // dog early would earn the same restart and pass on the line above.
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

    /// The whole diagnosis rests on that difference: one means the dog is not
    /// reaching the socket, the other that it is reaching it and not naming
    /// itself, and they have opposite fixes.
    #[tokio::test(start_paused = true)]
    async fn a_pid_that_never_called_is_told_apart_from_one_that_called_anonymously() {
        let contacts = PeerContacts::new();

        // Past the warm-up: on a map this new, absence is not yet a finding.
        // The subject here is the None/Anonymous/Named distinction.
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
    /// seconds every dog carried across the handover is absent from it. Reading
    /// that absence as "this dog never called" puts the reinstall verdict on a
    /// dog that is fine.
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
        // back.
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

    /// The question is whether this process has ever named itself, so a
    /// reconnect read before its `Hello` must not move it back into the pile.
    #[test]
    fn a_pid_that_has_named_a_dog_goes_on_having_named_one() {
        let contacts = PeerContacts::new();
        contacts.named_a_dog(7);
        contacts.connected(7);
        assert_eq!(contacts.from_pid(Some(7)), Contact::Named);
    }

    /// The bound stops this state growing without limit, and the eviction rule
    /// stops the bound costing the answer: a dog reconnects, so it is touched,
    /// so it survives any amount of churn from short-lived `shep` invocations.
    #[tokio::test(start_paused = true)]
    async fn a_full_map_forgets_the_pid_that_stopped_calling() {
        let contacts = PeerContacts::new();
        // An evicted entry reads as `None` only once the map is old enough for
        // an absence to be a finding. The subject here is eviction.
        tokio::time::advance(PEER_CONTACT_WARMUP * 2).await;
        let dog = 1;
        contacts.named_a_dog(dog);

        // Every stranger arrives after the dog's first call, and the dog calls
        // again partway through, which is what a live dog does.
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

    /// The assertion is not that the wording is nice: it is that the
    /// stale-binary claim appears on the one path where nothing was ever seen
    /// to arrive, and that the connected-but-anonymous path says the opposite
    /// out loud.
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
        // The whole command, not the flag on its own: `contains("--force")`
        // would pass on any sentence that mentioned it. A plain `cargo install
        // <crate>` on a dog whose version has not moved builds nothing and
        // exits 0.
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

    /// [`Contact::Named`] is the interesting row: a pid that named a dog and
    /// is judged silent anyway is a contradiction, since naming one sets
    /// `handshook` and `silent_dogs` filters a handshook dog out. The only
    /// honest reading is that the attribution cannot be trusted.
    #[tokio::test(start_paused = true)]
    async fn evidence_is_read_off_the_record_and_never_guessed() {
        let contacts = PeerContacts::new();
        // `Unreachable` is only ever read off a map that has been watching
        // long enough to claim it.
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

    /// Fails if shep's own account of a dog stays in `shepd.err.log`, where the
    /// dog's operator was never told to look.
    ///
    /// Both halves are asserted: the file is what survives to be read
    /// afterwards, and the bus is what a `shep bleats --follow` sees live.
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

        // A real `log.*` forwarder: `Bus::publish_log` skips the whole publish
        // while nothing has registered an interest in log topics, so a plain
        // `subscribe()` would assert the gate is shut. Registered first,
        // because a broadcast receiver starts at the channel's current tail.
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
