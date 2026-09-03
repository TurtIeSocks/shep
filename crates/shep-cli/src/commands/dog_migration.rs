//! Moving `[dog.<name>]` out of `shep.toml` and into `dogs.toml`, once,
//! and the one write path `dogs.toml` has.
//!
//! [`migrate_dog_sections`] runs at the top of every daemon boot and does
//! nothing on all but the first. [`forget_dog_section`] is the other
//! writer, `shep rehome`'s half of forgetting a dog, and it lives here
//! rather than beside that verb because both go through
//! [`write_dogs_config`]: that file holds webhook credentials at `0600`,
//! and one staged-temp-`fsync`-`rename` helper with the reasoning attached
//! is what keeps a second writer from reaching for `std::fs::write`.
//!
//! **Both hold `dogs.toml`'s own [`ConfigLock`] across the whole
//! read-modify-write**, the same discipline [`ShepToml::edit`] applies to
//! `shep.toml` and for the same reason: each rewrites the entire file, so
//! two unserialised writers lose one of each other's edits rather than
//! colliding visibly. Two `shep rehome` calls for two different dogs, out
//! of the sort of provisioning script `docs/dogs.md` tells operators is
//! safe, is the ordinary way to reach that. When both locks are held,
//! `shep.toml`'s is always the outer one. `RawDaemonConfig` keeps its `dog` field so an un-migrated file
//! still parses: deleting it would turn `deny_unknown_fields` into a
//! refused boot for every operator carrying a dog section, with the flock
//! left unsupervised at exactly the moment nobody is watching.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;

use shep_core::config::{DogsConfig, DogsConfigError};
use shep_core::paths::ShepPaths;
use toml_edit::{DocumentMut, Item, Table, TableLike};

