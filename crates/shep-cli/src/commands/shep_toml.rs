//! `$SHEP_HOME/shep.toml`, the daemon's own config file, read and rewritten
//! by [`ShepToml`], the one writer this binary has for it.
//!
//! Edits go through `toml_edit`'s [`DocumentMut`], so an operator's comments
//! and key order survive. [`shep_core::config::DaemonConfig::load`] decides
//! what a key means; this module only adds or removes the ones each verb owns.
//!
//! [`ShepToml::edit`] and [`ShepToml::try_edit`] are the whole write path:
//! `$SHEP_HOME` at `0700`, an exclusive advisory lock on a sibling
//! `shep.toml.lock` across the read-modify-write, and the document staged
//! `0600`, `fsync`ed and `rename`d. A `try_edit` closure's own `Err` leaves
//! `path` untouched.

// Fires only on Windows: `ShepTomlError` crosses the lint's 128-byte
// threshold there and stays under it elsewhere.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::style::StyleLevel;

/// Extensions [`ShepToml::write_starter_interpreters`] maps, in the order
/// they land in `shep.toml`.
///
/// `py` maps to `python3` rather than bare `python`, which is absent or
/// still points at Python 2 on plenty of hosts shep runs on. `ts` is left
/// out: ts-node, tsx and deno disagree about how to run one, and guessing
/// wrong silently is worse than making the operator say so.
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
/// above the `[interpreters]` table it scaffolds.
///
/// Plain `#` TOML comment lines: this text lands inside `shep.toml` for an
/// operator to read, so the same copy rules govern it as `welcome.rs`'s.
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
/// A missing file is created as an empty document, `$SHEP_HOME` with it; a
/// file that will not parse is refused rather than overwritten, since it may
/// hold every knob a daemon boots with, credentials included.
///
/// [`Self::edit`] and [`Self::try_edit`] are the only paths that write, and
/// they hold the document's lock for exactly as long as the closure runs.
/// Reading and writing are not separate public steps: a caller that could
/// read, think, and then write is the lost update this type takes a lock to
/// prevent.
pub struct ShepToml {
    path: PathBuf,
    doc: DocumentMut,
}

