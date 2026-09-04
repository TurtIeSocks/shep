//! `barks.jsonl`: the size-capped ring of fired alerts (spec §10.4).
//!
//! One [`Bark`] per line, appended by two writers in two different
//! processes — the bark dog when a rule fires, and the shepherd itself
//! when an enabled dog exhausts its restart budget — and read by a third,
//! `shep barks`. [`append`] keeps the file under a byte cap by evicting
//! whole lines oldest-first, rewriting the survivors plus the new record
//! to a sibling temp file and `rename`ing it over the original — the same
//! atomic-replace shape `shep-daemon`'s `snapshot::write_atomic` uses —
//! rather than truncating in place, so a writer that dies mid-rewrite
//! never leaves the reader a fragment.
//!
//! [`read`] is the forgiving half: a line that will not parse — a
//! partially-written record from a writer that died mid-append, or a
//! record from a future shep — costs the reader that one record, not the
//! whole history. This file is read during an incident; refusing the
//! whole ring over one bad line would be the wrong failure mode.
//!
//! Two writer processes is also why [`append`] takes an advisory lock on
//! a sibling `<path>.lock` and holds it across the whole
//! read-evict-rewrite-rename sequence. Without it the two writers
//! interleave read-modify-write and the later `rename` silently discards
//! every record the other appended in between — reproduced, not
//! theorised: two processes appending 200 records each left 200 of the
//! expected 400 in the file. Nothing about the atomic-replace shape
//! prevents that; atomicity buys the reader a whole file, not the writer
//! a whole transaction.
//!
//! Lives in shep-core, not shep-daemon, because it has two writers that
//! are two different processes (the shepherd and the bark dog) and
//! neither is the other's crate — one shared cap implementation, or the
//! two writers evict differently, and that is exactly the kind of drift
//! nobody watches until an incident.

use core::fmt;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Cap the ring keeps itself under when nobody configured one.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// One fired alert, as it lands in `$SHEP_HOME/barks.jsonl`.
///
/// One JSON object per line, because the file is appended to by two
/// writers (the bark dog when a rule fires, and the shepherd when an
/// enabled dog exhausts its budget) and read by a third (`shep barks`).
/// A line-delimited format is the one shape where an interrupted write
/// costs the reader one record instead of the file.
///
/// `Debug` is derived, not redacted. Every field here is shep's own
/// prose or a config key — never a sink's target — so printing a `Bark`
/// is safe, and it must stay that way: a field that ever carried a
/// webhook URL or token would need its own redacted `Debug` (IR-41) the
/// day it lands, and this comment is the tripwire for that review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bark {
    /// Unix millis when the alert fired.
    pub at_ms: u64,
    /// The rule that fired, or `daemon` when the shepherd wrote this
    /// itself.
    pub rule: String,
    /// What it is about: a sheep's name, or a dog's.
    pub subject: String,
    /// The human-readable line. Plain English, no theme — this is read
    /// during an incident.
    pub message: String,
    /// Which sinks the alert was delivered to, and whether each took it.
    /// Empty when the shepherd wrote the record itself: it has no sinks
    /// and no webhook code, and says so by carrying none.
    pub sinks: Vec<SinkOutcome>,
}

/// What one sink made of one alert.
///
/// Names the sink by its `[dog.bark.sinks]` config key, never by its
/// webhook URL or bearer token — that is what keeps this type, and
/// [`Bark`] alongside it, safe to print with a derived `Debug`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkOutcome {
    /// The sink's name from `[dog.bark.sinks]`.
    pub sink: String,
    /// `None` when it was delivered; the failure otherwise.
    pub error: Option<String>,
}

/// Error type returned by [`append`] and [`read`].
///
/// Wraps `io::Error`/`serde_json::Error` directly rather than
/// stringifying them (same reasoning as `shep-daemon`'s
/// `SnapshotError`) so callers keep the underlying diagnostic via
/// [`core::error::Error::source`] — the cost is that this enum cannot
/// derive `Clone`/`PartialEq`/`Eq` (IR-19's documented exception for
/// variants wrapping `io::Error`).
///
/// `#[non_exhaustive]`: shep-core is a published library and this enum is
/// reachable from it, so a third failure shape — a ring whose on-disk format
/// this build does not recognise, say — must not break an out-of-tree
/// consumer's `match` (IR-20).
#[non_exhaustive]
#[derive(Debug)]
pub enum BarkError {
    /// The ring file could not be read, written, or replaced.
    Io(std::io::Error),
    /// A [`Bark`] could not be serialized to JSON.
    Encode(serde_json::Error),
}

