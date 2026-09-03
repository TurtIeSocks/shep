//! `$SHEP_HOME/shep.toml`, the daemon's own config file — read and rewritten
//! by [`ShepToml`], the one writer this binary has for it.
//!
//! Edits go through `toml_edit`'s [`DocumentMut`] rather than round-tripping
//! through a plain `toml::Table`: `shep.toml` is hand-written far more often
//! than it is generated, and a `shep enable` that reformatted it — dropping
//! comments, reordering keys — would be a reason not to run `shep enable`.
//! [`shep_core::config::DaemonConfig::load`] is still the one place the
//! SHAPE of the file is decided (what a key means, what a bad value looks
//! like); this module only ever adds or removes the handful of keys each
//! verb owns, leaving everything else exactly as it was read.
//!
//! Every edit goes through [`ShepToml::edit`] or [`ShepToml::try_edit`] (for
//! a closure that can itself refuse), which together are the whole write
//! path: `$SHEP_HOME` created at `0700`, an exclusive advisory lock on a
//! sibling `shep.toml.lock` held across the read-modify-write, and the new
//! document staged in a `0600` temp file, `fsync`ed and `rename`d over the
//! original -- but only when the closure actually produced a value to
//! save; a `try_edit` closure's own `Err` leaves `path` untouched, not
//! merely unchanged. Each of the write's three steps is the same shape
//! `shep-core`'s `barks::append` already uses, for the same reasons and
//! after the same bug: two writers racing on `barks.jsonl` silently lost
//! half of each other's records until an advisory lock landed there.
//!
//! `#[cfg(unix)]` wholesale, at `commands`' own declaration in `main.rs` —
//! `flock(2)` and unix mode bits are both Unix-only, and Windows is shep's
//! 0% tier where no verb runs at all.

// `clippy::result_large_err` fires on every `Result<_, ShepTomlError>`
// signature in this module on Windows, and on none of them on macOS or
// Linux. The lint compares the error against a fixed 128-byte threshold, and
// `ShepTomlError` — a `PathBuf` plus a `toml_edit::TomlError` — sits close
// enough to it that the platform's own layout decides which side it lands
// on. Nothing about this module is different on Windows; only the
// measurement is.
//
// Allowed rather than fixed, deliberately. The fix the lint wants is boxing
// the error, which would change a `pub enum`'s shape for every consumer on
// every platform to satisfy a perf lint about a path that runs a handful of
// times per command and always ends in file I/O. Revisit if this type grows
// a genuinely large variant, which is a different fact from the one here.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::style::StyleLevel;

/// Mode `shep.toml` (and the sibling lock file and staging file it is
/// written through, and `dogs.toml`, which
/// `commands::dog_migration` stages through the same helper) is created
/// with: owner read/write, nobody else.
///
/// Unlike `barks.jsonl`'s own `0600`, this is not belt-and-braces. This
/// file is where `docs/dogs.md` tells an operator to paste a Discord or
/// Slack webhook URL, and both carry a bearer token in the path, so the
/// mode here is the guard rather than a second one behind `$SHEP_HOME`'s.
/// It is also what a `tar`, a `cp -p` or a backup of `$SHEP_HOME` carries
/// out with the file, somewhere no directory mode follows it.
#[cfg_attr(windows, allow(dead_code))]
const CONFIG_FILE_MODE: u32 = 0o600;

/// Extensions [`ShepToml::write_starter_interpreters`] maps, in the order
/// they land in `shep.toml`.
///
/// `js`/`mjs`/`cjs` cover the three ways a Node script is named; `py` maps
/// to `python3` rather than bare `python`, which is absent or still points
/// at Python 2 on plenty of hosts shep runs on; `rb` and `sh` round out the
/// four families the maintainer named directly. Two more chosen with judgement:
/// `pl` for Perl and `php` for PHP, both single, unambiguous interpreters
/// that ship alongside node/python3/ruby/sh on most of the same hosts.
/// Left out on purpose: `ts` (no single safe default exists; ts-node, tsx
/// and deno all disagree about how to run one, and picking the wrong one
/// silently is worse than making the operator say so), and anything
/// Windows-only such as `ps1`, since shep's Windows tier is 0 percent as
/// of this writing.
const STARTER_INTERPRETERS: &[(&str, &str)] = &[
    ("js", "node"),
    ("mjs", "node"),
    ("cjs", "node"),
    ("py", "python3"),
    ("rb", "ruby"),
    ("sh", "sh"),
    ("pl", "perl"),
    ("php", "php"),
];

/// The comment [`ShepToml::write_starter_interpreters`] writes directly
/// above the `[interpreters]` table it scaffolds, so the mapping reads as
/// something an operator can see and edit rather than as hidden shep
/// behaviour.
///
/// Plain `#` TOML comment lines, not `///` Rust doc syntax: this text
/// lands inside `shep.toml` itself, for an operator to read there, so the
/// project's "no dashes in anything a user reads" rule governs it exactly
/// as it governs `welcome.rs`'s copy.
const INTERPRETERS_STARTER_COMMENT: &str = "\
# Extension -> interpreter mapping. shep applies one of these to a script
# when nothing more specific already named an interpreter: not this app's
# own Flockfile entry, and not --interpreter on the command line, both of
# which win over anything here. shep never guesses beyond what is written
# below, so edit freely: change an interpreter, add an extension, or
# delete an entry (or this whole table) to turn the mapping off for it.
";

/// The one writer of `$SHEP_HOME/shep.toml` in this binary.
///
/// A missing file is created (as an empty document — [`Self::edit`] makes
/// `$SHEP_HOME` too, if needed); a file that will not parse is refused
/// rather than overwritten, because it may hold every knob a daemon boots
/// with, credentials included, and there is no undo for losing it to a
/// typo'd verb.
///
/// [`Self::edit`] is the only way to reach one of these, and holds the
/// document's lock for exactly as long as the closure runs. Reading and
/// writing are deliberately not separate public steps: a caller that
/// could read, think, and then write would be the lost update this type
/// takes a lock to prevent.
pub struct ShepToml {
    path: PathBuf,
    doc: DocumentMut,
}

