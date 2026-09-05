//! The filesystem-watch subsystem (spec §4).
//!
//! [`source`] is the OS seam: it bridges notify's debounced filesystem
//! events onto a tokio channel. [`WatchFilter`] decides which of those
//! delivered paths matter, and [`spawn_watch_group`] runs one name-group's
//! restart loop over them — single-flighted and re-checked, exactly like
//! [`crate::cron`]'s restart loop.
//!
//! # What a change reaches, and what a stop takes away
//!
//! **A triggering change restarts every instance of the name**, stopped
//! instances included — the same reach [`crate::cron`]'s schedule has. What
//! keeps a stopped sheep down is that **stopping a sheep disarms its watch**,
//! never a filter on the restart itself; the extras registry owns that
//! disarm.
//!
//! Those two sentences sound contradictory until they are read together, and
//! the case that shows why is a partially stopped group. With a
//! single-instance app the protection is total: `shep stop web` disarms the
//! watcher, and no later save can bring `web` back. With `web` at two
//! instances, `shep stop web-1` leaves `web-2` running and the group's one
//! watcher armed — so the next save under the tree restarts the whole name,
//! and `web-1` comes back up. That is the accepted consequence of a
//! one-watcher-per-name-group design, not a gap: the alternative was a
//! restart scope parameter threaded through the actor's manual-command path,
//! to serve a corner that a full stop already covers. Stop the group, or
//! delete the instance.
//!
//! # What is filtered
//!
//! A delivered path is checked against two glob sets, both rooted at the
//! watch root:
//!
//! - **Include** — the app's `watch_options`, or `**` when it names none.
//! - **Ignore** — `DEFAULT_IGNORE_GLOBS`, *plus* the app's `ignore_watch`,
//!   *plus* an entry per log file of the app's own that lies under the root
//!   (`own_log_ignores`). The defaults are never replaced by an app's list,
//!   only extended.
//!
//! A path triggers when it matches include **and** does not match ignore, so
//! ignore always wins: `watch_options = ["**/*.rs"]` with `ignore_watch =
//! ["target/**"]` watches Rust sources outside `target/`, and no
//! `watch_options` entry can re-admit a dot-file the defaults exclude. The
//! defaults exist because a `git status` would otherwise restart the flock.
//!
//! Shep's own writes are the derived third list's job, not the defaults'. An
//! app taking the default log paths writes under `$SHEP_HOME`, outside the
//! tree entirely; one naming an explicit `out_file`/`err_file` under its own
//! `cwd` is writing inside it, and `**/logs/**` covers that only if the user
//! happened to name the directory `logs`. See `own_log_ignores` for the loop
//! that would otherwise leave behind.
//!
//! Two paths never reach the glob sets, and both answer "no". A path that
//! does not lie under the root never triggers, rather than being matched as
//! though it were relative. The root *itself* never triggers either: it
//! strips to an empty relative path, and both lists are written about entries
//! inside the tree rather than about the tree's own inode.
//!
//! One thing does bypass the glob sets, and it is not a path: a **rescan**,
//! notify's marker for "I dropped events, re-read the tree". It arrives as
//! [`source::WatchBatch::rescan`] rather than as a path, precisely so that an
//! ordinary event on the root cannot be mistaken for it, and it restarts the
//! group whatever either list says.
//!
//! # Caveats
//!
//! - **The debounce is real time, not virtual.** `watch_delay` (default
//!   `DEFAULT_WATCH_DELAY`) is enforced inside notify's own debouncer,
//!   which runs on its own OS thread — `tokio::time`'s paused clock does not
//!   move it, so tests that need to observe a debounce must wait for it.
//! - **Delivery is the OS's.** notify uses FSEvents, inotify, or a polling
//!   fallback depending on platform; coalescing, ordering and latency differ
//!   between them, and a watch is a heuristic about "something changed"
//!   rather than a transaction log.
//! - **`watch = true` requires `cwd`.** There is no directory to watch
//!   otherwise, and `shep-core`'s `normalize` refuses the app rather than
//!   arming nothing quietly.
//! - **The restart is group-wide and all at once.** Rolling restarts are
//!   what reload is for.
//!
//! ## Reference
//!
//! - [`source::WatchSource`], [`source::watch_tree`], [`source::WatchError`]
//! - [`WatchFilter`], [`WatchFilterError`], [`spawn_watch_group`]
//! - `DEFAULT_WATCH_DELAY`, `DEFAULT_IGNORE_GLOBS`, `own_log_ignores`

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
/// Spec §4's default. Long enough to coalesce the multi-event burst a
/// single editor save produces (write to a temp file, rename over the
/// target, chmod), short enough that a save-to-restart round trip still
/// feels immediate.
///
/// Applied in exactly one place — [`crate::extras`]'s watch arming, where an
/// app's own `watch_delay` is preferred whenever it set one.
pub(crate) const DEFAULT_WATCH_DELAY: Duration = Duration::from_millis(500);

