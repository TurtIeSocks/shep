//! The bounded log reader behind the bleats feed, and the gap it admits to.
//!
//! Design decision 1 (`shep lookout` reads a sheep's log files from disk
//! rather than subscribing to the `log.*` bus topic) made concrete: a window
//! from the end of each file, a line cap on top of that window, and an exact
//! count of what the window and the cap left out. [`read`] is a pure function
//! over the filesystem — no bus, no shepherd, no dashboard — which is what
//! lets its own tests drive it with a [`std::collections::BTreeMap`] and a
//! `tempdir`.
//!
//! **The reader is bounded by itself, not by the sheep writing the file.** A
//! sheep writing 100 MB/s costs one seek and one 64 KiB read per refresh, the
//! same as a sheep writing nothing — see [`FEED_WINDOW_BYTES`] and
//! [`read_window`]'s `.take(..)`. Everything the window and the line cap left
//! out is counted rather than silently dropped: [`Tail::missed_lines`] for
//! what the reader saw and discarded, [`Tail::missed_bytes`] for what it
//! never read at all. A pane built on a reader that could not say "I skipped
//! N" would look complete exactly when the flock was busy, which is when
//! someone is watching it.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use crate::output::human_bytes;

/// The most of one log file this pane will read to find its lines.
///
/// 64 KiB, a quarter of `commands::bleats`' own `TAIL_WINDOW_BYTES`: that path
/// shows fifty lines of a one-shot command, this one shows five lines of a
/// pane, and this read happens every two seconds for the life of the
/// dashboard rather than once. Two files at 64 KiB every two seconds is
/// 64 KiB/s of reads, whatever the flock writes.
pub const FEED_WINDOW_BYTES: u64 = 64 * 1024;

/// The most lines one file contributes, once the window is split.
///
/// A byte window alone cannot bound the line count — 64 KiB of newlines is
/// 65536 lines — and a line count alone cannot bound memory, since one
/// arbitrarily long line with no newline defeats it. Both bounds, for the same
/// reason `bleats` carries both.
pub const FEED_TAIL_LINES: usize = 40;

/// Which of a sheep's two output streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// stdout.
    Out,
    /// stderr. **Not an error.** Most runtimes log there by default, which is
    /// why the feed renders this tag muted rather than in `--bark` — that
    /// colour means errored, refused and destructive and nothing else.
    Err,
}

/// One line, with the stream it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailLine {
    /// Which file it was in.
    pub stream: Stream,
    /// The line, without its terminator. Decoded with
    /// [`String::from_utf8_lossy`]: a log file is whatever the child wrote and
    /// is under no obligation to be UTF-8, and refusing to show a log over one
    /// bad byte is the wrong failure. `bleats` makes and states the same call.
    pub text: String,
}