use super::shep_toml::{ConfigLock, ShepToml, ShepTomlError, create_config_file};

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
/// - [`DogMigrationError::WouldOverwrite`] when a name holding VALUES is
///   present in both files. Two values for one key is a question shep
///   cannot answer, so it refuses and changes nothing. An empty
///   `[dog.<name>]` is not one of the two -- see [`declared_dog_names`].
/// - [`DogMigrationError::SectionsUnreadable`] when `shep.toml` declares a
///   name under `[dog]` that did not come back as a section to move.
///   Refused rather than skipped: `take_dog_sections` has already struck
///   the whole `[dog]` table by then, so moving what came back would drop
///   what did not.
/// - [`DogMigrationError::Parse`] when `dogs.toml` is not valid TOML, and
///   [`DogMigrationError::Read`], [`DogMigrationError::ReadDogs`],
///   [`DogMigrationError::Lock`], [`DogMigrationError::Write`] and
///   [`DogMigrationError::Toml`] for the underlying I/O. There is no
///   rendering failure to report: a [`DocumentMut`] always renders.
pub(crate) fn migrate_dog_sections(paths: &ShepPaths) -> Result<Vec<String>, DogMigrationError> {
    let existing_source = match std::fs::read_to_string(&paths.daemon_config) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(DogMigrationError::Read(err)),
    };
    // One parse of the string already in memory, answering both questions
    // this function has to ask before it may open the document: is there
    // anything to move, and which names. A read-only `toml::Table` parse
    // opens no file and costs nothing next to the boot around it.
    //
    // Not a substring test, which is what this was. `[dog.` and `[dog]`
    // are two of the spellings a `[dog]` header has and TOML allows the
    // others: `[ dog.metrics ]` and `[dog . metrics]` are the same section
    // and matched neither, so the migration reported nothing to do, the
    // section stayed in a file nothing reads any more, and the dog came up
    // on compiled defaults with no warning on any surface. For bark that
    // is every sink gone. A parse knows the spellings; a substring cannot.
    //
    // Skipping the open when there is nothing to move is load-bearing
    // rather than an optimisation. `ShepToml::edit` and `try_edit` both
    // stage a temp file and rename it over the original whenever `save`
    // runs, so opening the document at all would give an untouched
    // `shep.toml` a fresh inode, force `CONFIG_FILE_MODE` on it, and
    // replace a symlinked path with a plain file. Every boot after the
    // first reaches this line, so that cost would land on everyone.
    //
    // `[dog]` on its own is a header with nothing under it: a section
    // neither to move nor to lose, and refusing on it would leave an
    // operator with a stray `[dog]` line unable to boot at all. A
    // `[dog.metrics]` holding no values is the same shape one level down
    // and is skipped for the same reason -- see [`declared_dog_names`].
    //
    // `None` means this parser could not read the source at all. That is
    // not decided here: the document is opened and it falls through to
    // `ShepToml`, whose own parse error names the file and the line, and
    // the guard inside the closure falls back to the only question it can
    // still answer.
    let declared = existing_source
        .parse::<toml::Table>()
        .ok()
        .map(|table| declared_dog_names(&table));
    if declared.as_ref().is_some_and(BTreeSet::is_empty) {
        return Ok(Vec::new());
    }

    // `try_edit`, never `edit`. `edit` calls `save` unconditionally
    // (shep_toml.rs:173), so refusing from inside it would strike the
    // sections from `shep.toml` and only then fail, leaving them in
    // neither file. `try_edit`'s own `Err` skips `save` entirely and
    // leaves `path` exactly as it was found.
    ShepToml::try_edit(&paths.daemon_config, |doc| {
        // Empty sections dropped here, matching what [`declared_dog_names`]
        // already left out of `declared`, so the two agree on what there
        // was to move. An empty one is not moved and not counted as
        // missing: there is nothing in it to arrive at the other end. It
        // does still go, because `take_dog_sections` removes the whole
        // `[dog]` table, and striking an empty header an older `shep
        // enable` scaffolded is the right outcome anyway. Only the sections
        // beside it are what made this boot open the file at all.
        let incoming: BTreeMap<String, Item> = doc
            .take_dog_sections()
            .into_iter()
            .filter(|(_, section)| !section.as_table_like().is_some_and(TableLike::is_empty))
            .collect();
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
        // `dogs.toml`'s own lock, taken here rather than at the top of the
        // function so that this whole read-and-merge is one transaction
        // against the other writer of that file,
        // [`forget_dog_section`]. Nested INSIDE `shep.toml`'s lock, which
        // `try_edit` already holds: that is the ordering both call sites
        // keep, and the reason a deadlock is not reachable. This is the
        // only place either lock is nested in the other at all, and it
        // never takes them the other way round.
        let _dogs_lock =
            ConfigLock::acquire(&paths.dogs_config).map_err(DogMigrationError::Lock)?;
        // The destination as a live document, not a `toml::Table`: a
        // second migration writes into a `dogs.toml` an operator has been
        // hand-editing since the first one, and a `toml::to_string` of a
        // parsed map hands that file back with every comment gone (spec
        // decision 1 calls this file hand-editable, and decision 9 promises
        // `toml_edit` for exactly this reason).
        let mut merged = read_dogs_document(&paths.dogs_config)?;
        if let Some(name) = incoming.keys().find(|name| merged.contains_key(name)) {
            return Err(DogMigrationError::WouldOverwrite { name: name.clone() });
        }
        let mut moved: Vec<String> = incoming.keys().cloned().collect();
        moved.sort();

        // Appended, never wedged into the middle. A table taken out of
        // `shep.toml` still carries the position it held THERE, and
        // `toml_edit` renders tables in position order, so a
        // `[dog.metrics]` that was the first table in `shep.toml` lands
        // between the first and second tables of an operator's `dogs.toml`
        // -- inside the blank line that separated them. Renumbering from
        // one past the destination's own last table is what makes a
        // migration read like an append.
        let mut next = merged
            .iter()
            .filter_map(|(_, item)| item.as_table().and_then(Table::position))
            .max()
            .map_or(0, |last| last + 1);
        for (name, mut section) in incoming {
            renumber_tables(&mut section, &mut next);
            merged.insert(&name, section);
        }
        // Written before this closure returns, so `save` strikes the old
        // sections only once the new file already holds them. A crash
        // between the two leaves them readable from `shep.toml`, which is
        // the direction that loses nothing.
        write_dogs_config(&paths.dogs_config, &merged.to_string())
            .map_err(DogMigrationError::Write)?;
        Ok(moved)
    })
}

