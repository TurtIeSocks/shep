//! [`WatchSource`] bridges notify's debounced filesystem events, delivered on
//! notify's own OS thread, onto a tokio [`mpsc`] channel.
//!
//! The channel is unbounded: the sender runs on a foreign thread, where
//! `blocking_send` would park the watch and `try_send` would drop batches
//! under backpressure.
//!
//! [`WatchSource`] is a drop guard. A caller that spawns a loop over the
//! receiver must move it into that future, or the watch stops before any
//! event is seen.

use core::fmt;
use core::time::Duration;

use std::path::{Path, PathBuf};

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

/// A live filesystem watch over one directory tree.
///
/// Dropping this stops the watch, but not instantly: the debouncer's OS
/// thread notices at its own next tick, up to `delay / 4` later, and may
/// still emit one final batch before it exits. Once it does, the
/// receiver's next `recv()` returns `None`.
#[derive(Debug)]
pub struct WatchSource {
    /// Kept alive only: dropping it stops the watch.
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

/// Begins watching `root` recursively, debounced by `delay`.
///
/// Batches arrive on the returned receiver: the paths that changed, plus
/// [`WatchBatch::rescan`] for whether notify lost events along the way.
/// Delivered paths are resolved, not literal, since `root` is canonicalized
/// here before reaching notify; a caller stripping it as a literal prefix
/// must canonicalize its own copy first.
///
/// # Errors
///
/// - [`WatchError::Backend`]: notify could not create a watcher.
/// - [`WatchError::Watch`]: `root` could not be resolved or watched.
pub fn watch_tree(
    root: &Path,
    delay: Duration,
) -> Result<(WatchSource, mpsc::UnboundedReceiver<WatchBatch>), WatchError> {
    let resolved = std::fs::canonicalize(root).map_err(|err| WatchError::Watch {
        path: root.to_path_buf(),
        reason: err.to_string(),
    })?;
    let root = resolved.as_path();

    let (tx, rx) = mpsc::unbounded_channel();
    // Cloned into the handler closure below, which outlives this call: the
    // closure runs on the debouncer's own thread for as long as it lives,
    // long after `root: &Path` stops being valid to borrow.
    let watched_root = root.to_path_buf();

    let mut debouncer = new_debouncer(delay, None, move |result: DebounceEventResult| {
        // Runs on the debouncer's own OS thread.
        forward_batch(result, &watched_root, &tx);
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

/// One debounced delivery: what changed, and whether notify lost events on
/// the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchBatch {
    /// The changed paths, exactly as notify delivered them: absolute, and
    /// resolved rather than literal (see [`watch_tree`]).
    ///
    /// May be empty on a rescan on Linux; macOS attaches the watched path
    /// instead. Neither shape is meaningful on its own, which is what
    /// [`Self::rescan`] is for.
    pub paths: Vec<PathBuf>,
    /// Whether notify flagged this delivery for a rescan: it dropped events
    /// and wants the tree re-read.
    ///
    /// Carried as its own field rather than inferred: an empty path list is
    /// inotify's shape for a rescan and not macOS's, and a path equal to the
    /// watch root is macOS's shape for it but also an ordinary event on the
    /// root's own inode.
    ///
    /// Not a path, so no glob set can match it.
    pub rescan: bool,
}

/// Shapes one debounced result into a batch and forwards it. Runs on the
/// debouncer's own OS thread, not a tokio one.
///
/// A named function rather than an inline closure so a test can drive the
/// rescan shaping directly: a real inotify overflow is not something a
/// test can ask the OS for.
fn forward_batch(result: DebounceEventResult, root: &Path, tx: &mpsc::UnboundedSender<WatchBatch>) {
    match result {
        Ok(events) => {
            let mut paths: Vec<PathBuf> = Vec::new();
            let mut rescan = false;
            for debounced in events {
                // Read before `paths` moves out of the same event.
                // `.event.paths`, not `.paths` through `Deref`: moving out
                // of a deref target does not compile.
                rescan |= debounced.event.need_rescan();
                paths.extend(debounced.event.paths);
            }
            // An `Err` here means the receiver (and `WatchSource`)
            // has already been dropped. Nothing to do: this thread
            // keeps running until its own `Drop` stops it.
            let _ = tx.send(WatchBatch { paths, rescan });
        }
        Err(errors) => {
            for err in errors {
                // `root` named explicitly: with several watches armed,
                // "filesystem watch error" alone does not say which one.
                tracing::warn!(
                    root = %root.display(),
                    %err,
                    "filesystem watch error"
                );
            }
        }
    }
}

/// `err`'s rendered message, minus notify's own trailing `about [paths]`
/// clause.
///
/// [`WatchError::Watch`] already carries and renders `path` itself, so
/// keeping the clause would print it twice. If notify ever drops the
/// clause, `split_once` simply finds nothing to cut.
fn render_reason(err: &notify::Error) -> String {
    let rendered = err.to_string();
    match rendered.split_once(" about [") {
        Some((message, _)) => message.to_string(),
        None => rendered,
    }
}

/// Why a filesystem watch could not be established.
///
/// Two variants, no `#[non_exhaustive]`: notify has exactly two failure
/// points, building the backend and registering a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchError {
    /// notify could not construct a watcher for this platform's backend.
    /// Carries notify's rendered reason.
    Backend {
        /// notify's own rendered reason.
        reason: String,
    },
    /// The path could not be watched: `watch_tree` could not resolve it to
    /// a canonical path (it does not exist, or a symlink in it is broken),
    /// or notify could not begin watching it once resolved (not readable,
    /// or the backend's watch limit is exhausted). Carries the path exactly
    /// as `watch_tree` was called with it, and notify's rendered reason
    /// where notify is the one that failed.
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
    // Real time, not the paused clock: a real inotify/FSEvents/
    // ReadDirectoryChangesW delivery cannot come from a fake clock.

    use core::time::Duration;

    use std::path::PathBuf;
    use std::time::Instant;

    use notify::event::{Flag, ModifyKind};
    use notify::{Event, EventKind};
    use notify_debouncer_full::DebouncedEvent;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;

    use super::*;
    use crate::watch::real_time::{NO_EVENT_WINDOW, SMOKE_DEADLINE, TEST_DELAY};

    // `NO_EVENT_WINDOW` must stay several `TEST_DELAY`s wide, or a leaked
    // debouncer's stray batch could land after the window closes and
    // `dropping_the_source_stops_delivery` would stop catching it.
    const _: () = assert!(
        NO_EVENT_WINDOW.as_millis() >= TEST_DELAY.as_millis() * 4,
        "NO_EVENT_WINDOW must stay at least 4x TEST_DELAY, or \
         dropping_the_source_stops_delivery stops catching a leaked \
         debouncer guard"
    );

    /// Canonicalizes `dir`'s path: on macOS, `TempDir::path()` returns a
    /// `/var/...` symlink where FSEvents reports the resolved form.
    fn canonical_root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path()
            .canonicalize()
            .expect("canonicalize tempdir root")
    }

    /// Waits up to `within` for a batch satisfying `wanted`, returning every
    /// batch delivered up to and including it.
    ///
    /// A write does not always land in the next batch: FSEvents coalesces one
    /// write into one batch, while inotify may report the create, write and
    /// close separately, so the same write can arrive as two or three batches
    /// on Linux. Callers must not assume the first batch is the match.
    ///
    /// # Panics
    ///
    /// If the deadline passes with no matching batch, or the source ends
    /// first.
    async fn batches_until(
        rx: &mut UnboundedReceiver<WatchBatch>,
        within: Duration,
        wanted_desc: &str,
        wanted: impl Fn(&WatchBatch) -> bool,
    ) -> Vec<WatchBatch> {
        let deadline = Instant::now() + within;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match timeout(remaining, rx.recv()).await {
                Ok(Some(batch)) => {
                    let matched = wanted(&batch);
                    seen.push(batch);
                    if matched {
                        return seen;
                    }
                }
                Ok(None) => {
                    panic!("watch source ended before {wanted_desc} arrived; got {seen:?}")
                }
                Err(_) => {
                    panic!("no batch carried {wanted_desc} within the deadline; got {seen:?}")
                }
            }
        }
    }

