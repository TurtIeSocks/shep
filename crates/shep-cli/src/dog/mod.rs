//! `shep dog <name>`: the hidden re-exec target a built-in dog runs as, and
//! [`DogRuntime`] — the connection and configuration every dog needs before
//! it can do anything else.
//!
//! `docs/shepherd-channel.md` and `shep-daemon/src/dogs.rs`'s own module doc
//! are the rest of the dog contract; this module is the CLI side of it. A
//! dog inherits `$SHEP_HOME` and nothing else it did not already need in
//! order to exec — no `[dog.<name>]` value ever rides in the environment,
//! since that is readable from the process table, inherited by every child
//! a dog spawns, and captured into crash dumps. Instead [`DogRuntime::start`]
//! connects to the socket `$SHEP_HOME` names and asks for the section over
//! `Request::DogConfig`, the same reason
//! [`DogSectionToml`](shep_core::protocol::DogSectionToml) exists one layer
//! down the stack.
//!
//! [`run_dog`] is `main`'s whole `Commands::Dog` arm: validate the name is
//! one of the two built-ins, connect, and dispatch. Tasks 15 and 21 fill in
//! what `"metrics"` and `"bark"` actually do; until then each is a stub that
//! reports which task owns it and exits [`ExitCode::Failure`] — never
//! `todo!()`, which would abort a supervised process with a panic and a
//! confusing log line instead of a plain, restartable failure.

pub mod http;

use core::fmt;

use shep_client::{Client, ConnectError, RequestError};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response};

use crate::exit::ExitCode;

/// The dog names this binary can run built-in.
///
/// `enabled_dogs` (`commands::shep_toml`) accepts any name at all — an
/// adopted dog's name is an operator's own choice — but a re-exec through
/// `shep dog <name>` only ever reaches one of these two. Anything else in
/// the config did not come from `enable`/`adopt`, however it got there, and
/// [`run_dog`] refuses it before touching the socket.
const BUILT_IN_DOGS: [&str; 2] = ["metrics", "bark"];

/// A dog's connection to the shepherd, and its own configuration.
///
/// The whole of the dog contract from the dog's side: locate the socket
/// from `$SHEP_HOME` (the one variable a dog inherits), connect, handshake,
/// ask for `[dog.<name>]`, parse it. A dog has no useful work before this
/// exists — metrics polls the shepherd, bark subscribes to it — so nothing
/// here is deferred or made optional.
pub struct DogRuntime {
    /// The connected client. A dog IS a client; there is no second protocol.
    pub client: Client,
    /// This dog's `[dog.<name>]` section, exactly as the shepherd rendered
    /// it, for the dog to parse into its own shape. Empty when the file has
    /// no such section.
    pub section: String,
    /// `$SHEP_HOME` as this dog resolved it.
    pub paths: ShepPaths,
    /// The dog's own name, kept so [`Self::config`] can name it in a
    /// [`DogRunError::Section`] without every caller threading it through
    /// again.
    name: String,
}

/// Manual, not derived — the literal interface this task was handed shows
/// `#[derive(Debug)]`; this deviates from it, self-reported (see this
/// task's own report). [`Self::section`] is a dog's raw `[dog.<name>]`
/// config text, which routinely carries a webhook URL with a bearer token
/// in its query string (`SECURITY.md`) — a derived `Debug` would print it
/// in full the moment anything `{:?}`-logs a `DogRuntime`, exactly the leak
/// [`shep_core::protocol::DogSectionToml`]'s own manual `Debug` exists to
/// prevent one layer up the stack. `client` and `paths` carry nothing
/// sensitive and print unchanged; `name` likewise.
impl fmt::Debug for DogRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DogRuntime")
            .field("client", &self.client)
            .field("section", &format!("<{} bytes>", self.section.len()))
            .field("paths", &self.paths)
            .field("name", &self.name)
            .finish()
    }
}

/// Why [`DogRuntime::start`] or [`DogRuntime::config`] failed.
pub enum DogRunError {
    /// No shepherd answered at the socket.
    Connect(ConnectError),
    /// The shepherd refused the config request.
    Request(RequestError),
    /// The shepherd answered `Request::DogConfig` with something other than
    /// `Response::DogSection` — protocol drift this client does not
    /// recognise, not a connection or config problem.
    ///
    /// Never returned by a daemon on the same protocol version:
    /// `shep-daemon/src/rpc.rs`'s `DogConfig` arm has exactly one success
    /// reply. Kept as a reportable error rather than `unreachable!()`,
    /// for the same reason [`run_dog`]'s own doc gives for not using
    /// `todo!()` in its two built-in stubs — a dog is a process the
    /// shepherd restarts, and a clean exit code beats a panic.
    UnexpectedReply,
    /// The section does not fit the shape [`DogRuntime::config`] was asked
    /// to parse it as.
    ///
    /// `#[allow(dead_code)]`: no call site constructs this yet — nothing in
    /// this crate calls [`DogRuntime::config`] until Task 15/21's own built-in
    /// dogs do, the same reasoning `output::emit`'s own doc gives for
    /// `ShepToml::adopt_dog`/`rehome_dog` between Tasks 10 and 11. Covered by
    /// this module's own unit test in the meantime.
    #[allow(dead_code)]
    Section {
        /// The dog's own name.
        name: String,
        /// The parser's full complaint — can quote the offending line, and
        /// that line can be a `[dog.<name>]` webhook URL (see this type's
        /// `Debug`).
        message: String,
    },
}