/// Removes `name`'s section from `dogs.toml`, answering whether there was
/// one to remove.
///
/// `shep rehome <name>`'s half of the cross-file forget: [`ShepToml`] owns
/// `shep.toml` alone, so `rehome_dog` strikes the registration and this
/// strikes the configuration. A missing `dogs.toml`, or one with no
/// section under `name`, is `Ok(false)` and writes nothing: rehoming a dog
/// that was never configured is an ordinary thing to do, not a fault.
///
/// Called after `shep.toml` has already been rewritten, deliberately. The
/// two writes are not one transaction, and of the two ways a crash between
/// them can land, this is the harmless one: a section nothing reads, since
/// the name is out of `enabled_dogs` and `adopted_dogs` by then. The other
/// order loses an operator's webhook URLs while the dog is still enabled
/// and still running, and says nothing about it.
///
/// # Errors
///
/// - [`DogMigrationError::ReadDogs`] when `dogs.toml` exists and cannot be
///   read, and [`DogMigrationError::Parse`] when it is not valid TOML.
///   Refused rather than replaced: a file this verb cannot understand is
///   not one it may overwrite.
/// - [`DogMigrationError::Lock`] when this file's sibling lock could not be
///   taken, and [`DogMigrationError::Write`] when the staged replacement
///   fails.
///
/// The error type is shared with [`migrate_dog_sections`] rather than
/// split: every variant either half can produce says which of the two
/// files failed and how, which is what an operator needs from both.
pub(crate) fn forget_dog_section(path: &Path, name: &str) -> Result<bool, DogMigrationError> {
    // Held across the read, the removal and the rename, and dropped on the
    // way out of this function. Without it, two `shep rehome` calls for two
    // different dogs, backgrounded together out of a provisioning script,
    // both read the file before either writes and the second rename puts
    // one of the two removals back: a whole-file read-modify-write with
    // nothing serialising it. `docs/dogs.md` promises a provisioning script
    // exactly this guarantee. This function takes no other lock, so it can
    // never be the half of a deadlock that holds `dogs.toml` and waits on
    // `shep.toml`.
    let _lock = ConfigLock::acquire(path).map_err(DogMigrationError::Lock)?;
    let mut doc = read_dogs_document(path)?;
    if doc.remove(name).is_none() {
        return Ok(false);
    }
    write_dogs_config(path, &doc.to_string()).map_err(DogMigrationError::Write)?;
    Ok(true)
}

/// Reads `path` as an editable document, treating a missing file as an
/// empty one.
///
/// Both writers of `dogs.toml` read it this way, and both call this with
/// that file's [`ConfigLock`] already held: a read outside the lock is the
/// first half of a lost update, not a cheap shortcut.
///
/// A [`DocumentMut`] rather than a [`DogsConfig`], because both writers
/// rewrite the whole file and an operator is invited to hand-edit it. Every
/// comment, key order and inline table survives a `toml_edit` round trip
/// and none of them survives a `toml::to_string` of a parsed map, which is
/// what one `shep rehome` used to do to a commented file.
///
/// [`DogsConfig::load`] stays as the gate in front of that, unconditional
/// and first. It is strictly the stricter of the two parses -- a stray
/// top-level scalar is a valid document and not a valid `DogsConfig` -- and
/// it is the same call `shep_daemon::dogs` reads this file with, so a file
/// this gate refuses is one the daemon could not have served either.
/// Refusing it here is the rule both writers already followed: a file this
/// verb cannot understand is not one it may overwrite.
///
/// # Errors
///
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
        // No input reaches this. `toml` 0.8 is a thin layer over
        // `toml_edit`, so a source the gate above accepted parses here as
        // well. Reported rather than `expect`ed all the same: this runs at
        // the top of every daemon boot, and a boot is not a place to
        // panic on a disagreement between two parsers.
        DogMigrationError::ReadDogs(std::io::Error::other(
            "dogs.toml parses as TOML values and not as an editable document",
        ))
    })
}

/// Every name `table` declares a VALUE for under `[dog]`.
///
/// Read from a second parse of a file [`ShepToml`] is about to parse
/// again, and deliberately so: it is the only record of what was there
/// BEFORE [`ShepToml::take_dog_sections`] struck the table, which is what
/// the guard inside [`migrate_dog_sections`] compares against. Read-only,
/// over a string already in memory, and it never opens the file. Empty
/// also means "do not open the file at all", so this is the whole of that
/// caller's gate as well as its record.
///
/// A name holding nothing is left out, and that is the whole of
/// [`declares_nothing`]'s reason to exist. `shep enable metrics` on any
/// binary older than this branch scaffolds an EMPTY `[dog.metrics]`, and
/// this branch's own `enable` no longer does -- but a mixed-version host is
/// ordinary, so the shape keeps arriving. Counting it as a declared name
/// made the new binary refuse to boot against a `dogs.toml` that already
/// held `metrics`, on a `WouldOverwrite` whose own doc says "two values for
/// one key". An empty table is not a value.
fn declared_dog_names(table: &toml::Table) -> BTreeSet<String> {
    match table.get("dog") {
        Some(toml::Value::Table(dog)) => dog
            .iter()
            .filter(|(_, value)| !declares_nothing(value))
            .map(|(name, _)| name.clone())
            .collect(),
        // No `[dog]` at all, or a `dog` that is not a table: either way it
        // declares no dog and there is nothing here to open the document
        // for.
        _ => BTreeSet::new(),
    }
}