impl fmt::Display for BarkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "bark ring I/O failed: {err}"),
            Self::Encode(err) => write!(f, "bark record failed to serialize: {err}"),
        }
    }
}

impl core::error::Error for BarkError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Encode(err) => Some(err),
        }
    }
}

impl From<std::io::Error> for BarkError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<serde_json::Error> for BarkError {
    fn from(source: serde_json::Error) -> Self {
        Self::Encode(source)
    }
}

/// Appends `bark` to `path`, evicting oldest-first to keep the file under
/// `max_bytes`.
///
/// Eviction is oldest-out by whole lines: the file is rewritten with a
/// prefix of its lines dropped, atomically, so a reader never sees a
/// truncated one. A single record larger than `max_bytes` is written
/// anyway and leaves the file over the cap — the alternative is silently
/// dropping the alert that was too interesting to fit.
///
/// Serialized against other appenders — in this process or any other — by
/// an advisory lock on a sibling `<path>.lock`, held across the whole
/// read-modify-rename. Two writers without it lose each other's records
/// outright; see this module's own doc. Concurrent [`read`]s are not
/// blocked and do not need to be: the ring is only ever replaced whole,
/// by `rename`.
///
/// # Errors
/// - [`BarkError::Io`] — the file could not be read, written, or
///   replaced, or the lock beside it could not be taken.
/// - [`BarkError::Encode`] — the record could not be serialized.
pub fn append(path: &Path, bark: &Bark, max_bytes: u64) -> Result<(), BarkError> {
    // Held until this function returns, so the read below and the rename
    // at the end are one transaction as far as any other writer is
    // concerned — see [`RingLock`] for why the lock is not on `path`.
    let _lock = RingLock::acquire(path)?;

    let mut lines = read_lines(path)?;
    let new_line = serde_json::to_string(bark)?;
    lines.push(new_line);

    // Oldest-out: drop the front line until the ring fits under the cap,
    // or only the record just appended is left — see this function's own
    // doc for why a lone oversized record is kept rather than dropped.
    loop {
        if lines.len() <= 1 || ring_bytes(&lines) <= max_bytes {
            break;
        }
        lines.remove(0);
    }

    write_ring(path, &lines)
}

/// Reads every bark in `path`, oldest first, skipping any line that will
/// not parse.
///
/// A line that will not parse is a partially-written record from a writer
/// that died mid-append, or a record from a future shep. Neither is a
/// reason to refuse the whole history during an incident, which is the one
/// time this file is read.
///
/// # Errors
/// - [`BarkError::Io`] — the file exists and could not be read. A missing
///   file is `Ok(Vec::new())`: no barks yet is not a fault.
pub fn read(path: &Path) -> Result<Vec<Bark>, BarkError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(BarkError::Io(err)),
    }
}

/// `path`'s existing lines, raw and unparsed, or an empty ring if the
/// file does not exist yet.
///
/// Deliberately does not parse: eviction operates on whole lines exactly
/// as they sit on disk, so a line a future shep wrote (or a fragment a
/// dead writer left) still counts toward the byte cap and still survives
/// an eviction it does not trigger — [`read`], not this, is where an
/// unparseable line is finally dropped.
fn read_lines(path: &Path) -> Result<Vec<String>, BarkError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(BarkError::Io(err)),
    }
}

/// Total on-disk size, in bytes, if `lines` were written one per line
/// (each line plus its trailing `\n`).
fn ring_bytes(lines: &[String]) -> u64 {
    lines.iter().map(|line| line.len() as u64 + 1).sum()
}

/// Rewrites `path` to hold exactly `lines`: the new content lands in a
/// uniquely-named sibling temp file, is `fsync`ed, then `rename`d over
/// `path` — the same shape `snapshot::write_atomic` uses, so an
/// interrupted write leaves the original file exactly as it was rather
/// than a fragment (this module's own doc: rewrite-and-replace, not
/// truncate-in-place).
///
/// The name is unique per call, not a fixed `<path>.tmp`. A shared temp
/// name is not merely untidy: two writers racing on it had one process's
/// `rename` consume the other's staging file, and the loser died with
/// `ENOENT` renaming a path that no longer existed. [`RingLock`] already
/// keeps two appenders apart, so this is the second lock on the same door
/// — deliberately, because it is the half that survives a caller who ever
/// reaches `write_ring` by another route.
fn write_ring(path: &Path, lines: &[String]) -> Result<(), BarkError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = crate::atomic_file::create_staging_file(parent, "barks", ".tmp")?;

    for line in lines {
        tmp.write_all(line.as_bytes())?;
        tmp.write_all(b"\n")?;
    }
    tmp.as_file().sync_all()?;

    // `persist` is `rename(2)`. On failure the `NamedTempFile` comes back
    // inside the error and its `Drop` removes the staging file, so a
    // failed replace does not leave one behind.
    tmp.persist(path).map_err(|err| BarkError::Io(err.error))?;

    // The `sync_all` above made the CONTENTS durable; this makes the rename
    // that published them durable. See `shep_core::atomic_file`.
    crate::atomic_file::sync_dir(parent)?;
    Ok(())
}

