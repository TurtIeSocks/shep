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
/// Dropping this stops the watch, but not the instant the drop runs: it
/// flips the debouncer's stop flag, and its OS thread only notices at its
/// own next tick — up to `delay / 4` later, the debouncer's default tick
/// rate when none is given — and may still emit one final batch before it
/// exits. At a 60s Flockfile `delay` that is a thread living up to 15s past
/// the drop. Bounded, not a leak, but do not read "stops the watch" as
/// immediate. Once that thread does exit it drops the sender feeding the
/// receiver below, and the receiver's next `recv()` returns `None`.
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
/// whatever the debouncer coalesced within `delay`; it is never empty — a
/// `need_rescan` marker (notify dropped events and wants the tree re-read)
/// carries no paths of its own, so it is forwarded as `vec![root]` rather
/// than dropped.
///
/// **Delivered paths are resolved, not literal.** If `root` is, or passes
/// through, a symlink, every path in every batch still comes back through
/// the real directory — never through the symlink `root` was given as, and
/// proven by this module's own
/// `a_symlinked_root_delivers_the_resolved_path_not_the_one_passed_in` test.
/// A caller that strips `root` as a literal path prefix (`WatchFilter`) must
/// canonicalize `root` first, or a delivered path never looks like it lies
/// under the tree it was just reported from.
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
    // Cloned into the handler closure below, which outlives this call: the
    // closure runs on the debouncer's own thread for as long as it lives,
    // long after `root: &Path` stops being valid to borrow.
    let watched_root = root.to_path_buf();

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
                // A `need_rescan` marker event carries no paths of its own —
                // notify dropped events (e.g. an inotify queue overflow) and
                // wants the whole tree re-read. Silently eating it here would
                // produce no batch and therefore no restart precisely when
                // the watch most needs to fire. Forwarding the root itself
                // keeps this function's "a batch is never empty" doc promise
                // and still gives the caller something to act on.
                let paths = if paths.is_empty() {
                    vec![watched_root.clone()]
                } else {
                    paths
                };
                // An `Err` here means the receiver (and `WatchSource`)
                // has already been dropped. Nothing to do: this thread
                // keeps running until its own `Drop` stops it.
                let _ = tx.send(paths);
            }
            Err(errors) => {
                for err in errors {
                    // `root` named explicitly: with several watches armed,
                    // "filesystem watch error" alone does not say which one.
                    tracing::warn!(
                        root = %watched_root.display(),
                        %err,
                        "filesystem watch error"
                    );
                }
            }
        }
    })
    .map_err(|err| WatchError::Backend {
        reason: render_reason(&err),
    })?;

    debouncer
        .watch(root, RecursiveMode::Recursive)
        .map_err(|err| WatchError::Watch {
            path: root.to_path_buf(),
            reason: render_reason(&err),
        })?;

    Ok((
        WatchSource {
            _debouncer: debouncer,
        },
        rx,
    ))
}

