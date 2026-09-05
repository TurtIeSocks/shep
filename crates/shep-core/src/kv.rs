//! `kv.json`: the shepherd's key/value store (spec §5).
//!
//! A flat map of short strings under `$SHEP_HOME`, for ad-hoc operator notes
//! and dog runtime tweaks. Not the primary config path: a Flockfile
//! configures a sheep, `shep.toml` the shepherd and its dogs. A file rather
//! than an RPC, so `shep set`/`get`/`unset` work with no shepherd running.
//! Every mutation is a read-modify-rename under an exclusive lock on a
//! sibling `kv.json.lock`, staged through a temp file: the same shape
//! `barks::append` uses, so do not reimplement it here.
//!
//! Keys match `[A-Za-z0-9._-]`, 1 to [`MAX_KEY_BYTES`], not starting with
//! `.`; a dot is part of a key's name, not a path.

use core::fmt;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
// `PathBuf` backs `lock_path` below, gated the same way for both platform
// arms of `KvLock`.
#[cfg(any(unix, windows))]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The on-disk format's version.
///
/// A store carrying a higher version is refused rather than read or
/// replaced ([`KvError::FutureVersion`]): there is no undo for a downgrade
/// that overwrites an operator's store.
pub const KV_VERSION: u32 = 1;

/// Longest key this store accepts, in bytes.
pub const MAX_KEY_BYTES: usize = 128;

/// Longest value this store accepts, in bytes.
///
/// The store is read whole on every access; a cap keeps it from becoming an
/// unbounded blob store.
pub const MAX_VALUE_BYTES: usize = 4096;

/// The file's shape: a version and a flat map.
///
/// `BTreeMap`, not `HashMap`, so the file writes in key order: two writes of
/// the same content produce byte-identical files.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KvFile {
    version: u32,
    entries: BTreeMap<String, String>,
}

/// Error type returned by this module.
///
/// `#[non_exhaustive]`: shep-core is published, so a new failure variant
/// must not break an out-of-tree `match`.
///
/// Wraps `io::Error`/`serde_json::Error` directly rather than stringifying
/// them, matching [`BarkError`](crate::barks::BarkError), so callers keep
/// the underlying diagnostic through [`core::error::Error::source`]; this
/// type does not derive `Clone`/`PartialEq`/`Eq` as a result.
#[non_exhaustive]
#[derive(Debug)]
pub enum KvError {
    /// The store could not be read, written, or replaced.
    Io(std::io::Error),
    /// The store's JSON could not be parsed.
    ///
    /// Refused rather than repaired: a partial read would silently drop keys
    /// still on disk.
    Decode(serde_json::Error),
    /// A key outside the grammar; carries it verbatim so the message can quote
    /// what was typed.
    InvalidKey(String),
    /// A value over [`MAX_VALUE_BYTES`].
    ValueTooLong {
        /// The key it was being stored under.
        key: String,
        /// Its length in bytes.
        len: usize,
    },
    /// The store on disk is a version this build does not understand; carries
    /// that version. Nothing was written.
    FutureVersion(u32),
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "kv store I/O failed: {err}"),
            Self::Decode(err) => write!(f, "kv store failed to parse: {err}"),
            Self::InvalidKey(key) => write!(f, "`{key}` is not a valid kv key"),
            Self::ValueTooLong { key, len } => write!(
                f,
                "value for `{key}` is {len} bytes, over the {MAX_VALUE_BYTES}-byte limit"
            ),
            Self::FutureVersion(version) => {
                write!(
                    f,
                    "kv store is version {version}, newer than this build understands"
                )
            }
        }
    }
}

impl core::error::Error for KvError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Decode(err) => Some(err),
            Self::InvalidKey(_) | Self::ValueTooLong { .. } | Self::FutureVersion(_) => None,
        }
    }
}

impl From<std::io::Error> for KvError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<serde_json::Error> for KvError {
    fn from(source: serde_json::Error) -> Self {
        Self::Decode(source)
    }
}