    // Driven directly: no test can make the OS emit a real rescan.
    #[test]
    fn a_rescan_marker_is_forwarded_as_a_rescan_rather_than_as_a_path() {
        let root = PathBuf::from("/watched");
        let (tx, mut rx) = mpsc::unbounded_channel();

        // A marker exactly as notify delivers one on Linux: the `Rescan`
        // flag, and no path of its own.
        let rescan = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        forward_batch(
            Ok(vec![DebouncedEvent::new(rescan, Instant::now())]),
            &root,
            &tx,
        );
        assert_eq!(
            rx.try_recv().expect("a rescan marker must produce a batch"),
            WatchBatch {
                paths: Vec::new(),
                rescan: true,
            }
        );

        // Control: an ordinary batch carries its paths and is not a rescan.
        let changed = root.join("src/main.rs");
        let modified = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(changed.clone());
        forward_batch(
            Ok(vec![DebouncedEvent::new(modified, Instant::now())]),
            &root,
            &tx,
        );
        assert_eq!(
            rx.try_recv().expect("a path batch must be forwarded"),
            WatchBatch {
                paths: vec![changed],
                rescan: false,
            }
        );
    }

    // macOS attaches the watched path to its rescan marker, so emptiness
    // alone would miss every macOS rescan.
    #[test]
    fn a_rescan_marker_that_carries_a_path_is_still_a_rescan() {
        let root = PathBuf::from("/watched");
        let (tx, mut rx) = mpsc::unbounded_channel();

        // macOS's shape: the flag, with the watched root attached to it.
        let rescan = Event::new(EventKind::Other)
            .add_path(root.clone())
            .set_flag(Flag::Rescan);
        let changed = root.join("src/main.rs");
        let modified = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(changed.clone());
        forward_batch(
            Ok(vec![
                DebouncedEvent::new(modified, Instant::now()),
                DebouncedEvent::new(rescan, Instant::now()),
            ]),
            &root,
            &tx,
        );

        assert_eq!(
            rx.try_recv().expect("a rescan marker must produce a batch"),
            WatchBatch {
                paths: vec![changed, root],
                rescan: true,
            }
        );
    }

