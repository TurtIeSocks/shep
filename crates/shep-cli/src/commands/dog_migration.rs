//! Moving `[dog.<name>]` out of `shep.toml` and into `dogs.toml`, once,
//! and the one write path `dogs.toml` has.
//!
//! [`migrate_dog_sections`] runs at the top of every daemon boot and does
//! nothing on all but the first. [`forget_dog_section`] is `shep rehome`'s
//! half. Both hold `dogs.toml`'s own [`ConfigLock`] across the whole
//! read-modify-write, since each rewrites the entire file; when both locks
//! are held, `shep.toml`'s is the outer one. `RawDaemonConfig` keeps its
//! `dog` field so an un-migrated file still parses under
//! `deny_unknown_fields`.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;

use shep_core::config::{DogsConfig, DogsConfigError};
use shep_core::paths::ShepPaths;
use toml_edit::{DocumentMut, Item, TableLike};

use super::shep_toml::{ConfigLock, ShepToml, ShepTomlError, create_config_file};

/// Moves every `[dog.<name>]` section into `dogs.toml`, naming those moved
///
/// # Errors
/// - [`DogMigrationError::WouldOverwrite`] when a name holds values in both
///   files; an empty section is not a value on either side.
/// - [`DogMigrationError::SectionsUnreadable`] when a name declared under
///   `[dog]` did not come back as a section to move.
/// - [`DogMigrationError::Parse`] when `dogs.toml` is not valid TOML.
/// - [`DogMigrationError::Read`], [`DogMigrationError::ReadDogs`],
///   [`DogMigrationError::Lock`], [`DogMigrationError::Write`] and
///   [`DogMigrationError::Toml`] for the underlying I/O.
pub(crate) fn migrate_dog_sections(paths: &ShepPaths) -> Result<Vec<String>, DogMigrationError> {
    let existing_source = match std::fs::read_to_string(&paths.daemon_config) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(DogMigrationError::Read(err)),
    };
    // Not a substring test: `[ dog.metrics ]` is the same section spelled
    // differently. `None` means this parser could not read the source, which
    // `ShepToml` below reports with file and line.
    let declared = existing_source
        .parse::<toml::Table>()
        .ok()
        .map(|table| declared_dog_names(&table));
    // Skipping the open is load-bearing: `ShepToml::save` renames a staged
    // temp file over the original, so opening an untouched `shep.toml` would
    // give it a fresh inode and mode on every boot after the first.
    if declared.as_ref().is_some_and(BTreeSet::is_empty) {
        return Ok(Vec::new());
    }

    // `try_edit`, never `edit`: `edit` calls `save` unconditionally, so
    // refusing from inside it would strike the sections from `shep.toml` and
    // only then fail, leaving them in neither file.
    ShepToml::try_edit(&paths.daemon_config, |doc| {
        // Empty sections dropped here, matching what `declared_dog_names`
        // left out, so the two agree on what there was to move. They still
        // leave `shep.toml`: `take_dog_sections` removes the whole `[dog]`.
        let incoming: BTreeMap<String, Item> = doc
            .take_dog_sections()
            .into_iter()
            .filter(|(_, section)| !section.as_table_like().is_some_and(TableLike::is_empty))
            .collect();
        // `take_dog_sections` removes the whole `[dog]` table before deciding
        // what to hand back, so a count cannot see what it dropped: compare
        // names. With no readable source, fall back to whether any came back.
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
        // `dogs.toml`'s lock, so the read and merge below are one transaction
        // against `forget_dog_section`. Nested inside `shep.toml`'s lock,
        // which `try_edit` holds; that order is the only one taken anywhere.
        let _dogs_lock =
            ConfigLock::acquire(&paths.dogs_config).map_err(DogMigrationError::Lock)?;
        // A live document, not a `toml::Table`: a second migration writes into
        // a `dogs.toml` an operator has been hand-editing, and a
        // `toml::to_string` of a parsed map drops every comment in it.
        let mut merged = read_dogs_document(&paths.dogs_config)?;
        // Over values on both sides: a bare `[metrics]` in `dogs.toml` is not
        // a second value, so the section carrying values wins. A destination
        // `[[metrics]]` is not table-like, so it refuses.
        let collides = |name: &String| {
            merged
                .get(name)
                .is_some_and(|item| !item.as_table_like().is_some_and(TableLike::is_empty))
        };
        if let Some(name) = incoming.keys().find(|name| collides(name)) {
            return Err(DogMigrationError::WouldOverwrite { name: name.clone() });
        }
        let mut moved: Vec<String> = incoming.keys().cloned().collect();
        moved.sort();

        // Appended: a table taken out of `shep.toml` still carries the
        // position it held there. The scan covers every positioned table, not
        // top-level `Item::Table`s alone, since `[bark.sinks]` is neither.
        let mut next = merged
            .iter()
            .filter_map(|(_, item)| last_position(item))
            .max()
            .map_or(0, |last| last + 1);
        for (name, mut section) in incoming {
            renumber_tables(&mut section, &mut next);
            merged.insert(&name, section);
        }
        // Written before this closure returns, so `save` strikes the old
        // sections only once the new file holds them, durable on unix but not
        // against a Windows power cut (`sync_dir` no-ops there).
        write_dogs_config(&paths.dogs_config, &merged.to_string())
            .map_err(DogMigrationError::Write)?;
        Ok(moved)
    })
}