/// Manual, not derived: `doc` can hold a webhook URL with a bearer token in
/// an un-migrated `[dog.<name>]` table, so only the path is printed.
impl std::fmt::Debug for ShepToml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShepToml")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ShepToml {
    /// Reads `path`, hands the document to `f`, and writes it back under
    /// one exclusive advisory lock on a sibling `shep.toml.lock`
    /// ([`ConfigLock`]).
    ///
    /// `f`'s return value comes back on success. `f` is infallible;
    /// [`Self::try_edit`] takes one that can refuse.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] if `$SHEP_HOME` could not be created, the
    ///   lock could not be taken, or the file could not be read or replaced.
    /// - [`ShepTomlError::Parse`] if the file is not valid TOML. Refused
    ///   rather than overwritten, and `f` never runs.
    pub fn edit<T>(path: &Path, f: impl FnOnce(&mut Self) -> T) -> Result<T, ShepTomlError> {
        let (mut doc, _lock) = Self::open_locked(path)?;
        let value = f(&mut doc);
        doc.save()?;
        Ok(value)
    }

    /// Like [`Self::edit`], but for a closure that can itself refuse: `f`'s
    /// own `Err` skips [`Self::save`] entirely. Saving anyway would stage
    /// and rename a byte-identical copy, and that rename still lands a fresh
    /// inode, forces [`shep_core::atomic_file::OWNER_ONLY_FILE_MODE`], and
    /// replaces a symlinked `path` with a plain file.
    ///
    /// `E: From<ShepTomlError>` is what lets `?` cover this method's own
    /// setup failures (home dir, lock, parse) as well as `f`'s.
    ///
    /// # Errors
    /// Everything [`Self::edit`] can fail with, converted through `E::from`,
    /// plus whatever `f` returns as `Err`. `path` is untouched either way.
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
    /// opens the document: the setup [`Self::edit`] and [`Self::try_edit`]
    /// share.
    ///
    /// The returned [`ConfigLock`] must outlive every use of the returned
    /// `Self`. It is what makes this read and the caller's eventual `save`
    /// one transaction as far as any other editor is concerned.
    fn open_locked(path: &Path) -> Result<(Self, ConfigLock), ShepTomlError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        create_home_dir(parent).map_err(|source| ShepTomlError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        let lock = ConfigLock::acquire(path).map_err(|source| ShepTomlError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let doc = Self::open(path)?;
        Ok((doc, lock))
    }

    /// Reads `path`, treating a missing file as an empty document.
    ///
    /// Reached from [`Self::edit`]/[`Self::try_edit`] with the document's
    /// lock held, and from the read-only callers with no lock at all.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] if the file exists and could not be read.
    /// - [`ShepTomlError::Parse`] if the file exists and is not valid TOML.
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

    /// Reads `path` for a caller that only wants the answer: the settings
    /// screen's door into this type, and the shape every reader below is
    /// reached through.
    ///
    /// Takes no lock. [`Self::save`]'s rename onto `path` is atomic, so a
    /// concurrent writer can only make this read observe the document just
    /// before or just after that write, never a torn one.
    ///
    /// # Errors
    /// [`ShepTomlError::Io`] if `path` exists and could not be read.
    /// [`ShepTomlError::Parse`] if `path` exists and is not valid TOML.
    pub fn read_only(path: &Path) -> Result<Self, ShepTomlError> {
        Self::open(path)
    }

    /// Renders the in-memory document exactly as [`Self::save`] would
    /// write it, without touching disk.
    ///
    /// `commands::settings::apply_setting` mutates, renders, and hands the
    /// text to [`DaemonConfig::load`](shep_core::config::DaemonConfig::load)
    /// before calling [`Self::save`], so a refusal never stages or renames.
    ///
    /// A named method rather than a `Display` impl: this text can carry a
    /// dog's webhook token, and `format!("{doc}")` is what a future caller
    /// reaches for without thinking about that.
    #[must_use]
    pub(crate) fn rendered(&self) -> String {
        self.doc.to_string()
    }

    /// Adds `name` to `[daemon] enabled_dogs` (idempotently), and writes
    /// nothing else anywhere.
    ///
    /// Never scaffold an empty `[dog.<name>]` here. A dog's configuration
    /// lives in `dogs.toml`, and `commands::dog_migration` refuses to boot
    /// when one name holds values in both files. An enabled dog with no
    /// section runs on its defaults, so there is nothing to scaffold.
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
    /// That configuration lives in `dogs.toml`, so keeping it takes doing
    /// nothing at all here. [`Self::rehome_dog`] is the half that forgets a
    /// dog for real.
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
    /// Does no vetting of `exec` itself: `commands::dogs::adopt` has already
    /// run `vet_binary`.
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

    /// Removes the whole `[dog]` table and hands back what was under it,
    /// keyed by name with the `dog.` prefix dropped.
    ///
    /// Handed back as live [`Item`]s: a comment an operator wrote around
    /// `[dog.metrics]` travels with the section. Only table-like entries
    /// come back; `[dog] stray = 5` and `[[dog.x]]` are dropped. A document
    /// with no `[dog]` table yields an empty map, left byte-identical.
    ///
    /// Takes everything: a partial move would leave one key readable from
    /// two files.
    pub fn take_dog_sections(&mut self) -> BTreeMap<String, Item> {
        let Some(item) = self.doc.remove("dog") else {
            return BTreeMap::new();
        };
        // `[[dog]]` itself, the one shape with nothing table-like under it
        // to iterate.
        let Some(dog) = item.as_table_like() else {
            return BTreeMap::new();
        };
        dog.iter()
            .filter(|(_, value)| value.as_table_like().is_some())
            .map(|(name, value)| (name.to_owned(), value.clone()))
            .collect()
    }

    /// The binary path recorded for `name` in `[daemon] adopted_dogs`, if
    /// any. `None` for a built-in dog, or a name this document never heard of.
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

    /// The names in `[daemon] enabled_dogs`, in file order.
    ///
    /// Distinct from [`Self::adopted_dog_names`]: a dog can be adopted and
    /// not enabled, or built in and enabled without ever being adopted.
    #[must_use]
    pub fn enabled_dog_names(&self) -> Vec<String> {
        self.table("daemon")
            .and_then(|daemon| daemon.get("enabled_dogs"))
            .and_then(Item::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// [`Self::adopted_dog_path`] without the write side, for `lib.rs`'s
    /// `dispatch_adopted_dog`, which runs on every unrecognized verb.
    ///
    /// Creates nothing: a missing `$SHEP_HOME` or `path` is an ordinary "no
    /// such dog" answer, never a reason to create either. Takes no lock,
    /// for the reason [`Self::read_only`] gives.
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
    /// `shep.toml` still carries one.
    ///
    /// Half of a rehome. Striking the dog's configuration in `dogs.toml` is
    /// `commands::dog_migration::forget_dog_section`, called right after
    /// this: one file per writer, since this type owns `shep.toml` and only
    /// that.
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

    /// Writes `[style] level = "<level>"`, creating the `[style]` table when
    /// this document has none yet.
    ///
    /// The value is `level`'s own `Display` spelling, the same string
    /// `style_from_config` (`lib.rs`) parses back through
    /// `clap::ValueEnum::from_str`, so a round trip stays one grammar.
    ///
    /// # Errors
    /// [`ShepTomlError::WrongShape`] if `style` is already there as
    /// something other than a table, e.g. a hand-written `style = "full"` at
    /// the top level. Reported rather than `expect`ed, since that shape
    /// comes from data this process does not control.
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

    /// `[style] level`, or `None` when the document never wrote it, as the
    /// raw string on disk.
    ///
    /// `[style]` is the one settings field whose value in force can come
    /// from a layer above the file (`--style`, `$SHEP_STYLE`), so the
    /// resolved level and the level the document declares are two different
    /// facts.
    #[must_use]
    pub fn style_level(&self) -> Option<String> {
        self.table("style")?
            .get("level")?
            .as_str()
            .map(String::from)
    }

    /// `[daemon] log_json`, or `None` when the document never wrote it.
    ///
    /// A key written to its own default is still `Some`.
    /// [`DaemonConfig::load`]'s `#[serde(default)]` loses that distinction,
    /// so this reader and its four siblings below read the document itself.
    #[must_use]
    pub fn daemon_log_json(&self) -> Option<bool> {
        self.table("daemon")?.get("log_json")?.as_bool()
    }

    /// `[daemon] log_level`, or `None` when the document never wrote it,
    /// as the raw string on disk. Whether it names a real `LogLevel` is
    /// [`DaemonConfig::load`]'s question, not this reader's.
    #[must_use]
    pub fn daemon_log_level(&self) -> Option<String> {
        self.table("daemon")?
            .get("log_level")?
            .as_str()
            .map(String::from)
    }

    /// `[daemon] socket`, or `None` when the document never wrote it.
    #[must_use]
    pub fn daemon_socket(&self) -> Option<PathBuf> {
        self.table("daemon")?
            .get("socket")?
            .as_str()
            .map(PathBuf::from)
    }

    /// `[daemon] max_cron_sleep`, or `None` when the document never wrote
    /// it, as the raw string on disk rather than a parsed `UpDuration`:
    /// parsing here would put a second opinion about the grammar next to
    /// `DaemonConfig`'s.
    #[must_use]
    pub fn daemon_max_cron_sleep(&self) -> Option<String> {
        self.table("daemon")?
            .get("max_cron_sleep")?
            .as_str()
            .map(String::from)
    }

    /// `[whistle] allow_control`, or `None` when the document never wrote
    /// it.
    #[must_use]
    pub fn whistle_allow_control(&self) -> Option<bool> {
        self.table("whistle")?.get("allow_control")?.as_bool()
    }

    /// Writes `[daemon] log_json = <value>`, creating `[daemon]` when this
    /// document has none yet.
    ///
    /// # Errors
    /// [`ShepTomlError::WrongShape`] if `daemon` is already there as
    /// something other than a table.
    pub fn set_daemon_log_json(&mut self, value: bool) -> Result<(), ShepTomlError> {
        self.section_table_mut("daemon")?
            .insert("log_json", Item::Value(value.into()));
        Ok(())
    }

    /// Writes `[daemon] log_level = "<value>"`, creating `[daemon]` when
    /// this document has none yet. `value` is written as given, unchecked:
    /// [`DaemonConfig::load`] is what refuses a name that is not a real
    /// `LogLevel`.
    ///
    /// # Errors
    /// [`ShepTomlError::WrongShape`] if `daemon` is already there as
    /// something other than a table.
    pub fn set_daemon_log_level(&mut self, value: &str) -> Result<(), ShepTomlError> {
        self.section_table_mut("daemon")?
            .insert("log_level", Item::Value(value.into()));
        Ok(())
    }

    /// Writes `[daemon] socket = "<value>"`, creating `[daemon]` when this
    /// document has none yet.
    ///
    /// # Errors
    /// [`ShepTomlError::WrongShape`] if `daemon` is already there as
    /// something other than a table.
    pub fn set_daemon_socket(&mut self, value: &Path) -> Result<(), ShepTomlError> {
        self.section_table_mut("daemon")?.insert(
            "socket",
            Item::Value(value.to_string_lossy().into_owned().into()),
        );
        Ok(())
    }

    /// Writes `[daemon] max_cron_sleep = "<value>"`, creating `[daemon]`
    /// when this document has none yet. `value` is written as given,
    /// unchecked: [`DaemonConfig::load`] is what refuses a duration below
    /// the floor or one that does not parse at all.
    ///
    /// # Errors
    /// [`ShepTomlError::WrongShape`] if `daemon` is already there as
    /// something other than a table.
    pub fn set_daemon_max_cron_sleep(&mut self, value: &str) -> Result<(), ShepTomlError> {
        self.section_table_mut("daemon")?
            .insert("max_cron_sleep", Item::Value(value.into()));
        Ok(())
    }

    /// Writes `[whistle] allow_control = <value>`, creating `[whistle]`
    /// when this document has none yet.
    ///
    /// # Errors
    /// [`ShepTomlError::WrongShape`] if `whistle` is already there as
    /// something other than a table.
    pub fn set_whistle_allow_control(&mut self, value: bool) -> Result<(), ShepTomlError> {
        self.section_table_mut("whistle")?
            .insert("allow_control", Item::Value(value.into()));
        Ok(())
    }

    /// Removes `[daemon] socket` if it is set, and does nothing when
    /// `[daemon]` is absent or is not a table. No `Result`: removing a key
    /// from something that is not a table is already a no-op.
    pub fn unset_daemon_socket(&mut self) {
        if let Some(daemon) = self.doc.get_mut("daemon").and_then(Item::as_table_mut) {
            daemon.remove("socket");
        }
    }

    /// Removes `[daemon] max_cron_sleep` if it is set.
    pub fn unset_daemon_max_cron_sleep(&mut self) {
        if let Some(daemon) = self.doc.get_mut("daemon").and_then(Item::as_table_mut) {
            daemon.remove("max_cron_sleep");
        }
    }

    /// Writes the starter `[interpreters]` mapping, a script extension to
    /// the interpreter shep runs it with, under
    /// `INTERPRETERS_STARTER_COMMENT`.
    ///
    /// Written live rather than commented out: shep never infers an
    /// interpreter on its own, and a fresh `$SHEP_HOME` still has to run the
    /// `shep start server.js` that `welcome.rs` and `--help` advertise.
    ///
    /// A no-op when `[interpreters]` already exists, so a hand-edited
    /// `shep.toml` is never clobbered or duplicated.
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
    /// [`shep_core::atomic_file::OWNER_ONLY_FILE_MODE`], `fsync`ed,
    /// `rename`d over `path`, then the directory `fsync`ed so the rename
    /// survives a power cut.
    ///
    /// Not `std::fs::write`: its `O_TRUNC` would leave an operator's whole
    /// `shep.toml` empty on a crash between truncate and write. The rename
    /// also re-tightens a `shep.toml` an older shep left at `0644`.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] if the staging file could not be written, the
    ///   rename over `path` failed, or the directory could not be flushed.
    fn save(&self) -> Result<(), ShepTomlError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = create_config_file(parent).map_err(|source| self.io_error(source))?;
        tmp.write_all(self.doc.to_string().as_bytes())
            .map_err(|source| self.io_error(source))?;
        tmp.as_file()
            .sync_all()
            .map_err(|source| self.io_error(source))?;
        // `persist` is `rename(2)`. On failure the `NamedTempFile` comes back
        // inside the error and its `Drop` removes the staging file.
        tmp.persist(&self.path)
            .map_err(|err| self.io_error(err.error))?;

        // `sync_all` above made the contents durable; this makes the rename
        // that published them durable.
        shep_core::atomic_file::sync_dir(parent).map_err(|source| self.io_error(source))?;
        Ok(())
    }

    fn io_error(&self, source: std::io::Error) -> ShepTomlError {
        ShepTomlError::Io {
            path: self.path.clone(),
            source,
        }
    }

    /// `section` as a table, or `None` if this document never wrote it or
    /// wrote it as something else. The read side every scalar reader above
    /// shares: a reader has nothing to refuse, unlike a setter.
    fn table(&self, section: &str) -> Option<&Table> {
        self.doc.get(section).and_then(Item::as_table)
    }

    /// `section` as a table, creating it empty if this document has none
    /// yet, and refusing with [`ShepTomlError::WrongShape`] if `section` is
    /// already occupied by something else. The write side every scalar
    /// setter above shares.
    fn section_table_mut(&mut self, section: &'static str) -> Result<&mut Table, ShepTomlError> {
        let item = self
            .doc
            .entry(section)
            .or_insert_with(|| Item::Table(Table::new()));
        let found = item.type_name();
        item.as_table_mut().ok_or(ShepTomlError::WrongShape {
            path: self.path.clone(),
            key: section,
            found,
        })
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
/// `$SHEP_HOME` holds webhook URLs, and on a host that has never booted a
/// shepherd this call is the one that creates it: `boot::init_dirs`, which
/// force-chmods it, does not run until the first `shep muster`. A
/// `create_dir_all` would leave it at the ambient umask, typically `0755`,
/// until that boot, and asking for the mode at `mkdir` time leaves no window
/// in which the directory exists wider.
///
/// Reuses `shep_daemon::boot::DIR_MODE` rather than restating `0o700`.
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
/// The pair of names stays in this wrapper because
/// `commands::dog_migration` writes `dogs.toml` through the same staging
/// name as `shep.toml`, and two call sites spelling it out would drift.
pub(super) fn create_config_file(parent: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    shep_core::atomic_file::create_staging_file(parent, "shep", ".toml.tmp")
}

/// An exclusive advisory lock over one config file, released on drop or by
/// the kernel if the process dies holding it.
///
/// Keyed on the path it is given: [`ShepToml::edit`] locks `shep.toml`,
/// `commands::dog_migration` locks `dogs.toml`. `shep.toml`'s is always
/// taken first, to avoid deadlocking the two orderings.
///
/// On a sibling `<name>.lock`, never the config itself: [`ShepToml::save`]
/// renames a new file over the config, so a lock on the config would lock
/// an inode the next save unlinks. The lock file is never renamed,
/// rewritten or read, and stays on disk between edits.
pub(super) struct ConfigLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a
    /// leading underscore because it is held, never read.
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// The lock file, opened with `share_mode(0)`: the same primitive
    /// [`shep_core::kv`] and [`shep_core::barks`] use.
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
            .mode(shep_core::atomic_file::OWNER_ONLY_FILE_MODE)
            .open(lock_path(path))?;

        // `LockExclusive` blocks: a `shep enable` waiting its turn behind a
        // concurrent `shep adopt` is the behaviour wanted here.
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

/// What [`ShepToml::edit`] can fail with.
///
/// Not `#[non_exhaustive]`: nothing outside this binary can match on it, and
/// this crate's own exhaustive matches are the ones the compiler should
/// break when a new failure mode lands.
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
    /// `path` parses, but `key` is already there as something other than a
    /// table, e.g. `style = "full"` at the top level instead of `[style]`.
    /// Legal TOML, but forcing it to a table would discard what the operator
    /// wrote there.
    WrongShape {
        /// The file that holds the wrongly-shaped value.
        path: PathBuf,
        /// The table key that was expected.
        key: &'static str,
        /// What TOML found there ([`Item::type_name`]); never `"table"`.
        found: &'static str,
    },
}

/// Manual, not derived: `toml_edit::TomlError` keeps the whole source
/// document for `Display`'s line-and-column rendering, so a derived `Debug`
/// would print `shep.toml` in full, secrets included. `Debug` is what a log
/// captures, so it carries the path and the parser's short `message()` only;
/// `Display` still shows the full message, for the operator to read.
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

// unix only: asserts a `0600` mode and an inode preserved across an atomic
// rename. Windows differs; `tests/cli_e2e.rs` covers that tier.
#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use shep_core::config::DaemonConfig;

    use super::*;

    /// `path`'s permission bits, masked to the nine that matter.
    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// fails if the writer round-trips through a plain `toml::Table`,
    /// losing comments and key order.
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

    /// Three places have to be empty afterwards: `[daemon] adopted_dogs`,
    /// `enabled_dogs`, and `[dog.<name>]`.
    #[test]
    fn rehoming_a_dog_forgets_it_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        // Seeded by hand: no writer here creates a `[dog.<name>]` any more,
        // but an un-migrated file carries one.
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

    #[test]
    fn a_missing_file_opens_empty_and_edit_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("shep.toml");
        ShepToml::edit(&path, |doc| doc.enable_dog("metrics")).unwrap();
        assert!(path.exists());
    }

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

    /// Active, not commented out: a fresh `$SHEP_HOME` has to run
    /// `shep start server.js` with no further setup.
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

    /// The inode and mode checks are the point: [`ShepToml::edit`] would
    /// stage and rename a byte-identical copy on a refusal, which content
    /// equality alone hides. [`ShepToml::try_edit`] never reaches `save`
    /// when the closure returns `Err`.
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

    #[test]
    fn a_document_with_no_daemon_section_reads_every_scalar_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[interpreters]\njs = \"node\"\n").unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_log_json(), None);
        assert_eq!(cfg.daemon_log_level(), None);
        assert_eq!(cfg.daemon_socket(), None);
        assert_eq!(cfg.daemon_max_cron_sleep(), None);
        assert_eq!(cfg.whistle_allow_control(), None);
    }

    /// The distinction the screen rests on: a key written to its own default is
    /// not the same fact as a key nobody wrote, and `DaemonConfig::load` cannot
    /// tell them apart because every section is `serde(default)`.
    #[test]
    fn a_scalar_written_to_its_default_still_reads_as_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nlog_level = \"warn\"\nlog_json = false\n").unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_log_level().as_deref(), Some("warn"));
        assert_eq!(cfg.daemon_log_json(), Some(false));
    }

    #[test]
    fn a_missing_file_reads_as_an_empty_document_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_log_level(), None);
        assert!(!path.exists(), "a read must never create the file");
    }

    #[test]
    fn setting_a_scalar_keeps_the_comments_and_the_keys_around_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "# keep me\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[style]\nlevel = \"full\"\n",
        )
        .unwrap();

        ShepToml::try_edit(&path, |cfg| cfg.set_daemon_log_level("debug")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# keep me\n"), "got: {text}");
        assert!(text.contains("enabled_dogs = [\"metrics\"]"), "got: {text}");
        assert!(text.contains("level = \"full\""), "got: {text}");
        assert!(text.contains("log_level = \"debug\""), "got: {text}");

        // Substring checks can't tell which section `log_level = "debug"`
        // landed under; `daemon_log_level` reads `[daemon]` specifically.
        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_log_level().as_deref(), Some("debug"));
    }

    #[test]
    fn unsetting_removes_the_key_and_leaves_its_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nlog_level = \"debug\"\nmax_cron_sleep = \"30s\"\n",
        )
        .unwrap();

        ShepToml::edit(&path, ShepToml::unset_daemon_max_cron_sleep).unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_max_cron_sleep(), None);
        assert_eq!(cfg.daemon_log_level().as_deref(), Some("debug"));
    }

    #[test]
    fn a_daemon_key_of_the_wrong_shape_is_refused_rather_than_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "daemon = \"loud\"\n").unwrap();

        // `try_edit`, not `edit`: `edit` always saves, which would stage a
        // byte-identical copy despite the refusal.
        let refusal: Result<(), ShepTomlError> =
            ShepToml::try_edit(&path, |cfg| cfg.set_daemon_log_json(true));

        assert!(matches!(
            refusal,
            Err(ShepTomlError::WrongShape { key: "daemon", .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "daemon = \"loud\"\n"
        );
    }

    /// `enabled_dog_names` is `adopted_dog_names`'s sibling and was left
    /// untouched by the brief's own six tests. A dog can be adopted and not
    /// enabled, or (for a built-in) enabled without ever being adopted, so
    /// this reads `[daemon] enabled_dogs` specifically, in file order.
    #[test]
    fn enabled_dog_names_reads_the_array_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nenabled_dogs = [\"metrics\", \"bark\"]\n").unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.enabled_dog_names(), vec!["metrics", "bark"]);
    }

    /// A document with no `[daemon] enabled_dogs` at all reads as empty,
    /// never a panic or a default entry invented on its behalf.
    #[test]
    fn enabled_dog_names_is_empty_when_the_document_never_wrote_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nlog_level = \"debug\"\n").unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.enabled_dog_names(), Vec::<String>::new());
    }

    /// `set_daemon_socket` has no caller until the settings screen lands,
    /// so this is the only thing that exercises it before then. Reads back
    /// through `daemon_socket`, which is what actually pins that the value
    /// landed under `[daemon] socket` rather than merely appearing
    /// somewhere in the file (the gap `setting_a_scalar_keeps_the_comments_
    /// and_the_keys_around_it` had before this same fix round).
    #[test]
    fn setting_the_socket_reads_back_through_daemon_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nlog_level = \"debug\"\n").unwrap();

        ShepToml::try_edit(&path, |cfg| {
            cfg.set_daemon_socket(Path::new("/tmp/shep.sock"))
        })
        .unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_socket(), Some(PathBuf::from("/tmp/shep.sock")));
        assert_eq!(cfg.daemon_log_level().as_deref(), Some("debug"));
    }

    /// `unset_daemon_socket`'s own sibling test, pinning the same thing
    /// `unsetting_removes_the_key_and_leaves_its_neighbours` pins for
    /// `max_cron_sleep`: the key goes, its neighbours stay.
    #[test]
    fn unsetting_the_socket_removes_the_key_and_leaves_its_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nlog_level = \"debug\"\nsocket = \"/tmp/shep.sock\"\n",
        )
        .unwrap();

        ShepToml::edit(&path, ShepToml::unset_daemon_socket).unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_socket(), None);
        assert_eq!(cfg.daemon_log_level().as_deref(), Some("debug"));
    }

    /// `set_daemon_max_cron_sleep` has no caller until the settings screen
    /// lands. Reads back through `daemon_max_cron_sleep`, the raw string as
    /// written, not a parsed `UpDuration`.
    #[test]
    fn setting_max_cron_sleep_reads_back_through_daemon_max_cron_sleep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nlog_level = \"debug\"\n").unwrap();

        ShepToml::try_edit(&path, |cfg| cfg.set_daemon_max_cron_sleep("45s")).unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.daemon_max_cron_sleep().as_deref(), Some("45s"));
        assert_eq!(cfg.daemon_log_level().as_deref(), Some("debug"));
    }

    /// `set_whistle_allow_control` has no caller until the settings screen
    /// lands. Reads back through `whistle_allow_control`, which is what
    /// pins the value under `[whistle]` rather than `[daemon]`.
    #[test]
    fn setting_whistle_allow_control_reads_back_through_whistle_allow_control() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\nlog_level = \"debug\"\n").unwrap();

        ShepToml::try_edit(&path, |cfg| cfg.set_whistle_allow_control(true)).unwrap();

        let cfg = ShepToml::read_only(&path).unwrap();
        assert_eq!(cfg.whistle_allow_control(), Some(true));
        assert_eq!(cfg.daemon_log_level().as_deref(), Some("debug"));
    }

    /// Both modes are asserted on a path where neither the directory nor
    /// the file existed beforehand: that is the case the ambient umask
    /// would decide.
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

    /// The rename installs the staging file's inode, mode included:
    /// narrowing is a property of the write path, not a separate chmod.
    #[test]
    fn editing_a_world_readable_config_leaves_it_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(&path, "[daemon]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        ShepToml::edit(&path, |doc| doc.enable_dog("bark")).unwrap();

        assert_eq!(mode_of(&path), 0o600);
    }

    /// `Debug` carries only the path and the parser's short message, never
    /// the document `Display` quotes: a webhook in `[dog.bark]` must not
    /// reach `{:?}` output.
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

        // `Display` is what an operator reads for a typo; it still shows
        // the offending line.
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

    /// Child half of [`two_writer_processes_do_not_lose_each_other_s_edits`],
    /// re-executed with `--ignored --exact`. Hammers [`ShepToml::edit`] from
    /// a second OS process; asserts nothing itself, the parent judges.
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

    /// Two OS processes, not threads: the race is a read-modify-write
    /// across a `rename` with no lock between address spaces, which
    /// in-process serialisation cannot reproduce.
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

        // Exact string: a `toml::Table` round-trip would drop the comment
        // or reorder the key.
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
        // `as_array_of_tables`, not `as_array`: `[[dog.bark.rules]]` is a
        // `toml_edit::ArrayOfTables`, a document construct, where `sinks =
        // ["oncall"]` below is a `Value::Array`.
        let rules = bark["rules"]
            .as_array_of_tables()
            .expect("rules is an array of tables");
        assert_eq!(rules.len(), 1);
        let rule = rules.get(0).expect("one rule");
        assert_eq!(rule["on"].as_str(), Some("gave_up"));
        assert_eq!(
            rule["sinks"]
                .as_array()
                .and_then(|sinks| sinks.get(0))
                .and_then(toml_edit::Value::as_str),
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