/// What one refresh of the feed found.
///
/// **Two miss counters, not one, and the reason is the whole of design
/// decision 1.** Lines go missing in three places: below the byte window
/// (unknowable in lines, exact in bytes), above [`FEED_TAIL_LINES`] inside the
/// window (exact), and above the five rows the pane has (exact, and the pane's
/// own to compute). The first draft of this plan counted only the first, which
/// is the RARE case; the ordinary case — a sheep writing thirty lines between
/// two polls, overrunning no window at all — went unreported, and the pane
/// looked complete exactly when the flock was busy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    /// The newest lines, oldest first — `out`'s tail, then `err`'s.
    ///
    /// **There is no merge, and that is stated rather than hidden.** A log
    /// line does now carry the time the daemon wrote it, so a key exists —
    /// but nothing here reads it, and until something does, guessing an order
    /// from file order would be wrong exactly when a sheep writes to both at
    /// once. `bleats`' module doc records the same limitation for the same
    /// reason. The pane renders the LAST rows of this
    /// list, so a crash on stderr survives a chatty stdout — and its header
    /// says `out then err` rather than `out+err`, because `+` reads as one
    /// merged stream and this is two files end to end.
    pub lines: Vec<TailLine>,
    /// Lines this read **saw and discarded**, summed over both files.
    ///
    /// Those above [`FEED_TAIL_LINES`], plus one for the partial line a window
    /// boundary cut in half. Exact, and non-zero in the ordinary case: a
    /// window holds hundreds of lines and the cap keeps forty.
    ///
    /// The partial line is counted as **one** whether or not the boundary
    /// happened to fall exactly between two lines — the reader cannot tell
    /// without reading a byte it deliberately did not read, and over-counting
    /// by one is the safe direction. Claiming completeness is the failure this
    /// counter exists to prevent; being one pessimistic is not.
    pub missed_lines: usize,
    /// Bytes appended since the previous read that fell **below** the window
    /// and were therefore never read at all.
    ///
    /// Exact as a byte count. The number of LINES in them is genuinely
    /// unknowable — reading them is the thing the window exists to avoid — so
    /// the pane says "was never read" about these rather than putting a line
    /// count on them it would have to invent.
    ///
    /// Zero on the first read of a file: showing the tail of a file's history
    /// is not a gap *between two reads*, and a four-megabyte notice every time
    /// an operator selected a long-running sheep would train them to ignore
    /// the notice. Nothing is hidden by that, because the lines the window and
    /// the cap dropped are still in [`Self::missed_lines`]. Zero too when the
    /// file shrank, which is what a rotation or a `shep flush` looks like from
    /// here.
    pub missed_bytes: u64,
    /// Bytes this refresh actually pulled off disk, both files together.
    ///
    /// The bound design decision 3 claims, exposed so the tests can assert it
    /// **live** rather than argue it in a comment. Never above
    /// `2 * FEED_WINDOW_BYTES`, and — this is the part a static fixture cannot
    /// check — not above it even while the sheep is writing during the read.
    pub read_bytes: u64,
    /// Why there is nothing to show, when there is nothing to show.
    ///
    /// A sentence that names the CAUSE, not one that restates the fact. "the
    /// feed is empty" tells an operator nothing they cannot see; "this sheep
    /// has not written a log in this $SHEP_HOME" tells them whether to worry.
    pub note: Option<String>,
}

/// One refresh: both files, tagged, with the gap admitted to.
///
/// `seen` is the caller's memory of each file's length at the previous read —
/// [`super::source::LocalReader`] owns it. It is threaded in rather than held
/// here because this function is otherwise pure over the filesystem, which is
/// what lets its tests drive it with a `BTreeMap` and a `tempdir` and no
/// dashboard at all.
///
/// `std::fs`, not `tokio::fs`: shep-cli's tokio does not carry the `fs`
/// feature, and this is a bounded read on a task that is otherwise asleep.
/// `commands::bleats` makes and states the same call.
pub fn read(seen: &mut BTreeMap<PathBuf, u64>, out: Option<&Path>, err: Option<&Path>) -> Tail {
    let mut tail = Tail::default();
    let mut notes: Vec<String> = Vec::new();

    if out.is_none() && err.is_none() {
        tail.note = Some("the shepherd did not report a log path for this sheep".to_string());
        return tail;
    }

    for (stream, path) in [(Stream::Out, out), (Stream::Err, err)] {
        let Some(path) = path else { continue };
        match read_window(seen, path) {
            Ok(window) => {
                tail.read_bytes = tail.read_bytes.saturating_add(window.read_bytes);
                tail.missed_bytes = tail.missed_bytes.saturating_add(window.never_read);
                tail.missed_lines = tail.missed_lines.saturating_add(window.dropped);
                tail.lines.extend(
                    window
                        .lines
                        .into_iter()
                        .map(|text| TailLine { stream, text }),
                );
            }
            // The shepherd creates both files at spawn, so a missing one means
            // this sheep has never run in this `$SHEP_HOME` — a fact about the
            // flock, not a failure of the read.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => notes.push(format!(
                "this sheep has not written a log in this $SHEP_HOME ({})",
                path.display()
            )),
            Err(err) => notes.push(format!("could not read {}: {err}", path.display())),
        }
    }

    if tail.lines.is_empty() {
        tail.note = Some(if !notes.is_empty() {
            notes.join("; ")
        } else if tail.read_bytes > 0 {
            // Read plenty and found no terminator in any of it: one line
            // longer than the whole window. "has written nothing yet" would be
            // flatly false here, and false in the direction an operator acts
            // on — they would go looking for a sheep that was never started
            // instead of at a sheep writing one enormous line.
            format!(
                "this sheep's last {} of log contains no complete line",
                human_bytes(FEED_WINDOW_BYTES)
            )
        } else {
            "this sheep has written nothing yet".to_string()
        });
    }
    tail
}

