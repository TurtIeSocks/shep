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
//! `$SHEP_DOG_NAME` and nothing else it did not already need in order to
//! exec: it connects to the socket the first names, handshakes, and asks for
//! the `[dog.<name>]` section the second names. The reply is opaque text the
//! dog parses, so a third-party dog is bound to the shape of its own section
//! and not to shep's config model, file discovery, or layering rules.
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
use std::path::{Path, PathBuf};

use shep_core::barks::{self, Bark};
use shep_core::config::{AppConfig, DaemonConfig, ResolvedApp, normalize};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{BusEvent, DogSource, ProcessEventKind};
use tokio::sync::broadcast::{self, error::RecvError};

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
///
/// `#[non_exhaustive]` on this enum too, and not only on the [`DogSource`] it
/// discusses above: `shep-daemon`'s `dogs` module is `pub`, a dog gains a
/// failure shape every time it gains a source or a config surface, and an
/// out-of-tree consumer matching exhaustively today would face a breaking
/// change the day it does (IR-20).
#[non_exhaustive]
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
/// carries exactly two things it did not already need in order to exec:
/// `SHEP_HOME`, which is how every client locates the socket, and
/// `SHEP_DOG_NAME`, which is the name this dog was registered under and so
/// the `name` its `Request::DogConfig` has to carry. No `[dog.<name>]`
/// value is ever placed here — a dog asks for its section over the socket,
/// because the environment is readable from the process table, inherited by
/// every child, and captured into crash dumps. The section's KEY is not one
/// of its values, and a dog that cannot learn it cannot ask for the section
/// at all.
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
    // The name the operator registered this dog under — the `[dog.<name>]`
    // key its own section lives beneath, and so the `name` it has to put in
    // `Request::DogConfig`. A built-in dog reads it out of its argv; an
    // adopted one has no argv at all, so it needs another way to learn it —
    // without this, a third-party dog would have to hardcode a name and
    // hope the operator typed the same one. A mismatch is silent on both
    // sides — `dog_section`
    // answers a name nobody adopted with the same empty string a registered
    // dog with no section gets — so the whole of an operator's
    // configuration could be discarded and everything still looked healthy.
    //
    // An environment entry rather than an argv, deliberately: the argv
    // decision above still holds (an argv shep invents is one more thing a
    // foreign binary has to agree with before it can start), and a dog that
    // ignores a variable it does not recognize starts exactly as it did.
    //
    // Safe to place here for the same reason `SHEP_HOME` is, and for no
    // other: a name is not a secret. The rule this does not break is that
    // no `[dog.<name>]` VALUE travels in the environment — that is the key,
    // not the section.
    config
        .env
        .insert("SHEP_DOG_NAME".to_string(), spec.name.clone());
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

