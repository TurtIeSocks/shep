//! `overrides.json`: what an operator has changed since a Flockfile was
//! loaded.
//!
//! A Flockfile arrives from an app's own repository, so a merged pull request
//! must not be able to silently change a running flock's config out from
//! under an operator who edited it live. This store is where that edit lives:
//! one entry per sheep name, holding the fields an operator set that the
//! Flockfile does not currently declare. A later file load merges the two:
//! the Flockfile's declared keys win, everything else falls back to the
//! override, then to the built-in default. The store's own shape carries
//! no merge logic itself; it is the ledger the merge reads and writes.
//!
//! # Writing
//!
//! Same shape as [`crate::kv`]: a read-modify-rename under an exclusive
//! advisory lock on a sibling `overrides.json.lock`, staged through a
//! uniquely-named `0600` temp file, `fsync`ed and `rename`d over the
//! original. Copied rather than shared because `KvLock` is private to its
//! module: see that module's own doc for why the lock exists at all and why
//! `snapshot::write_atomic`'s lock-free shape does not apply here: this store
//! is written by the daemon today and will be written by CLI verbs later, so
//! two independent OS processes can race on it exactly as `kv.json` can.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
// `PathBuf` backs `lock_path` below, which both platform arms of
// `OverridesLock` need (the unix one for `nix::fcntl::Flock`'s target, the
// windows one for the `share_mode(0)` handle), so it is gated the same way
// `lock_path` is, rather than to `cfg(unix)` alone.
#[cfg(any(unix, windows))]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The on-disk format's version.
///
/// A store carrying a HIGHER version is refused rather than read or replaced
/// ([`OverridesError::FutureVersion`]): the file holds an operator's live
/// edits with no Flockfile copy to fall back to, and there is no undo for a
/// downgrade that overwrites it. `kv.rs`'s `KV_VERSION` is the precedent.
pub const OVERRIDES_VERSION: u32 = 1;

/// One sheep's overrides: the fields an operator has set that its current
/// Flockfile does not declare.
///
/// `fields` is a flat JSON object rather than a typed `AppConfig` because a
/// later shep version may accept fields this one does not know, and reading
/// this store must not silently drop them (the same reasoning
/// [`OverridesError::FutureVersion`] applies to the whole file, applied per
/// field instead). `declared` and `declared_env` are not overrides
/// themselves: they are the set of keys the *Flockfile* has established, kept
/// here so a later merge can tell "the file used to declare this and no
/// longer does" apart from "the file never mentioned it".
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppOverrides {
    /// Operator-set field values, keyed by the same names `AppConfig`'s
    /// fields use. May include an `env` object.
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Names of fields the current Flockfile declares.
    pub declared: BTreeSet<String>,
    /// Names of `env` keys the current Flockfile declares.
    pub declared_env: BTreeSet<String>,
}

/// Redacted: `fields` can hold an `env` map, and this store is the primary
/// place an operator's secrets live (IR-41).
impl fmt::Debug for AppOverrides {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppOverrides")
            .field("fields", &format_args!("<{} fields>", self.fields.len()))
            .field("declared", &self.declared)
            .field("declared_env", &self.declared_env)
            .finish()
    }
}

/// The file's shape: a version and a flat map of sheep name to overrides.
///
/// `BTreeMap`, not `HashMap`, so the file is written in key order and two
/// writes of the same content produce byte-identical files, which makes the
/// store diffable, greppable, and safe to keep in a dotfiles repository.
/// Same argument `kv::KvFile` records.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OverridesFile {
    version: u32,
    apps: BTreeMap<String, AppOverrides>,
}