/// Checks one key against the grammar.
///
/// # Errors
/// [`KvError::InvalidKey`]: empty, over [`MAX_KEY_BYTES`], starting with `.`,
/// or containing anything outside `[A-Za-z0-9._-]`.
fn check_key(key: &str) -> Result<(), KvError> {
    let ok = !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && !key.starts_with('.')
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(KvError::InvalidKey(key.to_string()))
    }
}

/// The lock file that guards `path`: its own name with `.lock` appended, so
/// it sits in `$SHEP_HOME` next to the store and inherits that directory's
/// `0700`.
///
/// `cfg(any(unix, windows))` alongside its two callers: [`KvLock::acquire`]
/// names a real lock file on both platforms now, unix through `flock(2)` and
/// windows through an exclusive `share_mode(0)` open.
#[cfg(any(unix, windows))]
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".lock");
    path.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

/// An exclusive advisory lock over one kv store, released when it drops,
/// including by the kernel if the process dies holding it.
///
/// On a sibling `kv.json.lock`, never on the store itself: `rename`
/// replaces the store's inode, which would orphan a lock held on it.
struct KvLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a leading
    /// underscore because it is held, never read.
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// The lock file, opened with `share_mode(0)` so no other handle can
    /// open it while this one is live; released by `Drop`, the same role
    /// `_flock` plays on unix. Named with a leading underscore because it
    /// is held, never read.
    #[cfg(windows)]
    _handle: std::fs::File,
}

impl KvLock {
    /// Blocks until this process holds the store's lock exclusively.
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

        Flock::lock(file, FlockArg::LockExclusive)
            .map(|flock| Self { _flock: flock })
            .map_err(|(_file, errno)| std::io::Error::from(errno))
    }

    /// Blocks until this process holds the store's lock exclusively.
    ///
    /// `share_mode(0)` denies every other open, in this process or another,
    /// giving the same exclusivity as unix `flock`. A contended open fails
    /// immediately with `ERROR_SHARING_VIOLATION` rather than blocking, so
    /// this polls on a short sleep until it succeeds.
    ///
    /// # Errors
    /// The lock file could not be created beside `path`, or the open failed
    /// for a reason other than sharing contention (contention retries rather
    /// than failing).
    #[cfg(windows)]
    fn acquire(path: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt as _;

        /// Windows' `ERROR_SHARING_VIOLATION`: another handle already holds
        /// share access this open's `share_mode(0)` denies. Hardcoded rather
        /// than pulled from `windows-sys`, since this crate has no other
        /// Windows-only dependency.
        const ERROR_SHARING_VIOLATION: i32 = 32;

        /// How long a contended retry sleeps before trying again. Short
        /// enough that a lock held for a normal `set`/`get`'s duration (a
        /// handful of small file operations) costs this loop only a few
        /// iterations, long enough not to spin the CPU while it waits.
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

/// Reads `path` under the lock the caller already holds.
///
/// A missing file reads as an empty, current-version store: `shep get`
/// against a fresh `$SHEP_HOME` should not fail with `ENOENT`. Any other
/// `io::Error` propagates.
fn read_file(path: &Path) -> Result<KvFile, KvError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(KvFile::default()),
        Err(err) => return Err(KvError::Io(err)),
    };
    let file: KvFile = serde_json::from_str(&raw)?;
    if file.version > KV_VERSION {
        return Err(KvError::FutureVersion(file.version));
    }
    Ok(file)
}

/// Rewrites `path` to hold exactly `file`, atomically: staged through a
/// temp file, then renamed over the original.
fn write_file(path: &Path, file: &KvFile) -> Result<(), KvError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = crate::atomic_file::create_staging_file(parent, "kv", ".tmp")?;

    let json = serde_json::to_string_pretty(file)?;
    tmp.write_all(json.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;

    // `persist` is `rename(2)`. On failure the `NamedTempFile` comes back
    // inside the error and its `Drop` removes the staging file, so a failed
    // replace does not leave one behind.
    tmp.persist(path).map_err(|err| KvError::Io(err.error))?;

    // `sync_all` above made the contents durable; this makes the rename
    // that published them durable too.
    crate::atomic_file::sync_dir(parent)?;
    Ok(())
}