/// Manual, not derived: `doc` is the parsed document, and a `[dog.<name>]`
/// table routinely holds a webhook URL with a bearer token in its query
/// string (`SECURITY.md`) — the same exposure `DogSectionToml`
/// (shep-core's own `protocol::request`) exists to keep out of a
/// `{:?}`-formatted `Response`. `ShepToml` never crosses that wire, but it
/// is exactly as capable of being `{:?}`-printed into a log by some future
/// caller, so it gets the same treatment: only the path, never the parsed
/// document.
impl std::fmt::Debug for ShepToml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShepToml")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ShepToml {
    /// Reads `path`, hands the document to `f`, and writes it back — the
    /// whole read-modify-write under one exclusive advisory lock.
    ///
    /// The one way to write this file. `f` returns whatever the caller
    /// needed to read while the lock was held — `shep enable` reads a
    /// dog's source before it edits the same document — and that value
    /// comes back on success.
    ///
    /// Serialised against any other editor, in this process or another,
    /// by a lock on a sibling `shep.toml.lock` ([`ConfigLock`]). Without
    /// it, `shep adopt otel ... & shep enable metrics &` from one
    /// provisioning script has both processes read the pre-edit document
    /// and write the whole thing back, and the loser's edit is gone with
    /// no error on either side. That is not theory: `barks.jsonl` lost
    /// half its records to the identical shape before it grew the same
    /// lock.
    ///
    /// `f` here is infallible; see [`Self::try_edit`] for a closure that
    /// can itself refuse the edit before anything is written.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] — `$SHEP_HOME` could not be created, the
    ///   lock beside the file could not be taken, or the file could not
    ///   be read or replaced.
    /// - [`ShepTomlError::Parse`] — the file exists and is not valid
    ///   TOML. Refused rather than overwritten, and `f` never runs.
    pub fn edit<T>(path: &Path, f: impl FnOnce(&mut Self) -> T) -> Result<T, ShepTomlError> {
        let (mut doc, _lock) = Self::open_locked(path)?;
        let value = f(&mut doc);
        doc.save()?;
        Ok(value)
    }

    /// Like [`Self::edit`], but for a closure that can itself refuse the
    /// edit: `f`'s own `Err` skips [`Self::save`] entirely, the same way a
    /// [`Self::open`] failure already does. A setter whose key can already
    /// be occupied by a shape it cannot write into (an operator's
    /// hand-written `style = "full"` where [`Self::set_style_level`] needs
    /// a table, say) must be able to say so without the read-modify-
    /// write underneath it staging and renaming a byte-identical copy of
    /// the file back over itself anyway — that rename still lands a fresh
    /// inode and forces [`CONFIG_FILE_MODE`] on a file that a refused edit
    /// never actually touched, and for a symlinked `path` it is what
    /// replaces the link with a plain file. [`Self::edit`]'s `f` cannot
    /// refuse at all, so that failure mode did not exist before this
    /// method's first caller needed to fail from inside the closure.
    ///
    /// Generic over the closure's own error `E` rather than fixed to
    /// [`ShepTomlError`], so a caller whose own failure is its own type
    /// does not have to wrap this module's error a second time;
    /// `E: From<ShepTomlError>` is what lets `?` cover this method's own
    /// setup failures (home dir, lock, parse) the same way it already
    /// covers `f`'s.
    ///
    /// # Errors
    /// Everything [`Self::edit`] can fail with, converted through
    /// `E::from`, plus whatever `f` itself returns as `Err` -- in either
    /// case, `path` is left exactly as [`Self::open`] found it.
    pub fn try_edit<T, E: From<ShepTomlError>>(
        path: &Path,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        let (mut doc, _lock) = Self::open_locked(path)?;
        let value = f(&mut doc)?;
        doc.save()?;
        Ok(value)
    }

    /// Creates `$SHEP_HOME` if missing, takes `path`'s exclusive lock, and
    /// opens the document -- the setup [`Self::edit`] and [`Self::try_edit`]
    /// share; only what happens with the open document, and whether a
    /// failure from it still reaches [`Self::save`], differs between the
    /// two.
    ///
    /// The returned [`ConfigLock`] must outlive every use of the returned
    /// `Self` -- it is what makes the read this function just did and the
    /// caller's eventual `save` one transaction as far as any other editor
    /// is concerned, the same guarantee [`Self::edit`]'s own doc describes.
    fn open_locked(path: &Path) -> Result<(Self, ConfigLock), ShepTomlError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        create_home_dir(parent).map_err(|source| ShepTomlError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        // Held until the caller's `Self`/`ConfigLock` pair both drop, so
        // the read just below and the caller's eventual rename inside
        // `save` are one transaction as far as any other editor is
        // concerned.
        let lock = ConfigLock::acquire(path).map_err(|source| ShepTomlError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let doc = Self::open(path)?;
        Ok((doc, lock))
    }

    /// Reads `path`, treating a missing file as an empty document.
    ///
    /// Private. Reached two ways: from [`Self::edit`]/[`Self::try_edit`]
    /// with the document's lock already held for a write, and from
    /// [`Self::adopted_dog_path_readonly`] with no lock at all, for a
    /// caller that only ever reads.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] — the file exists and could not be read.
    /// - [`ShepTomlError::Parse`] — the file exists and is not valid TOML.
    fn open(path: &Path) -> Result<Self, ShepTomlError> {
        let doc = match std::fs::read_to_string(path) {
            Ok(text) => text
                .parse::<DocumentMut>()
                .map_err(|source| ShepTomlError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
            Err(source) => {
                return Err(ShepTomlError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            doc,
        })
    }

    /// Adds `name` to `[daemon] enabled_dogs` (idempotently), and writes
    /// nothing else anywhere.
    ///
    /// **It used to scaffold an empty `[dog.<name>]` here, and that is now
    /// a boot-breaking bug rather than a nicety.** A dog's configuration
    /// lives in `dogs.toml`, so an operator who enabled a dog and then
    /// configured it where `docs/dogs.md` says to had that name in both
    /// files, and `commands::dog_migration` refuses on exactly that: two
    /// values for one key is a question shep cannot answer. The daemon
    /// exits 4 and the flock is left unsupervised, over a table nobody
    /// asked for.
    ///
    /// Scaffolding into `dogs.toml` instead would keep the nicety, and is
    /// the wrong trade. It puts this type, which owns `shep.toml` and only
    /// that, in the business of writing a second file behind a second lock,
    /// and every write of that file has to hold the staged-temp, `fsync`
    /// and `rename` discipline `dog_migration::write_dogs_config` carries
    /// because webhook credentials live there at `0600`. The nicety it
    /// buys is thin: `shep-daemon`'s `dog_section` already documents an
    /// absent section as legitimate and answers an empty string, so a dog
    /// enabled with no section runs on its defaults, and an empty table
    /// tells an operator nothing a documented example does not tell them
    /// better. Writing nothing cannot collide with anything.
    pub fn enable_dog(&mut self, name: &str) {
        let daemon = self.daemon_table_mut();
        let enabled_dogs = daemon
            .entry("enabled_dogs")
            .or_insert_with(|| Item::Value(Value::Array(Array::new())))
            .as_array_mut()
            .expect("enabled_dogs is only ever written as an array");
        if !enabled_dogs.iter().any(|v| v.as_str() == Some(name)) {
            enabled_dogs.push(name);
        }
    }

    /// Removes `name` from `[daemon] enabled_dogs` and touches nothing
    /// else: an operator who disables a dog to restart it must not lose the
    /// configuration they wrote for it.
    ///
    /// That configuration lives in `dogs.toml` now, so keeping it is
    /// something this method achieves by doing nothing at all rather than
    /// by leaving a `[dog.<name>]` table alone. The behaviour is unchanged;
    /// only the file the promise is about moved. [`Self::rehome_dog`] is
    /// the half that forgets a dog for real, and `commands::dogs::rehome`
    /// is where the other file is reached.
    pub fn disable_dog(&mut self, name: &str) {
        if let Some(enabled_dogs) = self
            .doc
            .get_mut("daemon")
            .and_then(Item::as_table_mut)
            .and_then(|daemon| daemon.get_mut("enabled_dogs"))
            .and_then(Item::as_array_mut)
        {
            enabled_dogs.retain(|v| v.as_str() != Some(name));
        }
    }

    /// Records `name`'s binary in `[daemon] adopted_dogs` and enables it.
    ///
    /// Called by `commands::dogs::adopt`, once `vet_binary` has already
    /// vetted `exec` — this method itself does no vetting, and never
    /// truncates anything past the two keys it owns.
    pub fn adopt_dog(&mut self, name: &str, exec: &Path) {
        let daemon = self.daemon_table_mut();
        let adopted_dogs = daemon
            .entry("adopted_dogs")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .expect("adopted_dogs is only ever written as a table");
        adopted_dogs.insert(
            name,
            Item::Value(exec.to_string_lossy().into_owned().into()),
        );
        self.enable_dog(name);
    }

    /// Removes the whole `[dog]` table and hands back what was under it.
    ///
    /// Keyed by dog name with the `dog.` prefix dropped, which is the shape
    /// `dogs.toml` wants. A document with no `[dog]` table yields an empty
    /// map and is left byte-identical, so a second call after a migration
    /// writes nothing.
    ///
    /// The one caller is the boot migration. This is not a general editing
    /// primitive: it takes everything, because a partial move would leave
    /// the same key readable from two files.
    pub fn take_dog_sections(&mut self) -> BTreeMap<String, toml::Table> {
        let Some(item) = self.doc.remove("dog") else {
            return BTreeMap::new();
        };

        // A `Table`'s own `to_string()` renders only its direct key/value
        // pairs -- a nested sub-table, an array of tables, or an inline
        // table underneath it does not survive that round trip once the
        // table is detached from the document root. Re-attaching `item`
        // under a fresh document and rendering THAT is what keeps every
        // header and array-of-tables marker: the fresh document is exactly
        // as capable of printing `[dog.bark.sinks]` and `[[dog.bark.rules]]`
        // as the original one was. Parsing the result with `toml` (not
        // `toml_edit`) rather than walking `item` by hand is what also
        // catches the inline-table shape, since `toml`'s deserializer does
        // not care how a table was spelled.
        let mut wrapper = DocumentMut::new();
        wrapper.insert("dog", item);
        let Ok(mut parsed) = wrapper.to_string().parse::<toml::Table>() else {
            return BTreeMap::new();
        };
        let Some(toml::Value::Table(dog)) = parsed.remove("dog") else {
            return BTreeMap::new();
        };
        dog.into_iter()
            .filter_map(|(name, value)| match value {
                toml::Value::Table(section) => Some((name, section)),
                _ => None,
            })
            .collect()
    }

    /// The binary path recorded for `name` in `[daemon] adopted_dogs`, if
    /// any — `None` for a built-in dog, or a name this document has never
    /// heard of.
    ///
    /// Read by `commands::dogs::rehome` before [`Self::rehome_dog`] removes
    /// the entry, so the verb can still report what it forgot.
    #[must_use]
    pub fn adopted_dog_path(&self, name: &str) -> Option<PathBuf> {
        self.doc
            .get("daemon")?
            .as_table()?
            .get("adopted_dogs")?
            .as_table()?
            .get(name)?
            .as_str()
            .map(PathBuf::from)
    }

    /// Every name `[daemon] adopted_dogs` records, in TOML document order.
    ///
    /// Read by `commands::dogs::enable` to name the adopted dogs in its
    /// refusal of a name that is neither adopted nor built in — a refusal
    /// that lists the way out is worth the allocation, and this is the
    /// only caller that wants the whole set rather than one lookup.
    #[must_use]
    pub fn adopted_dog_names(&self) -> Vec<String> {
        self.doc
            .get("daemon")
            .and_then(Item::as_table)
            .and_then(|daemon| daemon.get("adopted_dogs"))
            .and_then(Item::as_table)
            .map(|adopted| adopted.iter().map(|(name, _)| name.to_string()).collect())
            .unwrap_or_default()
    }

    /// [`Self::adopted_dog_path`] without [`Self::edit`]'s write side --
    /// for a caller that only wants the answer, such as `lib.rs`'s
    /// `dispatch_adopted_dog`, which runs on every unrecognized verb, most
    /// of which are typos rather than dog names.
    ///
    /// Creates nothing: a missing `$SHEP_HOME` or a missing `path` is an
    /// ordinary "no such dog" answer ([`Self::open`] already treats a
    /// missing file as an empty document), never a reason to create
    /// either. Takes no lock, unlike [`Self::edit`] -- `Self::save`'s
    /// rename onto `path` is atomic, so a concurrent writer can only ever
    /// make this read observe the document just before or just after that
    /// write, never a torn one.
    ///
    /// # Errors
    /// [`ShepTomlError::Io`] if `path` exists and could not be read.
    /// [`ShepTomlError::Parse`] if `path` exists and is not valid TOML.
    pub fn adopted_dog_path_readonly(
        path: &Path,
        name: &str,
    ) -> Result<Option<PathBuf>, ShepTomlError> {
        Ok(Self::open(path)?.adopted_dog_path(name))
    }

    /// Forgets `name` in this file: out of `enabled_dogs`, out of
    /// `adopted_dogs`, and `[dog.<name>]` removed if an un-migrated
    /// `shep.toml` still carries one. The difference between `rehome` and
    /// `disable`, and the reason they are two verbs.
    ///
    /// **This is half of a rehome.** A dog's configuration lives in
    /// `dogs.toml` now, and striking it there is
    /// `commands::dog_migration::forget_dog_section`, called by
    /// `commands::dogs::rehome` immediately after this: one file per
    /// writer, since this type owns `shep.toml` and only that.
    pub fn rehome_dog(&mut self, name: &str) {
        self.disable_dog(name);
        if let Some(adopted_dogs) = self
            .doc
            .get_mut("daemon")
            .and_then(Item::as_table_mut)
            .and_then(|daemon| daemon.get_mut("adopted_dogs"))
            .and_then(Item::as_table_mut)
        {
            adopted_dogs.remove(name);
        }
        if let Some(dog) = self.doc.get_mut("dog").and_then(Item::as_table_mut) {
            dog.remove(name);
        }
    }

    /// Writes `[style] level = "<level>"`, creating the `[style]` table
    /// when this document has none yet, and replacing the value when one
    /// is already there.
    ///
    /// The value written is `level`'s own `Display` spelling --
    /// `full`/`plain`/`bare` -- the same string `style_from_config`
    /// (`lib.rs`) parses back through `clap::ValueEnum::from_str`, so a
    /// round trip through this setter and back stays one grammar rather
    /// than a writer and a reader that merely happen to agree today.
    ///
    /// Called by `shep style <level>` (`Commands::Style`'s set form).
    ///
    /// # Errors
    /// [`ShepTomlError::WrongShape`] -- `style` is already there as
    /// something other than a table, e.g. an operator hand-wrote
    /// `style = "full"` at the top level. Reported rather than forced:
    /// `.as_table_mut().expect(..)` on `entry().or_insert_with(..)` is
    /// this module's usual idiom (`enable_dog`/`disable_dog`/`adopt_dog`/
    /// `rehome_dog` all still use it), but it is sound there only because
    /// nothing else in this file ever writes those keys as anything but
    /// a table, so the `expect` never actually fires on real input. This
    /// setter's key can be hand-written by an operator who reasonably
    /// guessed `style = "full"` instead of the `[style]` header, so the
    /// same `expect` here is reachable from data this process does not
    /// control -- exactly the panicking-constructor shape IR-21 rules
    /// out. The four sibling setters above still carry the shape this one
    /// used to; that is a tracked follow-up, not this fix's scope.
    pub fn set_style_level(&mut self, level: StyleLevel) -> Result<(), ShepTomlError> {
        let item = self
            .doc
            .entry("style")
            .or_insert_with(|| Item::Table(Table::new()));
        let Some(style) = item.as_table_mut() else {
            return Err(ShepTomlError::WrongShape {
                path: self.path.clone(),
                key: "style",
                found: item.type_name(),
            });
        };
        style.insert("level", Item::Value(level.to_string().into()));
        Ok(())
    }

    /// Writes the starter `[interpreters]` mapping (task 47) -- a script
    /// extension to the interpreter shep runs it with, active from the
    /// moment it lands rather than commented into inertness, with an
    /// explanatory comment above the table so it reads as something an
    /// operator wrote and can freely edit rather than as hidden behaviour.
    /// Active is the point: shep never infers an interpreter on its own,
    /// but a fresh `$SHEP_HOME` still has to be able to run the
    /// `shep start server.js` `welcome.rs` and `--help` both advertise as
    /// the quick start, and a mapping nobody has uncommented yet cannot do
    /// that.
    ///
    /// A no-op when `[interpreters]` already exists. Called once per home
    /// from `lib.rs`'s first-run scaffold, but idempotent by construction
    /// rather than by that single call site -- the same reasoning
    /// [`Self::enable_dog`] gives for its own idempotence -- so a caller
    /// that runs this twice, or a `shep.toml` an operator has since hand-
    /// edited, is never clobbered or duplicated.
    pub fn write_starter_interpreters(&mut self) {
        if self.doc.contains_key("interpreters") {
            return;
        }
        let mut table = Table::new();
        for (extension, interpreter) in STARTER_INTERPRETERS {
            table.insert(extension, Item::Value((*interpreter).into()));
        }
        table.decor_mut().set_prefix(INTERPRETERS_STARTER_COMMENT);
        self.doc.insert("interpreters", Item::Table(table));
    }

    /// Writes the document back: staged in a sibling temp file at
    /// [`CONFIG_FILE_MODE`], `fsync`ed, then `rename`d over `path`.
    ///
    /// Private, and reached only from [`Self::edit`] with the lock held.
    ///
    /// `std::fs::write` opens `O_TRUNC`, so a crash, a signal or an
    /// `ENOSPC` between the truncate and the write leaves an operator's
    /// whole `shep.toml` truncated or empty — the one loss this type's
    /// own doc says there is no undo for. Staging and renaming is what
    /// every other file this workspace writes already does
    /// (`barks::write_ring`, `snapshot::write_atomic`,
    /// `boot::write_pidfile`), and it is also what re-tightens a
    /// `shep.toml` that an older shep left at `0644`: the rename
    /// installs the staging file's inode, mode included.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] — the staging file could not be created or
    ///   written, or the rename over `path` failed.
    fn save(&self) -> Result<(), ShepTomlError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = create_config_file(parent).map_err(|source| self.io_error(source))?;
        tmp.write_all(self.doc.to_string().as_bytes())
            .map_err(|source| self.io_error(source))?;
        tmp.as_file()
            .sync_all()
            .map_err(|source| self.io_error(source))?;
        // `persist` is `rename(2)`. On failure the `NamedTempFile` comes
        // back inside the error and its `Drop` removes the staging file,
        // so a failed replace leaves nothing behind in `$SHEP_HOME`.
        tmp.persist(&self.path)
            .map_err(|err| self.io_error(err.error))?;
        Ok(())
    }

    /// This file's [`ShepTomlError::Io`], for the several ways one write
    /// of it can fail.
    fn io_error(&self, source: std::io::Error) -> ShepTomlError {
        ShepTomlError::Io {
            path: self.path.clone(),
            source,
        }
    }

    /// `[daemon]`, creating it (empty) if this document has none yet.
    fn daemon_table_mut(&mut self) -> &mut Table {
        self.doc
            .entry("daemon")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .expect("daemon is only ever written as a table")
    }
}

/// Creates `dir` (and any missing parent) at `boot::DIR_MODE` directly,
/// via `DirBuilderExt`, rather than `create_dir_all` and a later `chmod`.
///
/// `$SHEP_HOME` holds the webhook URLs `docs/dogs.md` tells an operator to
/// paste into `[dog.bark.sinks]`, and on a host that has never booted a
/// shepherd this call is the one that creates it — `boot::init_dirs`, which
/// force-chmods it to `DIR_MODE`, does not run until the first `shep
/// muster`. A `create_dir_all` here would leave it at the ambient umask,
/// typically `0755`, for every local user to read until that boot. Asking
/// for the mode at `mkdir` time also leaves no window in which the
/// directory exists wider, the same TOCTOU discipline
/// `launch::launch_command` and `boot::create_dir_at_dir_mode` each spell
/// out at their own call.
///
/// Reuses `shep_daemon::boot::DIR_MODE` rather than restating `0o700`: one
/// spelling of the number, so a change to the daemon's posture cannot pass
/// this by.
fn create_home_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    // Windows has no scalar mode; `shep_daemon::boot::create_dir_at_dir_mode`
    // carries the argument for what protects `$SHEP_HOME` there instead.
    #[cfg(unix)]
    builder.mode(shep_daemon::boot::DIR_MODE);
    builder.create(dir)
}

