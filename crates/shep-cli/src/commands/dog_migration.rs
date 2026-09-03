//! Moving `[dog.<name>]` out of `shep.toml` and into `dogs.toml`, once.
//!
//! Runs at the top of every daemon boot and does nothing on all but the
//! first. `RawDaemonConfig` keeps its `dog` field so an un-migrated file
//! still parses: deleting it would turn `deny_unknown_fields` into a
//! refused boot for every operator carrying a dog section, with the flock
//! left unsupervised at exactly the moment nobody is watching.

use core::fmt;
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;

use shep_core::config::{DogsConfig, DogsConfigError};
use shep_core::paths::ShepPaths;

use super::shep_toml::{ShepToml, ShepTomlError, create_config_file};

/// Moves every `[dog.<name>]` section into `dogs.toml`, returning the names
/// moved
///
/// Empty when there was nothing to move, which is every boot after the
/// first. Writes `dogs.toml` before striking `shep.toml`, so a crash
/// between the two leaves the sections readable from the old file rather
/// than from neither.
///
/// # Errors
///
/// - [`DogMigrationError::WouldOverwrite`] when a name is present in both
///   files. Two values for one key is a question shep cannot answer, so it
///   refuses and changes nothing.
/// - [`DogMigrationError::SectionsUnreadable`] when `shep.toml` declares a
///   name under `[dog]` that did not come back as a section to move.
///   Refused rather than skipped: `take_dog_sections` has already struck
///   the whole `[dog]` table by then, so moving what came back would drop
///   what did not.
/// - [`DogMigrationError::Parse`] when `dogs.toml` is not valid TOML,
///   [`DogMigrationError::Render`] when the merged sections will not render
///   back, and [`DogMigrationError::Read`], [`DogMigrationError::Write`]
///   and [`DogMigrationError::Toml`] for the underlying I/O.
pub(crate) fn migrate_dog_sections(paths: &ShepPaths) -> Result<Vec<String>, DogMigrationError> {
    let existing_source = match std::fs::read_to_string(&paths.daemon_config) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(DogMigrationError::Read(err)),
    };
    // Cheap check first, and load-bearing rather than an optimisation.
    // `ShepToml::edit` and `try_edit` both stage a temp file and rename it
    // over the original whenever `save` runs, so opening the document at
    // all would give an untouched `shep.toml` a fresh inode, force
    // `CONFIG_FILE_MODE` on it, and replace a symlinked path with a plain
    // file. A boot with nothing to do must not open it.
    if !existing_source.contains("[dog.") && !existing_source.contains("[dog]") {
        return Ok(Vec::new());
    }
    // The substring above answers "might there be sections"; this answers
    // "which ones", by name, and both questions have to be asked. Either
    // spelling can appear in a comment or inside a string, and `[dog]` on
    // its own is a header with nothing under it: a section neither to move
    // nor to lose, and refusing on it would leave an operator with a stray
    // `[dog]` line unable to boot at all.
    //
    // `None` means this parser could not read the source. That is not
    // decided here: it falls through to `ShepToml`, whose own parse error
    // names the file and the line, and the guard inside the closure falls
    // back to the only question it can still answer.
    let declared = declared_dog_names(&existing_source);
    if declared.as_ref().is_some_and(BTreeSet::is_empty) {
        return Ok(Vec::new());
    }

    let already = match std::fs::read_to_string(&paths.dogs_config) {
        Ok(source) => DogsConfig::load(Some(&source)).map_err(DogMigrationError::Parse)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DogsConfig::default(),
        Err(err) => return Err(DogMigrationError::Read(err)),
    };

    // `try_edit`, never `edit`. `edit` calls `save` unconditionally
    // (shep_toml.rs:173), so refusing from inside it would strike the
    // sections from `shep.toml` and only then fail, leaving them in
    // neither file. `try_edit`'s own `Err` skips `save` entirely and
    // leaves `path` exactly as it was found.
    ShepToml::try_edit(&paths.daemon_config, |doc| {
        let incoming = doc.take_dog_sections();
        // `take_dog_sections` removes the WHOLE `[dog]` table and then
        // decides what to hand back, so anything it drops on the way is
        // gone from the document with nothing written in its place: a
        // failed internal reparse hands back nothing at all, and its filter
        // drops a non-table entry while keeping the tables beside it. That
        // second shape is the one a count cannot see, since `[dog] stray =
        // 5` next to `[dog.metrics]` comes back one entry long and not
        // empty. So the guard is over NAMES, comparing what the source
        // declared against what came back, and it refuses before anything
        // is written: `try_edit`'s own `Err` is what makes that a `save`
        // that never happens.
        //
        // With no readable source there are no names to compare, and the
        // fallback is the weaker question: did anything come back at all.
        let missing: Vec<String> = match &declared {
            Some(names) => names
                .iter()
                .filter(|name| !incoming.contains_key(*name))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        if !missing.is_empty() || incoming.is_empty() {
            return Err(DogMigrationError::SectionsUnreadable { names: missing });
        }
        if let Some(name) = incoming.keys().find(|name| already.dog.contains_key(*name)) {
            return Err(DogMigrationError::WouldOverwrite { name: name.clone() });
        }
        let mut moved: Vec<String> = incoming.keys().cloned().collect();
        moved.sort();

        let mut merged = already.dog.clone();
        merged.extend(incoming);
        let rendered = toml::to_string(&merged).map_err(DogMigrationError::Render)?;
        // Written before this closure returns, so `save` strikes the old
        // sections only once the new file already holds them. A crash
        // between the two leaves them readable from `shep.toml`, which is
        // the direction that loses nothing.
        write_dogs_config(&paths.dogs_config, &rendered).map_err(DogMigrationError::Write)?;
        Ok(moved)
    })
}

