//! [`WatchSource`] — notify's debounced filesystem events, bridged onto a
//! tokio channel (spec §4).
//!
//! The debouncer's own handler runs on its own OS thread, never on a tokio
//! one. [`watch_tree`] wraps that handler in a plain closure that forwards
//! each non-empty batch through an unbounded [`mpsc`] sender — non-blocking
//! and callable from any thread, which is exactly what a foreign-thread
//! callback needs. A bounded sender's `blocking_send` would park the very
//! thread that owns the watch; `try_send` would silently drop batches the
//! moment the channel briefly filled.
//!
//! [`WatchSource`] is a drop guard: the caller that spawns a loop over the
//! receiver must move it into the spawned future, or the watch stops before
//! the loop ever sees an event. See the struct's own doc for the failure
//! mode this causes and why nothing warns about it.

// Rejected alternative: a bounded channel sized to some capacity. The
// debouncer already coalesces every burst within `delay`, so this
// producer's rate is bounded by `delay`, not by the filesystem's raw event
// rate — the case a bounded channel exists to protect against does not
// arise here, and picking a bound would only add a capacity nobody could
// justify a number for.

use core::fmt;
use core::time::Duration;

use std::path::{Path, PathBuf};

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

/// A live filesystem watch over one directory tree.
///
/// Dropping this stops the watch: the debouncer's OS thread shuts down with
/// its guard, which drops the sender feeding the receiver below, so the
/// receiver's next `recv()` returns `None`.
///
/// **A caller that spawns a loop over the receiver must move this guard into
/// the spawned future.** A `WatchSource` left as a local in the spawning
/// function drops when that function returns — before the first event is ever
/// delivered — and the loop sees an immediate `None` and exits. Nothing warns
/// about it: Rust does not warn that a value is being dropped, and the
/// cheapest way to silence the `unused_variables` this raises under
/// `-D warnings` is to rename the binding, which preserves the bug exactly.
#[derive(Debug)]
pub struct WatchSource {
    /// Kept alive only: dropping it is what stops the watch (see the
    /// struct's own doc above).
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

/// Begins watching `root` recursively, debounced by `delay`.
///
/// Batches of changed paths arrive on the returned receiver. A batch is
/// whatever the debouncer coalesced within `delay`; it is never empty.
///
/// # Errors
///
/// - [`WatchError::Backend`] — notify could not create a watcher.
/// - [`WatchError::Watch`] — notify could not watch `root`, carrying the path.
pub fn watch_tree(
    root: &Path,
    delay: Duration,
) -> Result<(WatchSource, mpsc::UnboundedReceiver<Vec<PathBuf>>), WatchError> {
    let (tx, rx) = mpsc::unbounded_channel();

    let mut debouncer = new_debouncer(delay, None, move |result: DebounceEventResult| {
        // Runs on the debouncer's own OS thread — see this module's doc.
        match result {
            Ok(events) => {
                // `.event.paths`, not `.paths` through the `Deref`: moving
                // out of a deref target does not compile, and `DebouncedEvent`
                // exposes its wrapped `Event` as a plain owned field for
                // exactly this.
                let paths: Vec<PathBuf> = events
                    .into_iter()
                    .flat_map(|debounced| debounced.event.paths)
                    .collect();
                // A `need_rescan` marker event carries no paths of its own;
                // forwarding an empty batch would break this function's own
                // "a batch is never empty" doc promise.
                if !paths.is_empty() {
                    // An `Err` here means the receiver (and `WatchSource`)
                    // has already been dropped. Nothing to do: this thread
                    // keeps running until its own `Drop` stops it.
                    let _ = tx.send(paths);
                }
            }
            Err(errors) => {
                for err in errors {
                    tracing::warn!(%err, "filesystem watch error");
                }
            }
        }
    })
    .map_err(|err| WatchError::Backend {
        reason: err.to_string(),
    })?;

    debouncer
        .watch(root, RecursiveMode::Recursive)
        .map_err(|err| WatchError::Watch {
            path: root.to_path_buf(),
            reason: err.to_string(),
        })?;

    Ok((
        WatchSource {
            _debouncer: debouncer,
        },
        rx,
    ))
}

/// Why a filesystem watch could not be established.
///
/// Two variants and no `#[non_exhaustive]`: notify gives this module exactly
/// two failure points — building the backend and registering a path — and a
/// third reason would mean a third API call, not a third rendering of these
/// (IR-20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchError {
    /// notify could not construct a watcher for this platform's backend.
    /// Carries notify's rendered reason.
    Backend {
        /// notify's own rendered reason.
        reason: String,
    },
    /// notify could not begin watching the path: it does not exist, it is not
    /// readable, or the backend's watch limit is exhausted. Carries the path
    /// and notify's rendered reason.
    Watch {
        /// The path notify could not watch.
        path: PathBuf,
        /// notify's own rendered reason.
        reason: String,
    },
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend { reason } => {
                write!(f, "could not create a filesystem watcher: {reason}")
            }
            Self::Watch { path, reason } => {
                write!(f, "could not watch `{}`: {reason}", path.display())
            }
        }
    }
}