/// Floor the watch arming enforces on an app's own `watch_delay`.
///
/// `shep-core`'s `normalize` already rejects an explicit `watch_delay = "0"`
/// (`NormalizeError::ZeroWatchDelay`), but that guard lives behind boot wiring
/// this crate does not own — the same reason `probes::MIN_PROBE_INTERVAL` and
/// `crate::cron::MIN_MAX_SLEEP` keep their own floors. Without one here too,
/// any caller could hand [`spawn_watch_group`] a zero, and
/// `notify-debouncer-full` derives its poll tick as `delay / 4` and sleeps it
/// on a dedicated OS thread: at zero that thread becomes `loop { sleep(0);
/// lock(); }`, measured at 5.98s of user CPU across a three-second watch that
/// costs 0.00s at [`DEFAULT_WATCH_DELAY`].
///
/// One millisecond, where its two siblings are a full second, because this is
/// a debounce rather than a polling period: a floor high enough to be a
/// *tuning* value would silently lengthen a save-to-restart round trip the
/// user deliberately shortened. Zero is the only value that spins — `1ms / 4`
/// is a 250µs tick, and the thread parks on it — so this is the largest floor
/// that fixes the spin and clamps nothing else, which is what keeps it in
/// agreement with the rejection above rather than overruling it.
pub(crate) const MIN_WATCH_DELAY: Duration = Duration::from_millis(1);

/// Paths ignored by every watch, before `ignore_watch` is even consulted.
///
/// Dot-entries cover editor swap files and `.git`'s own churn — a `git
/// status` would otherwise restart the flock.
///
/// The `logs`/`pids` entries are narrower than they look, and are not what
/// keeps shep's own writes from restarting the app. They match a *root-
/// relative* path, so they cover a `logs/` or `pids/` directory the user
/// happens to keep inside the watched tree; shep's own defaults are written
/// under `$SHEP_HOME`, which is normally nowhere near it. The arrangement
/// that really does put a shep write inside the tree is an app naming an
/// explicit `out_file`/`err_file` under its own `cwd`, and that path need not
/// contain a `logs` component at all — `own_log_ignores` is what covers it.
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

/// Ignore patterns covering a sheep's own log files — one per path in `logs`
/// that lies under `root`, and nothing for the ones that don't.
///
/// This is the guard [`DEFAULT_IGNORE_GLOBS`] cannot be. An app naming an
/// explicit `out_file` or `err_file` under its own `cwd` is watching a file
/// shep itself writes, and unignored that is a loop nothing breaks: the
/// startup line trips the debounce, the debounce restarts the name group, the
/// restart writes another startup line. `max_restarts` is not a backstop
/// either — an automatic restart resets the budget rather than spending it.
///
/// Each path is canonicalized through its parent before being stripped,
/// because `root` arrives canonical and an app's `cwd` need not be: on macOS a
/// `/var/…` directory resolves to `/private/var/…`, and stripping the raw form
/// would silently yield no ignore at all — the same trap the watch root itself
/// documents.
pub(crate) fn own_log_ignores<'a>(
    root: &Path,
    logs: impl IntoIterator<Item = &'a Path>,
) -> Vec<String> {
    logs.into_iter()
        .filter_map(|log| literal_glob_under(root, log))
        .collect()
}