/// An exclusive advisory lock over one bark ring, held for as long as the
/// value lives and released when it drops (including on an early `?`, and
/// by the kernel if the process dies holding it).
///
/// The lock is on a sibling `<path>.lock`, never on the ring itself, and
/// that is the whole design decision: `append` finishes by `rename`ing a
/// new file over `path`, which replaces the inode. A lock taken on the
/// ring would be a lock on an inode that the very next successful append
/// unlinks — the next writer would open the *new* inode, find it
/// unlocked, and the two would be excluding nothing. The lock file is
/// never renamed, never rewritten, and never read; it exists only to be
/// an inode with a stable identity, and it is left on disk between
/// appends on purpose so both writers keep agreeing on which one it is.
struct RingLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a
    /// leading underscore because it is held, never read.
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// The lock file, opened with `share_mode(0)` so no other handle —
    /// same-process or not, read or write — can open it while this one is
    /// live. Released by this handle's `Drop`, the same role `_flock` plays
    /// on unix. Named with a leading underscore because it is held, never
    /// read.
    #[cfg(windows)]
    _handle: std::fs::File,
}

impl RingLock {
    /// Blocks until this process holds the ring's lock exclusively.
    ///
    /// # Errors
    /// The lock file could not be created beside `path`, or `flock` failed
    /// for a reason other than contention (contention blocks rather than
    /// failing).
    #[cfg(unix)]
    fn acquire(path: &Path) -> std::io::Result<Self> {
        use nix::fcntl::{Flock, FlockArg};
        use std::os::unix::fs::OpenOptionsExt as _;

        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(crate::atomic_file::OWNER_ONLY_FILE_MODE)
            .open(lock_path(path))?;

        // `LockExclusive` blocks; the non-blocking variant would need a
        // retry loop and a deadline, and an append that waits its turn is
        // exactly the behaviour wanted here.
        Flock::lock(file, FlockArg::LockExclusive)
            .map(|flock| Self { _flock: flock })
            .map_err(|(_file, errno)| std::io::Error::from(errno))
    }