impl core::error::Error for WatchError {}

#[cfg(test)]
mod tests {
    // IR-33: real time, not the paused clock. This is the phase's OS seam,
    // and what it proves — a real inotify/FSEvents/ReadDirectoryChangesW
    // delivery — cannot come from a fake clock. Mirrors `probes::os`'s own
    // real-time justification.

    use core::time::Duration;

    use std::path::PathBuf;

    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;

    use super::*;

    /// Debounce window for every test below: tens of milliseconds, so a real
    /// save-to-batch round trip finishes fast without accidentally
    /// coalescing writes a test means to keep distinct.
    const TEST_DELAY: Duration = Duration::from_millis(50);

    /// How long a test waits for a batch that IS expected to arrive.
    /// Generous enough that a loaded CI runner's real inotify/FSEvents
    /// latency cannot turn a genuine pass into a flaky timeout.
    const WATCH_SMOKE_DEADLINE: Duration = Duration::from_secs(5);

    /// How long the "delivery has stopped" case waits for a batch that must
    /// NOT arrive. Short on purpose: this window is a cost every passing run
    /// of that one test pays (it is the one case whose passing path is a
    /// timeout, not an event), and it exists only to prove a negative —
    /// generous enough that a real event, if the guard failed to stop the
    /// watch, has time to reach the channel; short enough not to make a
    /// green suite slow.
    const NO_DELIVERY_WINDOW: Duration = Duration::from_millis(500);

    /// Canonicalizes `dir`'s path before returning it.
    ///
    /// On macOS, FSEvents reports paths through `/private/var/...` while
    /// `TempDir::path()` returns the `/var/...` symlink to it — without
    /// canonicalizing first, a batch's delivered paths would never equal the
    /// paths this module's own tests construct from the un-resolved root.
    fn canonical_root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path()
            .canonicalize()
            .expect("canonicalize tempdir root")
    }

    /// Waits up to [`WATCH_SMOKE_DEADLINE`] for the next batch.
    async fn expect_batch(rx: &mut UnboundedReceiver<Vec<PathBuf>>) -> Vec<PathBuf> {
        timeout(WATCH_SMOKE_DEADLINE, rx.recv())
            .await
            .expect("no batch arrived within the deadline")
            .expect("watch source ended before a batch arrived")
    }

    // fails if the watch is non-recursive, or if the handler's
    // thread-to-tokio bridge drops the batch
    #[tokio::test]
    async fn a_file_created_under_the_root_produces_a_batch_containing_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let (_source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

        let file = root.join("created.txt");
        std::fs::write(&file, b"hello").unwrap();

        let batch = expect_batch(&mut rx).await;
        assert!(
            batch.contains(&file),
            "expected {file:?} in the batch, got {batch:?}"
        );
    }

    // fails if `RecursiveMode::NonRecursive` was passed instead of `Recursive`
    #[tokio::test]
    async fn a_file_created_in_a_nested_subdirectory_also_produces_a_batch() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let (_source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

        let file = nested.join("created.txt");
        std::fs::write(&file, b"hello").unwrap();

        let batch = expect_batch(&mut rx).await;
        assert!(
            batch.contains(&file),
            "expected {file:?} in the batch, got {batch:?}"
        );
    }

    // fails if the debouncer guard is leaked (a `std::mem::forget`-equivalent,
    // or stored somewhere that outlives the `WatchSource`, e.g. a `static`),
    // which would leave an OS thread watching a deleted sheep's directory
    // forever: a write after the drop would still reach `rx` within the
    // bounded window below
    #[tokio::test]
    async fn dropping_the_source_stops_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let (source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

        // Prove the watch is live first, so a failure below can't be
        // confused with "this watch never worked in the first place".
        std::fs::write(root.join("first.txt"), b"hello").unwrap();
        expect_batch(&mut rx).await;

        drop(source);
        std::fs::write(root.join("second.txt"), b"hello").unwrap();

        // A bounded `timeout` + `recv`, not a bare `try_recv` (Global
        // Constraints rule 11). Both an expired timeout (nothing arrived)
        // and a closed channel (the debouncer thread already tore down and
        // dropped its sender) are honest readings of "delivery stopped";
        // only a delivered batch is the leak this test exists to catch.
        match timeout(NO_DELIVERY_WINDOW, rx.recv()).await {
            Err(_) => {}   // window elapsed with nothing arriving — expected
            Ok(None) => {} // sender dropped alongside the debouncer thread — expected
            Ok(Some(batch)) => {
                panic!("unexpected batch delivered after WatchSource was dropped: {batch:?}")
            }
        }
    }
}