/// One path's root-relative form as a glob matching it and nothing else, or
/// `None` when it does not lie under `root` (the ordinary case — the default
/// log paths live in `$SHEP_HOME`) or cannot be spelled as a pattern at all.
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
/// The file itself may not exist yet — a re-arm happens before the respawned
/// child has written a byte — while its directory does by the time a spawn has
/// succeeded. Falls back to `path` untouched when even the parent will not
/// resolve, which just means no ignore is derived from it.
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
    /// Two paths never reach the glob sets, and both answer `false`.
    ///
    /// A path that does not lie under `root` never triggers, rather than
    /// falling back to matching the untouched absolute form against patterns
    /// written for relative ones. The OS should not deliver one — `root` is
    /// the tree being watched — but a symlinked subtree inside it can (see
    /// [`source::watch_tree`]'s own doc on resolved-vs-literal paths).
    ///
    /// `root` itself does not trigger either. It strips to an empty relative
    /// path, and both glob sets are written about entries *inside* the tree,
    /// not about the tree's own inode: a `chmod` or a rename of that inode
    /// changed nothing under it. macOS in particular delivers a spurious
    /// `Create(Folder)` for the root whenever a stream is armed before
    /// `fseventsd`'s cursor has passed the `mkdir` that made it, and treating
    /// that as a trigger restarts an app the instant its watch is armed —
    /// past every `ignore_watch` entry, since a bypass consults neither list.
    ///
    /// The rescan a bypass exists for is not a path at all, and reaches
    /// [`run_group`] as [`source::WatchBatch::rescan`] instead.
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
/// `root` is an already-canonicalized absolute directory. It comes from the
/// app's own `cwd`, which config validation requires whenever `watch` is
/// on, and never from the daemon's working directory. Aborting the handle
/// stops the OS watch as well as the loop, because the debouncer guard
/// lives inside the spawned future.
///
/// A triggering change restarts every instance of the name. Stopping a
/// sheep is what stops its watch: the last instance of a name going away
/// disarms this group. Stopping ONE instance of a name whose siblings are
/// still up does not — see this module's own doc for the partially-stopped
/// group.
///
/// A rescan — the OS told notify it dropped events — restarts the group
/// whatever `watch_options` and `ignore_watch` say, since the paths that
/// changed during the gap are exactly what nobody knows. The rule is keyed on
/// notify's own rescan flag, carried alongside the paths as
/// [`source::WatchBatch::rescan`]. An event on the root directory itself (a
/// `chmod` or a rename of that inode) is an ordinary event and is *not* one:
/// it changed nothing under the tree, so it restarts nothing.
///
/// Must be called from within a Tokio runtime context: it spawns the group
/// loop immediately, the same way [`crate::cron::spawn_cron_worker`] and
/// [`crate::probes::spawn_liveness_task`] already document for themselves.
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
/// group-wide restart through [`SupervisorHandle::restart_automatic`] — a
/// file changing is not a person's `shep restart`, so an operator's `stop`
/// landing while one is still mid-kill-ladder takes the sheep back off it
/// instead of being converted into the restart it raced.
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
    mut rx: mpsc::UnboundedReceiver<WatchBatch>,
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
            batch.paths.extend(more.paths);
            batch.rescan |= more.rescan;
        }
        // A rescan is checked ahead of the glob sets, and deliberately: it is
        // not a path but a statement that unknown paths changed, so there is
        // nothing for either list to be matched against. Restarting is the
        // conservative reading, and the alternative is a watch that goes quiet
        // precisely when it knows least.
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
                // group yet — a race with disarm, not a fault, and the
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
                | SupervisorError::CannotStart(_)
                | SupervisorError::IsADog(_)
                | SupervisorError::InvalidEnv(_)
                | SupervisorError::InvalidField(_)
                | SupervisorError::Overrides(_)),
            ) => {
                // A restart touches no log files, starts no reload, scales
                // nothing, registers no batch and never reads or writes the
                // override store, so none of the eight can arrive. Named
                // rather than swept into a catch-all, so a variant this path
                // CAN produce still fails to compile here.
                tracing::warn!(name, %err, "watch-triggered restart reported an unrelated failure");
            }
            Err(err @ SupervisorError::EngineStopped) => {
                tracing::warn!(name, %err, "supervisor engine has shut down; watch worker ending");
                return;
            }
        }
    }
}

/// The real-time constants shared by every real-filesystem test suite in this
/// crate (IR-33): [`source`]'s smoke tests, this module's own case for
/// [`spawn_watch_group`], and the extras registry's arm/disarm case.
///
/// One owner rather than a copy per suite. Every one of them drives the same
/// debouncer at the same delay, so a value tuned in one place and not the
/// others silently weakens whichever copy was left behind — and the
/// relationship between `TEST_DELAY` and `NO_EVENT_WINDOW` is load-bearing
/// (see the assertion beside `dropping_the_source_stops_delivery`).
#[cfg(test)]
pub(crate) mod real_time {
    use core::time::Duration;

