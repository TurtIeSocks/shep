//! The filesystem-watch subsystem (spec §4).
//!
//! [`source`] is the OS seam: it bridges notify's debounced filesystem
//! events onto a tokio channel. [`WatchFilter`] decides which of those
//! delivered paths matter, and [`spawn_watch_group`] runs one name-group's
//! restart loop over them — single-flighted and re-checked, exactly like
//! [`crate::cron`]'s restart loop.
//!
//! A triggering change restarts the whole name-group, stopped instances
//! included — the same reach [`crate::cron`]'s schedule has. What keeps a
//! stopped sheep down is disarming its group's watcher the moment the last
//! instance of its name stops, never a filter on the restart itself; the
//! extras registry owns that disarm.
//!
//! ## Reference
//!
//! - [`source::WatchSource`], [`source::watch_tree`], [`source::WatchError`]
//! - [`WatchFilter`], [`WatchFilterError`], [`spawn_watch_group`]

pub mod source;

use core::fmt;
use core::time::Duration;

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use tokio::sync::mpsc;

use shep_core::selector::ProcessSelector;

use crate::supervisor::{SupervisorError, SupervisorHandle};
use crate::watch::source::{WatchError, watch_tree};

/// Debounce window when an app sets no `watch_delay`.
///
/// Spec §4's default. Long enough to coalesce the multi-event burst a
/// single editor save produces (write to a temp file, rename over the
/// target, chmod), short enough that a save-to-restart round trip still
/// feels immediate.
///
/// Not read by any non-test code path yet: choosing between this default
/// and a configured `watch_delay` is the extras registry's job — it only
/// owns the constant, not the call site that picks between it and a
/// configured value. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing here needs yet.
#[allow(dead_code)]
pub(crate) const DEFAULT_WATCH_DELAY: Duration = Duration::from_millis(500);

/// Paths ignored by every watch, before `ignore_watch` is even consulted.
///
/// Dot-entries cover editor swap files and `.git`'s own churn — a `git
/// status` would otherwise restart the flock. The log and pid directories
/// are shep's own writes, and watching them makes every restart trigger the
/// next one.
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
    /// - [`WatchFilterError::Glob`] — a pattern the globset crate rejected,
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

    /// Whether `path` — relative to the watch root — triggers a restart.
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
        // Unreachable in practice: every `Glob` above already parsed
        // successfully on its own, and globset's set-compilation step has
        // no further way to reject an already-valid pattern list. Handled
        // instead of `.expect()`-ed so a change in that guarantee fails
        // this function's own contract loudly rather than panicking
        // (IR-21).
        WatchFilterError::Glob {
            pattern: patterns.join(", "),
            reason: err.to_string(),
        }
    })
}

/// Why a watch filter could not be built.
///
/// One variant and no `#[non_exhaustive]`: the only way this construction
/// fails is a pattern globset rejects (IR-20).
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

/// A [`WatchFilter`] paired with the root its `triggers` calls are relative
/// to — the two travel together once a group loop is running, since every
/// path notify delivers needs both to answer "does this trigger a
/// restart".
#[derive(Debug)]
struct RootedFilter {
    root: PathBuf,
    filter: WatchFilter,
}

impl RootedFilter {
    /// Whether `path` — an absolute path exactly as notify delivered it —
    /// triggers a restart: strips `root`, then asks `filter`.
    ///
    /// A path that does not lie under `root` never triggers, rather than
    /// falling back to matching the untouched absolute form against
    /// patterns written for relative ones. The OS should not deliver one —
    /// `root` is the tree being watched — but a symlinked subtree inside it
    /// can (see [`source::watch_tree`]'s own doc on resolved-vs-literal
    /// paths).
    fn triggers(&self, path: &Path) -> bool {
        match path.strip_prefix(&self.root) {
            Ok(relative) => self.filter.triggers(relative),
            Err(_) => false,
        }
    }
}

/// Runs one name-group's watch until the returned handle is aborted.
///
/// `root` is an already-canonicalized absolute directory. It comes from the
/// app's own `cwd`, which config validation requires whenever `watch` is
/// on, and never from the daemon's working directory. Aborting the handle
/// stops the OS watch as well as the loop, because the debouncer guard
/// lives inside the spawned future.
///
/// A triggering change restarts every instance of the name. Stopping a
/// sheep is what stops its watch: the last instance of a name going away
/// disarms this group, so a stopped sheep has no watcher left to restart
/// it.
///
/// # Errors
///
/// - [`WatchError::Backend`] — notify could not create a watcher,
///   propagated from [`watch_tree`].
/// - [`WatchError::Watch`] — notify could not watch `root`, carrying the
///   path.
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
        // The guard lives in the task, not in this function: dropping it
        // stops the OS watch, so its lifetime has to be the loop's
        // lifetime. Aborting the handle drops the future and therefore the
        // guard, which is what makes disarm-by-abort actually stop the
        // watch rather than only the loop.
        let _source = source;
        run_group(name, filter, rx, supervisor).await;
    });
    Ok(handle)
}

