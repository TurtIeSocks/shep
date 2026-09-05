//! The muster roll: persisted flock state for restart-survival (`shep muster`)
//!
//! `FlockRegistry::roll` turns the registry's [`AppConfig`]s plus a live
//! [`ProcessInfo`] listing into a [`FlockSnapshot`], written to `flock.json`
//! by `write_atomic`. A `SnapshotWriter` task debounces lifecycle events so a
//! restart storm produces one write. `restorable` re-validates every entry,
//! since the file is human-editable.
//!
//! `muster` reads the roll and starts those apps, from `boot` and from the
//! `Muster` request alike. An app restores iff it was running when the roll was
//! saved (`instances_running > 0`) and `autostart` is still true; one the flock
//! already has is left where it stands, still reported as restored.

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

use crate::bus::SharedEvent;
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
    /// The config the sheep was started from (Debug redacts `env`, through
    /// [`AppConfig`]'s own redacting `Debug`)
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

    /// Records one app's config directly, for a successor rebuilding this
    /// registry from a handover blob.
    ///
    /// [`Self::record`] takes [`ResolvedApp`]s because every other caller has
    /// just normalized one. A successor holds only the [`AppConfig`] the blob
    /// carried, and the supervisor re-normalizes that on its own way to
    /// installing the sheep.
    ///
    /// The roll is written from this registry, so a successor that left it
    /// empty would overwrite a good roll within seconds of taking over.
    #[cfg(unix)]
    pub(crate) fn record_config(&self, config: &AppConfig) {
        self.apps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(config.name.clone(), config.clone());
    }

    /// Drops every recorded app, so the next [`Self::roll`] describes an
    /// empty flock regardless of what is still live in the supervisor's own
    /// listing.
    ///
    /// The one caller is [`crate::boot::RunningDaemon::run`]'s teardown, under
    /// `BootOptions::delete_flock_on_shutdown`: `shep dev`'s isolated session,
    /// where nothing should survive for a later `shep muster`. A production
    /// shutdown never calls it.
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
/// [`ProcStatus::Stopping`] is absent: a reload's drainee and its replacement
/// hold the same instance slot, so counting both would report two running
/// instances for one.
fn is_running(status: ProcStatus) -> bool {
    matches!(
        status,
        ProcStatus::Online | ProcStatus::Starting | ProcStatus::WaitingRestart
    )
}

/// Error type returned from `write_atomic` and [`read`]
///
/// Wraps `io::Error`/`serde_json::Error` directly rather than stringifying
/// them, so callers keep the underlying diagnostic through
/// [`core::error::Error::source`]. That costs the enum
/// `Clone`/`PartialEq`/`Eq`.
///
/// `#[non_exhaustive]`: a future roll-format refusal would need its own
/// variant, distinct from [`Self::Parse`]'s catch-all.
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

/// Writes `snapshot` to `path` atomically: a temp file in the same directory,
/// since `rename(2)` is atomic only within one filesystem, `fsync`ed, renamed
/// over `path`, then on unix the directory `fsync`ed so the rename survives a
/// power cut (a no-op on Windows). The temp file is owner-only (unix mode
/// 0600) and `persist` keeps that mode: the roll stores app `env` verbatim,
/// the one place shep writes secrets to disk.
///
/// # Errors
/// - [`SnapshotError::NoParent`]: the roll path has no directory to write into.
/// - [`SnapshotError::Encode`]: the roll failed to serialize.
/// - [`SnapshotError::Io`]: the temp file, fsync, or rename failed.
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

    // The `sync_all` above made the CONTENTS durable; this makes the rename
    // that published them durable. See `shep_core::atomic_file`.
    shep_core::atomic_file::sync_dir(parent)?;
    Ok(())
}

/// Reads and validates a muster roll written by `write_atomic`.
///
/// Public only for `tests/daemon_e2e.rs`, which asserts on the roll a live
/// daemon wrote; the daemon's own restore path calls it from inside `boot`.
///
/// # Errors
/// - [`SnapshotError::Io`]: the roll could not be read.
/// - [`SnapshotError::Parse`]: invalid JSON, or a schema version this daemon
///   does not know.
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
/// the ones that failed re-validation
#[derive(Debug)]
pub(crate) struct Restorable {
    /// Every entry that still normalizes, in roll order. The flock is a
    /// membership list, so all of these are registered whether or not they
    /// run.
    pub(crate) members: Vec<ResolvedApp>,
    /// The subset of [`Self::members`] that was up when the roll was written
    /// and still opts into `autostart`.
    pub(crate) to_start: Vec<ResolvedApp>,
    /// Sheep name, and why its saved config failed to normalize
    pub(crate) rejected: Vec<(String, NormalizeError)>,
}

