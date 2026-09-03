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
//! one of the two built-ins, connect, and dispatch. `"metrics"` reaches
//! [`metrics::run`]; `"bark"` reaches [`run_bark`] (Task 21), this module's
//! own half of bark's wiring — parse `[dog.bark]`, build
//! [`bark::rules::Rules`], subscribe to the shepherd's bus, and hand both
//! to [`bark::run_loop`] alongside a [`ClientFlockSource`] wrapping the
//! same connection. [`ClientFlockSource`] and the [`bark::EventSource`]
//! impl for [`shep_client::EventStream`] both live here rather than in
//! `bark::mod` itself, because both are thin adapters over
//! [`shep_client::ReconnectingClient`], the type this module already owns
//! through [`DogRuntime`].

pub mod bark;
pub mod metrics;

use core::fmt;

use shep_client::{ConnectError, EventStream, ReconnectingClient, RequestError};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{BusEvent, ProcessInfo, Request, Response, RpcError, RpcErrorCode};

use crate::exit::ExitCode;

/// The dog names this binary can run built-in.
///
/// `enabled_dogs` (`commands::shep_toml`) accepts any name at all — an
/// adopted dog's name is an operator's own choice — but a re-exec through
/// `shep dog <name>` only ever reaches one of these two. Anything else in
/// the config did not come from `enable`/`adopt`, however it got there, and
/// [`run_dog`] refuses it before touching the socket.
pub(crate) const BUILT_IN_DOGS: [&str; 2] = ["metrics", "bark"];