    /// Debounce window for every real-filesystem test in this crate: tens of
    /// milliseconds, so a real save-to-batch round trip finishes fast without
    /// accidentally coalescing writes a test means to keep distinct.
    pub(crate) const TEST_DELAY: Duration = Duration::from_millis(50);

    /// How long a test waits for something that IS expected to arrive — a
    /// delivered batch, or a watch-triggered restart. Generous enough that
    /// a loaded CI runner's real inotify/FSEvents latency cannot turn a
    /// genuine pass into a flaky timeout.
    pub(crate) const SMOKE_DEADLINE: Duration = Duration::from_secs(5);

    /// How long a test waits for something that must NOT arrive. Short on
    /// purpose: this window is a cost every passing run of such a test pays
    /// (its passing path is a timeout, not an event), and it exists only to
    /// prove a negative — generous enough that a real event, if whatever
    /// was meant to stop did not, has time to land; short enough not to
    /// make a green suite slow.
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
    // `WatchFilter` and the root-relative boundary — pure, no tokio, no
    // filesystem (IR-40).
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
    //
    // The include pattern is the literal `"**"` a user would write, not the
    // `MATCH_EVERYTHING` const: passing the const would make this case's
    // premise track whatever that const later becomes, and stop it being
    // distinguishable from the empty-`watch_options` case it exists to
    // contrast with.
    #[test]
    fn default_ignores_beat_an_explicit_include() {
        let filter = WatchFilter::new(&["**".to_string()], &[]).unwrap();
        assert!(!filter.triggers(Path::new(".git/index")));
        assert!(!filter.triggers(Path::new("node_modules/x/y.js")));
    }