/// Splits a loaded [`FlockSnapshot`] into members, the subset to start, and
/// the entries rejected on re-validation.
///
/// Membership survives everything but `delete`: a sheep stopped when the roll
/// was written comes back registered and `Stopped`. What to START is a
/// separate question, answered by `instances_running > 0 && autostart`.
///
/// The roll is a file a human can edit, so every entry is run back through
/// [`normalize()`] like peer input, and a bad one is collected into `rejected`
/// rather than aborting the muster.
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
/// The daemon's one restore path: `boot` runs it under `--restore`, the
/// `Muster` request runs it for an operator. An app the flock already has is
/// left where it stands, still counted as restored, which makes the verb
/// idempotent; starting it again is not a no-op, since
/// [`instance_slots`](crate::assemble::instance_slots) takes the lowest free
/// slot. A missing roll is not an error; an unparseable one is reported.
///
/// # Errors
/// - [`SnapshotError`]: the roll exists but could not be read or parsed.
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
    // and the `start` below announces its own failure.
    let running = supervisor.list_checked().await.unwrap_or_default();
    let known = |app: &ResolvedApp| running.iter().any(|info| info.name == app.config().name);

    // Membership for every entry `start` will not bring up, so a sheep saved
    // while stopped comes back listed. The ones being started are excluded:
    // registering them at rest first would leave an idle `instance: 0` entry
    // for `start` to allocate around, and the sheep would show up twice.
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
    // Recorded whether or not `start` fully succeeds: already-registered
    // entries must persist even when a later spawn in the batch fails. Only
    // what this call starts, so an app left where it stands keeps the config
    // it is actually running under.
    registry.record(&to_start);
    // `start_restored`, not `start`: `start` refuses a whole batch over an app
    // whose script provably is not there, which is right for an operator typing
    // `shep start` and wrong at an unattended boot, where a binary missing after
    // a rebuild would cost the machine its entire flock.
    if let Err(err) = supervisor.start_restored(to_start).await {
        // One bad entry does not sink the muster; the sheep that failed to
        // spawn is already recorded `Errored` by the supervisor.
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
    /// Read only by [`Self::writes`], which carries the same `allow`.
    #[allow(dead_code, reason = "read by this crate's own tests through `writes`")]
    writes: Arc<AtomicU64>,
}

impl SnapshotWriter {
    /// Completed roll writes since boot
    ///
    /// The metrics dog reads this off the wire, so the only callers are this
    /// module's tests and it is dead in a non-test build.
    // A trivial atomic load, not per-frame hot: #[inline], never #[inline(always)].
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
    events: broadcast::Receiver<SharedEvent>,
) -> SnapshotWriter {
    let writes = Arc::new(AtomicU64::new(0));
    let task_writes = Arc::clone(&writes);
    let handle = tokio::spawn(run_writer(path, supervisor, registry, events, task_writes));
    SnapshotWriter { handle, writes }
}