/// Whether `value` holds nothing an operator could lose by not moving it.
///
/// An empty table, an empty array, and an array of tables that are all
/// empty. That last one is why this recurses rather than testing
/// `Table::is_empty` directly: `[[dog.metrics]]` with nothing under it used
/// to reach [`DogMigrationError::SectionsUnreadable`], because the name was
/// declared and [`ShepToml::take_dog_sections`] hands back no array. Under
/// the pre-flight that refusal is a boot the operator has to repair by
/// hand, over a header holding nothing.
///
/// A `[[dog.metrics]]` that DOES carry values is a different question and
/// still refuses: there is no one section for it to become, and dropping it
/// silently is the outcome the name guard exists to prevent.
fn declares_nothing(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.is_empty(),
        toml::Value::Array(items) => items.iter().all(declares_nothing),
        _ => false,
    }
}

/// Walks `item` depth-first and gives every table it holds the next
/// document position, so a section moved out of `shep.toml` renders after
/// everything already in `dogs.toml` rather than at the index it happened
/// to hold in the file it came from.
///
/// Depth-first and over sub-tables too, because `toml_edit` renders every
/// table by its own position and not by its parent's: renumbering only the
/// top-level `bark` would leave `[bark.sinks]` back at `shep.toml`'s
/// index, split away from the header it belongs under.
///
/// [`Item::Value`] and [`Item::None`] hold no positioned table --- an
/// inline `metrics = { .. }` is a key/value pair and renders with the
/// others, above the tables --- so both are left alone.
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
/// contents, so there is nothing here to redact. That is a claim about the
/// two wrapped types that COULD carry a file, and both of them redact their
/// own `Debug` for exactly that reason: [`ShepTomlError`] for `shep.toml`,
/// and [`DogsConfigError`] for `dogs.toml`. The second one did not, for a
/// while, and this comment asserted it anyway -- a derive over
/// `toml::de::Error` prints the parser's `raw` field, which is the whole
/// source document. A derive here is only ever as safe as what it forwards
/// to, so a new variant wrapping a parse error needs the same check.
#[derive(Debug)]
// `#[non_exhaustive]`: the migration is the one writer of `dogs.toml` and
// will grow refusals as operators meet shapes nobody predicted, so a
// seventh variant should be additive rather than breaking (IR-20).
#[non_exhaustive]
pub(crate) enum DogMigrationError {
    /// `shep.toml` itself could not be read.
    Read(std::io::Error),
    /// `dogs.toml` exists and could not be read.
    ///
    /// Its own variant rather than [`Self::Read`]: both are the same I/O
    /// failure, and an operator's next move depends entirely on which of
    /// the two files it was.
    ReadDogs(std::io::Error),
    /// `dogs.toml` already exists and is not valid TOML.
    Parse(DogsConfigError),
    /// `name` has a section carrying values in both files, so the move
    /// would silently pick one of the two.
    ///
    /// An empty `[dog.<name>]` never raises this. It is not a value, it is
    /// what every `shep enable` before this branch scaffolded, and refusing
    /// on it took a mixed-version host to a shepherd that would not boot.
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
    /// `dogs.toml` could not be written.
    Write(std::io::Error),
    /// `dogs.toml`'s sibling lock file could not be created or locked.
    ///
    /// Refused rather than pressed on without the lock: the guarantee the
    /// dogs page makes to a provisioning script is the whole reason the
    /// lock is taken, and a write that skips it quietly is worse than one
    /// that says it could not run.
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

    /// The bug the lock exists for, forced rather than hoped for: two
    /// `shep rehome` calls for two DIFFERENT dogs, started together the way
    /// a provisioning script backgrounds them, and both removals have to
    /// land. Each call is a whole-file read, remove, rename, so without a
    /// lock both threads read the two-dog file and the second rename puts
    /// the first one's dog back, with nothing anywhere reporting it.
    ///
    /// Two threads and a barrier, not two processes: `flock(2)` is
    /// per-open-file-description rather than per-process, so two threads
    /// that each open the lock file contend exactly as two `shep`
    /// invocations do, and this needs no re-exec harness to prove it.
    /// Repeated over fresh homes because the losing interleaving is a
    /// race, and one round of a race that happens to serialise itself
    /// proves nothing.
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