/// Creates the staging file a config is written through, in `parent` so
/// the later `rename` stays within one filesystem.
///
/// Mode-at-creation rather than a separate `chmod` pass (`tempfile` passes
/// these permissions to the `open` call itself): there is no window in
/// which the file holding a webhook token sits at whatever the process
/// umask leaves it. Same shape, and same reasoning, as
/// `barks::create_ring_file`.
///
/// `pub(super)` rather than private, and deliberately shared rather than
/// copied: `commands::dog_migration` writes `dogs.toml`, which holds the
/// webhook URLs that used to live in this file and needs the same
/// [`CONFIG_FILE_MODE`] at the same `open`. A second implementation of
/// create-at-mode plus `fsync` plus `rename` is how the two would drift,
/// and the one that drifted would be the one nobody was reading.
pub(super) fn create_config_file(parent: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("shep").suffix(".toml.tmp");
    // `shep.toml` can hold a webhook token, so on unix it is created `0600`
    // at the `open` itself rather than chmod'ed after. On Windows it
    // inherits `$SHEP_HOME`'s ACL — the same gap `create_home_dir` above
    // names, and the reason the operator docs say so out loud.
    #[cfg(unix)]
    builder.permissions(std::fs::Permissions::from_mode(CONFIG_FILE_MODE));
    builder.tempfile_in(parent)
}