/// The writer's actor loop. Cancel-safe: the debounce deadline is recomputed
/// from the stored `Option<Instant>` every iteration, so losing the `select!`
/// race never extends the window.
async fn run_writer(
    path: PathBuf,
    supervisor: SupervisorHandle,
    registry: FlockRegistry,
    mut events: broadcast::Receiver<SharedEvent>,
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
    use crate::testing::test_paths;
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

    // fails if `is_running` starts counting `ProcStatus::Stopping`, the status
    // a reload's drainee wears once its replacement is spawned. Both share one
    // instance slot for the swap, so counting the drainee would roll a
    // one-instance app at two.
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
        // The `0600` half lives in `write_atomic_is_owner_only_on_unix`.
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
        // Unix-gated because `0600` has no Windows ACL equivalent.
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

    /// Restoring only the running ones would make `shep stop` destructive
    /// across a daemon restart: the sheep would leave the flock entirely.
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

    /// So a later `restart` has something to find.
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
        // The roll carries env, and its Debug lands in daemon logs.
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
    /// says was up, or drops either from the flock. All three in one case: an
    /// inverted restore rule would pass any one alone.
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

        let (events, _rx) = crate::bus::test_bus(64);
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

    /// fails if a bad entry that is not LAST takes every app after it down.
    ///
    /// `FlockRegistry` is a `BTreeMap`, so the roll is alphabetical and
    /// `restorable` preserves that order: the `a-`/`b-`/`c-` names here make
    /// the roll order the production order, with the wreck in the middle.
    ///
    /// One `muster` call: `shep muster`'s CLI path calls twice and the second
    /// re-registers what the first lost, so only the single unattended restore
    /// a `shep startup` unit performs can show this. `failing_to_spawn` is
    /// load-bearing, since scripts are consumed in spawn order.
    #[tokio::test(start_paused = true)]
    async fn a_bad_saved_app_does_not_take_the_apps_after_it_down() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(paths.snapshot.parent().unwrap()).unwrap();
        let saved = |name: &str| SavedApp {
            app: AppConfig::minimal(name, "./srv"),
            instances_running: 1,
        };
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![saved("a-good"), saved("b-bad"), saved("c-good")],
        };
        write_atomic(&paths.snapshot, &roll).unwrap();

        let (events, _rx) = crate::bus::test_bus(64);
        let handle = spawn_supervisor(
            // Two scripts for the two that must come up. `b-bad` consumes
            // none, so a run that let it take one would starve `c-good` and
            // fail here for a reason of its own.
            ScriptedRunner::new(vec![ProcScript::never_exits(); 2]).failing_to_spawn(&["b-bad"]),
            paths.clone(),
            events,
        );
        let registry = FlockRegistry::new();

        let restored = muster(&paths.snapshot, &registry, &handle).await.unwrap();
        assert_eq!(
            restored,
            vec![
                "a-good".to_string(),
                "b-bad".to_string(),
                "c-good".to_string()
            ]
        );

        let mut listed = handle.list().await;
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        let seen: Vec<(&str, ProcStatus)> =
            listed.iter().map(|i| (i.name.as_str(), i.status)).collect();
        assert_eq!(
            seen,
            vec![
                ("a-good", ProcStatus::Online),
                ("b-bad", ProcStatus::Errored),
                ("c-good", ProcStatus::Online),
            ],
            "every app after the broken one must still get its turn, and the \
             broken one must be visible rather than absent"
        );
        handle.shutdown().await;
    }

    /// fails if ONE saved app that cannot start keeps the rest of the flock
    /// down at the next boot.
    ///
    /// `muster` calls `start_restored`, not `start`, because `start`'s
    /// pre-registration check refuses a whole batch over one app whose script
    /// is gone, which at an unattended boot costs the machine its flock.
    ///
    /// `refusing` is load-bearing: `ScriptedRunner`'s `preflight` answers
    /// `Unknown` for everything, so the validating pass refuses nothing on its
    /// own. One script for two apps, so `gone` also fails at its spawn and has
    /// to land as a visible `Errored` row rather than vanishing.
    #[tokio::test(start_paused = true)]
    async fn one_unstartable_saved_app_does_not_keep_the_rest_of_the_flock_down() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(paths.snapshot.parent().unwrap()).unwrap();
        let roll = FlockSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_ms: 0,
            apps: vec![
                SavedApp {
                    app: AppConfig::minimal("good", "./srv"),
                    instances_running: 1,
                },
                SavedApp {
                    app: AppConfig::minimal("gone", "./deleted-by-a-rebuild"),
                    instances_running: 1,
                },
            ],
        };
        write_atomic(&paths.snapshot, &roll).unwrap();

        let (events, _rx) = crate::bus::test_bus(64);
        let handle = spawn_supervisor(
            ScriptedRunner::new(vec![ProcScript::never_exits()]).refusing(&["gone"]),
            paths.clone(),
            events,
        );
        let registry = FlockRegistry::new();

        let restored = muster(&paths.snapshot, &registry, &handle).await.unwrap();
        assert_eq!(restored, vec!["good".to_string(), "gone".to_string()]);

        let mut listed = handle.list().await;
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        let seen: Vec<(&str, ProcStatus)> =
            listed.iter().map(|i| (i.name.as_str(), i.status)).collect();
        assert_eq!(
            seen,
            vec![("gone", ProcStatus::Errored), ("good", ProcStatus::Online)],
            "the app that could still run must come up, and the one that \
             could not must be visible rather than absent"
        );
        handle.shutdown().await;
    }

    /// fails if `muster` starts an app the flock already has.
    ///
    /// `instance_slots` hands a second `Start` of a one-instance app the next
    /// free slot, so an unconditional muster would leave it running two. The
    /// single script makes that visible: the flock's own `web` consumes it, so
    /// a duplicate lands as `Errored`.
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

        let (events, _rx) = crate::bus::test_bus(64);
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
    /// none, and a daemon that refused over it could not boot on a clean
    /// machine.
    #[tokio::test(start_paused = true)]
    async fn a_missing_roll_restores_nothing_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(paths.snapshot.parent().unwrap()).unwrap();
        assert!(!paths.snapshot.exists());

        let (events, _rx) = crate::bus::test_bus(64);
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
        let (events, _keep) = crate::bus::test_bus(64);
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
                .send(
                    BusEvent::Process {
                        event,
                        info: info(0, "web", ProcStatus::Online),
                        manually: false,
                        at_ms: 0,
                    }
                    .into(),
                )
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
        let (events, _keep) = crate::bus::test_bus(64);
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
                .send(
                    BusEvent::LogOut {
                        id,
                        line: "chatty".to_string(),
                    }
                    .into(),
                )
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(SNAPSHOT_DEBOUNCE_MS * 4)).await;
        assert_eq!(writer.writes(), 0, "log lines must never rewrite the roll");
        assert!(!paths.snapshot.exists());
        writer.stop().await;
    }
}