/// Removes `name`'s section from `dogs.toml`, answering whether there was
/// one to remove. A missing file, or no section under `name`, is `Ok(false)`.
///
/// Call only after `shep.toml` is rewritten: the two writes are not one
/// transaction, and the other order can drop a section while the dog stays
/// enabled, guaranteed on unix only since `sync_dir` no-ops on Windows.
///
/// # Errors
/// - [`DogMigrationError::ReadDogs`], [`DogMigrationError::Parse`]:
///   `dogs.toml` cannot be read, or is not valid TOML.
/// - [`DogMigrationError::Lock`], [`DogMigrationError::Write`]: the lock
///   could not be taken, or the staged write failed.
pub(crate) fn forget_dog_section(path: &Path, name: &str) -> Result<bool, DogMigrationError> {
    // Held across the read, the removal and the rename: this is a whole-file
    // read-modify-write, so two unserialised `shep rehome` calls lose one of
    // the two removals. No other lock is taken here, so it cannot deadlock.
    let _lock = ConfigLock::acquire(path).map_err(DogMigrationError::Lock)?;
    let mut doc = read_dogs_document(path)?;
    if doc.remove(name).is_none() {
        return Ok(false);
    }
    write_dogs_config(path, &doc.to_string()).map_err(DogMigrationError::Write)?;
    Ok(true)
}

/// Reads `path` as an editable document, treating a missing file as an empty
/// one. Callers hold `path`'s [`ConfigLock`]: a read outside it is the first
/// half of a lost update.
///
/// A [`DocumentMut`] rather than a [`DogsConfig`], since both writers rewrite
/// the whole file and an operator hand-edits it: comments and inline tables
/// survive a `toml_edit` round trip and not a `toml::to_string` of a parsed
/// map. [`DogsConfig::load`] still gates it, being the stricter parse.
///
/// # Errors
/// - [`DogMigrationError::ReadDogs`] when the file exists and could not be
///   read, and [`DogMigrationError::Parse`] when it is not valid TOML.
fn read_dogs_document(path: &Path) -> Result<DocumentMut, DogMigrationError> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DocumentMut::new());
        }
        Err(err) => return Err(DogMigrationError::ReadDogs(err)),
    };
    DogsConfig::load(Some(&source)).map_err(DogMigrationError::Parse)?;
    source.parse::<DocumentMut>().map_err(|_| {
        // Unreachable: `toml` is a thin layer over `toml_edit`, so a source
        // the gate accepted parses here too. Reported rather than panicked
        // on, since this runs at the top of every daemon boot.
        DogMigrationError::ReadDogs(std::io::Error::other(
            "dogs.toml parses as TOML values and not as an editable document",
        ))
    })
}

/// Every name `table` declares a value for under `[dog]`.
///
/// The only record of what was there before
/// [`ShepToml::take_dog_sections`] struck the table, which is what the guard
/// inside [`migrate_dog_sections`] compares against. Read-only, over a string
/// already in memory; empty also means do not open the file at all.
///
/// A name holding nothing is left out. An empty `[dog.metrics]` is what
/// `shep enable` scaffolds on older binaries, and counting it as declared
/// refuses a boot against a `dogs.toml` that already holds `metrics`.
fn declared_dog_names(table: &toml::Table) -> BTreeSet<String> {
    match table.get("dog") {
        Some(toml::Value::Table(dog)) => dog
            .iter()
            .filter(|(_, value)| !declares_nothing(value))
            .map(|(name, _)| name.clone())
            .collect(),
        // No `[dog]` at all, or a `dog` that is not a table: either way
        // nothing is declared and there is no reason to open the document.
        _ => BTreeSet::new(),
    }
}