/// `err`'s rendered message, minus notify's own trailing `about [paths]`
/// clause.
///
/// notify's `Display` appends that clause whenever the error carries paths
/// (`notify-8.2.0/src/error.rs:120-125`) — e.g. `` No path was found. about
/// ["/x"] ``. [`WatchError::Watch`] already carries and renders `path`
/// itself, so keeping the clause would print the same path twice: ``could
/// not watch `/x`: No path was found. about ["/x"]``. Cutting it is cosmetic
/// only and safe if notify ever changes this format: `split_once` then finds
/// nothing to cut and the clause reappears — today's behavior, not a
/// regression.
fn render_reason(err: &notify::Error) -> String {
    let rendered = err.to_string();
    match rendered.split_once(" about [") {
        Some((message, _)) => message.to_string(),
        None => rendered,
    }
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

    // `dropping_the_source_stops_delivery` only catches a leaked debouncer
    // because `NO_DELIVERY_WINDOW` outlasts `TEST_DELAY` by a wide margin: a
    // leaked debouncer keeps debouncing on `TEST_DELAY`, so its stray batch
    // for `second.txt` lands roughly one `TEST_DELAY` after the write, well
    // inside the window. Someone who later hits CI flake on that test and
    // "fixes" it by raising `TEST_DELAY` — without also raising this window
    // — silently deletes the guard: at `TEST_DELAY` close to or past
    // `NO_DELIVERY_WINDOW`, the stray batch would arrive *after* the window
    // closes and the leak would pass undetected. This assertion turns that
    // edit into a compile error instead of a silent regression.
    const _: () = assert!(
        NO_DELIVERY_WINDOW.as_millis() >= TEST_DELAY.as_millis() * 4,
        "NO_DELIVERY_WINDOW must stay at least 4x TEST_DELAY, or \
         dropping_the_source_stops_delivery stops catching a leaked \
         debouncer guard"
    );

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

    // fails if `watch_tree` ignores its own `delay` and debounces against a
    // hardcoded window instead: both writes happen *before* either is
    // observed, so a longer hardcoded window would still be running when
    // `second.txt` lands and the first `expect_batch` below would already
    // carry it.
    #[tokio::test]
    async fn writes_separated_by_more_than_delay_produce_two_batches() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let (_source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

        let first = crate::testing::touch(&root, "first.txt").unwrap();
        tokio::time::sleep(TEST_DELAY * 4).await;
        let second = crate::testing::touch(&root, "second.txt").unwrap();

        let batch_one = expect_batch(&mut rx).await;
        assert!(
            batch_one.contains(&first),
            "expected {first:?} in the first batch, got {batch_one:?}"
        );
        assert!(
            !batch_one.contains(&second),
            "the two writes coalesced into one batch despite the gap \
             exceeding `delay` — got {batch_one:?}"
        );

        let batch_two = expect_batch(&mut rx).await;
        assert!(
            batch_two.contains(&second),
            "expected {second:?} in a batch of its own, got {batch_two:?}"
        );
    }

    // fails if `delay` is honoured too eagerly (e.g. treated as zero, or
    // each event flushed independently regardless of `delay`): these two
    // near-simultaneous writes would then arrive as two batches instead of
    // one. No sleep between them on purpose. A wider-than-`TEST_DELAY` local
    // window is deliberate too: the debouncer only combines events into one
    // handler call when both are still ready in the same `tick_rate` slice
    // (`delay / 4`), and at `TEST_DELAY`'s 12.5ms slice, a couple of
    // milliseconds of real FSEvents/thread-scheduling jitter between the two
    // writes was enough to straddle a tick and flake this test. A wider
    // window buys the same proof more slack, without slowing every other
    // test in this module that does not need it. Measured flaky at 300ms
    // (roughly 1 run in 8) on a loaded dev machine — a prior test's
    // debouncer thread can still be winding down (see the struct doc on
    // `WatchSource`'s drop not being instant) and steal a tick's worth of
    // scheduling from this one — so the margin is generous rather than
    // tight.
    const COALESCE_TEST_DELAY: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn writes_within_delay_coalesce_into_one_batch() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let (_source, mut rx) = watch_tree(&root, COALESCE_TEST_DELAY).unwrap();

        let first = crate::testing::touch(&root, "first.txt").unwrap();
        let second = crate::testing::touch(&root, "second.txt").unwrap();

        let batch = expect_batch(&mut rx).await;
        assert!(batch.contains(&first), "expected {first:?} in {batch:?}");
        assert!(
            batch.contains(&second),
            "writes inside `delay` must land in one batch, got {batch:?}"
        );

        match timeout(NO_DELIVERY_WINDOW, rx.recv()).await {
            Err(_) => {} // window elapsed with nothing further — expected
            Ok(None) => panic!("watch source ended unexpectedly"),
            Ok(Some(extra)) => panic!("unexpected second batch: {extra:?}"),
        }
    }

    // fails if a missing root is silently accepted, reported as the wrong
    // variant, or reported with an empty reason
    #[tokio::test]
    async fn a_nonexistent_root_returns_a_watch_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("does-not-exist");

        let err = watch_tree(&root, TEST_DELAY).expect_err("a nonexistent root must fail to watch");
        let WatchError::Watch { path, reason } = err else {
            panic!("expected WatchError::Watch, got {err:?}");
        };
        assert_eq!(path, root);
        assert!(!reason.is_empty(), "reason must not be empty");
        assert!(
            !reason.contains(" about ["),
            "reason must not carry notify's own redundant path clause: {reason:?}"
        );
    }

    // Pins both variants' rendered text directly, without depending on a
    // real notify failure to produce it (`Backend` never has one in these
    // tests — its only source is `new_debouncer` failing to build a
    // watcher backend at all, which nothing here can force).
    #[test]
    fn watch_error_display_does_not_repeat_the_path() {
        let backend = WatchError::Backend {
            reason: "boom".to_string(),
        };
        assert_eq!(
            backend.to_string(),
            "could not create a filesystem watcher: boom"
        );

        let watch = WatchError::Watch {
            path: PathBuf::from("/x"),
            // notify's own shape for a path-carrying reason, pre-stripped by
            // `render_reason` — see that function's doc for why the
            // trailing `about [...]` clause never reaches here.
            reason: "No path was found.".to_string(),
        };
        assert_eq!(
            watch.to_string(),
            "could not watch `/x`: No path was found."
        );

        // Both variants satisfy `core::error::Error`, not just `Display`.
        let _: &dyn core::error::Error = &backend;
        let _: &dyn core::error::Error = &watch;
    }

    // fails if deleting a watched path panics the handler thread or
    // otherwise breaks delivery instead of just reporting the removal
    #[tokio::test]
    async fn a_path_deleted_while_watched_still_produces_a_batch() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(&dir);
        let file = crate::testing::touch(&root, "doomed.txt").unwrap();
        let (_source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

        std::fs::remove_file(&file).unwrap();

        let batch = expect_batch(&mut rx).await;
        assert!(
            batch.contains(&file),
            "expected the deleted path {file:?} in the batch, got {batch:?}"
        );
    }

    // fails if a caller can assume delivered paths share root's own literal
    // form — see the trap this pins for `WatchFilter`'s prefix stripping,
    // documented on `watch_tree` itself
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_root_delivers_the_resolved_path_not_the_one_passed_in() {
        let target = tempfile::tempdir().unwrap();
        let resolved_target = canonical_root(&target);
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("link-to-target");
        std::os::unix::fs::symlink(&resolved_target, &link).unwrap();

        // Watch through the symlink, deliberately left un-canonicalized.
        let (_source, mut rx) = watch_tree(&link, TEST_DELAY).unwrap();
        crate::testing::touch(&link, "through-the-link.txt").unwrap();

        let batch = expect_batch(&mut rx).await;
        let resolved_file = resolved_target.join("through-the-link.txt");
        assert!(
            batch.contains(&resolved_file),
            "expected the resolved path {resolved_file:?} in {batch:?}"
        );
        assert!(
            !batch.iter().any(|p| p.starts_with(&link)),
            "expected no path under the symlink itself, got {batch:?}"
        );
    }
}