/// Every name `source` declares under `[dog]`, or `None` when `source` is
/// not TOML this parser can read.
///
/// A second parse of a file [`ShepToml`] is about to parse again, and
/// deliberately so: it is the only record of what was there BEFORE
/// [`ShepToml::take_dog_sections`] struck the table, which is what the
/// guard inside [`migrate_dog_sections`] compares against. Read-only, over
/// a string already in memory, and it never opens the file.
fn declared_dog_names(source: &str) -> Option<BTreeSet<String>> {
    let table = source.parse::<toml::Table>().ok()?;
    match table.get("dog") {
        Some(toml::Value::Table(dog)) => Some(dog.keys().cloned().collect()),
        // No `[dog]` at all, or a `dog` that is not a table: either way it
        // declares no dog, and the substring gate that let us this far was
        // matching a comment or a string.
        _ => Some(BTreeSet::new()),
    }
}

/// Writes `rendered` to `path`: staged in a sibling temp file at
/// `CONFIG_FILE_MODE`, `fsync`ed, then `rename`d over `path`.
///
/// The same three steps, through the same helper, that
/// [`ShepToml::save`] writes `shep.toml` with, and for the same two
/// reasons. **Mode**: these bytes are the ones that used to sit inside
/// `shep.toml` at `0600`, and they are where `docs/dogs.md` tells an
/// operator to paste a Discord or Slack webhook URL, which is a bearer
/// token in a path. A `std::fs::write` here would create the file at the
/// ambient umask, typically `0644`, so the migration itself would be the
/// downgrade. `$SHEP_HOME` being `0700` does not answer it: a `tar`, a
/// `cp -p` or a backup carries a file's own mode somewhere no directory
/// mode follows. **Atomicity**: `std::fs::write` opens `O_TRUNC`, so a
/// crash between the truncate and the write leaves a half-written
/// `dogs.toml` behind; the rename installs the whole file or none of it.
fn write_dogs_config(path: &Path, rendered: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = create_config_file(parent)?;
    tmp.write_all(rendered.as_bytes())?;
    tmp.as_file().sync_all()?;
    // `persist` is `rename(2)`. On failure the `NamedTempFile` comes back
    // inside the error and its `Drop` removes the staging file, so a failed
    // replace leaves nothing behind in `$SHEP_HOME`.
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

/// Why `[dog.<name>]` could not be moved into `dogs.toml`
///
/// Derived `Debug`, deliberately (IR-41): every variant carries a dog name,
/// an I/O error, or a serializer's complaint, and never a section's
/// contents, so there is nothing here to redact. The one wrapped type that
/// could carry the file, [`ShepTomlError`], redacts its own `Debug` for
/// exactly that reason.
#[derive(Debug)]
// `#[non_exhaustive]`: the migration is the one writer of `dogs.toml` and
// will grow refusals as operators meet shapes nobody predicted, so a
// seventh variant should be additive rather than breaking (IR-20).
#[non_exhaustive]
pub(crate) enum DogMigrationError {
    /// `shep.toml` itself could not be read.
    Read(std::io::Error),
    /// `dogs.toml` already exists and is not valid TOML.
    Parse(DogsConfigError),
    /// `name` has a section in both files, so the move would silently pick
    /// one of two values for it.
    WouldOverwrite {
        /// The dog named in both `shep.toml` and `dogs.toml`.
        name: String,
    },
    /// `shep.toml` declares names under `[dog]` that did not come back as
    /// sections to move.
    SectionsUnreadable {
        /// The names that were declared and did not come back, empty when
        /// the source could not be parsed closely enough to name them.
        /// Keys, never values: a dog's name is what an operator typed as a
        /// section header, and it is already what `WouldOverwrite` carries.
        names: Vec<String>,
    },
    /// The merged sections would not render back to TOML.
    Render(toml::ser::Error),
    /// `dogs.toml` could not be written.
    Write(std::io::Error),
    /// Reading, locking or rewriting `shep.toml` failed.
    Toml(ShepTomlError),
}

impl fmt::Display for DogMigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(err) => write!(f, "shep.toml could not be read: {err}"),
            Self::Parse(err) => write!(f, "dogs.toml could not be read: {err}"),
            Self::WouldOverwrite { name } => write!(
                f,
                "dog '{name}' is configured in both shep.toml and dogs.toml; \
                 delete one of the two sections and start the daemon again"
            ),
            Self::SectionsUnreadable { names } if names.is_empty() => write!(
                f,
                "shep.toml has entries under [dog] that are not dog sections; \
                 give each dog a table of its own, or delete them"
            ),
            Self::SectionsUnreadable { names } => write!(
                f,
                "shep.toml has entries under [dog] that are not dog sections ({}); \
                 give each dog a table of its own, or delete them",
                names.join(", ")
            ),
            Self::Render(err) => write!(f, "dogs.toml could not be rendered: {err}"),
            Self::Write(err) => write!(f, "dogs.toml could not be written: {err}"),
            Self::Toml(err) => write!(f, "shep.toml could not be rewritten: {err}"),
        }
    }
}

