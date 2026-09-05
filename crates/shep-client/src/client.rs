//! The connected [`Client`] handle: [`Client::connect`], [`Client::request`],
//! [`RequestError`].
//!
//! A `Client` is a thin, actor-backed handle: the socket itself is owned by
//! the task [`crate::actor::spawn`] starts, and every method here sends a
//! command to that task and awaits the answer. `&self` is enough for every
//! method, so concurrent callers share one `Client` (behind an `Arc`, or
//! just a shared reference) instead of cloning a handle per caller.

use core::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use shep_core::protocol::{HelloAck, Request, Response, RpcError, WireError};

use crate::actor::{self, Command};
use crate::connection::{ConnectError, Connection, HANDSHAKE_TIMEOUT};
use crate::events::EventStream;

/// Daemon-side budget applied when a caller names none. Mirrors the daemon's
/// own `DEFAULT_DEADLINE_MS = 5_000` (`shep-daemon/src/rpc.rs`).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);

/// Budget for `Request::Start`. A cold spawn plus a readiness probe routinely
/// outruns the 5s default, and the daemon clamps anything over
/// `MAX_DEADLINE_MS = 60_000` (`shep-daemon/src/rpc.rs:38`), so this is well
/// inside what the daemon will honour.
pub const START_DEADLINE: Duration = Duration::from_secs(30);

/// Budget for the log-plane verbs that walk the flock file by file:
/// `Request::Reopen` and `Request::Flush`.
///
/// The daemon visits matched sheep one after another with no per-sheep
/// bound, so a wedged or NFS-backed log directory can make the whole walk
/// take as long as the kernel does. Same 30s as [`START_DEADLINE`],
/// comfortably inside the daemon's own clamp.
pub const LOG_PLANE_DEADLINE: Duration = Duration::from_secs(30);

/// Budget for `Request::Trigger`.
///
/// An app's own `AppConfig::action_timeout` can reach `MAX_ACTION_TIMEOUT`
/// (58s), 2s under the daemon's own 60s clamp, so this asks for the full
/// 60s rather than abandon a reply the daemon is still building.
pub const TRIGGER_DEADLINE: Duration = Duration::from_secs(60);

/// How much longer the client waits than the deadline it asked the daemon
/// to honour, so it doesn't report a timeout for work that succeeded.
pub const DEADLINE_GRACE: Duration = Duration::from_secs(2);

/// Why a [`Client::request`] (or [`Client::request_with_deadline`]) call failed.
///
/// Non-exhaustive: expect more variants.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestError {
    /// The daemon accepted the request and answered it with a structured error.
    Rpc(RpcError),
    /// No reply arrived within the request's own deadline plus [`DEADLINE_GRACE`].
    Timeout {
        /// The client-side budget that was exceeded.
        after: Duration,
    },
    /// The connection closed (daemon exit, crash, or a prior [`Client::close`])
    /// before this request's reply arrived.
    Closed,
    /// `body` failed to encode onto the wire.
    Wire(WireError),
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(err) => write!(f, "the daemon reported {:?}: {}", err.code, err.message),
            Self::Timeout { after } => write!(f, "no reply within {after:?}"),
            Self::Closed => f.write_str("the connection closed before a reply arrived"),
            Self::Wire(err) => write!(f, "request frame error: {err}"),
        }
    }
}

impl core::error::Error for RequestError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Wire(err) => Some(err),
            Self::Rpc(_) | Self::Timeout { .. } | Self::Closed => None,
        }
    }
}

/// A live connection to the daemon.
///
/// Backed by one actor task (see the crate's `actor` module) that owns the
/// socket; `request`/`request_with_deadline`/`close` all take `&self`, so
/// callers share one `Client` behind an `Arc` or a reference rather than
/// cloning a handle per caller.
pub struct Client {
    commands: mpsc::Sender<Command>,
    ack: HelloAck,
    socket: PathBuf,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("socket", &self.socket)
            .field("ack", &self.ack)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connects to `socket` and performs the version handshake, bounded by
    /// [`HANDSHAKE_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// See [`Self::connect_with_timeout`].
    pub async fn connect(socket: &Path) -> Result<Self, ConnectError> {
        Self::connect_with_timeout(socket, HANDSHAKE_TIMEOUT).await
    }

    /// As [`Self::connect`], but with a caller-supplied handshake timeout.
    ///
    /// # Errors
    ///
    /// - [`ConnectError::Connect`]: the initial `connect(2)` call failed.
    /// - [`ConnectError::Wire`]: `Hello` failed to encode, or the reply failed to decode.
    /// - [`ConnectError::Io`]: a framed read or write failed after connect.
    /// - [`ConnectError::HandshakeClosed`]: the peer closed before a `HelloReply`.
    /// - [`ConnectError::HandshakeTimeout`]: no `HelloReply` arrived within `timeout`.
    /// - [`ConnectError::ProtocolMismatch`]: the daemon refused on protocol-version skew.
    pub async fn connect_with_timeout(
        socket: &Path,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        Self::connect_as(socket, timeout, None).await
    }

