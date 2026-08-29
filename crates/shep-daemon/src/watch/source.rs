//! [`WatchSource`] — notify's debounced filesystem events, bridged onto a
//! tokio channel (spec §4).
//!
//! The debouncer's own handler runs on its own OS thread, never on a tokio
//! one. [`watch_tree`] wraps that handler in a plain closure that forwards
//! each debounced batch through an unbounded [`mpsc`] sender — non-blocking
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
/// Batches arrive on the returned receiver. A batch is whatever the debouncer
/// coalesced within `delay`: the paths it saw change, plus
/// [`WatchBatch::rescan`] for whether notify lost events along the way. The
/// two travel together rather than one standing in for the other — see that
/// field for why a rescan cannot be spelled as a path.
///
/// **Delivered paths are resolved, not literal.** If `root` is, or passes
/// through, a symlink, every path in every batch still comes back through
/// the real directory — never through the symlink `root` was given as, and
/// proven by this module's own
/// `a_symlinked_root_delivers_the_resolved_path_not_the_one_passed_in` test.
/// A caller that strips `root` as a literal path prefix (`WatchFilter`) must
/// canonicalize its own copy of `root` first (as `extras.rs`'s `arm_watch`
/// does), or a delivered path never looks like it lies under the tree it was
/// just reported from — this function canonicalizes only the copy it hands
/// to the OS watch, not anything the caller goes on to hold.
///
/// The guarantee is this function's own, not the backend's: `root` is
/// resolved here, before it is ever handed to notify, rather than trusted to
/// arrive already resolved or to come back resolved on its own. FSEvents
/// happens to resolve symlinks on macOS regardless, but inotify on Linux
/// builds every delivered path by joining whatever root it was actually
/// armed with — arm it with a symlink and every batch carries that symlink
/// right back. Canonicalizing here is what makes the promise hold on both.
///
/// # Errors
///
/// - [`WatchError::Backend`] — notify could not create a watcher.
/// - [`WatchError::Watch`] — `root` could not be resolved to a canonical
///   path, or notify could not watch it once resolved. Either way the
///   error carries `root` exactly as this function was called with it.
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
        // Runs on the debouncer's own OS thread — see this module's doc.
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
    /// The changed paths, exactly as notify delivered them — absolute, and
    /// resolved rather than literal (see [`watch_tree`]).
    ///
    /// May be empty on a rescan, and on some platforms may not be: macOS
    /// attaches the watched path to its `MustScanSubDirs` marker where Linux
    /// leaves an inotify overflow path-less. Neither shape says anything the
    /// caller can act on, which is what [`Self::rescan`] is for.
    pub paths: Vec<PathBuf>,
    /// Whether notify flagged this delivery for a rescan: it dropped events
    /// and wants the tree re-read.
    ///
    /// Carried as its own field rather than inferred, because the two things
    /// a caller might infer it from are both wrong. An empty path list is
    /// inotify's shape for it and not macOS's. A path equal to the watch root
    /// is macOS's shape for it *and* an ordinary event the OS delivers for
    /// the root's own inode — so reading one as a rescan restarts an app for
    /// a `chmod` no glob set was ever asked about.
    ///
    /// Not a path, and no glob set can be matched against it: the paths that
    /// changed during the gap are exactly what nobody knows.
    pub rescan: bool,
}