/// Manual, not derived (IR-41): [`DogRunError::Section`]'s `message` is the
/// TOML parser's own complaint, which quotes the offending line verbatim —
/// and that line can be a `[dog.<name>]` webhook URL with a bearer token in
/// its query string. Redacted to the dog's name and a fixed description.
/// `Connect`/`Request` wrap types with their own non-leaking `Debug`
/// already (neither one ever holds a parsed section), so they format
/// unchanged, and `UnexpectedReply` carries no fields at all.
impl fmt::Debug for DogRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => f.debug_tuple("Connect").field(err).finish(),
            Self::Request(err) => f.debug_tuple("Request").field(err).finish(),
            Self::UnexpectedReply => f.write_str("UnexpectedReply"),
            Self::Section { name, .. } => f
                .debug_struct("Section")
                .field("name", name)
                .field("message", &"<redacted: may quote the section>")
                .finish(),
        }
    }
}

impl fmt::Display for DogRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "no shepherd answered at the socket: {err}"),
            Self::Request(err) => write!(f, "the shepherd refused the config request: {err}"),
            Self::UnexpectedReply => {
                f.write_str("the shepherd answered with a response this client does not understand")
            }
            Self::Section { name, message } => {
                write!(f, "dog {name}'s own configuration does not fit: {message}")
            }
        }
    }
}

impl core::error::Error for DogRunError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Connect(err) => Some(err),
            Self::Request(err) => Some(err),
            Self::UnexpectedReply | Self::Section { .. } => None,
        }
    }
}

impl DogRuntime {
    /// Connects and fetches `name`'s section.
    ///
    /// # Errors
    /// - [`DogRunError::Connect`] — no shepherd answered at the socket.
    /// - [`DogRunError::Request`] — the shepherd refused the config request.
    pub async fn start(name: &str, paths: ShepPaths) -> Result<Self, DogRunError> {
        let client = Client::connect(&paths.socket)
            .await
            .map_err(DogRunError::Connect)?;
        let response = client
            .request(Request::DogConfig {
                name: name.to_string(),
            })
            .await
            .map_err(DogRunError::Request)?;
        let Response::DogSection { toml } = response else {
            return Err(DogRunError::UnexpectedReply);
        };
        Ok(Self {
            section: toml.as_str().to_string(),
            client,
            paths,
            name: name.to_string(),
        })
    }

    /// This dog's section parsed into `T`, or `T::default()` when the
    /// shepherd had no section for it.
    ///
    /// # Errors
    /// - [`DogRunError::Section`] — the section does not fit `T`, naming
    ///   the dog and the parser's own message. A dog refuses to run on
    ///   configuration it cannot read rather than silently falling back to
    ///   defaults an operator did not ask for.
    ///
    /// `#[allow(dead_code)]`: no call site in this crate calls this yet —
    /// `run_dog`'s two stubs don't need config until Task 15/21's own
    /// built-in dogs land, matching `ShepToml::adopt_dog`/`rehome_dog`'s own
    /// precedent between Tasks 10 and 11. Covered by this module's own unit
    /// test (`a_section_that_does_not_fit_is_refused_rather_than_defaulted`)
    /// in the meantime.
    #[allow(dead_code)]
    pub fn config<T>(&self) -> Result<T, DogRunError>
    where
        T: serde::de::DeserializeOwned + Default,
    {
        if self.section.is_empty() {
            return Ok(T::default());
        }
        toml::from_str(&self.section).map_err(|err| DogRunError::Section {
            name: self.name.clone(),
            message: err.to_string(),
        })
    }
}

/// Maps a failed [`DogRuntime::start`] to the exit code that reports it.
///
/// `Connect`/`Request` defer to the same `ExitCode` conversions every other
/// verb's own client-connect/request failure already goes through
/// (`exit.rs`), so a dog and an operator's own CLI invocation report the
/// same cause the same way. `Section` is
/// [`ExitCode::InvalidConfig`] — the shape spec §9 gives a Flockfile or
/// daemon config that fails validation — and `UnexpectedReply` is
/// [`ExitCode::Internal`], matching every other "the daemon answered with a
/// response this client does not understand" call site in this crate.
fn exit_code_for(err: &DogRunError) -> ExitCode {
    match err {
        DogRunError::Connect(inner) => ExitCode::from(inner),
        DogRunError::Request(inner) => ExitCode::from(inner),
        DogRunError::Section { .. } => ExitCode::InvalidConfig,
        DogRunError::UnexpectedReply => ExitCode::Internal,
    }
}

