//! Finishing an atomic file replace so it survives the machine, not just
//! the process
//!
//! Every file this workspace rewrites in place is staged in a sibling temp
//! file, `fsync`ed, then `rename`d over the target, so a reader never
//! observes a fragment. [`sync_dir`] is the last step of that shape: it
//! makes the *rename* durable, which the temp file's own `fsync` does not.
//!
//! The two halves answer different questions, and it is easy to buy one
//! and believe you bought both. `File::sync_all` on the staging file
//! flushes that file's CONTENTS. The directory entry the rename then
//! creates is a change to the parent DIRECTORY, and it sits in the page
//! cache until that directory is itself flushed. Lose power in between and
//! the data survives with nothing pointing at it: the old file is still
//! there, and the write is silently undone.
//!
//! An ordinary crash is not the case this covers. A completed `rename(2)`
//! is visible to every later process whether or not anything was flushed,
//! so a panic, a `SIGKILL` or an `ENOSPC` already leaves either the whole
//! old file or the whole new one. Only a power cut or a kernel panic can
//! lose a landed rename, and the writer that most needs the difference is
//! the muster roll, whose whole reason to exist is being read back after
//! the machine comes up again.

use std::path::Path;

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
/// A no-op on Windows, which has no equivalent of this call and does not
/// need one for the same reasons. `File::open` on a directory fails there
/// outright unless the handle is opened with `FILE_FLAG_BACKUP_SEMANTICS`,
/// which `std` does not do; and NTFS journals metadata operations, so the
/// rename `NamedTempFile::persist` performs is already recorded rather
/// than left sitting in a cache for a directory flush to chase.
///
/// # Errors
///
/// - [`std::io::Error`] when `dir` could not be opened, or when flushing
///   it failed for a reason the filesystem could act on.
///
/// `EINVAL` is deliberately not one of them. POSIX lets `fsync` answer it
/// when the implementation has no synchronized I/O to perform for the file
/// it was handed, which is how some FUSE and network mounts answer for a
/// directory. Reporting that as a failed write would break writes that do
/// land, on hosts where they land today, so it reads as "this filesystem
/// has no such step" and the write stands. Every other error is real and
/// propagates: a helper that swallowed `EIO` could never tell a caller
/// that the durability it asked for did not happen.
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
