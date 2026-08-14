//! What the link task reads the shepherd through, and the real implementation
//! over `shep-client`.
//!
//! Three traits rather than one concrete type, so [`super::link::run_link`] is
//! drivable with no socket at all: the tests that matter here are about a bus
//! that genuinely drops frames and a shepherd that genuinely will not answer,
//! and neither is reachable through a real connection on demand.
//!
//! **Why two source traits and not one object.** Reading the bus needs `&mut`
//! and issuing a `ListFlock` needs a shared reference, and `tokio::select!`
//! cannot hold both against one value. `crate::dog::bark` split its own pair
//! for exactly this reason and this follows it.
//!
//! **Why they are declared here and not shared with bark.** The shapes look
//! alike; the meanings differ. Bark's `EventSource::next` yields
//! `Result<BusEvent, u64>` because a dog only needs the count; this one yields
//! `Result<BusEvent, `[`Lagged`]`>` because the status bar prints the notice
//! and has to distinguish it from the shepherd's own `BusEvent::Dropped`. A
//! shared home for two six-line traits, one of which would then be generic
//! over its error type to serve both callers, is a worse trade than the
//! duplication — the repetition here is of shape, not of meaning.
//!
//! Not called outside this module's own tests yet: Task 8 (`mod.rs`, the verb
//! and the event loop) is the real caller for every public item below, and it
//! has not landed. `#[allow(dead_code)]` on each says so explicitly, same
//! convention `theme::Palette` and `app::App` already carry for the identical
//! reason.

use core::fmt;
use core::future::Future;
use std::path::{Path, PathBuf};

use shep_client::{Client, ConnectError, EventStream, Lagged, RequestError};
use shep_core::protocol::{BusEvent, ProcessInfo, Request, Response};

use crate::exit::ExitCode;

/// The topics lookout subscribes to.
///
/// `process.*` is what the flock table is made of; `daemon.*` carries
/// `BusEvent::Dropped` and `BusEvent::DaemonShutdown`, both of which this
/// dashboard reports rather than ignores.
///
/// **Not `log.*`, deliberately.** The bleats feed is Phase 12b. Subscribing to
/// every line every sheep writes, in order to draw a pane that does not exist,
/// would make lookout the highest-volume subscriber on the bus for no visible
/// reason — and would manufacture the very `Dropped`/`Lagged` condition
/// [`super::link`] exists to survive.
#[allow(dead_code)]
pub const TOPICS: &[&str] = &["process.*", "daemon.*"];

/// Reading the flock. `&self`, so [`super::link::run_connected`] can hold it
/// across the same `select!` that holds an [`EventSource`] mutably.
#[allow(dead_code)]
pub trait FlockSource: Send + Sync {
    /// The flock as it stands.
    ///
    /// # Errors
    /// Whatever the underlying source could not answer with — for the real
    /// implementation, whatever `Request::ListFlock` failed with.
    fn flock(&self) -> impl Future<Output = Result<Vec<ProcessInfo>, RequestError>> + Send;
}

/// One source of bus frames.
#[allow(dead_code)]
pub trait EventSource: Send {
    /// The next frame; `Err(`[`Lagged`]`)` when this client's own receiver fell
    /// behind and discarded frames; `None` when the subscription ends, which
    /// is how a dead connection announces itself.
    fn next_event(&mut self) -> impl Future<Output = Option<Result<BusEvent, Lagged>>> + Send;
}

/// Opens a connection and hands back both halves of it together.
///
/// One factory rather than two independently-refreshable parameters: a
/// reconnect rebuilds the request path and the subscription at the same
/// moment, and a signature that let a caller replace one without the other
/// would admit a state the real connection cannot be in.
#[allow(dead_code)]
pub trait Shepherd: Send {
    /// This connection's request half.
    type Flock: FlockSource;
    /// This connection's subscription half.
    type Events: EventSource;

    /// Connects and subscribes.
    ///
    /// # Errors
    /// [`LinkError::Unreachable`] when the socket would not answer or the
    /// handshake failed, [`LinkError::Refused`] when it answered and then
    /// refused the subscription.
    fn link(
        &mut self,
    ) -> impl Future<Output = Result<(Self::Flock, Self::Events), LinkError>> + Send;
}

