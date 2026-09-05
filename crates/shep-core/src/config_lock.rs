//! The exclusive advisory lock a config file's writers hold across their
//! read-modify-write, and the staging file they write the new value
//! through.
//!
//! Moved here from `shep-cli` (where it was `pub(super)` and reachable only
//! from that binary) because `dogs.toml` is about to gain a daemon-side
//! writer: the daemon holds no lock on it today, and a type it cannot name
//! is a type it cannot hold. `shep-cli`'s three existing writers of
//! `shep.toml` and `dogs.toml` keep using this one unchanged, imported back
//! in through a `pub(super) use`.
//!
//! Deliberately not consolidated with [`crate::overrides`]'s own
//! `OverridesLock`, which is the same `flock(2)`/`share_mode(0)` shape
//! against a different lock file. The two crates that hold a [`ConfigLock`]
//! already agree on its ordering (`shep.toml` outer, `dogs.toml` inner, per
//! `dog_migration.rs`'s own header), and unifying the two lock types is a
//! separate, later change.

use std::path::{Path, PathBuf};

/// Creates the staging file a config is written through, in `parent` so
/// the later `rename` stays within one filesystem.
///
/// The create-at-mode reasoning lives with
/// [`crate::atomic_file::create_staging_file`], which four stores now
/// share. What is left here is the pair of names, and this wrapper is
/// where they stay: `commands::dog_migration` writes `dogs.toml` through
/// the same staging name as `shep.toml`, and two call sites spelling that
/// pair out separately is how the two would drift.
///
/// # Errors
/// The staging file could not be created in `parent`.
pub fn create_config_file(parent: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    crate::atomic_file::create_staging_file(parent, "shep", ".toml.tmp")
}

/// An exclusive advisory lock over one config file, held for as long as
/// the value lives and released when it drops (including on an early `?`,
/// and by the kernel if the process dies holding it).
///
/// Keyed on the path it is given rather than on `shep.toml` specifically:
/// `shep-cli`'s `ShepToml::edit` takes one over `shep.toml`, and
/// `commands::dog_migration` takes one over `dogs.toml`, which has two
/// writers of its own. **Whenever both are held at once, `shep.toml`'s is
/// taken first**, which is the whole of what keeps the two orderings from
/// deadlocking; `migrate_dog_sections` is the one caller that holds both,
/// and it says so at the point it nests them.
///
/// The lock is on a sibling `<name>.lock`, never on the config itself,
/// and that is the whole design decision — the same one `barks::RingLock`
/// records: `ShepToml::save` finishes by `rename`ing a new file over the
/// config, which replaces the inode. A lock taken on the config would be a
/// lock on an inode the very next successful save unlinks; the next writer
/// would open the *new* inode, find it unlocked, and the two would be
/// excluding nothing. The lock file is never renamed, never rewritten and
/// never read; it exists only to be an inode with a stable identity, and
/// it is left on disk between edits on purpose so both writers keep
/// agreeing on which one it is.
///
/// Derives `Debug` rather than opting out: the fields are a held OS lock
/// handle (a `flock(2)` wrapper on unix, a bare `File` on Windows), never a
/// secret, so there is nothing here for a redacted impl to protect.
#[derive(Debug)]
pub struct ConfigLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a
    /// leading underscore because it is held, never read.
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// The lock file, opened with `share_mode(0)`. The same primitive and
    /// the same sibling-file shape [`crate::kv`] and [`crate::barks`] use;
    /// see either for the full argument.
    #[cfg(windows)]
    _handle: std::fs::File,
}

impl ConfigLock {
    /// Blocks until this process holds `path`'s lock exclusively.
    ///
    /// # Errors
    /// The lock file could not be created beside `path`, or `flock` failed
    /// for a reason other than contention (contention blocks rather than
    /// failing).
    #[cfg(windows)]
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt as _;

        /// Another handle already holds share access this open denies.
        const ERROR_SHARING_VIOLATION: i32 = 32;
        /// How long a contended retry sleeps. The unix arm blocks in the
        /// kernel; this polls, for the reason `shep_core::kv` documents.
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

    /// Blocks until this process holds `path`'s lock exclusively.
    ///
    /// # Errors
    /// The lock file could not be created beside `path`, or `flock` failed
    /// for a reason other than contention (contention blocks rather than
    /// failing).
    #[cfg(unix)]
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;

        use nix::fcntl::{Flock, FlockArg};

        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(crate::atomic_file::OWNER_ONLY_FILE_MODE)
            .open(lock_path(path))?;

        // `LockExclusive` blocks; the non-blocking variant would need a
        // retry loop and a deadline, and a `shep enable` that waits its
        // turn behind a concurrent `shep adopt` is exactly the behaviour
        // wanted here.
        Flock::lock(file, FlockArg::LockExclusive)
            .map(|flock| Self { _flock: flock })
            .map_err(|(_file, errno)| std::io::Error::from(errno))
    }
}

/// The lock file that guards `path`: its own name with `.lock` appended,
/// so it sits in `$SHEP_HOME` next to the config and inherits that
/// directory's `0700`.
fn lock_path(path: &Path) -> PathBuf {
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

    #[test]
    fn a_second_acquire_on_the_same_path_blocks_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dogs.toml");
        let first = ConfigLock::acquire(&path).unwrap();
        let path2 = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let _second = ConfigLock::acquire(&path2).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "must block"
        );
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("must proceed once released");
        t.join().unwrap();
    }

    #[test]
    fn a_staged_config_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = create_config_file(dir.path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = tmp.as_file().metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