    /// Blocks until this process holds the ring's lock exclusively.
    ///
    /// `share_mode(0)` gives the same exclusivity `flock(2)` does through a
    /// different door — opening the lock file with every share flag cleared
    /// means no other handle, another process's or this one's, read or
    /// write, can be opened on it while this handle lives. That is
    /// mandatory (enforced by the OS on every open) rather than merely
    /// advisory, so it is if anything a stronger guarantee than the unix
    /// arm's.
    ///
    /// What it does not give is a blocking wait: a contended open fails at
    /// once with `ERROR_SHARING_VIOLATION` rather than parking the thread
    /// the way `FlockArg::LockExclusive` does, so this polls on a short
    /// sleep until the open succeeds. That is the one real behavioural
    /// difference between the two arms, and it is why "contention blocks
    /// rather than failing" stays true here by a retry loop rather than by
    /// the kernel.
    ///
    /// Identical in shape to [`KvLock::acquire`](crate::kv)'s Windows arm,
    /// deliberately — the two guard the same kind of file in the same
    /// directory, and a reader who has understood one should not have to
    /// re-derive the other.
    ///
    /// # Errors
    /// The lock file could not be created beside `path`, or the open failed
    /// for a reason other than sharing contention (contention retries
    /// rather than failing).
    #[cfg(windows)]
    fn acquire(path: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt as _;

        /// Windows' `ERROR_SHARING_VIOLATION`: another handle already holds
        /// share access this open's `share_mode(0)` denies. Hardcoded
        /// rather than pulled from `windows-sys` — this crate has no
        /// Windows-only dependency today, and one well-known, stable error
        /// code does not earn it one.
        const ERROR_SHARING_VIOLATION: i32 = 32;

        /// How long a contended retry sleeps before trying again. Short
        /// enough that a lock held for one `append`'s duration (a read, a
        /// write, a rename) costs this loop only a few iterations, long
        /// enough not to spin the CPU while it waits.
        const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

        let lock_path = lock_path(path);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(0)
                .open(&lock_path)
            {
                Ok(handle) => return Ok(Self { _handle: handle }),
                Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                    std::thread::sleep(RETRY_INTERVAL);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// The lock file that guards `path`: its own name with `.lock` appended,
/// so it sits in `$SHEP_HOME` next to the ring and inherits that
/// directory's `0700`.
///
/// `cfg(any(unix, windows))` alongside its two callers —
/// [`RingLock::acquire`] names a real lock file on both platforms now, unix
/// through `flock(2)` and windows through an exclusive `share_mode(0)` open.
#[cfg(any(unix, windows))]
fn lock_path(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".lock");
    path.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative fired alert. `at_ms` is a caller-chosen tag, not a
    /// real timestamp — tests use it to tell records apart, not to
    /// exercise time handling.
    fn bark_for(subject: &str, at_ms: u64) -> Bark {
        Bark {
            at_ms,
            rule: "watchdog".to_string(),
            subject: subject.to_string(),
            message: "restart budget exhausted".to_string(),
            sinks: vec![SinkOutcome {
                sink: "discord".to_string(),
                error: None,
            }],
        }
    }

    /// The serialized length (plus its trailing newline) of one
    /// `bark_for`-shaped line, measured rather than hard-coded — a
    /// constant that happened to equal the implementation's own byte
    /// count would pass for any cap, which is the assertion-against-the-
    /// same-constant shape this project has shipped before.
    fn one_bark_len() -> u64 {
        let line = serde_json::to_string(&bark_for("second", 1)).unwrap();
        line.len() as u64 + 1
    }

    /// The eviction, which is the whole reason this is a ring and not an
    /// append. A cap the test never reaches leaves an append-only file with
    /// extra code, so the cap here is deliberately small enough that the
    /// third write MUST evict — and the assertion names the surviving
    /// subject rather than counting lines, so a ring that evicted the
    /// NEWEST record would fail here rather than pass on the count.
    #[test]
    fn the_ring_drops_the_oldest_bark_to_stay_under_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");
        let cap = 2 * one_bark_len();

        for (i, subject) in ["first", "second", "third"].iter().enumerate() {
            append(&path, &bark_for(subject, i as u64), cap).unwrap();
        }

        let barks = read(&path).unwrap();
        let subjects: Vec<&str> = barks.iter().map(|b| b.subject.as_str()).collect();
        assert_eq!(subjects, ["second", "third"], "oldest out, newest kept");
        assert!(
            std::fs::metadata(&path).unwrap().len() <= cap,
            "the cap is a cap"
        );
    }

    /// fails if a record larger than the whole cap is silently dropped. An
    /// alert too interesting to fit is exactly the one an operator needs;
    /// leaving the file over its cap for one record is the cheaper wrong.
    #[test]
    fn a_bark_bigger_than_the_cap_is_written_anyway() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");
        let huge = Bark {
            message: "x".repeat(4096),
            ..bark_for("web", 0)
        };
        append(&path, &huge, 64).unwrap();
        assert_eq!(read(&path).unwrap().len(), 1);
    }

