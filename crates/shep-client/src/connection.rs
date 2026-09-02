//! The bounded connect-plus-handshake: [`Connection::open`], [`ConnectError`]
//!
//! A bound-but-not-accepting unix socket still completes `connect(2)` into
//! the kernel backlog, so a bare connect is never a readiness probe by
//! itself (spec's readiness trap). The only thing that counts as "the
//! daemon is up" is a completed version handshake: connect, send [`Hello`],
//! receive a [`HelloAck`]. [`Connection::open`] performs exactly that,
//! bounded end-to-end by one [`tokio::time::timeout`] so a backlogged
//! socket times out instead of hanging forever.
//!
//! Nothing here names an OS transport type. Which carrier is underneath —
//! an `AF_UNIX` socket or a Windows named pipe — is
//! [`shep_core::transport`]'s to decide, and the readiness trap above has
//! an exact analogue on both: a pipe whose every server instance is busy
//! completes no connect either, and the same single timeout covers it.

use core::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use shep_core::protocol::{
    Hello, HelloAck, HelloReply, PROTOCOL_VERSION, WireError, codec, decode_frame, encode_frame,
};
use shep_core::transport::{self, ClientStream};

/// Budget for one connect-plus-handshake attempt.
///
/// Deliberately mirrors the daemon's own `HANDSHAKE_TIMEOUT_MS = 5_000`
/// (`shep-daemon/src/server.rs:41`) so neither side out-waits the other.
/// The single constant the `spawn` module's own connect-and-wait budget
/// reads from, so the two never drift apart.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The framed transport a [`Connection`] wraps, named so callers past this
/// module (the actor task that takes ownership of it) don't have to spell
/// out `Framed<ClientStream, LengthDelimitedCodec>` themselves.
///
/// [`ClientStream`] is the per-platform carrier, so this one alias is what
/// keeps `crate::actor` — which owns a `Frames` for the connection's whole
/// life — free of any platform gate at all.
pub(crate) type Frames = Framed<ClientStream, LengthDelimitedCodec>;

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
    /// sentence, verbatim — still not for parsing, and no longer the only
    /// thing a caller gets: `daemon_version` carries the running daemon's
    /// version as data.
    ///
    /// The [`Display`](fmt::Display) rendering of this variant is a public
    /// surface in its own right, and a load-bearing one: a refused dog is
    /// refused before it can issue a single request, so this string is the
    /// entire account of the failure that reaches its log. It states the
    /// skew and then what to do about it — see the impl for why it names
    /// both directions of the remedy rather than picking one.
    ProtocolMismatch {
        /// This client's own protocol version.
        client: u32,
        /// The daemon's own crate version, when it named one.
        ///
        /// `None` from a daemon built before the refusal carried it, which
        /// no upgrade can change: read it as "unknown" and take the
        /// conservative path rather than assuming an old daemon or a new
        /// one.
        daemon_version: Option<String>,
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
            // The skew itself is stated once, by the daemon's own `message`,
            // and this line adds the two things that sentence cannot carry:
            // what to do, and which shep to do it against.
            //
            // `daemon_version` USED to be deliberately unrendered, on the
            // grounds that `message` already names the daemon's side. That
            // held while this was a bare statement of the skew: `message`
            // names the daemon's PROTOCOL, and a protocol number is enough to
            // describe the mismatch. It stops holding the moment the line
            // says "rebuild against shep X", because a protocol number is not
            // something anyone can install — the crate version is. So the
            // field is rendered now, and only inside the remedy.
            //
            // The remedy names both directions because this type cannot tell
            // them apart. `client` is a number and `message` is prose, so
            // nothing here knows whether this build or the running shepherd
            // is the older one, and a line that guessed would send half its
            // readers to reinstall the wrong thing — the exact failure the
            // silent-dog verdict in `shep-daemon`'s `dogs.rs` was rewritten
            // to stop making. Naming both costs one clause and is true
            // either way.
            Self::ProtocolMismatch {
                client,
                daemon_version,
                message,
            } => {
                // A daemon too old to have sent its version still gets a
                // usable sentence: "the running shep" is a thing the reader
                // can identify, where a bare "shep" would leave them
                // guessing which one this program failed to reach.
                let shep = daemon_version
                    .as_deref()
                    .map_or_else(|| "the running shep".to_string(), |v| format!("shep {v}"));
                write!(
                    f,
                    "protocol mismatch (this client speaks {client}): {message}. \
                     The older of the two has to be replaced: rebuild or reinstall this \
                     program against {shep} and restart it, or upgrade shep itself and run \
                     `shep daemon reload`"
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
    /// `dog_name` is the name this client was registered under as a dog, or
    /// `None` for every client that is not one. It travels in the `Hello`
    /// so a daemon that REFUSES this handshake knows which dog it just
    /// refused: a refused handshake never reaches a request, so nothing
    /// later on the connection could tell it (the handover design's G8).
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
    pub(crate) async fn open(
        socket: &Path,
        timeout: Duration,
        dog_name: Option<&str>,
    ) -> Result<Self, ConnectError> {
        // `timeout` bounds `connect(2)` and the handshake together, not just
        // the handshake — intentional, but barely testable over AF_UNIX (a
        // mutant moving `connect` outside this timeout still passes all
        // five tests below). Not directly covered; don't contort a test to
        // chase it.
        tokio::time::timeout(timeout, Self::open_inner(socket, dog_name))
            .await
            .map_err(|_elapsed| ConnectError::HandshakeTimeout { after: timeout })?
    }

    async fn open_inner(socket: &Path, dog_name: Option<&str>) -> Result<Self, ConnectError> {
        let stream = transport::connect(socket)
            .await
            .map_err(|source| ConnectError::Connect {
                path: socket.to_path_buf(),
                source,
            })?;
        let mut frames = Framed::new(stream, codec());

        let hello = Hello {
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            dog_name: dog_name.map(str::to_owned),
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
            // Carried through rather than dropped with the rest of the
            // `RpcError`: this is the only rebuild between the wire and the
            // caller, so a field lost here is lost entirely.
            daemon_version: err.daemon_version,
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
    use crate::testing::fake_daemon;
    use shep_core::protocol::{HelloAck, PROTOCOL_VERSION, RpcError, RpcErrorCode};
    use shep_core::transport::Listener;

    #[tokio::test]
    async fn open_sends_hello_and_returns_the_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::testing::control_address(dir.path());
        let ack = HelloAck {
            daemon_version: "9.9.9".into(),
            protocol: PROTOCOL_VERSION,
            pid: 4242,
        };
        let served = fake_daemon(&path, Ok(ack.clone())).await;

        let conn = Connection::open(&path, HANDSHAKE_TIMEOUT, None)
            .await
            .unwrap();

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
        assert_eq!(
            hello.dog_name, None,
            "a client that is not a dog must not claim to be one"
        );
    }

    /// fails if a dog's name is dropped between [`Connection::open`]'s
    /// argument and the frame that goes out. It is the whole of what the
    /// daemon has to work with when it REFUSES this handshake, and a
    /// refused handshake never reaches a request, so nothing later on the
    /// connection could supply it (the handover design's G8).
    #[tokio::test]
    async fn a_dogs_hello_carries_the_name_it_was_registered_under() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::testing::control_address(dir.path());
        let ack = HelloAck {
            daemon_version: "9.9.9".into(),
            protocol: PROTOCOL_VERSION,
            pid: 4242,
        };
        let served = fake_daemon(&path, Ok(ack)).await;

        let _conn = Connection::open(&path, HANDSHAKE_TIMEOUT, Some("metrics"))
            .await
            .unwrap();

        let hello = served.await.unwrap();
        assert_eq!(hello.dog_name.as_deref(), Some("metrics"));
    }

    #[tokio::test]
    async fn a_protocol_refusal_becomes_its_own_error_variant() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::testing::control_address(dir.path());
        let refusal = RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 2, client speaks 1".into(),
            daemon_version: None,
        };
        let _served = fake_daemon(&path, Err(refusal)).await;

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT, None)
            .await
            .unwrap_err();

        let ConnectError::ProtocolMismatch {
            client, message, ..
        } = err
        else {
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
    async fn a_refusal_carries_the_daemon_version_past_the_flattening() {
        // `open_inner` rebuilds the `RpcError` into a `ConnectError`, so a
        // field the daemon sends is only useful if the rebuild carries it.
        // The daemon-side test cannot see this half.
        let dir = tempfile::tempdir().unwrap();
        let path = crate::testing::control_address(dir.path());
        let refusal = RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 2, client speaks 1".into(),
            daemon_version: Some("0.1.16".into()),
        };
        let _served = fake_daemon(&path, Err(refusal)).await;

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT, None)
            .await
            .unwrap_err();

        let ConnectError::ProtocolMismatch { daemon_version, .. } = err else {
            panic!("expected a protocol refusal, got {err:?}");
        };
        assert_eq!(daemon_version.as_deref(), Some("0.1.16"));
    }

    #[tokio::test]
    async fn an_old_daemons_refusal_still_connects_and_reports_no_version() {
        // A daemon predating this field sends no `daemon_version`, and
        // `skip_serializing_if` means `None` puts exactly those bytes on the
        // wire — the old daemon's frame, byte for byte. It must decode
        // cleanly and read as `None` rather than failing the handshake, or
        // this field breaks the one upgrade it exists to smooth.
        let dir = tempfile::tempdir().unwrap();
        let path = crate::testing::control_address(dir.path());
        let refusal = RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 2, client speaks 1".into(),
            daemon_version: None,
        };
        // That `None` really is absent from the wire, and not a `null` key,
        // is pinned byte-for-byte next to the field itself
        // (`shep-core/src/protocol/request.rs`); this crate has no
        // `serde_json` to assert it with and does not need one.
        let _served = fake_daemon(&path, Err(refusal)).await;

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT, None)
            .await
            .unwrap_err();

        let ConnectError::ProtocolMismatch {
            daemon_version,
            message,
            ..
        } = err
        else {
            panic!("expected a protocol refusal, got {err:?}");
        };
        assert_eq!(daemon_version, None);
        assert!(
            message.contains("protocol 2"),
            "the daemon's own message must still survive: {message}"
        );
    }

    /// fails if the rendered refusal states the skew and stops.
    ///
    /// This string is what a dog writes into its own log, and it is the
    /// whole of what an operator gets: the dog is refused before it can ask
    /// for anything, so there is no later line to explain this one. A
    /// sentence naming the two protocol numbers and nothing else leaves them
    /// to invent the next step, which is how the production incident behind
    /// this phase turned into two days of reinstalling.
    #[test]
    fn a_rendered_refusal_says_what_to_do_about_it() {
        let rendered = ConnectError::ProtocolMismatch {
            client: 1,
            daemon_version: Some("0.1.27".into()),
            message: "daemon speaks protocol 2, client sent 1".into(),
        }
        .to_string();

        assert!(
            rendered.contains("daemon speaks protocol 2, client sent 1"),
            "the daemon's own sentence still leads: {rendered}"
        );
        assert!(
            rendered.contains("shep 0.1.27"),
            "the remedy has to name a version somebody can install: {rendered}"
        );
        assert!(
            rendered.contains("rebuild or reinstall this program"),
            "one direction of the remedy: {rendered}"
        );
        assert!(
            rendered.contains("shep daemon reload"),
            "the other direction, because this type cannot tell which build \
             is the older one: {rendered}"
        );
        assert_eq!(rendered.lines().count(), 1, "one line: {rendered}");
    }

    /// fails if a daemon too old to name its version renders a remedy with a
    /// hole in it -- `rebuild against shep` with nothing after `shep`, or a
    /// literal `None`. That daemon is exactly the one most likely to produce
    /// this error, so its wording is not an edge case.
    #[test]
    fn a_refusal_without_a_version_still_names_something_to_build_against() {
        let rendered = ConnectError::ProtocolMismatch {
            client: 2,
            daemon_version: None,
            message: "daemon speaks protocol 1, client sent 2".into(),
        }
        .to_string();

        assert!(
            rendered.contains("against the running shep and restart it"),
            "the remedy must still point somewhere: {rendered}"
        );
        assert!(
            !rendered.contains("None"),
            "an absent version must never reach the reader: {rendered}"
        );
    }

    /// fails if a daemon that accepts and immediately closes is reported as
    /// anything other than "unreachable". Deliberately asserts the OUTCOME
    /// BUCKET rather than one `ConnectError` variant, because which variant
    /// this produces is a kernel-semantics question and the two kernels shep
    /// runs on answer it differently:
    ///
    /// - macOS lets the `Hello` write succeed and delivers the close to the
    ///   following read, which is `HandshakeClosed`;
    /// - Linux delivers the peer's close to the pending write, so
    ///   `frames.send` fails first and the error is `Io`.
    ///
    /// Both are correct. Nothing downstream distinguishes them either —
    /// `shep-cli`'s `exit.rs` folds `Io`, `HandshakeClosed`, `Connect`,
    /// `Wire` and `HandshakeTimeout` alike into `DaemonUnreachable`, and
    /// `spawn.rs`'s `connect_or_spawn_with` special-cases only `Connect` and
    /// `HandshakeTimeout`. Pinning one variant here would assert a platform,
    /// not a contract.
    ///
    /// What must NOT happen is a silent success, and that is what this still
    /// guards: an `Ok(Connection)` from a peer that answered nothing.
    #[tokio::test]
    async fn a_daemon_that_closes_without_answering_is_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::testing::control_address(dir.path());
        let mut listener = Listener::bind(&path).unwrap();
        tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            drop(stream);
        });

        let err = Connection::open(&path, HANDSHAKE_TIMEOUT, None)
            .await
            .expect_err("a peer that closed without a HelloReply is not a connection");

        assert!(
            matches!(err, ConnectError::HandshakeClosed | ConnectError::Io(_)),
            "a peer that closed mid-handshake must report as unreachable, got {err:?}"
        );
    }

    #[tokio::test]
    async fn connecting_to_a_missing_socket_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::testing::control_address(dir.path());
        let ConnectError::Connect { path: reported, .. } =
            Connection::open(&path, HANDSHAKE_TIMEOUT, None)
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
        let path = crate::testing::control_address(dir.path());
        // Bound; never accepted from. The two platforms reach the same
        // verdict by different mechanics, which is exactly why this asserts
        // the verdict: on unix the kernel completes `connect()` into the
        // backlog, and on Windows the client's open succeeds against an
        // instance the server has not called `ConnectNamedPipe` on. Either
        // way the `Hello` goes out and nothing ever answers it, so only the
        // timeout ends the attempt.
        let _listener = Listener::bind(&path).unwrap();

        let err = Connection::open(&path, Duration::from_millis(150), None)
            .await
            .unwrap_err();

        let ConnectError::HandshakeTimeout { after } = err else {
            panic!("a backlogged connect must time out, not hang or read as success; got {err:?}");
        };
        assert_eq!(after, Duration::from_millis(150));
    }
}