/// An exclusive advisory lock over one config file, held for as long as
/// the value lives and released when it drops (including on an early `?`,
/// and by the kernel if the process dies holding it).
///
/// Keyed on the path it is given rather than on `shep.toml` specifically:
/// [`ShepToml::edit`] takes one over `shep.toml`, and
/// `commands::dog_migration` takes one over `dogs.toml`, which has two
/// writers of its own. **Whenever both are held at once, `shep.toml`'s is
/// taken first**, which is the whole of what keeps the two orderings from
/// deadlocking; `migrate_dog_sections` is the one caller that holds both,
/// and it says so at the point it nests them.
///
/// The lock is on a sibling `<name>.lock`, never on the config itself,
/// and that is the whole design decision — the same one `barks::RingLock`
/// records: [`ShepToml::save`] finishes by `rename`ing a new file over the
/// config, which replaces the inode. A lock taken on the config would be a
/// lock on an inode the very next successful save unlinks; the next writer
/// would open the *new* inode, find it unlocked, and the two would be
/// excluding nothing. The lock file is never renamed, never rewritten and
/// never read; it exists only to be an inode with a stable identity, and
/// it is left on disk between edits on purpose so both writers keep
/// agreeing on which one it is.
pub(super) struct ConfigLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a
    /// leading underscore because it is held, never read.
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// The lock file, opened with `share_mode(0)`. The same primitive and
    /// the same sibling-file shape [`shep_core::kv`] and
    /// [`shep_core::barks`] use; see either for the full argument.
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
    pub(super) fn acquire(path: &Path) -> std::io::Result<Self> {
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

    #[cfg(unix)]
    pub(super) fn acquire(path: &Path) -> std::io::Result<Self> {
        use nix::fcntl::{Flock, FlockArg};

        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(CONFIG_FILE_MODE)
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

/// What [`ShepToml::edit`] can fail with. Module-scoped per IR-18.
///
/// Deliberately NOT `#[non_exhaustive]`, and this is the comment IR-20 asks
/// for in the negative case. shep-cli is `[[bin]]`-only — no `lib.rs`, no
/// published surface — so nothing outside this binary can match on this enum
/// and there is no downstream `match` for the attribute to protect. Adding it
/// would tax only this crate's own exhaustive matches, which are the ones we
/// WANT the compiler to break when `ShepToml::edit` grows a new failure mode.
/// Same reasoning as [`CronScheduleError`](shep_core::config::CronScheduleError)'s
/// own omission, for a different reason: that one is closed, this one is
/// unexported.
pub enum ShepTomlError {
    /// A read or write of `path` itself failed.
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying IO failure.
        source: std::io::Error,
    },
    /// `path` exists but is not valid TOML.
    Parse {
        /// The path that failed to parse.
        path: PathBuf,
        /// The parser's own complaint.
        source: toml_edit::TomlError,
    },
    /// `path` parses, but `key` is already there as something other than
    /// a table -- e.g. an operator writing `style = "full"` at the top
    /// level instead of `[style]` / `level = "full"`. Legal TOML, but not
    /// a shape any setter in this module can write a sub-key into:
    /// forcing it to a table would silently discard whatever the
    /// operator actually wrote there.
    WrongShape {
        /// The file that holds the wrongly-shaped value.
        path: PathBuf,
        /// The table key that was expected -- `"style"` for
        /// [`ShepToml::set_style_level`], the only caller today.
        key: &'static str,
        /// What TOML actually found there ([`Item::type_name`]) --
        /// `"string"`, `"array"`, and so on; never `"table"`, since that
        /// is the one shape this variant is never raised for.
        found: &'static str,
    },
}

/// Manual, not derived: `toml_edit::TomlError` carries the ENTIRE source
/// document internally (its own `raw` field, kept for `Display`'s
/// line-and-column rendering) — a `#[derive(Debug)]` here would forward to
/// that type's own derived `Debug` and print `shep.toml` in full, secrets
/// included, the exact leak `DaemonConfig`'s own `Debug` already declines
/// for its `dog` field. `Display` still shows the parser's full
/// line-of-context message (below): that is the one deliberate surface
/// meant for the operator who broke their own file to read, same as
/// `DaemonConfigError::Toml`'s. `Debug` is not that surface — it is what a
/// log captures — so it is redacted to the path and the parser's short
/// `message()`, never the line it quotes.
impl std::fmt::Debug for ShepTomlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => f
                .debug_struct("Io")
                .field("path", path)
                .field("source", source)
                .finish(),
            Self::Parse { path, source } => f
                .debug_struct("Parse")
                .field("path", path)
                .field("message", &source.message())
                .finish(),
            Self::WrongShape { path, key, found } => f
                .debug_struct("WrongShape")
                .field("path", path)
                .field("key", key)
                .field("found", found)
                .finish(),
        }
    }
}

