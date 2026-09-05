//! `barks.jsonl`: the size-capped ring of fired alerts.
//!
//! One [`Bark`] per line, appended by the bark dog on a rule fire and by
//! the shepherd on a dog's exhausted restart budget, and read by `shep
//! barks`. [`append`] evicts oldest-first under a byte cap by rewriting
//! survivors to a temp file and `rename`ing it over the original, so a
//! writer that dies mid-rewrite never leaves a fragment. [`read`] skips
//! any unparseable line, since this file is read during an incident.
//!
//! The two writers are separate OS processes, so a shared advisory lock
//! on a sibling `<path>.lock` serializes them; shep-core, not
//! shep-daemon, is where that lock belongs.

use core::fmt;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Cap the ring keeps itself under when nobody configured one.
pub const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// One fired alert, as it lands in `$SHEP_HOME/barks.jsonl`.
///
/// One JSON object per line: an interrupted write then costs the reader
/// one record, not the whole file.
///
/// `Debug` is derived, not redacted. Every field here is shep's own prose
/// or a config key, never a sink's target; a field that ever carried a
/// webhook URL or token would need its own redacted `Debug`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bark {
    /// Unix millis when the alert fired.
    pub at_ms: u64,
    /// The rule that fired, or `daemon` when the shepherd wrote this
    /// itself.
    pub rule: String,
    /// What it is about: a sheep's name, or a dog's.
    pub subject: String,
    /// The human-readable line. Plain English, no theme: this is read
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
/// webhook URL or bearer token, so [`Bark`] stays safe to print with a
/// derived `Debug`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkOutcome {
    /// The sink's name from `[dog.bark.sinks]`.
    pub sink: String,
    /// `None` when it was delivered; the failure otherwise.
    pub error: Option<String>,
}

/// Error type returned by [`append`] and [`read`].
///
/// Wraps `io::Error`/`serde_json::Error` directly so callers keep the
/// underlying diagnostic via [`core::error::Error::source`]; this enum
/// cannot derive `Clone`/`PartialEq`/`Eq` as a result.
///
/// `#[non_exhaustive]`: shep-core is a published library, so a future
/// failure variant must not break an out-of-tree consumer's `match`.
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
/// Eviction drops a prefix of whole lines, atomically; an oversized
/// record is written anyway, since dropping it would silently lose the
/// one alert too big to fit. Serialized against other appenders by an
/// advisory lock on a sibling `<path>.lock`. Concurrent [`read`]s are not
/// blocked: the ring is only ever replaced whole.
///
/// # Errors
/// - [`BarkError::Io`]: the file or its lock could not be read, written, or replaced.
/// - [`BarkError::Encode`]: the record could not be serialized.
pub fn append(path: &Path, bark: &Bark, max_bytes: u64) -> Result<(), BarkError> {
    // Held until this returns, so the read and the final rename are one
    // transaction as far as any other writer is concerned.
    let _lock = RingLock::acquire(path)?;

    let mut lines = read_lines(path)?;
    let new_line = serde_json::to_string(bark)?;
    lines.push(new_line);

    // Oldest-out: drop the front line until the ring fits under the cap,
    // or only the record just appended is left.
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
/// that died mid-append, or a record from a future shep. Neither refuses
/// the whole history during an incident, which is the one time this file
/// is read.
///
/// # Errors
/// - [`BarkError::Io`]: the file exists and could not be read. A missing
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
/// Does not parse: eviction operates on whole lines exactly as they sit
/// on disk, so a line a future shep wrote, or a fragment a dead writer
/// left, still counts toward the byte cap and survives an eviction it
/// does not trigger. [`read`] is where an unparseable line is dropped.
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

/// Rewrites `path` to hold exactly `lines`: the content lands in a
/// uniquely-named sibling temp file, is `fsync`ed, then `rename`d over
/// `path`, so an interrupted write leaves the original untouched.
///
/// The name is unique per call, not a fixed `<path>.tmp`: two writers
/// racing on a shared name can have one `rename` consume the other's
/// staging file. [`RingLock`] already keeps two appenders apart; this is
/// the second lock for a caller that reaches `write_ring` another way.
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

    // `sync_all` made the contents durable; this makes the rename that
    // published them durable.
    crate::atomic_file::sync_dir(parent)?;
    Ok(())
}

/// An exclusive advisory lock over one bark ring, held for as long as the
/// value lives and released when it drops, including on an early `?` or
/// if the process dies holding it.
///
/// The lock is on a sibling `<path>.lock`, never on the ring itself:
/// `append` replaces the ring's inode with every `rename`, so a lock on
/// the ring itself would guard an inode the next append immediately
/// unlinks, excluding nothing. The lock file is never renamed, rewritten,
/// or read, and stays on disk between appends so both writers keep
/// agreeing on which file it is.
struct RingLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a
    /// leading underscore because it is held, never read.
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// The lock file, opened with `share_mode(0)` so no other handle,
    /// same-process or not, can open it while this one is live. Released
    /// by this handle's `Drop`, named with a leading underscore because
    /// it is held, never read.
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
    /// `share_mode(0)` denies every other handle, same process or not,
    /// while this one is live: an OS-enforced exclusivity rather than
    /// `flock`'s advisory one. It gives no blocking wait, though: a
    /// contended open fails at once with `ERROR_SHARING_VIOLATION`, so
    /// this polls on a short sleep until it succeeds.
    ///
    /// # Errors
    /// The lock file could not be created beside `path`, or the open failed
    /// for a reason other than sharing contention.
    #[cfg(windows)]
    fn acquire(path: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt as _;

        /// Windows' `ERROR_SHARING_VIOLATION`: another handle already holds
        /// share access this open's `share_mode(0)` denies. Hardcoded
        /// since this crate has no other Windows-only dependency.
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
/// `cfg(any(unix, windows))`: [`RingLock::acquire`] locks it for real on
/// both platforms, through `flock(2)` on unix and an exclusive
/// `share_mode(0)` open on windows.
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
    /// real timestamp: tests use it to tell records apart.
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

    /// The serialized length, plus its trailing newline, of one
    /// `bark_for`-shaped line, computed here so it cannot happen to equal
    /// the implementation's own byte count.
    fn one_bark_len() -> u64 {
        let line = serde_json::to_string(&bark_for("second", 1)).unwrap();
        line.len() as u64 + 1
    }

    /// Cap set to force eviction on the third write.
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
    /// How many records each of the two writers appends. Large enough
    /// that the two read-modify-rename sequences overlap many times over.
    #[cfg(any(unix, windows))]
    const RECORDS_PER_WRITER: u64 = 200;

    /// Not a test: the child half of
    /// [`two_writer_processes_do_not_lose_each_other_s_barks`], re-executed
    /// as a separate OS process via `--ignored --exact`. Asserts nothing;
    /// its job is to hammer [`append`] while the parent judges the result.
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

    /// Two OS processes, not threads: an in-process mutex would prove
    /// nothing about a race that crosses address spaces via `rename`.
    /// Covers Windows too: reverting `acquire`'s Windows arm to
    /// `Ok(Self {})` reddens this rather than passing quietly. Without
    /// the lock this can still pass on a lucky serial schedule.
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

    /// No field here is a credential today; the mode stays narrow so a
    /// future one that is arrives already protected.
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