/// The group loop: filters each debounced batch and single-flights a
/// group-wide restart through [`SupervisorHandle::restart`].
///
/// No dirty flag, no state machine (IR-31): the channel's own buffering is
/// the re-check mechanism. A batch that arrives while a restart is in
/// flight simply stays queued; the next iteration drains whatever
/// accumulated — one send or several — into a single combined check before
/// deciding whether to restart again, so a backlog produces one restart,
/// not one per queued send. Because the restart is always awaited before
/// the next receive, two restarts can never be in flight for the same
/// group.
async fn run_group(
    name: String,
    filter: RootedFilter,
    mut rx: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    supervisor: SupervisorHandle,
) {
    loop {
        let Some(mut batch) = rx.recv().await else {
            return; // the source is gone: WatchSource dropped, or its debouncer thread exited
        };
        // Drain whatever else is already queued — batches that arrived
        // while the *previous* restart (if any) was in flight — into this
        // same check.
        while let Ok(more) = rx.try_recv() {
            batch.extend(more);
        }
        if !batch.iter().any(|path| filter.triggers(path)) {
            continue;
        }
        match supervisor
            .restart(ProcessSelector::Name(name.clone()))
            .await
        {
            Ok(_) => {}
            Err(SupervisorError::NotFound) => {
                // The sheep is gone but the registry has not disarmed this
                // group yet — a race with disarm, not a fault, and the
                // disarm is moments away.
                tracing::debug!(name, "watch fired but no sheep by this name is registered");
            }
            Err(err @ SupervisorError::SpawnFailed(_)) => {
                tracing::warn!(name, %err, "watch-triggered restart failed to spawn");
            }
            Err(err @ SupervisorError::EngineStopped) => {
                tracing::warn!(name, %err, "supervisor engine has shut down; watch worker ending");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;

    use super::*;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::supervisor::spawn_supervisor;
    use crate::testing::test_paths;
    use shep_core::config::{AppConfig, normalize};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
    use shep_core::status::ProcStatus;

    // ------------------------------------------------------------------
    // Step 1: `WatchFilter` and the root-relative boundary — pure, no
    // tokio, no filesystem (IR-40).
    // ------------------------------------------------------------------

    // fails if an empty `watch_options` is treated as "matches nothing"
    // (e.g. an empty `GlobSet` built with no `MATCH_EVERYTHING` fallback)
    #[test]
    fn empty_watch_options_matches_every_path() {
        let filter = WatchFilter::new(&[], &[]).unwrap();
        assert!(filter.triggers(Path::new("top.txt")));
        assert!(filter.triggers(Path::new("src/a/b.rs")));
    }

    // fails if `*.rs` is matched without path anchoring (would also match
    // `other/a.rs`), or if the glob is treated as a literal path
    #[test]
    fn an_explicit_pattern_matches_its_own_tree_and_nothing_else() {
        let filter = WatchFilter::new(&["src/**/*.rs".to_string()], &[]).unwrap();
        assert!(filter.triggers(Path::new("src/a/b.rs")));
        assert!(!filter.triggers(Path::new("src/a/b.txt")));
        assert!(!filter.triggers(Path::new("other/a.rs")));
    }

    // fails if the default ignore set is only consulted when `ignore_watch`
    // is non-empty — the bug that makes every app with custom
    // `watch_options` restart on its own log writes
    #[test]
    fn default_ignores_beat_an_explicit_include() {
        let filter = WatchFilter::new(&[MATCH_EVERYTHING.to_string()], &[]).unwrap();
        assert!(!filter.triggers(Path::new(".git/index")));
        assert!(!filter.triggers(Path::new("node_modules/x/y.js")));
    }

    // fails if `ignore_watch` patterns are never merged into the ignore set
    #[test]
    fn an_ignore_watch_entry_beats_an_include() {
        let filter =
            WatchFilter::new(&[MATCH_EVERYTHING.to_string()], &["dist/**".to_string()]).unwrap();
        assert!(!filter.triggers(Path::new("dist/bundle.js")));
        // Control: only `dist/**` is excluded, not everything.
        assert!(filter.triggers(Path::new("src/main.rs")));
    }

    // fails if a pattern with no real-world matches is conflated with "no
    // watch_options" and falls back to matching everything
    #[test]
    fn a_pattern_matching_nothing_never_triggers() {
        let filter = WatchFilter::new(&["nomatch/**/*.foo".to_string()], &[]).unwrap();
        assert!(!filter.triggers(Path::new("src/main.rs")));
    }

    // fails if the error doesn't carry the offending pattern, reports the
    // wrong variant, or panics instead of returning `Err`
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

    // fails if a strip-prefix failure falls back to matching the untouched
    // absolute path — `MATCH_EVERYTHING` would then wrongly trigger on a
    // path that lies entirely outside the watched root
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

    // ------------------------------------------------------------------
    // Step 2: the group loop, paused-clock, driven by a hand-fed channel.
    // ------------------------------------------------------------------

    /// Generous bound on how long a paused-clock test may wait for a
    /// restart event before concluding the loop is broken. Costs no real
    /// wall-clock time under `start_paused`: auto-advance walks straight to
    /// it once nothing else is ready.
    const EVENT_WAIT: Duration = Duration::from_secs(30);

    /// How many `tokio::task::yield_now` rounds [`settle`] spends. Never
    /// advances the paused clock itself — only an explicit `advance()`, or
    /// the runtime's own idle auto-advance, can do that — so no value here
    /// can accidentally resolve a pending timer early. This is headroom
    /// against the group loop, the actor and a sheep's own task each
    /// needing their own scheduling turn, not a precise count.
    const SETTLE_YIELDS: usize = 16;

    /// Lets every task that can make progress without the clock moving do
    /// so, then returns — see [`SETTLE_YIELDS`].
    async fn settle() {
        for _ in 0..SETTLE_YIELDS {
            tokio::task::yield_now().await;
        }
    }

    /// One supervisor engine over a scripted runner, plus the bus receiver
    /// and tempdir backing it.
    fn spawn_test_fixture(
        scripts: Vec<ProcScript>,
    ) -> (
        SupervisorHandle,
        broadcast::Receiver<BusEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (events, rx) = broadcast::channel(64);
        let runner = ScriptedRunner::new(scripts);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        (handle, rx, dir)
    }

    /// Registers `name` with `instances` copies, returning each spawned
    /// instance's info.
    async fn start_app(handle: &SupervisorHandle, name: &str, instances: u32) -> Vec<ProcessInfo> {
        let mut app = AppConfig::minimal(name, "./srv");
        app.instances = instances;
        handle.start(vec![normalize(app).unwrap()]).await.unwrap()
    }

    /// Waits up to `deadline` for the next `BusEvent::Process { event:
    /// Restart, .. }` for `name`, wrapped in a timeout so a loop that never
    /// restarts fails the test instead of hanging it (Global Constraints
    /// rule 11).
    async fn expect_restart(
        rx: &mut broadcast::Receiver<BusEvent>,
        name: &str,
        deadline: Duration,
    ) -> ProcessInfo {
        loop {
            match tokio::time::timeout(deadline, rx.recv()).await {
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
    /// arrives — a real poll, not a bare `try_recv` (Global Constraints
    /// rule 11): a restart still working its way through the loop → actor →
    /// sheep-task round trip needs the scheduling rounds a bounded `recv`
    /// gives it, which a `try_recv` negative cannot.
    async fn assert_no_restart_within(
        rx: &mut broadcast::Receiver<BusEvent>,
        name: &str,
        window: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Err(_) => return, // window elapsed with nothing matching — expected
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
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(err)) => {
                    panic!("event channel closed while checking for no restart of {name}: {err}")
                }
            }
        }
    }

    /// A [`RootedFilter`] matching every path under `root`.
    fn matches_everything(root: PathBuf) -> RootedFilter {
        RootedFilter {
            root,
            filter: WatchFilter::new(&[], &[]).unwrap(),
        }
    }

    // fails if only-ignored paths still reach `supervisor.restart` — e.g. a
    // loop that restarts on any non-empty batch without ever consulting the
    // filter
    #[tokio::test(start_paused = true)]
    async fn a_batch_of_only_ignored_paths_produces_no_restart() {
        let (handle, mut rx, _dir) = spawn_test_fixture(vec![ProcScript::never_exits()]);
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

        tx.send(vec![root.join(".git/index")]).unwrap();
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        group.abort();
    }

    // fails if a triggering batch is dropped, or if the loop somehow
    // restarts more than once for a single batch
    #[tokio::test(start_paused = true)]
    async fn a_batch_with_one_triggering_path_produces_exactly_one_restart() {
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

        tx.send(vec![root.join("src/main.rs")]).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        group.abort();
    }

    // fails two ways: a loop that drops a batch queued during an in-flight
    // restart (only 1 restart total, not 2 — no re-check), and a loop that
    // processes each queued send as its own `recv`/restart cycle instead of
    // draining them into one check (3 restarts total, not 2)
    #[tokio::test(start_paused = true)]
    async fn a_batch_queued_during_a_restart_is_rechecked_and_drained_as_one() {
        // Four scripts, not three: a broken implementation that processes
        // `b.rs` and `c.rs` as two separate restarts (instead of draining
        // them into one) needs a fourth spawn to succeed. With only three,
        // that third respawn attempt would hit an exhausted script and
        // report `Errored` instead of `Restart` — invisible to
        // `assert_no_restart_within` below, which only watches for
        // `Restart` — and the mutation would pass this test by accident.
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
        // graceful signal, so the kill ladder is genuinely stuck on the
        // full `kill_timeout` (1600ms, `AppConfig::minimal`'s default)
        // until the paused clock actually moves — nothing below can
        // resolve it early, `settle` included (it only yields, it never
        // advances time).
        tx.send(vec![root.join("a.rs")]).unwrap();
        settle().await;

        // Two more sends land in the queue while restart 1 is still
        // pending.
        tx.send(vec![root.join("b.rs")]).unwrap();
        tx.send(vec![root.join("c.rs")]).unwrap();

        let first = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(first.restarts, 1);
        let second = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(second.restarts, 2);
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        group.abort();
    }

    // fails if the group task lingers after its source disappears instead
    // of returning
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

    // fails if the group loop filters by status — the reach is the whole
    // name-group, not just its running instances, pinning this against a
    // reimplementation of the withdrawn per-instance filter
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

        tx.send(vec![root.join("src/main.rs")]).unwrap();

        let first = expect_restart(&mut rx, name, EVENT_WAIT).await;
        let second = expect_restart(&mut rx, name, EVENT_WAIT).await;
        let stopped_info = [first, second]
            .into_iter()
            .find(|info| info.id == stopped_id)
            .expect("the previously-stopped instance never restarted");
        // `Online` alone would pass against a group that never touched the
        // stopped instance if something else had started it — `restarts`
        // is what makes the claim about *this* restart.
        assert_eq!(stopped_info.status, ProcStatus::Online);
        assert_eq!(stopped_info.restarts, 1);

        group.abort();
    }

    // fails if `NotFound` is treated as fatal and ends the loop, leaving
    // the watch armed but deaf to every batch after the first
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
        tx.send(vec![root.join("a.rs")]).unwrap();
        assert_no_restart_within(&mut rx, name, Duration::from_millis(200)).await;
        assert!(!group.is_finished(), "the loop must not exit on NotFound");

        // Registering the name for real and sending a second batch: if the
        // earlier `NotFound` had ended the loop, this would time out.
        start_app(&handle, name, 1).await;
        tx.send(vec![root.join("b.rs")]).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);

        group.abort();
    }

    // IR-33: real time, not the paused clock — the same OS-seam
    // justification as `source`'s own smoke tests. This is the only case
    // that can catch a dropped `WatchSource` guard (see
    // `spawn_watch_group`'s own doc): every other test above drives
    // `run_group` directly over a hand-fed channel and never constructs a
    // real `WatchSource` at all.
    const REAL_TEST_DELAY: Duration = Duration::from_millis(50);
    const REAL_SMOKE_DEADLINE: Duration = Duration::from_secs(5);
    const REAL_NO_RESTART_WINDOW: Duration = Duration::from_millis(500);

    // Mirrors `source`'s own `NO_DELIVERY_WINDOW` guard: raising
    // `REAL_TEST_DELAY` for CI-flake reasons without raising this window
    // would silently stop the abort-half of the test below from being able
    // to catch a leaked watch.
    const _: () = assert!(
        REAL_NO_RESTART_WINDOW.as_millis() >= REAL_TEST_DELAY.as_millis() * 4,
        "REAL_NO_RESTART_WINDOW must stay at least 4x REAL_TEST_DELAY, or \
         the abort case stops catching a leaked watch"
    );

    // fails if the debouncer guard is dropped before the loop ever sees an
    // event (no restart at all), or if it is leaked past `abort()` (a
    // restart for the post-abort touch)
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
            REAL_TEST_DELAY,
            handle.clone(),
        )
        .unwrap();

        crate::testing::touch(&root, "trigger.txt").unwrap();
        let info = expect_restart(&mut rx, name, REAL_SMOKE_DEADLINE).await;
        assert_eq!(info.restarts, 1);

        group.abort();
        crate::testing::touch(&root, "after-abort.txt").unwrap();
        assert_no_restart_within(&mut rx, name, REAL_NO_RESTART_WINDOW).await;
    }
}