/// Watches the bus and records, locally, every enabled dog that exhausts
/// its restart budget.
///
/// The shepherd cannot DELIVER an alert about a dead bark dog: it has no
/// sinks and no webhook code, by design. What it can guarantee is a local
/// trail, so an operator reading `shep barks` after an outage finds the
/// moment alerting stopped rather than a gap they have to infer.
///
/// A bus WATCHER rather than a branch inside the supervisor, and the
/// distinction is the phase's own tripwire: this answers *who should see
/// this*, from outside, and the supervisor stays a machine that knows only
/// how to supervise. A `dog` arm inside `handle_exited` would be the same
/// behaviour reaching into the wrong place.
///
/// Its `JoinHandle` is held by the caller and aborted at teardown: the task
/// parks on a broadcast receiver, which ends on its own when the sender
/// drops, and holding the handle is what makes the end deterministic rather
/// than dependent on sender count.
pub fn spawn_dog_watch(
    mut events: broadcast::Receiver<BusEvent>,
    barks: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                // Only a DOG's Errored is this watcher's business. A sheep's
                // Errored is bark's job — bark writes those records over its
                // own client connection — and duplicating that write here
                // would leave one event with two authors in one file. Exit
                // is excluded too: it fires on every restart a dog survives,
                // and a `barks.jsonl` full of those is one an operator stops
                // reading.
                Ok(BusEvent::Process {
                    event: ProcessEventKind::Errored,
                    info,
                    ..
                }) if info.dog.is_some() => {
                    record_dog_errored(&barks, &info.name, info.restarts);
                }
                Ok(_) => {}
                // The bus DROPS events for a lagging subscriber rather than
                // queuing them (`tokio::sync::broadcast`'s own contract), so
                // a dog's death notice may be among what this receiver just
                // lost. There is no poll to recover it — building one here
                // would be building a second bark dog inside the shepherd,
                // exactly the subsystem this module exists to avoid.
                // Metrics' `shep_dog_up` is the intended answer to this gap.
                Err(RecvError::Lagged(count)) => {
                    tracing::warn!(
                        count,
                        "the shepherd's dog watch dropped bus events; a dog's exhausted restart budget may have gone unrecorded"
                    );
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// Records `name`'s exhausted restart budget as a [`Bark`] the shepherd
/// wrote itself, and logs the same facts at `tracing::error!`.
///
/// The two records serve different audiences: `message` is plain English
/// for an operator reading `shep barks` mid-incident, and the `tracing`
/// event carries the same facts structured for `journalctl`. `sinks` is
/// left empty, which is how a [`Bark`] says the shepherd has no webhook
/// code of its own (see [`Bark::sinks`]'s own doc).
///
/// A dog is supervised with `AppConfig`'s own defaults — [`dog_app`] never
/// overrides `max_restarts` — so `AppConfig::default().max_restarts` is the
/// exhausted budget for every dog, not a guess.
fn record_dog_errored(barks_path: &Path, name: &str, restarts: u32) {
    let budget = AppConfig::default().max_restarts;
    tracing::error!(dog = %name, restarts, budget, "a dog exhausted its restart budget");
    let bark = Bark {
        at_ms: crate::now_ms(),
        rule: "daemon".to_string(),
        subject: name.to_string(),
        message: format!(
            "dog {name} exhausted its restart budget: {restarts} restarts against a budget of {budget}"
        ),
        sinks: Vec::new(),
    };
    if let Err(err) = barks::append(barks_path, &bark, barks::DEFAULT_MAX_BYTES) {
        tracing::warn!(%err, dog = %name, "failed to record a dog's exhausted restart budget");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_paths;
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    /// fails if a `[dog.<name>]` value is folded into the child's
    /// environment. That is the design's whole reason for putting config on
    /// the socket: a webhook URL in the environment is readable from the
    /// process table on some systems, inherited by every child the dog
    /// spawns, and captured into crash dumps. The assertion is over the
    /// ASSEMBLED spec, not the config, because `assemble` is where an env
    /// map would actually be merged.
    ///
    /// Also fails if the section's KEY stops travelling there, which is the
    /// opposite rule and not a contradiction of it: `SHEP_DOG_NAME` is what
    /// a dog puts in `Request::DogConfig` to ask for the section in the
    /// first place, so withholding it withholds the configuration rather
    /// than protecting it.
    #[test]
    fn a_dogs_child_environment_carries_shep_home_and_its_name_and_no_configuration() {
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
        assert_eq!(
            assembled.env.get("SHEP_DOG_NAME"),
            Some(&"bark".to_string()),
            "a dog is told the name its own section lives under"
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

    /// fails if an ADOPTED dog is left to guess the name it was registered
    /// under. A built-in dog can read its own argv (`dog <name>`, asserted
    /// above); an adopted one is given no argv at all, on purpose, so the
    /// environment is the only channel it has. Without this it has to
    /// hardcode a name and hope the operator typed the same one — and a
    /// mismatch is answered with the same empty section a dog with no
    /// configuration gets, so it looks exactly like working.
    ///
    /// Asserted on the name the operator chose, not on the binary's file
    /// stem: `shep adopt ./shep-otel --name telemetry` registers
    /// `telemetry`, and the filename is not the key.
    #[test]
    fn an_adopted_dog_is_told_the_name_it_was_registered_under() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);

        let adopted = dog_app(
            &DogSpec {
                name: "telemetry".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            },
            &paths,
        )
        .unwrap();

        assert!(
            adopted.config().args.is_empty(),
            "the name arrives without shep inventing an argv for a foreign binary"
        );
        assert_eq!(
            adopted.config().env.get("SHEP_DOG_NAME"),
            Some(&"telemetry".to_string())
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

    /// A minimal `Process` bus event, `name` carrying either a sheep's or a
    /// dog's entry depending on `dog`.
    fn process_event(name: &str, kind: ProcessEventKind, dog: Option<DogSource>) -> BusEvent {
        BusEvent::Process {
            event: kind,
            info: ProcessInfo::builder(1, name, ProcStatus::Errored)
                .restarts(16)
                .dog(dog)
                .build(),
            manually: false,
            at_ms: 1_700_000_000_000,
        }
    }

    fn errored_event(name: &str, dog: Option<DogSource>) -> BusEvent {
        process_event(name, ProcessEventKind::Errored, dog)
    }

    /// Polls `path` under a real timeout until it holds at least `n` barks.
    /// The watcher writing to it runs as a separate task, so a bare read
    /// races it; a bare `recv().await` on nothing is the hang this project
    /// has already paid for twice.
    async fn await_barks(path: &std::path::Path, n: usize) -> Vec<Bark> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let found = barks::read(path).unwrap_or_default();
                if found.len() >= n {
                    return found;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("barks.jsonl never reached the expected record count")
    }

    /// fails if the shepherd records a sheep's death as well as a dog's.
    /// Bark writes the sheep records; two writers for one event, into one
    /// file, is how a history stops being trustworthy. Both halves are
    /// needed: without the negative assertion, a watcher that recorded
    /// EVERY `Errored` passes.
    #[tokio::test]
    async fn the_shepherd_records_a_dog_that_gave_up_and_leaves_the_sheep_to_bark() {
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");
        let (events, rx) = broadcast::channel(16);
        let watch = spawn_dog_watch(rx, barks_path.clone());

        events.send(errored_event("web", None)).unwrap();
        events
            .send(errored_event("bark", Some(DogSource::BuiltIn)))
            .unwrap();

        let recorded = await_barks(&barks_path, 1).await;
        assert_eq!(recorded.len(), 1, "one record, and it is the dog's");
        assert_eq!(recorded[0].subject, "bark");
        assert_eq!(recorded[0].rule, "daemon");
        assert!(
            recorded[0].sinks.is_empty(),
            "the shepherd has no sinks and says so by carrying none"
        );

        watch.abort();
    }

    /// fails if a restart a dog survives is recorded as a death. A dog that
    /// crashes and comes back is not an outage, and a `barks.jsonl` full of
    /// them is one an operator stops reading.
    #[tokio::test]
    async fn a_dog_that_merely_exited_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");
        let (events, rx) = broadcast::channel(16);
        let watch = spawn_dog_watch(rx, barks_path.clone());

        events
            .send(process_event(
                "bark",
                ProcessEventKind::Exit,
                Some(DogSource::BuiltIn),
            ))
            .unwrap();
        // A real `Errored` after it is what proves the watcher was ever
        // listening: without this, a watcher that recorded nothing at all
        // (dead code, or the wrong topic) would also pass.
        events
            .send(errored_event("bark", Some(DogSource::BuiltIn)))
            .unwrap();

        let recorded = await_barks(&barks_path, 1).await;
        assert_eq!(
            recorded.len(),
            1,
            "the Exit left no record; only the Errored that followed it did"
        );

        watch.abort();
    }
}
