//! The filesystem-watch subsystem (spec §4).
//!
//! [`source`] bridges notify's debounced events onto a tokio channel.
//! [`WatchFilter`] decides which delivered paths trigger a restart, and
//! [`spawn_watch_group`] runs one name-group's restart loop over them,
//! single-flighted like [`crate::cron`]'s.
//!
//! A triggering change restarts every instance of the name, stopped ones
//! included: disarming a sheep's watch, not filtering the restart, is what
//! keeps it down. A rescan bypasses both glob sets and always restarts.
//! `watch = true` requires `cwd`; the debounce runs on notify's own OS
//! thread, so a paused clock in tests never moves it.

pub mod source;

use core::fmt;
use core::time::Duration;

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use tokio::sync::mpsc;

use shep_core::selector::ProcessSelector;

use crate::supervisor::{SupervisorError, SupervisorHandle};
use crate::watch::source::{WatchBatch, WatchError, watch_tree};

/// Debounce window when an app sets no `watch_delay`.
///
/// Long enough to coalesce the multi-event burst a single editor save
/// produces (write to a temp file, rename over the target, chmod), short
/// enough that a save-to-restart round trip still feels immediate.
pub(crate) const DEFAULT_WATCH_DELAY: Duration = Duration::from_millis(500);

/// Floor the watch arming enforces on an app's own `watch_delay`.
///
/// Nothing upstream stops a caller from handing [`spawn_watch_group`] a
/// zero, and `notify-debouncer-full` derives its poll tick as `delay / 4`:
/// at zero that spins the debouncer thread in a tight sleep loop.
///
/// One millisecond, not the full second [`crate::cron::MIN_MAX_SLEEP`]
/// uses: this is a debounce, not a polling period, so a floor that high
/// would noticeably lengthen a save-to-restart round trip.
pub(crate) const MIN_WATCH_DELAY: Duration = Duration::from_millis(1);

/// Paths ignored by every watch, before `ignore_watch` is even consulted.
///
/// Dot-entries cover editor swap files and `.git`'s own churn. The
/// `logs`/`pids` entries match a root-relative path, so they cover only a
/// `logs/` or `pids/` directory the user keeps inside the watched tree;
/// they do not cover shep's own log writes, which is what
/// `own_log_ignores` is for.
const DEFAULT_IGNORE_GLOBS: &[&str] = &[
    "**/.*",
    "**/.*/**",
    "**/node_modules/**",
    "**/logs/**",
    "**/pids/**",
];

/// Pattern standing in for "no `watch_options` configured": matches every
/// relative path, so an app that names none is filtered by the default
/// ignores alone.
const MATCH_EVERYTHING: &str = "**";

/// Ignore patterns covering a sheep's own log files: one per path in `logs`
/// that lies under `root`, nothing for the ones that don't.
///
/// [`DEFAULT_IGNORE_GLOBS`] cannot cover an explicit `out_file`/`err_file`
/// under an app's own `cwd`: unignored, it loops forever, since an
/// automatic restart resets `max_restarts` rather than spending it.
///
/// Each path is canonicalized through its parent before stripping `root`,
/// since an app's `cwd` need not already be canonical (macOS resolves
/// `/var/…` to `/private/var/…`).
pub(crate) fn own_log_ignores<'a>(
    root: &Path,
    logs: impl IntoIterator<Item = &'a Path>,
) -> Vec<String> {
    logs.into_iter()
        .filter_map(|log| literal_glob_under(root, log))
        .collect()
}

/// One path's root-relative form as a glob matching it and nothing else, or
/// `None` when it does not lie under `root` (the ordinary case, since the
/// default log paths live in `$SHEP_HOME`) or cannot be spelled as a pattern.
fn literal_glob_under(root: &Path, path: &Path) -> Option<String> {
    let relative = canonical_parent_of(path);
    let relative = relative.strip_prefix(root).ok()?;
    // Assembled component by component rather than from `to_str`, because a
    // glob's separator is `/` on every platform while a Windows path spells it
    // `\`. `escape` then makes each component match LITERALLY: a log file whose
    // name contains `[` or `*` is a filename, not a pattern.
    let mut pattern = String::new();
    for component in relative.iter() {
        if !pattern.is_empty() {
            pattern.push('/');
        }
        pattern.push_str(&globset::escape(component.to_str()?));
    }
    (!pattern.is_empty()).then_some(pattern)
}

