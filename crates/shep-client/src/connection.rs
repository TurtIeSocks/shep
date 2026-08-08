//! The bounded connect-plus-handshake: [`Connection::open`], [`ConnectError`]
//!
//! A bound-but-not-accepting unix socket still completes `connect(2)` into
//! the kernel backlog, so a bare connect is never a readiness probe by
//! itself (spec's readiness trap). The only thing that counts as "the
//! daemon is up" is a completed version handshake: connect, send [`Hello`],
//! receive a [`HelloAck`]. [`Connection::open`] performs exactly that,
//! bounded end-to-end by one [`tokio::time::timeout`] so a backlogged
//! socket times out instead of hanging forever.

use core::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use shep_core::protocol::{
    Hello, HelloAck, HelloReply, PROTOCOL_VERSION, WireError, codec, decode_frame, encode_frame,
};

/// Budget for one connect-plus-handshake attempt.
///
/// Deliberately mirrors the daemon's own `HANDSHAKE_TIMEOUT_MS = 5_000`
/// (`shep-daemon/src/server.rs:41`) so neither side out-waits the other.
/// The single constant the `spawn` module's own connect-and-wait budget
/// reads from, so the two never drift apart.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The framed transport a [`Connection`] wraps, named so callers past this
/// module (the actor task that takes ownership of it) don't have to spell
/// out `Framed<UnixStream, LengthDelimitedCodec>` themselves.
pub(crate) type Frames = Framed<UnixStream, LengthDelimitedCodec>;

/// Why `Connection::open` failed.
///
/// Growth is expected — this is a library crate's public error type (IR-20).
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectError {
    /// `connect(2)` itself failed — nothing is listening at `path` (no
    /// socket file, permission denied, connection refused, ...).
    Connect {
        /// The socket path that was dialed.
        path: PathBuf,
        /// The OS error `connect` returned.
        source: std::io::Error,
    },
    /// A framed read or write failed after the connection was established.
    Io(std::io::Error),
    /// The `Hello` failed to encode, or the reply frame failed to decode.
    Wire(WireError),
    /// The peer closed the connection before a [`HelloReply`] arrived.
    HandshakeClosed,
    /// Connect succeeded but no [`HelloReply`] (and no close) arrived within
    /// the timeout. Distinct from [`Self::Connect`]: a refusal means nothing
    /// is listening, a timeout means something is bound but not answering.
    HandshakeTimeout {
        /// The timeout that was exceeded.
        after: Duration,
    },
    /// The daemon refused the handshake on protocol-version skew. `client`
    /// is our own [`PROTOCOL_VERSION`]; `message` is the daemon's own
    /// sentence, verbatim — do not parse it (the daemon's version exists
    /// only inside this prose, never as a separate field).
    ProtocolMismatch {
        /// This client's own protocol version.
        client: u32,
        /// The daemon's refusal message, verbatim.
        message: String,
    },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect { path, source } => {
                write!(f, "could not connect to `{}`: {source}", path.display())
            }
            Self::Io(err) => write!(f, "connection I/O error: {err}"),
            Self::Wire(err) => write!(f, "handshake frame error: {err}"),
            Self::HandshakeClosed => {
                f.write_str("the daemon closed the connection during the handshake")
            }
            Self::HandshakeTimeout { after } => {
                write!(f, "the handshake did not complete within {after:?}")
            }
            Self::ProtocolMismatch { client, message } => {
                write!(
                    f,
                    "protocol mismatch (this client speaks {client}): {message}"
                )
            }
        }
    }
}

impl core::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Connect { source, .. } | Self::Io(source) => Some(source),
            Self::Wire(err) => Some(err),
            Self::HandshakeClosed
            | Self::HandshakeTimeout { .. }
            | Self::ProtocolMismatch { .. } => None,
        }
    }
}

/// One handshaken connection to the daemon: an established, version-checked
/// framed transport plus the daemon's [`HelloAck`].
pub(crate) struct Connection {
    frames: Frames,
    ack: HelloAck,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection")
            .field("ack", &self.ack)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Connects to `socket` and performs the version handshake, bounding
    /// the whole attempt — connect, send [`Hello`], read the reply — by
    /// `timeout`.
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
    pub(crate) async fn open(socket: &Path, timeout: Duration) -> Result<Self, ConnectError> {
        // `timeout` bounds `connect(2)` and the handshake together, not just
        // the handshake — intentional, but barely testable over AF_UNIX (a
        // mutant moving `connect` outside this timeout still passes all
        // five tests below). Not directly covered; don't contort a test to
        // chase it.
        tokio::time::timeout(timeout, Self::open_inner(socket))
            .await
            .map_err(|_elapsed| ConnectError::HandshakeTimeout { after: timeout })?
    }

