//! `kv.json`: the shepherd's key/value store (spec §5).
//!
//! A flat map of short strings under `$SHEP_HOME`, for ad-hoc operator notes
//! and dog runtime tweaks. Explicitly **not** the primary config path — a
//! Flockfile is what configures a sheep and `shep.toml` is what configures the
//! shepherd and its dogs. This is the place for the things neither of those
//! has a field for.
//!
//! # Why this is a file and not an RPC
//!
//! Spec §5 says the store is for "ad-hoc + dog runtime tweaks", so a dog reads
//! it — which rules out keeping it private to shep-cli, and is why it lives
//! here, where every crate in the workspace and every `shep dog <name>` gets it
//! for free. It does NOT follow that it has to go over the socket. A dog's
//! `[dog.<name>]` section travels that way because the alternative on the table
//! was the child's ENVIRONMENT, which is readable from the process table,
//! inherited by every grandchild and captured into crash dumps (spec §8). A
//! `0600` file inside a `0700` `$SHEP_HOME`, opened by a process running as the
//! same user, has none of those properties, so the socket would buy nothing —
//! while costing the thing every other config verb in this tree provides:
//! `shep set` works with no shepherd running, exactly as `shep enable` and
//! `shep barks` do.
//!
//! # Writing
//!
//! Every mutation is a read-modify-rename under an exclusive advisory lock on a
//! sibling `kv.json.lock`, with the new content staged through a uniquely-named
//! `0600` temp file, `fsync`ed and `rename`d over the original. That is the
//! same shape `barks::append` uses, for the same reasons and after the same
//! bug: two processes appending to `barks.jsonl` silently lost half of each
//! other's records until an advisory lock landed there, and a shared temp name
//! had one writer's `rename` consume the other's staging file. Do not
//! reimplement either half here — it is a third instance of one pattern, not a
//! third pattern.
//!
//! # Keys
//!
//! One flat string per key, matching `[A-Za-z0-9._-]`, 1 to
//! [`MAX_KEY_BYTES`], not starting with `.`. A dot is part of a NAME, not a
//! path: `bark.cooldown` is one key, and there is no nested object behind it.
//! map.md inherited a dotted-path parse from pm2's own store; this project's
//! standing decision is that pm2's formats live only in the importer, and a
//! nesting grammar here would be a second config language — with its own
//! quoting rules — for a store the spec itself calls not the primary config
//! path. The narrow alphabet also means `shep get $key` never needs quoting.

use core::fmt;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
// `PathBuf` backs `lock_path` below, which both platform arms of `KvLock`
// need — the unix one for `nix::fcntl::Flock`'s target, the windows one for
// the `share_mode(0)` handle — so it is gated the same way `lock_path` is,
// rather than to `cfg(unix)` alone.
#[cfg(any(unix, windows))]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The on-disk format's version.
///
/// A store carrying a HIGHER version is refused rather than read or replaced
/// ([`KvError::FutureVersion`]): the file is small, it is an operator's, and
/// there is no undo for a downgrade that overwrites it. The muster roll's
/// `SNAPSHOT_VERSION` is the precedent.
pub const KV_VERSION: u32 = 1;

/// Longest key this store accepts, in bytes.
pub const MAX_KEY_BYTES: usize = 128;

/// Longest value this store accepts, in bytes.
///
/// The store is read whole on every access, and a cap is what keeps it from
/// quietly becoming a blob store — which it would, because it is the only
/// writable thing in `$SHEP_HOME` with no schema.
pub const MAX_VALUE_BYTES: usize = 4096;

/// The file's shape: a version and a flat map.
///
/// `BTreeMap`, not `HashMap`, so the file is written in key order and two
/// writes of the same content produce byte-identical files — which makes the
/// store diffable, greppable, and safe to keep in a dotfiles repository.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KvFile {
    version: u32,
    entries: BTreeMap<String, String>,
}

/// Error type returned by this module.
///
/// `#[non_exhaustive]`: shep-core is a published library and this enum is
/// reachable from it, so a further failure shape — a store whose size exceeded
/// a future cap, say — must not break an out-of-tree consumer's `match`
/// (IR-20).
///
/// Wraps `io::Error`/`serde_json::Error` directly rather than stringifying
/// them, matching [`BarkError`](crate::barks::BarkError), so callers keep the
/// underlying diagnostic through [`core::error::Error::source`] — at the cost,
/// documented there too, of not deriving `Clone`/`PartialEq`/`Eq` (IR-19's
/// exception for variants wrapping `io::Error`).
#[non_exhaustive]
#[derive(Debug)]
pub enum KvError {
    /// The store could not be read, written, or replaced.
    Io(std::io::Error),
    /// The store's JSON could not be parsed.
    ///
    /// Refused rather than repaired: unlike `barks.jsonl`, which is read during
    /// an incident and so forgives a bad line, this file is a map an operator
    /// wrote and a partial read of it would silently drop keys that are still
    /// on disk.
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
/// [`KvError::InvalidKey`] — empty, over [`MAX_KEY_BYTES`], starting with `.`,
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
/// `cfg(any(unix, windows))` alongside its two callers — [`KvLock::acquire`]
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

/// An exclusive advisory lock over one kv store, held for as long as the
/// value lives and released when it drops (including on an early `?`, and by
/// the kernel if the process dies holding it).
///
/// The same lock [`barks::RingLock`](crate::barks) documents, on this file:
/// on a **sibling** `kv.json.lock`, never on the store itself, because the
/// `rename` that installs new content replaces the inode a lock on the
/// target would be held on.
struct KvLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a leading
    /// underscore because it is held, never read.
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
    /// `flock(2)` has no Windows equivalent, but `share_mode(0)` gives the
    /// same exclusivity through a different door: opening the lock file with
    /// every share flag cleared means no other handle — another process's or
    /// this one's, read or write — can be opened on it while this handle
    /// lives, which is mandatory (enforced by the OS on every open, not just
    /// respected by cooperating callers) exactly as `flock` is. What it does
    /// not give is a blocking wait: a contended open fails immediately with
    /// `ERROR_SHARING_VIOLATION` rather than parking the thread the way
    /// `flock`'s `LockExclusive` does, so this polls on a short sleep until
    /// the open succeeds. Two writers in the *same* process are covered too —
    /// Windows share-mode denial is per-file, not per-process, so a second
    /// thread's open contends with the first thread's open handle exactly as
    /// a second process's would.
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
        /// than pulled from `windows-sys` — this crate has no Windows-only
        /// dependency today, and one well-known, stable error code does not
        /// earn it one.
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
/// A missing file reads as an empty, current-version store — `shep get`
/// against a fresh `$SHEP_HOME` is the first thing anyone runs, and an
/// `ENOENT` in their face would be wrong. Any other `io::Error` propagates.
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

/// Rewrites `path` to hold exactly `file`, atomically — see this module's
/// own doc for the staged-temp-file-then-rename shape.
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
    Ok(())
}