    /// The other pair of writers, and the reason the migration takes the
    /// same lock rather than only the verb that races itself: a boot
    /// migrating a section INTO `dogs.toml` while an operator's `shep
    /// rehome` takes a different dog's section out. Whoever renames second
    /// wins the whole file, so an unlocked pair either resurrects the
    /// rehomed dog or drops the migrated one, and both are silent.
    ///
    /// The migration holds `shep.toml`'s lock while it takes `dogs.toml`'s.
    /// The rehome half takes only `dogs.toml`'s and holds nothing else, so
    /// there is no second ordering for the two to deadlock across.
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
            // Which of the two ran first is genuinely undecided, and only
            // one ordering has a section for the rehome to find: if it
            // went first, `otel` was struck before the migration merged
            // `metrics` in beside nothing. Both orderings agree on the
            // file that has to be left behind, which is the property.
            assert_eq!(moved, vec!["metrics".to_string()], "round {round}");
            assert_eq!(
                left.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                vec!["metrics"],
                "round {round}: the migrated section stays and the rehomed one goes"
            );
        }
    }

    /// The whole of spec decision 9, at the writer that reproduced its
    /// absence: `shep rehome` used to turn a commented `dogs.toml` with
    /// inline tables into header-per-key output with every comment gone.
    ///
    /// Exact string, not a parse-and-compare, for the same reason
    /// `taking_dog_sections_leaves_every_other_section_alone` pins one: a
    /// reparse agrees on the values, which is precisely what a wrecked
    /// file also does. The comments, the inline table and the blank lines
    /// are the assertion.
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

    /// The same promise at the other writer, and the case that says the
    /// migration's first write is not the only one it makes: a second
    /// `[dog.<name>]` appearing in `shep.toml` after an operator has spent
    /// months hand-editing the `dogs.toml` the first migration created.
    ///
    /// Two properties in one exact string, because they are two halves of
    /// the same round trip. The destination keeps its own comment and its
    /// inline table, and the moved section arrives carrying the comments
    /// an operator wrote around it in `shep.toml` -- both of which a
    /// `toml::to_string` of a parsed map dropped.
    ///
    /// The moved section lands AFTER everything already there, which is
    /// what `renumber_tables` is for: without it `[metrics]` carries the
    /// position it held in `shep.toml` and renders between the
    /// destination's first and second tables.
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
    /// `toml_edit` renders every table by its own position, not by its
    /// parent's, so renumbering only the top-level `bark` would leave
    /// `[bark.sinks]` and `[[bark.rules]]` at the indices they held in
    /// `shep.toml` -- interleaved with `[a]`, `[b]` and `[c]` here. The
    /// three destination sections staying consecutive is the property.
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

    /// Fails if an empty `[dog.<name>]` can stop a shepherd booting.
    ///
    /// Reproduced with a pre-branch `0.1.30` binary: its `shep enable
    /// metrics` scaffolds an empty `[dog.metrics]`, and the new binary then
    /// refused the whole migration on `WouldOverwrite` against a
    /// `dogs.toml` that already held `metrics` -- so `shep start` exited 5
    /// on a daemon that exited 4. This branch stopped the new `enable` from
    /// scaffolding, but every older binary still on a box does it and a
    /// mixed-version host is ordinary.
    ///
    /// Nothing written on either side is the assertion: the configured
    /// `dogs.toml` is the one that wins, untouched, and `shep.toml` keeps
    /// its stray header rather than being rewritten to strike it.
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

    /// The item this subsumes: `[[dog.metrics]]` with nothing under it.
    ///
    /// `take_dog_sections` hands back no array, so the name guard used to
    /// call it `SectionsUnreadable` -- which under the reload pre-flight is
    /// a refused boot over a header holding nothing.
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

    /// The other half of the same ruling, and the reason the skip is over
    /// values rather than over the spelling: an array of tables that DOES
    /// carry values has no one section to become, and dropping it silently
    /// is what the name guard exists to prevent.
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

    /// Fails if a header an operator spelled with spaces inside the
    /// brackets strands its section in `shep.toml` forever.
    ///
    /// `[ dog.metrics ]` is ordinary TOML and means exactly what
    /// `[dog.metrics]` means. A substring test for `"[dog."` matched
    /// neither it nor `[dog . metrics]`, so the migration returned "nothing
    /// to do", the section stayed where nothing reads it any more, and the
    /// dog came up on its compiled defaults with no warning on any surface.
    /// For bark that is every sink gone and alerting silently off.
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
    /// The inode is the assertion, not the bytes, because the bytes are
    /// identical either way. `ShepToml::edit` and `try_edit` stage a temp
    /// file and rename it over the original whenever `save` runs, so
    /// opening the document on an idle boot would hand an untouched
    /// `shep.toml` a fresh inode, force `CONFIG_FILE_MODE` onto it, and
    /// turn a symlinked path into a plain file. Every boot after the first
    /// takes this path, so the cost lands on every operator.
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
