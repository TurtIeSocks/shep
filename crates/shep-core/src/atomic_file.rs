//! The atomic file replace, its first step and its last
//!
//! Every file this workspace rewrites in place goes the same way: stage a
//! fresh file beside the real one, write it, `fsync` it, `rename` it over
//! the target, then `fsync` the directory the rename landed in. A reader
//! sees the whole old file or the whole new one, never a fragment.
//!
//! [`create_staging_file`] is the first step and [`sync_dir`] is the last.
//!
//! [`sync_dir`] makes the *rename* durable, which the temp file's own
//! `fsync` does not. On unix. It is a no-op on Windows, so every
//! durability claim below, and in the six writers that call it, is a unix
//! one.
//!
//! Those two flushes answer different questions, and it is easy to buy one
//! and believe you bought both. `File::sync_all` on the staging file
//! flushes that file's CONTENTS. The directory entry the rename then
//! creates is a change to the parent DIRECTORY, and it sits in the page
//! cache until that directory is itself flushed. Lose power in between and
//! the data survives with nothing pointing at it: the old file is still
//! there, and the write is silently undone.
//!
//! An ordinary crash is not the case [`sync_dir`] covers. A completed
//! `rename(2)` is visible to every later process whether or not anything
//! was flushed, so a panic, a `SIGKILL` or an `ENOSPC` already leaves
//! either the whole old file or the whole new one. Only a power cut or a
//! kernel panic can lose a landed rename, and the writer that most needs
//! the difference is the muster roll, whose whole reason to exist is being
//! read back after the machine comes up again.
//!
//! The middle is not here. The write, the staging file's own `sync_all`
//! and the `persist` stay with each store, because each maps a failure of
//! them onto its own error type.

// Where the create step came from, which is a maintainer's question rather
// than a caller's (IR-31). `kv.json`, `barks.jsonl`, `overrides.json` and
// `shep.toml` (with `dogs.toml` riding on the last one) each carried their
// own copy of it, six lines apiece and identical but for a prefix, a
// suffix and a mode constant that held `0o600` in all four. A fifth copy
// was one `pub(super)` away when this became one function instead.

use std::path::Path;

// Why `0600` and not something a caller could widen (IR-31). Four files
// reached it by different arguments, and the strictest is the one that
// governs a shared value. `$SHEP_HOME` is itself `0700`, so for `kv.json`,
// `barks.jsonl` and `overrides.json` this is a second lock on the same
// door. For `shep.toml` and `dogs.toml` it is the only lock that counts:
// `docs/dogs.md` tells an operator to paste a Discord or Slack webhook URL
// there, and both of those carry a bearer token in the path. It is also
// the mode a `tar`, a `cp -p` or a backup of `$SHEP_HOME` carries out with
// the file, somewhere no directory mode follows it.
/// Mode a file under `$SHEP_HOME` is created with: owner read/write,
/// nobody else.
///
/// # Platforms
///
/// Unix only. Windows has no equivalent bit, so nothing there applies this
/// and a file inherits the ACL of the directory it lands in. That is the
/// same gap `shep.toml`'s own home-directory creation names in the
/// operator docs.
pub const OWNER_ONLY_FILE_MODE: u32 = 0o600;

// The three choices a caller might otherwise ask about (IR-31).
//
// NO MODE PARAMETER. Every caller writes a file under `$SHEP_HOME` that
// holds a credential, an `env` value, or a record of what the shepherd
// told an outside service, so none of them wants anything but
// `OWNER_ONLY_FILE_MODE`. A parameter would buy nothing today and would be
// the door a later caller walks a `0644` through.
//
// A UNIQUE MIDDLE, not a fixed `<path>.tmp`. Two writers sharing one had
// a process's `rename` consume the other's staging file, and the loser
// died with `ENOENT` renaming a path that no longer existed.
//
// NO SEPARATOR IN EITHER PART. `tempfile`'s own docs call one legal and
// inadvisable, and `tempfile_in` joins the result onto `parent`, so a `..`
// in front of a separator stages the file outside the directory this
// function's whole contract is that it stays inside. Nothing in the tree
// passes anything but a literal; the check is what keeps that true now
// four private helpers have become one public one.
//
// `tempfile`'s OWN TYPE back, rather than a newtype. The caller does the
// writing, the `sync_all` and the `persist`, so a wrapper's whole body
// would forward those three calls. The cost is a dependency's type in this
// crate's public API, taken deliberately.
/// Creates the staging file a store is rewritten through, in `parent` so
/// the later `rename` stays within one filesystem.
///
/// `prefix` and `suffix` bracket a unique middle `tempfile` picks, and
/// neither may contain a path separator. The file is created
/// [`OWNER_ONLY_FILE_MODE`] on unix, at the `open` itself rather than by a
/// later `chmod`, so there is no window in which it sits at whatever the
/// process umask leaves it. It carries no mode on Windows.
///
/// The caller writes it, `sync_all`s it, and `persist`s it over the real
/// file.
///
/// # Errors
/// - [`std::io::ErrorKind::InvalidInput`] when `prefix` or `suffix`
///   contains `/` or `\`. Both are refused on both platforms, so the same
///   argument is accepted or refused wherever it runs.
/// - Otherwise, the staging file could not be created in `parent`: the
///   directory is missing or unwritable, or `tempfile` ran out of attempts
///   at a unique name.
pub fn create_staging_file(
    parent: &Path,
    prefix: &str,
    suffix: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    for (label, part) in [("prefix", prefix), ("suffix", suffix)] {
        if part.contains(['/', '\\']) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("staging file {label} must not contain a path separator: {part:?}"),
            ));
        }
    }

    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix).suffix(suffix);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(OWNER_ONLY_FILE_MODE));
    }

    builder.tempfile_in(parent)
}

