//! The staging file every `$SHEP_HOME` store is rewritten through.
//!
//! Four stores replace themselves the same way: stage a fresh file beside
//! the real one, `fsync` it, then `rename` it over the top, so a reader
//! sees either the whole old file or the whole new one and never a
//! half-written fragment. `kv.json`, `barks.jsonl`, `overrides.json` and
//! `shep.toml` (with `dogs.toml` riding on the last one) each carried
//! their own copy of the create step, six lines apiece and identical but
//! for a name. This module is the one copy they share.
//!
//! Only the create step lives here. The write, the `sync_all` and the
//! `persist` stay with each store, because each maps a failure of them
//! onto its own error type.

use std::path::Path;

/// Mode a file under `$SHEP_HOME` is created with: owner read/write,
/// nobody else.
///
/// `$SHEP_HOME` is itself `0700`, so for `kv.json`, `barks.jsonl` and
/// `overrides.json` this is a second lock on the same door. For
/// `shep.toml` and `dogs.toml` it is the only lock that counts:
/// `docs/dogs.md` tells an operator to paste a Discord or Slack webhook
/// URL there, and both of those carry a bearer token in the path. Four
/// files reached `0600` by different arguments and the strictest of them
/// is the one that governs the shared value.
///
/// It is also the mode a `tar`, a `cp -p` or a backup of `$SHEP_HOME`
/// carries out with the file, somewhere no directory mode follows it.
///
/// Windows has no equivalent bit, so nothing there applies it and a file
/// inherits the ACL of the directory it lands in. That is the same gap
/// `shep.toml`'s own home-directory creation names in the operator docs.
pub const OWNER_ONLY_FILE_MODE: u32 = 0o600;

/// Creates the staging file a store is rewritten through, in `parent` so
/// the later `rename` stays within one filesystem.
///
/// `prefix` and `suffix` bracket a unique middle `tempfile` picks. The
/// uniqueness is not tidiness: two writers sharing a fixed `<path>.tmp`
/// had one process's `rename` consume the other's staging file, and the
/// loser died with `ENOENT` renaming a path that no longer existed.
///
/// On unix the mode goes to the `open` itself rather than a later `chmod`
/// pass, so there is no window in which a file holding a webhook token
/// sits at whatever the process umask leaves it.
///
/// There is no mode parameter, and that is the function rather than an
/// omission. Every caller writes a file under `$SHEP_HOME` that holds a
/// credential, an `env` value, or a record of what the shepherd told an
/// outside service, so none of them wants anything but
/// [`OWNER_ONLY_FILE_MODE`]. A parameter would buy nothing today and
/// would be the door a later caller walks a `0644` through.
///
/// Returns `tempfile`'s own type, because the caller does the writing, the
/// `sync_all` and the `persist`. That puts a dependency's type in this
/// crate's public API on purpose: the alternative is a newtype whose whole
/// body forwards those three calls.
///
/// # Errors
/// The staging file could not be created in `parent`: the directory is
/// missing or unwritable, or `tempfile` ran out of attempts at a unique
/// name.
pub fn create_staging_file(
    parent: &Path,
    prefix: &str,
    suffix: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix).suffix(suffix);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(OWNER_ONLY_FILE_MODE));
    }

    builder.tempfile_in(parent)
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
}