/// Every key/value pair in the store, in key order.
///
/// # Errors
///
/// - [`KvError::Io`] — the store could not be opened or read. A store that is
///   simply absent is not an error: it reads as empty.
/// - [`KvError::Decode`] — the file is not the JSON this module writes.
/// - [`KvError::FutureVersion`] — the file's `version` is newer than
///   [`KV_VERSION`]. Nothing is read and nothing is written.
pub fn all(path: &Path) -> Result<BTreeMap<String, String>, KvError> {
    // Taking the lock here too costs one extra `open` and removes the
    // question of whether a lock-free reader could observe a half-`rename`d
    // file entirely — harmless in practice, since the rename is atomic and
    // the worst case is a whole old file, but not worth reasoning about
    // twice. Do not "optimize" this away without re-deriving that.
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
/// - [`KvError::InvalidKey`] — the key is outside the grammar.
/// - [`KvError::ValueTooLong`] — the value exceeds [`MAX_VALUE_BYTES`].
/// - [`KvError::FutureVersion`] — the store on disk is newer than this build
///   understands. **Nothing is written**; a downgrade that overwrote an
///   operator's store has no undo.
/// - [`KvError::Decode`] — the existing file could not be parsed. Refused
///   rather than replaced, for the same reason.
/// - [`KvError::Io`] — the lock, the temp file, the `fsync` or the `rename`
///   failed. Either the whole write landed or none of it did.
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
/// store that does not exist clears to `0` rather than failing — `shep unset
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

    /// fails if a set value cannot be read back, or if the file is not created
    /// on first write. Everything else here is a refusal or a race; this is the
    /// one case that says the store stores.
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

    /// fails if a missing store is an error rather than an empty one. `shep get`
    /// against a fresh `$SHEP_HOME` is the first thing anyone runs, and an
    /// `ENOENT` in their face would be wrong: the store has no keys, which is
    /// a fact, not a failure.
    #[test]
    fn a_store_that_does_not_exist_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        assert!(all(&path).unwrap().is_empty());
        assert_eq!(get(&path, "anything").unwrap(), None);
    }

    /// fails if `unset` stops distinguishing a key it removed from one that was
    /// never there. `shep unset typo` has to be able to say so rather than
    /// exiting 0 on a no-op the operator will read as success.
    #[test]
    fn unset_reports_whether_the_key_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "a", "1").unwrap();
        assert!(unset(&path, "a").unwrap());
        assert!(!unset(&path, "a").unwrap());
    }

    /// fails if `clear` misreports how much it removed, or leaves anything.
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

    /// fails if the key grammar widens. Each rejection here is deliberate: a
    /// key goes onto a shell command line (`shep get $k`) and into a JSON
    /// object, so whitespace, control characters and an empty name all have to
    /// be refused at the door rather than quoted around forever.
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

    /// fails if a key that merely CONTAINS a dot is treated as a path into a
    /// nested object. `bark.cooldown` is one key whose name has a dot in it —
    /// the store is flat, and the dot is a naming convention, not a grammar.
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

    /// fails if an oversized value is stored. The store is `$SHEP_HOME`'s
    /// smallest file and is read whole on every access; a cap keeps it from
    /// quietly becoming a blob store.
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

    /// fails if a store written by a future shep is silently overwritten. This
    /// file is small but it is an operator's, and clobbering it on a downgrade
    /// would be an unrecoverable loss for no gain.
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

    /// fails if the file is created group- or world-readable. `$SHEP_HOME` is
    /// already `0700`, so this is belt-and-braces — and it is the mode a `tar`,
    /// a `cp -p` or a backup carries out of that directory with the file, where
    /// no directory mode follows it. Same argument `barks.jsonl` records.
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

    /// fails if two concurrent writers lose each other's keys. This is not a
    /// theoretical race: `barks.jsonl` lost half of 400 records to exactly this
    /// shape before it grew the same advisory lock, and the store has the same
    /// two-writer future (an operator's `shep set` and a dog's own).
    ///
    /// Bounded (IR-46): the join is under a timeout, so a lock that deadlocks
    /// fails this test instead of hanging the suite.
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