/// Error type returned by this module.
///
/// `#[non_exhaustive]`: shep-core is a published library and this enum is
/// reachable from it, so a further failure shape must not break an
/// out-of-tree consumer's `match` (IR-20).
///
/// Wraps `io::Error`/`serde_json::Error` directly rather than stringifying
/// them, matching [`crate::kv::KvError`], so callers keep the underlying
/// diagnostic through [`core::error::Error::source`], at the cost,
/// documented there too, of not deriving `Clone`/`PartialEq`/`Eq` (IR-19's
/// exception for variants wrapping `io::Error`).
#[non_exhaustive]
#[derive(Debug)]
pub enum OverridesError {
    /// The store could not be read, written, or replaced.
    Io(std::io::Error),
    /// The store's JSON could not be parsed.
    ///
    /// Refused rather than repaired: this file is an operator's live config
    /// and a partial read of it would silently drop overrides that are still
    /// on disk.
    Decode(serde_json::Error),
    /// The store on disk is a version this build does not understand; carries
    /// that version. Nothing was written.
    FutureVersion(u32),
}

impl fmt::Display for OverridesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "overrides store I/O failed: {err}"),
            Self::Decode(err) => write!(f, "overrides store failed to parse: {err}"),
            Self::FutureVersion(version) => {
                write!(
                    f,
                    "overrides store is version {version}, newer than this build understands"
                )
            }
        }
    }
}

impl core::error::Error for OverridesError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Decode(err) => Some(err),
            Self::FutureVersion(_) => None,
        }
    }
}

impl From<std::io::Error> for OverridesError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<serde_json::Error> for OverridesError {
    fn from(source: serde_json::Error) -> Self {
        Self::Decode(source)
    }
}

/// The lock file that guards `path`: its own name with `.lock` appended, so
/// it sits in `$SHEP_HOME` next to the store and inherits that directory's
/// `0700`.
///
/// Copied from `kv::lock_path`: see that module's doc for why a sibling
/// file rather than a lock on the store itself.
#[cfg(any(unix, windows))]
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".lock");
    path.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

/// An exclusive advisory lock over one overrides store, held for as long as
/// the value lives and released when it drops (including on an early `?`,
/// and by the kernel if the process dies holding it).
///
/// Copied from `kv::KvLock`, which is private to its module: see that
/// module's doc for the two-platform dance this mirrors, and for why the
/// lock is on a **sibling** `overrides.json.lock`, never on the store
/// itself.
struct OverridesLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a leading
    /// underscore because it is held, never read.
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// The lock file, opened with `share_mode(0)` so no other handle,
    /// same-process or not, read or write, can open it while this one is
    /// live. Released by this handle's `Drop`, the same role `_flock` plays
    /// on unix. Named with a leading underscore because it is held, never
    /// read.
    #[cfg(windows)]
    _handle: std::fs::File,
}

impl OverridesLock {
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
    /// same exclusivity through a different door: see `kv::KvLock::acquire`
    /// (windows) for the full reasoning this mirrors, including why it polls
    /// on a short sleep rather than blocking.
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
        /// than pulled from `windows-sys`, matching `kv::KvLock::acquire`.
        const ERROR_SHARING_VIOLATION: i32 = 32;

        /// How long a contended retry sleeps before trying again. Short
        /// enough that a lock held for a normal `put`/`get`'s duration (a
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
/// A missing file reads as an empty, current-version store: a fresh
/// `$SHEP_HOME` has no overrides, and that is the normal state, not a fault.
/// Any other `io::Error` propagates.
fn read_file(path: &Path) -> Result<OverridesFile, OverridesError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(OverridesFile::default());
        }
        Err(err) => return Err(OverridesError::Io(err)),
    };
    let file: OverridesFile = serde_json::from_str(&raw)?;
    if file.version > OVERRIDES_VERSION {
        return Err(OverridesError::FutureVersion(file.version));
    }
    Ok(file)
}

/// Rewrites `path` to hold exactly `file`, atomically: see this module's
/// own doc for the staged-temp-file-then-rename shape.
fn write_file(path: &Path, file: &OverridesFile) -> Result<(), OverridesError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = crate::atomic_file::create_staging_file(parent, "overrides", ".tmp")?;

    let json = serde_json::to_string_pretty(file)?;
    tmp.write_all(json.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;

    // `persist` is `rename(2)`. On failure the `NamedTempFile` comes back
    // inside the error and its `Drop` removes the staging file, so a failed
    // replace does not leave one behind.
    tmp.persist(path)
        .map_err(|err| OverridesError::Io(err.error))?;
    Ok(())
}