    /// As [`Self::connect_with_timeout`], but announcing this client as the
    /// dog registered under `dog_name`.
    ///
    /// Crate-private: the public constructors above pass `None`, and only
    /// [`ReconnectingClient`](crate::ReconnectingClient) passes a name. A
    /// client that could claim an arbitrary dog name could get that dog
    /// restarted on its own say-so.
    ///
    /// # Errors
    ///
    /// See [`Self::connect_with_timeout`].
    pub(crate) async fn connect_as(
        socket: &Path,
        timeout: Duration,
        dog_name: Option<&str>,
    ) -> Result<Self, ConnectError> {
        let connection = Connection::open(socket, timeout, dog_name).await?;
        let (frames, ack) = connection.into_parts();
        let commands = actor::spawn(frames);
        Ok(Self {
            commands,
            ack,
            socket: socket.to_path_buf(),
        })
    }

    /// The daemon's handshake acknowledgement.
    #[must_use]
    pub fn daemon(&self) -> &HelloAck {
        &self.ack
    }

    /// Resolves once this connection has ended: daemon exit, crash,
    /// `execve`, a write failure, or a prior [`Self::close`]. Resolves
    /// immediately if already gone, and may be awaited more than once.
    ///
    /// This does not reconnect, and [`Self::request`] does not retry. See
    /// [`ReconnectingClient`](crate::ReconnectingClient) for the supervised
    /// wrapper.
    pub async fn closed(&self) {
        self.commands.closed().await;
    }

    /// The path this client is connected through.
    ///
    /// `HelloAck` doesn't carry the socket path, so the `Client` keeps the
    /// `PathBuf` it connected with, for a caller that needs it after
    /// teardown.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Sends `body` with [`DEFAULT_DEADLINE`].
    ///
    /// Shorthand for [`Self::request_with_deadline`]`(body, None)`.
    ///
    /// # Errors
    ///
    /// See [`Self::request_with_deadline`].
    pub async fn request(&self, body: Request) -> Result<Response, RequestError> {
        self.request_with_deadline(body, None).await
    }

    /// Sends `body` with `deadline`, or [`DEFAULT_DEADLINE`] if `None`. The
    /// client waits `deadline` plus [`DEADLINE_GRACE`] for a reply before
    /// giving up locally.
    ///
    /// # Errors
    ///
    /// - [`RequestError::Rpc`]: the daemon answered with a structured error.
    /// - [`RequestError::Timeout`]: no reply within `deadline + DEADLINE_GRACE`.
    /// - [`RequestError::Closed`]: the connection closed before a reply arrived.
    /// - [`RequestError::Wire`]: `body` failed to encode.
    pub async fn request_with_deadline(
        &self,
        body: Request,
        deadline: Option<Duration>,
    ) -> Result<Response, RequestError> {
        let deadline = deadline.unwrap_or(DEFAULT_DEADLINE);
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands
            .send(Command::Request {
                body,
                deadline_ms: Some(millis(deadline)),
                reply_to,
            })
            .await
            .map_err(|_send_error| RequestError::Closed)?;

        // Saturating: `deadline` is the caller's, and `Duration`'s `Add`
        // panics rather than saturating on overflow.
        let budget = deadline.saturating_add(DEADLINE_GRACE);
        match tokio::time::timeout(budget, reply_rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_recv_error)) => Err(RequestError::Closed),
            Err(_elapsed) => Err(RequestError::Timeout { after: budget }),
        }
    }

    /// Subscribes this connection to `topics`: dotted glob patterns matched
    /// against [`shep_core::protocol::BusEvent::topic`] (`process.*`,
    /// `log.*`, `daemon.*`, ...).
    ///
    /// A second call on the same `Client` replaces the daemon-side filter
    /// rather than adding to it; a caller wanting two topic sets needs two
    /// `Client`s. The returned [`EventStream`]'s receiver is installed
    /// before the `Subscribe` request is sent, so no pushed event is missed.
    ///
    /// # Errors
    ///
    /// Same as [`Self::request`].
    pub async fn subscribe(&self, topics: Vec<String>) -> Result<EventStream, RequestError> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands
            .send(Command::Subscribe { reply_to })
            .await
            .map_err(|_send_error| RequestError::Closed)?;
        let receiver = reply_rx.await.map_err(|_recv_error| RequestError::Closed)?;

        // reply is `Response::Subscribed`; unchecked, since interpreting
        // `Response` variants is shep-cli's job, not this crate's
        self.request(Request::Subscribe { topics }).await?;
        Ok(EventStream::new(receiver))
    }

    /// Closes the connection.
    ///
    /// Drops the command channel to the actor task, which ends the actor's
    /// loop and drops the underlying socket.
    ///
    /// # Errors
    ///
    /// Never fails today. `Result` leaves room for a later, more graceful
    /// teardown (draining in-flight requests before dropping, say) to start
    /// returning one without an API break.
    pub async fn close(self) -> Result<(), RequestError> {
        drop(self.commands);
        Ok(())
    }
}

/// Saturating `Duration` to wire milliseconds. A caller-supplied `Duration`
/// above the wire range saturates at `u64::MAX` ms rather than overflowing.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