    // fails if `ignore_watch` patterns are never merged into the ignore set
    #[test]
    fn an_ignore_watch_entry_beats_an_include() {
        let filter = WatchFilter::new(&["**".to_string()], &["dist/**".to_string()]).unwrap();
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

    // Pins the rendered text, which the case above does not: it destructures
    // the variant and never renders it, leaving `Display` reachable only
    // through code no test ran.
    //
    // fails if the body stops writing what it carries — an empty rendering,
    // or one that drops the pattern or the reason. At config load this string
    // is the only thing telling a user WHICH of their patterns globset
    // refused, and why.
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

    // The root is an ordinary path, and this is the case that says so. The
    // opposite reading — the root triggering ahead of both glob sets — cannot
    // be right on macOS, where FSEvents delivers a `Create(Folder)` for the
    // watch root itself to a stream armed before `fseventsd`'s event-ID cursor
    // has passed the `mkdir` that made it: an app would be restarted the moment
    // its watch came up, past every `ignore_watch` entry it had written. The
    // rescan that DOES bypass both sets is not a path and never arrives as one
    // — `a_rescan_restarts_under_a_non_matching_watch_options` is its case.
    //
    // fails if an unconditional bypass is put back — a `return true` for the
    // empty relative path — under which no `watch_options` and no
    // `ignore_watch` can suppress an event on the root's own inode.
    #[test]
    fn the_root_itself_never_triggers_however_wide_the_watch_options() {
        // The widest include there is, so a failure here is about the root
        // and not about patterns that happened not to match it.
        let filter = matches_everything(PathBuf::from("/watched"));
        assert!(!filter.triggers(Path::new("/watched")));
        // Control: the same filter, one level in, does trigger — so the case
        // above is about the root itself rather than about a filter that
        // matches nothing.
        assert!(filter.triggers(Path::new("/watched/other/a.txt")));
    }

    // fails if `own_log_ignores` derives a pattern from a path OUTSIDE the
    // watch root — which is the ORDINARY case, since the default log paths
    // live under `$SHEP_HOME`, and an app that watches its whole `cwd` would
    // then be handed an ignore beginning `../..` that either matches nothing
    // or matches by accident. Also fails if the path reaches globset
    // unescaped: a log file named `app[0].log` would become a character class,
    // matching `app0.log` while missing its own name — and the loop the
    // ignore exists to break would run anyway.
    //
    // Touches the filesystem (nothing else can canonicalize) but no tokio: the
    // two directories exist only so the parent resolution has something real
    // to resolve.
    #[test]
    fn own_log_ignores_covers_only_the_paths_under_the_root() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        // Canonical, as `arm_watch` hands it over — the raw tempdir path is
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
        broadcast::Receiver<SharedEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (events, rx) = crate::bus::test_bus(64);
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
    /// arrives — a real poll, not a bare `try_recv` (Global Constraints
    /// rule 11): a restart still working its way through the loop → actor →
    /// sheep-task round trip needs the scheduling rounds a bounded `recv`
    /// gives it, which a `try_recv` negative cannot.
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
                // A negative assertion cannot skip events: the ones the
                // broadcast channel dropped may include the very `Restart`
                // this forbids, so continuing here would return success on
                // an overflow. `expect_restart` may skip them safely — the
                // worst a lag costs it is a timeout — but this one has to
                // fail loudly instead of failing open.
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

    /// A [`RootedFilter`] matching every path under `root`.
    fn matches_everything(root: PathBuf) -> RootedFilter {
        RootedFilter {
            root,
            filter: WatchFilter::new(&[], &[]).unwrap(),
        }
    }

    /// An ordinary delivery: these paths changed, and notify lost nothing.
    fn changed(paths: Vec<PathBuf>) -> WatchBatch {
        WatchBatch {
            paths,
            rescan: false,
        }
    }

    /// A rescan in its path-less (inotify) shape — notify dropped events and
    /// wants the tree re-read. The macOS shape carries the root alongside the
    /// flag, and `source`'s own tests cover the difference; what matters here
    /// is the flag.
    fn rescan_marker() -> WatchBatch {
        WatchBatch {
            paths: Vec::new(),
            rescan: true,
        }
    }

    // fails if only-ignored paths still reach `supervisor.restart` — e.g. a
    // loop that restarts on any non-empty batch without ever consulting the
    // filter
    #[tokio::test(start_paused = true)]
    async fn a_batch_of_only_ignored_paths_produces_no_restart() {
        // Two scripts, not one: `start_app` consumes the first, so a
        // filter-bypassing implementation needs a second for its respawn to
        // succeed and emit a real `Restart`. With one, that respawn would
        // hit an exhausted script and report `Errored` instead — invisible
        // to `assert_no_restart_within`, which only watches for `Restart` —
        // and the mutation this test names would pass by accident.
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

    // fails if a triggering batch is dropped, or if the loop somehow
    // restarts more than once for a single batch
    #[tokio::test(start_paused = true)]
    async fn a_batch_with_one_triggering_path_produces_exactly_one_restart() {
        // Three scripts: one for `start_app`, one for the expected restart,
        // and a third so a double-firing implementation has something left
        // to spawn from and emits a second visible `Restart`. With only
        // two, that second restart would hit an exhausted script and report
        // `Errored` — which `assert_no_restart_within` does not watch for —
        // leaving the trailing negative below unable to fail.
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

    // fails if a rescan is filtered away like an ordinary path: it carries no
    // path a configured `watch_options` could match — on Linux it carries no
    // path at all — so consulting the glob sets at all leaves the watch deaf
    // exactly when notify has already lost events. Two scripts — one for
    // `start_app`, one for the restart this expects — which is enough for the
    // mutation to fail visibly, since dropping the marker produces no restart
    // at all rather than an extra one.
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

    // The group-loop half of what
    // `the_root_itself_never_triggers_however_wide_the_watch_options` pins at
    // the filter tier, and it needs its own case: the loop's rescan check
    // happens before the filter is consulted at all, so a loop that read the
    // root as its rescan signal would restart here while every filter
    // assertion stayed green.
    //
    // fails if an ordinary event on the root restarts the group: `batch.rescan
    // || batch.paths.iter().any(|p| p == root)`, or a `RootedFilter` that
    // answers `true` for the empty relative path.
    //
    // Two scripts: one for `start_app`, one so a loop that does restart here
    // has something to spawn and emits a visible `Restart` — with one, the
    // respawn would hit an exhausted script and land in `Errored`, which
    // `assert_no_restart_within` does not watch for, and the mutation would
    // pass by accident.
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

        // The root, with no rescan flag on it — a `chmod` of that inode, or
        // FSEvents' arm-time `Create(Folder)`.
        tx.send(changed(vec![root.clone()])).unwrap();
        assert_no_restart_within(&mut rx, name, Duration::from_secs(5)).await;

        // Control: the same loop, the same watch, one level in — so the
        // silence above is the root being filtered and not a loop that stopped
        // restarting for anything.
        tx.send(changed(vec![root.join("src/main.rs")])).unwrap();
        let info = expect_restart(&mut rx, name, EVENT_WAIT).await;
        assert_eq!(info.restarts, 1);

        group.abort();
    }

    // fails if the drain drops the rescan flag off a batch it folds in —
    // `batch.paths.extend(more.paths);` without the `batch.rescan |=
    // more.rescan;` beside it. A rescan that lands while the loop is busy, or
    // simply behind a batch of ignored paths, would then be swallowed by the
    // batch it was merged into: the loop consults the glob sets, finds nothing
    // triggering, and re-reads nothing.
    //
    // The two sends are made back to back with no `settle` between them so
    // they are genuinely queued together when the loop next looks, which is
    // what puts them through the drain rather than through two `recv` rounds.
    //
    // Two scripts, one for `start_app` and one for the restart expected here.
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

        tx.send(changed(vec![root.join("src/main.rs")])).unwrap();

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

    // An operator's `stop` landing on a sheep whose kill ladder a watched
    // file already started. Nobody typed the file change, so the operator's
    // intent wins: the sheep named ends `Stopped`, never respawned, and
    // `stop()` reports that honestly.
    //
    // Two instances, because one could not tell a pass from a test whose
    // batch never reached the actor at all — with a single sheep, a `stop`
    // that simply arrived first produces the very same `Stopped`. The second
    // instance is left alone precisely so its restart is observable: waiting
    // on that restart is both the proof the batch fired and the barrier that
    // puts it strictly before the `stop`, since one `begin_manual` claims
    // both instances' markers in the same synchronous pass.
    //
    // fails if the group loop declares `CommandOrigin::Operator` — calling
    // `restart` rather than `restart_automatic`: `claim_manual` then keeps
    // the batch's marker under plain first-command-wins, `handle_exited`
    // respawns, and the `stop()` caller is handed an `Online` snapshot of a
    // sheep that is genuinely back up with `restarts: 1`.
    #[tokio::test(start_paused = true)]
    async fn an_operators_stop_beats_a_watch_triggered_restart_mid_ladder() {
        // Four procs, which is the most this test can demand: both instances'
        // initial ones, the respawn the untouched instance legitimately
        // performs, and the respawn a broken implementation performs behind
        // the stop's back. A pool of three would answer that fourth spawn
        // `SpawnFailed("script exhausted")` and land the bug in `Errored`
        // rather than the `Online` that shows how bad it is.
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

    // The loop's other exit: not its source going away, but the engine it
    // restarts through. `dropping_the_sender_ends_the_group_task` covers the
    // first; nothing covered this one.
    //
    // fails if the `EngineStopped` arm falls through to the next iteration
    // instead of returning: the group would sit on a live OS watch, spending
    // a debouncer thread and one restart attempt per save on a mailbox nobody
    // reads, for as long as the process lives. The sender is held until after
    // the join, so the arm under test is the only way the task can end.
    //
    // No scripts in the fixture, and that is the honest count: the engine is
    // shut down before the batch is sent, so no spawn is reachable under
    // either implementation — the correct one or the fallen-through one.
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

    // IR-33: real time, not the paused clock — the same OS-seam
    // justification as `source`'s own smoke tests. This is the only case
    // that constructs a real `WatchSource` at all: every other test above
    // drives `run_group` directly over a hand-fed channel.
    //
    // What each half proves, precisely:
    //
    // - The touch half is load-bearing. It is the only case anywhere that
    //   catches a `WatchSource` guard dropped before the loop sees an event
    //   — `let _ = source;` in place of `let _source = source;` — which
    //   kills the watch and produces no restart at all.
    // - The abort half proves only that the loop stops. It cannot catch a
    //   *leaked* guard: `rx` is owned by the aborted future, so `abort()`
    //   drops the receiver together with `_source`. A deliberately leaked
    //   `WatchSource` would keep an OS thread alive with no reader left to
    //   call `restart`, so no `Restart` event can exist under that mutation
    //   at this tier. A leak is observable only end-to-end, where the
    //   orphaned thread outlives a removed sheep and keeps watching its
    //   directory; `source`'s own `dropping_the_source_stops_delivery`
    //   covers the guard's drop semantics directly, and the end-to-end
    //   watch scenario is what catches the leak itself.

    // Boundary sweep (IR-40): a watch root that does not exist, seen from
    // the arming entry point rather than from the seam.
    // `source::tests::a_nonexistent_root_returns_a_watch_error_naming_the_path`
    // already pins what `watch_tree` returns; what is unpinned is that
    // `spawn_watch_group` PROPAGATES it — the failure mode being an arming
    // path that swallows the error and hands back a live handle over a watch
    // that was never registered, which would cost an app its watch silently.
    // The subsystem's other two boundaries are already pinned above too — an
    // empty `watch_options` by `empty_watch_options_matches_every_path`, and
    // a glob with no matches by `a_pattern_matching_nothing_never_triggers`,
    // which also settles that such a glob never triggers rather than
    // erroring.
    //
    // fails if the arming swallows the failure, and fails if the root notify
    // could not watch comes back as `Backend` (which carries no path, so an
    // operator running several watched apps cannot tell which cwd went
    // missing) or as `Watch` carrying some path other than the one it was
    // handed. `unwrap_err` alone would pass against all three.
    //
    // IR-33: real time and a real notify backend, like every other case in
    // this crate that constructs a watcher. Nothing here waits on an event,
    // so the real clock costs nothing.
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
    // The single-flight property (IR-37): the group loop against generated
    // batch sequences and generated restart durations.
    // ------------------------------------------------------------------

    /// Most batches one generated case feeds the group.
    const MAX_BATCHES: usize = 8;

    /// Scripted procs one generated case may spawn.
    ///
    /// Sized against the maximum a BROKEN loop can demand, not a correct one:
    /// the worst case is one restart per batch (a loop that never drains, so
    /// nothing collapses), plus the initial start. An exhausted
    /// `ScriptedRunner` answers `SpawnFailed("script exhausted")`, which the
    /// actor reports as `Errored` and not `Restart` — so a pool sized to a
    /// correct run would swallow exactly the extra restarts this property
    /// exists to see.
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
        // Zero is the case the property is really about — a send that lands
        // while the previous restart is still in flight — so it is drawn
        // half the time. The other two arms straddle the generated kill
        // timeout's own 200..2000ms range, so batches also arrive partway
        // through a ladder and well after one has finished.
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
    /// A model of the loop as written, and deliberately a strictly sequential
    /// one: it holds no notion of two restarts overlapping, because the loop
    /// it models has none. Every arrival already queued when the loop next
    /// looks is folded into one check (`run_group`'s drain), and a restart
    /// occupies the model for exactly `restart` from the moment that check
    /// decided to run it.
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
            // has not seen — or, if that already happened while it was busy,
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
        // across every generated gap, so a case costs more. `PROPTEST_CASES`
        // still overrides it (IR-37) — see `testing::proptest_config`.
        #![proptest_config(crate::testing::proptest_config(64))]

        // The mechanism is `run_group`'s own ordering — the restart is
        // awaited before the next `recv`, so single flight falls out of the
        // shape rather than out of a flag — and this property checks that
        // ordering against generated batch sequences and generated restart
        // durations. It does not change the loop.
        //
        // A restart's duration is the generated `kill_timeout`, exactly,
        // because every scripted proc here ignores its graceful signal and
        // only `kill_tree` ends it: the ladder runs the full timeout every
        // time. That is what turns "two restarts overlapped" into something
        // observable from the bus alone — two restarts genuinely in flight at
        // once finish less than `kill_timeout` apart.
        //
        // fails if a batch queued during an in-flight restart gets its own
        // `recv`/restart cycle instead of being drained into the next check
        // (too many restarts), if a batch is dropped (too few), or if the
        // restart is spawned rather than awaited (restarts that finish closer
        // together than one takes).
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
                // The bus is drained by its OWN task, started before the
                // first send, because WHEN a restart landed is half of this
                // property. A broadcast send wakes this task without moving
                // the paused clock, so the instant it records is the instant
                // the actor emitted — whereas a driver that reads the bus
                // only after its own `sleep` would stamp every event with
                // the end of that sleep instead. The window is a bounded
                // `timeout` + `recv` (Global Constraints rule 11), re-armed
                // on every event, so it expires on real silence rather than
                // on a gap the generator chose; it comfortably exceeds the
                // widest generated gap (6s) and kill timeout (2s), and stays
                // far under a scripted proc's own 30-day deadline.
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
                // with it on when — and how many times — it restarted.
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

        // fails if the debouncer guard is dropped before the loop ever sees an
        // event: the watch dies inside `spawn_watch_group` and the touch below
        // produces no restart. The abort half asserts the weaker claim its
        // comment above describes — the loop stops — not a leak check.
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