/// Whether `value` holds nothing an operator could lose by not moving it.
///
/// An empty table, an empty array, and an array of tables that are all empty.
/// The last is why this recurses rather than testing `Table::is_empty`: an
/// empty `[[dog.metrics]]` is a declared name that
/// [`ShepToml::take_dog_sections`] hands back nothing for.
///
/// A `[[dog.metrics]]` that carries values is not this, and still refuses:
/// there is no one section for it to become.
fn declares_nothing(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.is_empty(),
        toml::Value::Array(items) => items.iter().all(declares_nothing),
        _ => false,
    }
}

/// The highest render position any table inside `item` holds, or `None`
/// when it holds no positioned table at all.
///
/// [`renumber_tables`]'s read-only twin, walking the same three shapes:
/// `toml_edit` positions every table individually, so a nested `[bark.sinks]`
/// and an `[[bark.rules]]` entry each carry one and neither is an
/// `Item::Table` at the top level. An implicit parent has no position of its
/// own, hence the descent into children.
fn last_position(item: &Item) -> Option<usize> {
    match item {
        Item::Table(table) => table
            .position()
            .into_iter()
            .chain(table.iter().filter_map(|(_, child)| last_position(child)))
            .max(),
        Item::ArrayOfTables(array) => array
            .iter()
            .flat_map(|table| {
                table
                    .position()
                    .into_iter()
                    .chain(table.iter().filter_map(|(_, child)| last_position(child)))
            })
            .max(),
        // Neither holds a positioned table: an inline `metrics = { .. }` is a
        // key/value pair and renders above the tables, with the other pairs.
        Item::Value(_) | Item::None => None,
    }
}

/// Walks `item` depth-first and gives every table it holds the next document
/// position, so a section moved out of `shep.toml` renders after everything
/// already in `dogs.toml`.
///
/// Over sub-tables too, because `toml_edit` renders every table by its own
/// position and not by its parent's: renumbering only the top-level `bark`
/// would leave `[bark.sinks]` back at `shep.toml`'s index. [`Item::Value`]
/// and [`Item::None`] hold no positioned table and are left alone.
fn renumber_tables(item: &mut Item, next: &mut usize) {
    match item {
        Item::Table(table) => {
            table.set_position(*next);
            *next += 1;
            for (_, child) in table.iter_mut() {
                renumber_tables(child, next);
            }
        }
        Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                table.set_position(*next);
                *next += 1;
                for (_, child) in table.iter_mut() {
                    renumber_tables(child, next);
                }
            }
        }
        Item::Value(_) | Item::None => {}
    }
}

/// Writes `rendered` to `path`: staged in a sibling temp file at
/// `OWNER_ONLY_FILE_MODE`, `fsync`ed, then `rename`d over `path`.
///
/// The mode matters: this file holds dog webhook URLs, and a plain
/// `std::fs::write` would create it at the ambient umask instead. The
/// rename avoids the half-written file a plain write's `O_TRUNC` leaves on
/// a crash. The directory flush publishes the rename and is a no-op on
/// Windows.
fn write_dogs_config(path: &Path, rendered: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = create_config_file(parent)?;
    tmp.write_all(rendered.as_bytes())?;
    tmp.as_file().sync_all()?;
    // `persist` is `rename(2)`. On failure the `NamedTempFile` comes back in
    // the error and its `Drop` removes the staging file.
    tmp.persist(path).map_err(|err| err.error)?;

    // The `sync_all` above made the contents durable; this makes the rename
    // that published them durable.
    shep_core::atomic_file::sync_dir(parent)?;
    Ok(())
}