    /// fails if one unparseable line refuses the whole history. That line
    /// is a writer that died mid-append or a record from a future shep, and
    /// this file is read during an incident — the surviving records are
    /// what the reader came for.
    #[test]
    fn a_line_that_will_not_parse_costs_one_record_and_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");
        append(&path, &bark_for("web", 1), DEFAULT_MAX_BYTES).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"at_ms\": 2, \"rul\n")
            .unwrap();
        append(&path, &bark_for("api", 3), DEFAULT_MAX_BYTES).unwrap();

        let barks = read(&path).unwrap();
        assert_eq!(
            barks.iter().map(|b| b.subject.as_str()).collect::<Vec<_>>(),
            ["web", "api"]
        );
    }

    /// fails if a missing file is an error. No barks yet is the state every
    /// machine starts in.
    #[test]
    fn no_file_yet_is_no_barks_rather_than_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(&dir.path().join("nothing.jsonl")).unwrap(), vec![]);
    }

    /// Env var naming the ring file the re-executed child should append
    /// to. Its presence is also what tells the child it is a child.
    #[cfg(any(unix, windows))]
    const CHILD_PATH_VAR: &str = "SHEP_BARK_RACE_PATH";
    /// Env var carrying the child's tag, which it stamps into every
    /// record's `subject` so the parent can tell the two writers apart.
    #[cfg(any(unix, windows))]
    const CHILD_TAG_VAR: &str = "SHEP_BARK_RACE_TAG";
    /// How many records each of the two writers appends. Large enough that
    /// the two read-modify-rename sequences overlap many times over; the
    /// reviewed reproduction of the lost-update bug used this count and
    /// lost half the records.
    #[cfg(any(unix, windows))]
    const RECORDS_PER_WRITER: u64 = 200;

    /// Not a test — the child half of
    /// [`two_writer_processes_do_not_lose_each_other_s_barks`], which
    /// re-executes this binary with `--ignored --exact` to reach it. It is
    /// `#[ignore]`d so a normal run never picks it up, and it asserts
    /// nothing: its job is to hammer [`append`] from a second OS process,
    /// and the parent does the judging.
    #[cfg(any(unix, windows))]
    #[test]
    #[ignore = "child process of two_writer_processes_do_not_lose_each_other_s_barks"]
    fn bark_race_child() {
        let Ok(path) = std::env::var(CHILD_PATH_VAR) else {
            panic!("{CHILD_PATH_VAR} unset — this test is only run as a child process");
        };
        let tag = std::env::var(CHILD_TAG_VAR).expect("child needs a tag");
        let path = std::path::PathBuf::from(path);

        for i in 0..RECORDS_PER_WRITER {
            append(&path, &bark_for(&tag, i), DEFAULT_MAX_BYTES).expect("child append");
        }
    }

    /// fails if two writers in two *processes* lose each other's records —
    /// the whole reason this module lives in shep-core rather than in
    /// shep-daemon. Two OS processes, not two threads: any in-process
    /// mutex would serialise threads and prove nothing about the bug,
    /// which is a read-modify-write across a `rename` with no lock between
    /// address spaces.
    ///
    /// Covers Windows too: this is what keeps the Windows lock honest —
    /// revert `acquire`'s Windows arm to `Ok(Self {})` and this reddens
    /// rather than passing quietly.
    ///
    /// Without the lock this exposes the race; nothing synchronises the
    /// two children into overlap, so a lucky serial schedule can still pass
    /// one run while failing under load.
    #[cfg(any(unix, windows))]
    #[test]
    fn two_writer_processes_do_not_lose_each_other_s_barks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");
        let exe = std::env::current_exe().expect("test binary path");

        let children: Vec<_> = ["alpha", "beta"]
            .iter()
            .map(|tag| {
                std::process::Command::new(&exe)
                    .args(["--exact", "--ignored", "barks::tests::bark_race_child"])
                    .env(CHILD_PATH_VAR, &path)
                    .env(CHILD_TAG_VAR, tag)
                    // Piped, not inherited: a passing run should not
                    // interleave two child harnesses' output into this
                    // one's, and a failing child's harness output is
                    // exactly what the assertion below needs to show.
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn writer")
            })
            .collect();

        for child in children {
            let out = child.wait_with_output().expect("wait for writer");
            assert!(
                out.status.success(),
                "a writer process failed: {}\n{}",
                out.status,
                String::from_utf8_lossy(&out.stdout)
            );
        }

        let barks = read(&path).unwrap();
        for tag in ["alpha", "beta"] {
            let mut seen: Vec<u64> = barks
                .iter()
                .filter(|b| b.subject == tag)
                .map(|b| b.at_ms)
                .collect();
            seen.sort_unstable();
            let expected: Vec<u64> = (0..RECORDS_PER_WRITER).collect();
            assert_eq!(
                seen, expected,
                "{tag}'s records did not all survive the other writer"
            );
        }
        assert_eq!(
            barks.len() as u64,
            2 * RECORDS_PER_WRITER,
            "the ring holds records nobody wrote"
        );
    }

    /// fails if the ring lands wider than owner-only. A `Bark` carries no
    /// credential today: it holds a rule name, a subject and a message,
    /// and a [`SinkOutcome`] names a sink by its config key rather than by
    /// the webhook URL behind it. The mode stays tight anyway, because
    /// this file is still a record of what the shepherd told an outside
    /// service, and because a future field that did carry a URL should
    /// arrive into a file that is already narrow. See
    /// [`crate::atomic_file::OWNER_ONLY_FILE_MODE`] for the shared value.
    #[cfg(unix)]
    #[test]
    fn append_creates_the_ring_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("barks.jsonl");

        append(&path, &bark_for("web", 0), DEFAULT_MAX_BYTES).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "barks.jsonl is not the credential file, but stays narrow anyway"
        );
    }
}
