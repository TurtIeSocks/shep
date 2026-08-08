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
/// own `DEFAULT_DEADLINE_MS = 5_000` (`shep-daemon/src/rpc.rs:36`), which is
/// what an `Envelope` with `deadline_ms: None` would get anyway — stated here
/// so the value is a decision, not an inheritance.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);

/// Budget for `Request::Start`. A cold spawn plus a readiness probe routinely
/// outruns the 5s default, and the daemon clamps anything over
/// `MAX_DEADLINE_MS = 60_000` (`shep-daemon/src/rpc.rs:38`), so this is well
/// inside what the daemon will honour.
pub const START_DEADLINE: Duration = Duration::from_secs(30);

/// How much longer the client waits than the deadline it asked the daemon to
/// honour. Without a gap the client abandons a request the daemon is still
/// legitimately working on, and the user sees a timeout for work that
/// succeeded (IR-26: named, not a magic `+ 2`).
pub const DEADLINE_GRACE: Duration = Duration::from_secs(2);

/// Why a [`Client::request`] (or [`Client::request_with_deadline`]) call failed.
///
/// Growth is expected — library-crate public error type (IR-20).
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
    /// The connection closed — daemon exit, crash, or a prior [`Client::close`]
    /// — before this request's reply arrived.
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
/// callers share one `Client` — behind an `Arc`, or just a reference —
/// rather than cloning a handle per caller.
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
    /// See [`Self::connect_with_timeout`] — every error variant it can
    /// return, this returns unchanged.
    pub async fn connect(socket: &Path) -> Result<Self, ConnectError> {
        Self::connect_with_timeout(socket, HANDSHAKE_TIMEOUT).await
    }

    /// As [`Self::connect`], but with a caller-supplied handshake timeout —
    /// for callers that deliberately want a tighter or looser bound than
    /// [`HANDSHAKE_TIMEOUT`] (a test exercising the timeout path itself, say).
    ///
    /// # Errors
    ///
    /// - [`ConnectError::Connect`] — `connect(2)` failed; nothing is
    ///   listening at `socket`.
    /// - [`ConnectError::Wire`] — the `Hello` failed to encode, or the
    ///   reply frame failed to decode.
    /// - [`ConnectError::Io`] — a framed read or write failed after connect.
    /// - [`ConnectError::HandshakeClosed`] — the peer closed the connection
    ///   before sending a `HelloReply`.
    /// - [`ConnectError::HandshakeTimeout`] — connect succeeded but no
    ///   `HelloReply` (or close) arrived within `timeout`.
    /// - [`ConnectError::ProtocolMismatch`] — the daemon refused the
    ///   handshake on protocol-version skew.
    pub async fn connect_with_timeout(
        socket: &Path,
        timeout: Duration,
    ) -> Result<Self, ConnectError> {
        let connection = Connection::open(socket, timeout).await?;
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

    /// The path this client is connected through.
    ///
    /// `HelloAck` carries `daemon_version`, `protocol` and `pid` and
    /// nothing else (`shep-core/src/protocol/request.rs:20-28`), so the
    /// socket path cannot be recovered from the handshake — a caller that
    /// needs to detect the socket file disappearing during teardown has to
    /// keep the path some other way, so the `Client` keeps the `PathBuf` it
    /// connected with.
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

    /// Sends `body` with `deadline` (or [`DEFAULT_DEADLINE`] if `None`),
    /// stated explicitly on the envelope's `deadline_ms` — never left as
    /// `None`, so the daemon's own default is a decision this client makes
    /// on the caller's behalf, not one it silently inherits.
    ///
    /// The client itself waits `deadline + `[`DEADLINE_GRACE`]` for a reply
    /// before giving up locally, a separate bound from the one the daemon
    /// was asked to honour.
    ///
    /// # Errors
    ///
    /// - [`RequestError::Rpc`] — the daemon answered with a structured error.
    /// - [`RequestError::Timeout`] — no reply within `deadline + DEADLINE_GRACE`.
    /// - [`RequestError::Closed`] — the connection closed before a reply arrived.
    /// - [`RequestError::Wire`] — `body` failed to encode.
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

        let budget = deadline + DEADLINE_GRACE;
        match tokio::time::timeout(budget, reply_rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_recv_error)) => Err(RequestError::Closed),
            Err(_elapsed) => Err(RequestError::Timeout { after: budget }),
        }
    }

    /// Subscribes this connection to `topics` — dotted glob patterns matched
    /// against [`shep_core::protocol::BusEvent::topic`] (`process.*`,
    /// `log.*`, `daemon.*`, ...).
    ///
    /// A second call on the same `Client` **replaces** the daemon-side
    /// filter rather than adding to it (`shep-daemon/src/server.rs:360-364`
    /// — the daemon keeps one subscriber filter per connection, not a
    /// growing union). A caller that wants two independent topic sets needs
    /// two `Client`s, each with its own connection.
    ///
    /// The returned [`EventStream`]'s receiver is installed on the actor
    /// *before* the `Subscribe` request is sent, so no event the daemon
    /// pushes between it answering `Subscribed` and this call returning can
    /// be missed — a daemon that answers and immediately starts pushing
    /// (the real one does exactly that) cannot race ahead of a subscriber
    /// that isn't listening yet.
    ///
    /// # Errors
    ///
    /// Same as [`Self::request`]: a daemon-side [`RpcError`], a client-side
    /// timeout, a closed connection, or a wire encode failure.
    pub async fn subscribe(&self, topics: Vec<String>) -> Result<EventStream, RequestError> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands
            .send(Command::Subscribe { reply_to })
            .await
            .map_err(|_send_error| RequestError::Closed)?;
        let receiver = reply_rx.await.map_err(|_recv_error| RequestError::Closed)?;

        // The reply is expected to be `Response::Subscribed`, but it is
        // deliberately unexamined here: no unexpected-response error variant
        // exists yet, and the Global Constraints put `Response` variant
        // interpretation in shep-cli, not this crate.
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

/// Saturating `Duration` -> wire milliseconds. Every deadline this crate
/// sends comes from its own named constants or a caller-supplied
/// `Duration`, none of which come remotely close to `u64::MAX` ms (over 500
/// million years) — saturating rather than panicking here is a belt, not a
/// live path.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