impl core::error::Error for DogMigrationError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(err) | Self::Write(err) => Some(err),
            Self::Parse(err) => Some(err),
            Self::Render(err) => Some(err),
            Self::Toml(err) => Some(err),
            Self::WouldOverwrite { .. } | Self::SectionsUnreadable { .. } => None,
        }
    }
}

// `ShepToml::try_edit` is generic as `E: From<ShepTomlError>`: this is what
// lets its own setup failures (home dir, lock, parse) reach the caller as
// this module's error without a second wrapping layer.
impl From<ShepTomlError> for DogMigrationError {
    fn from(source: ShepTomlError) -> Self {
        Self::Toml(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(shep_toml: &str) -> (tempfile::TempDir, ShepPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        // `ShepPaths` has no temp-directory constructor and does not grow
        // one for a test's convenience: `resolve` with an empty environment
        // puts the home under `dir` and every other test in this crate
        // builds one the same way.
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.home).expect("home");
        std::fs::write(&paths.daemon_config, shep_toml).expect("write");
        (dir, paths)
    }

    #[test]
    fn sections_move_and_the_original_loses_them() {
        let (_dir, paths) = home_with(
            "# mine\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\nbind = \"127.0.0.1:9615\"\n",
        );

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert_eq!(moved, vec!["metrics".to_string()]);
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            "# mine\n[daemon]\nenabled_dogs = [\"metrics\"]\n"
        );
        let written = std::fs::read_to_string(&paths.dogs_config).expect("read");
        let parsed = DogsConfig::load(Some(&written)).expect("valid");
        assert_eq!(
            parsed.dog["metrics"]["bind"].as_str(),
            Some("127.0.0.1:9615")
        );
    }