/// Every sheep's overrides, in name order.
///
/// # Errors
///
/// - [`OverridesError::Io`]: the store could not be opened or read. A store
///   that is simply absent is not an error: it reads as empty.
/// - [`OverridesError::Decode`]: the file is not the JSON this module
///   writes.
/// - [`OverridesError::FutureVersion`]: the file's `version` is newer than
///   [`OVERRIDES_VERSION`]. Nothing is read and nothing is written.
pub fn all(path: &Path) -> Result<BTreeMap<String, AppOverrides>, OverridesError> {
    // Taking the lock here too costs one extra `open` and removes the
    // question of whether a lock-free reader could observe a half-`rename`d
    // file entirely: harmless in practice, since the rename is atomic and
    // the worst case is a whole old file, but not worth reasoning about
    // twice. Do not "optimize" this away without re-deriving that. Same
    // argument `kv::all` records.
    let _lock = OverridesLock::acquire(path)?;
    Ok(read_file(path)?.apps)
}

/// One sheep's overrides, or `None` if it has none.
///
/// # Errors
///
/// [`OverridesError::Io`], [`OverridesError::Decode`] and
/// [`OverridesError::FutureVersion`], exactly as [`all`] returns them.
pub fn get(path: &Path, name: &str) -> Result<Option<AppOverrides>, OverridesError> {
    Ok(all(path)?.remove(name))
}

/// Stores `value` under `name`, replacing any previous overrides.
///
/// # Errors
///
/// - [`OverridesError::FutureVersion`]: the store on disk is newer than this
///   build understands. **Nothing is written**; a downgrade that overwrote
///   an operator's overrides has no undo.
/// - [`OverridesError::Decode`]: the existing file could not be parsed.
///   Refused rather than replaced, for the same reason.
/// - [`OverridesError::Io`]: the lock, the temp file, the `fsync` or the
///   `rename` failed. Either the whole write landed or none of it did.
pub fn put(path: &Path, name: &str, value: &AppOverrides) -> Result<(), OverridesError> {
    let _lock = OverridesLock::acquire(path)?;
    let mut file = read_file(path)?;
    file.version = OVERRIDES_VERSION;
    file.apps.insert(name.to_string(), value.clone());
    write_file(path, &file)
}

/// Removes `name`'s overrides, returning whether it was there.
///
/// # Errors
///
/// The same set [`put`] returns: `FutureVersion`, `Decode`, `Io`.
pub fn remove(path: &Path, name: &str) -> Result<bool, OverridesError> {
    let _lock = OverridesLock::acquire(path)?;
    let mut file = read_file(path)?;
    let was_present = file.apps.remove(name).is_some();
    if was_present {
        file.version = OVERRIDES_VERSION;
        write_file(path, &file)?;
    }
    Ok(was_present)
}