/// Every key/value pair in the store, in key order.
///
/// # Errors
///
/// - [`KvError::Io`]: the store could not be opened or read. A store that is
///   simply absent is not an error: it reads as empty.
/// - [`KvError::Decode`]: the file is not the JSON this module writes.
/// - [`KvError::FutureVersion`]: the file's `version` is newer than
///   [`KV_VERSION`]. Nothing is read and nothing is written.
pub fn all(path: &Path) -> Result<BTreeMap<String, String>, KvError> {
    // Taking the lock here too costs one extra `open`, but it orders this
    // read against `set`/`unset`'s read-modify-rename instead of racing it.
    let _lock = KvLock::acquire(path)?;
    Ok(read_file(path)?.entries)
}

/// One key's value, or `None` if it is not in the store.
///
/// # Errors
///
/// [`KvError::InvalidKey`] for a key outside the grammar (refused before the
/// file is opened, so a malformed key never creates one), plus `Io`, `Decode`
/// and `FutureVersion` exactly as [`all`] returns them.
pub fn get(path: &Path, key: &str) -> Result<Option<String>, KvError> {
    check_key(key)?;
    Ok(all(path)?.remove(key))
}

/// Stores `value` under `key`, replacing any previous value.
///
/// # Errors
///
/// - [`KvError::InvalidKey`]: the key is outside the grammar.
/// - [`KvError::ValueTooLong`]: the value exceeds [`MAX_VALUE_BYTES`].
/// - [`KvError::FutureVersion`]: the store on disk is newer than this
///   build understands. Nothing is written.
/// - [`KvError::Decode`]: the existing file could not be parsed.
/// - [`KvError::Io`]: the lock, the temp file, the `fsync` or the
///   `rename` failed.
pub fn set(path: &Path, key: &str, value: &str) -> Result<(), KvError> {
    check_key(key)?;
    if value.len() > MAX_VALUE_BYTES {
        return Err(KvError::ValueTooLong {
            key: key.to_string(),
            len: value.len(),
        });
    }

    let _lock = KvLock::acquire(path)?;
    let mut file = read_file(path)?;
    file.version = KV_VERSION;
    file.entries.insert(key.to_string(), value.to_string());
    write_file(path, &file)
}

/// Removes `key`, returning whether it was there.
///
/// # Errors
///
/// The same set [`set`] returns, minus [`KvError::ValueTooLong`]: `InvalidKey`,
/// `FutureVersion`, `Decode`, `Io`.
pub fn unset(path: &Path, key: &str) -> Result<bool, KvError> {
    check_key(key)?;

    let _lock = KvLock::acquire(path)?;
    let mut file = read_file(path)?;
    let was_present = file.entries.remove(key).is_some();
    if was_present {
        file.version = KV_VERSION;
        write_file(path, &file)?;
    }
    Ok(was_present)
}