    // Used by a test that checks only elapsed time, not coalescing: whether
    // two writes share a batch depends on OS timing a loaded CI runner does
    // not guarantee. A batch arriving too early is a real bug; one arriving
    // late from load is not what this catches.
    const HOLD_TEST_DELAY: Duration = Duration::from_secs(1);

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

    // Pins both variants' rendered text directly: `Backend` has no real
    // notify failure to produce it in these tests.
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
            // notify's shape for a path-carrying reason, pre-stripped by
            // `render_reason`.
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

    /// Tests that wait on real filesystem events or real elapsed time.
    ///
    /// The inner loop skips this module with `--skip ::slow::`; the full
    /// suite still runs them because nothing here is `#[ignore]`d.
    mod slow {
        use super::*;

        // Not the first batch alone: FSEvents can deliver the arm-time event
        // for the root by itself, ahead of the write. See `batches_until`.
        #[tokio::test]
        async fn a_file_created_under_the_root_produces_a_batch_containing_it() {
            let dir = tempfile::tempdir().unwrap();
            let root = canonical_root(&dir);
            let (_source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

            let file = root.join("created.txt");
            std::fs::write(&file, b"hello").unwrap();

            batches_until(&mut rx, SMOKE_DEADLINE, &format!("{file:?}"), |batch| {
                batch.paths.contains(&file)
            })
            .await;
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

            // Not the first batch alone: on inotify it is often the two
            // directories rather than the file, with the file arriving later.
            // See `batches_until`.
            batches_until(&mut rx, SMOKE_DEADLINE, &format!("{file:?}"), |batch| {
                batch.paths.contains(&file)
            })
            .await;
        }

        // fails if the debouncer guard is leaked (kept alive past the
        // `WatchSource`'s drop): a write after the drop would still reach
        // `rx` within the window below.
        #[tokio::test]
        async fn dropping_the_source_stops_delivery() {
            let dir = tempfile::tempdir().unwrap();
            let root = canonical_root(&dir);
            let (source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

            // Prove the watch is live first, so a failure below can't be
            // confused with "this watch never worked in the first place".
            // On the write itself, not the first batch: on FSEvents that can
            // be the arm-time root event, and dropping the source on it would
            // read `first.txt`'s own late batch as a leak.
            let first = root.join("first.txt");
            std::fs::write(&first, b"hello").unwrap();
            batches_until(&mut rx, SMOKE_DEADLINE, &format!("{first:?}"), |batch| {
                batch.paths.contains(&first)
            })
            .await;

            drop(source);
            std::fs::write(root.join("second.txt"), b"hello").unwrap();

            // A stray batch naming `first.txt` is not the leak: the
            // debouncer's OS thread may emit one final batch after the
            // stop flag flips. Only a batch naming `second.txt`, written
            // after the drop, means delivery did not stop.
            let deadline = Instant::now() + NO_EVENT_WINDOW;
            // Loop, not one recv: a first.txt straggler must not hide a
            // second.txt leak arriving right after it in the same window.
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match timeout(remaining, rx.recv()).await {
                    Err(_) => break,   // window elapsed with nothing further, expected
                    Ok(None) => break, // sender dropped alongside the debouncer thread, expected
                    Ok(Some(batch)) => assert!(
                        !batch.paths.iter().any(|p| p.ends_with("second.txt")),
                        "unexpected batch delivered after WatchSource was dropped: {batch:?}"
                    ),
                }
            }
        }

        // fails if `watch_tree` ignores `delay` for a hardcoded window:
        // both writes happen before either is observed, so a too-long
        // hardcoded window would still be running when `second.txt` lands.
        #[tokio::test]
        async fn writes_separated_by_more_than_delay_produce_two_batches() {
            let dir = tempfile::tempdir().unwrap();
            let root = canonical_root(&dir);
            let (_source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

            let first = crate::testing::touch(&root, "first.txt").unwrap();
            tokio::time::sleep(TEST_DELAY * 4).await;
            let second = crate::testing::touch(&root, "second.txt").unwrap();

            // The property is that the two writes did not *share* a batch, not
            // that they produced exactly two. Linux delivers `first.txt` more
            // than once for one write, so counting batches would fail on a
            // backend difference rather than on the behaviour under test.
            let batches = batches_until(&mut rx, SMOKE_DEADLINE, &format!("{second:?}"), |batch| {
                batch.paths.contains(&second)
            })
            .await;
            assert!(
                !batches
                    .iter()
                    .any(|b| b.paths.contains(&first) && b.paths.contains(&second)),
                "the two writes coalesced into one batch despite the gap \
             exceeding `delay` — got {batches:?}"
            );
            assert!(
                batches.iter().any(|b| b.paths.contains(&first)),
                "expected {first:?} in an earlier batch than {second:?}, got {batches:?}"
            );
        }

        #[tokio::test]
        async fn no_batch_arrives_before_delay_has_elapsed() {
            let dir = tempfile::tempdir().unwrap();
            let root = canonical_root(&dir);
            let (_source, mut rx) = watch_tree(&root, HOLD_TEST_DELAY).unwrap();

            let started = Instant::now();
            let file = crate::testing::touch(&root, "first.txt").unwrap();

            let batches = batches_until(&mut rx, HOLD_TEST_DELAY * 4, &format!("{file:?}"), |b| {
                b.paths.contains(&file)
            })
            .await;
            assert!(
                started.elapsed() >= HOLD_TEST_DELAY,
                "a batch arrived after {:?}, inside `delay` ({HOLD_TEST_DELAY:?}) -- \
                 got {batches:?}",
                started.elapsed()
            );
        }

        #[tokio::test]
        async fn a_path_deleted_while_watched_still_produces_a_batch() {
            let dir = tempfile::tempdir().unwrap();
            let root = canonical_root(&dir);
            let file = crate::testing::touch(&root, "doomed.txt").unwrap();
            let (_source, mut rx) = watch_tree(&root, TEST_DELAY).unwrap();

            std::fs::remove_file(&file).unwrap();

            // Not the first batch alone, for the reason the root-level write
            // case above gives: FSEvents' arm-time event for the root can be
            // a batch of its own ahead of the removal.
            batches_until(&mut rx, SMOKE_DEADLINE, &format!("{file:?}"), |batch| {
                batch.paths.contains(&file)
            })
            .await;
        }

        // fails if a caller can assume delivered paths share root's own
        // literal form.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_symlinked_root_delivers_the_resolved_path_not_the_one_passed_in() {
            let target = tempfile::tempdir().unwrap();
            let resolved_target = canonical_root(&target);
            let link_parent = tempfile::tempdir().unwrap();
            let link = link_parent.path().join("link-to-target");
            std::os::unix::fs::symlink(&resolved_target, &link).unwrap();

            // Watch through the symlink, left un-canonicalized.
            let (_source, mut rx) = watch_tree(&link, TEST_DELAY).unwrap();
            crate::testing::touch(&link, "through-the-link.txt").unwrap();

            // Every batch up to the one naming the file, and the literal-form
            // check runs over all of them: a root-only batch spelled through
            // the link would be the same trap one batch earlier.
            let resolved_file = resolved_target.join("through-the-link.txt");
            let batches = batches_until(
                &mut rx,
                SMOKE_DEADLINE,
                &format!("{resolved_file:?}"),
                |batch| batch.paths.contains(&resolved_file),
            )
            .await;
            for batch in &batches {
                assert!(
                    !batch.paths.iter().any(|p| p.starts_with(&link)),
                    "expected no path under the symlink itself, got {batch:?}"
                );
            }
        }
    }
}