/// Applies several changes at once: `Some` stores, `None` removes.
///
/// One lock acquisition and one rewrite for the whole batch, where the
/// per-name [`put`] and [`remove`] take one each. The daemon merges a whole
/// Flockfile in one pass, and doing that through the single-name calls made
/// an eleven-app file 11 full rewrites of this store on the thread
/// supervising the flock. It also makes the record of one load atomic: either
/// every app the load established is written, or none is.
///
/// Names this batch does not mention are left exactly as they are, which is
/// what makes this safe against a concurrent writer touching a different app:
/// the read and the write both happen under the one lock, so this is a
/// read-modify-write of the whole file rather than a blind overwrite of it.
///
/// An empty batch takes no lock and writes nothing.
///
/// # Errors
///
/// The same set [`put`] returns: `FutureVersion`, `Decode`, `Io`. Nothing is
/// written on any of them.
pub fn update(
    path: &Path,
    changes: &BTreeMap<String, Option<AppOverrides>>,
) -> Result<(), OverridesError> {
    if changes.is_empty() {
        return Ok(());
    }
    let _lock = OverridesLock::acquire(path)?;
    let mut file = read_file(path)?;
    for (name, change) in changes {
        match change {
            Some(value) => {
                file.apps.insert(name.clone(), value.clone());
            }
            None => {
                file.apps.remove(name);
            }
        }
    }
    file.version = OVERRIDES_VERSION;
    write_file(path, &file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a batch does not store and remove in one pass, or if it
    /// touches a name it was not given. The daemon writes a whole Flockfile
    /// through this, so a batch that clobbered an app the file never
    /// mentioned would delete an operator's overrides for it.
    #[test]
    fn update_stores_removes_and_leaves_the_rest_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("overrides.json");
        let record = |value: u64| AppOverrides {
            fields: [("max_restarts".to_string(), serde_json::json!(value))]
                .into_iter()
                .collect(),
            ..AppOverrides::default()
        };
        put(&path, "web", &record(1)).unwrap();
        put(&path, "worker", &record(2)).unwrap();
        put(&path, "bystander", &record(3)).unwrap();

        let changes = BTreeMap::from([
            ("web".to_string(), Some(record(9))),
            ("worker".to_string(), None),
        ]);
        update(&path, &changes).unwrap();

        let all = all(&path).unwrap();
        assert_eq!(all.get("web"), Some(&record(9)));
        assert_eq!(all.get("worker"), None);
        assert_eq!(all.get("bystander"), Some(&record(3)));
    }

    /// fails if an empty batch writes anything. A load whose every app
    /// refused must not rewrite the store at all.
    #[test]
    fn an_empty_update_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("overrides.json");
        update(&path, &BTreeMap::new()).unwrap();
        assert!(!path.exists(), "an empty batch created a store");
    }

    /// fails if a written override does not come back.
    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("overrides.json");
        let mut fields = serde_json::Map::new();
        fields.insert("max_memory".to_string(), serde_json::json!("512M"));
        let value = AppOverrides {
            fields,
            declared: ["name", "script"].iter().map(|s| s.to_string()).collect(),
            declared_env: BTreeSet::new(),
        };
        put(&path, "web", &value).unwrap();
        assert_eq!(get(&path, "web").unwrap().as_ref(), Some(&value));
    }

    /// fails if a missing store is an error. A fresh $SHEP_HOME has no
    /// overrides and that is the normal state, not a fault.
    #[test]
    fn a_missing_store_reads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(all(&dir.path().join("overrides.json")).unwrap().is_empty());
    }

    /// fails if the store is readable by anyone but its owner. It holds env
    /// values, which is what flock.json's own owner-only test exists for.
    #[cfg(unix)]
    #[test]
    fn the_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("overrides.json");
        put(&path, "web", &AppOverrides::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// fails if Debug prints an env value. This store is where an operator's
    /// secrets will live (IR-41).
    #[test]
    fn debug_redacts_override_values() {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "env".to_string(),
            serde_json::json!({"DATABASE_URL": "postgres://hunter2"}),
        );
        let value = AppOverrides {
            fields,
            ..AppOverrides::default()
        };
        let rendered = format!("{value:?}");
        assert!(!rendered.contains("hunter2"), "leaked: {rendered}");
        // Exact string pinned so a lazy derive(Debug) refactor fails here,
        // matching `config::app`'s own `debug_redacts_env_values`.
        assert_eq!(
            rendered,
            "AppOverrides { fields: <1 fields>, declared: {}, declared_env: {} }"
        );
    }

    /// fails if a store written by a NEWER shep is silently rewritten by an
    /// older one, which would drop every field this binary does not know.
    #[test]
    fn a_future_version_refuses_without_clobbering() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("overrides.json");
        std::fs::write(&path, r#"{"version":99,"apps":{}}"#).unwrap();
        assert!(matches!(
            get(&path, "web"),
            Err(OverridesError::FutureVersion(99))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"version":99,"apps":{}}"#
        );
    }

    /// fails if two concurrent writers lose each other's work. This is what
    /// the lock is for; `kv.rs`'s own version of this test is the model.
    ///
    /// Bounded (IR-46): each join is under a timeout, so a lock that
    /// deadlocks fails this test instead of hanging the suite.
    #[test]
    fn two_concurrent_writers_lose_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("overrides.json");
        const PER_WRITER: usize = 50;

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        for writer in 0..2 {
            let path = path.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                for n in 0..PER_WRITER {
                    put(&path, &format!("w{writer}-{n}"), &AppOverrides::default()).unwrap();
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
