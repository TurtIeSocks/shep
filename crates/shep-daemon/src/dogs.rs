//! The dog contract: what a dog is spawned as, and how it is served its own
//! configuration
//!
//! A dog is an ordinary supervised process that happens to speak the control
//! protocol. Nothing here teaches the engine a second kind of supervision:
//! [`dog_app`] assembles the same [`ResolvedApp`] a Flockfile entry would
//! produce, and the supervisor spawns, restarts, reloads and kills it exactly
//! as it does a sheep.
//!
//! ## The two halves of the contract
//!
//! **Where the binary comes from** is [`DogSpec::source`]. A built-in dog is
//! an argv branch of the shep binary itself; an adopted one is a binary an
//! operator installed. That is the whole difference — both are run at the
//! daemon's own trust level, and neither gets a supervision rule of its own.
//!
//! **Where the configuration comes from** is [`dog_section`], reached over
//! the socket as `Request::DogConfig`. A dog inherits `$SHEP_HOME` and
//! nothing else it did not already need in order to exec: it connects to the
//! socket that names, handshakes, and asks for its own `[dog.<name>]`
//! section. The reply is opaque text the dog parses, so a third-party dog is
//! bound to the shape of its own section and not to shep's config model,
//! file discovery, or layering rules.
//!
//! ## Why the section travels over the socket
//!
//! The environment is readable from the process table on some systems,
//! inherited by every child a dog spawns, and captured into crash dumps. A
//! dog's section routinely holds sinks with credentials in them — a Discord
//! or Slack webhook URL is a bearer token in a query string — so it stays
//! off the child's environment entirely. `SECURITY.md` already discloses
//! that `flock.json` records app `env` in the clear; this declines to widen
//! that exposure to a second surface.

use core::fmt;
use std::path::Path;

use shep_core::config::{AppConfig, DaemonConfig, ResolvedApp, normalize};
use shep_core::paths::ShepPaths;
use shep_core::protocol::DogSource;

use crate::supervisor::SupervisorHandle;

/// One dog the daemon knows about: its name, and where its binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogSpec {
    /// The dog's name — the `[dog.<name>]` key and the entry's name.
    pub name: String,
    /// Where its binary comes from.
    pub source: DogSource,
}

/// Error assembling a dog's app config, or reading its section
///
/// `Debug` is derived and needs no redaction: the variants carry a path, a
/// normalizer complaint about a config this module assembled itself, or a
/// TOML parser message — never a value read out of a parsed `[dog.<name>]`
/// table. The one way a section's own text can reach a message is a *syntax*
/// error, where the parser quotes the line it failed on; that is the same
/// exposure [`DaemonConfigError`](shep_core::config::DaemonConfigError)
/// already carries, and it reaches only the peer that asked, which
/// peer-cred auth has already established owns the file.
///
/// [`Self::NoBinary`] and [`Self::Io`] wrap the underlying [`std::io::Error`]
/// rather than rendering it, so a caller keeps the OS diagnostic through
/// [`core::error::Error::source`]; that costs this enum `Clone`/`PartialEq`/
/// `Eq` (IR-19's documented exception).
#[derive(Debug)]
pub enum DogError {
    /// [`std::env::current_exe`] failed, so a built-in dog has no program to
    /// run
    NoBinary(std::io::Error),
    /// The dog's binary comes from a source this build cannot spawn
    /// (carries the source as `Debug` renders it). [`DogSource`] is
    /// `#[non_exhaustive]`, so a name enabled by a newer shep can reach an
    /// older daemon.
    UnsupportedSource(String),
    /// The assembled config failed `normalize`, or the file read is not
    /// valid `shep.toml`, or the section it holds cannot be rendered back to
    /// TOML (carries the rejection message)
    Config(String),
    /// The file exists and could not be read
    Io(std::io::Error),
}

impl fmt::Display for DogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBinary(err) => write!(f, "this binary's own path is unreadable: {err}"),
            Self::UnsupportedSource(source) => {
                write!(f, "no way to spawn a dog from source {source}")
            }
            Self::Config(msg) => write!(f, "dog configuration is unusable: {msg}"),
            Self::Io(err) => write!(f, "dog configuration could not be read: {err}"),
        }
    }
}

