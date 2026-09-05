//! The bounded log reader behind the bleats feed, and the gap it admits to.
//!
//! A window from the end of each log file, a line cap on top of that window,
//! and a count of what the two left out: [`Tail::missed_lines`] for what the
//! reader saw and discarded, off by at most one at a boundary, plus
//! [`Tail::missed_bytes`] for what it never read, exact. One refresh costs
//! at most one seek and one [`FEED_WINDOW_BYTES`] read per file whatever
//! the sheep writes, so the reader is bounded by itself and not by the writer.
//! [`read`] is pure over the filesystem, which is what lets its tests drive
//! it with a [`std::collections::BTreeMap`] and a `tempdir`.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use crate::output::human_bytes;

/// The most of one log file this pane will read to find its lines.
///
/// 64 KiB, a quarter of `commands::bleats`' own `TAIL_WINDOW_BYTES`: that path
/// shows fifty lines once, this one shows five lines every two seconds for the
/// life of the dashboard. Two files at 64 KiB every two seconds is 64 KiB/s of
/// reads, whatever the flock writes.
pub const FEED_WINDOW_BYTES: u64 = 64 * 1024;

/// The most lines one file contributes, once the window is split.
///
/// Both bounds are needed: 64 KiB of newlines is 65536 lines, and one
/// arbitrarily long line with no newline defeats a line count.
pub const FEED_TAIL_LINES: usize = 40;

/// Which of a sheep's two output streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// stdout.
    Out,
    /// stderr. Not an error: most runtimes log there by default, so the feed
    /// renders this tag muted rather than in `--bark`, which means errored,
    /// refused and destructive.
    Err,
}

/// One line, with the stream it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailLine {
    /// Which file it was in.
    pub stream: Stream,
    /// The line, without its terminator. Decoded with
    /// [`String::from_utf8_lossy`]: a log file is whatever the child wrote and
    /// is under no obligation to be UTF-8.
    pub text: String,
}

/// What one refresh of the feed found.
///
/// Two miss counters, because lines go missing in two places this reader can
/// tell apart: below the byte window, exact in bytes and unknowable in lines,
/// and above [`FEED_TAIL_LINES`] inside it, exact in lines. Lines above the
/// rows the pane has are the pane's own to count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    /// The newest lines, oldest first: `out`'s tail, then `err`'s.
    ///
    /// Not merged. This reader strips the per-line timestamp, so nothing here
    /// carries a key to merge on, and file order is not time order when a
    /// sheep writes to both at once. The pane renders the last rows of this
    /// list, so a crash on stderr survives a chatty stdout.
    pub lines: Vec<TailLine>,
    /// Lines this read saw and discarded, summed over both files.
    ///
    /// Those above [`FEED_TAIL_LINES`], plus one for the partial line a window
    /// boundary cut. The partial line counts as one even when the boundary
    /// fell between two lines: telling those apart takes a byte the window
    /// excluded, and over-counting by one is the safe direction.
    pub missed_lines: usize,
    /// Bytes appended since the previous read that fell below the window and
    /// were therefore never read at all.
    ///
    /// Exact as a byte count; the number of lines in them is unknowable
    /// without reading them, which is what the window exists to avoid. Zero on
    /// the first read of a file, since a file's history is not a gap between
    /// two reads, and zero when the file shrank, which is what a rotation or a
    /// `shep flush` looks like from here. Either way the lines the window and
    /// the cap dropped are still in [`Self::missed_lines`].
    pub missed_bytes: u64,
    /// Bytes this refresh actually pulled off disk, both files together.
    ///
    /// Never above `2 * FEED_WINDOW_BYTES`, including while the sheep is
    /// writing during the read. Exposed so the tests can assert that live.
    pub read_bytes: u64,
    /// Why there is nothing to show, when there is nothing to show. Names the
    /// cause rather than restating the fact an operator can already see.
    pub note: Option<String>,
}

