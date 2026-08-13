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
//! Every edit goes through [`ShepToml::edit`], which is the whole write
//! path: `$SHEP_HOME` created at `0700`, an exclusive advisory lock on a
//! sibling `shep.toml.lock` held across the read-modify-write, and the new
//! document staged in a `0600` temp file, `fsync`ed and `rename`d over the
//! original. Each of those three is the same shape `shep-core`'s
//! `barks::append` already uses, for the same reasons and after the same
//! bug: two writers racing on `barks.jsonl` silently lost half of each
//! other's records until an advisory lock landed there.
//!
//! `#[cfg(unix)]` wholesale, at `commands`' own declaration in `main.rs` —
//! `flock(2)` and unix mode bits are both Unix-only, and Windows is shep's
//! 0% tier where no verb runs at all.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// Mode `shep.toml` — and the sibling lock file and staging file it is
/// written through — is created with: owner read/write, nobody else.
///
/// Unlike `barks.jsonl`'s own `0600`, this is not belt-and-braces. This
/// file is where `docs/dogs.md` tells an operator to paste a Discord or
/// Slack webhook URL, and both carry a bearer token in the path, so the
/// mode here is the guard rather than a second one behind `$SHEP_HOME`'s.
/// It is also what a `tar`, a `cp -p` or a backup of `$SHEP_HOME` carries
/// out with the file, somewhere no directory mode follows it.
const CONFIG_FILE_MODE: u32 = 0o600;

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
    /// # Errors
    /// - [`ShepTomlError::Io`] — `$SHEP_HOME` could not be created, the
    ///   lock beside the file could not be taken, or the file could not
    ///   be read or replaced.
    /// - [`ShepTomlError::Parse`] — the file exists and is not valid
    ///   TOML. Refused rather than overwritten, and `f` never runs.
    pub fn edit<T>(path: &Path, f: impl FnOnce(&mut Self) -> T) -> Result<T, ShepTomlError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        create_home_dir(parent).map_err(|source| ShepTomlError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        // Held until this function returns, so the read below and the
        // rename inside `save` are one transaction as far as any other
        // editor is concerned.
        let _lock = ConfigLock::acquire(path).map_err(|source| ShepTomlError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let mut doc = Self::open(path)?;
        let value = f(&mut doc);
        doc.save()?;
        Ok(value)
    }

    /// Reads `path`, treating a missing file as an empty document.
    ///
    /// Private, and reached only from [`Self::edit`] with the document's
    /// lock already held — see that method's own doc.
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

    /// Adds `name` to `[daemon] enabled_dogs` (idempotently) and ensures a
    /// `[dog.<name>]` table exists for the dog to be configured through.
    ///
    /// Never truncates a `[dog.<name>]` table that already exists — a
    /// dog's own configuration is not this writer's to touch, only its
    /// existence.
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
        self.dog_table_mut(name);
    }

    /// Removes `name` from `[daemon] enabled_dogs`, leaving `[dog.<name>]`
    /// in place: an operator who disables a dog to restart it must not lose
    /// the configuration they wrote for it.
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

    /// Forgets `name` entirely: out of `enabled_dogs`, out of
    /// `adopted_dogs`, and `[dog.<name>]` removed. The difference between
    /// `rehome` and `disable`, and the reason they are two verbs.
    ///
    /// Called by `commands::dogs::rehome`.
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

    /// `[dog.<name>]`, creating it (empty) if it does not exist yet — never
    /// touched again once it does, per [`Self::enable_dog`]'s own doc.
    fn dog_table_mut(&mut self, name: &str) -> &mut Table {
        let dog = self
            .doc
            .entry("dog")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .expect("dog is only ever written as a table");
        dog.entry(name)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .expect("a dog's own section is only ever written as a table")
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
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(shep_daemon::boot::DIR_MODE)
        .create(dir)
}

/// Creates the staging file the config is written through, in `parent` so
/// the later `rename` stays within one filesystem.
///
/// Mode-at-creation rather than a separate `chmod` pass (`tempfile` passes
/// these permissions to the `open` call itself): there is no window in
/// which the file holding a webhook token sits at whatever the process
/// umask leaves it. Same shape, and same reasoning, as
/// `barks::create_ring_file`.
fn create_config_file(parent: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix("shep")
        .suffix(".toml.tmp")
        .permissions(std::fs::Permissions::from_mode(CONFIG_FILE_MODE))
        .tempfile_in(parent)
}

/// An exclusive advisory lock over one `shep.toml`, held for as long as
/// the value lives and released when it drops (including on an early `?`,
/// and by the kernel if the process dies holding it).
///
/// The lock is on a sibling `shep.toml.lock`, never on the config itself,
/// and that is the whole design decision — the same one `barks::RingLock`
/// records: [`ShepToml::save`] finishes by `rename`ing a new file over the
/// config, which replaces the inode. A lock taken on the config would be a
/// lock on an inode the very next successful save unlinks; the next writer
/// would open the *new* inode, find it unlocked, and the two would be
/// excluding nothing. The lock file is never renamed, never rewritten and
/// never read; it exists only to be an inode with a stable identity, and
/// it is left on disk between edits on purpose so both writers keep
/// agreeing on which one it is.
struct ConfigLock {
    /// `flock(2)` is released by this handle's `Drop`. Named with a
    /// leading underscore because it is held, never read.
    _flock: nix::fcntl::Flock<std::fs::File>,
}

impl ConfigLock {
    /// Blocks until this process holds `path`'s lock exclusively.
    ///
    /// # Errors
    /// The lock file could not be created beside `path`, or `flock` failed
    /// for a reason other than contention (contention blocks rather than
    /// failing).
    fn acquire(path: &Path) -> std::io::Result<Self> {
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
        }
    }
}

impl std::fmt::Display for ShepTomlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Parse { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl core::error::Error for ShepTomlError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
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
            cfg.dog.contains_key("metrics"),
            "a table to configure it through"
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
        let path = PathBuf::from("/home/rin/.shep/shep.toml");
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
            "Parse { path: \"/home/rin/.shep/shep.toml\", message: \"invalid table header\\n\
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
}
