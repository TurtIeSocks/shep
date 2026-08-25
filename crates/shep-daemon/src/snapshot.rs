//! The muster roll: persisted flock state for restart-survival (`shep muster`)
//!
//! `FlockRegistry` tracks each registered sheep's [`AppConfig`] in memory;
//! `FlockRegistry::roll` turns it plus a live [`ProcessInfo`] listing into
//! a [`FlockSnapshot`], written to `flock.json` by `write_atomic`. A
//! `SnapshotWriter` task (`spawn_snapshot_writer`) debounces lifecycle
//! events off the bus so a whole restart storm produces one write, not one
//! per event. `restorable` turns a loaded snapshot back into apps a
//! `muster` should start, re-validating every entry (the file is
//! human-editable) and collecting failures instead of aborting the whole
//! muster.
//!
//! `muster` reads the roll and starts those apps. It is the daemon's ONE
//! restore path, reached both from `boot` and from the `Muster` request an
//! operator sends, so the restore that runs unattended after a reboot is the
//! same code an operator can drive by hand.
//!
//! ## Restore rule
//!
//! An app restores iff it was running when the roll was saved
//! (`instances_running > 0`) AND `autostart` is still true — "was up when we
//! saved" is the muster contract; `autostart = false` is the user's explicit
//! opt-out of being brought back automatically.
//!
//! An app the flock ALREADY has is then left exactly where it stands, and is
//! still reported as restored. That half of the rule only ever bites the
//! operator's call — boot's flock is empty by construction — and `muster`'s
//! own doc carries the reasoning.

use core::fmt;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep_until};

use shep_core::config::{AppConfig, NormalizeError, ResolvedApp, normalize};
use shep_core::protocol::{BusEvent, ProcessInfo};
use shep_core::status::ProcStatus;

use crate::supervisor::SupervisorHandle;

/// Schema version of `flock.json`
pub(crate) const SNAPSHOT_VERSION: u32 = 1;

/// How long the writer lets a burst of lifecycle events settle before it
/// rewrites the roll.
///
/// One restart emits Exit + Restart + Start + Online within microseconds;
/// 250 ms folds a whole restart storm into a single atomic write while still
/// landing the roll orders of magnitude faster than the reboot it protects
/// against (spec §13.4).
pub(crate) const SNAPSHOT_DEBOUNCE_MS: u64 = 250;

/// The muster roll: which apps were registered, and how many were up
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlockSnapshot {
    /// Schema version this roll was written under (`SNAPSHOT_VERSION`)
    pub version: u32,
    /// Wall-clock milliseconds since the Unix epoch when this roll was built
    pub saved_at_ms: u64,
    /// One entry per sheep still known to the flock at save time
    pub apps: Vec<SavedApp>,
}

/// One sheep's entry in a [`FlockSnapshot`]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedApp {
    /// The config the sheep was started from (Debug redacts `env` — nests
    /// through [`AppConfig`]'s own redacting `Debug`, IR-41)
    pub app: AppConfig,
    /// How many instances of this sheep were running when the roll was built
    pub instances_running: u32,
}

/// The daemon's record of the config each registered sheep was started from
///
/// The supervisor owns runtime state; nothing in a [`ProcessInfo`] can
/// reproduce the `AppConfig` a sheep came from, which is exactly what a roll
/// needs. Cheap to clone (one `Arc`).
#[derive(Debug, Clone, Default)]
pub(crate) struct FlockRegistry {
    apps: Arc<Mutex<BTreeMap<String, AppConfig>>>,
}

impl FlockRegistry {
    /// Builds an empty registry
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records (or re-records) each app's config, keyed by name.
    pub(crate) fn record(&self, apps: &[ResolvedApp]) {
        let mut map = self.apps.lock().unwrap_or_else(PoisonError::into_inner);
        for app in apps {
            map.insert(app.config().name.clone(), app.config().clone());
        }
    }

