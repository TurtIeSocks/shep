//! The atomic file replace: its first step and its last.
//!
//! Stage a fresh file beside the real one, write it, `fsync` it, `rename`
//! it over the target, then `fsync` the directory the rename landed in. A
//! reader sees the whole old file or the whole new one, never a fragment.
//! [`create_staging_file`] is the first step, [`sync_dir`] the last; the
//! middle stays with each store.
//!
//! [`sync_dir`] makes the rename durable, which the temp file's own
//! `fsync` does not, on unix only, and only where the filesystem
//! implements the flush.

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

// No mode parameter: everything staged here holds a credential or an
// `env` value, so one fixed mode is always right. Prefix and suffix
// refuse separators because `tempfile_in` joins them onto `parent`, and
// `../evil` would escape the directory this is contracted to stay inside.
/// Creates the staging file a store is rewritten through, in `parent` so
/// the later `rename` stays within one filesystem.
///
/// `prefix` and `suffix` bracket a unique middle `tempfile` picks: two
/// concurrent writers never share a name. Neither may contain a path
/// separator. Created [`OWNER_ONLY_FILE_MODE`] on unix at the `open`
/// itself, never by a later `chmod`. Carries no mode on Windows.
///
/// # Errors
/// - [`std::io::ErrorKind::InvalidInput`] if `prefix` or `suffix` contains `/` or `\`.
/// - Otherwise `parent` is missing or unwritable, or `tempfile` ran out of unique names.
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

// `sync_all` on the staged file flushes its contents; the rename's
// directory entry is a separate change to the parent directory, which
// this flushes. Only an unclean shutdown can lose an entry a completed
// `rename` already made visible to every later process.
/// Flushes `dir`'s own metadata, making renames into it durable.
///
/// Call after the `rename` that installs a staged file: the directory
/// entry it created needs a separate flush to reach disk. Skipping this
/// keeps the atomicity guarantee and loses only durability, to a power cut.
///
/// # Platforms
/// Unix only. A no-op on Windows: as durable as NTFS makes it.
///
/// # Errors
/// - [`std::io::Error`] if `dir` could not be opened or flushed. `EINVAL`
///   is tolerated as "no such step"; never errors on Windows.
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
        // Windows returns `Ok` unconditionally, so this only has teeth on unix.
        sync_dir(dir.path()).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn sync_dir_reports_a_directory_that_is_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");

        // Guards the `EINVAL` tolerance from widening into swallowing every error.
        let err = sync_dir(&missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err:?}");
    }
}