/// `path` with its PARENT canonicalized and its file name left alone.
///
/// The file itself may not exist yet: a re-arm happens before the
/// respawned child has written a byte, while its directory does by the
/// time a spawn succeeds. Falls back to `path` untouched when even the
/// parent will not resolve.
fn canonical_parent_of(path: &Path) -> PathBuf {
    let (Some(parent), Some(file)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    std::fs::canonicalize(parent).map_or_else(|_| path.to_path_buf(), |dir| dir.join(file))
}

/// Decides whether a changed path should trigger a restart.
#[derive(Debug)]
pub struct WatchFilter {
    include: GlobSet,
    ignore: GlobSet,
}

impl WatchFilter {
    /// Builds the filter from an app's `watch_options` and `ignore_watch`.
    ///
    /// An empty `watch_options` matches every path; the default ignores
    /// always apply on top of `ignore_watch`.
    ///
    /// # Errors
    ///
    /// - [`WatchFilterError::Glob`]: a pattern the globset crate rejected,
    ///   carrying the pattern and the reason.
    pub fn new(
        watch_options: &[String],
        ignore_watch: &[String],
    ) -> Result<Self, WatchFilterError> {
        let include_patterns: Vec<String> = if watch_options.is_empty() {
            vec![MATCH_EVERYTHING.to_string()]
        } else {
            watch_options.to_vec()
        };
        let ignore_patterns: Vec<String> = DEFAULT_IGNORE_GLOBS
            .iter()
            .map(|pattern| (*pattern).to_string())
            .chain(ignore_watch.iter().cloned())
            .collect();

        Ok(Self {
            include: build_glob_set(&include_patterns)?,
            ignore: build_glob_set(&ignore_patterns)?,
        })
    }

    /// Whether `path`, relative to the watch root, triggers a restart.
    #[must_use]
    pub fn triggers(&self, path: &Path) -> bool {
        self.include.is_match(path) && !self.ignore.is_match(path)
    }
}

/// Compiles `patterns` into one [`GlobSet`], attributing a rejected pattern
/// to itself rather than reporting globset's own aggregate failure.
fn build_glob_set(patterns: &[String]) -> Result<GlobSet, WatchFilterError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|err| WatchFilterError::Glob {
            pattern: pattern.clone(),
            reason: err.to_string(),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|err| {
        // Unreachable in practice: every `Glob` above already parsed, and
        // globset's set-compilation step has no further way to reject an
        // already-valid pattern list. Returned as an error rather than
        // unwrapped, so a future violation fails loudly instead of panicking.
        WatchFilterError::Glob {
            pattern: patterns.join(", "),
            reason: err.to_string(),
        }
    })
}

/// Why a watch filter could not be built.
///
/// One variant, no `#[non_exhaustive]`: the only way construction fails is
/// a pattern globset rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchFilterError {
    /// A `watch_options` or `ignore_watch` pattern globset rejected.
    /// Carries the pattern as the user wrote it and globset's rendered
    /// reason.
    Glob {
        /// The pattern as written in the Flockfile.
        pattern: String,
        /// globset's own rendered reason.
        reason: String,
    },
}

impl fmt::Display for WatchFilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Glob { pattern, reason } => {
                write!(f, "invalid watch pattern `{pattern}`: {reason}")
            }
        }
    }
}

impl core::error::Error for WatchFilterError {}

/// A [`WatchFilter`] paired with the root its `triggers` calls are
/// relative to.
#[derive(Debug)]
struct RootedFilter {
    root: PathBuf,
    filter: WatchFilter,
}

impl RootedFilter {
    /// Whether `path`, an absolute path exactly as notify delivered it,
    /// triggers a restart: strips `root`, then asks `filter`.
    ///
    /// A path outside `root` never triggers, rather than falling back to
    /// matching the untouched absolute form. `root` itself never triggers
    /// either, since it strips to an empty relative path; this matters
    /// because macOS can deliver a spurious `Create(Folder)` for the root
    /// the instant a watch arms.
    fn triggers(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return false;
        }
        self.filter.triggers(relative)
    }
}

/// Runs one name-group's watch until the returned handle is aborted.
///
/// `root` must already be canonicalized, from the app's own `cwd`. Must run
/// inside a Tokio runtime: it spawns the group loop immediately.
///
/// A triggering change restarts every instance of the name; the last instance
/// stopping disarms the group. A rescan restarts regardless of `watch_options`/`ignore_watch`.
///
/// # Errors
///
/// - [`WatchError::Backend`]: notify could not create a watcher.
/// - [`WatchError::Watch`]: notify could not watch `root`.
pub fn spawn_watch_group(
    name: String,
    root: PathBuf,
    filter: WatchFilter,
    delay: Duration,
    supervisor: SupervisorHandle,
) -> Result<tokio::task::JoinHandle<()>, WatchError> {
    let (source, rx) = watch_tree(&root, delay)?;
    let filter = RootedFilter { root, filter };
    let handle = tokio::spawn(async move {
        // The guard lives in the task, not in this function: aborting the
        // handle drops the future and therefore the guard, which is what
        // stops the OS watch, not just the loop.
        let _source = source;
        run_group(name, filter, rx, supervisor).await;
    });
    Ok(handle)
}

/// The group loop: filters each debounced batch and single-flights a
/// group-wide restart through [`SupervisorHandle::restart_automatic`], so
/// an operator's `stop` racing a watch-triggered restart wins rather than
/// being converted into it.
///
/// No dirty flag: the channel's own buffering is the re-check mechanism.
/// A batch arriving mid-restart stays queued, and the next iteration
/// drains whatever accumulated into one combined check, so a backlog
/// produces one restart, not one per queued send.
async fn run_group(
    name: String,
    filter: RootedFilter,
    mut rx: mpsc::UnboundedReceiver<WatchBatch>,
    supervisor: SupervisorHandle,
) {
    loop {
        let Some(mut batch) = rx.recv().await else {
            return; // the source is gone: WatchSource dropped, or its debouncer thread exited
        };
        // Drain whatever else is already queued, batches that arrived
        // while the previous restart was in flight, into this same check.
        while let Ok(more) = rx.try_recv() {
            batch.paths.extend(more.paths);
            batch.rescan |= more.rescan;
        }
        // A rescan is checked ahead of the glob sets: it is not a path, so
        // there is nothing for either list to match against. Restarting is
        // the conservative reading; the alternative is a watch that goes
        // quiet precisely when it knows least.
        if !batch.rescan && !batch.paths.iter().any(|path| filter.triggers(path)) {
            continue;
        }
        match supervisor
            .restart_automatic(ProcessSelector::Name(name.clone()))
            .await
        {
            Ok(_) => {}
            Err(SupervisorError::NotFound) => {
                // The sheep is gone but the registry has not disarmed this
                // group yet: a race with disarm, not a fault, and the
                // disarm is moments away.
                tracing::debug!(name, "watch fired but no sheep by this name is registered");
            }
            Err(err @ SupervisorError::SpawnFailed(_)) => {
                tracing::warn!(name, %err, "watch-triggered restart failed to spawn");
            }
            Err(
                err @ (SupervisorError::ReopenFailed(_)
                | SupervisorError::FlushFailed(_)
                | SupervisorError::ReloadInFlight(_)
                | SupervisorError::InvalidScale(_)
                | SupervisorError::CannotStart(_)),
            ) => {
                // A restart touches no log files, starts no reload, scales
                // nothing and registers no batch, so none of these five can
                // arrive here. Named rather than swept into a catch-all, so
                // a variant this path can produce still fails to compile.
                tracing::warn!(name, %err, "watch-triggered restart reported an unrelated failure");
            }
            Err(err @ SupervisorError::EngineStopped) => {
                tracing::warn!(name, %err, "supervisor engine has shut down; watch worker ending");
                return;
            }
        }
    }
}