    #[test]
    fn a_file_with_no_dog_sections_is_not_rewritten() {
        let before = "[daemon]\nlog_level = \"info\"\n";
        let (_dir, paths) = home_with(before);

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert!(moved.is_empty());
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            before
        );
        assert!(!paths.dogs_config.exists(), "no sections means no file");
    }

    #[test]
    fn a_second_boot_writes_nothing() {
        let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");
        migrate_dog_sections(&paths).expect("first");
        let after_first = std::fs::read_to_string(&paths.dogs_config).expect("read");

        let moved = migrate_dog_sections(&paths).expect("second");

        assert!(moved.is_empty());
        assert_eq!(
            std::fs::read_to_string(&paths.dogs_config).expect("read"),
            after_first
        );
    }

    // `take_dog_sections` answers an empty map on two paths that have
    // already struck the sections from the document: a failed internal
    // reparse, and a value under `[dog]` that is not a table. Both are
    // close to unreachable, and treating either as "nothing to move" would
    // write an empty `dogs.toml` over a `shep.toml` it had just stripped.
    // `metrics = 5` is the reachable half: legal TOML, a non-table under
    // `[dog]`, and dropped by that method's own filter.
    #[test]
    fn entries_under_dog_that_come_back_as_nothing_make_the_migration_refuse() {
        let (_dir, paths) = home_with("[dog]\nmetrics = 5\n");

        let err = migrate_dog_sections(&paths).expect_err("a section that will not travel");

        assert!(matches!(err, DogMigrationError::SectionsUnreadable { .. }));
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            "[dog]\nmetrics = 5\n",
            "a refused migration strikes nothing"
        );
        assert!(!paths.dogs_config.exists(), "nor writes anything");
    }

    // The counterpart to the refusal above, and the reason it is keyed on
    // what the file actually holds rather than on the substring gate: a
    // header with nothing under it has no section to move and no section to
    // lose, so it is neither migrated nor refused. Refusing it would leave
    // an operator with a `[dog]` line unable to boot at all.
    #[test]
    fn an_empty_dog_table_is_left_exactly_as_it_was() {
        let before = "[daemon]\nlog_level = \"info\"\n\n[dog]\n";
        let (_dir, paths) = home_with(before);

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert!(moved.is_empty());
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            before
        );
        assert!(!paths.dogs_config.exists(), "no sections means no file");
    }

    // Partial loss, which a count cannot see and a name can: the whole
    // `[dog]` table is struck, `metrics` comes back, `stray` does not, and
    // a migration that shipped what came back would drop `stray` from disk
    // with no error and no warning. Refused instead, with the name in the
    // message, because nothing else on the machine holds that line.
    #[test]
    fn an_entry_that_comes_back_beside_a_section_but_not_as_one_makes_the_migration_refuse() {
        let (_dir, paths) =
            home_with("[dog]\nstray = 5\n\n[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");

        let err = migrate_dog_sections(&paths).expect_err("stray would be dropped");

        let DogMigrationError::SectionsUnreadable { names } = &err else {
            panic!("expected a refusal naming what would be lost, got {err:?}");
        };
        assert_eq!(names, &vec!["stray".to_string()]);
        assert!(err.to_string().contains("stray"), "{err}");
        assert!(
            std::fs::read_to_string(&paths.daemon_config)
                .expect("read")
                .contains("stray = 5"),
            "a refused migration strikes nothing"
        );
        assert!(!paths.dogs_config.exists(), "nor writes anything");
    }

    /// `dogs.toml` holds the webhook URLs that used to sit inside
    /// `shep.toml` at `0600`, so it is created at `0600` too, at the `open`
    /// rather than by a later `chmod`. Fails if the migration goes back to
    /// `std::fs::write`, which would leave it at the ambient umask.
    #[cfg(unix)]
    #[test]
    fn the_written_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");

        migrate_dog_sections(&paths).expect("migrate");

        let mode = std::fs::metadata(&paths.dogs_config)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:o}");
    }

    // The one case that must never silently merge: an operator who already
    // hand-wrote dogs.toml and still has a stale section in shep.toml. Two
    // values for one key, and picking either would be shep guessing.
    #[test]
    fn an_existing_dogs_file_makes_the_migration_refuse() {
        let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");
        std::fs::write(&paths.dogs_config, "[metrics]\nbind = \"0.0.0.0:9615\"\n").expect("write");

        let err = migrate_dog_sections(&paths).expect_err("both files hold metrics");

        assert!(matches!(err, DogMigrationError::WouldOverwrite { .. }));
        assert!(
            std::fs::read_to_string(&paths.daemon_config)
                .expect("read")
                .contains("[dog.metrics]"),
            "a refused migration strikes nothing"
        );
    }
}