/// One refresh: both files, tagged, with the gap admitted to.
///
/// `seen` is the caller's memory of each file's length at the previous read;
/// [`super::source::LocalReader`] owns it, which keeps this function pure over
/// the filesystem.
///
/// `std::fs`, not `tokio::fs`: shep-cli's tokio does not carry the `fs`
/// feature, and this is a bounded read on a task that is otherwise asleep.
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
            // this sheep has never run in this `$SHEP_HOME`.
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
            // Read plenty and found no line terminator: "has written nothing
            // yet" would be wrong here.
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
/// A struct rather than a tuple: three of the four returns are counters that
/// differ only in what they count, so positional returns would transpose
/// silently.
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
/// The file could not be opened, `stat`ed, seeked or read. [`read`] treats
/// [`std::io::ErrorKind::NotFound`] differently from every other kind.
fn read_window(seen: &mut BTreeMap<PathBuf, u64>, path: &Path) -> std::io::Result<Window> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(FEED_WINDOW_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }

    // `.take(..)` is what bounds this read. `read_to_end` alone reads to the
    // file's current end, not to the `len` just `stat`ed, so a sheep appending
    // while the read is in flight makes both it and this `Vec` grow without
    // limit on the UI task.
    let mut window = Vec::with_capacity(usize::try_from(len.min(FEED_WINDOW_BYTES)).unwrap_or(0));
    (&mut file)
        .take(FEED_WINDOW_BYTES)
        .read_to_end(&mut window)?;
    let read_bytes = u64::try_from(window.len()).unwrap_or(u64::MAX);

    // A window boundary can land mid-line, so the bytes up to and including
    // the first newline are discarded and counted. Counted as one even when
    // the boundary fell between two lines: telling those apart needs the byte
    // at `start - 1`, which this function did not read.
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
    // it: a line has to read the same in this pane as in the live feed.
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

    // Bytes never read: those appended since the previous read that fell below
    // the window. Not `len - previous - covered`, which also counts the bytes
    // of the lines `dropped` counts in lines. `saturating_sub`, so a file that
    // shrank reports zero rather than sixteen exabytes.
    let previous = seen.insert(path.to_path_buf(), len);
    let never_read = match previous {
        // The first read of a file shows the tail of its history, which is not
        // a gap between reads. The lines the window and the cap dropped are in
        // `dropped` either way.
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
    use std::io::Write as _;

    use super::*;

    /// Asserted on `read_bytes` rather than on the size of what came back:
    /// forty short lines are under 64 KiB even for an implementation that read
    /// the whole four megabytes and threw them away.
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
        assert!(
            tail.missed_lines > 400,
            "a 64 KiB window of 121-byte lines holds ~540 of them and keeps 40; \
             counted only {}",
            tail.missed_lines
        );
    }

    /// Sixty lines fit inside one window, so the byte accounting never fires
    /// and only the cap drops anything.
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

    /// A static fixture cannot exercise this: a background writer appends for
    /// the whole read, for a fixed number of iterations.
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
        // The writer actually ran: a file that never grew would pass for the
        // wrong reason.
        assert!(
            std::fs::metadata(&path).unwrap().len() > 32 * 1024 * 1024,
            "the writer did not get far enough for this test to mean anything"
        );
    }

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
        // `missed_bytes` is what was never read. The last 64 KiB was read, so
        // it is not in this number, which is the upper bound below.
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

        // A third read with nothing appended reports no gap between reads, but
        // still reports the line the window cuts.
        let third = read(&mut seen, Some(&path), None);
        assert_eq!(third.missed_bytes, 0);
        assert_eq!(third.missed_lines, 1);
    }

    /// A rotation or a `shep flush` makes the file smaller between two reads,
    /// and a subtraction that wrapped would claim sixteen exabytes.
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

    /// Not cosmetic: half a log line shown as complete is a lie an
    /// operator acts on.
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

    /// The pane renders the last rows of this list, so `err` coming last is
    /// what makes a crash survive a chatty stdout.
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

        // An existing, empty file is not an error: a quiet sheep is not a
        // broken one.
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

        // A file with content but no newline in the window: `lines` is empty
        // here too, so it needs its own note rather than "has written
        // nothing yet".
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