/// A dog's connection to the shepherd, and its own configuration.
///
/// The whole of the dog contract from the dog's side: locate the socket
/// from `$SHEP_HOME` (one of the two variables a dog inherits, the other
/// being `$SHEP_DOG_NAME` — which a built-in dog does not need, since its
/// own `dog <name>` argv already names it), connect, handshake, ask for
/// `[dog.<name>]`, parse it. A dog has no useful work before this
/// exists — metrics polls the shepherd, bark subscribes to it — so nothing
/// here is deferred or made optional.
pub struct DogRuntime {
    /// The connected client. A dog IS a client; there is no second protocol.
    ///
    /// A [`ReconnectingClient`] rather than a bare
    /// [`Client`](shep_client::Client), and that is the difference between
    /// a dog that crosses a daemon handover and one that does not. A dog's
    /// process survives the shepherd's `execve` for free — it is a child of
    /// a daemon whose pid does not change — but only the LISTENING socket
    /// crosses that exec, so the accepted connection underneath this field
    /// dies every time an operator reloads. Measured over six real reloads
    /// before this was supervised: the metrics dog kept its pid, reported
    /// zero restarts, stayed `online`, wrote nothing to stderr, and
    /// answered HTTP 503 to every scrape. The CLI keeps the bare `Client`
    /// deliberately; see `shep_client`'s own `reconnect` module docs for
    /// why one-shot verbs must not gain this.
    pub client: ReconnectingClient,
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
    /// reply. Kept as a reportable error rather than `unreachable!()` or
    /// `todo!()` — a dog is a process the shepherd restarts, and a clean
    /// exit code beats a panic and a confusing log line.
    UnexpectedReply,
    /// The section does not fit the shape [`DogRuntime::config`] was asked
    /// to parse it as.
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

impl From<ConnectError> for DogRunError {
    fn from(source: ConnectError) -> Self {
        Self::Connect(source)
    }
}

impl From<RequestError> for DogRunError {
    fn from(source: RequestError) -> Self {
        Self::Request(source)
    }
}

impl DogRuntime {
    /// Connects and fetches `name`'s section.
    ///
    /// Announces itself as the dog registered under `name`, so a daemon
    /// that refuses this handshake on protocol skew knows which dog it just
    /// refused and can restart it once from disk (the handover design's
    /// G8). A refused handshake never reaches the `DogConfig` request
    /// below, which is the only other place this name would have travelled.
    ///
    /// # Errors
    /// - [`DogRunError::Connect`] — no shepherd answered at the socket.
    /// - [`DogRunError::Request`] — the shepherd refused the config request.
    /// - [`DogRunError::UnexpectedReply`] — the shepherd answered
    ///   `Request::DogConfig` with something other than
    ///   `Response::DogSection`. Its own doc explains why a same-version
    ///   shepherd never sends it.
    pub async fn start(name: &str, paths: ShepPaths) -> Result<Self, DogRunError> {
        let client = ReconnectingClient::connect_as_dog(&paths.socket, name).await?;
        let response = client
            .request(Request::DogConfig {
                name: name.to_string(),
            })
            .await?;
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
        "metrics" => metrics::run(runtime).await,
        "bark" => run_bark(runtime).await,
        _ => unreachable!("checked against BUILT_IN_DOGS above"),
    }
}

/// Runs the bark dog until it is signalled.
///
/// Parses `[dog.bark]` (refusing a section that does not fit, the same
/// posture [`metrics::run`] takes toward its own), builds
/// [`bark::rules::Rules`] — [`bark::rules::Rules::default_rules`] when the
/// operator configured none at all — subscribes to the shepherd's bus on
/// `process.*` (the topic every [`bark::rules::Trigger`] variant reads:
/// `GaveUp` and `Event` off the frames themselves, `RestartRate` and
/// `MemoryAbove` off the reconciliation poll [`bark::run_loop`] drives
/// independently of this subscription), and hands both to
/// [`bark::run_loop`] alongside a [`ClientFlockSource`] wrapping this same
/// connection.
///
/// A refused config or a rule set `Rules::new` rejects are both
/// [`ExitCode::InvalidConfig`] — the same code [`DogRunError::Section`]
/// reports for a section `DogRuntime::config` could not parse at all, since
/// both are "this dog will not run on what it was given," just caught one
/// step later. A failed subscribe defers to `RequestError`'s own
/// conversion, the same one every other verb's failed request goes
/// through.
async fn run_bark(runtime: DogRuntime) -> ExitCode {
    let config = match runtime.config::<bark::BarkConfig>() {
        Ok(config) => config,
        Err(_err) => {
            // The fact, not the value: a `[bark]` section routinely
            // carries a webhook URL with a bearer token in its path, and
            // `DogRunError::Section`'s own message can quote it — see that
            // type's redacted `Debug`. `metrics::run`'s own diagnostic
            // takes the same posture for the same reason.
            eprintln!("shep dog bark: [bark] in dogs.toml does not parse; see `shep dogs`");
            return ExitCode::InvalidConfig;
        }
    };
    let rule_list = if config.rules.is_empty() {
        bark::rules::Rules::default_rules(&config.sinks)
    } else {
        config.rules.clone()
    };
    let rules = match bark::rules::Rules::new(rule_list, &config.sinks) {
        Ok(rules) => rules,
        Err(err) => {
            eprintln!("shep dog bark: {err}");
            return ExitCode::InvalidConfig;
        }
    };
    let events = match runtime.client.subscribe(vec!["process.*".to_owned()]).await {
        Ok(events) => events,
        Err(err) => {
            eprintln!("shep dog bark: could not subscribe to the shepherd's bus: {err}");
            return ExitCode::from(&err);
        }
    };
    let barks_path = runtime.paths.barks.clone();
    let flock = ClientFlockSource {
        client: runtime.client,
    };
    bark::run_loop(events, flock, rules, &config, &barks_path).await
}

/// Adapts [`EventStream`] to [`bark::EventSource`]: its own `next` already
/// yields `Option<Result<BusEvent, shep_client::Lagged>>`, so this is a
/// `map_err` over [`shep_client::Lagged::count`] — the count is the whole
/// of what [`bark::EventSource::next`]'s own `Err` carries.
///
/// `self.next()` below resolves to [`EventStream`]'s own INHERENT method,
/// not a recursive call into this trait impl: `EventStream::next`'s own doc
/// is explicit that an inherent method wins name resolution over a trait
/// method of the same name, which is exactly what makes that call safe to
/// write here.
impl bark::EventSource for EventStream {
    async fn next(&mut self) -> Option<Result<BusEvent, u64>> {
        self.next()
            .await
            .map(|item| item.map_err(|lagged| lagged.count))
    }
}

/// Wraps [`ReconnectingClient`] as [`bark::FlockSource`]: `Request::ListFlock`, mapped
/// into the shape [`bark::run_loop`] can poll without a socket of its own —
/// the same reason [`bark::EventSource`] exists for the subscription side.
struct ClientFlockSource {
    client: ReconnectingClient,
}

impl bark::FlockSource for ClientFlockSource {
    async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
        match self.client.request(Request::ListFlock).await? {
            Response::Flock(flock) => Ok(flock),
            // Never returned by a daemon on the same protocol version —
            // `ListFlock`'s only success reply is `Response::Flock` — kept
            // reportable rather than `unreachable!()`, the same posture
            // `DogRunError::UnexpectedReply`'s own doc explains.
            _ => Err(RequestError::Rpc(RpcError {
                code: RpcErrorCode::Internal,
                message: "the shepherd answered ListFlock with something other than \
                          Response::Flock"
                    .to_owned(),
                daemon_version: None,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use shep_client::testing::{fake_reconnecting_client_on, sample_ack, serve_one_request};

    use super::*;

    /// A [`ShepPaths`] rooted at `dir`, with `socket` pointed wherever the
    /// caller's fake daemon actually bound — flat, not nested under `run/`,
    /// so a test never has to create that directory just to bind a
    /// listener.
    fn test_paths(dir: &Path, socket: PathBuf) -> ShepPaths {
        let home = dir.to_path_buf();
        ShepPaths {
            daemon_config: home.join("shep.toml"),
            dogs_config: home.join("dogs.toml"),
            snapshot: home.join("flock.json"),
            logs: home.join("logs"),
            pids: home.join("pids"),
            run: home.join("run"),
            socket,
            barks: home.join("barks.jsonl"),
            kv: home.join("kv.json"),
            overrides: home.join("overrides.json"),
            home,
        }
    }

    /// Builds a [`DogRuntime`] carrying `section`, backed by a real (if
    /// otherwise unused) connection — [`DogRuntime::config`] never touches
    /// `client`, but the field has to hold a real one, so this reaches for
    /// the lightest fixture that produces one ([`fake_reconnecting_client_on`]) rather
    /// than growing a second connection double. Bridges into its own fresh
    /// Tokio runtime rather than being `async` itself, so call sites stay
    /// plain `#[test]`s — matching `config`, which is sync.
    fn runtime_with_section(section: &str) -> DogRuntime {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (client, _daemon) = fake_reconnecting_client_on(&socket).await;
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
        let socket = shep_client::testing::control_address(dir.path());
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

    /// fails if a dog connects anonymously. The name in the `Hello` is the
    /// only thing a daemon that REFUSES this handshake has to work with —
    /// the `DogConfig` request below never happens on that path — so a dog
    /// that named itself in the request and not in the handshake would
    /// leave the shepherd unable to say which dog went stale, or to restart
    /// it from disk (the handover design's G8).
    ///
    /// The fake closes right after acking, so the `DogConfig` request that
    /// follows fails and `start` returns an error. That is not what this
    /// asserts on: the handshake has already happened by then, and it is
    /// the frame under test.
    #[tokio::test]
    async fn a_dog_announces_its_own_name_at_the_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let served = shep_client::testing::fake_daemon(&socket, Ok(sample_ack())).await;
        let paths = test_paths(dir.path(), socket);

        let _started = DogRuntime::start("bark", paths).await;

        let hello = tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .expect("DogRuntime::start must reach the wire; it hung instead of connecting")
            .unwrap();
        assert_eq!(
            hello.dog_name.as_deref(),
            Some("bark"),
            "a dog must announce the name it was registered under"
        );
    }

    /// fails if `run_dog` ever reaches the socket for a name that never
    /// came from `enable`/`adopt` — no listener is bound at this path at
    /// all, so a connection attempt would report `DaemonUnreachable`, not
    /// `Usage`, proving the name check runs first.
    #[tokio::test]
    async fn an_unknown_dog_name_is_usage_without_touching_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(
            dir.path(),
            shep_client::testing::control_address(dir.path()),
        );
        let code = run_dog("otel", paths).await;
        assert_eq!(code, ExitCode::Usage);
    }

    /// fails if `"bark"` stops reaching [`DogRuntime::start`] — the same
    /// dispatch-reaches-it proof [`run_dog_reaches_metrics`] gives for its
    /// own name. It cannot assert an exit code: [`run_bark`] subscribes to
    /// the shepherd's bus once its config parses, and `serve_one_request`'s
    /// fake daemon closes the connection right after this one `DogConfig`
    /// reply — so this proves dispatch reaches the wire, nothing about
    /// what `run_bark` does next.
    #[tokio::test]
    async fn run_dog_reaches_bark() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let response = Response::DogSection {
            toml: String::new().into(),
        };
        let handle = serve_one_request(&socket, sample_ack(), response).await;
        let paths = test_paths(dir.path(), socket);

        let task = tokio::spawn(run_dog("bark", paths));

        let envelope = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run_dog must reach the wire")
            .unwrap();
        assert_eq!(
            envelope.body,
            Request::DogConfig {
                name: "bark".to_string()
            }
        );

        task.abort();
    }

    /// fails if `"metrics"` stops reaching [`DogRuntime::start`] — proof
    /// that dispatch still gets there, nothing more: [`metrics::run`]
    /// blocks on a shutdown signal once it is up, so this spawns it,
    /// waits only for the `DogConfig` request to land on the wire, then
    /// aborts the task rather than awaiting a return that never comes on
    /// its own. The section answers `bind = "127.0.0.1:0"` — an
    /// OS-assigned port, never [`metrics::MetricsConfig::default`]'s fixed
    /// `9615`, which a developer's own running shepherd (or a leftover
    /// process from a prior hung run) could already hold.
    #[tokio::test]
    async fn run_dog_reaches_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let response = Response::DogSection {
            toml: "bind = \"127.0.0.1:0\"\n".to_string().into(),
        };
        let handle = serve_one_request(&socket, sample_ack(), response).await;
        let paths = test_paths(dir.path(), socket);

        let task = tokio::spawn(run_dog("metrics", paths));

        let envelope = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("run_dog must reach the wire")
            .unwrap();
        assert_eq!(
            envelope.body,
            Request::DogConfig {
                name: "metrics".to_string()
            }
        );

        task.abort();
    }

    /// fails if `run_dog` swallows a connect failure instead of reporting
    /// it — a shepherd that is not up is `DaemonUnreachable`, the same code
    /// every other verb's own failed connect reports.
    #[tokio::test]
    async fn run_dog_reports_daemon_unreachable_with_no_shepherd_running() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(
            dir.path(),
            shep_client::testing::control_address(dir.path()),
        );
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