    async fn open_inner(socket: &Path) -> Result<Self, ConnectError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|source| ConnectError::Connect {
                path: socket.to_path_buf(),
                source,
            })?;
        let mut frames = Framed::new(stream, codec());

        let hello = Hello {
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
        };
        let payload = encode_frame(&hello).map_err(ConnectError::Wire)?;
        frames.send(payload).await.map_err(ConnectError::Io)?;

        let frame = frames
            .next()
            .await
            .ok_or(ConnectError::HandshakeClosed)?
            .map_err(ConnectError::Io)?;
        let reply: HelloReply = decode_frame(&frame).map_err(ConnectError::Wire)?;
        // Flattens every `RpcError` into `ProtocolMismatch` regardless of
        // `code` — sound only because `server.rs` is the sole producer today
        // and always sends `ProtocolMismatch` (shep-daemon/src/server.rs:387-397).
        let ack = reply.map_err(|err| ConnectError::ProtocolMismatch {
            client: PROTOCOL_VERSION,
            message: err.message,
        })?;

        Ok(Self { frames, ack })
    }

    /// Splits the connection into its raw framed transport and the
    /// handshake acknowledgement, for the actor task that takes ownership
    /// of the transport and needs both.
    pub(crate) fn into_parts(self) -> (Frames, HelloAck) {
        (self.frames, self.ack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_client_testing::fake_daemon;
    use shep_core::protocol::{HelloAck, PROTOCOL_VERSION, RpcError, RpcErrorCode};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn open_sends_hello_and_returns_the_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let ack = HelloAck {
            daemon_version: "9.9.9".into(),
            protocol: PROTOCOL_VERSION,
            pid: 4242,
        };
        let served = fake_daemon(&path, Ok(ack.clone())).await;

        let conn = Connection::open(&path, HANDSHAKE_TIMEOUT).await.unwrap();

        let (_frames, actual_ack) = conn.into_parts();
        assert_eq!(actual_ack, ack);
        let hello = served.await.unwrap();
        assert_eq!(
            hello.protocol, PROTOCOL_VERSION,
            "the client must announce the version it speaks"
        );
        assert_eq!(
            hello.client_version,
            env!("CARGO_PKG_VERSION"),
            "the client must identify its own version"
        );
    }

    #[tokio::test]
    async fn a_protocol_refusal_becomes_its_own_error_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let refusal = RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 2, client speaks 1".into(),
        };
        let _served = fake_daemon(&path, Err(refusal)).await;

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT)
            .await
            .unwrap_err();

        let ConnectError::ProtocolMismatch { client, message } = err else {
            panic!("a protocol refusal must not be flattened into a generic error, got {err:?}");
        };
        assert_eq!(
            client, PROTOCOL_VERSION,
            "`client` is our own version, not the daemon's"
        );
        assert!(
            message.contains("protocol 2"),
            "the daemon's own message must survive: {message}"
        );
    }

    #[tokio::test]
    async fn a_daemon_that_closes_without_answering_is_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        assert!(matches!(
            Connection::open(&path, HANDSHAKE_TIMEOUT).await,
            Err(ConnectError::HandshakeClosed)
        ));
    }

    #[tokio::test]
    async fn connecting_to_a_missing_socket_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let ConnectError::Connect { path: reported, .. } =
            Connection::open(&path, HANDSHAKE_TIMEOUT)
                .await
                .unwrap_err()
        else {
            panic!("a missing socket must report which path failed");
        };
        assert_eq!(reported, path);
    }

    /// The bound-but-never-accepted case, at the `Connection` layer. The
    /// kernel completes `connect()` into the backlog, so only the timeout
    /// ends this. Real timings would make it a 5s test; 150ms proves the
    /// same thing.
    #[tokio::test]
    async fn a_socket_bound_but_never_accepted_from_times_out_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let _listener = UnixListener::bind(&path).unwrap(); // bound; never accepted from

        let err = Connection::open(&path, Duration::from_millis(150))
            .await
            .unwrap_err();

        let ConnectError::HandshakeTimeout { after } = err else {
            panic!("a backlogged connect must time out, not hang or read as success; got {err:?}");
        };
        assert_eq!(after, Duration::from_millis(150));
    }
}