    /// Drops every recorded app, so the next [`Self::roll`] describes an
    /// empty flock regardless of what is still live in the supervisor's own
    /// listing.
    ///
    /// The one caller is [`crate::boot::RunningDaemon::run`]'s teardown,
    /// and only when that boot asked for it
    /// (`BootOptions::delete_flock_on_shutdown`) — `shep dev`'s isolated
    /// session, where nothing should survive for a later `shep muster` to
    /// restore, even when the session ends by signal rather than through
    /// the CLI's own `Stop`/`Delete` requests. A production `runtime`/
    /// `daemon` shutdown never calls this: the roll surviving with the
    /// flock's true running state is the entire point of muster.
    pub(crate) fn clear(&self) {
        self.apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Builds the roll from the live listing, pruning names the flock no
    /// longer has (a deleted sheep must not resurrect).
    #[must_use]
    pub(crate) fn roll(&self, infos: &[ProcessInfo], now_ms: u64) -> FlockSnapshot {
        // A poisoned lock recovers instead of panicking: the map is a plain
        // BTreeMap, so a panic elsewhere cannot leave it inconsistent, and
        // taking the daemon down over it would be the worse failure.
        let mut apps = self.apps.lock().unwrap_or_else(PoisonError::into_inner);
        apps.retain(|name, _| infos.iter().any(|info| &info.name == name));
        let saved = apps
            .iter()
            .map(|(name, app)| SavedApp {
                app: app.clone(),
                instances_running: u32::try_from(
                    infos
                        .iter()
                        .filter(|i| &i.name == name && is_running(i.status))
                        .count(),
                )
                .unwrap_or(u32::MAX),
            })
            .collect();
        FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: now_ms,
            apps: saved,
        }
    }
}

/// True for the statuses [`FlockRegistry::roll`] counts as "up".
///
/// [`ProcStatus::Stopping`] is deliberately absent: a reload's drainee is
/// going away and its replacement already holds the same instance slot, so
/// counting both would report two running instances for one — the muster
/// roll must see only the replacement, never the entry riding out its own
/// kill ladder to a terminal status.
fn is_running(status: ProcStatus) -> bool {
    matches!(
        status,
        ProcStatus::Online | ProcStatus::Starting | ProcStatus::WaitingRestart
    )
}

/// Error type returned from `write_atomic` and [`read`]
///
/// Wraps `io::Error`/`serde_json::Error` directly rather than stringifying
/// them (contrast [`WireError`](shep_core::protocol::WireError)) so callers
/// keep the underlying OS/serde diagnostic via [`core::error::Error::source`];
/// that costs this enum `Clone`/`PartialEq`/`Eq` (IR-19's documented
/// exception for variants wrapping `io::Error`).
///
/// `#[non_exhaustive]`: today's four variants cover the write-side steps (no
/// parent, serialize, I/O) and the read-side parse failure, and a future
/// roll-format change — a version this daemon recognizes but refuses to
/// restore, rather than merely fails to parse — would need its own variant
/// distinct from [`Self::Parse`]'s catch-all, and shep-daemon is a published
/// library an out-of-tree matcher should not break for (IR-20).
#[non_exhaustive]
#[derive(Debug)]
pub enum SnapshotError {
    /// The roll path has no parent directory to create the temp file in
    /// (carries the path)
    NoParent(PathBuf),
    /// The roll failed to serialize to JSON
    Encode(serde_json::Error),
    /// The temp file, `fsync`, rename, or read failed
    Io(std::io::Error),
    /// The roll on disk is not valid JSON, or its `version` is one this
    /// daemon does not know how to restore (carries the parse/version
    /// message)
    Parse(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoParent(path) => {
                write!(f, "roll path `{}` has no parent directory", path.display())
            }
            Self::Encode(err) => write!(f, "muster roll failed to serialize: {err}"),
            Self::Io(err) => write!(f, "muster roll I/O failed: {err}"),
            Self::Parse(msg) => write!(f, "muster roll is unreadable: {msg}"),
        }
    }
}

impl core::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Encode(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::NoParent(_) | Self::Parse(_) => None,
        }
    }
}

impl From<serde_json::Error> for SnapshotError {
    fn from(source: serde_json::Error) -> Self {
        Self::Encode(source)
    }
}

impl From<std::io::Error> for SnapshotError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

/// Writes `snapshot` to `path` atomically: a temp file in the same
/// directory (so `rename(2)` is guaranteed atomic — it only is within one
/// filesystem), `fsync`ed, then renamed over `path`.
///
/// The temp file is created owner-only (unix mode 0600) and `persist` keeps
/// that mode across the rename. That mode is not cosmetic: the roll stores
/// app `env` verbatim so a restore can reproduce it, which is the one place
/// shep writes secrets to disk (spec §10 redacts them everywhere else).
///
/// # Errors
/// - [`SnapshotError::NoParent`] — the roll path has no directory to write into.
/// - [`SnapshotError::Encode`] — the roll failed to serialize.
/// - [`SnapshotError::Io`] — the temp file, fsync, or rename failed.
pub(crate) fn write_atomic(path: &Path, snapshot: &FlockSnapshot) -> Result<(), SnapshotError> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| SnapshotError::NoParent(path.to_path_buf()))?;
    let json = serde_json::to_vec_pretty(snapshot)?;

    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(&json)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|err| SnapshotError::Io(err.error))?;
    Ok(())
}