/// Shapes one debounced result into a batch and forwards it — the body of
/// the handler [`watch_tree`] installs, and so the code that runs on the
/// debouncer's own OS thread rather than on a tokio one.
///
/// A function rather than an inline closure because the batch it produces
/// for a rescan is the only signal an inotify overflow leaves, and a real
/// overflow is not something a test can ask an OS for: named here, the
/// shaping can be driven directly.
fn forward_batch(result: DebounceEventResult, root: &Path, tx: &mpsc::UnboundedSender<WatchBatch>) {
    match result {
        Ok(events) => {
            let mut paths: Vec<PathBuf> = Vec::new();
            let mut rescan = false;
            for debounced in events {
                // notify's own accessor for the `Rescan` flag, read before
                // the paths below move out of the same event. `.event.paths`
                // rather than `.paths` through the `Deref`: moving out of a
                // deref target does not compile, and `DebouncedEvent`
                // exposes its wrapped `Event` as a plain owned field for
                // exactly this.
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
    // IR-33: real time, not the paused clock. This is the phase's OS seam,
    // and what it proves — a real inotify/FSEvents/ReadDirectoryChangesW
    // delivery — cannot come from a fake clock. Mirrors `probes::os`'s own
    // real-time justification.

    use core::time::Duration;

    use std::path::PathBuf;
    use std::time::Instant;

    use notify::event::{Flag, ModifyKind};
    use notify::{Event, EventKind};
    use notify_debouncer_full::DebouncedEvent;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;

    use super::*;
    // The subsystem's shared real-time constants, owned by `watch` itself
    // rather than re-declared per suite — see that module for why.
    use crate::watch::real_time::{NO_EVENT_WINDOW, SMOKE_DEADLINE, TEST_DELAY};

    // `dropping_the_source_stops_delivery` only catches a leaked debouncer
    // because `NO_EVENT_WINDOW` outlasts `TEST_DELAY` by a wide margin: a
    // leaked debouncer keeps debouncing on `TEST_DELAY`, so its stray batch
    // for `second.txt` lands roughly one `TEST_DELAY` after the write, well
    // inside the window. Someone who later hits CI flake on that test and
    // "fixes" it by raising `TEST_DELAY` — without also raising that window
    // — silently deletes the guard: at `TEST_DELAY` close to or past
    // `NO_EVENT_WINDOW`, the stray batch would arrive *after* the window
    // closes and the leak would pass undetected. This assertion turns that
    // edit into a compile error instead of a silent regression.
    const _: () = assert!(
        NO_EVENT_WINDOW.as_millis() >= TEST_DELAY.as_millis() * 4,
        "NO_EVENT_WINDOW must stay at least 4x TEST_DELAY, or \
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

    /// Waits up to [`SMOKE_DEADLINE`] for the next batch.
    async fn expect_batch(rx: &mut UnboundedReceiver<WatchBatch>) -> WatchBatch {
        timeout(SMOKE_DEADLINE, rx.recv())
            .await
            .expect("no batch arrived within the deadline")
            .expect("watch source ended before a batch arrived")
    }

    /// Waits up to `within` for a batch satisfying `wanted`, and returns every
    /// batch delivered up to and including it.
    ///
    /// `within` rather than [`SMOKE_DEADLINE`] for everyone: a test whose own
    /// debounce window is wider than the shared deadline would time out before
    /// its first batch could legitimately arrive.
    ///
    /// A test must not assume a write lands in the *next* batch. The debouncer
    /// merges events into one handler call only while they share a `tick_rate`
    /// slice (`delay / 4`), and the backends disagree about how many events a
    /// single write even produces: FSEvents coalesces in the kernel, while
    /// inotify reports the create, the write and the close separately, and the
    /// parent directory's change besides. One `fs::write` therefore arrives as
    /// one batch on macOS and as two or three on Linux, in whatever order the
    /// tick boundary happens to fall.
    ///
    /// Asserting on the first batch alone was measured at 4 failures in 10
    /// serial runs on an *idle* Linux box, so this is not load: it is a real
    /// difference in what the backends report, and no amount of quiet fixes
    /// it.
    ///
    /// # Panics
    ///
    /// If the deadline passes with no matching batch, or the source ends
    /// first. Either message names what was wanted and every batch that did
    /// arrive.
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

    // fails if [`forward_batch`] stops reading notify's own `Rescan` flag —
    // `let rescan = false;`, or a rescan inferred from `paths.is_empty()`.
    // A rescan means notify dropped events and wants the tree re-read, and it
    // is the one signal the group loop must act on without consulting a glob
    // set; losing it leaves the watch quiet exactly when it knows least.
    //
    // The one case that enters the code PRODUCING that marker. The rescan case
    // in `watch`'s own tests fabricates a flagged batch downstream, so it pins
    // the consumer and leaves this side of the contract free to change.
    //
    // Driven directly rather than through a real watcher because the OS is
    // what emits a rescan (an inotify queue overflow, an FSEvents drop), and
    // no test can ask it for one.
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

        // Control: an ordinary batch carries its paths and is NOT a rescan,
        // so the flag above belongs to the marker alone rather than being set
        // for every delivery.
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

    // fails if the rescan flag is inferred from an empty path list rather than
    // read from notify. macOS attaches the watched path to its
    // `MustScanSubDirs` marker, so on that platform an inference from emptiness
    // reports every real rescan as an ordinary event on the root, and the tree
    // is never re-read.
    //
    // Also fails if a mixed batch loses the flag: the debouncer coalesces
    // whatever landed inside one `delay`, so a marker very often arrives
    // alongside the paths notify did manage to keep.
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

    // The window `no_batch_arrives_before_delay_has_elapsed` measures against.
    //
    // Deliberately does NOT assert coalescing (that two writes issued back to
    // back share one batch): whether two events land in the same batch
    // depends on the debouncer merging them inside one `tick_rate` slice
    // (`delay / 4`), which depends in turn on the OS delivering both within
    // that slice, and a loaded CI runner is under no obligation to.
    //
    // What is left is the half shep actually owns and load cannot break. The
    // regression this test is written for is `delay` being ignored -- passed
    // as zero, or flushed on every event regardless -- and either shows up as
    // a batch arriving too EARLY. A stalled machine can only push a batch
    // later, never sooner, so the assertion has no race in it at all.
    //
    // Coalescing itself is `notify-debouncer-full`'s behaviour rather than
    // ours; shep's only lever on it is the `delay` it passes, which is
    // exactly what the elapsed-time bound checks.
    const HOLD_TEST_DELAY: Duration = Duration::from_secs(1);

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

    /// Tests that wait on real filesystem events or real elapsed time.
    ///
    /// The inner loop skips this module with `--skip ::slow::`; the full
    /// suite still runs them because nothing here is `#[ignore]`d.
    mod slow {
        use super::*;

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
                batch.paths.contains(&file),
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

            // Not `expect_batch`: on inotify the first batch is often the two
            // directories rather than the file -- measured as
            // `paths: ["<root>/a", "<root>/a/b"]` -- with the file arriving in
            // a later one. See `batches_until`.
            batches_until(&mut rx, SMOKE_DEADLINE, &format!("{file:?}"), |batch| {
                batch.paths.contains(&file)
            })
            .await;
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
            // and a closed channel (the debouncer thread already tore down
            // and dropped its sender) are honest readings of "delivery
            // stopped".
            //
            // A batch naming `first.txt` is NOT the leak: `WatchSource`'s
            // own doc says the debouncer's OS thread "may still emit one
            // final batch" after the stop flag flips, and a loaded machine
            // can duplicate the raw fs event behind `first.txt`'s own
            // (already-consumed) write — observed directly, both here and
            // on CI, as a stray batch containing `first.txt` and nothing
            // else. The one honest reading of "the drop leaked" is a batch
            // that names `second.txt`, the write that happens after the
            // drop; only that fails the test. Loop rather than a single
            // `recv`, so a harmless `first.txt` straggler can't hide a real
            // `second.txt` leak that follows it within the same window.
            let deadline = Instant::now() + NO_EVENT_WINDOW;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match timeout(remaining, rx.recv()).await {
                    Err(_) => break,   // window elapsed with nothing further — expected
                    Ok(None) => break, // sender dropped alongside the debouncer thread — expected
                    Ok(Some(batch)) => assert!(
                        !batch.paths.iter().any(|p| p.ends_with("second.txt")),
                        "unexpected batch delivered after WatchSource was dropped: {batch:?}"
                    ),
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
                batch.paths.contains(&file),
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
                batch.paths.contains(&resolved_file),
                "expected the resolved path {resolved_file:?} in {batch:?}"
            );
            assert!(
                !batch.paths.iter().any(|p| p.starts_with(&link)),
                "expected no path under the symlink itself, got {batch:?}"
            );
        }
    }
}