impl std::fmt::Display for ShepTomlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Parse { path, source } => write!(f, "{}: {source}", path.display()),
            Self::WrongShape { path, key, found } => write!(
                f,
                "{}: [{key}] must be a table, found a {found}",
                path.display()
            ),
        }
    }
}

impl core::error::Error for ShepTomlError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::WrongShape { .. } => None,
        }
    }
}

// `unix` because the config-writing cases assert a `0600` mode and an inode preserved across an atomic rename — guarantees the Windows tier
// deliberately makes differently, each argued at its own call site
// above. What Windows claims instead is covered by `tests/cli_e2e.rs`
// and by the real-flock verification in the Windows port's own notes;
// this module's unix coverage is unchanged.
#[cfg(all(test, unix))]
mod tests {
    use shep_core::config::DaemonConfig;

    use super::*;

    /// `path`'s permission bits, masked to the nine that matter.
    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// fails if the writer round-trips through a plain `toml::Table`. An
    /// operator's `shep.toml` is hand-written, and a `shep enable` that
    /// silently dropped their comments and reordered their keys is a reason
    /// not to run `shep enable`.
    #[test]
    fn enabling_a_dog_leaves_the_rest_of_the_file_exactly_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        let original = "# the shepherd's own knobs\n[daemon]\nlog_level = \"info\"  # chatty\nlog_json = false\n";
        std::fs::write(&path, original).unwrap();