/// Why `[dog.<name>]` could not be moved into `dogs.toml`
///
/// Derived `Debug`: every variant carries a dog name, an I/O error or a
/// serializer's complaint, never a section's contents. That holds only
/// because [`ShepTomlError`] and [`DogsConfigError`] redact their own
/// `Debug`, so a new variant wrapping a parse error needs the same check.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum DogMigrationError {
    /// `shep.toml` itself could not be read.
    Read(std::io::Error),
    /// `dogs.toml` exists and could not be read.
    ///
    /// Its own variant rather than [`Self::Read`]: an operator's next move
    /// depends on which of the two files it was.
    ReadDogs(std::io::Error),
    /// `dogs.toml` already exists and is not valid TOML.
    Parse(DogsConfigError),
    /// `name` has a section carrying values in both files, so the move
    /// would silently pick one of the two.
    ///
    /// An empty section never raises this on either side: in the source it is
    /// skipped, in `dogs.toml` it is the header the moved section lands on.
    WouldOverwrite {
        /// The dog named in both `shep.toml` and `dogs.toml`.
        name: String,
    },
    /// `shep.toml` declares names under `[dog]` that did not come back as
    /// sections to move.
    SectionsUnreadable {
        /// The names that were declared and did not come back, empty when
        /// the source could not be parsed closely enough to name them.
        names: Vec<String>,
    },
    /// `dogs.toml` could not be written.
    Write(std::io::Error),
    /// `dogs.toml`'s sibling lock file could not be created or locked.
    Lock(std::io::Error),
    /// Reading, locking or rewriting `shep.toml` failed.
    Toml(ShepTomlError),
}

impl fmt::Display for DogMigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(err) => write!(f, "shep.toml could not be read: {err}"),
            Self::ReadDogs(err) => write!(f, "dogs.toml could not be read: {err}"),
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
            Self::Write(err) => write!(f, "dogs.toml could not be written: {err}"),
            Self::Lock(err) => write!(f, "dogs.toml could not be locked: {err}"),
            Self::Toml(err) => write!(f, "shep.toml could not be rewritten: {err}"),
        }
    }
}