impl core::error::Error for DogError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::NoBinary(err) | Self::Io(err) => Some(err),
            Self::UnsupportedSource(_) | Self::Config(_) => None,
        }
    }
}

/// The app config the daemon spawns `spec` from.
///
/// A built-in dog is `<this binary> dog <name>`; an adopted one is the
/// operator's binary with no arguments. Either way the child's environment
/// carries exactly one thing it did not already need in order to exec:
/// `SHEP_HOME`, which is how every client locates the socket. No
/// `[dog.<name>]` value is ever placed here — a dog asks for its section
/// over the socket (`Request::DogConfig`), because the environment is
/// readable from the process table, inherited by every child, and captured
/// into crash dumps.
///
/// `autorestart` and the restart budget are left at their defaults: a dog
/// is supervised exactly as a sheep is.
///
/// # Errors
/// - [`DogError::NoBinary`] — [`std::env::current_exe`] failed, so a
///   built-in dog has no program to run.
/// - [`DogError::UnsupportedSource`] — the source is a kind this build does
///   not know how to spawn.
/// - [`DogError::Config`] — the assembled config failed `normalize`.
pub fn dog_app(spec: &DogSpec, paths: &ShepPaths) -> Result<ResolvedApp, DogError> {
    let (script, args) = match &spec.source {
        DogSource::BuiltIn => (
            std::env::current_exe()
                .map_err(DogError::NoBinary)?
                .display()
                .to_string(),
            vec!["dog".to_string(), spec.name.clone()],
        ),
        // No arguments: an adopted dog is a binary somebody else wrote, and
        // an argv shep invented for it is one more thing it has to agree
        // with before it can start.
        DogSource::Adopted { path } => (path.clone(), Vec::new()),
        source => return Err(DogError::UnsupportedSource(format!("{source:?}"))),
    };

    let mut config = AppConfig::minimal(&spec.name, &script);
    config.args = args;
    config
        .env
        .insert("SHEP_HOME".to_string(), paths.home.display().to_string());
    normalize(config).map_err(|err| DogError::Config(err.to_string()))
}