/// Empties the store, returning how many keys were removed.
///
/// # Errors
///
/// [`KvError::FutureVersion`], [`KvError::Decode`] and [`KvError::Io`]. A
/// store that does not exist clears to `0` rather than failing: `shep unset
/// --all` on a fresh machine is a success that removed nothing.
pub fn clear(path: &Path) -> Result<u32, KvError> {
    let _lock = KvLock::acquire(path)?;
    let file = read_file(path)?;
    let count = u32::try_from(file.entries.len()).unwrap_or(u32::MAX);
    if count > 0 {
        write_file(
            path,
            &KvFile {
                version: KV_VERSION,
                entries: BTreeMap::new(),
            },
        )?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_survives_a_write_and_a_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "bark.cooldown", "30s").unwrap();
        assert_eq!(
            get(&path, "bark.cooldown").unwrap(),
            Some("30s".to_string())
        );
    }

    #[test]
    fn a_store_that_does_not_exist_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        assert!(all(&path).unwrap().is_empty());
        assert_eq!(get(&path, "anything").unwrap(), None);
    }

    #[test]
    fn unset_reports_whether_the_key_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "a", "1").unwrap();
        assert!(unset(&path, "a").unwrap());
        assert!(!unset(&path, "a").unwrap());
    }

    #[test]
    fn clear_empties_the_store_and_counts_what_it_took() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "a", "1").unwrap();
        set(&path, "b", "2").unwrap();
        assert_eq!(clear(&path).unwrap(), 2);
        assert!(all(&path).unwrap().is_empty());
        assert_eq!(clear(&path).unwrap(), 0);
    }

    /// A key goes onto a shell command line (`shep get $k`) and into a JSON
    /// object, so whitespace, control characters and an empty name are
    /// refused.
    #[test]
    fn the_key_grammar_refuses_what_it_says_it_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        for bad in [
            "", " ", "a b", "a\nb", "a/b", "a:b", ".hidden", "a\"b", "$HOME",
        ] {
            assert!(
                matches!(set(&path, bad, "1"), Err(KvError::InvalidKey(_))),
                "`{bad}` was accepted as a key"
            );
        }
        for good in ["a", "bark.cooldown", "metrics_port", "a-b", "A1.b-c_d"] {
            assert!(set(&path, good, "1").is_ok(), "`{good}` was refused");
        }
    }

    #[test]
    fn a_dotted_key_is_one_flat_key_and_not_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "bark.cooldown", "30s").unwrap();
        set(&path, "bark.sink", "discord").unwrap();
        let stored = all(&path).unwrap();
        assert_eq!(stored.len(), 2);
        assert!(stored.contains_key("bark.cooldown"));
        assert_eq!(get(&path, "bark").unwrap(), None);
        // And on disk, not just in the map: a nested writer would produce
        // `{"bark":{"cooldown":…}}` and this is what notices.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(r#""bark.cooldown""#), "{raw}");
    }

    #[test]
    fn an_oversized_value_is_refused_by_name_and_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        let big = "x".repeat(MAX_VALUE_BYTES + 1);
        let err = set(&path, "a", &big).unwrap_err();
        let KvError::ValueTooLong { key, len } = err else {
            panic!("expected ValueTooLong, got {err:?}");
        };
        assert_eq!(key, "a");
        assert_eq!(len, MAX_VALUE_BYTES + 1);
    }

    #[test]
    fn a_store_from_a_future_shep_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        std::fs::write(&path, r#"{"version":99,"entries":{"a":"1"}}"#).unwrap();
        assert!(matches!(all(&path), Err(KvError::FutureVersion(99))));
        assert!(matches!(
            set(&path, "b", "2"),
            Err(KvError::FutureVersion(99))
        ));
        // Untouched, which is the half that matters.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(r#""a":"1""#), "{raw}");
    }

    /// `$SHEP_HOME` is already `0700`; this guards the mode a `tar`, a
    /// `cp -p` or a backup carries out with the file, where no directory
    /// mode follows.
    #[cfg(unix)]
    #[test]
    fn the_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "a", "1").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    /// Bounded: each join is under a timeout, so a lock that deadlocks fails
    /// this test instead of hanging the suite.
    #[test]
    fn two_concurrent_writers_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        const PER_WRITER: usize = 100;

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        for writer in 0..2 {
            let path = path.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                for n in 0..PER_WRITER {
                    set(&path, &format!("w{writer}.k{n}"), "v").unwrap();
                }
                done_tx.send(()).unwrap();
            });
        }
        drop(done_tx);
        for _ in 0..2 {
            done_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("a writer did not finish within 60s");
        }

        assert_eq!(all(&path).unwrap().len(), PER_WRITER * 2);
    }
}