/// Reads and validates a muster roll written by `write_atomic`.
///
/// Public only for `tests/daemon_e2e.rs`, which reads the roll a live daemon
/// wrote and asserts on its contents; [`FlockSnapshot`], [`SavedApp`] and
/// [`SnapshotError`] are public because they are what this returns. The
/// daemon's own restore path calls this from inside `boot`.
///
/// # Errors
/// - [`SnapshotError::Io`] — the roll could not be read.
/// - [`SnapshotError::Parse`] — invalid JSON, or a schema version this
///   daemon does not know.
pub fn read(path: &Path) -> Result<FlockSnapshot, SnapshotError> {
    let bytes = std::fs::read(path)?;
    let snapshot: FlockSnapshot =
        serde_json::from_slice(&bytes).map_err(|err| SnapshotError::Parse(err.to_string()))?;
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(SnapshotError::Parse(format!(
            "roll schema version {} is not one this daemon knows (expected {SNAPSHOT_VERSION})",
            snapshot.version
        )));
    }
    Ok(snapshot)
}

/// What a `muster` should put back: every member, which of them to start, and
/// the ones the roll can no longer justify
#[derive(Debug)]
pub(crate) struct Restorable {
    /// Every entry that still normalizes, in roll order. The flock is a
    /// membership list, so all of these are registered whether or not they
    /// run.
    pub(crate) members: Vec<ResolvedApp>,
    /// The subset of [`Self::members`] that was up when the roll was written
    /// and still opts into `autostart`.
    pub(crate) to_start: Vec<ResolvedApp>,
    /// Sheep name + the reason its saved config no longer normalizes
    pub(crate) rejected: Vec<(String, NormalizeError)>,
}

/// Splits a loaded [`FlockSnapshot`] into members, the subset to start, and
/// the entries rejected on re-validation.
///
/// **Membership survives everything but `delete`.** A sheep that was stopped
/// when the roll was written comes back registered and `Stopped`, not gone.
/// The rule this replaces restored an app only when
/// `instances_running > 0 && autostart`, which made `shep stop` silently
/// destructive across a daemon restart: stop a sheep, restart the shepherd,
/// and the sheep was no longer in the flock at all, with its config sitting
/// in the roll where only `cat` would find it. Rin hit exactly that and asked
/// why stopping something should mean forgetting it. It should not.
///
/// The old rule was not wrong about what to START -- "was up when we saved"
/// is still the contract for that, and `autostart = false` is still the
/// user's opt-out of coming back automatically. It was wrong to conflate
/// running with belonging. [`FlockRegistry::roll`] never did: it keeps an
/// app in the roll whenever it appears in the listing, running or not, and
/// records the count separately. This is the read side finally agreeing with
/// the write side.
///
/// The roll is a file a human can edit, so every surviving entry is run back
/// through [`normalize()`] exactly like peer input (spec §6's "the daemon
/// MUST re-normalize" rule) — a bad entry is collected into `rejected`
/// instead of aborting the whole muster.
#[must_use]
pub(crate) fn restorable(snapshot: FlockSnapshot) -> Restorable {
    let mut members = Vec::new();
    let mut to_start = Vec::new();
    let mut rejected = Vec::new();
    for saved in snapshot.apps {
        let name = saved.app.name.clone();
        let was_up = saved.instances_running > 0;
        let autostart = saved.app.autostart;
        match normalize(saved.app) {
            Ok(resolved) => {
                if was_up && autostart {
                    to_start.push(resolved.clone());
                }
                members.push(resolved);
            }
            Err(err) => rejected.push((name, err)),
        }
    }
    Restorable {
        members,
        to_start,
        rejected,
    }
}