/// Starts every dog in `specs`, warning and carrying on for each one that
/// will not start.
///
/// Never fails the boot. A dog that cannot be spawned is a monitoring gap,
/// and refusing to bring the flock up over it would turn that gap into an
/// outage — the one trade this whole subsystem is built to avoid.
///
/// Two ways a dog can fail to start, both answered the same way — a
/// `warn!` naming the dog, and moving on to the next one in `specs`:
///
/// - [`dog_app`] rejects the spec before anything is registered: the
///   binary's own path is unreadable, or the source is one this build does
///   not know how to spawn.
/// - [`SupervisorHandle::start_dog`] itself fails to spawn the binary, or
///   — the guard `Request::EnableDog`'s handler already carries, and this
///   boot path has to carry too — comes back `Ok` over a sheep that
///   already holds the name. `start_dog` is idempotent by NAME, so an
///   unmarked reply means a sheep got there first: no dog was started, and
///   logging success over that reply would be the exact false positive the
///   RPC arm already refuses to give an operator who types `shep enable`.
pub async fn spawn_enabled_dogs(
    specs: &[DogSpec],
    paths: &ShepPaths,
    supervisor: &SupervisorHandle,
) {
    for spec in specs {
        let app = match dog_app(spec, paths) {
            Ok(app) => app,
            Err(err) => {
                tracing::warn!(dog = %spec.name, %err, "a dog did not start");
                continue;
            }
        };
        match supervisor.start_dog(app, spec.source.clone()).await {
            Ok(info) if info.dog.is_none() => tracing::warn!(
                dog = %spec.name,
                "a sheep is already registered under this name; the dog did not start"
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(dog = %spec.name, %err, "a dog did not start"),
        }
    }
}

/// The `[dog.<name>]` section of `path`, rendered back to TOML text.
///
/// Reads the file on every call rather than serving a copy cached at boot:
/// one reader can never be stale, and it is what makes
/// `shep disable X && shep enable X` re-read an edited section.
///
/// A missing file, or a file with no such section, is `Ok(String::new())` —
/// a dog with no configuration is the ordinary case, not a fault.
///
/// # Errors
/// - [`DogError::Config`] — the file exists and is not valid `shep.toml`,
///   or its section will not render back to TOML.
/// - [`DogError::Io`] — the file exists and could not be read.
pub fn dog_section(path: &Path, name: &str) -> Result<String, DogError> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => return Err(DogError::Io(err)),
    };
    // Loaded through the daemon's own config loader rather than parsed here,
    // so a broken `shep.toml` is one named error and not a second parser's
    // opinion of the same file. No environment closure: `SHEP_*` overrides
    // govern the daemon's own knobs and have nothing to say about a dog's
    // section.
    let config = DaemonConfig::load(Some(&source), &|_| None)
        .map_err(|err| DogError::Config(err.to_string()))?;
    match config.dog.get(name) {
        None => Ok(String::new()),
        Some(table) => toml::to_string(table).map_err(|err| DogError::Config(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_paths;

    /// fails if a `[dog.<name>]` value is folded into the child's
    /// environment. That is the design's whole reason for putting config on
    /// the socket: a webhook URL in the environment is readable from the
    /// process table on some systems, inherited by every child the dog
    /// spawns, and captured into crash dumps. The assertion is over the
    /// ASSEMBLED spec, not the config, because `assemble` is where an env
    /// map would actually be merged.
    #[test]
    fn a_dogs_child_environment_carries_shep_home_and_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::write(
            &paths.daemon_config,
            "[dog.bark]\nwebhook = \"https://example.invalid/hook\"\n",
        )
        .unwrap();
        let spec = DogSpec {
            name: "bark".to_string(),
            source: DogSource::BuiltIn,
        };
        let app = dog_app(&spec, &paths).unwrap();
        let assembled = crate::assemble::assemble(&app, 0, &paths, None);
        assert_eq!(
            assembled.env.get("SHEP_HOME"),
            Some(&paths.home.display().to_string())
        );
        assert!(
            !assembled
                .env
                .values()
                .any(|v| v.contains("example.invalid")),
            "a dog's configuration never travels in its environment: {:?}",
            assembled.env
        );
    }

    /// fails if a built-in dog is spawned as anything but this binary's own
    /// hidden `dog <name>` branch, and fails if an adopted one is given
    /// arguments it never asked for — which would make every third-party
    /// dog see an argv shep invented for it.
    #[test]
    fn a_built_in_dog_runs_this_binary_and_an_adopted_one_runs_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        let built_in = dog_app(
            &DogSpec {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
            },
            &paths,
        )
        .unwrap();
        assert_eq!(
            built_in.config().script,
            std::env::current_exe().unwrap().display().to_string()
        );
        assert_eq!(built_in.config().args, vec!["dog", "metrics"]);

        let adopted = dog_app(
            &DogSpec {
                name: "otel".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            },
            &paths,
        )
        .unwrap();
        assert_eq!(adopted.config().script, "/usr/local/bin/shep-otel");
        assert!(adopted.config().args.is_empty());
        assert_eq!(
            adopted.config().name,
            "otel",
            "the NAME is the config key, never the filename"
        );
    }

    /// fails if `dog_section` returns the whole file, or a typed structure,
    /// or fails on a file with no such section. The blob is what a
    /// third-party dog parses; handing it a table it did not ask for is the
    /// same bug as handing it nothing.
    #[test]
    fn a_dogs_section_comes_back_as_its_own_table_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\nlog_json = true\n\n[dog.bark]\ndebounce = \"30s\"\n\n[dog.metrics]\nport = 9615\n",
        )
        .unwrap();

        let bark = dog_section(&path, "bark").unwrap();
        assert!(bark.contains("debounce"));
        assert!(
            !bark.contains("9615"),
            "one dog never sees another's config"
        );
        assert!(!bark.contains("log_json"), "nor the daemon's own");
        // Round-trips as TOML, since that is the contract the dog parses under.
        let parsed: toml::Table = toml::from_str(&bark).unwrap();
        assert_eq!(parsed["debounce"].as_str(), Some("30s"));

        assert_eq!(dog_section(&path, "absent").unwrap(), "");
        assert_eq!(
            dog_section(&dir.path().join("gone.toml"), "bark").unwrap(),
            ""
        );
    }
}
