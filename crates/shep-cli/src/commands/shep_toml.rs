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

use std::path::{Path, PathBuf};

use toml_edit::{Array, DocumentMut, Item, Table, Value};

/// The one writer of `$SHEP_HOME/shep.toml` in this binary.
///
/// A missing file is created (as an empty document — [`Self::save`] makes
/// the directory too, if needed); a file that will not parse is refused
/// rather than overwritten, because it may hold every knob a daemon boots
/// with, credentials included, and there is no undo for losing it to a
/// typo'd verb.
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
    /// Reads `path`, treating a missing file as an empty document.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] — the file exists and could not be read.
    /// - [`ShepTomlError::Parse`] — the file exists and is not valid TOML.
    pub fn open(path: &Path) -> Result<Self, ShepTomlError> {
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

    /// Writes the document back, creating the parent directory if needed.
    ///
    /// # Errors
    /// - [`ShepTomlError::Io`] — the write failed.
    pub fn save(&self) -> Result<(), ShepTomlError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ShepTomlError::Io {
                path: self.path.clone(),
                source,
            })?;
        }
        std::fs::write(&self.path, self.doc.to_string()).map_err(|source| ShepTomlError::Io {
            path: self.path.clone(),
            source,
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

/// What [`ShepToml::open`]/[`ShepToml::save`] can fail with. Module-scoped
/// per IR-18.
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

        let mut doc = ShepToml::open(&path).unwrap();
        doc.enable_dog("metrics");
        doc.save().unwrap();

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

        let mut doc = ShepToml::open(&path).unwrap();
        doc.enable_dog("bark");
        doc.enable_dog("bark");
        doc.save().unwrap();
        let cfg =
            DaemonConfig::load(Some(&std::fs::read_to_string(&path).unwrap()), &|_| None).unwrap();
        assert_eq!(cfg.daemon.enabled_dogs, vec!["bark"]);

        let mut doc = ShepToml::open(&path).unwrap();
        doc.disable_dog("bark");
        doc.save().unwrap();
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
            ShepToml::open(&path),
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
        let mut doc = ShepToml::open(&path).unwrap();
        doc.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));
        doc.save().unwrap();
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

        let mut doc = ShepToml::open(&path).unwrap();
        doc.rehome_dog("otel");
        doc.save().unwrap();
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
        let mut doc = ShepToml::open(&path).unwrap();
        doc.enable_dog("metrics"); // built-in: no `adopted_dogs` entry at all
        doc.adopt_dog("otel", Path::new("/usr/local/bin/shep-otel"));

        assert_eq!(
            doc.adopted_dog_path("otel"),
            Some(PathBuf::from("/usr/local/bin/shep-otel"))
        );
        assert_eq!(doc.adopted_dog_path("metrics"), None);
        assert_eq!(doc.adopted_dog_path("ghost"), None);
    }

    /// A missing `shep.toml` is not an error — `open` treats it as an empty
    /// document, and `save` creates the file (and its parent directory).
    #[test]
    fn a_missing_file_opens_empty_and_save_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("shep.toml");
        let mut doc = ShepToml::open(&path).unwrap();
        doc.enable_dog("metrics");
        doc.save().unwrap();
        assert!(path.exists());
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
}