/// What one file's window yielded.
///
/// A struct rather than a tuple: four returns, three of them counters that
/// differ only in what they count, is exactly the shape where positional
/// returns get transposed at the call site and nothing complains.
struct Window {
    /// The lines that survived both bounds, oldest first.
    lines: Vec<String>,
    /// Lines this read saw and discarded: those above [`FEED_TAIL_LINES`],
    /// plus one for the partial line the window boundary cut.
    dropped: usize,
    /// Bytes appended since the previous read that fell below the window and
    /// were never read.
    never_read: u64,
    /// Bytes this read pulled off disk. Never above [`FEED_WINDOW_BYTES`].
    read_bytes: u64,
}

/// One file's window: the last [`FEED_TAIL_LINES`] lines of it, what it had to
/// discard to get there, and what it never read at all.
///
/// # Errors
/// The file could not be opened, `stat`ed, seeked or read — notably
/// [`std::io::ErrorKind::NotFound`] and `EISDIR`, which [`read`] treats
/// differently from each other.
fn read_window(seen: &mut BTreeMap<PathBuf, u64>, path: &Path) -> std::io::Result<Window> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(FEED_WINDOW_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }

    // `.take(..)`, and this one call is what design decision 3's entire bound
    // rests on. `read_to_end` alone reads to the file's CURRENT end, not to
    // the `len` that was just `stat`ed — so a sheep appending while this read
    // is in flight makes both the read and this `Vec` grow without limit, on
    // the UI task. That is the writer-bounded behaviour decision 1 chose files
    // over the bus to avoid, reintroduced by one missing call, and no fixture
    // that has stopped growing can catch it.
    let mut window = Vec::with_capacity(usize::try_from(len.min(FEED_WINDOW_BYTES)).unwrap_or(0));
    (&mut file)
        .take(FEED_WINDOW_BYTES)
        .read_to_end(&mut window)?;
    let read_bytes = u64::try_from(window.len()).unwrap_or(u64::MAX);

    // A window boundary can land mid-line. Half a line shown as a whole one is
    // a lie, so the bytes up to and including the first newline are discarded
    // — and COUNTED, because a discarded line is one the pane is not showing.
    //
    // Counted as one whether or not the boundary happened to fall exactly
    // between two lines: telling those apart needs the byte at `start - 1`,
    // which is a byte this function deliberately did not read. Over-counting
    // by one is the safe direction; claiming completeness is not.
    let mut dropped = usize::from(start > 0);
    let bytes: &[u8] = if start > 0 {
        match window.iter().position(|&byte| byte == b'\n') {
            Some(newline) => &window[newline + 1..],
            // No newline in a whole window: it is all the middle of one line.
            None => &[],
        }
    } else {
        &window
    };

    let text = String::from_utf8_lossy(bytes);
    // Stripped here for the same reason `commands::bleats::read_tail` strips
    // it: this pane and the live feed show one sheep's output side by side,
    // and a line that grew a 30-character prefix on only one of those two
    // paths would read as two different sheep.
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| shep_core::logstamp::strip(line).to_string())
        .collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let keep_from = lines.len().saturating_sub(FEED_TAIL_LINES);
    dropped += keep_from;
    lines.drain(..keep_from);

    // Bytes that were NEVER READ: those appended since the previous read that
    // fell below the window. Not `len - previous - covered`, which the first
    // draft used: that form counts the bytes of the lines this reader saw and
    // dropped, which `dropped` already counts in lines, so the two
    // double-count each other and neither is exactly anything.
    //
    // `saturating_sub`, so a file that SHRANK — a rotation, a `shep flush` —
    // reports zero rather than sixteen exabytes.
    let previous = seen.insert(path.to_path_buf(), len);
    let never_read = match previous {
        // The first read of a file shows the tail of its history. That is not
        // a gap BETWEEN READS, and a four-megabyte notice every time an
        // operator selected a long-running sheep would train them to ignore
        // the notice. Nothing is hidden by this: the lines the window and the
        // cap dropped are in `dropped` either way.
        None => 0,
        Some(previous) => start.saturating_sub(previous),
    };
    Ok(Window {
        lines,
        dropped,
        never_read,
        read_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write as _; // `writeln!` into a `File`

    use super::*;

    /// fails if the reader stops being bounded by the READER rather than the
    /// writer. This is the property design decision 1 chose files over the bus
    /// for: a sheep writing four megabytes between two refreshes must cost one
    /// seek and one window, exactly as a silent sheep does.
    ///
    /// A live assertion, not a `tokio::time::timeout` — `read` is synchronous,
    /// so a timer around it would complete on the first poll and bound
    /// nothing. What can actually go wrong here is unbounded growth, so that
    /// is what is asserted — **on `read_bytes`, the quantity that would
    /// grow**, and not on the size of what came back. Forty short lines are
    /// under 64 KiB for any implementation whatsoever, including one that read
    /// the whole four megabytes and then threw them away; an assertion that
    /// cannot distinguish those two is not asserting the bound.
    #[test]
    fn a_four_megabyte_file_costs_one_window_and_forty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        let line = "x".repeat(120);
        let mut body = String::new();
        while body.len() < 4 * 1024 * 1024 {
            body.push_str(&line);
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&path), None);

        assert!(
            tail.read_bytes <= FEED_WINDOW_BYTES,
            "the reader pulled {} bytes off a 4 MiB file",
            tail.read_bytes
        );
        assert_eq!(tail.lines.len(), FEED_TAIL_LINES);
        assert_eq!(
            tail.missed_bytes, 0,
            "the first read of a file is not a gap BETWEEN READS"
        );
        // …and that is only defensible because the lines it did drop are
        // counted. Without this the pane draws five lines of four million and
        // says nothing.
        assert!(
            tail.missed_lines > 400,
            "a 64 KiB window of 121-byte lines holds ~540 of them and keeps 40; \
             counted only {}",
            tail.missed_lines
        );
    }

    /// fails if the reader stops counting the lines it discarded to honour
    /// [`FEED_TAIL_LINES`]. **This is the ordinary case the first draft of
    /// this plan missed entirely**, and it is the one that matters: sixty
    /// lines fit comfortably inside one 64 KiB window, so `missed_bytes` is
    /// zero and nothing about the byte accounting fires — while twenty of
    /// those lines are dropped by the cap. A pane handed a zero here draws
    /// five lines of sixty and looks complete, which it does exactly when the
    /// flock is busy and someone is watching.
    #[test]
    fn the_lines_the_cap_dropped_are_counted_even_when_no_bytes_were_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        let sixty: String = (0..60).map(|n| format!("line-{n}\n")).collect();
        assert!(
            sixty.len() < usize::try_from(FEED_WINDOW_BYTES).unwrap(),
            "the fixture has to sit well inside one window or it tests the wrong thing"
        );
        std::fs::write(&path, &sixty).unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&path), None);
        assert_eq!(tail.missed_bytes, 0, "nothing overran the window");
        assert_eq!(tail.lines.len(), FEED_TAIL_LINES);
        assert_eq!(tail.missed_lines, 20, "sixty in, forty kept");
        assert_eq!(tail.lines[0].text, "line-20", "and it is the NEWEST forty");
    }

    /// fails if the reader is bounded by the WRITER rather than by itself.
    ///
    /// `read_to_end` after a seek reads to the file's CURRENT end, not to the
    /// `len` that was just `stat`ed — so a sheep appending while the read is
    /// in flight makes both the read and its `Vec` grow past 64 KiB without
    /// limit, on the UI task. That is precisely the writer-bounded behaviour
    /// design decision 1 chose files over the bus to avoid, reintroduced by
    /// one missing call.
    ///
    /// **A static fixture cannot catch it**, however large: by the time the
    /// test runs the file has stopped growing, and `len` and the true end
    /// agree. So this one keeps a writer appending for the whole read.
    ///
    /// IR-46: bounded by construction — a fixed number of appends, a fixed
    /// number of reads, and the writer joined before the assertions. Nothing
    /// here waits on a condition, so it cannot hang whatever the reader does.
    #[test]
    fn a_file_that_grows_during_the_read_is_still_bounded_by_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        // A megabyte to start with, so every read takes the seek branch rather
        // than the whole-file one.
        std::fs::write(&path, "seed\n".repeat(200_000)).unwrap();

        let writing = path.clone();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writing)
                .unwrap();
            let chunk = "w".repeat(64 * 1024 - 1);
            for _ in 0..512 {
                writeln!(file, "{chunk}").unwrap();
            }
        });

        let mut seen = BTreeMap::new();
        let mut worst = 0;
        for _ in 0..200 {
            worst = worst.max(read(&mut seen, Some(&path), None).read_bytes);
        }
        writer.join().unwrap();

        assert!(
            worst <= FEED_WINDOW_BYTES,
            "one read pulled {worst} bytes off a file that was still being written"
        );
        // And the writer actually ran: a green run on a file that never grew
        // would be this test passing for the wrong reason.
        assert!(
            std::fs::metadata(&path).unwrap().len() > 32 * 1024 * 1024,
            "the writer did not get far enough for this test to mean anything"
        );
    }

    /// fails if the gap notice stops being exact, or stops appearing at all.
    /// This is the answer to "what happens under a flock writing faster than
    /// the terminal can draw", and it is the half that has to be visible.
    #[test]
    fn a_file_that_grew_between_reads_reports_the_bytes_it_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let mut seen = BTreeMap::new();
        let first = read(&mut seen, Some(&path), None);
        assert_eq!(first.missed_bytes, 0);
        assert_eq!(first.lines.len(), 2);

        // Four megabytes of burst, then two lines the pane will actually show.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let burst = "y".repeat(4 * 1024 * 1024);
        writeln!(file, "{burst}").unwrap();
        writeln!(file, "three").unwrap();
        writeln!(file, "four").unwrap();
        drop(file);

        let second = read(&mut seen, Some(&path), None);
        // `missed_bytes` is what was NEVER READ — everything appended below
        // the window. The last 64 KiB WAS read, so it is not in this number.
        // That upper bound is what distinguishes this definition from
        // `len - previous - covered`, which the first draft used and which
        // double-counted the lines the reader dropped: that form would return
        // slightly MORE than the whole burst and redden the second assertion.
        assert!(
            second.missed_bytes > 4 * 1024 * 1024 - 2 * FEED_WINDOW_BYTES,
            "got {}",
            second.missed_bytes
        );
        assert!(
            second.missed_bytes < 4 * 1024 * 1024,
            "the last window WAS read, so it does not belong in the gap: {}",
            second.missed_bytes
        );
        assert_eq!(
            second.lines.last().unwrap().text,
            "four",
            "the NEWEST lines survive"
        );
        assert_eq!(
            second.missed_lines, 1,
            "the four-megabyte line the window cut in half is one line, counted"
        );

        // A third read with nothing appended reports no gap between reads —
        // but still reports the line the window cuts, because that is still
        // true, and a pane that stopped saying so would start claiming a
        // completeness it does not have.
        let third = read(&mut seen, Some(&path), None);
        assert_eq!(third.missed_bytes, 0);
        assert_eq!(third.missed_lines, 1);
    }

    /// fails if a file that SHRANK is reported as a gap. A rotation or a
    /// `shep flush` makes the file smaller between two reads, and a subtraction
    /// that wrapped would claim sixteen exabytes were skipped.
    #[test]
    fn a_truncated_file_reports_no_gap_and_re_reads_from_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        let mut seen = BTreeMap::new();
        let _ = read(&mut seen, Some(&path), None);

        std::fs::write(&path, "fresh\n").unwrap();
        let after = read(&mut seen, Some(&path), None);
        assert_eq!(after.missed_bytes, 0);
        assert_eq!(after.missed_lines, 0, "and nothing was dropped either");
        assert_eq!(after.lines.len(), 1);
        assert_eq!(after.lines[0].text, "fresh");
    }

    /// fails if a window boundary landing mid-line renders half a line as a
    /// whole one. `bleats::read_tail` makes the same discard for the same
    /// reason, and it is not cosmetic: half a log line shown as complete is a
    /// lie an operator will act on.
    #[test]
    fn a_window_boundary_discards_the_partial_line_it_lands_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        let filler = "z".repeat(usize::try_from(FEED_WINDOW_BYTES).unwrap());
        std::fs::write(&path, format!("{filler}PARTIAL-HEAD\nwhole-line\n")).unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&path), None);
        assert!(
            !tail.lines.iter().any(|l| l.text.contains("PARTIAL")),
            "a line cut by the window must be dropped, not shown: {:?}",
            tail.lines
        );
        assert_eq!(tail.lines.last().unwrap().text, "whole-line");
        assert_eq!(
            tail.missed_lines, 1,
            "dropped is not the same as hidden: the cut line is counted"
        );
    }

    /// fails if the two files stop being distinguishable, or if stderr stops
    /// coming last. The pane renders the LAST rows of this list, so `err`
    /// being last is what makes a crash survive a chatty stdout.
    #[test]
    fn both_streams_are_tagged_and_stderr_comes_last() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("web-out.log");
        let err = dir.path().join("web-err.log");
        std::fs::write(&out, "hello\n").unwrap();
        std::fs::write(&err, "panicked at 'boom'\n").unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&out), Some(&err));
        assert_eq!(
            tail.lines[0],
            TailLine {
                stream: Stream::Out,
                text: "hello".to_string()
            }
        );
        assert_eq!(
            tail.lines.last().unwrap(),
            &TailLine {
                stream: Stream::Err,
                text: "panicked at 'boom'".to_string()
            }
        );
        assert_eq!(tail.note, None, "there was something to show");
    }

    /// fails if an empty feed stops saying WHY it is empty. Three different
    /// causes, three different sentences — 12a shipped a caption claiming a
    /// sentence said why when it only stated the fact, and this is the same
    /// mistake one layer down.
    #[test]
    fn each_reason_the_feed_is_empty_gets_its_own_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen = BTreeMap::new();

        // The shepherd predates the field: no path at all.
        let unknown = read(&mut seen, None, None);
        assert!(unknown.lines.is_empty());
        assert!(
            unknown
                .note
                .as_deref()
                .unwrap()
                .contains("did not report a log path"),
            "got {:?}",
            unknown.note
        );

        // Never ran in this $SHEP_HOME: the shepherd creates both files at
        // spawn, so a missing file means exactly this.
        let missing = read(&mut seen, Some(&dir.path().join("nope.log")), None);
        assert!(
            missing
                .note
                .as_deref()
                .unwrap()
                .contains("has not written a log"),
            "got {:?}",
            missing.note
        );

        // Present but unreadable: a directory where a file should be.
        let as_dir = dir.path().join("a-directory.log");
        std::fs::create_dir(&as_dir).unwrap();
        let unreadable = read(&mut seen, Some(&as_dir), None);
        assert!(
            unreadable
                .note
                .as_deref()
                .unwrap()
                .contains("could not read"),
            "got {:?}",
            unreadable.note
        );

        // And an EXISTING, EMPTY file is not an error at all — a quiet sheep
        // is not a broken one.
        let quiet = dir.path().join("quiet.log");
        std::fs::write(&quiet, "").unwrap();
        let silent = read(&mut seen, Some(&quiet), None);
        assert!(silent.lines.is_empty());
        assert!(
            silent
                .note
                .as_deref()
                .unwrap()
                .contains("has written nothing"),
            "got {:?}",
            silent.note
        );

        // A FIFTH reason, and the one that is easy to miss: a file with
        // content but no newline anywhere in the last window. `lines` is empty
        // and `read_bytes` is 64 KiB, so "has written nothing yet" would be
        // flatly false — and false in the direction an operator acts on.
        let unterminated = dir.path().join("one-long-line.log");
        std::fs::write(
            &unterminated,
            "q".repeat(usize::try_from(FEED_WINDOW_BYTES).unwrap() + 10),
        )
        .unwrap();
        let long = read(&mut seen, Some(&unterminated), None);
        assert!(long.lines.is_empty());
        assert!(long.read_bytes > 0, "it read plenty; it just found no line");
        assert!(
            long.note.as_deref().unwrap().contains("no complete line"),
            "got {:?}",
            long.note
        );
    }
}