impl core::error::Error for DogMigrationError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(err) | Self::ReadDogs(err) | Self::Write(err) | Self::Lock(err) => Some(err),
            Self::Parse(err) => Some(err),
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

    /// How many times [`two_rehomes_at_once_both_land`] re-runs its race.
    ///
    /// A lost update needs both threads to read before either renames, and
    /// on this write path (stage, `fsync`, rename) that window is wide, so
    /// one round is usually enough. Twenty costs a few milliseconds and
    /// makes the failure a certainty rather than a likelihood, which is
    /// what a test guarding a race has to be to be worth having.
    const ROUNDS_OF_CONTENTION: u32 = 20;

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

    // `metrics = 5` is legal TOML: a non-table value under `[dog]`, dropped
    // by `declared_dog_names`'s own filter.
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

    // A `[dog]` header with nothing under it has no section to move or
    // lose, so it is left alone rather than refused.
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

    // `stray` comes back as an entry, not a section, so it would be
    // silently dropped rather than moved; refused instead, naming it.
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

    /// `dogs.toml` is created at `0600`, at the `open` rather than by a
    /// later `chmod`. Fails if the migration goes back to `std::fs::write`,
    /// which would leave it at the ambient umask.
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

    /// Two threads sharing a barrier, not two processes: `flock(2)` is
    /// per-open-file-description, so two threads contend the same as two
    /// `shep` invocations. Repeated because a race that serialises itself
    /// once proves nothing.
    #[test]
    fn two_rehomes_at_once_both_land() {
        use std::sync::{Arc, Barrier};

        for round in 0..ROUNDS_OF_CONTENTION {
            let (_dir, paths) = home_with("");
            std::fs::write(
                &paths.dogs_config,
                "[otel]\nendpoint = \"127.0.0.1:4317\"\n\n[watchdog]\nevery = \"30s\"\n",
            )
            .expect("write");

            let gate = Arc::new(Barrier::new(2));
            let removals: Vec<_> = ["otel", "watchdog"]
                .into_iter()
                .map(|name| {
                    let gate = Arc::clone(&gate);
                    let path = paths.dogs_config.clone();
                    std::thread::spawn(move || {
                        gate.wait();
                        forget_dog_section(&path, name).expect("rehome")
                    })
                })
                .collect();
            for removal in removals {
                assert!(removal.join().expect("thread"), "each found its own dog");
            }

            let left = read_dogs_document(&paths.dogs_config).expect("read");
            assert!(
                left.is_empty(),
                "round {round}: both removals must survive, {:?} came back",
                left.iter().map(|(name, _)| name).collect::<Vec<_>>()
            );
        }
    }

    /// A boot migrating a section in while `shep rehome` takes a different
    /// one out. Whoever renames second wins the whole file, so without the
    /// shared lock one write silently drops the other.
    ///
    /// The migration holds `shep.toml`'s lock while it takes `dogs.toml`'s;
    /// `forget_dog_section` takes only `dogs.toml`'s, so there is no second
    /// ordering for the two to deadlock across.
    #[test]
    fn a_boot_migration_and_a_rehome_at_once_both_land() {
        use std::sync::{Arc, Barrier};

        for round in 0..ROUNDS_OF_CONTENTION {
            let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");
            std::fs::write(
                &paths.dogs_config,
                "[otel]\nendpoint = \"127.0.0.1:4317\"\n",
            )
            .expect("write");

            let gate = Arc::new(Barrier::new(2));
            let migrating = {
                let gate = Arc::clone(&gate);
                let paths = paths.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    migrate_dog_sections(&paths).expect("migrate")
                })
            };
            let rehoming = {
                let gate = Arc::clone(&gate);
                let path = paths.dogs_config.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    forget_dog_section(&path, "otel").expect("rehome")
                })
            };
            let moved = migrating.join().expect("thread");
            // Whether the rehome found a section to strike is the one
            // thing the ordering genuinely decides, so it is not asserted
            // on; the file left behind is the same either way.
            let _removed = rehoming.join().expect("thread");

            let left = read_dogs_document(&paths.dogs_config).expect("read");
            // Both orderings must agree on the file left behind, whichever
            // ran first.
            assert_eq!(moved, vec!["metrics".to_string()], "round {round}");
            assert_eq!(
                left.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                vec!["metrics"],
                "round {round}: the migrated section stays and the rehomed one goes"
            );
        }
    }

    /// Exact string: a reparse agrees on the values whether the comments,
    /// inline table and blank lines survived or not, so only the string
    /// tells a correct rewrite from a wrecked one.
    #[test]
    fn forgetting_a_dog_keeps_every_other_comment_and_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dogs.toml");
        std::fs::write(
            &path,
            "# hand-written, do not clobber\n[otel]\nendpoint = \"127.0.0.1:4317\"\nheaders = { auth = \"x\" }\n\n[metrics]\nbind = \"127.0.0.1:9615\"\n",
        )
        .expect("write");

        assert!(forget_dog_section(&path, "metrics").expect("forget"));

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "# hand-written, do not clobber\n[otel]\nendpoint = \"127.0.0.1:4317\"\nheaders = { auth = \"x\" }\n"
        );
    }

    /// Exact string: it pins both that the destination keeps its own
    /// comment and inline table, and that the moved section arrives
    /// carrying its own `shep.toml` comments, which a `toml::to_string` of
    /// a parsed map would drop.
    ///
    /// The moved section lands after everything already there:
    /// `renumber_tables` gives it a fresh position instead of the one it
    /// held in `shep.toml`.
    #[test]
    fn migrating_into_a_hand_edited_file_keeps_both_files_comments() {
        let (_dir, paths) = home_with(
            "[daemon]\nlog_level = \"info\"\n\n# scrape target\n[dog.metrics]\nbind = \"127.0.0.1:9615\" # loopback only\n",
        );
        std::fs::write(
            &paths.dogs_config,
            "# hand-written, do not clobber\n[otel]\nendpoint = \"127.0.0.1:4317\"\nheaders = { auth = \"x\" }\n",
        )
        .expect("write");

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert_eq!(moved, vec!["metrics".to_string()]);
        assert_eq!(
            std::fs::read_to_string(&paths.dogs_config).expect("read"),
            "# hand-written, do not clobber\n[otel]\nendpoint = \"127.0.0.1:4317\"\nheaders = { auth = \"x\" }\n\n# scrape target\n[metrics]\nbind = \"127.0.0.1:9615\" # loopback only\n"
        );
    }

    /// Fails if a moved dog with sub-tables and an array of tables is
    /// split across the destination's own sections.
    ///
    /// `toml_edit` renders every table by its own position, not its
    /// parent's, so `renumber_tables` must descend into `[bark.sinks]` and
    /// `[[bark.rules]]` too, or they stay at the indices they held in
    /// `shep.toml` and interleave with `[a]`, `[b]` and `[c]` here.
    #[test]
    fn a_moved_dog_with_sub_tables_lands_in_one_piece_at_the_end() {
        let (_dir, paths) = home_with(
            "[dog.bark.sinks]\noncall = { url = \"u\" }\n\n[[dog.bark.rules]]\non = \"gave_up\"\n",
        );
        std::fs::write(
            &paths.dogs_config,
            "[a]\nk = 1\n\n[b]\nk = 2\n\n[c]\nk = 3\n",
        )
        .expect("write");

        migrate_dog_sections(&paths).expect("migrate");

        let written = std::fs::read_to_string(&paths.dogs_config).expect("read");
        let headers: Vec<&str> = written
            .lines()
            .filter(|line| line.starts_with('['))
            .collect();
        assert_eq!(
            headers,
            vec!["[a]", "[b]", "[c]", "[bark.sinks]", "[[bark.rules]]"],
            "the migration appends; it does not interleave: {written}"
        );
    }

    /// Fails if a destination with nested or array-of-tables sections puts
    /// the moved section at the front instead of the end.
    ///
    /// `last_position` must count `[bark.sinks]` (nested under an implicit
    /// `bark`) and `[[bark.rules]]` (an array of tables), not just
    /// top-level `Item::Table`s, or it answers 0 and the moved `[metrics]`
    /// renders first with no blank line before `[bark.sinks]`. Two
    /// `[[bark.rules]]` entries make the array-arm miss visible; with one,
    /// a scan of only the nested table would tie with it by chance.
    ///
    /// Exact string: a reparse agrees with a wrong rendering too, so only
    /// the string tells them apart.
    #[test]
    fn a_moved_dog_lands_after_a_destination_of_nested_and_array_tables() {
        let (_dir, paths) = home_with(
            "[daemon]\nlog_level = \"info\"\n\n[dog.metrics]\nbind = \"127.0.0.1:19615\"\n",
        );
        let before = "[bark.sinks]\noncall = { url = \"u\" }\n\n[[bark.rules]]\non = \"gave_up\"\n\n[[bark.rules]]\non = \"restarted\"\n";
        std::fs::write(&paths.dogs_config, before).expect("write");

        migrate_dog_sections(&paths).expect("migrate");

        assert_eq!(
            std::fs::read_to_string(&paths.dogs_config).expect("read"),
            format!("{before}\n[metrics]\nbind = \"127.0.0.1:19615\"\n")
        );
    }

    /// Fails if an empty `[dog.<name>]` can stop a shepherd booting.
    ///
    /// An older `shep enable` scaffolds an empty `[dog.metrics]`; that must
    /// not collide with a `dogs.toml` that already configures the same
    /// dog, since a mixed-version host is ordinary.
    ///
    /// Nothing is written on either side: the configured `dogs.toml` wins
    /// untouched, and `shep.toml` keeps its stray header.
    #[test]
    fn an_empty_section_colliding_with_a_configured_one_is_not_a_refusal() {
        let before = "[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\n";
        let (_dir, paths) = home_with(before);
        let dogs = "[metrics]\nbind = \"127.0.0.1:19616\"\n";
        std::fs::write(&paths.dogs_config, dogs).expect("write");

        let moved = migrate_dog_sections(&paths).expect("an empty table is not a second value");

        assert!(moved.is_empty());
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            before
        );
        assert_eq!(
            std::fs::read_to_string(&paths.dogs_config).expect("read"),
            dogs
        );
    }

    /// `[[dog.metrics]]` with nothing under it: `take_dog_sections` hands
    /// back no array for it, so the name guard must not call this
    /// `SectionsUnreadable`, or a header holding nothing refuses the boot.
    #[test]
    fn an_empty_array_of_tables_under_dog_is_skipped_rather_than_refused() {
        let before = "[[dog.metrics]]\n";
        let (_dir, paths) = home_with(before);

        let moved = migrate_dog_sections(&paths).expect("nothing declared, nothing to lose");

        assert!(moved.is_empty());
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            before
        );
    }

    /// An array of tables that carries values has no one section to
    /// become, so the name guard refuses rather than dropping it silently.
    #[test]
    fn an_array_of_tables_with_values_still_makes_the_migration_refuse() {
        let before = "[[dog.metrics]]\nbind = \"127.0.0.1:9615\"\n";
        let (_dir, paths) = home_with(before);

        let err = migrate_dog_sections(&paths).expect_err("values with nowhere to go");

        assert!(matches!(
            err,
            DogMigrationError::SectionsUnreadable { ref names } if names == &["metrics".to_string()]
        ));
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            before
        );
    }

    /// An empty section beside a real one still lets the real one move, and
    /// the empty header goes with it: `take_dog_sections` strikes the whole
    /// `[dog]` table, and an empty scaffold is not something to preserve.
    #[test]
    fn an_empty_section_beside_a_real_one_does_not_block_it() {
        let (_dir, paths) =
            home_with("[dog.metrics]\n\n[dog.bark.sinks]\noncall = { url = \"u\" }\n");

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert_eq!(moved, vec!["bark".to_string()]);
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            ""
        );
        let written = std::fs::read_to_string(&paths.dogs_config).expect("read");
        assert!(
            !written.contains("metrics"),
            "an empty scaffold is not carried across: {written}"
        );
    }

    /// Fails if a bare header in `dogs.toml` can refuse a section that
    /// carries values.
    ///
    /// `WouldOverwrite` exists because two values for one key is a
    /// question shep cannot answer; a bare `[metrics]` holds no value, so
    /// there is no second answer to guess between.
    #[test]
    fn a_bare_header_in_dogs_toml_does_not_refuse_a_configured_section() {
        let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:19615\"\n");
        std::fs::write(&paths.dogs_config, "[metrics]\n").expect("write");

        let moved = migrate_dog_sections(&paths).expect("an empty destination is not a value");

        assert_eq!(moved, vec!["metrics".to_string()]);
        let written = std::fs::read_to_string(&paths.dogs_config).expect("read");
        let parsed = DogsConfig::load(Some(&written)).expect("valid");
        assert_eq!(
            parsed.dog["metrics"]["bind"].as_str(),
            Some("127.0.0.1:19615"),
            "the section that had values is the one left standing: {written}"
        );
    }

    /// Fails if a header spelled with spaces inside the brackets strands
    /// its section in `shep.toml`.
    ///
    /// `[ dog.metrics ]` is ordinary TOML and means exactly what
    /// `[dog.metrics]` means, so it must parse the same as the tight form
    /// rather than by a `"[dog."` substring match.
    #[test]
    fn a_header_spelled_with_whitespace_still_migrates() {
        let (_dir, paths) = home_with(
            "[daemon]\nenabled_dogs = [\"metrics\"]\n\n[ dog.metrics ]\nbind = \"127.0.0.1:19615\"\n",
        );

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert_eq!(moved, vec!["metrics".to_string()]);
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            "[daemon]\nenabled_dogs = [\"metrics\"]\n",
            "the section leaves shep.toml"
        );
        let written = std::fs::read_to_string(&paths.dogs_config).expect("read");
        let parsed = DogsConfig::load(Some(&written)).expect("valid");
        assert_eq!(
            parsed.dog["metrics"]["bind"].as_str(),
            Some("127.0.0.1:19615")
        );
    }

    /// Fails if a boot with nothing to migrate opens `shep.toml` for
    /// editing anyway.
    ///
    /// The inode is the assertion, not the bytes, which are identical
    /// either way. `try_edit` stages a temp file and renames it over the
    /// original whenever `save` runs, so opening the document on an idle
    /// boot would hand an untouched `shep.toml` a fresh inode, force
    /// `OWNER_ONLY_FILE_MODE` onto it, and turn a symlinked path into a
    /// plain file.
    #[cfg(unix)]
    #[test]
    fn a_boot_with_nothing_to_move_never_reopens_the_file() {
        use std::os::unix::fs::MetadataExt as _;

        let (_dir, paths) = home_with("[daemon]\nlog_level = \"info\"\n\n[dog]\n");
        let before = std::fs::metadata(&paths.daemon_config).expect("stat").ino();

        assert!(migrate_dog_sections(&paths).expect("migrate").is_empty());

        let after = std::fs::metadata(&paths.daemon_config).expect("stat").ino();
        assert_eq!(before, after, "an idle boot must not rewrite shep.toml");
    }

    // Two values for one key: picking either would be shep guessing, so
    // this refuses instead of merging.
    #[test]
    fn an_existing_dogs_file_makes_the_migration_refuse() {
        let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");
        std::fs::write(&paths.dogs_config, "[metrics]\nbind = \"0.0.0.0:9615\"\n").expect("write");

        let err = migrate_dog_sections(&paths).expect_err("both files hold metrics");

        assert!(matches!(
            err,
            DogMigrationError::WouldOverwrite { ref name } if name == "metrics"
        ));
        assert!(
            std::fs::read_to_string(&paths.daemon_config)
                .expect("read")
                .contains("[dog.metrics]"),
            "a refused migration strikes nothing"
        );
    }
}