/// Runs the named dog until it is signalled. `main`'s `Commands::Dog` arm.
///
/// An unknown name is refused before the socket is ever touched
/// ([`ExitCode::Usage`], not [`ExitCode::Internal`]) — `name` comes from
/// `enabled_dogs`, which an operator typed, and naming the two built-ins in
/// the refusal is what turns their typo into a fix rather than a daemon
/// log line nobody reads.
///
/// A dog's own diagnostics go to stderr, plain text — no `Streams`, no
/// `--format json` envelope. That is deliberate: this is a supervised
/// process, not an interactive one, and the daemon's log pump already
/// captures its stderr into `$SHEP_HOME/logs/<name>-0-err.log` like any
/// sheep's — `shep bleats <name>` is how an operator reads it.
pub async fn run_dog(name: &str, paths: ShepPaths) -> ExitCode {
    if !BUILT_IN_DOGS.contains(&name) {
        eprintln!("shep dog: unknown dog {name:?}; the built-in dogs are \"metrics\" and \"bark\"");
        return ExitCode::Usage;
    }
    let runtime = match DogRuntime::start(name, paths).await {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("shep dog {name}: {err}");
            return exit_code_for(&err);
        }
    };
    match name {
        "metrics" => stub(&runtime, "Task 15", "crates/shep-cli/src/dog/metrics.rs"),
        "bark" => stub(&runtime, "Task 21", "crates/shep-cli/src/dog/bark.rs"),
        _ => unreachable!("checked against BUILT_IN_DOGS above"),
    }
}

