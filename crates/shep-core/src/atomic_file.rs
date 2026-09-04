//! The atomic file replace, its first step and its last
//!
//! Stage a fresh file beside the real one, write it, `fsync` it, `rename`
//! it over the target, then `fsync` the directory the rename landed in. A
//! reader sees the whole old file or the whole new one, never a fragment.
//! [`create_staging_file`] is the first step, [`sync_dir`] the last, and
//! the middle stays with each store, which maps its failures onto its own
//! error type.
//!
//! [`sync_dir`] makes the *rename* durable, which the temp file's own
//! `fsync` does not. On unix, and there only where the filesystem
//! implements the flush: it is a no-op on Windows, and it answers `Ok` to
//! the `EINVAL` some FUSE and network mounts return instead of flushing.

use std::path::Path;

// `$SHEP_HOME` is already `0700`, but `shep.toml` and `dogs.toml` hold
// webhook URLs with a bearer token in the path, and a `tar` or `cp -p` of
// them carries this mode somewhere no directory mode follows.
/// Mode a file under `$SHEP_HOME` is created with: owner read/write,
/// nobody else.
///
/// # Platforms
///
/// Unix only. On Windows a file inherits the ACL of the directory it lands
/// in.
pub const OWNER_ONLY_FILE_MODE: u32 = 0o600;

// No mode parameter, because a caller that wanted a looser one would be
// wrong: every file here holds a credential or an `env` value.
//
// The unique middle is not tidiness. Two writers sharing a fixed
// `<path>.tmp` had one `rename` consume the other's staging file.
//
// `tempfile_in` joins a separator onto `parent`, so `../evil` escapes the
// one directory this is contracted to stay inside.
/// Creates the staging file a store is rewritten through, in `parent` so
/// the later `rename` stays within one filesystem.
///
/// `prefix` and `suffix` bracket a unique middle `tempfile` picks, and
/// neither may contain a path separator. The file is created
/// [`OWNER_ONLY_FILE_MODE`] on unix at the `open` itself, never by a later
/// `chmod`, so it is never briefly wider. It carries no mode on Windows.
///
/// The caller writes it, `sync_all`s it, and `persist`s it over the real
/// file.
///
/// # Errors
/// - [`std::io::ErrorKind::InvalidInput`] when `prefix` or `suffix`
///   contains `/` or `\`, both refused on both platforms.
/// - Otherwise `parent` is missing or unwritable, or `tempfile` ran out of
///   attempts at a unique name.
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

// The two flushes answer different questions and it is easy to buy one
// believing you bought both. `sync_all` on the staging file flushes its
// CONTENTS; the entry the rename creates is a change to the parent
// DIRECTORY, which sits in the page cache until that directory is flushed
// too. Lose power in between and the data survives with nothing pointing
// at it. A crash is not that case: a completed `rename(2)` is visible to
// every later process whether or not anything was flushed, so it takes an
// unclean shutdown (power cut, kernel panic, hypervisor reset) to undo one,
// and the old file is what comes back. The muster roll needs the
// difference most, its whole job being read back after a reboot.
//
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

    /// fails if the file lands outside `parent`, where the later `rename`
    /// would cross a filesystem and stop being atomic.
    #[test]
    fn the_staging_file_lands_in_the_parent_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = create_staging_file(dir.path(), "kv", ".tmp").unwrap();
        assert_eq!(tmp.path().parent(), Some(dir.path()));
    }

    /// fails if either part is dropped or swapped on the way to `tempfile`.
    #[test]
    fn the_name_carries_the_prefix_and_the_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = create_staging_file(dir.path(), "barks", ".tmp").unwrap();

        let name = tmp.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("barks"), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
        assert!(name.len() > "barks.tmp".len(), "no unique middle: {name}");
    }

    /// fails if a separator reaches `tempfile`. Both spellings, both
    /// platforms, so an argument cannot be legal on one and an escape on
    /// the other.
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