// Why the two arms differ, which is not a caller's question (IR-31).
//
// UNIX. `fsync` on a directory descriptor is the portable way to flush the
// entry a rename created, and `EINVAL` is tolerated because POSIX lets
// `fsync` answer it when the implementation has no synchronized I/O to
// perform for the file it was handed. Some FUSE and network mounts answer
// exactly that for a directory, and reporting it as a failed write would
// break writes that do land, on hosts where they land today. Every other
// error propagates: a helper that swallowed `EIO` could never tell a
// caller that the durability it asked for did not happen.
//
// WINDOWS. There is no call to make. `File::open` on a directory fails
// outright unless the handle carries `FILE_FLAG_BACKUP_SEMANTICS`, which
// `std` does not pass, so the unix arm would not even compile into
// something runnable. NTFS journals metadata operations, which keeps the
// filesystem CONSISTENT across a crash, and that is a weaker promise than
// the unix arm makes: `MoveFileEx` without `MOVEFILE_WRITE_THROUGH` does
// not force the rename out, so a power cut can still lose it. Closing that
// would mean reaching past `std` for a directory handle, or a
// write-through rename in place of `NamedTempFile::persist`. Neither is
// free and neither is done here, so the honest position is that the
// guarantee below is a unix one.
/// Flushes `dir`'s own metadata, making renames into it durable.
///
/// Call it after the `rename` that installs a staged file, not before: it
/// is the directory entry created by that rename that needs to reach the
/// disk. Callers that skip it keep the atomicity guarantee (a reader sees
/// the old file or the new one, never a fragment) and lose only the
/// durability one, and only to a power cut.
///
/// # Platforms
///
/// Unix only. On Windows this is a no-op that answers `Ok` without
/// touching `dir`, so a rename there is as durable as NTFS makes it and no
/// more. Callers get the same API on both and a weaker guarantee on one.
///
/// # Errors
///
/// - [`std::io::Error`] when `dir` could not be opened, or when flushing
///   it failed for a reason the filesystem could act on. `EINVAL` is not
///   one of them: it reads as "this filesystem has no such step" and the
///   write stands. Never returns an error on Windows.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        match std::fs::File::open(dir)?.sync_all() {
            Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
            other => other,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the staging file lands anywhere but the directory the
    /// caller named. A `rename` is only atomic within one filesystem, so
    /// staging next to the real file is the whole reason this takes a
    /// `parent` rather than using the system temp directory.
    #[test]
    fn the_staging_file_lands_in_the_parent_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = create_staging_file(dir.path(), "kv", ".tmp").unwrap();
        assert_eq!(tmp.path().parent(), Some(dir.path()));
    }

    /// fails if a caller's prefix or suffix is dropped on the way to
    /// `tempfile`. Each store passes its own pair, and swapping them would
    /// leave every store staging under one name.
    #[test]
    fn the_name_carries_the_prefix_and_the_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = create_staging_file(dir.path(), "barks", ".tmp").unwrap();

        let name = tmp.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("barks"), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
        assert!(name.len() > "barks.tmp".len(), "no unique middle: {name}");
    }

    /// fails if a separator in either argument reaches `tempfile`, which
    /// allows one and joins the result onto `parent`. Both spellings are
    /// refused on both platforms so an argument cannot be legal on one and
    /// an escape on the other.
    #[test]
    fn a_path_separator_is_refused_in_either_argument() {
        let dir = tempfile::tempdir().unwrap();

        for (prefix, suffix) in [
            ("../escape", ".tmp"),
            ("kv", "/etc/passwd"),
            ("..\\escape", ".tmp"),
            ("kv", "\\tmp"),
        ] {
            let err = create_staging_file(dir.path(), prefix, suffix)
                .expect_err("a separator must not reach tempfile");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "{prefix:?} {suffix:?}: {err:?}"
            );
        }
    }

    /// fails if the staging file is created readable by anyone but its
    /// owner. `persist` is a `rename`, which installs this inode and its
    /// mode over the real file, so this mode is the one the store ends up
    /// wearing on disk.
    #[cfg(unix)]
    #[test]
    fn the_staging_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let tmp = create_staging_file(dir.path(), "overrides", ".tmp").unwrap();

        let mode = tmp.as_file().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:o}");
    }

    #[test]
    fn sync_dir_accepts_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        // The Windows arm returns `Ok` without looking at the path, so this
        // only has teeth on unix -- which is the only place it has a job.
        sync_dir(dir.path()).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn sync_dir_reports_a_directory_that_is_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");

        // Guards the `EINVAL` arm above from widening into "ignore every
        // error": a `sync_dir` that answered `Ok` to everything would pass
        // the test above and leave a caller unable to learn that the flush
        // it asked for never happened.
        let err = sync_dir(&missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err:?}");
    }
}