/// The real-time constants shared by every real-filesystem test suite
/// in this crate: [`source`]'s smoke tests, this module's own case for
/// [`spawn_watch_group`], and the extras registry's arm/disarm case.
///
/// One owner rather than a copy per suite, since every one of them
/// drives the same debouncer at the same delay. `TEST_DELAY` and
/// `NO_EVENT_WINDOW` are load-bearing together; see the assertion at the
/// top of `source`'s tests.
#[cfg(test)]
pub(crate) mod real_time {
    use core::time::Duration;

    /// Debounce window for every real-filesystem test in this crate: tens of
    /// milliseconds, so a real save-to-batch round trip finishes fast without
    /// accidentally coalescing writes a test means to keep distinct.
    pub(crate) const TEST_DELAY: Duration = Duration::from_millis(50);

    /// How long a test waits for something expected to arrive: a delivered
    /// batch, or a watch-triggered restart. Generous enough that a loaded
    /// CI runner's real inotify/FSEvents latency cannot turn a genuine pass
    /// into a flaky timeout.
    pub(crate) const SMOKE_DEADLINE: Duration = Duration::from_secs(5);

    /// How long a test waits for something that must not arrive. Short on
    /// purpose: this window is a cost every passing run of such a test
    /// pays, and it exists only to prove a negative. Generous enough that a
    /// real event has time to land; short enough not to make a green suite
    /// slow.
    pub(crate) const NO_EVENT_WINDOW: Duration = Duration::from_millis(500);
}

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;

    use super::*;
    use crate::bus::SharedEvent;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use crate::testing::test_paths;
    use shep_core::config::{AppConfig, normalize};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
    use shep_core::status::ProcStatus;
    use shep_core::values::UpDuration;

    // ------------------------------------------------------------------
    // `WatchFilter` and the root-relative boundary: pure, no tokio, no
    // filesystem.
    // ------------------------------------------------------------------

    #[test]
    fn empty_watch_options_matches_every_path() {
        let filter = WatchFilter::new(&[], &[]).unwrap();
        assert!(filter.triggers(Path::new("top.txt")));
        assert!(filter.triggers(Path::new("src/a/b.rs")));
    }

    #[test]
    fn an_explicit_pattern_matches_its_own_tree_and_nothing_else() {
        let filter = WatchFilter::new(&["src/**/*.rs".to_string()], &[]).unwrap();
        assert!(filter.triggers(Path::new("src/a/b.rs")));
        assert!(!filter.triggers(Path::new("src/a/b.txt")));
        assert!(!filter.triggers(Path::new("other/a.rs")));
    }

    // Uses the literal glob a user would write, to stay distinguishable
    // from the empty-`watch_options` case.
    #[test]
    fn default_ignores_beat_an_explicit_include() {
        let filter = WatchFilter::new(&["**".to_string()], &[]).unwrap();
        assert!(!filter.triggers(Path::new(".git/index")));
        assert!(!filter.triggers(Path::new("node_modules/x/y.js")));
    }

    #[test]
    fn an_ignore_watch_entry_beats_an_include() {
        let filter = WatchFilter::new(&["**".to_string()], &["dist/**".to_string()]).unwrap();
        assert!(!filter.triggers(Path::new("dist/bundle.js")));
        // Control: only the `dist` tree is excluded, not everything.
        assert!(filter.triggers(Path::new("src/main.rs")));
    }

    #[test]
    fn a_pattern_matching_nothing_never_triggers() {
        let filter = WatchFilter::new(&["nomatch/**/*.foo".to_string()], &[]).unwrap();
        assert!(!filter.triggers(Path::new("src/main.rs")));
    }

    #[test]
    fn an_invalid_glob_is_rejected_with_its_pattern() {
        let err = WatchFilter::new(&["[".to_string()], &[]).unwrap_err();
        let WatchFilterError::Glob { pattern, reason } = err;
        assert_eq!(pattern, "[");
        assert!(!reason.is_empty());

        let _: &dyn core::error::Error = &WatchFilterError::Glob {
            pattern: "[".to_string(),
            reason: "boom".to_string(),
        };
    }

    // Pins the rendered text: the case above destructures the variant and
    // never renders it.
    #[test]
    fn watch_filter_error_display_names_the_pattern_and_its_reason() {
        // A fabricated reason rather than globset's own, so the assertion is
        // an exact string and not a re-statement of whatever that crate
        // happens to render this release.
        let err = WatchFilterError::Glob {
            pattern: "[".to_string(),
            reason: "unclosed character class".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid watch pattern `[`: unclosed character class"
        );
    }

    #[test]
    fn a_path_outside_the_root_never_triggers() {
        let filter = RootedFilter {
            root: PathBuf::from("/watched"),
            filter: WatchFilter::new(&[], &[]).unwrap(),
        };
        assert!(!filter.triggers(Path::new("/elsewhere/file.rs")));
        // Control: the same filter, under the root, does trigger.
        assert!(filter.triggers(Path::new("/watched/file.rs")));
    }

    // The root triggering ahead of both glob sets cannot be right on macOS,
    // which delivers a spurious `Create(Folder)` the instant a watch arms.
    #[test]
    fn the_root_itself_never_triggers_however_wide_the_watch_options() {
        // The widest include there is, so a failure here is about the root
        // and not about patterns that happened not to match it.
        let filter = matches_everything(PathBuf::from("/watched"));
        assert!(!filter.triggers(Path::new("/watched")));
        // Control: the same filter, one level in, does trigger, so the
        // case above is about the root itself rather than about a filter
        // that matches nothing.
        assert!(filter.triggers(Path::new("/watched/other/a.txt")));
    }

    // fails if the path reaches globset unescaped: `app[0].log` would
    // become a character class matching `app0.log`.
    #[test]
    fn own_log_ignores_covers_only_the_paths_under_the_root() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // Canonical, as `arm_watch` hands it over: the raw tempdir path is
        // `/var/…` on macOS where its resolved form is `/private/var/…`.
        let canonical = std::fs::canonicalize(root.path()).unwrap();

        let inside = root.path().join("app[0].log");
        let outside = elsewhere.path().join("web-0-out.log");
        let ignores = own_log_ignores(&canonical, [inside.as_path(), outside.as_path()]);

        assert_eq!(ignores, vec!["app[[]0[]].log".to_string()]);
        let filter = WatchFilter::new(&[], &ignores).unwrap();
        assert!(!filter.triggers(Path::new("app[0].log")));
        // Controls: the escape matches that name and not the class it would
        // otherwise have spelled, and an unrelated sibling still triggers.
        assert!(filter.triggers(Path::new("app0.log")));
        assert!(filter.triggers(Path::new("src/main.rs")));
    }

    // ------------------------------------------------------------------
    // The group loop: paused clock, driven by a hand-fed channel.
    // ------------------------------------------------------------------

    /// Generous bound on how long a paused-clock test waits for a restart
    /// before concluding the loop is broken. Costs no real wall-clock time:
    /// auto-advance walks straight to it once nothing else is ready.
    const EVENT_WAIT: Duration = Duration::from_secs(30);

    /// How many `tokio::task::yield_now` rounds [`settle`] spends: headroom
    /// for the group loop, the actor and a sheep's task each needing a
    /// scheduling turn. Never advances the paused clock itself.
    const SETTLE_YIELDS: usize = 16;

    async fn settle() {
        for _ in 0..SETTLE_YIELDS {
            tokio::task::yield_now().await;
        }
    }

    /// A supervisor engine over a scripted runner, its bus receiver, and
    /// tempdir.
    fn spawn_test_fixture(
        scripts: Vec<ProcScript>,
    ) -> (
        SupervisorHandle,
        broadcast::Receiver<SharedEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (events, rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(scripts);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        (handle, rx, dir)
    }

    async fn start_app(handle: &SupervisorHandle, name: &str, instances: u32) -> Vec<ProcessInfo> {
        let mut app = AppConfig::minimal(name, "./srv");
        app.instances = instances;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap()
    }

    /// Waits up to `deadline` for the next `Restart` for `name`; times out
    /// rather than hanging.
    async fn expect_restart(
        rx: &mut broadcast::Receiver<SharedEvent>,
        name: &str,
        deadline: Duration,
    ) -> ProcessInfo {
        loop {
            match tokio::time::timeout(deadline, rx.recv())
                .await
                .map(|received| received.map(|event| event.to_event()))
            {
                Ok(Ok(BusEvent::Process {
                    event: ProcessEventKind::Restart,
                    info,
                    ..
                })) if info.name == name => return info,
                Ok(Ok(_)) => continue,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(err)) => panic!("event stream closed before a restart of {name}: {err}"),
                Err(_) => panic!("timed out waiting for a watch-triggered restart of {name}"),
            }
        }
    }

    /// Waits up to `window` for a `Restart` for `name`, panicking if one
    /// arrives. A real poll, not a bare `try_recv`: a restart working its
    /// way through the loop, actor and sheep-task round trip needs the
    /// scheduling rounds a bounded `recv` gives it.
    async fn assert_no_restart_within(
        rx: &mut broadcast::Receiver<SharedEvent>,
        name: &str,
        window: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv())
                .await
                .map(|received| received.map(|event| event.to_event()))
            {
                Err(_) => return, // window elapsed with nothing matching, expected
                Ok(Ok(BusEvent::Process {
                    event: ProcessEventKind::Restart,
                    info,
                    ..
                })) if info.name == name => {
                    panic!(
                        "unexpected watch-triggered restart of {name} observed (restarts={})",
                        info.restarts
                    );
                }
                Ok(Ok(_)) => continue,
                // A negative assertion cannot skip events: a dropped one
                // may be the very `Restart` this forbids, so it must fail
                // loudly instead of failing open.
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    panic!(
                        "event stream lagged by {skipped} while checking for no restart of \
                         {name}: a skipped event may have been the restart this forbids"
                    )
                }
                Ok(Err(err)) => {
                    panic!("event channel closed while checking for no restart of {name}: {err}")
                }
            }
        }
    }

    fn matches_everything(root: PathBuf) -> RootedFilter {
        RootedFilter {
            root,
            filter: WatchFilter::new(&[], &[]).unwrap(),
        }
    }

    /// Builds a batch with `rescan: false`.
    fn changed(paths: Vec<PathBuf>) -> WatchBatch {
        WatchBatch {
            paths,
            rescan: false,
        }
    }

    /// A rescan in its path-less (inotify) shape: notify dropped events and
    /// wants the tree re-read. `source`'s own tests cover the macOS shape,
    /// which carries the root alongside the flag.
    fn rescan_marker() -> WatchBatch {
        WatchBatch {
            paths: Vec::new(),
            rescan: true,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_batch_of_only_ignored_paths_produces_no_restart() {
        // Two scripts, not one: `start_app` consumes the first, so a
        // filter-bypassing implementation needs a second for its respawn.
        // With one, that respawn would report `Errored`, invisible to
        // `assert_no_restart_within`, and the mutation would pass by accident.
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 2]);
        let name = "web";
        start_app(&handle, name, 1).await;
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        tx.send(changed(vec![root.join(".git/index")])).unwrap();
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        group.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_batch_with_one_triggering_path_produces_exactly_one_restart() {
        // Three scripts: one for `start_app`, one for the expected restart,
        // and a third so a double-firing implementation can spawn and emit
        // a second `Restart`. With only two, it would report `Errored`
        // instead, and the trailing negative below could not fail.
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 3]);
        let name = "web";
        start_app(&handle, name, 1).await;
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        tx.send(changed(vec![root.join("src/main.rs")])).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        group.abort();
    }

    // fails if a rescan is filtered like an ordinary path: on Linux it
    // carries no path at all, so consulting the glob sets leaves the watch
    // deaf exactly when notify has already lost events. Two scripts: one
    // for `start_app`, one for the restart this expects.
    #[tokio::test(start_paused = true)]
    async fn a_rescan_restarts_under_a_non_matching_watch_options() {
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 2]);
        let name = "web";
        start_app(&handle, name, 1).await;
        let root = PathBuf::from("/watched");
        let filter = RootedFilter {
            root,
            filter: WatchFilter::new(&["src/**/*.rs".to_string()], &[]).unwrap(),
        };
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            filter,
            group_rx,
            handle.clone(),
        ));

        tx.send(rescan_marker()).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);

        group.abort();
    }

    // The loop's rescan check runs before the filter, so a loop that read
    // the root itself as a rescan signal would restart here while every
    // filter-tier assertion stayed green.
    #[tokio::test(start_paused = true)]
    async fn an_ordinary_event_on_the_root_itself_produces_no_restart() {
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 2]);
        let name = "web";
        start_app(&handle, name, 1).await;
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        // The root, with no rescan flag on it: a `chmod` of that inode, or
        // FSEvents' arm-time `Create(Folder)`.
        tx.send(changed(vec![root.clone()])).unwrap();
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        // Control: the same loop, the same watch, one level in, so the
        // silence above is the root being filtered rather than the loop
        // having simply stopped restarting.
        tx.send(changed(vec![root.join("src/main.rs")])).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);

        group.abort();
    }

    // The two sends are made back to back with no `settle`, so they are
    // genuinely queued together when the loop next looks.
    #[tokio::test(start_paused = true)]
    async fn a_rescan_queued_behind_an_ignored_batch_survives_the_drain() {
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 2]);
        let name = "web";
        start_app(&handle, name, 1).await;
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        tx.send(changed(vec![root.join(".git/index")])).unwrap();
        tx.send(rescan_marker()).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);

        group.abort();
    }

    // fails two ways: a loop that drops a batch queued during an in-flight
    // restart (only 1 restart total, not 2, no re-check), and a loop that
    // processes each queued send as its own `recv`/restart cycle instead of
    // draining them into one check (3 restarts total, not 2).
    #[tokio::test(start_paused = true)]
    async fn a_batch_queued_during_a_restart_is_rechecked_and_drained_as_one() {
        // Four scripts, not three: a broken implementation that processes
        // `b.rs` and `c.rs` as two separate restarts needs a fourth spawn.
        // With only three, the third attempt would report `Errored` and
        // the mutation would pass by accident.
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::ignores_signals(); 4]);
        let name = "web";
        start_app(&handle, name, 1).await;
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        // Batch 1 kicks off a restart. The scripted process ignores its
        // graceful signal, so the kill ladder is stuck on the full
        // `kill_timeout` until the paused clock actually moves; `settle`
        // only yields, it never advances time.
        tx.send(changed(vec![root.join("a.rs")])).unwrap();
        settle().await;

        // Two more sends land in the queue while restart 1 is still
        // pending.
        tx.send(changed(vec![root.join("b.rs")])).unwrap();
        tx.send(changed(vec![root.join("c.rs")])).unwrap();

        let first = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(first.restarts, 1);
        let second = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(second.restarts, 2);
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        group.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_sender_ends_the_group_task() {
        let (handle, _rx, _dir) = spawn_test_fixture(vec![]);
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            "ghost".to_string(),
            matches_everything(root),
            group_rx,
            handle,
        ));

        drop(tx);

        tokio::time::timeout(EVENT_WAIT, group)
            .await
            .expect("group task did not end after its sender was dropped")
            .expect("group task panicked");
    }

    // fails if the group loop filters by status: the reach is the whole
    // name-group, not just its running instances, pinning this against a
    // reimplementation of the withdrawn per-instance filter.
    #[tokio::test(start_paused = true)]
    async fn a_triggering_batch_restarts_a_stopped_instance_in_the_same_group() {
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 4]);
        let name = "web";
        let infos = start_app(&handle, name, 2).await;
        let stopped_id = infos[1].id;
        handle.stop(ProcessSelector::Id(stopped_id)).await.unwrap();

        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        tx.send(changed(vec![root.join("src/main.rs")])).unwrap();

        let first = expect_restart(&mut rx, name, EVENT_WAIT).await;
        let second = expect_restart(&mut rx, name, EVENT_WAIT).await;
        let stopped_info = [first, second]
            .into_iter()
            .find(|info| info.id == stopped_id)
            .expect("the previously-stopped instance never restarted");
        // `Online` alone would pass against a group that never touched the
        // stopped instance if something else had started it; `restarts`
        // is what makes the claim about this restart.
        assert_eq!(stopped_info.status, ProcStatus::Online);
        assert_eq!(stopped_info.restarts, 1);

        group.abort();
    }

    // Two instances so the restart is provably observed before the stop
    // lands. fails if the loop calls `restart` instead of
    // `restart_automatic`.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_beats_a_watch_triggered_restart_mid_ladder() {
        // Four procs is the most this test can demand: both instances'
        // initial ones, the untouched instance's legitimate respawn, and
        // the respawn a broken implementation performs behind the stop's
        // back. Three would report `Errored` instead of showing `Online`.
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![
            ProcScript::ignores_signals(), // held for the whole 1600ms ladder
            ProcScript::never_exits(),     // exits the moment the ladder signals it
            ProcScript::never_exits(),     // the untouched instance's respawn
            ProcScript::never_exits(),     // the respawn a broken implementation performs
        ]);
        let name = "web";
        let infos = start_app(&handle, name, 2).await;
        let (held, released) = (infos[0].id, infos[1].id);
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        // The batch claims BOTH instances' next exit and starts both kill
        // ladders. Only the second sheep's ladder can finish without the clock
        // moving, so its restart lands while the first is still mid-ladder.
        tx.send(changed(vec![root.join("src/main.rs")])).unwrap();
        let restarted = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(
            (restarted.id, restarted.restarts),
            (released, 1),
            "the batch never reached the actor, so the stop below would race \
             nothing -- got {restarted:?}"
        );
        // Aborted before the stop so no later batch can reach the assertions
        // below. The restart is already in the actor's hands; the dropped
        // reply receiver only means nobody reads the answer.
        group.abort();

        let stopped = handle.stop(ProcessSelector::Id(held)).await.unwrap();
        assert_eq!(stopped.len(), 1);
        assert_eq!(
            (stopped[0].id, stopped[0].status, stopped[0].restarts),
            (held, ProcStatus::Stopped, 0),
            "an operator's stop was silently converted into the watch-triggered \
             restart it raced -- got {stopped:?}"
        );
        let listed = handle.list().await;
        assert_eq!(
            (listed[0].id, listed[0].status, listed[0].pid),
            (held, ProcStatus::Stopped, None),
            "the sheep an operator stopped is running again -- got {listed:?}"
        );
        assert_eq!(
            (listed[1].id, listed[1].status),
            (released, ProcStatus::Online),
            "the instance the operator did not name must still be up, \
             restarted by the batch -- got {listed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn not_found_leaves_the_loop_alive_for_the_next_batch() {
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 2]);
        let name = "ghost";
        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle.clone(),
        ));

        // `name` matches nothing yet: the restart resolves `NotFound`, and
        // the loop must stay alive rather than returning.
        tx.send(changed(vec![root.join("a.rs")])).unwrap();
        assert_no_restart_within(&mut rx, name, Duration::from_millis(200)).await;
        assert!(!group.is_finished(), "the loop must not exit on NotFound");

        // Registering the name for real and sending a second batch: if the
        // earlier `NotFound` had ended the loop, this would time out.
        start_app(&handle, name, 1).await;
        tx.send(changed(vec![root.join("b.rs")])).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);

        group.abort();
    }

    // The loop's other exit: the engine it restarts through, rather than
    // its source going away. fails if the `EngineStopped` arm falls through
    // instead of returning, leaving the group watching forever. No scripts
    // in the fixture: the engine is shut down before the batch is sent.
    #[tokio::test(start_paused = true)]
    async fn the_group_task_ends_when_the_supervisor_engine_has_stopped() {
        let (handle, _rx, _dir) = spawn_test_fixture(Vec::new());
        let name = "web";
        handle.shutdown().await;
        // The premise, stated rather than assumed: with the actor gone, the
        // restart this batch is about to trigger answers `EngineStopped`.
        assert_eq!(
            handle
                .restart_automatic(ProcessSelector::Name(name.to_string()))
                .await
                .unwrap_err(),
            SupervisorError::EngineStopped
        );

        let root = PathBuf::from("/watched");
        let (tx, group_rx) = mpsc::unbounded_channel();
        let group = tokio::spawn(run_group(
            name.to_string(),
            matches_everything(root.clone()),
            group_rx,
            handle,
        ));

        tx.send(changed(vec![root.join("src/main.rs")])).unwrap();
        tokio::time::timeout(EVENT_WAIT, group)
            .await
            .expect("the group task did not end after the engine shut down")
            .expect("group task panicked");
        drop(tx); // kept alive until here: a dropped sender ends the loop too
    }

    // The only case that constructs a real `WatchSource`, exercised by
    // `spawn_watch_group_restarts_on_a_real_touch_and_stops_on_abort`. The
    // touch half catches a guard dropped before the loop sees an event; the
    // abort half only proves the loop stops, not that a leaked guard is caught.

    // A watch root that does not exist, seen from the arming entry point:
    // fails if the arming swallows the failure, or reports `Backend`
    // instead of `Watch` carrying the exact path handed to it. Real time,
    // like every other case here that constructs a watcher.
    #[tokio::test]
    async fn a_watch_root_that_does_not_exist_names_the_path_it_could_not_watch() {
        let (handle, _rx, _dir) = spawn_test_fixture(vec![]);
        // Inside a live tempdir so the parent exists and only the leaf does
        // not: a root whose whole prefix is missing would leave "which
        // component did notify object to" ambiguous.
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("no-such-directory");

        let err = spawn_watch_group(
            "web".to_string(),
            missing.clone(),
            WatchFilter::new(&[], &[]).unwrap(),
            real_time::TEST_DELAY,
            handle,
        )
        .unwrap_err();

        let WatchError::Watch { path, reason } = err else {
            panic!("a root that does not exist must report `Watch`, got {err:?}");
        };
        assert_eq!(path, missing, "`Watch` must carry the root it was handed");
        assert!(!reason.is_empty(), "`Watch` must carry notify's own reason");
    }

    // ------------------------------------------------------------------
    // The single-flight property: the group loop against generated batch
    // sequences and generated restart durations.
    // ------------------------------------------------------------------

    /// Most batches one generated case feeds the group.
    const MAX_BATCHES: usize = 8;

    /// Scripted procs one generated case may spawn.
    ///
    /// Sized against the maximum a broken loop can demand, not a correct
    /// one: the worst case is one restart per batch, plus the initial
    /// start. A pool sized to a correct run would swallow the extra
    /// restarts this property exists to see.
    const SINGLE_FLIGHT_SCRIPTS: usize = MAX_BATCHES + 4;

    /// One generated debounced batch: whether its path triggers a restart,
    /// and how long after the previous send it arrives.
    #[derive(Debug, Clone, Copy)]
    struct Batch {
        triggers: bool,
        gap: Duration,
    }

    fn gap_strategy() -> impl proptest::strategy::Strategy<Value = Duration> {
        use proptest::strategy::Strategy as _; // `prop_map` below
        // Zero (a send landing mid-restart) is drawn half the time. The
        // other two arms straddle the generated kill timeout's own
        // 200..2000ms range, so batches also arrive partway through a
        // ladder and well after one has finished.
        proptest::prop_oneof![
            4 => proptest::strategy::Just(Duration::ZERO),
            3 => (1u64..2_000u64).prop_map(Duration::from_millis),
            1 => (2_000u64..6_000u64).prop_map(Duration::from_millis),
        ]
    }

    fn batch_strategy() -> impl proptest::strategy::Strategy<Value = Batch> {
        use proptest::strategy::Strategy as _; // `prop_map` below
        (proptest::bool::ANY, gap_strategy()).prop_map(|(triggers, gap)| Batch { triggers, gap })
    }

    /// When a correct group loop finishes each restart, given the instants
    /// its batches arrive at and how long one restart takes.
    ///
    /// A strictly sequential model: it holds no notion of two restarts
    /// overlapping, because the loop it models has none. Every arrival
    /// already queued when the loop next looks is folded into one check,
    /// and a restart occupies the model for exactly `restart` from the
    /// moment that check decided to run it.
    fn expected_restart_instants(batches: &[Batch], restart: Duration) -> Vec<Duration> {
        let mut arrivals = Vec::with_capacity(batches.len());
        let mut at = Duration::ZERO;
        for batch in batches {
            at += batch.gap;
            arrivals.push((at, batch.triggers));
        }

        let mut finished = Vec::new();
        let mut idle_since = Duration::ZERO;
        let mut i = 0;
        while i < arrivals.len() {
            // The loop is parked on `recv` and wakes at the first arrival it
            // has not seen, or, if that already happened while it was busy,
            // the moment it became free again.
            let woke_at = arrivals[i].0.max(idle_since);
            let mut triggers = arrivals[i].1;
            i += 1;
            // ...and drains everything else already queued at that instant.
            while i < arrivals.len() && arrivals[i].0 <= woke_at {
                triggers |= arrivals[i].1;
                i += 1;
            }
            idle_since = if triggers {
                let done = woke_at + restart;
                finished.push(done);
                done
            } else {
                woke_at
            };
        }
        finished
    }

    proptest::proptest! {
        // 64 rather than the supervisor proptest's 128: each case here boots
        // a runtime, a supervisor and a group loop and walks virtual time
        // across every generated gap, so a case costs more.
        // `PROPTEST_CASES` still overrides it.
        #![proptest_config(crate::testing::proptest_config(64))]

        // `run_group` awaits each restart before its next `recv`, so single
        // flight falls out of the shape. Every scripted proc ignores its
        // graceful signal, so a restart takes exactly the generated
        // `kill_timeout`: two restarts less than that apart overlapped.
        #[test]
        fn a_watch_group_never_has_two_restarts_in_flight(
            batches in proptest::collection::vec(batch_strategy(), 1..=MAX_BATCHES),
            kill_timeout_ms in 200u64..2_000u64,
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            runtime.block_on(async move {
                let kill_timeout = Duration::from_millis(kill_timeout_ms);
                let (events, mut rx) = crate::bus::test_bus(1024);
                let runner =
                    ScriptedRunner::new(vec![ProcScript::ignores_signals(); SINGLE_FLIGHT_SCRIPTS]);
                let handle = spawn_supervisor(runner, test_paths(&dir), events);
                let name = "web";
                let mut app = AppConfig::minimal(name, "./srv");
                app.kill_timeout = UpDuration::from_millis(kill_timeout_ms);
                handle.start(vec![normalize(app).unwrap()]).await.unwrap();

                let root = PathBuf::from("/watched");
                let (tx, group_rx) = mpsc::unbounded_channel();
                let group = tokio::spawn(run_group(
                    name.to_string(),
                    matches_everything(root.clone()),
                    group_rx,
                    handle.clone(),
                ));

                let start = tokio::time::Instant::now();
                // Drained by its own task, started before the first send:
                // a broadcast send wakes it without moving the paused
                // clock, so the recorded instant is the instant the actor
                // emitted, not the end of some later sleep.
                let watched = name.to_string();
                let collector = tokio::spawn(async move {
                    let mut observed = Vec::new();
                    loop {
                        match tokio::time::timeout(EVENT_WAIT, rx.recv()).await.map(|received| received.map(|event| event.to_event())) {
                            Ok(Ok(BusEvent::Process {
                                event: ProcessEventKind::Restart,
                                info,
                                ..
                            })) if info.name == watched => {
                                observed
                                    .push((tokio::time::Instant::now() - start, info.restarts));
                            }
                            Ok(Ok(_)) => continue,
                            // A claim about overlap cannot skip events: a
                            // dropped one may be the very restart that
                            // overlapped.
                            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                                return Err(skipped);
                            }
                            Ok(Err(broadcast::error::RecvError::Closed)) => break,
                            Err(_elapsed) => break, // the group has gone quiet
                        }
                    }
                    Ok(observed)
                });

                for (i, batch) in batches.iter().enumerate() {
                    if batch.gap > Duration::ZERO {
                        tokio::time::sleep(batch.gap).await;
                    }
                    // `.git/` is in `DEFAULT_IGNORE_GLOBS`, so a non-
                    // triggering batch is a real delivered path the filter
                    // rejects rather than an empty send the loop would never
                    // see.
                    let path = if batch.triggers {
                        root.join(format!("src/f{i}.rs"))
                    } else {
                        root.join(format!(".git/o{i}"))
                    };
                    tx.send(changed(vec![path])).unwrap();
                }

                let observed = match collector.await.expect("collector task panicked") {
                    Ok(observed) => observed,
                    Err(skipped) => {
                        return Err(proptest::test_runner::TestCaseError::fail(format!(
                            "event stream lagged by {skipped}"
                        )));
                    }
                };
                group.abort();

                // The invariant itself, read off the bus: consecutive
                // restarts of one group are never closer together than one
                // restart takes.
                for pair in observed.windows(2) {
                    proptest::prop_assert!(
                        pair[1].0 - pair[0].0 >= kill_timeout,
                        "two restarts of {} finished {:?} apart, less than the {:?} one takes: \
                         they overlapped",
                        name,
                        pair[1].0 - pair[0].0,
                        kill_timeout
                    );
                }

                // ...and the same claim stated positively, against the
                // sequential model: a loop that overlaps restarts, drops a
                // batch, or gives each queued send its own cycle disagrees
                // with it on when, and how many times, it restarted.
                let expected = expected_restart_instants(&batches, kill_timeout);
                let counted: Vec<u32> = (1..=expected.len() as u32).collect();
                proptest::prop_assert_eq!(
                    observed,
                    expected.into_iter().zip(counted).collect::<Vec<_>>()
                );
                Ok::<(), proptest::test_runner::TestCaseError>(())
            })?;
        }
    }

    /// Tests that wait on real filesystem events or real elapsed time.
    ///
    /// The inner loop skips this module with `--skip ::slow::`; the full
    /// suite still runs them because nothing here is `#[ignore]`d.
    mod slow {
        use super::*;

        // fails if the debouncer guard is dropped before the loop ever sees
        // an event: the watch dies inside `spawn_watch_group` and the touch
        // below produces no restart. The abort half only proves the loop
        // stops, not that a leak is caught.
        #[tokio::test]
        async fn spawn_watch_group_restarts_on_a_real_touch_and_stops_on_abort() {
            let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits(); 2]);
            let name = "web";
            start_app(&handle, name, 1).await;

            let watch_dir = tempfile::tempdir().unwrap();
            let root = watch_dir.path().canonicalize().unwrap();
            let filter = WatchFilter::new(&[], &[]).unwrap();
            let group = spawn_watch_group(
                name.to_string(),
                root.clone(),
                filter,
                real_time::TEST_DELAY,
                handle.clone(),
            )
            .unwrap();

            crate::testing::touch(&root, "trigger.txt").unwrap();
            let info = expect_restart(&mut rx, name, real_time::SMOKE_DEADLINE).await;
            assert_eq!(info.restarts, 1);

            group.abort();
            crate::testing::touch(&root, "after-abort.txt").unwrap();
            assert_no_restart_within(&mut rx, name, real_time::NO_EVENT_WINDOW).await;
        }
    }
}