/// What every built-in dog answers until its own task lands it.
///
/// Reports plainly rather than `todo!()`, which would abort this
/// (supervised, restarted-on-exit) process with a panic and a confusing
/// stack trace in `<name>-0-err.log` instead of one readable line.
fn stub(runtime: &DogRuntime, task: &str, module: &str) -> ExitCode {
    eprintln!(
        "shep dog {}: not implemented yet — {task}'s own module ({module}) lands this",
        runtime.name,
    );
    ExitCode::Failure
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use shep_client::testing::{fake_client_on, sample_ack, serve_one_request};

    use super::*;

    /// A [`ShepPaths`] rooted at `dir`, with `socket` pointed wherever the
    /// caller's fake daemon actually bound — flat, not nested under `run/`,
    /// so a test never has to create that directory just to bind a
    /// listener.
    fn test_paths(dir: &Path, socket: PathBuf) -> ShepPaths {
        let home = dir.to_path_buf();
        ShepPaths {
            daemon_config: home.join("shep.toml"),
            snapshot: home.join("flock.json"),
            logs: home.join("logs"),
            pids: home.join("pids"),
            run: home.join("run"),
            socket,
            barks: home.join("barks.jsonl"),
            home,
        }
    }

    /// Builds a [`DogRuntime`] carrying `section`, backed by a real (if
    /// otherwise unused) connection — [`DogRuntime::config`] never touches
    /// `client`, but the field has to hold a real one, so this reaches for
    /// the lightest fixture that produces one ([`fake_client_on`]) rather
    /// than growing a second connection double. Bridges into its own fresh
    /// Tokio runtime rather than being `async` itself, so call sites stay
    /// plain `#[test]`s — matching `config`, which is sync.
    fn runtime_with_section(section: &str) -> DogRuntime {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (client, _daemon) = fake_client_on(&socket).await;
            DogRuntime {
                client,
                section: section.to_string(),
                paths: test_paths(dir.path(), socket),
                name: "testdog".to_string(),
            }
        })
    }

    /// fails if a dog is handed defaults for a section it could not parse.
    /// A bark dog silently running with no rules because a `debounce` was
    /// misspelled is precisely the outcome that makes an operator trust the
    /// alerting they no longer have.
    #[test]
    fn a_section_that_does_not_fit_is_refused_rather_than_defaulted() {
        #[derive(Debug, Default, serde::Deserialize, PartialEq)]
        #[serde(deny_unknown_fields, default)]
        struct Cfg {
            port: u16,
        }
        let runtime = runtime_with_section("port = \"nine thousand\"\n");
        let err = runtime.config::<Cfg>().unwrap_err();
        assert!(matches!(err, DogRunError::Section { .. }));
        assert!(err.to_string().contains("port"));

        let empty = runtime_with_section("");
        assert_eq!(empty.config::<Cfg>().unwrap(), Cfg::default());
    }

    /// fails if the dog asks for someone else's section, or for none at
    /// all. `Request::DogConfig` carries the name, and a dog that sent a
    /// hardcoded one would read another dog's webhook URLs.
    #[tokio::test]
    async fn a_dog_asks_for_its_own_section_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let response = Response::DogSection {
            toml: "webhook = \"https://example.invalid/hook\"\n"
                .to_string()
                .into(),
        };
        let handle = serve_one_request(&socket, sample_ack(), response).await;
        let paths = test_paths(dir.path(), socket);

        let runtime = DogRuntime::start("bark", paths).await.unwrap();

        let envelope = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("DogRuntime::start must reach the wire; it hung instead of connecting")
            .unwrap();
        assert_eq!(
            envelope.body,
            Request::DogConfig {
                name: "bark".to_string()
            }
        );
        assert_eq!(
            runtime.section,
            "webhook = \"https://example.invalid/hook\"\n"
        );
    }

    /// fails if `run_dog` ever reaches the socket for a name that never
    /// came from `enable`/`adopt` — no listener is bound at this path at
    /// all, so a connection attempt would report `DaemonUnreachable`, not
    /// `Usage`, proving the name check runs first.
    #[tokio::test]
    async fn an_unknown_dog_name_is_usage_without_touching_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path(), dir.path().join("never-bound.sock"));
        let code = run_dog("otel", paths).await;
        assert_eq!(code, ExitCode::Usage);
    }

    /// fails if either built-in name stops reaching [`DogRuntime::start`],
    /// or the stub's `Failure` exit turns into a panic (`todo!()`) — both
    /// silent regressions `run_dog`'s own doc warns against.
    #[tokio::test]
    async fn run_dog_reaches_the_stub_for_each_built_in_dog() {
        for name in BUILT_IN_DOGS {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("s.sock");
            let response = Response::DogSection {
                toml: String::new().into(),
            };
            let handle = serve_one_request(&socket, sample_ack(), response).await;
            let paths = test_paths(dir.path(), socket);

            let code = run_dog(name, paths).await;
            assert_eq!(code, ExitCode::Failure, "{name}");

            let envelope = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("run_dog must reach the wire")
                .unwrap();
            assert_eq!(
                envelope.body,
                Request::DogConfig {
                    name: name.to_string()
                },
                "{name}"
            );
        }
    }

    /// fails if `run_dog` swallows a connect failure instead of reporting
    /// it — a shepherd that is not up is `DaemonUnreachable`, the same code
    /// every other verb's own failed connect reports.
    #[tokio::test]
    async fn run_dog_reports_daemon_unreachable_with_no_shepherd_running() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path(), dir.path().join("never-bound.sock"));
        let code = run_dog("metrics", paths).await;
        assert_eq!(code, ExitCode::DaemonUnreachable);
    }

    /// The redaction IR-41 requires: `Debug` on a section mismatch carries
    /// the dog's name and a fixed description, never the parser's message —
    /// which, for a real `[dog.<name>]` table, can quote a webhook URL.
    #[test]
    fn dog_run_error_section_debug_never_prints_the_message() {
        let secret = "https://hooks.example.com/services/T00/B00/super-secret-token";
        let err = DogRunError::Section {
            name: "bark".to_string(),
            message: format!("invalid type: string \"{secret}\", expected u16\nin `webhook`"),
        };
        let debug = format!("{err:?}");
        assert!(!debug.contains(secret), "{debug}");
        assert!(!debug.contains("webhook"), "{debug}");
        assert_eq!(
            debug,
            "Section { name: \"bark\", message: \"<redacted: may quote the section>\" }"
        );
    }

    /// The `DogRuntime` sibling of the test above: a derived `Debug` here
    /// would print [`DogRuntime::section`] in full, undoing the same
    /// redaction one layer down.
    ///
    /// `client`'s own `Debug` embeds this test's tempdir socket path, so the
    /// whole struct can't be one hardcoded exact string the way
    /// `dog_run_error_section_debug_never_prints_the_message` is — the
    /// redacted `section` field itself still gets an exact-string pin
    /// (`section`'s byte count is fixed by the literal below), alongside the
    /// never-contains checks that matter most.
    #[test]
    fn dog_runtime_debug_never_prints_the_section() {
        let secret = "https://hooks.example.com/services/T00/B00/super-secret-token";
        let section = format!("webhook = \"{secret}\"\n");
        let byte_len = section.len();
        let runtime = runtime_with_section(&section);
        let debug = format!("{runtime:?}");
        assert!(!debug.contains(secret), "{debug}");
        assert!(!debug.contains("webhook"), "{debug}");
        assert!(
            debug.contains(&format!("section: \"<{byte_len} bytes>\"")),
            "{debug}"
        );
    }
}