/// Reads the muster roll and starts every app it restores, returning the
/// names it restored.
///
/// The daemon's one restore path, and deliberately only one: `boot` runs it
/// when `--restore` is set, and the `Muster` request runs it for an operator,
/// so the restore that happens unattended after a reboot is the same code an
/// operator exercises by hand. The two callers differ in what they do with
/// the return — boot discards it, the request handler turns it into a listing
/// — which is why this hands back NAMES rather than a listing: a listing
/// would tempt the boot path into reporting one nobody reads.
///
/// An app the flock already has is left exactly where it stands, and is still
/// counted as restored. Boot never meets that case, since its flock is empty
/// by construction; an operator meets it whenever a muster follows a partial
/// restore, or simply runs twice. Starting such an app again would not be the
/// no-op it looks like: [`instance_slots`](crate::assemble::instance_slots)
/// allocates the lowest FREE slot, so a second start of a one-instance app
/// leaves it running two, and the next roll records the pair. Restarting it
/// instead would drop live connections over a verb that never claimed to be
/// `restart`, and refusing the whole muster over it would break the
/// partial-restore case the verb is most useful for. Leaving it alone is what
/// makes the verb idempotent, and idempotence is what lets it be run twice,
/// or from a boot script, without an operator having to think about it. The
/// unit of that rule is the APP, not the instance count: a muster starts apps
/// the flock does not have, and never adds instances to one it does.
///
/// A missing roll restores nothing and is not an error — a fresh
/// `$SHEP_HOME` has none, and that is a first boot rather than a fault. A
/// roll that exists and will not parse IS reported: something corrupted or
/// hand-edited it, and starting an empty flock instead would hide that.
///
/// # Errors
/// - [`SnapshotError`] — the roll exists but could not be read or parsed.
pub(crate) async fn muster(
    path: &Path,
    registry: &FlockRegistry,
    supervisor: &SupervisorHandle,
) -> Result<Vec<String>, SnapshotError> {
    let saved = match read(path) {
        Ok(saved) => saved,
        Err(SnapshotError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => return Err(err),
    };
    let restorable = restorable(saved);
    for (name, err) in &restorable.rejected {
        tracing::warn!(name, %err, "muster roll entry rejected on restore");
    }
    if restorable.members.is_empty() {
        return Ok(Vec::new());
    }
    let restored: Vec<String> = restorable
        .members
        .iter()
        .map(|app| app.config().name.clone())
        .collect();

    // A supervisor that cannot be listed has no flock left to collide with,
    // and the `start` below announces itself when it fails for that same
    // reason, so this needs no branch of its own.
    let running = supervisor.list_checked().await.unwrap_or_default();
    let known = |app: &ResolvedApp| running.iter().any(|info| info.name == app.config().name);

    // Membership for every surviving entry the `start` below will NOT bring
    // up: the flock is a list of what belongs, and `delete` is the only thing
    // that ends belonging. A sheep saved while stopped lands here and nowhere
    // else, which is the whole point -- it comes back listed and restartable
    // instead of vanishing.
    //
    // The ones being started are excluded deliberately. Registering them at
    // rest first would leave an idle `instance: 0` entry for `start` to
    // allocate around, and the sheep would show up twice: once stopped, once
    // online.
    let starting: Vec<&str> = restorable
        .to_start
        .iter()
        .map(|app| app.config().name.as_str())
        .collect();
    let members: Vec<ResolvedApp> = restorable
        .members
        .iter()
        .filter(|app| !known(app) && !starting.contains(&app.config().name.as_str()))
        .cloned()
        .collect();
    if !members.is_empty() {
        registry.record(&members);
        if let Err(err) = supervisor.register_at_rest(members).await {
            tracing::warn!(%err, "muster roll restore could not register one or more apps");
        }
    }

    let to_start: Vec<ResolvedApp> = restorable
        .to_start
        .into_iter()
        .filter(|app| !known(app))
        .collect();
    if to_start.is_empty() {
        return Ok(restored);
    }
    // Recorded regardless of whether `start` below fully succeeds, matching
    // `rpc::run`'s own Start handler: already-registered entries must
    // persist even when a later spawn in the same batch fails. Only what
    // this call starts is recorded — an app left where it stands keeps the
    // config it is actually running under, rather than having the roll's
    // copy written over it for the next save to persist.
    registry.record(&to_start);
    if let Err(err) = supervisor.start(to_start).await {
        // One bad entry does not sink the muster — the same policy
        // `restorable` already applies at validation time. The sheep that
        // failed to spawn is already recorded `Errored` by the supervisor.
        tracing::warn!(%err, "muster roll restore failed to spawn one or more apps");
    }
    Ok(restored)
}

/// True for lifecycle transitions the roll cares about; false for log
/// traffic and daemon-wide notices, which must not trigger a rewrite.
fn is_state_change(event: &BusEvent) -> bool {
    matches!(event, BusEvent::Process { .. })
}

/// Handle to the debounced writer task
#[derive(Debug)]
pub(crate) struct SnapshotWriter {
    handle: JoinHandle<()>,
    /// Read only by [`Self::writes`]; see that method for why both carry an
    /// `allow` rather than an `expect`.
    #[allow(dead_code, reason = "read by this crate's own tests through `writes`")]
    writes: Arc<AtomicU64>,
}

impl SnapshotWriter {
    /// Completed roll writes since boot — the number the metrics dog reports
    ///
    /// The dog reads that number off the wire, not off this handle, so this
    /// accessor's only callers today are this module's own tests, and it is
    /// dead in a non-test build. `allow` rather than `expect` because the
    /// expectation would go unfulfilled in the test build.
    // IR-25: trivial atomic load, no branch — inline across codegen units.
    // Not per-frame hot, so `#[inline]`, never `#[inline(always)]`.
    #[inline]
    #[must_use]
    #[allow(dead_code, reason = "called by this module's own tests")]
    pub(crate) fn writes(&self) -> u64 {
        self.writes.load(Ordering::SeqCst)
    }

    /// Stops the writer and waits for it (the caller then owns roll timing)
    pub(crate) async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// Spawns the debounced muster-roll writer.
///
/// Coalesces bursts of lifecycle events (spec §13.4: one restart storm, one
/// write) into a single [`write_atomic`] call per [`SNAPSHOT_DEBOUNCE_MS`]
/// window. Log traffic never resets or starts the debounce timer.
pub(crate) fn spawn_snapshot_writer(
    path: PathBuf,
    supervisor: SupervisorHandle,
    registry: FlockRegistry,
    events: broadcast::Receiver<BusEvent>,
) -> SnapshotWriter {
    let writes = Arc::new(AtomicU64::new(0));
    let task_writes = Arc::clone(&writes);
    let handle = tokio::spawn(run_writer(path, supervisor, registry, events, task_writes));
    SnapshotWriter { handle, writes }
}

/// The writer's actor loop — cancel-safe by construction: the debounce
/// deadline is recomputed from the STORED `Option<Instant>` every iteration,
/// so losing the `select!` race never extends the window.
async fn run_writer(
    path: PathBuf,
    supervisor: SupervisorHandle,
    registry: FlockRegistry,
    mut events: broadcast::Receiver<BusEvent>,
    writes: Arc<AtomicU64>,
) {
    let mut deadline: Option<Instant> = None;
    loop {
        tokio::select! {
            received = events.recv() => match received {
                // Only lifecycle events change the roll; log lines must not
                // rewrite a file once per output line.
                Ok(event) => if is_state_change(&event) && deadline.is_none() {
                    deadline = Some(Instant::now() + Duration::from_millis(SNAPSHOT_DEBOUNCE_MS));
                },
                // A lag may have swallowed a lifecycle event: assume dirty.
                Err(RecvError::Lagged(_)) => if deadline.is_none() {
                    deadline = Some(Instant::now() + Duration::from_millis(SNAPSHOT_DEBOUNCE_MS));
                },
                Err(RecvError::Closed) => break,
            },
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = None;
                write_now(&path, &supervisor, &registry, &writes).await;
            }
        }
    }
}

// The write is a few KiB to a local file once per debounce window;
// `spawn_blocking` would buy a task hop and nothing else.
async fn write_now(
    path: &Path,
    supervisor: &SupervisorHandle,
    registry: &FlockRegistry,
    writes: &AtomicU64,
) {
    // Engine gone: there is nothing left to record and the shutdown path has
    // already written the final roll.
    let Ok(infos) = supervisor.list_checked().await else {
        return;
    };
    let roll = registry.roll(&infos, crate::now_ms()); // lock released before any IO
    match write_atomic(path, &roll) {
        Ok(()) => {
            writes.fetch_add(1, Ordering::SeqCst);
        }
        Err(err) => tracing::warn!(%err, "muster roll write failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use crate::testing::test_paths; // the one crate-root fixture (IR-33)
    use shep_core::config::{AppConfig, normalize};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
    use shep_core::status::ProcStatus;
    use std::time::Duration;

    fn info(id: u32, name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(id, name, status)
            .pid(Some(1000 + id))
            .out_file(Some(format!("/logs/{name}-0-out.log")))
            .err_file(Some(format!("/logs/{name}-0-err.log")))
            .build()
    }

    // fails if `is_running` starts counting `ProcStatus::Stopping` — the
    // status a reload's drainee wears from the moment its replacement is
    // spawned. Both entries share one instance slot for the length of the
    // swap, so counting the drainee as well would roll a one-instance app at
    // two. This is the half of that guarantee that lives here: the
    // supervisor's
    // `a_reload_stops_counting_the_drainee_as_running_before_its_replacement_starts`
    // pins the other half, that the mark lands before the replacement
    // registers. Neither covers for the other.
    #[test]
    fn roll_counts_running_instances_and_prunes_deleted_names() {
        let registry = FlockRegistry::new();
        let web = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        let job = normalize(AppConfig::minimal("job", "./job")).unwrap();
        registry.record(&[web, job]);

        let infos = [
            info(0, "web", ProcStatus::Online),
            info(1, "web", ProcStatus::WaitingRestart),
            info(2, "web", ProcStatus::Stopped),
            info(3, "web", ProcStatus::Stopping),
        ]; // `job` was deleted: no entries left
        let roll = registry.roll(&infos, 1_700_000_000_000);

        assert_eq!(roll.version, SNAPSHOT_VERSION);
        assert_eq!(roll.saved_at_ms, 1_700_000_000_000);
        assert_eq!(roll.apps.len(), 1, "a name with no live entry is pruned");
        assert_eq!(roll.apps[0].app.name, "web");
        // online + waiting-restart; neither the stopped one nor the drainee
        assert_eq!(roll.apps[0].instances_running, 2);
        // The prune is sticky: a second roll must not resurrect `job`.
        assert_eq!(registry.roll(&infos, 0).apps.len(), 1);
    }

    #[test]
    fn write_atomic_round_trips_with_no_leftovers() {
        // Portable: snapshot.rs is in the 'compiles everywhere' tier (Global
        // Constraints' portability split). The 0600 owner-only guarantee is
        // unix-only and lives in write_atomic_is_owner_only_on_unix below —
        // Windows has no equivalent permission bit to assert here.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.json");
        let registry = FlockRegistry::new();
        registry.record(&[normalize(AppConfig::minimal("web", "./srv")).unwrap()]);
        let roll = registry.roll(&[info(0, "web", ProcStatus::Online)], 42);

        write_atomic(&path, &roll).unwrap();
        write_atomic(&path, &roll).unwrap(); // overwriting keeps the guarantees

        assert_eq!(read(&path).unwrap(), roll);
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "no temp file may survive a completed write"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_is_owner_only_on_unix() {
        // The roll stores app env verbatim (spec §10): owner-only, always.
        // This is the ONE unix-gated test in an otherwise-portable file
        // (Global Constraints' portability split exception) — 0600 is a
        // unix permission-bit guarantee with no Windows ACL equivalent.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.json");
        let registry = FlockRegistry::new();
        registry.record(&[normalize(AppConfig::minimal("web", "./srv")).unwrap()]);
        let roll = registry.roll(&[info(0, "web", ProcStatus::Online)], 42);

        write_atomic(&path, &roll).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the muster roll holds app env in cleartext");
    }

    #[test]
    fn read_rejects_corrupt_json_and_unknown_schema_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(matches!(read(&path), Err(SnapshotError::Parse { .. })));

        let future = format!(
            "{{\"version\":{},\"saved_at_ms\":0,\"apps\":[]}}",
            SNAPSHOT_VERSION + 1
        );
        std::fs::write(&path, future.as_bytes()).unwrap();
        assert!(matches!(read(&path), Err(SnapshotError::Parse { .. })));
    }

    /// Membership and running are different questions, and the roll answers
    /// both. Every entry that still normalizes is a member; only the ones
    /// that were up and opt into `autostart` are started.
    ///
    /// The old rule dropped the other two entirely, which made `shep stop`
    /// destructive across a daemon restart: the sheep left the flock and its
    /// config survived only in a file the operator had to `cat` to find.
    #[test]
    fn restorable_keeps_every_member_and_starts_only_what_was_up() {
        let mut stopped = AppConfig::minimal("stopped", "./s");
        stopped.instances = 1;
        let mut opted_out = AppConfig::minimal("manual", "./m");
        opted_out.autostart = false;

        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![
                SavedApp {
                    app: AppConfig::minimal("web", "./srv"),
                    instances_running: 2,
                },
                SavedApp {
                    app: stopped,
                    instances_running: 0,
                },
                SavedApp {
                    app: opted_out,
                    instances_running: 1,
                },
            ],
        };
        let restorable = restorable(roll);

        let members: Vec<&str> = restorable
            .members
            .iter()
            .map(|a| a.config().name.as_str())
            .collect();
        assert_eq!(
            members,
            ["web", "stopped", "manual"],
            "every entry that normalizes belongs to the flock, running or not"
        );

        let starting: Vec<&str> = restorable
            .to_start
            .iter()
            .map(|a| a.config().name.as_str())
            .collect();
        assert_eq!(
            starting,
            ["web"],
            "only the sheep that was up and opts into autostart is started"
        );
        assert!(restorable.rejected.is_empty());
    }

    /// The bug this contract change exists for, at the unit level: a sheep
    /// saved while stopped is still a member, so a later `restart` has
    /// something to find.
    #[test]
    fn a_sheep_saved_while_stopped_is_still_a_member() {
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp {
                app: AppConfig::minimal("zeus-auth", "./zeus-auth"),
                instances_running: 0,
            }],
        };
        let restorable = restorable(roll);
        assert_eq!(restorable.members.len(), 1, "stopping is not forgetting");
        assert_eq!(restorable.members[0].config().name, "zeus-auth");
        assert!(
            restorable.to_start.is_empty(),
            "but it stays stopped: it was not running when the roll was written"
        );
    }

    #[test]
    fn restorable_reports_a_hand_edited_invalid_app_instead_of_aborting() {
        let mut broken = AppConfig::minimal("broken", "./b");
        broken.instances = 0; // someone edited the roll
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![
                SavedApp {
                    app: broken,
                    instances_running: 1,
                },
                SavedApp {
                    app: AppConfig::minimal("web", "./srv"),
                    instances_running: 1,
                },
            ],
        };
        let restorable = restorable(roll);
        assert_eq!(
            restorable.members.len(),
            1,
            "one bad entry must not sink the muster"
        );
        assert_eq!(
            restorable.rejected,
            vec![(
                "broken".to_string(),
                shep_core::config::NormalizeError::ZeroInstances
            )]
        );
    }

    #[test]
    fn debug_does_not_leak_env_values() {
        // IR-41: the roll carries env; its Debug lands in daemon logs.
        let mut app = AppConfig::minimal("web", "./srv");
        app.env
            .insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp {
                app,
                instances_running: 1,
            }],
        };
        let rendered = format!("{roll:?}");
        assert!(!rendered.contains("postgres://secret"), "{rendered}");
        assert!(rendered.contains("<1 vars>"), "{rendered}");
    }

    /// fails if `muster` starts an app the roll says was down, skips one it
    /// says was up, or drops either from the flock. All three in one case,
    /// because a restore rule that inverted would pass any one alone.
    ///
    /// Both entries come back as members -- membership ends only at
    /// `delete` -- and exactly one of them is running.
    #[tokio::test(start_paused = true)]
    async fn muster_restores_both_and_starts_only_the_one_that_was_up() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(paths.snapshot.parent().unwrap()).unwrap();
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![
                SavedApp {
                    app: AppConfig::minimal("up", "./srv"),
                    instances_running: 1,
                },
                SavedApp {
                    app: AppConfig::minimal("down", "./srv"),
                    instances_running: 0,
                },
            ],
        };
        write_atomic(&paths.snapshot, &roll).unwrap();

        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let handle = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events,
        );
        let registry = FlockRegistry::new();

        let restored = muster(&paths.snapshot, &registry, &handle).await.unwrap();
        assert_eq!(
            restored,
            vec!["up".to_string(), "down".to_string()],
            "both are restored to the flock; only one of them runs"
        );

        let mut listed = handle.list().await;
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        let seen: Vec<(&str, ProcStatus)> =
            listed.iter().map(|i| (i.name.as_str(), i.status)).collect();
        assert_eq!(
            seen,
            vec![("down", ProcStatus::Stopped), ("up", ProcStatus::Online)],
            "the sheep that was down is listed and stopped, not missing"
        );
        handle.shutdown().await;
    }

    /// fails if `muster` starts an app the flock already has.
    ///
    /// `instance_slots` hands a second `Start` of a one-instance app the next
    /// FREE slot rather than nothing, so a muster that started every
    /// restorable app unconditionally would leave a one-instance app running
    /// two — and the next roll would record the pair. The single script is
    /// what makes that visible: the flock's own `web` consumes it, so the
    /// duplicate lands as `Errored` (`ScriptedRunner` answers
    /// `SpawnFailed("script exhausted")`), and this asserts on the status as
    /// well as the count so the wrong implementation cannot pass by leaving
    /// one healthy sheep and one wreck.
    #[tokio::test(start_paused = true)]
    async fn muster_leaves_an_app_the_flock_already_has_where_it_stands() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(paths.snapshot.parent().unwrap()).unwrap();
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![SavedApp {
                app: AppConfig::minimal("web", "./srv"),
                instances_running: 1,
            }],
        };
        write_atomic(&paths.snapshot, &roll).unwrap();

        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let handle = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events,
        );
        let registry = FlockRegistry::new();
        let app = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        registry.record(std::slice::from_ref(&app));
        handle.start(vec![app]).await.unwrap();

        let restored = muster(&paths.snapshot, &registry, &handle).await.unwrap();
        assert_eq!(
            restored,
            vec!["web".to_string()],
            "the roll's app is restored whether or not this call is what started it"
        );
        let listed = handle.list().await;
        assert_eq!(listed.len(), 1, "one instance of a one-instance app");
        assert_eq!(listed[0].status, ProcStatus::Online);
        handle.shutdown().await;
    }

    /// fails if a missing roll becomes an error. A fresh `$SHEP_HOME` has
    /// none, and a daemon that refused to boot over it would be unbootable
    /// on a clean machine.
    #[tokio::test(start_paused = true)]
    async fn a_missing_roll_restores_nothing_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(paths.snapshot.parent().unwrap()).unwrap();
        assert!(!paths.snapshot.exists());

        let (events, _rx) = tokio::sync::broadcast::channel(64);
        let handle = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events,
        );
        let registry = FlockRegistry::new();

        assert_eq!(
            muster(&paths.snapshot, &registry, &handle).await.unwrap(),
            Vec::<String>::new()
        );
        assert!(handle.list().await.is_empty());
        handle.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn writer_coalesces_a_burst_into_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.home).unwrap();
        let (events, _keep) = tokio::sync::broadcast::channel(64);
        let supervisor = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events.clone(),
        );
        let registry = FlockRegistry::new();
        let app = normalize(AppConfig::minimal("web", "./srv")).unwrap();
        registry.record(std::slice::from_ref(&app));
        supervisor.start(vec![app]).await.unwrap();

        // Subscribing here means the start's own events are already behind us.
        let writer = spawn_snapshot_writer(
            paths.snapshot.clone(),
            supervisor.clone(),
            registry,
            events.subscribe(),
        );
        for event in [
            ProcessEventKind::Exit,
            ProcessEventKind::Restart,
            ProcessEventKind::Online,
        ] {
            events
                .send(BusEvent::Process {
                    event,
                    info: info(0, "web", ProcStatus::Online),
                    manually: false,
                    at_ms: 0,
                })
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(SNAPSHOT_DEBOUNCE_MS + 1)).await;

        assert_eq!(writer.writes(), 1, "one debounce window is one write");
        let roll = read(&paths.snapshot).unwrap();
        assert_eq!(roll.apps.len(), 1);
        assert_eq!(roll.apps[0].instances_running, 1);
        writer.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn writer_ignores_log_traffic() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.home).unwrap();
        let (events, _keep) = tokio::sync::broadcast::channel(64);
        let supervisor = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]),
            paths.clone(),
            events.clone(),
        );
        let writer = spawn_snapshot_writer(
            paths.snapshot.clone(),
            supervisor,
            FlockRegistry::new(),
            events.subscribe(),
        );
        for id in 0..50 {
            events
                .send(BusEvent::LogOut {
                    id,
                    line: "chatty".to_string(),
                })
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(SNAPSHOT_DEBOUNCE_MS * 4)).await;
        assert_eq!(writer.writes(), 0, "log lines must never rewrite the roll");
        assert!(!paths.snapshot.exists());
        writer.stop().await;
    }
}