        ShepToml::edit(&path, |doc| doc.enable_dog("metrics")).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# the shepherd's own knobs"));
        assert!(written.contains("# chatty"));
        assert!(
            written.find("log_level").unwrap() < written.find("log_json").unwrap(),
            "key order survives"
        );

        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["metrics"]);
        assert!(
            cfg.dog.is_empty(),
            "enable writes no dog section at all; the next boot refuses a \
             name held in both files: {written}"
        );
    }

    /// fails if `enable` appends a duplicate on the second call, which
    /// would make the daemon try to start one dog twice at boot, or if
    /// `disable` takes the dog's configuration with it — the operator who
    /// disables a dog to restart it must get their rules back.
    #[test]
    fn enable_is_idempotent_and_disable_keeps_the_config_it_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[dog.bark]\ndebounce = \"30s\"\n").unwrap();

        ShepToml::edit(&path, |doc| {
            doc.enable_dog("bark");
            doc.enable_dog("bark");
        })
        .unwrap();
        let cfg =
            DaemonConfig::load(Some(&std::fs::read_to_string(&path).unwrap()), &|_| None).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["bark"]);

        ShepToml::edit(&path, |doc| doc.disable_dog("bark")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(cfg.daemon.enabled_dogs.is_empty());
        assert!(
            written.contains("30s"),
            "disable stops a dog; rehome is what forgets it"
        );
    }

    /// fails if a `shep.toml` that will not parse is overwritten instead of
    /// refused. That file may hold every knob a daemon boots with; losing
    /// it to a typo'd `shep enable` is not recoverable.
    #[test]
    fn a_file_that_will_not_parse_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon\nlog_json = true\n").unwrap();
        assert!(matches!(
            ShepToml::edit(&path, |doc| doc.enable_dog("metrics")),
            Err(ShepTomlError::Parse { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[daemon\nlog_json = true\n"
        );
    }

    /// fails if `rehome_dog` leaves anything behind: `[daemon] adopted_dogs`,
    /// `enabled_dogs`, or `[dog.<name>]` itself — the whole point of `rehome`
    /// over `disable` is that nothing survives it.
    #[test]
    fn rehoming_a_dog_forgets_it_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        // Seeded by hand, since no writer here creates one any more: a
        // `[dog.<name>]` in `shep.toml` is what an un-migrated file carries,
        // and striking it is still this method's job.
        std::fs::write(&path, "[dog.otel]\ndebounce = \"30s\"\n").unwrap();
        ShepToml::edit(&path, |doc| {
            doc.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["otel"]);
        assert_eq!(
            cfg.daemon
                .adopted_dogs
                .get("otel")
                .map(std::path::PathBuf::as_path),
            Some(Path::new("/usr/local/bin/shep-otel"))
        );
        assert!(cfg.dog.contains_key("otel"));

        ShepToml::edit(&path, |doc| doc.rehome_dog("otel")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(cfg.daemon.enabled_dogs.is_empty());
        assert!(!cfg.daemon.adopted_dogs.contains_key("otel"));
        assert!(!cfg.dog.contains_key("otel"));
    }

    /// fails if `adopted_dog_path` cannot see an entry `adopt_dog` wrote
    /// (the read `commands::dogs::rehome` needs before `rehome_dog` erases
    /// it), or if it invents a path for a name it never recorded — a
    /// built-in dog, or one this document has never heard of.
    #[test]
    fn adopted_dog_path_reads_what_adopt_dog_wrote_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        ShepToml::edit(&path, |doc| {
            doc.enable_dog("metrics"); // built-in: no `adopted_dogs` entry at all
            doc.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));

            assert_eq!(
                doc.adopted_dog_path("otel"),
                Some(PathBuf::from("/usr/local/bin/shep-otel"))
            );
            assert_eq!(doc.adopted_dog_path("metrics"), None);
            assert_eq!(doc.adopted_dog_path("ghost"), None);
        })
        .unwrap();
    }

    /// A missing `shep.toml` is not an error — `edit` opens it as an empty
    /// document and creates the file (and `$SHEP_HOME` itself).
    #[test]
    fn a_missing_file_opens_empty_and_edit_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("shep.toml");
        ShepToml::edit(&path, |doc| doc.enable_dog("metrics")).unwrap();
        assert!(path.exists());
    }

    /// fails if `set_style_level` writes a spelling `DaemonConfig::load`'s
    /// own `[style]` reader can't parse back for any of the three levels —
    /// the whole point of writing `level`'s own `Display` string is that
    /// the round trip needs no hand-written expectation to agree with.
    #[test]
    fn setting_a_style_level_round_trips_through_daemon_config() {
        for level in [StyleLevel::Full, StyleLevel::Plain, StyleLevel::Bare] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("shep.toml");
            ShepToml::try_edit(&path, |doc| doc.set_style_level(level)).unwrap();
            let written = std::fs::read_to_string(&path).unwrap();
            let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
            assert_eq!(cfg.style.level.as_deref(), Some(level.to_string().as_str()));
        }
    }

    /// fails if setting a style level round-trips through a plain
    /// `toml::Table` the way `enabling_a_dog_leaves_the_rest_of_the_file_
    /// exactly_as_it_was` guards against for dogs — an operator's
    /// `shep.toml` is hand-written, and `shep style` touching only
    /// `[style]` is the whole point of `ShepToml` over `toml::to_string`.
    #[test]
    fn setting_a_style_level_leaves_the_rest_of_the_file_exactly_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        let original = "# the shepherd's own knobs\n[daemon]\nlog_level = \"info\"  # chatty\nlog_json = false\n";
        std::fs::write(&path, original).unwrap();

        ShepToml::try_edit(&path, |doc| doc.set_style_level(StyleLevel::Plain)).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# the shepherd's own knobs"));
        assert!(written.contains("# chatty"));
        assert!(
            written.find("log_level").unwrap() < written.find("log_json").unwrap(),
            "key order survives"
        );

        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert_eq!(cfg.style.level.as_deref(), Some("plain"));
    }

    /// fails if a second `shep style` appends a second `level` key rather
    /// than replacing the first — `toml_edit::Table::insert` on an
    /// existing key is supposed to do the latter, but nothing pinned that
    /// this setter actually relies on that rather than, say, always going
    /// through `entry().or_insert_with()`, which would leave the first
    /// value in place forever.
    #[test]
    fn setting_a_style_level_twice_replaces_rather_than_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        ShepToml::try_edit(&path, |doc| doc.set_style_level(StyleLevel::Full)).unwrap();
        ShepToml::try_edit(&path, |doc| doc.set_style_level(StyleLevel::Bare)).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("level").count(), 1, "one key, not appended");
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert_eq!(cfg.style.level.as_deref(), Some("bare"));
    }

    /// A `$SHEP_HOME` with no `shep.toml` at all is the common case for a
    /// first `shep style <level>`, and it must create one rather than
    /// refusing — the same behaviour `a_missing_file_opens_empty_and_edit_
    /// creates_it` pins for `enable_dog`.
    #[test]
    fn setting_a_style_level_into_a_home_with_no_shep_toml_creates_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        assert!(!path.exists());

        ShepToml::try_edit(&path, |doc| doc.set_style_level(StyleLevel::Bare)).unwrap();

        assert!(path.exists());
        let cfg =
            DaemonConfig::load(Some(&std::fs::read_to_string(&path).unwrap()), &|_| None).unwrap();
        assert_eq!(cfg.style.level.as_deref(), Some("bare"));
    }

    /// The starter mapping is active, not commented into inertness: task
    /// 47's whole point is that a fresh `$SHEP_HOME` can run `shep start
    /// server.js` without any further setup, and a mapping nobody has
    /// uncommented cannot do that.
    #[test]
    fn the_starter_interpreters_are_written_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");

        ShepToml::edit(&path, |doc| doc.write_starter_interpreters()).unwrap();

        let cfg =
            DaemonConfig::load(Some(&std::fs::read_to_string(&path).unwrap()), &|_| None).unwrap();
        assert_eq!(cfg.interpreters.get("js").map(String::as_str), Some("node"));
        assert_eq!(
            cfg.interpreters.get("mjs").map(String::as_str),
            Some("node")
        );
        assert_eq!(
            cfg.interpreters.get("cjs").map(String::as_str),
            Some("node")
        );
        assert_eq!(
            cfg.interpreters.get("py").map(String::as_str),
            Some("python3")
        );
        assert_eq!(cfg.interpreters.get("rb").map(String::as_str), Some("ruby"));
        assert_eq!(cfg.interpreters.get("sh").map(String::as_str), Some("sh"));
    }

    /// The mapping is visible and editable, not hidden magic: an
    /// explanatory comment sits right above the table it writes, in the
    /// same file an operator already has open.
    #[test]
    fn the_starter_interpreters_carry_an_explanatory_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");

        ShepToml::edit(&path, |doc| doc.write_starter_interpreters()).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# Extension -> interpreter mapping"),
            "no explanatory comment above [interpreters]:\n{written}"
        );
        assert!(
            written.find("# Extension -> interpreter mapping").unwrap()
                < written.find("[interpreters]").unwrap(),
            "the comment must precede the table it explains:\n{written}"
        );
        assert!(
            !written.contains('\u{2014}') && !written.contains('\u{2013}'),
            "no em or en dashes in copy an operator reads:\n{written}"
        );
    }

    /// fails if a second scaffold (a second `ensure_home_at` for whatever
    /// reason, or a caller re-running the first-run hook) appends a
    /// duplicate `[interpreters]` table, or overwrites one an operator has
    /// since edited to their own taste.
    #[test]
    fn writing_the_starter_interpreters_twice_does_not_duplicate_or_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");

        ShepToml::edit(&path, |doc| doc.write_starter_interpreters()).unwrap();
        // An operator's own edit to the mapping this scaffold wrote.
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("js = \"node\"", "js = \"bun\"");
        std::fs::write(&path, &edited).unwrap();

        ShepToml::edit(&path, |doc| doc.write_starter_interpreters()).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written.matches("[interpreters]").count(),
            1,
            "one table, not appended:\n{written}"
        );
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert_eq!(
            cfg.interpreters.get("js").map(String::as_str),
            Some("bun"),
            "the operator's own edit must survive a second scaffold call"
        );
    }

    /// fails if `set_style_level` panics instead of reporting when
    /// `style` already exists as something other than a table: an operator
    /// writing `style = "full"` at the top level (legal TOML, and a natural
    /// guess) must get a clean [`ShepTomlError::WrongShape`], not an
    /// internal assertion aborting the whole process.
    ///
    /// Also fails if a refused write still replaces the file. Routing the
    /// setter through [`ShepToml::edit`], which always calls `save()` after
    /// the closure runs regardless of what the closure returned, would
    /// still stage a fresh file and rename it over the original on a
    /// refusal: identical bytes, but a new inode, and the mode forced to
    /// [`CONFIG_FILE_MODE`] even though the original here is `0644`.
    /// Content equality alone hides that, which is why this checks the
    /// file's metadata rather than only what is in it.
    /// [`ShepToml::try_edit`] is what actually prevents it: it never
    /// reaches `save` when the closure returns `Err`.
    #[test]
    fn a_style_key_that_is_not_a_table_is_reported_and_the_file_is_never_rewritten() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        let original = "style = \"full\"\n";
        std::fs::write(&path, original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = std::fs::metadata(&path).unwrap();

        let err = ShepToml::try_edit(&path, |doc| doc.set_style_level(StyleLevel::Bare))
            .expect_err("style is a string here, not a table");
        assert!(
            matches!(
                &err,
                ShepTomlError::WrongShape { key, found, .. }
                    if *key == "style" && *found == "string"
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "{}: [style] must be a table, found a string",
                path.display()
            )
        );

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a refused write must leave the operator's file exactly as it was"
        );
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            before.ino(),
            after.ino(),
            "a refused write must not replace the file -- same inode, not just same bytes"
        );
        assert_eq!(
            before.mode() & 0o777,
            after.mode() & 0o777,
            "a refused write must not touch the file's mode"
        );
    }

    /// fails if the first `shep enable` on a host that has never booted a
    /// shepherd leaves either the file or `$SHEP_HOME` readable by another
    /// local user. This is the file `docs/dogs.md` tells an operator to
    /// paste a Discord webhook URL into, and `boot::init_dirs` — the
    /// force-chmod that would otherwise narrow the directory — does not run
    /// until the first `shep muster`, which may be much later or never.
    ///
    /// Both modes are asserted on a path where neither the directory nor
    /// the file existed beforehand, because that is the case the ambient
    /// umask would decide.
    #[test]
    fn a_first_edit_creates_the_home_and_the_file_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("cold");
        let path = home.join("shep.toml");

        ShepToml::edit(&path, |doc| doc.enable_dog("bark")).unwrap();

        assert_eq!(
            mode_of(&home),
            0o700,
            "$SHEP_HOME is readable by other local users until the first boot"
        );
        assert_eq!(
            mode_of(&path),
            0o600,
            "the file a webhook token goes in, and the mode a `tar` of it keeps"
        );
    }

    /// fails if an existing `shep.toml` left wide by an older shep (or by
    /// an operator's own `touch`) stays wide after this writer replaces it.
    /// The rename installs the staging file's inode, mode included, so the
    /// narrowing is a property of the write path rather than a chmod pass
    /// somebody has to remember to run.
    #[test]
    fn editing_a_world_readable_config_leaves_it_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        ShepToml::edit(&path, |doc| doc.enable_dog("bark")).unwrap();

        assert_eq!(mode_of(&path), 0o600);
    }

    /// The redaction IR-41 requires: `Debug` on a parse failure carries the
    /// path and the parser's short message, never the document
    /// `toml_edit::TomlError` quotes to render its own `Display` — a
    /// `[dog.bark]` table sitting next to the syntax error would otherwise
    /// put a webhook token in `{:?}` output.
    #[test]
    fn parse_error_debug_never_prints_the_document() {
        let path = PathBuf::from("/home/ada/.shep/shep.toml");
        let secret = "https://hooks.example.com/services/T00/B00/super-secret-token";
        let broken = format!("[dog.bark]\nwebhook = \"{secret}\"\n[daemon\n");
        let source = broken.parse::<DocumentMut>().unwrap_err();
        let err = ShepTomlError::Parse { path, source };

        let debug = format!("{err:?}");
        assert!(
            !debug.contains(secret),
            "the document must never reach Debug: {debug}"
        );
        assert!(!debug.contains("webhook"), "{debug}");
        assert!(!debug.contains("hooks.example.com"), "{debug}");
        assert_eq!(
            debug,
            "Parse { path: \"/home/ada/.shep/shep.toml\", message: \"invalid table header\\n\
             expected `.`, `]`\" }"
        );

        // `Display`, unlike `Debug`, is the deliberate surface an operator
        // reads to find their own typo — it still shows the offending line.
        let display = err.to_string();
        assert!(display.contains("invalid table header"));
    }

    /// Env var naming the `shep.toml` the re-executed child should edit.
    /// Its presence is also what tells the child it is a child.
    const CHILD_PATH_VAR: &str = "SHEP_CONFIG_RACE_PATH";
    /// Env var carrying the child's tag, which decides both which verb's
    /// edit it makes and what it names the dogs it writes.
    const CHILD_TAG_VAR: &str = "SHEP_CONFIG_RACE_TAG";
    /// How many edits each of the two writers makes. One apiece would race
    /// only in the instant the two overlap; this many makes an unlocked
    /// read-modify-write lose an edit on essentially every run.
    const EDITS_PER_WRITER: usize = 100;
    /// The tag whose child adopts (`[daemon] adopted_dogs` plus
    /// `enabled_dogs`); the other enables (`enabled_dogs` alone). Two
    /// different edits, so a survivor of one cannot stand in for the other.
    const ADOPTING_TAG: &str = "alpha";

    /// Not a test — the child half of
    /// [`two_writer_processes_do_not_lose_each_other_s_edits`], which
    /// re-executes this binary with `--ignored --exact` to reach it. It is
    /// `#[ignore]`d so a normal run never picks it up, and it asserts
    /// nothing: its job is to hammer [`ShepToml::edit`] from a second OS
    /// process, and the parent does the judging.
    #[test]
    #[ignore = "child process of two_writer_processes_do_not_lose_each_other_s_edits"]
    fn config_race_child() {
        let Ok(path) = std::env::var(CHILD_PATH_VAR) else {
            panic!("{CHILD_PATH_VAR} unset — this test is only run as a child process");
        };
        let tag = std::env::var(CHILD_TAG_VAR).expect("child needs a tag");
        let path = PathBuf::from(path);

        for i in 0..EDITS_PER_WRITER {
            let name = format!("{tag}-{i}");
            ShepToml::edit(&path, |doc| {
                if tag == ADOPTING_TAG {
                    doc.adopt_dog(&name, Path::new("/usr/local/bin/shep-otel"));
                } else {
                    doc.enable_dog(&name);
                }
            })
            .expect("child edit");
        }
    }

    /// fails if two `shep` processes editing one `shep.toml` lose each
    /// other's edits — `shep adopt otel ... & shep enable metrics &` out of
    /// a provisioning script, where both read the pre-edit document and
    /// both write the whole thing back.
    ///
    /// Two OS processes, not two threads: this document is read and written
    /// by whole `shep` invocations, so any in-process serialisation would
    /// prove nothing about the bug, which is a read-modify-write across a
    /// `rename` with no lock between address spaces. `barks.jsonl` had the
    /// identical shape and lost half its records to it.
    #[test]
    fn two_writer_processes_do_not_lose_each_other_s_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        let exe = std::env::current_exe().expect("test binary path");

        let children: Vec<_> = [ADOPTING_TAG, "beta"]
            .iter()
            .map(|tag| {
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "--ignored",
                        "commands::shep_toml::tests::config_race_child",
                    ])
                    .env(CHILD_PATH_VAR, &path)
                    .env(CHILD_TAG_VAR, tag)
                    // Piped, not inherited: a passing run should not
                    // interleave two child harnesses' output into this
                    // one's, and a failing child's harness output is
                    // exactly what the assertion below needs to show.
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn writer")
            })
            .collect();

        for child in children {
            let out = child.wait_with_output().expect("wait for writer");
            assert!(
                out.status.success(),
                "a writer process failed: {}\n{}",
                out.status,
                String::from_utf8_lossy(&out.stdout)
            );
        }

        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        for i in 0..EDITS_PER_WRITER {
            let adopted = format!("{ADOPTING_TAG}-{i}");
            let enabled = format!("beta-{i}");
            assert!(
                cfg.daemon.adopted_dogs.contains_key(&adopted),
                "{adopted}: an adopt was overwritten by the other writer"
            );
            assert!(
                cfg.daemon.enabled_dogs.contains(&adopted),
                "{adopted}: the adopt's own enable was overwritten"
            );
            assert!(
                cfg.daemon.enabled_dogs.contains(&enabled),
                "{enabled}: an enable was overwritten by the other writer"
            );
        }
        assert_eq!(
            cfg.daemon.enabled_dogs.len(),
            2 * EDITS_PER_WRITER,
            "the config enables dogs nobody asked for"
        );
    }

    #[test]
    fn taking_dog_sections_returns_them_keyed_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\nbind = \"127.0.0.1:9615\"\n\n[dog.bark.sinks]\noncall = { kind = \"discord\" }\n",
        )
        .expect("write");

        let taken = ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

        assert_eq!(taken.keys().collect::<Vec<_>>(), vec!["bark", "metrics"]);
        assert_eq!(taken["metrics"]["bind"].as_str(), Some("127.0.0.1:9615"));
    }

    #[test]
    fn taking_dog_sections_leaves_every_other_section_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "# keep me\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\nbind = \"127.0.0.1:9615\"\n\n[style]\nlevel = \"full\"\n",
        )
        .expect("write");

        ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

        // Exact string: the whole reason this goes through `toml_edit` rather
        // than a `toml::Table` round-trip is that a comment or a reordered key
        // would be a reason not to run the upgrade.
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "# keep me\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[style]\nlevel = \"full\"\n"
        );
    }

    #[test]
    fn taking_from_a_file_with_no_dog_sections_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shep.toml");
        let before = "[daemon]\nlog_level = \"info\"\n";
        std::fs::write(&path, before).expect("write");

        let taken = ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

        assert!(taken.is_empty());
        // Content identity, not proof that nothing was written: `edit` always
        // stages and renames, so the file has a new inode either way. Not
        // writing at all is the migration's job, and its own early return is
        // where that is tested.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
    }

    #[test]
    fn taking_dog_sections_keeps_nested_tables_and_arrays_of_tables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[dog.bark.sinks]\noncall = { kind = \"discord\", url = \"https://discord.com/api/webhooks/x\" }\n\n[[dog.bark.rules]]\non = \"gave_up\"\nsinks = [\"oncall\"]\n",
        )
        .expect("write");

        let taken = ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

        let bark = &taken["bark"];
        assert_eq!(
            bark["sinks"]["oncall"]["url"].as_str(),
            Some("https://discord.com/api/webhooks/x"),
            "a nested sub-table's own values must survive the take"
        );
        let rules = bark["rules"]
            .as_array()
            .expect("rules is an array of tables");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["on"].as_str(), Some("gave_up"));
        assert_eq!(
            rules[0]["sinks"][0].as_str(),
            Some("oncall"),
            "the array-of-tables entry keeps its own array field"
        );
    }

    #[test]
    fn taking_dog_sections_keeps_an_inline_table_dog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[dog]\nmetrics = { bind = \"127.0.0.1:9615\" }\n").expect("write");

        let taken = ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

        assert_eq!(
            taken["metrics"]["bind"].as_str(),
            Some("127.0.0.1:9615"),
            "an inline-table dog under [dog] must not be dropped"
        );
    }
}