/// Why opening a connection failed.
///
/// No `#[non_exhaustive]`, and that is a decision rather than an oversight:
/// IR-20's obligation is on `pub` error enums in LIBRARY crates, and shep-cli
/// is `[[bin]]`-only — there is no downstream to break, and every match on this
/// type is in this crate. Stated here rather than left silent, which is the
/// half of IR-20 that applies either way.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// Nothing answered at the socket, or the handshake did not complete.
    Unreachable(String),
    /// The shepherd answered and speaks a different wire version.
    ///
    /// Held apart from [`Self::Unreachable`] for one reason: it is the single
    /// connect failure with its own exit code, and `main.rs`'s
    /// `connect_client` — the path every other client verb takes — already
    /// makes that distinction. A lookout that reported a version skew as
    /// "the shepherd did not answer" would send the operator to check whether
    /// the daemon is running, which it is.
    Protocol(String),
    /// The shepherd answered but refused the subscription.
    Refused(String),
}

impl LinkError {
    /// The exit code this reports when it happens on the FIRST dial, before
    /// the dashboard exists.
    ///
    /// Only the first dial reaches this: once a link has been established, a
    /// failure is a rung on [`super::link::run_link`]'s ladder and never an
    /// exit. Derived from `ExitCode::from(&ConnectError)` at conversion time
    /// rather than re-decided here, so this and every other verb's mapping
    /// cannot drift apart.
    #[allow(dead_code)]
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Protocol(_) => ExitCode::ProtocolMismatch,
            Self::Unreachable(_) | Self::Refused(_) => ExitCode::DaemonUnreachable,
        }
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "the shepherd did not answer: {why}"),
            Self::Protocol(why) => write!(f, "{why}"),
            Self::Refused(why) => write!(f, "the shepherd refused the subscription: {why}"),
        }
    }
}

impl core::error::Error for LinkError {}

impl From<ConnectError> for LinkError {
    fn from(err: ConnectError) -> Self {
        // `ExitCode::from(&ConnectError)` is the existing taxonomy — reused
        // rather than re-derived, so the two cannot skew.
        if ExitCode::from(&err) == ExitCode::ProtocolMismatch {
            return Self::Protocol(err.to_string());
        }
        Self::Unreachable(err.to_string())
    }
}

/// The request half of a live connection.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ClientFlock(Client);

impl FlockSource for ClientFlock {
    async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
        match self.0.request(Request::ListFlock).await? {
            Response::Flock(flock) => Ok(flock),
            // `Response` is `#[non_exhaustive]`; a reply this binary does not
            // recognise is not a reason to tear the dashboard down, and the
            // next poll asks again.
            _unrecognised => Ok(Vec::new()),
        }
    }
}

impl EventSource for EventStream {
    async fn next_event(&mut self) -> Option<Result<BusEvent, Lagged>> {
        self.next().await
    }
}

/// The real thing: a socket path that can be dialled again.
#[allow(dead_code)]
#[derive(Debug)]
pub struct UnixShepherd {
    socket: PathBuf,
}

impl UnixShepherd {
    /// Watches the shepherd listening at `socket`.
    #[allow(dead_code)]
    #[must_use]
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }
}

impl Shepherd for UnixShepherd {
    type Flock = ClientFlock;
    type Events = EventStream;

    async fn link(&mut self) -> Result<(Self::Flock, Self::Events), LinkError> {
        // `Client::connect`, never `connect_or_spawn`: opening a dashboard
        // must not start a shepherd, and a RECONNECT starting one would be
        // worse still — it would resurrect a supervisor the operator may have
        // just killed on purpose, from a process whose whole job is to watch.
        // `main.rs`'s own dispatch draws the same line for every verb but
        // `start` and `muster`.
        let client = Client::connect(&self.socket).await?;
        let topics = TOPICS.iter().map(|topic| (*topic).to_string()).collect();
        let stream = client
            .subscribe(topics)
            .await
            .map_err(|err| LinkError::Refused(err.to_string()))?;
        Ok((ClientFlock(client), stream))
    }
}
